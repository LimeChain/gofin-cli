//! Fixed-installation, metadata-only helpers for the public runtime updater.
//! No configuration, credentials, operation records or recovery code is loaded.
use gofin_domain::GoFinError;
use std::path::Path;

/// Owns the preparation outcome until the protected bootstrap replaces this
/// process. Only fixed lifecycle metadata reaches the event sink.
struct UpdatePreparation<'a> {
    sink: &'a dyn gofin_analytics::EventSink,
    from: &'a str,
    to: &'a str,
    stage: &'static str,
    owned: bool,
}

impl<'a> UpdatePreparation<'a> {
    fn new(sink: &'a dyn gofin_analytics::EventSink, from: &'a str, to: &'a str) -> Self {
        let report = Self {
            sink,
            from,
            to,
            stage: "prepare",
            owned: true,
        };
        report.emit(gofin_analytics::LifecycleOutcome::Attempted);
        report
    }

    fn emit(&self, outcome: gofin_analytics::LifecycleOutcome) {
        self.sink.send(&gofin_analytics::lifecycle_events(
            gofin_analytics::Lifecycle::Update,
            outcome,
            Some(self.from),
            Some(self.to),
            Some(self.stage),
            Some(gofin_analytics::Rollback::NotNeeded),
        ));
    }

    fn downloading(&mut self) {
        self.stage = "download";
    }

    fn transfer(&mut self) {
        self.owned = false;
    }

    fn exec_failed(&mut self) {
        self.owned = true;
    }
}

impl Drop for UpdatePreparation<'_> {
    fn drop(&mut self) {
        if self.owned {
            self.owned = false;
            self.emit(gofin_analytics::LifecycleOutcome::Failed);
        }
    }
}

