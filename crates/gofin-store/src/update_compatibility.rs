//! Metadata-only update guard. This never opens `Store` or runs migrations.
use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Read just SQLite's schema header using a read-only connection. Ownership is
/// checked before SQLite sees the file, including its WAL/SHM companions.
#[cfg(unix)]
pub fn inspect_update_schema(path: &Path, expected_uid: u32) -> Result<i64> {
    use std::os::unix::fs::MetadataExt;
    fn validate(path: &Path, uid: u32, directory: bool) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| StoreError::Io("journal metadata is unavailable".into()))?;
        if metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.mode() & 0o022 != 0
            || if directory {
                !metadata.is_dir()
            } else {
                !metadata.is_file()
            }
        {
            return Err(StoreError::Io(
                "journal metadata has unsafe ownership, permissions or type".into(),
            ));
        }
        Ok(())
    }
    validate(
        path.parent()
            .ok_or_else(|| StoreError::Io("journal parent is unavailable".into()))?,
        expected_uid,
        true,
    )?;
    validate(path, expected_uid, false)?;
    let mut live_wal = false;
    for suffix in ["-wal", "-shm"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) => {
                validate(&sidecar, expected_uid, false)?;
                if suffix == "-wal" {
                    live_wal = metadata.len() > 0;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(StoreError::Io(
                    "journal sidecar metadata is unavailable".into(),
                ));
            }
        }
    }
    // URI encoding is essential: a path containing '?' must not supply SQLite
    // connection options. immutable avoids creating sidecars for a closed DB.
    let encoded: String = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
                char::from(*byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let uri = format!(
        "file:{encoded}?mode=ro{}",
        if live_wal { "" } else { "&immutable=1" }
    );
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    let schema: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(schema)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn reads_only_schema_and_preserves_pending_history_and_policy_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("journal?not-an-option.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=19; CREATE TABLE pending(id TEXT); INSERT INTO pending VALUES('unresolved-original-identity');").unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(inspect_update_schema(&path, uid).unwrap(), 19);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            !root
                .path()
                .join("journal?not-an-option.sqlite-wal")
                .exists()
        );
        assert!(inspect_update_schema(&path, uid.wrapping_add(1)).is_err());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(inspect_update_schema(&alias, uid).is_err());
    }

    #[test]
    fn live_wal_schema_is_observed_without_migrating_records() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("journal.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE history(id TEXT); PRAGMA user_version=21;").unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(inspect_update_schema(&path, uid).unwrap(), 21);
        let count: i64 = connection
            .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
