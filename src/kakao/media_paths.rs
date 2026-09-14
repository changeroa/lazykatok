//! KakaoTalk media-cache path helpers.
//!
//! Media files live below 40-hex account directories in the KakaoTalk macOS
//! store roots. Each chat room is a SHA-1 of the reversed chat id, and each media
//! filename stem is a SHA-1 of the reversed KakaoTalk media key string.

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use super::auth;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDirs {
    roots: Vec<PathBuf>,
}

impl MediaDirs {
    /// Scan every KakaoTalk store root once for direct 40-hex media account
    /// dirs, merging what each root holds — an install that changed roots keeps
    /// serving already-downloaded media from the abandoned one.
    ///
    /// This is intentionally bounded to one directory level. An unreadable
    /// root is skipped if another root can be scanned. Having no readable store
    /// root is an error; a readable empty root is a valid cache state.
    pub fn discover(home: &Path) -> Result<Self> {
        let present: Vec<PathBuf> = super::store::store_dirs(home)
            .into_iter()
            .filter(|dir| dir.is_dir())
            .collect();
        if present.is_empty() {
            return Err(Error::Kakao(format!(
                "kakao media container not found: {}",
                auth::container_dir(home).display()
            )));
        }
        let mut merged = Self { roots: Vec::new() };
        let mut scanned_any = false;
        let mut scan_error = None;
        for dir in present {
            match Self::discover_in_container(&dir) {
                Ok(found) => {
                    scanned_any = true;
                    merged.roots.extend(found.roots);
                }
                Err(error) => scan_error = Some(error),
            }
        }
        if let Some(error) = scan_error {
            if !scanned_any {
                return Err(error);
            }
        }
        merged.roots.sort();
        merged.roots.dedup();
        Ok(merged)
    }

    pub fn discover_in_container(container: &Path) -> Result<Self> {
        if !container.is_dir() {
            return Err(Error::Kakao(format!(
                "kakao media container not found: {}",
                container.display()
            )));
        }
        let entries = std::fs::read_dir(container).map_err(|err| {
            Error::Kakao(format!(
                "cannot scan kakao media container {}: {err}",
                container.display()
            ))
        })?;
        let mut roots = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                Error::Kakao(format!(
                    "cannot scan kakao media container {}: {err}",
                    container.display()
                ))
            })?;
            let path = entry.path();
            let is_media_account = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_media_account_dir_name);
            if is_media_account && path.is_dir() {
                roots.push(path);
            }
        }
        roots.sort();
        Ok(Self { roots })
    }

    pub fn from_roots_for_test(mut roots: Vec<PathBuf>) -> Self {
        roots.sort();
        Self { roots }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Search every account dir for `<sha1_rev(chat_id)>/<stem><ext>`.
    pub fn find_media_file(&self, chat_id: i64, name_stem: &str, ext: &str) -> Option<PathBuf> {
        let chat_sha = chat_media_dir_name(chat_id);
        let filename = format!("{name_stem}{ext}");
        self.roots
            .iter()
            .map(|root| root.join(&chat_sha).join(&filename))
            .find(|candidate| candidate.is_file())
    }
}

pub fn sha1_rev(input: &str) -> String {
    sha1_hex(input.chars().rev().collect::<String>().as_bytes())
}

pub fn chat_media_dir_name(chat_id: i64) -> String {
    sha1_rev(&chat_id.to_string())
}

pub fn photo_full_stem(log_id: i64) -> String {
    sha1_rev(&format!("p{log_id}"))
}

pub fn photo_thumb_stem(log_id: i64) -> String {
    sha1_rev(&format!("t{log_id}"))
}

/// Video cache stem. KakaoTalk stores video bodies as `<sha1_rev("v<logId>")>.vid`
/// in the same per-chat directory as photos; only the key prefix differs.
pub fn video_full_stem(log_id: i64) -> String {
    sha1_rev(&format!("v{log_id}"))
}

pub fn album_full_stem(log_id: i64, idx: usize) -> String {
    sha1_rev(&format!("p{idx}_{log_id}"))
}

pub fn album_thumb_stem(log_id: i64, idx: usize) -> String {
    sha1_rev(&format!("t{idx}_{log_id}"))
}