/// The privileged entrypoint is compiled into the fixed root-owned broker. Root
/// never executes a user-writable downloaded updater or user-selected shell file.
pub async fn install_release(tag: &str) -> gofin_domain::Result<()> {
    use std::os::unix::process::CommandExt;
    if unsafe { libc::geteuid() } != 0
        || std::env::current_exe().ok().as_deref()
            != Some(Path::new("/Library/GoFin/runtime/gofin-broker"))
        || !tag.starts_with('v')
        || tag.len() > 100
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
    {
        return Err(refused(
            "Use gofin update from the installed human Terminal.",
        ));
    }
    let human_uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|uid| uid.parse::<u32>().ok())
        .filter(|uid| *uid >= 501)
        .ok_or_else(|| refused("Run gofin update from your own account using fresh sudo."))?;
    gofin_human_terminal::HumanTerminal::open(human_uid)?.finish()?;
    gofin_analytics::set_broker_user(human_uid);
    let sink = gofin_analytics::ProductionSink;
    let mut analytics = UpdatePreparation::new(&sink, compiled_version(), tag);
    validate_newer_beta(tag, compiled_version())?;
    analytics.downloading();
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| refused("The latest GoFin release could not be checked."))?;
    let mut response = client
        .get("https://limechain.github.io/gofin-cli/latest-beta.json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| refused("The latest GoFin release could not be checked."))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| refused("The latest GoFin release could not be checked."))?
    {
        if bytes.len() + chunk.len() > 16_384 {
            return Err(refused("Release metadata is invalid."));
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Metadata {
        schema_version: u32,
        tag: String,
    }
    let metadata: Metadata =
        serde_json::from_slice(&bytes).map_err(|_| refused("Release metadata is invalid."))?;
    if metadata.schema_version != 1 || metadata.tag != tag {
        return Err(refused(
            "The selected release no longer matches the latest completed GoFin beta. Retry gofin update.",
        ));
    }
    let bootstrap = bootstrap_script();
    // Successful exec transfers lifecycle ownership to the embedded bootstrap.
    // If exec returns, reclaim it so the failure has one terminal outcome.
    analytics.transfer();
    let error = std::process::Command::new("/bin/bash")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .arg("-c")
        .arg(bootstrap)
        .arg("gofin-protected-update")
        .arg(tag)
        .arg(compiled_version())
        .exec();
    analytics.exec_failed();
    Err(refused(&format!(
        "The protected updater could not start: {error}"
    )))
}

const ROOT_CLEANUP: &str = r#"cleanup() {
  gofin_status=$?
  trap - EXIT HUP INT TERM
  if [ -n "$gofin_child" ]; then kill -TERM "$gofin_child" 2>/dev/null || true; wait "$gofin_child" 2>/dev/null || true; fi
  if [ "${gofin_analytics_owned:-0}" = 1 ]; then
    gofin_analytics_owned=0
    if [ "${gofin_analytics_canceled:-0}" = 1 ]; then report_preparation canceled; else report_preparation failed; fi
  fi
  if [ -n "$gofin_work" ]; then /bin/rm -rf "$gofin_work"; fi
  exit "$gofin_status"
}
trap cleanup EXIT
trap 'gofin_analytics_canceled=1; exit 1' HUP INT TERM"#;

fn bootstrap_script() -> String {
    const INSTALLER: &str = include_str!("../../../scripts/install.sh");
    const UPDATER: &str = include_str!("../../../scripts/update-rust-macos-runtime.sh");
    format!(
        r##"
set -euo pipefail
umask 077
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH
gofin_work=''
gofin_child=''
gofin_analytics_owned=0
gofin_analytics_canceled=0
report_preparation() {{
  /usr/bin/sudo -n -H -u "#$SUDO_UID" /usr/bin/env -i \
    PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    /Library/GoFin/runtime/gofin internal-analytics-lifecycle --kind update \
    --outcome "$1" --stage download --from-version "$gofin_analytics_from" \
    --to-version "$gofin_analytics_to" --rollback not-needed \
    </dev/null >/dev/null 2>&1 || true
}}
gofin_analytics_from=$2
gofin_analytics_to=$1
{ROOT_CLEANUP}
[ -d /Library/GoFin ] && [ ! -L /Library/GoFin ] && [ "$(/usr/bin/stat -f '%u' /Library/GoFin)" = 0 ] || exit 1
/bin/ls -ld /Library/GoFin | /usr/bin/awk '{{if (substr($1,6,1)=="w" || substr($1,9,1)=="w" || index($1,"+")) bad=1}} END {{exit bad}}' || exit 1
if [ {analytics_enabled} = 1 ]; then
  gofin_analytics_owned=1
fi
gofin_work=$(/usr/bin/mktemp -d /Library/GoFin/.release-update.XXXXXX)
export TMPDIR="$gofin_work"
/bin/cat > "$gofin_work/install.sh" <<'GOFIN_COMPILED_INSTALLER'
{INSTALLER}
GOFIN_COMPILED_INSTALLER
/bin/cat > "$gofin_work/update.sh" <<'GOFIN_COMPILED_UPDATER'
{UPDATER}
GOFIN_COMPILED_UPDATER
/bin/sh "$gofin_work/install.sh" --version "$1" --download-only "$gofin_work/bundle" <&0 &
gofin_child=$!
wait "$gofin_child"
gofin_child=''
# The replacement script now owns the terminal outcome, including rollback.
# Set this before spawning so cancellation cannot produce duplicate outcomes.
gofin_analytics_owned=0
/bin/bash "$gofin_work/update.sh" --public-release --from /Library/GoFin/runtime --to "$gofin_work/bundle" <&0 &
gofin_child=$!
wait "$gofin_child"
gofin_child=''
"##,
        analytics_enabled = u8::from(gofin_analytics::release_enabled())
    )
}

fn compiled_version() -> &'static str {
    option_env!("GOFIN_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

fn validate_newer_beta(tag: &str, current: &str) -> gofin_domain::Result<()> {
    let incoming = tag
        .strip_prefix('v')
        .and_then(|value| semver::Version::parse(value).ok())
        .ok_or_else(|| refused("The update version is invalid."))?;
    let current = semver::Version::parse(current.trim_start_matches('v'))
        .map_err(|_| refused("The installed version cannot be compared safely."))?;
    let first = incoming.pre.as_str().split('.').next().unwrap_or_default();
    let beta = first == "beta"
        || first.strip_prefix("beta").is_some_and(|number| {
            !number.is_empty()
                && number.bytes().all(|c| c.is_ascii_digit())
                && (number.len() == 1 || !number.starts_with('0'))
        });
    if !beta || !incoming.build.is_empty() || tag != format!("v{incoming}") || incoming <= current {
        return Err(refused(
            "Only a newer completed GoFin beta can be installed by this updater.",
        ));
    }
    Ok(())
}

/// Called using the installed trusted broker, before executing any incoming
/// binary. Directory selection is confined to the root-owned staging namespace.
pub fn verify_staged_bundle(directory: &Path) -> gofin_domain::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let backup = directory
        .parent()
        .ok_or_else(|| refused("The update staging path is invalid."))?;
    if unsafe { libc::geteuid() } != 0
        || directory.file_name().is_none_or(|name| name != "incoming")
        || backup.parent() != Some(Path::new("/Library/GoFin"))
        || !backup
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("runtime-backup."))
    {
        return Err(refused("The update staging path is invalid."));
    }
    for path in [directory, backup, Path::new("/Library/GoFin")] {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| refused("The update staging metadata is unavailable."))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(refused("The update staging directory is unsafe."));
        }
    }
    let buildinfo = std::fs::read(directory.join("BUILDINFO"))
        .map_err(|_| refused("The staged release identity is unavailable."))?;
    validate_staged_identity(&buildinfo, compiled_version())?;
    verify_libraries(directory)
}

