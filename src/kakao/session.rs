//! Whether the running KakaoTalk app is actually using its message store.
//!
//! A signed-out KakaoTalk keeps running at its login window and never opens the
//! encrypted store, so the store on disk stops changing while every other check
//! (app installed, container present, database found, key cached) still passes.
//! `sync` then succeeds and adds nothing. This module tells that state apart by
//! asking which files the app process holds open — file paths only, compared
//! against the store files `discover_database_files` already found. Nothing is
//! decrypted and no path or filename is ever emitted.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::auth::{container_dir, discover_database_files};

/// What the running KakaoTalk app is doing with its message store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppSession {
    /// No KakaoTalk process is running.
    NotRunning,
    /// KakaoTalk is running and holds a message store open.
    StoreOpen,
    /// KakaoTalk is running but holds no message store open: most likely signed out.
    StoreClosed,
    /// KakaoTalk is running but its open files could not be listed.
    Unknown,
}

impl AppSession {
    /// The one-line warning for a state in which `sync` cannot see new messages.
    pub fn warning(self) -> Option<&'static str> {
        match self {
            AppSession::StoreClosed => Some(
                "lazykatok: KakaoTalk is running but has no message database open; it is most \
                 likely signed out, so new messages will not reach the archive until you sign \
                 in to KakaoTalk",
            ),
            _ => None,
        }
    }
}

/// Classify the app session from the processes found and the files they hold open.
///
/// `open_files` is `None` when the open-file listing failed. A store counts as open
/// when the app holds the store file itself or its write-ahead log.
pub fn classify(
    app_running: bool,
    open_files: Option<&[PathBuf]>,
    database_files: &[PathBuf],
) -> AppSession {
    if !app_running {
        return AppSession::NotRunning;
    }
    let Some(open_files) = open_files else {
        return AppSession::Unknown;
    };
    let holds_store = open_files.iter().any(|open| {
        database_files
            .iter()
            .any(|db| open == db || *open == wal_path(db))
    });
    if holds_store {
        AppSession::StoreOpen
    } else {
        AppSession::StoreClosed
    }
}

fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

pub fn probe(home: &Path) -> AppSession {
    let database_files = discover_database_files(&container_dir(home));
    let pids = kakaotalk_pids();
    if pids.is_empty() {
        return classify(false, None, &database_files);
    }
    let open_files = open_files_of(&pids);
    classify(true, open_files.as_deref(), &database_files)
}

fn kakaotalk_pids() -> Vec<String> {
    let Ok(output) = Command::new("/usr/bin/pgrep")
        .args(["-x", "KakaoTalk"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

fn open_files_of(pids: &[String]) -> Option<Vec<PathBuf>> {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-Fn", "-p", &pids.join(",")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_lsof_names(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_lsof_names(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> PathBuf {
        dir.join("0".repeat(78))
    }

    #[test]
    fn signed_out_app_holds_no_store_open() {
        let dir = Path::new("/synthetic/container");
        let open = vec![
            PathBuf::from("/synthetic/Library/Caches/Cache.db"),
            PathBuf::from("/synthetic/Library/HTTPStorages/httpstorages.sqlite"),
        ];

        let session = classify(true, Some(&open), &[store(dir)]);

        assert_eq!(session, AppSession::StoreClosed);
        assert!(session.warning().is_some());
    }

    #[test]
    fn signed_in_app_holds_the_store_or_its_wal() {
        let dir = Path::new("/synthetic/container");
        let db = store(dir);

        assert_eq!(
            classify(
                true,
                Some(std::slice::from_ref(&db)),
                std::slice::from_ref(&db)
            ),
            AppSession::StoreOpen
        );
        assert_eq!(
            classify(true, Some(&[wal_path(&db)]), &[db]),
            AppSession::StoreOpen
        );
    }

    #[test]
    fn shared_memory_file_alone_is_not_an_open_store() {
        // A reader that only maps `-shm` (lazykatok's own read touches it) is not the app using the store.
        let db = store(Path::new("/synthetic/container"));
        let mut shm = db.as_os_str().to_os_string();
        shm.push("-shm");

        assert_eq!(
            classify(true, Some(&[PathBuf::from(shm)]), &[db]),
            AppSession::StoreClosed
        );
    }

    #[test]
    fn no_process_and_unlistable_files_are_not_warnings() {
        let db = store(Path::new("/synthetic/container"));

        assert_eq!(
            classify(false, None, std::slice::from_ref(&db)),
            AppSession::NotRunning
        );
        assert_eq!(classify(true, None, &[db]), AppSession::Unknown);
        assert_eq!(AppSession::NotRunning.warning(), None);
        assert_eq!(AppSession::Unknown.warning(), None);
        assert_eq!(AppSession::StoreOpen.warning(), None);
    }

    #[test]
    fn parses_lsof_field_output_names() {
        let stdout = "p788\nfcwd\nn/\nf13\nn/synthetic/a.db\nf14\nn/synthetic/a.db-wal\n";

        assert_eq!(
            parse_lsof_names(stdout),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/synthetic/a.db"),
                PathBuf::from("/synthetic/a.db-wal"),
            ]
        );
    }

    #[test]
    fn warning_names_no_path_or_file() {
        let warning = AppSession::StoreClosed.warning().unwrap();

        assert!(!warning.contains('/'));
        assert!(!warning.contains(".db"));
    }
}
