//! Where KakaoTalk keeps its encrypted stores on macOS, and which copy is live.
//!
//! The databases have shipped from two different roots: inside the app sandbox
//! container, and in `Library/Application Support` outside it. An install that
//! moves between them leaves the abandoned copy on disk under the *same*
//! filename, frozen at the moment of the move — and a reader that knows only
//! the old root keeps decrypting that twin, so every room appears to have
//! stopped talking on moving day while the app carries on normally. Scan both
//! roots and let the most recently modified copy of each filename win.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const STORE_DIR_NAME: &str = "com.kakao.KakaoTalkMac";

/// The store root inside the app sandbox container.
pub fn container_dir(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Containers")
        .join(STORE_DIR_NAME)
        .join("Data")
        .join("Library")
        .join("Application Support")
        .join(STORE_DIR_NAME)
}

/// The store root outside the sandbox container.
pub fn app_support_dir(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join(STORE_DIR_NAME)
}

/// Every root a KakaoTalk store can live in.
pub fn store_dirs(home: &Path) -> Vec<PathBuf> {
    vec![app_support_dir(home), container_dir(home)]
}

/// DB files directly in `dir` matching `^[0-9a-f]{78}(?:\.db)?$`.
pub fn discover_database_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_hex_db_name)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

/// Database files across every store root, keeping only the most recently
/// modified copy of each filename. A file whose mtime cannot be read is treated
/// as the oldest possible, so a readable copy always outranks it.
pub fn discover_databases(home: &Path) -> Vec<PathBuf> {
    let mut newest: HashMap<String, (SystemTime, PathBuf)> = HashMap::new();
    for dir in store_dirs(home) {
        for path in discover_database_files(&dir) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            match newest.entry(name.to_string()) {
                Entry::Occupied(mut slot) => {
                    if modified > slot.get().0 {
                        slot.insert((modified, path));
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert((modified, path));
                }
            }
        }
    }
    let mut files: Vec<PathBuf> = newest.into_values().map(|(_, path)| path).collect();
    files.sort();
    files
}

fn is_hex_db_name(name: &str) -> bool {
    // Mirror the reference HEX_DATABASE_PATTERN `^[0-9a-f]{78}(?:\.db)?$`: accept
    // an optional trailing ".db" before the 78-lowercase-hex check.
    let stem = name.strip_suffix(".db").unwrap_or(name);
    stem.len() == 78
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write_db(dir: &Path, name: &str, mtime_secs: u64) -> PathBuf {
        std::fs::create_dir_all(dir).expect("create store root");
        let path = dir.join(name);
        let file = std::fs::File::create(&path).expect("create db file");
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs))
            .expect("set db mtime");
        path
    }

    fn db_name() -> String {
        "a".repeat(78)
    }

    #[test]
    fn hex_db_name_requires_lowercase_78() {
        assert!(is_hex_db_name(&"a".repeat(78)));
        assert!(!is_hex_db_name(&"a".repeat(77)));
        assert!(!is_hex_db_name(&"A".repeat(78)));
        assert!(!is_hex_db_name(&"g".repeat(78)));
    }

    #[test]
    fn hex_db_name_accepts_optional_db_suffix() {
        // Reference HEX_DATABASE_PATTERN `^[0-9a-f]{78}(?:\.db)?$`.
        assert!(is_hex_db_name(&format!("{}.db", "a".repeat(78))));
        // Wrong stem length even with the suffix is still rejected.
        assert!(!is_hex_db_name(&format!("{}.db", "a".repeat(77))));
        // A bare ".db" or other suffixes (-wal/-shm) are not databases.
        assert!(!is_hex_db_name(".db"));
        assert!(!is_hex_db_name(&format!("{}-wal", "a".repeat(78))));
    }

    #[test]
    fn discovers_db_suffixed_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stem = "a".repeat(78);
        let suffixed = dir.path().join(format!("{stem}.db"));
        std::fs::write(&suffixed, b"x").expect("write db file");
        let found = discover_database_files(dir.path());
        assert_eq!(found, vec![suffixed]);
    }

    #[test]
    fn finds_a_store_outside_the_sandbox_container() {
        let home = tempfile::tempdir().expect("temp home");
        let live = write_db(&app_support_dir(home.path()), &db_name(), 1_000);
        assert_eq!(discover_databases(home.path()), vec![live]);
    }

    #[test]
    fn newest_copy_of_a_filename_wins_across_roots() {
        let home = tempfile::tempdir().expect("temp home");
        let name = db_name();
        // Same filename in both roots: the abandoned copy keeps its old mtime.
        write_db(&container_dir(home.path()), &name, 1_000);
        let live = write_db(&app_support_dir(home.path()), &name, 2_000);
        assert_eq!(discover_databases(home.path()), vec![live]);
    }

    #[test]
    fn container_copy_wins_while_it_is_the_fresh_one() {
        let home = tempfile::tempdir().expect("temp home");
        let name = db_name();
        write_db(&app_support_dir(home.path()), &name, 1_000);
        let live = write_db(&container_dir(home.path()), &name, 2_000);
        assert_eq!(discover_databases(home.path()), vec![live]);
    }

    #[test]
    fn distinct_filenames_from_both_roots_are_all_returned() {
        let home = tempfile::tempdir().expect("temp home");
        let outside = write_db(&app_support_dir(home.path()), &"a".repeat(78), 1_000);
        let inside = write_db(&container_dir(home.path()), &"b".repeat(78), 1_000);
        let mut expected = vec![outside, inside];
        expected.sort();
        assert_eq!(discover_databases(home.path()), expected);
    }
}