fn validate_staged_identity(buildinfo: &[u8], current: &str) -> gofin_domain::Result<()> {
    if buildinfo.len() > 16_384 {
        return Err(refused("The staged release identity is invalid."));
    }
    let text = std::str::from_utf8(buildinfo)
        .map_err(|_| refused("The staged release identity is invalid."))?;
    let mut identities = text
        .lines()
        .filter_map(|line| line.strip_prefix("release_version="));
    let tag = identities
        .next()
        .ok_or_else(|| refused("The staged release identity is missing."))?;
    if identities.next().is_some() {
        return Err(refused("The staged release identity is ambiguous."));
    }
    // This function executes inside the CURRENT fixed installed broker while
    // the updater owns its lock. Recheck to prevent a slow older update from
    // downgrading a release another updater installed during the download.
    validate_newer_beta(tag, current)
}

fn refused(message: &str) -> GoFinError {
    GoFinError::new(
        "UPDATE_COMPATIBILITY_REQUIRED",
        "update",
        message,
        "The installed runtime and saved state were preserved. Use a reviewed migration for schema changes.",
    )
}

/// The public updater holds this lock until runtime replacement finishes.
/// Exact schema equality deliberately prevents migrations before rollback is
/// committed. Versioned journal migrations need a separately reviewed updater.
pub async fn hold(parent: u32) -> gofin_domain::Result<()> {
    use std::io::Write;
    if unsafe { libc::geteuid() } != 0 || parent <= 1 || unsafe { libc::getppid() } as u32 != parent
    {
        return Err(refused(
            "Run this helper only from the protected runtime updater.",
        ));
    }
    let _guard = crate::try_lock_protected_configuration(true)?.ok_or_else(|| {
        refused("GoFin is busy. Finish active operations or enrollment and retry.")
    })?;
    let schema = gofin_store::update_compatibility::inspect_update_schema(
        Path::new(crate::OPERATIONS_PATH),
        0,
    )
    .map_err(|_| refused("The protected journal schema could not be inspected safely."))?;
    if schema != gofin_store::supported_schema_version() {
        return Err(refused(
            "This release requires a journal migration; automatic runtime update was refused.",
        ));
    }
    println!("compatible");
    std::io::stdout()
        .flush()
        .map_err(|_| refused("The update guard could not report readiness."))?;
    while unsafe { libc::getppid() } as u32 == parent {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(())
}

// Public users do not need Xcode's otool. Inspect the thin 64-bit Mach-O load
// commands ourselves; release bundles contain one binary per architecture.
fn libraries_are_system(bytes: &[u8]) -> bool {
    fn word(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
        ))
    }
    if word(bytes, 0) != Some(0xfeed_facf) {
        return false;
    }
    let Some(count) = word(bytes, 16) else {
        return false;
    };
    let Some(length) = word(bytes, 20) else {
        return false;
    };
    let Some(end) = 32usize.checked_add(length as usize) else {
        return false;
    };
    if end > bytes.len() || count == 0 || count > length / 8 {
        return false;
    }
    let mut offset = 32;
    for _ in 0..count {
        let Some(command) = word(bytes, offset) else {
            return false;
        };
        let Some(size) = word(bytes, offset + 4).map(|value| value as usize) else {
            return false;
        };
        let Some(next) = offset.checked_add(size) else {
            return false;
        };
        if size < 8 || next > end {
            return false;
        }
        // Reject rpaths altogether, matching the fixed OS-library contract.
        if command == 0x8000_001c {
            return false;
        }
        if matches!(command & 0x7fff_ffff, 0xc | 0x18 | 0x1f | 0x20 | 0x23) {
            let Some(name) = word(bytes, offset + 8).map(|value| value as usize) else {
                return false;
            };
            if name < 24 || name >= size {
                return false;
            }
            let tail = &bytes[offset + name..next];
            let Some(nul) = tail.iter().position(|byte| *byte == 0) else {
                return false;
            };
            let path = &tail[..nul];
            if !(path.starts_with(b"/usr/lib/") || path.starts_with(b"/System/Library/"))
                || path.windows(3).any(|part| part == b"/..")
            {
                return false;
            }
        }
        offset = next;
    }
    offset == end
}

