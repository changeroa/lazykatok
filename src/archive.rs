use crate::{Error, Result};
use rusqlite::Connection;
use std::path::Path;

mod model;
mod parent;
mod read;
mod schema;
mod write;

pub use model::{ChunkDraft, DeletedMessage, ParentChunkDraft, StoredMessage};
pub use read::{RAW_MESSAGES_FOR_CHAT_SINCE_QUERY, TAIL_REBUILD_START_QUERY};
pub use write::{
    DELETE_CHAT_CHUNKS_STATEMENTS, DELETE_REPLY_EDGES_FOR_CHAT, INSERT_CHUNK_PARENT_REFS_FOR_CHAT,
    INSERT_REPLY_EDGES_FOR_CHAT, RESOLVE_REPLY_EDGES_FOR_CHAT, SCOPED_REF_REBUILD_STATEMENTS,
};

pub struct Archive {
    pub(super) conn: Connection,
}

impl Archive {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::paths::ensure_private_dir(parent)?;
        }
        let conn = Connection::open(path).map_err(Error::Sql)?;
        // Every message body lives in this file unencrypted, so it gets the same owner-only mode
        // as the vector store. SQLite gives its journal files the mode of the main file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(Error::Io)?;
        }
        let archive = Self { conn };
        schema::migrate(&archive.conn)?;
        Ok(archive)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Run `work` inside a single SQLite transaction, rolling back if it fails.
    ///
    /// Without this every statement commits on its own, so a sync paid one journal fsync per
    /// row — and a sync that died midway left the archive partially written with nothing to roll
    /// back to.
    pub fn in_transaction<T, E>(
        &self,
        work: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<Error>,
    {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|err| E::from(Error::Sql(err)))?;
        let value = work()?;
        tx.commit().map_err(|err| E::from(Error::Sql(err)))?;
        Ok(value)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::Archive;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn archive_file_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");

        let fresh = dir.path().join("fresh").join("archive.sqlite3");
        Archive::open(&fresh).expect("open fresh archive");
        assert_eq!(
            mode(&fresh),
            0o600,
            "a new archive must not be group/world readable"
        );

        // An archive written by an earlier version keeps the mode it was created with.
        let existing = dir.path().join("existing").join("archive.sqlite3");
        std::fs::create_dir_all(existing.parent().expect("parent")).expect("dir");
        std::fs::write(&existing, b"").expect("seed empty archive");
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        Archive::open(&existing).expect("open existing archive");
        assert_eq!(
            mode(&existing),
            0o600,
            "opening must tighten an older archive"
        );
    }
}
