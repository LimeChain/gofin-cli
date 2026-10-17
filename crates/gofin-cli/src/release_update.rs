//! Public update handoff. Only the fixed installed broker runs with privilege.
use gofin_domain::GoFinError;
use serde_json::Value;

fn failure(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "update",
        message,
        "Run gofin update yourself in a fresh macOS Terminal window. Saved connections, policy and history are preserved.",
    )
}

#[cfg(target_os = "macos")]
const HANDOFF: &str = r#"
set -eu
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH DYLD_FRAMEWORK_PATH
gofin_reporter=$2
gofin_context=$3
cleanup() {
  gofin_status=$?
  trap - EXIT HUP INT TERM
  /usr/bin/sudo -K || true
  printf '%s' "$gofin_context" | "$gofin_reporter" internal-analytics-command --exit-code "$gofin_status" >/dev/null 2>&1 || true
  exit "$gofin_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
printf '%s\n' 'GoFin needs your Mac login password to verify and update its protected runtime. Connections, policy and history are kept.' >&2
/usr/bin/sudo -k -p 'Mac login password for %p: ' -- /Library/GoFin/runtime/gofin-broker --runtime-update-release "$1"
"#;

pub(crate) async fn run() -> Result<Value, GoFinError> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        return Err(failure(
            "HUMAN_TERMINAL_REQUIRED",
            "Updates require your own interactive Terminal without redirection.",
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(failure(
            "PLATFORM_UPDATE_UNAVAILABLE",
            "Protected runtime updates require macOS.",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        let installed = std::path::Path::new("/Library/GoFin/runtime/gofin");
        if std::env::current_exe().ok().as_deref() != Some(installed)
            || unsafe { libc::geteuid() } == 0
        {
            return Err(failure(
                "INSTALLED_RUNTIME_REQUIRED",
                "Use the installed GoFin command from your own account to update.",
            ));
        }
        let release = super::updates::latest_release().await?;
        let current =
            semver::Version::parse(super::VERSION.trim_start_matches('v')).map_err(|_| {
                failure(
                    "UPDATE_VERSION_UNKNOWN",
                    "The installed version cannot be compared safely.",
                )
            })?;
        if release.version <= current {
            return Ok(serde_json::json!({"status":"up_to_date", "version":super::VERSION}));
        }
        eprintln!("Updating GoFin to {}.", release.tag);
        // exec removes the gofin process from the lineage and idle-client guard.
        // The wrapper is immutable command text, never a user-writable script.
        let error = std::process::Command::new("/bin/bash")
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .arg("-c")
            .arg(HANDOFF)
            .arg("gofin-update")
            .arg(release.tag)
            .arg(installed)
            .arg(
                serde_json::to_string(&gofin_analytics::CommandContext::new(
                    "update", None, None, false,
                ))
                .unwrap_or_default(),
            )
            .exec();
        Err(failure(
            "UPDATE_HANDOFF_FAILED",
            &format!("The update could not start: {error}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn redirected_update_never_contacts_network_or_privilege() {
        let error = super::run().await.unwrap_err();
        assert_eq!(error.code, "HUMAN_TERMINAL_REQUIRED");
    }
}
