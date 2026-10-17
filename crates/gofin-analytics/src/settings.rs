use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub struct SettingsStatus {
    /// The saved preference, independent of whether this build can send.
    pub enabled: bool,
    pub effective_enabled: bool,
    pub release_enabled: bool,
    pub reason: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSettings {
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notice_version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SettingsStore {
    home: PathBuf,
    uid: u32,
}

#[derive(Debug)]
enum SettingsError {
    Missing,
    Unsafe,
    Unreadable,
    Malformed,
}
impl SettingsError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "enabled_by_default",
            Self::Unsafe => "unsafe_settings",
            Self::Unreadable => "unreadable_settings",
            Self::Malformed => "malformed_settings",
        }
    }
}

fn file_from_fd(fd: libc::c_int) -> std::io::Result<File> {
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn io_error(error: std::io::Error) -> SettingsError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => SettingsError::Missing,
        Some(libc::ELOOP | libc::ENOTDIR) => SettingsError::Unsafe,
        _ => SettingsError::Unreadable,
    }
}
fn valid(file: &File, uid: u32, directory: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    metadata.uid() == uid
        && if directory {
            metadata.is_dir() && metadata.mode() & 0o022 == 0
        } else {
            metadata.is_file()
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1
                && metadata.len() <= 4096
        }
}
fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> std::io::Result<File> {
    file_from_fd(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )
    })
}