fn verify_libraries(directory: &Path) -> gofin_domain::Result<()> {
    for name in ["gofin", "gofin-broker", "gofin-touch-id"] {
        let bytes = std::fs::read(directory.join(name))
            .map_err(|_| refused("A staged executable is unavailable."))?;
        if !libraries_are_system(&bytes) {
            return Err(refused(
                "A staged executable uses unsupported libraries or Mach-O format.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockAnalytics(std::cell::RefCell<Vec<serde_json::Value>>);

    impl gofin_analytics::EventSink for MockAnalytics {
        fn send(&self, events: &[gofin_analytics::Event]) {
            self.0
                .borrow_mut()
                .extend(events.iter().map(gofin_analytics::Event::capture));
        }
    }

    #[test]
    fn update_preparation_reports_attempt_and_fixed_failure_stage() {
        for downloading in [false, true] {
            let sink = MockAnalytics::default();
            {
                let mut reporter = UpdatePreparation::new(&sink, "0.2.0-beta.1", "v0.2.0-beta.2");
                if downloading {
                    reporter.downloading();
                }
            }
            let events = sink.0.borrow();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0]["event"], "gofin.update");
            assert_eq!(events[0]["properties"]["outcome"], "attempted");
            assert_eq!(events[1]["properties"]["outcome"], "failed");
            assert_eq!(events[2]["event"], "$exception");
            assert_eq!(
                events[1]["properties"]["stage"],
                if downloading { "download" } else { "prepare" }
            );
        }
    }

    #[test]
    fn update_preparation_transfers_ownership_and_reclaims_failed_exec() {
        for failed_exec in [false, true] {
            let sink = MockAnalytics::default();
            {
                let mut reporter = UpdatePreparation::new(&sink, "0.2.0-beta.1", "v0.2.0-beta.2");
                reporter.downloading();
                reporter.transfer();
                if failed_exec {
                    reporter.exec_failed();
                }
            }
            let events = sink.0.borrow();
            assert_eq!(events.len(), if failed_exec { 3 } else { 1 });
            assert_eq!(events[0]["properties"]["outcome"], "attempted");
            if failed_exec {
                assert_eq!(events[1]["properties"]["outcome"], "failed");
            }
        }
    }
    #[test]
    fn refuses_same_version_downgrade_stable_and_invalid_beta_before_download() {
        for tag in [
            "v0.2.0-beta.1",
            "v0.1.0-beta.9",
            "v0.2.0",
            "v0.3.0-alpha.1",
            "v0.3.0-beta01",
            "v0.3.0-beta.2+other",
        ] {
            assert!(validate_newer_beta(tag, "0.2.0-beta.1").is_err(), "{tag}");
        }
        assert!(validate_newer_beta("v0.2.0-beta.2", "v0.2.0-beta.1").is_ok());
    }

    #[test]
    fn locked_staging_check_refuses_stale_update_after_another_upgrade() {
        let metadata = b"release_version=v0.2.0-beta.2\nsource_state=clean\n";
        assert!(validate_staged_identity(metadata, "v0.2.0-beta.1").is_ok());
        assert!(validate_staged_identity(metadata, "v0.2.0-beta.2").is_err());
        assert!(validate_staged_identity(metadata, "v0.2.0-beta.3").is_err());
        assert!(
            validate_staged_identity(
                b"release_version=v0.2.0-beta.2\nrelease_version=v0.3.0-beta.1\n",
                "0.2.0-beta.1"
            )
            .is_err()
        );
    }

    #[test]
    fn privileged_bootstrap_uses_only_embedded_scripts_and_root_private_temporary_files() {
        let script = bootstrap_script();
        assert!(script.contains("export TMPDIR=\"$gofin_work\""));
        assert!(script.contains("mktemp -d /tmp/gofin-download.XXXXXX"));
        assert!(script.contains("kill -TERM \"$gofin_child\""));
        assert!(script.contains("wait \"$gofin_child\""));
        assert!(!script.contains("--download-only \"$gofin_work/bundle\" |"));
        assert!(script.contains("internal-analytics-lifecycle --kind update"));
        assert!(script.contains("--outcome \"$1\" --stage download"));
        assert!(script.contains("/usr/bin/sudo -n -H -u \"#$SUDO_UID\" /usr/bin/env -i"));
        assert!(!script.contains("report_preparation attempted"));
        let mut child = std::process::Command::new("/bin/bash")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn preparation_cleanup_reports_only_while_it_owns_the_outcome() {
        for (owned, command, expected, code) in [
            (1, "exit 7", "failed", 7),
            (1, "kill -TERM $$", "canceled", 1),
            (0, "exit 7", "", 7),
        ] {
            let script = format!(
                r#"
set -eu
gofin_work=''
gofin_child=''
gofin_analytics_owned={owned}
gofin_analytics_canceled=0
report_preparation() {{ printf '%s' "$1"; }}
{ROOT_CLEANUP}
{command}
"#
            );
            let output = std::process::Command::new("/bin/bash")
                .arg("-c")
                .arg(script)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(output.stdout, expected.as_bytes());
        }
    }

    #[test]
    fn interrupted_bootstrap_waits_for_child_cleanup_before_removing_downloads() {
        let fixture = tempfile::tempdir().unwrap();
        let work = fixture.path().join("download");
        std::fs::create_dir(&work).unwrap();
        let ready = fixture.path().join("ready");
        let finished = fixture.path().join("finished");
        let script = format!(
            r#"
set -eu
gofin_work=$1
gofin_child=''
{ROOT_CLEANUP}
/bin/bash -c 'trap '\''[ -d "$1" ] && printf done > "$3"; exit 0'\'' TERM; printf ready > "$2"; while true; do /bin/sleep 0.05; done' gofin-fixture "$1" "$2" "$3" &
gofin_child=$!
wait "$gofin_child"
"#
        );
        let mut child = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(script)
            .arg("gofin-fixture")
            .arg(&work)
            .arg(&ready)
            .arg(&finished)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("The disposable updater child did not become ready.");
        }
        // Target only our disposable shell, never the user's process group.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
        assert!(!child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(finished).unwrap(), "done");
        assert!(!work.exists());
    }
    fn image(path: &[u8]) -> Vec<u8> {
        let size = (24 + path.len() + 1).div_ceil(8) * 8;
        let mut bytes = vec![0; 32 + size];
        for (offset, value) in [
            (0, 0xfeed_facfu32),
            (16, 1),
            (20, size as u32),
            (32, 0xc),
            (36, size as u32),
            (40, 24),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[56..56 + path.len()].copy_from_slice(path);
        bytes
    }
    #[test]
    fn accepts_only_bounded_system_load_commands_without_xcode() {
        assert!(libraries_are_system(&image(b"/usr/lib/libSystem.B.dylib")));
        assert!(libraries_are_system(&image(
            b"/System/Library/Frameworks/Security.framework/Security"
        )));
        for path in [
            b"@rpath/untrusted".as_slice(),
            b"/opt/homebrew/lib/unsafe.dylib",
            b"/usr/lib/../../tmp/untrusted",
        ] {
            assert!(!libraries_are_system(&image(path)));
        }
        let mut truncated = image(b"/usr/lib/libSystem.B.dylib");
        truncated.pop();
        assert!(!libraries_are_system(&truncated));
        assert!(!libraries_are_system(b"not a Mach-O image"));
    }
}
