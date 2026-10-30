//! Best-effort storage for public token metadata, never credentials or financial records.
//!
//! Callers define identity-bearing keys (including chain and contract/mint), validate
//! decoded values, and supply Unix seconds. A TTL of -1 means no expiration; positive
//! TTLs expire at `fetched_at + ttl`. Changing a TTL invalidates existing entries.
//! Disk-backed reads always reread disk so another process's invalidation is visible.
//! Missing, oversized, incompatible, or corrupt entries are cache misses.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const VERSION: u32 = 1;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;

/// A small per-key JSON cache, with isolated in-memory storage available for fixtures.
pub struct TokenCache {
    storage: Storage,
}

enum Storage {
    Memory(Mutex<BTreeMap<String, Vec<u8>>>),
    Disk(PathBuf),
}

#[derive(Deserialize, Serialize)]
struct Envelope<T> {
    version: u32,
    key: String,
    ttl_seconds: i64,
    fetched_at: i64,
    value: T,
}

impl TokenCache {
    /// Create a fresh cache isolated from the filesystem and other instances.
    pub fn memory() -> Self {
        Self {
            storage: Storage::Memory(Mutex::new(BTreeMap::new())),
        }
    }

    /// Use a caller-owned directory. Creation is deferred until the first write.
    /// The parent directory must be trusted; cache entries must not contain secrets.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            storage: Storage::Disk(root.into()),
        }
    }

    /// Read a current entry. Invalid policy, time, format, or IO is a cache miss.
    /// Successful deserialization does not replace the caller's semantic validation.
    pub fn get<T: DeserializeOwned>(&self, key: &str, ttl_seconds: i64, now: i64) -> Option<T> {
        if !valid_policy(ttl_seconds) || now < 0 {
            return None;
        }
        let bytes = match &self.storage {
            Storage::Memory(entries) => entries.lock().ok()?.get(key)?.clone(),
            Storage::Disk(root) => {
                if !directory_is_safe(root) {
                    return None;
                }
                read_entry(&entry_path(root, key))?
            }
        };
        let entry: Envelope<T> = serde_json::from_slice(&bytes).ok()?;
        if entry.version != VERSION
            || entry.key != key
            || entry.ttl_seconds != ttl_seconds
            || entry.fetched_at < 0
            || entry.fetched_at > now
            || (ttl_seconds != -1 && now >= entry.fetched_at.checked_add(ttl_seconds)?)
        {
            return None;
        }
        Some(entry.value)
    }

    /// Store an entry atomically. Failure returns false and must not fail a live read.
    pub fn put<T: Serialize>(&self, key: &str, value: &T, ttl_seconds: i64, now: i64) -> bool {
        if !valid_policy(ttl_seconds)
            || now < 0
            || (ttl_seconds != -1 && now.checked_add(ttl_seconds).is_none())
        {
            return false;
        }
        let entry = Envelope {
            version: VERSION,
            key: key.to_owned(),
            ttl_seconds,
            fetched_at: now,
            value,
        };
        let Ok(bytes) = serde_json::to_vec(&entry) else {
            return false;
        };
        if bytes.len() > MAX_ENTRY_BYTES {
            return false;
        }
        match &self.storage {
            Storage::Memory(entries) => match entries.lock() {
                Ok(mut entries) => {
                    entries.insert(key.to_owned(), bytes);
                    true
                }
                Err(_) => false,
            },
            Storage::Disk(root) => write_entry(root, key, &bytes).is_ok(),
        }
    }

    /// Remove an existing entry. Missing entries and failed removals return false.
    pub fn remove(&self, key: &str) -> bool {
        match &self.storage {
            Storage::Memory(entries) => entries
                .lock()
                .ok()
                .and_then(|mut entries| entries.remove(key))
                .is_some(),
            Storage::Disk(root) => {
                directory_is_safe(root) && fs::remove_file(entry_path(root, key)).is_ok()
            }
        }
    }
}

fn valid_policy(ttl_seconds: i64) -> bool {
    ttl_seconds == -1 || ttl_seconds > 0
}

fn entry_path(root: &Path, key: &str) -> PathBuf {
    root.join(format!(
        "{}.json",
        hex::encode(Sha256::digest(key.as_bytes()))
    ))
}

fn directory_is_safe(root: &Path) -> bool {
    fs::symlink_metadata(root)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn read_entry(path: &Path) -> Option<Vec<u8>> {
    // O_NOFOLLOW also closes the final-component symlink race on Unix.
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENTRY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_ENTRY_BYTES).then_some(bytes)
}

