//! Optional public release notices. Never open broker state or delay a command for HTTP.
use gofin_domain::GoFinError;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENDPOINT: &str = "https://limechain.github.io/gofin-cli/latest-beta.json";
const MAX_METADATA: usize = 16_384;
const CHECK_INTERVAL: u64 = 24 * 60 * 60;

pub(crate) struct Release {
    pub tag: String,
    pub version: Version,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema_version: u32,
    tag: String,
}

fn unavailable() -> GoFinError {
    GoFinError::new(
        "UPDATE_CHECK_UNAVAILABLE",
        "update",
        "The latest completed GoFin release could not be checked.",
        "Check your internet connection and try gofin update again. Your installation was preserved.",
    )
}

fn parse_release(bytes: &[u8]) -> Result<Release, GoFinError> {
    if bytes.len() > MAX_METADATA {
        return Err(unavailable());
    }
    let metadata: Metadata = serde_json::from_slice(bytes).map_err(|_| unavailable())?;
    if metadata.schema_version != 1 {
        return Err(unavailable());
    }
    let version = metadata
        .tag
        .strip_prefix('v')
        .and_then(|s| Version::parse(s).ok())
        .ok_or_else(unavailable)?;
    let first = version.pre.as_str().split('.').next().unwrap_or_default();
    let supported = version.pre.is_empty()
        || ["beta", "rc"].into_iter().any(|channel| {
            first == channel
                || first.strip_prefix(channel).is_some_and(|number| {
                    !number.is_empty()
                        && number.bytes().all(|c| c.is_ascii_digit())
                        && (number.len() == 1 || !number.starts_with('0'))
                })
        });
    if !supported || !version.build.is_empty() || metadata.tag != format!("v{version}") {
        return Err(unavailable());
    }
    Ok(Release {
        tag: metadata.tag,
        version,
    })
}

pub(crate) async fn latest_release() -> Result<Release, GoFinError> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(3))
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .user_agent("GoFin update check")
        .build()
        .map_err(|_| unavailable())?;
    let mut response = client
        .get(ENDPOINT)
        .send()
        .await
        .map_err(|_| unavailable())?
        .error_for_status()
        .map_err(|_| unavailable())?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA as u64)
    {
        return Err(unavailable());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len() + chunk.len() > MAX_METADATA {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_release(&bytes)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    checked_at: u64,
    tag: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn safe_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        prelude::PermissionsExt,
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid reads process identity only.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(std::io::Error::other("Unsafe update cache"));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn safe_file(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn read_cache(directory: &Path) -> Cache {
    let result = (|| {
        let mut bytes = Vec::new();
        safe_file(&directory.join("update.json"))?
            .take((MAX_METADATA + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_METADATA {
            return Err(std::io::Error::other("Oversized cache"));
        }
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    })();
    result.unwrap_or_default()
}

fn write_cache(directory: &Path, cache: &Cache) -> std::io::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, cache)?;
    temporary.flush()?;
    temporary
        .persist(directory.join("update.json"))
        .map_err(|e| e.error)?;
    Ok(())
}

fn cache_directory() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Elevated invocations must never touch user files or spawn a checker.
        if unsafe { libc::geteuid() } < 501 {
            return None;
        }
        let directory = PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/GoFin");
        fs::create_dir_all(&directory).ok()?;
        let metadata = fs::symlink_metadata(&directory).ok()?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return None;
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).ok()?;
        Some(directory)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

struct CheckLock(File);
impl CheckLock {
    #[cfg(unix)]
    fn acquire(directory: &Path) -> Option<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("update.lock"))
            .ok()?;
        if !file.metadata().ok()?.is_file()
            || file.metadata().ok()?.uid() != unsafe { libc::geteuid() }
        {
            return None;
        }
        // SAFETY: flock holds only this cache lock, released on close/exit.
        (unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0)
            .then_some(Self(file))
    }
    #[cfg(not(unix))]
    fn acquire(_directory: &Path) -> Option<Self> {
        None
    }
}
impl Drop for CheckLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

fn newer_tag(cache: &Cache, installed: &str) -> Option<String> {
    let tag = cache.tag.as_ref()?;
    let release = parse_release(
        &serde_json::to_vec(&serde_json::json!({"schema_version":1,"tag":tag})).ok()?,
    )
    .ok()?;
    let current = Version::parse(installed).ok()?;
    release
        .version
        .cmp_precedence(&current)
        .is_gt()
        .then_some(release.tag)
}

fn due(cache: &Cache, time: u64) -> bool {
    // A clock adjustment or forged far-future cache must not suppress checks forever.
    cache.checked_at == 0 || time < cache.checked_at || time - cache.checked_at >= CHECK_INTERVAL
}

pub(crate) fn notice_enabled(
    machine: bool,
    interactive: bool,
    installed: bool,
    internal: bool,
) -> bool {
    !machine && interactive && installed && !internal
}