fn is_media_account_dir_name(name: &str) -> bool {
    name.len() == 40
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha1_hex(bytes: &[u8]) -> String {
    let digest = Sha1::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_media_from_both_store_roots() {
        let home = tempfile::tempdir().expect("temp home");
        let mut expected = Vec::new();
        for dir in super::super::store::store_dirs(home.path()) {
            let account = dir.join("a".repeat(40));
            std::fs::create_dir_all(&account).expect("create media account");
            expected.push(account);
        }
        expected.sort();
        assert_eq!(
            MediaDirs::discover(home.path()).expect("discover").roots(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_store_does_not_hide_media_in_the_other_root() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temp home");
        let dirs = super::super::store::store_dirs(home.path());
        for dir in &dirs {
            std::fs::create_dir_all(dir).expect("create store root");
        }
        // Exercise both scan orders: the failure can precede or follow success.
        for unreadable_index in 0..dirs.len() {
            let unreadable = &dirs[unreadable_index];
            let readable = &dirs[1 - unreadable_index];
            let account = readable.join("a".repeat(40));
            let chat_dir = account.join(chat_media_dir_name(42));
            std::fs::create_dir_all(&chat_dir).expect("create chat dir");
            let media = chat_dir.join("test.img");
            std::fs::write(&media, b"synthetic media").expect("write media");
            let permissions = std::fs::metadata(unreadable)
                .expect("metadata")
                .permissions();
            std::fs::set_permissions(unreadable, std::fs::Permissions::from_mode(0o111))
                .expect("remove read permission");
            let scan_error = std::fs::read_dir(unreadable).err();
            let found = MediaDirs::discover(home.path());
            // Restore permissions before assertions so temporary fixtures can be removed.
            std::fs::set_permissions(unreadable, permissions).expect("restore permissions");
            assert_eq!(
                scan_error
                    .expect("test requires an unprivileged user")
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let found = found.expect("readable root must remain usable");
            assert_eq!(found.roots(), std::slice::from_ref(&account));
            assert_eq!(found.find_media_file(42, "test", ".img"), Some(media));
            std::fs::remove_dir_all(account).expect("remove account fixture");
        }
    }

    #[test]
    fn no_store_is_an_error_but_a_readable_empty_store_is_valid() {
        let home = tempfile::tempdir().expect("temp home");
        assert!(MediaDirs::discover(home.path()).is_err());
        std::fs::create_dir_all(super::super::store::app_support_dir(home.path()))
            .expect("create empty store");
        assert!(MediaDirs::discover(home.path())
            .expect("empty store")
            .roots()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_only_store_is_an_error_but_readable_empty_peer_is_valid() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("temp home");
        let unreadable = super::super::store::app_support_dir(home.path());
        std::fs::create_dir_all(&unreadable).expect("create store");
        let permissions = std::fs::metadata(&unreadable)
            .expect("metadata")
            .permissions();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o111))
            .expect("remove read permission");
        let only_unreadable = MediaDirs::discover(home.path());
        let peer = super::super::store::container_dir(home.path());
        let create_peer = std::fs::create_dir_all(peer);
        let with_empty_peer = MediaDirs::discover(home.path());
        std::fs::set_permissions(&unreadable, permissions).expect("restore permissions");
        create_peer.expect("create readable empty store");
        assert!(only_unreadable.is_err());
        assert!(with_empty_peer
            .expect("readable empty peer")
            .roots()
            .is_empty());
    }

    #[test]
    fn sha1_rev_matches_python_reference() {
        assert_eq!(
            sha1_rev("p1234567890123"),
            "6a57dbd91a25d5f1e503c316e13487b6abd8de5c"
        );
        assert_eq!(
            chat_media_dir_name(1234567890123),
            "f3040a56bce932b9fe31cf4e68a2eae23c33165b"
        );
    }

    #[test]
    fn discovers_only_lowercase_40_hex_account_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let valid = tmp.path().join("0123456789abcdef0123456789abcdef01234567");
        let uppercase = tmp.path().join("abcdefabcdefabcdefabcdefabcdefabcdefABCD");
        let too_long = tmp.path().join("0123456789abcdef0123456789abcdef012345678");
        std::fs::create_dir(&valid).expect("valid dir");
        std::fs::create_dir(&uppercase).expect("uppercase dir");
        std::fs::create_dir(&too_long).expect("too-long dir");
        std::fs::write(
            tmp.path().join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b"file",
        )
        .expect("media-looking file");

        let dirs = MediaDirs::discover_in_container(tmp.path()).expect("discover");

        assert_eq!(dirs.roots(), &[valid]);
    }

    #[test]
    fn finds_media_by_account_chat_and_stem() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("0123456789abcdef0123456789abcdef01234567");
        let chat_dir = root.join(chat_media_dir_name(42));
        std::fs::create_dir_all(&chat_dir).expect("chat dir");
        let stem = photo_full_stem(77);
        let expected = chat_dir.join(format!("{stem}.img"));
        std::fs::write(&expected, b"media").expect("media file");
        let dirs = MediaDirs::from_roots_for_test(vec![root]);

        assert_eq!(dirs.find_media_file(42, &stem, ".img"), Some(expected));
        assert_eq!(dirs.find_media_file(43, &stem, ".img"), None);
    }
}