fn write_entry(root: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(root)?;
    if !directory_is_safe(root) {
        return Err(std::io::Error::other("unsafe cache directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let path = entry_path(root, key);
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(std::io::Error::other("unsafe cache entry"));
    }
    // NamedTempFile uses exclusive, unique creation and private file permissions.
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    // Best effort directory durability: losing a cache after a crash is a miss.
    if let Ok(directory) = File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn check_policy(cache: &TokenCache) {
        assert!(cache.put("ethereum:token", &18, 60, 100));
        assert_eq!(cache.get::<u8>("ethereum:token", 60, 100), Some(18));
        assert_eq!(cache.get::<u8>("ethereum:token", 60, 159), Some(18));
        assert_eq!(cache.get::<u8>("ethereum:token", 60, 160), None);
        assert_eq!(cache.get::<u8>("ethereum:token", 60, 99), None);
        assert_eq!(cache.get::<u8>("ethereum:token", 61, 100), None);
        assert_eq!(cache.get::<u8>("ethereum:token", -1, 100), None);
        assert_eq!(cache.get::<u8>("base:token", 60, 100), None);
        assert!(cache.put("ethereum:immutable", &6, -1, 100));
        assert_eq!(cache.get::<u8>("ethereum:immutable", -1, i64::MAX), Some(6));
        assert_eq!(cache.get::<u8>("ethereum:immutable", -1, 99), None);
        assert_eq!(cache.get::<u8>("ethereum:immutable", 60, 100), None);
        for invalid in [0, -2, i64::MIN] {
            assert!(!cache.put("bad", &18, invalid, 100));
            assert_eq!(cache.get::<u8>("bad", invalid, 100), None);
        }
        assert!(!cache.put("bad", &18, 60, -1));
        assert!(!cache.put("bad", &18, 60, i64::MAX));
        assert!(cache.remove("ethereum:token"));
        assert!(!cache.remove("ethereum:token"));
        assert_eq!(cache.get::<u8>("ethereum:token", 60, 101), None);
    }

    #[test]
    fn memory_cache_enforces_time_and_policy() {
        check_policy(&TokenCache::memory());
    }

    #[test]
    fn persistent_cache_enforces_time_and_policy() {
        let directory = tempfile::tempdir().unwrap();
        check_policy(&TokenCache::new(directory.path().join("cache")));
    }

    #[test]
    fn restart_and_other_instance_invalidation_are_visible() {
        let directory = tempfile::tempdir().unwrap();
        let first = TokenCache::new(directory.path());
        assert!(first.put("mint", &9, -1, 100));
        let second = TokenCache::new(directory.path());
        assert_eq!(second.get::<u8>("mint", -1, 101), Some(9));
        assert!(second.put("mint", &6, -1, 101));
        assert_eq!(first.get::<u8>("mint", -1, 102), Some(6));
        assert!(second.remove("mint"));
        assert_eq!(first.get::<u8>("mint", -1, 102), None);
    }

    #[test]
    fn corrupt_incompatible_and_oversized_entries_are_recoverable_misses() {
        let directory = tempfile::tempdir().unwrap();
        let cache = TokenCache::new(directory.path());
        let path = entry_path(directory.path(), "token");
        let invalid = [
            "{",
            r#"{"version":2,"key":"token","ttl_seconds":60,"fetched_at":100,"value":18}"#,
            r#"{"version":1,"key":"other","ttl_seconds":60,"fetched_at":100,"value":18}"#,
            r#"{"version":1,"key":"token","ttl_seconds":60,"fetched_at":-1,"value":18}"#,
            r#"{"version":1,"key":"token","ttl_seconds":60,"fetched_at":100,"value":"wrong-type"}"#,
        ];
        for content in invalid {
            fs::write(&path, content).unwrap();
            assert_eq!(cache.get::<u8>("token", 60, 101), None);
            assert!(cache.put("token", &18, 60, 100));
            assert_eq!(cache.get::<u8>("token", 60, 101), Some(18));
        }
        fs::write(&path, vec![b' '; MAX_ENTRY_BYTES + 1]).unwrap();
        assert_eq!(cache.get::<u8>("token", 60, 101), None);
        assert!(cache.put("token", &18, 60, 100));
        assert!(!cache.put("token", &"x".repeat(MAX_ENTRY_BYTES), 60, 100));
        assert_eq!(cache.get::<u8>("token", 60, 101), Some(18));
    }

    #[test]
    fn unavailable_directory_does_not_break_reads_or_overwrite_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("file-not-directory");
        fs::write(&root, "keep").unwrap();
        let cache = TokenCache::new(&root);
        assert!(!cache.put("token", &18, -1, 100));
        assert_eq!(cache.get::<u8>("token", -1, 100), None);
        assert!(!cache.remove("token"));
        assert_eq!(fs::read_to_string(root).unwrap(), "keep");
    }

    #[test]
    fn arbitrary_keys_cannot_escape_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let cache = TokenCache::new(&root);
        let key = "../../escape/\0token";
        assert!(cache.put(key, &18, -1, 100));
        assert_eq!(cache.get::<u8>(key, -1, 100), Some(18));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let entries: Vec<_> = fs::read_dir(&root).unwrap().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().to_str().unwrap().len(), 69);
    }

    #[test]
    fn concurrent_writes_are_complete_and_separate_keys_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Arc::new(TokenCache::new(directory.path()));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for iteration in 0..20 {
                        let value = vec![index; 100];
                        assert!(cache.put(&format!("token-{index}"), &value, -1, iteration));
                        assert!(cache.put("shared-token", &value, -1, iteration));
                        let read = cache.get::<Vec<u8>>("shared-token", -1, 100).unwrap();
                        assert_eq!(read.len(), 100);
                        assert!(read.iter().all(|value| *value == read[0]));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        for index in 0..8 {
            assert_eq!(
                cache.get::<Vec<u8>>(&format!("token-{index}"), -1, 100),
                Some(vec![index; 100])
            );
        }
        fs::write(directory.path().join(".tmp-interrupted"), "{").unwrap();
        assert_eq!(cache.get::<Vec<u8>>("token-0", -1, 100), Some(vec![0; 100]));
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_and_symlink_rejection() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let cache = TokenCache::new(&root);
        assert!(cache.put("token", &18, -1, 100));
        let path = entry_path(&root, "token");
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let target = directory.path().join("target");
        fs::write(&target, "keep").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(cache.get::<u8>("token", -1, 100), None);
        assert!(!cache.put("token", &18, -1, 100));
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        let root_link = directory.path().join("root-link");
        symlink(&root, &root_link).unwrap();
        let linked_cache = TokenCache::new(root_link);
        assert_eq!(linked_cache.get::<u8>("token", -1, 100), None);
        assert!(!linked_cache.put("other", &18, -1, 100));
    }
}