/// Called only by the installed binary's entry point after a successful command.
/// Cached reads and spawn are local; the background helper owns all HTTP waits.
pub(crate) fn notify(arguments: &[std::ffi::OsString], installed_version: &str) {
    use std::io::IsTerminal;
    let internal = arguments.iter().skip(1).any(|arg| {
        arg == "update"
            || arg == "analytics"
            || arg.to_str().is_some_and(|s| s.starts_with("internal-"))
            || arg == "completions"
    });
    let machine = super::requested_json(arguments);
    let executable = std::env::current_exe().ok();
    if !notice_enabled(
        machine,
        std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal(),
        cfg!(target_os = "macos")
            && executable.as_deref() == Some(Path::new("/Library/GoFin/runtime/gofin")),
        internal,
    ) || std::env::var_os("GOFIN_NO_UPDATE_CHECK").is_some()
    {
        return;
    }
    let Some(directory) = cache_directory() else {
        return;
    };
    let cache = read_cache(&directory);
    if let Some(tag) = newer_tag(&cache, installed_version) {
        let _ = gofin_presentation::Ui::new(false, false).warning(&format!(
            "GoFin {tag} is available. Run gofin update in your Terminal to update."
        ));
    }
    let Some(_lock) = CheckLock::acquire(&directory) else {
        return;
    };
    let mut cache = read_cache(&directory);
    let time = now();
    if !due(&cache, time) {
        return;
    }
    cache.checked_at = time;
    if write_cache(&directory, &cache).is_err() {
        return;
    }
    drop(_lock);
    if let Some(executable) = executable {
        let _ = Command::new(executable)
            .arg("internal-update-check")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

pub(crate) async fn refresh_cache() {
    let Some(directory) = cache_directory() else {
        return;
    };
    let Some(_lock) = CheckLock::acquire(&directory) else {
        return;
    };
    let mut cache = read_cache(&directory);
    cache.checked_at = now();
    if let Ok(release) = latest_release().await {
        cache.tag = Some(release.tag);
    }
    let _ = write_cache(&directory, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(tag: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"schema_version":1,"tag":tag})).unwrap()
    }
    #[test]
    fn accepts_only_canonical_supported_release_metadata() {
        for tag in [
            "v0.0.0",
            "v0.2.0",
            "v1.2.3",
            "v0.2.0-beta.1",
            "v0.2.0-beta2",
            "v1.0.0-beta",
            "v0.3.0-rc",
            "v0.3.0-rc0",
            "v0.3.0-rc2",
            "v0.3.0-rc.2",
        ] {
            assert!(parse_release(&metadata(tag)).is_ok(), "{tag}");
        }
        for tag in [
            "v0.02.0",
            "0.3.0",
            "v0.3.0+build",
            "v0.2.0-alpha.1",
            "v0.2.0-beta01",
            "0.2.0-beta.1",
            "v0.2.0-beta.01",
            "v0.2.0-beta.1+build",
            "v0.2.0-beta; echo BAD",
            "v0.2.0-beta\nGoFin",
            "v0.3.0-rc01",
            "v0.3.0-rc.01",
            "v0.3.0-rcandidate",
            "v0.3.0-rc--2",
            "v0.3.0-rc2+build",
        ] {
            assert!(parse_release(&metadata(tag)).is_err(), "{tag}");
        }
        assert!(parse_release(br#"{"schema_version":2,"tag":"v0.2.0-beta.1"}"#).is_err());
        assert!(parse_release(&vec![b' '; MAX_METADATA + 1]).is_err());
    }
    #[test]
    fn compares_semantic_versions_without_downgrade_or_same_version_notice() {
        let cache = Cache {
            checked_at: 1,
            tag: Some("v0.2.0-beta.10".into()),
        };
        assert!(newer_tag(&cache, "0.2.0-beta.2").is_some());
        for current in ["0.2.0-beta.10", "0.2.0", "0.3.0-beta.1", "invalid"] {
            assert!(newer_tag(&cache, current).is_none());
        }
    }
    #[test]
    fn notices_never_reach_machine_redirected_development_or_internal_commands() {
        assert!(notice_enabled(false, true, true, false));
        for (machine, interactive, installed, internal) in [
            (true, true, true, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
        ] {
            assert!(!notice_enabled(machine, interactive, installed, internal));
        }
    }
    #[cfg(unix)]
    #[test]
    fn cache_symlinks_are_not_followed_or_written_through() {
        let temp = tempfile::tempdir().unwrap();
        let other = temp.path().join("sentinel");
        fs::write(&other, "preserve").unwrap();
        std::os::unix::fs::symlink(&other, temp.path().join("update.json")).unwrap();
        assert!(read_cache(temp.path()).tag.is_none());
        write_cache(temp.path(), &Cache::default()).unwrap();
        assert_eq!(fs::read_to_string(&other).unwrap(), "preserve");
        let _held = CheckLock::acquire(temp.path()).unwrap();
        assert!(CheckLock::acquire(temp.path()).is_none());
    }
    #[cfg(unix)]
    #[test]
    fn fifo_cache_cannot_delay_the_foreground_command() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("update.json");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated temporary fixture path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let directory = temp.path().to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(read_cache(&directory).tag).unwrap());
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
    }
}