impl SettingsStore {
    /// Explicit homes are for isolated test fixtures; production uses the OS account database.
    pub fn for_home(home: PathBuf, uid: u32) -> Self {
        Self { home, uid }
    }
    pub fn for_current_account() -> Result<Self, String> {
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            return Err("root_settings_unavailable".into());
        }
        if uid != unsafe { libc::getuid() } {
            return Err("account_identity_unavailable".into());
        }
        let mut password: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; 16 * 1024];
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &mut password,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code != 0 || result.is_null() || password.pw_dir.is_null() {
            return Err("account_home_unavailable".into());
        }
        let path = unsafe { CStr::from_ptr(password.pw_dir) };
        let home = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).to_path_buf();
        if !home.is_absolute() {
            return Err("account_home_unavailable".into());
        }
        Ok(Self { home, uid })
    }

    fn directory(&self, create: bool) -> Result<File, SettingsError> {
        let path =
            CString::new(self.home.as_os_str().as_bytes()).map_err(|_| SettingsError::Unsafe)?;
        let mut directory = file_from_fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .map_err(io_error)?;
        if !valid(&directory, self.uid, true) {
            return Err(SettingsError::Unsafe);
        }
        for name in [c".config", c"gofin"] {
            if create {
                let code = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if code != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                {
                    return Err(SettingsError::Unreadable);
                }
            }
            directory =
                open_at(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(io_error)?;
            if !valid(&directory, self.uid, true) {
                return Err(SettingsError::Unsafe);
            }
        }
        Ok(directory)
    }
    fn read_from(&self, directory: &File) -> Result<SavedSettings, SettingsError> {
        let file = open_at(directory, c"analytics.json", libc::O_RDONLY).map_err(io_error)?;
        if !valid(&file, self.uid, false) {
            return Err(SettingsError::Unsafe);
        }
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| SettingsError::Unreadable)?;
        if bytes.len() > 4096 {
            return Err(SettingsError::Unsafe);
        }
        serde_json::from_slice(&bytes).map_err(|_| SettingsError::Malformed)
    }
    pub fn status(&self, release_enabled: bool) -> SettingsStatus {
        let result = self
            .directory(false)
            .and_then(|directory| self.read_from(&directory));
        let (enabled, reason) = match result {
            Ok(saved) => (
                saved.enabled,
                if saved.enabled {
                    "enabled_by_user"
                } else {
                    "disabled_by_user"
                },
            ),
            Err(SettingsError::Missing) => (true, "enabled_by_default"),
            Err(error) => (false, error.reason()),
        };
        SettingsStatus {
            enabled,
            effective_enabled: enabled && release_enabled,
            release_enabled,
            reason: reason.into(),
        }
    }
    fn lock(&self, directory: &File, nonblocking: bool) -> Result<File, SettingsError> {
        let lock = open_at(directory, c"analytics.lock", libc::O_RDWR | libc::O_CREAT)
            .map_err(io_error)?;
        if !valid(&lock, self.uid, false) {
            return Err(SettingsError::Unsafe);
        }
        // Settings commands may wait for another local writer; sending never takes this lock.
        let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
        if unsafe { libc::flock(lock.as_raw_fd(), flags) } != 0 {
            return Err(SettingsError::Unreadable);
        }
        Ok(lock)
    }
    fn write_to(&self, directory: &File, saved: &SavedSettings) -> Result<(), SettingsError> {
        // Do not replace symlinks, hardlinks, wrong-owner or overly permissive existing files.
        match self.read_from(directory) {
            Ok(_) | Err(SettingsError::Missing | SettingsError::Malformed) => {}
            Err(error) => return Err(error),
        }
        let temporary = CString::new(format!(".analytics-{}.tmp", uuid::Uuid::new_v4()))
            .expect("UUID filename");
        let mut file = open_at(
            directory,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(io_error)?;
        let result = (|| {
            let bytes = serde_json::to_vec(saved).map_err(|_| SettingsError::Malformed)?;
            file.write_all(&bytes)
                .map_err(|_| SettingsError::Unreadable)?;
            file.sync_all().map_err(|_| SettingsError::Unreadable)?;
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    c"analytics.json".as_ptr(),
                )
            } != 0
            {
                return Err(SettingsError::Unreadable);
            }
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }
    pub fn set_enabled(
        &self,
        enabled: bool,
        release_enabled: bool,
    ) -> Result<SettingsStatus, String> {
        let directory = self
            .directory(true)
            .map_err(|error| error.reason().to_owned())?;
        let _lock = self
            .lock(&directory, false)
            .map_err(|error| error.reason().to_owned())?;
        let mut saved = match self.read_from(&directory) {
            Ok(saved) => saved,
            Err(SettingsError::Missing | SettingsError::Malformed) => SavedSettings::default(),
            Err(error) => return Err(error.reason().into()),
        };
        saved.enabled = enabled;
        self.write_to(&directory, &saved)
            .map_err(|error| error.reason().to_owned())?;
        Ok(self.status(release_enabled))
    }
    pub fn notice(&self, release: &str) -> bool {
        let Ok(directory) = self.directory(true) else {
            return false;
        };
        let Ok(_lock) = self.lock(&directory, true) else {
            return false;
        };
        let mut saved = match self.read_from(&directory) {
            Ok(saved) => saved,
            Err(SettingsError::Missing) => SavedSettings {
                enabled: true,
                notice_version: None,
            },
            Err(_) => return false,
        };
        if !saved.enabled || saved.notice_version.as_deref() == Some(release) {
            return false;
        }
        saved.notice_version = Some(release.into());
        self.write_to(&directory, &saved).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn fixture() -> (tempfile::TempDir, SettingsStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = SettingsStore::for_home(temp.path().to_path_buf(), unsafe { libc::geteuid() });
        (temp, store)
    }
    #[test]
    fn defaults_persist_and_release_gate_are_independent() {
        let (home, store) = fixture();
        assert!(store.status(true).effective_enabled);
        assert!(!store.status(false).effective_enabled);
        assert!(!store.set_enabled(false, true).unwrap().enabled);
        assert!(!store.status(true).effective_enabled);
        assert!(store.set_enabled(true, true).unwrap().enabled);
        let path = home.path().join(".config/gofin/analytics.json");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(!std::fs::read_to_string(path).unwrap().contains("id"));
    }
    #[test]
    fn malformed_unsafe_and_symlink_settings_fail_closed() {
        let (home, store) = fixture();
        store.set_enabled(true, true).unwrap();
        let path = home.path().join(".config/gofin/analytics.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(store.status(true).reason, "malformed_settings");
        assert!(!store.status(true).effective_enabled);
        store.set_enabled(false, true).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(store.status(true).reason, "unsafe_settings");
        assert!(store.set_enabled(true, true).is_err());
        std::fs::remove_file(&path).unwrap();
        symlink(home.path().join("target"), path).unwrap();
        assert_eq!(store.status(true).reason, "unsafe_settings");
        assert!(store.set_enabled(true, true).is_err());
    }
    #[test]
    fn unsafe_directory_and_hardlinks_fail_closed() {
        let (home, store) = fixture();
        store.set_enabled(true, true).unwrap();
        let path = home.path().join(".config/gofin/analytics.json");
        std::fs::hard_link(&path, home.path().join("linked")).unwrap();
        assert_eq!(store.status(true).reason, "unsafe_settings");
        std::fs::remove_file(home.path().join("linked")).unwrap();
        std::fs::set_permissions(
            home.path().join(".config"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert_eq!(store.status(true).reason, "unsafe_settings");
    }
    #[test]
    fn unreadable_files_and_special_files_suppress_reporting() {
        let (home, store) = fixture();
        store.set_enabled(true, true).unwrap();
        let path = home.path().join(".config/gofin/analytics.json");
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(store.status(true).reason, "unreadable_settings");
            assert!(!store.status(true).effective_enabled);
        }
        std::fs::remove_file(&path).unwrap();
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert_eq!(store.status(true).reason, "unsafe_settings");
        assert!(!store.status(true).effective_enabled);
        assert!(store.set_enabled(true, true).is_err());
    }
    #[test]
    fn notice_is_once_per_release_and_never_reenables_optout() {
        let (_home, store) = fixture();
        assert!(store.notice("0.2.0"));
        assert!(!store.notice("0.2.0"));
        assert!(store.notice("0.2.1"));
        store.set_enabled(false, true).unwrap();
        assert!(!store.notice("0.3.0"));
        assert!(!store.status(true).enabled);
    }
    #[test]
    fn writes_are_atomic_under_concurrent_notice_and_optout() {
        let (_home, store) = fixture();
        store.set_enabled(true, true).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..10 {
                    store.notice("0.2.0");
                }
            });
            scope.spawn(|| {
                store.set_enabled(false, true).unwrap();
            });
        });
        assert!(!store.status(true).enabled);
    }
    #[test]
    fn interactive_notice_drops_if_another_writer_holds_the_settings_lock() {
        let (_home, store) = fixture();
        store.set_enabled(true, true).unwrap();
        let directory = store.directory(false).unwrap();
        let _writer = store.lock(&directory, false).unwrap();
        assert!(!store.notice("0.2.0"));
    }
}
