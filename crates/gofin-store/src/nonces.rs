//! Durable, process-safe nonce allocation. Keys are irreversible credential digests.
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, sync::Mutex};

pub struct NonceStore(Mutex<Connection>);

impl NonceStore {
    pub fn open(path: &Path) -> super::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            match options.open(path) {
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(super::StoreError::Io(e.to_string())),
            }
            let meta = std::fs::symlink_metadata(path)
                .map_err(|e| super::StoreError::Io(e.to_string()))?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.mode() & 0o777 != 0o600
                || meta.uid() != unsafe { libc_uid() }
            {
                return Err(super::StoreError::Io(
                    "Unsafe nonce store ownership or permissions".into(),
                ));
            }
        }
        // macOS's standard /var temporary directory is itself a symlink. Resolve
        // ancestors, while retaining SQLite's no-follow protection on the file.
        let parent = path
            .parent()
            .ok_or_else(|| super::StoreError::Io("Nonce store needs a parent directory".into()))?;
        let canonical = parent
            .canonicalize()
            .map_err(|e| super::StoreError::Io(e.to_string()))?
            .join(
                path.file_name()
                    .ok_or_else(|| super::StoreError::Io("Nonce store needs a filename".into()))?,
            );
        let connection = Connection::open_with_flags(
            canonical,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        Self::initialize(connection)
    }

    pub fn in_memory() -> super::Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> super::Result<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS credential_nonces (key_digest TEXT PRIMARY KEY, high_water TEXT NOT NULL);")?;
        Ok(Self(Mutex::new(connection)))
    }

    /// Commit before returning; a crash may skip a nonce but can never reuse it.
    pub fn allocate(&self, key_digest: &str, clock_ms: u64) -> super::Result<u64> {
        if key_digest.len() != 64 || !key_digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(super::StoreError::Io(
                "Invalid nonce credential digest".into(),
            ));
        }
        let mut connection = self.0.lock().map_err(|_| super::StoreError::Poisoned)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT high_water FROM credential_nonces WHERE key_digest=?1",
                [key_digest],
                |r| r.get(0),
            )
            .optional()?;
        let previous = previous
            .map(|s| s.parse::<u64>())
            .transpose()
            .map_err(|_| super::StoreError::Io("Invalid persisted nonce".into()))?
            .unwrap_or(0);
        let next = previous
            .checked_add(1)
            .ok_or_else(|| super::StoreError::Io("Nonce exhausted".into()))?
            .max(clock_ms);
        tx.execute("INSERT INTO credential_nonces VALUES (?1,?2) ON CONFLICT(key_digest) DO UPDATE SET high_water=excluded.high_water", params![key_digest, next.to_string()])?;
        tx.commit()?;
        Ok(next)
    }
}

#[cfg(unix)]
unsafe fn libc_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_and_clock_rollback_preserve_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonce.sqlite");
        let key = "a".repeat(64);
        {
            let store = NonceStore::open(&path).unwrap();
            assert_eq!(store.allocate(&key, 100).unwrap(), 100);
        }
        let store = NonceStore::open(&path).unwrap();
        assert_eq!(store.allocate(&key, 1).unwrap(), 101);
        assert_eq!(store.allocate(&"b".repeat(64), 2).unwrap(), 2);
    }
    #[test]
    fn independent_process_connections_serialize_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonce.sqlite");
        let stores: Vec<_> = (0..4).map(|_| NonceStore::open(&path).unwrap()).collect();
        let mut handles = vec![];
        for store in stores {
            handles.push(std::thread::spawn(move || {
                (0..25)
                    .map(|_| store.allocate(&"c".repeat(64), 1).unwrap())
                    .collect::<Vec<_>>()
            }));
        }
        let mut values: Vec<_> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        values.sort();
        assert_eq!(values, (1..=100).collect::<Vec<_>>());
    }
    #[test]
    fn corrupt_or_exhausted_high_water_never_resets_or_reuses_a_nonce() {
        let store = NonceStore::in_memory().unwrap();
        let digest = "a".repeat(64);
        for bad in ["corrupt", "-1", "18446744073709551615"] {
            store.0.lock().unwrap().execute("INSERT INTO credential_nonces VALUES (?1,?2) ON CONFLICT(key_digest) DO UPDATE SET high_water=excluded.high_water",params![digest,bad]).unwrap();
            assert!(store.allocate(&digest, 1).is_err());
            let actual: String = store
                .0
                .lock()
                .unwrap()
                .query_row(
                    "SELECT high_water FROM credential_nonces WHERE key_digest=?1",
                    [&digest],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, bad);
        }
        assert!(store.allocate("private-api-key", 1).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn nonce_files_reject_symlinks_hardlinks_and_loose_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.sqlite");
        let store = NonceStore::open(&original).unwrap();
        store.allocate(&"a".repeat(64), 1).unwrap();
        drop(store);
        let link = directory.path().join("link.sqlite");
        symlink(&original, &link).unwrap();
        assert!(NonceStore::open(&link).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&original, &link).unwrap();
        assert!(NonceStore::open(&original).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(NonceStore::open(&original).is_err());
    }
}
