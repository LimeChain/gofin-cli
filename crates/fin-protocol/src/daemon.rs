//! Protected marker used to persist an explicit broker stop request.

use fin_domain::FinError;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub const STOP_MARKER_PATH: &str = "/Library/Fin/run/daemon-stopped";
pub const SERVICE_TARGET: &str = "system/io.fin-cli.broker";

const MARKER_CONTENTS: &[u8] = b"Fin broker stopped\n";

fn marker_error(code: &'static str) -> FinError {
    FinError::new(
        code,
        "daemon",
        "The protected broker stop marker could not be safely accessed.",
        "Check the protected Fin run directory and restart the broker through its service manager.",
    )
}

/// Return whether the protected stop intent is present. Invalid marker state
/// is an error so callers fail closed instead of treating it as running.
pub fn stop_requested() -> Result<bool, FinError> {
    stop_requested_at(Path::new(STOP_MARKER_PATH), 0)
}

/// Persist stop intent atomically before the broker stops accepting peers.
pub fn request_stop() -> Result<(), FinError> {
    request_stop_at(Path::new(STOP_MARKER_PATH), 0)
}

/// Remove stop intent only for an explicit start request.
pub fn resume() -> Result<(), FinError> {
    resume_at(Path::new(STOP_MARKER_PATH), 0)
}

/// Path-injected form for synthetic broker composition tests. Production code
/// should use the fixed-path public functions above.
#[doc(hidden)]
pub fn stop_requested_at(path: &Path, owner_uid: u32) -> Result<bool, FinError> {
    let parent = path
        .parent()
        .ok_or_else(|| marker_error("DAEMON_MARKER_INVALID"))?;
    match fs::symlink_metadata(parent) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(marker_error("DAEMON_MARKER_INVALID")),
        Ok(_) => {}
    }
    validate_directory(parent, owner_uid)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_marker_metadata(&metadata, owner_uid)?;
            let contents = fs::read(path).map_err(|_| marker_error("DAEMON_MARKER_INVALID"))?;
            if contents != MARKER_CONTENTS {
                return Err(marker_error("DAEMON_MARKER_INVALID"));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(marker_error("DAEMON_MARKER_INVALID")),
    }
}

/// Path-injected form for synthetic broker composition tests.
#[doc(hidden)]
pub fn request_stop_at(path: &Path, owner_uid: u32) -> Result<(), FinError> {
    let parent = path
        .parent()
        .ok_or_else(|| marker_error("DAEMON_MARKER_INVALID"))?;
    validate_directory(parent, owner_uid)?;
    if path.exists() || fs::symlink_metadata(path).is_ok() {
        stop_requested_at(path, owner_uid)?;
    }

    let temporary = temporary_path(path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| marker_error("DAEMON_MARKER_WRITE_FAILED"))?;
    let write_result = file
        .write_all(MARKER_CONTENTS)
        .and_then(|_| file.sync_all());
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(marker_error("DAEMON_MARKER_WRITE_FAILED"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .set_permissions(fs::Permissions::from_mode(0o644))
            .and_then(|_| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return Err(marker_error("DAEMON_MARKER_WRITE_FAILED"));
        }
    }
    drop(file);
    if fs::rename(&temporary, path).is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(marker_error("DAEMON_MARKER_WRITE_FAILED"));
    }
    sync_directory(parent)?;
    stop_requested_at(path, owner_uid).map(|_| ())
}

/// Path-injected form for synthetic broker composition tests.
#[doc(hidden)]
pub fn resume_at(path: &Path, owner_uid: u32) -> Result<(), FinError> {
    let parent = path
        .parent()
        .ok_or_else(|| marker_error("DAEMON_MARKER_INVALID"))?;
    validate_directory(parent, owner_uid)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_marker_metadata(&metadata, owner_uid)?;
            let contents = fs::read(path).map_err(|_| marker_error("DAEMON_MARKER_INVALID"))?;
            if contents != MARKER_CONTENTS {
                return Err(marker_error("DAEMON_MARKER_INVALID"));
            }
            fs::remove_file(path).map_err(|_| marker_error("DAEMON_MARKER_WRITE_FAILED"))?;
            sync_directory(parent)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(marker_error("DAEMON_MARKER_INVALID")),
    }
}

fn validate_directory(path: &Path, owner_uid: u32) -> Result<(), FinError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| marker_error("DAEMON_MARKER_INVALID"))?;
    if !metadata.file_type().is_dir() || metadata_uid(&metadata) != owner_uid {
        return Err(marker_error("DAEMON_MARKER_INVALID"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o022 != 0 {
            return Err(marker_error("DAEMON_MARKER_INVALID"));
        }
    }
    Ok(())
}

fn validate_marker_metadata(metadata: &fs::Metadata, owner_uid: u32) -> Result<(), FinError> {
    if !metadata.file_type().is_file() || metadata_uid(metadata) != owner_uid {
        return Err(marker_error("DAEMON_MARKER_INVALID"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o777 != 0o644 || metadata.nlink() != 1 {
            return Err(marker_error("DAEMON_MARKER_INVALID"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_uid(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.uid()
}

#[cfg(not(unix))]
fn metadata_uid(_: &fs::Metadata) -> u32 {
    0
}

fn temporary_path(path: &Path) -> PathBuf {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{filename}.{}.tmp", std::process::id()))
}

fn sync_directory(path: &Path) -> Result<(), FinError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| marker_error("DAEMON_MARKER_WRITE_FAILED"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_directory() -> PathBuf {
        static NEXT_DIRECTORY: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fin-daemon-marker-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn directory_uid(path: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(path).unwrap().uid()
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            0
        }
    }

    #[test]
    fn marker_persists_across_reads_and_only_explicit_resume_clears_it() {
        let directory = temporary_directory();
        let marker = directory.join("daemon-stopped");
        let uid = directory_uid(&directory);
        assert!(!stop_requested_at(&marker, uid).unwrap());
        request_stop_at(&marker, uid).unwrap();
        assert!(stop_requested_at(&marker, uid).unwrap());
        assert_eq!(fs::read(&marker).unwrap(), MARKER_CONTENTS);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = fs::metadata(&marker).unwrap().mode() & 0o777;
            assert_eq!(
                mode, 0o644,
                "the stop marker is public and non-owner-readable"
            );
            assert_eq!(mode & 0o444, 0o444);
            assert_eq!(mode & 0o022, 0);
        }
        assert!(stop_requested_at(&marker, uid).unwrap());
        resume_at(&marker, uid).unwrap();
        assert!(!stop_requested_at(&marker, uid).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_marker_and_symlink_fail_closed() {
        let directory = temporary_directory();
        let marker = directory.join("daemon-stopped");
        let uid = directory_uid(&directory);
        fs::write(&marker, b"stopped\n").unwrap();
        assert_eq!(
            stop_requested_at(&marker, uid).unwrap_err().code,
            "DAEMON_MARKER_INVALID"
        );
        fs::remove_file(&marker).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(directory.join("target"), &marker).unwrap();
        #[cfg(unix)]
        assert_eq!(
            resume_at(&marker, uid).unwrap_err().code,
            "DAEMON_MARKER_INVALID"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn absent_run_directory_means_no_stop_request() {
        let directory = temporary_directory();
        let missing_parent = directory.join("missing").join("daemon-stopped");
        let uid = directory_uid(&directory);
        assert!(!stop_requested_at(&missing_parent, uid).unwrap());
        let _ = fs::remove_dir_all(directory);
    }
}
