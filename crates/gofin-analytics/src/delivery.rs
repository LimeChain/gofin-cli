use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::json;

use crate::Event;

pub const PROXY_URL: &str = "https://gofin-analytics-proxy-fj4qllww7q-ew.a.run.app/batch/";
// PostHog's public project ingestion key. This is neither a query nor an admin credential.
const PROJECT_TOKEN: &str = "phc_sixeA8gDXU2WE4CmKzx5AnwZPzUhf6XThaLr4Xtiqy5i";
pub const INSTALLED_HELPER: &str = "/Library/GoFin/runtime/gofin";
pub const MAX_PAYLOAD: usize = 4096;
static CHILDREN: AtomicUsize = AtomicUsize::new(0);

/// Complete, inspectable process description. Production never invokes a shell.
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub input: Vec<u8>,
    pub detached: bool,
}
pub trait ProcessLauncher {
    fn launch(&self, process: ProcessSpec) -> io::Result<()>;
}
pub struct SystemProcessLauncher;

impl ProcessLauncher for SystemProcessLauncher {
    fn launch(&self, process: ProcessSpec) -> io::Result<()> {
        if process.input.len() > MAX_PAYLOAD {
            return Err(io::Error::other("analytics payload too large"));
        }
        if CHILDREN
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                (count < 4).then_some(count + 1)
            })
            .is_err()
        {
            return Err(io::Error::other("analytics send limit"));
        }
        let result = launch_child(process);
        if result.is_err() {
            CHILDREN.fetch_sub(1, Ordering::AcqRel);
        }
        result
    }
}
fn launch_child(process: ProcessSpec) -> io::Result<()> {
    let mut command = Command::new(process.executable);
    command
        .args(process.arguments)
        .env_clear()
        .envs(process.environment)
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if process.detached {
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command.spawn()?;
    let Some(input) = child.stdin.take() else {
        let _ = child.kill();
        // Retain this occupied slot if an OS invariant fails after spawn.
        return Ok(());
    };
    let fd = input.as_raw_fd();
    // A bounded nonblocking write cannot make the foreground command wait for HTTP.
    // If the pipe cannot accept the complete payload, drop this attempt.
    let written = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            -1
        } else {
            libc::write(fd, process.input.as_ptr().cast(), process.input.len())
        }
    };
    drop(input);
    if written != process.input.len() as isize {
        let _ = child.kill();
    }
    // Broker sends need reaping; CLI exit may reparent its already detached child.
    // At most four such threads/processes exist per process.
    match std::thread::Builder::new()
        .name("gofin-analytics-reap".into())
        .spawn(move || {
            let _ = child.wait();
            CHILDREN.fetch_sub(1, Ordering::AcqRel);
        }) {
        Ok(_) => Ok(()),
        // The child handle was dropped by the failed thread spawn, but the OS
        // child can still exist. Keep its slot occupied for this process lifetime
        // instead of permitting an unbounded series of unreaped children.
        Err(_) => Ok(()),
    }
}

pub fn capture_process(events: &[Event], installation_id: uuid::Uuid) -> Option<ProcessSpec> {
    if events.is_empty() || events.len() > 3 {
        return None;
    }
    let input = serde_json::to_vec(&json!({"api_key":PROJECT_TOKEN,"batch":events.iter().map(|event| event.capture_for_installation(installation_id)).collect::<Vec<_>>()})).ok()?;
    if input.len() > MAX_PAYLOAD {
        return None;
    }
    Some(ProcessSpec {
        executable: "/usr/bin/curl".into(),
        arguments: [
            "--disable",
            "--silent",
            "--fail",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--retry",
            "0",
            "--max-redirs",
            "0",
            "--proto",
            "=https",
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            PROXY_URL,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
        input,
        detached: true,
    })
}

pub fn delegate_process(uid: u32, helper: &Path, events: &[Event]) -> Option<ProcessSpec> {
    if uid == 0 || events.is_empty() || events.len() > 3 || !helper.is_absolute() {
        return None;
    }
    let mut events = events.to_vec();
    for event in &mut events {
        event.sanitize();
    }
    let input = serde_json::to_vec(&events).ok()?;
    if input.len() > MAX_PAYLOAD {
        return None;
    }
    Some(ProcessSpec {
        executable: "/usr/bin/sudo".into(),
        arguments: vec![
            "-n".into(),
            "-H".into(),
            "-u".into(),
            format!("#{uid}"),
            "--".into(),
            "/usr/bin/env".into(),
            "-i".into(),
            "PATH=/usr/bin:/bin".into(),
            helper.to_str()?.into(),
            "internal-analytics-send".into(),
        ],
        environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
        input,
        detached: true,
    })
}

pub(crate) fn protected_helper(helper: &Path) -> bool {
    if !helper.is_absolute() {
        return false;
    }
    helper.ancestors().all(|path| {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && !metadata.file_type().is_symlink()
            && if path == helper {
                metadata.is_file() && metadata.mode() & 0o111 != 0
            } else {
                metadata.is_dir()
            }
    })
}

/// Resolve current account settings and installation identity at the send boundary.
/// The launcher is injectable so tests never start a subprocess.
pub fn deliver_with(
    events: &[Event],
    store: &crate::SettingsStore,
    release_enabled: bool,
    launcher: &dyn ProcessLauncher,
) {
    if let Some(installation_id) = store.installation_id(release_enabled)
        && let Some(process) = capture_process(events, installation_id)
    {
        let _ = launcher.launch(process);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandContext, Outcome};
    use std::sync::Mutex;
    #[derive(Default)]
    struct MockLauncher {
        processes: Mutex<Vec<ProcessSpec>>,
        fail: bool,
    }
    impl ProcessLauncher for MockLauncher {
        fn launch(&self, process: ProcessSpec) -> io::Result<()> {
            self.processes.lock().unwrap().push(process);
            if self.fail {
                Err(io::Error::other("synthetic spawn failure"))
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn delivery_is_one_bounded_detached_attempt_with_payload_only_on_stdin() {
        let events = CommandContext::new("read", Some("ethereum.balance"), None, true)
            .events(Outcome::Succeeded, None);
        let launcher = MockLauncher::default();
        let home = tempfile::tempdir().unwrap();
        let store =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        deliver_with(&events, &store, true, &launcher);
        let processes = launcher.processes.lock().unwrap();
        assert_eq!(processes.len(), 1);
        let process = &processes[0];
        assert!(process.detached);
        assert_eq!(process.executable, Path::new("/usr/bin/curl"));
        assert_eq!(process.arguments[0], "--disable");
        assert!(
            process
                .arguments
                .windows(2)
                .any(|args| args == ["--max-time", "10"])
        );
        assert!(
            process
                .arguments
                .windows(2)
                .any(|args| args == ["--connect-timeout", "5"])
        );
        assert!(
            !process
                .arguments
                .iter()
                .any(|arg| arg.contains(PROJECT_TOKEN))
        );
        let value: serde_json::Value = serde_json::from_slice(&process.input).unwrap();
        assert_eq!(value["batch"].as_array().unwrap().len(), 1);
        let installation_id = store.installation_id(true).unwrap().to_string();
        assert_eq!(value["batch"][0]["distinct_id"], installation_id);
        assert_eq!(
            value["batch"][0]["properties"]["$process_person_profile"],
            false
        );
        assert_eq!(value["batch"][0]["properties"]["$geoip_disable"], true);
        assert_eq!(
            value["batch"][0]["properties"]["$ip"],
            serde_json::Value::Null
        );
        assert_eq!(
            process.environment,
            vec![("PATH".into(), "/usr/bin:/bin".into())]
        );
    }
    #[test]
    fn optout_and_spawn_failures_do_not_escape() {
        let events = CommandContext::new("doctor", None, None, false).events(Outcome::Failed, None);
        let launcher = MockLauncher {
            fail: true,
            ..Default::default()
        };
        let home = tempfile::tempdir().unwrap();
        let store =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        deliver_with(&events, &store, false, &launcher);
        assert!(launcher.processes.lock().unwrap().is_empty());
        deliver_with(&events, &store, true, &launcher);
        assert_eq!(launcher.processes.lock().unwrap().len(), 1);
    }
    #[test]
    fn root_handoff_is_noninteractive_and_has_no_payload_arguments() {
        let events =
            CommandContext::new("configure", None, None, false).events(Outcome::Failed, None);
        let process = delegate_process(501, Path::new(INSTALLED_HELPER), &events).unwrap();
        assert_eq!(process.executable, Path::new("/usr/bin/sudo"));
        assert_eq!(&process.arguments[..5], ["-n", "-H", "-u", "#501", "--"]);
        assert!(process.arguments.contains(&"-i".into()));
        assert_eq!(process.arguments.last().unwrap(), "internal-analytics-send");
        assert!(delegate_process(0, Path::new(INSTALLED_HELPER), &events).is_none());
    }

    #[test]
    fn queued_broker_handoff_uses_current_preference_at_delivery() {
        let home = tempfile::tempdir().unwrap();
        let store =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        let launcher = MockLauncher::default();
        let events = crate::exception_events(crate::Component::Broker, None, "PANIC", "runtime");
        let handoff = delegate_process(501, Path::new(INSTALLED_HELPER), &events).unwrap();
        let events: Vec<Event> = serde_json::from_slice(&handoff.input).unwrap();
        // The broker may keep running across both changes. A human helper checks
        // settings when handling each message, rather than caching broker state.
        store.set_enabled(false, true).unwrap();
        deliver_with(&events, &store, true, &launcher);
        assert!(launcher.processes.lock().unwrap().is_empty());
        store.set_enabled(true, true).unwrap();
        deliver_with(&events, &store, true, &launcher);
        assert_eq!(launcher.processes.lock().unwrap().len(), 1);
    }

    #[test]
    fn persistent_identity_is_shared_across_independent_command_lifecycle_and_helper_events() {
        let home = tempfile::tempdir().unwrap();
        let store =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        let command = CommandContext::new("read", Some("ethereum.balance"), None, true)
            .events(Outcome::Failed, Some(("RPC_UNAVAILABLE", "dispatch")));
        let second_command =
            CommandContext::new("doctor", None, None, false).events(Outcome::Succeeded, None);
        let lifecycle = crate::lifecycle_events(
            crate::Lifecycle::Update,
            crate::LifecycleOutcome::Succeeded,
            None,
            Some("1.2.3"),
            Some("complete"),
            Some(crate::Rollback::NotNeeded),
        );
        let helper = crate::exception_events(
            crate::Component::Broker,
            Some("ethereum.balance"),
            "RPC_UNAVAILABLE",
            "dispatch",
        );
        let handoff = delegate_process(501, Path::new(INSTALLED_HELPER), &helper).unwrap();
        let helper: Vec<Event> = serde_json::from_slice(&handoff.input).unwrap();
        let launcher = MockLauncher::default();

        deliver_with(&command, &store, true, &launcher);
        deliver_with(&lifecycle, &store, true, &launcher);
        deliver_with(&helper, &store, true, &launcher);
        let reopened_store =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        deliver_with(&second_command, &reopened_store, true, &launcher);

        let processes = launcher.processes.lock().unwrap();
        assert_eq!(processes.len(), 4);
        let distinct_ids: Vec<String> = processes
            .iter()
            .flat_map(|process| {
                let value: serde_json::Value = serde_json::from_slice(&process.input).unwrap();
                value["batch"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|event| event["distinct_id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(distinct_ids.len(), 5);
        assert!(distinct_ids.iter().all(|id| id == &distinct_ids[0]));
        let event_ids: Vec<String> = command
            .iter()
            .chain(lifecycle.iter())
            .chain(helper.iter())
            .chain(second_command.iter())
            .map(|event| {
                serde_json::to_value(event).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(event_ids[0], event_ids[1]);
        assert_ne!(event_ids[0], event_ids[4]);
        assert!(event_ids.iter().all(|id| id != &distinct_ids[0]));

        let other_home = tempfile::tempdir().unwrap();
        let other_store = crate::SettingsStore::for_home(other_home.path().to_path_buf(), unsafe {
            libc::geteuid()
        });
        let other_id = other_store.installation_id(true).unwrap().to_string();
        assert_ne!(other_id, distinct_ids[0]);
    }

    #[test]
    fn missing_identity_and_optout_do_not_launch() {
        let events =
            CommandContext::new("doctor", None, None, false).events(Outcome::Succeeded, None);
        let launcher = MockLauncher::default();
        let home = tempfile::tempdir().unwrap();
        // A home owned by a different account cannot mint an ID.
        let wrong_owner = crate::SettingsStore::for_home(
            home.path().to_path_buf(),
            unsafe { libc::geteuid() } + 1,
        );
        deliver_with(&events, &wrong_owner, true, &launcher);
        assert!(launcher.processes.lock().unwrap().is_empty());

        let valid =
            crate::SettingsStore::for_home(home.path().to_path_buf(), unsafe { libc::geteuid() });
        valid.set_enabled(false, true).unwrap();
        deliver_with(&events, &valid, true, &launcher);
        assert!(launcher.processes.lock().unwrap().is_empty());

        valid.set_enabled(true, true).unwrap();
        valid.installation_id(true).unwrap();
        let identity_path = home.path().join(".config/gofin/analytics-identity.json");
        std::fs::write(
            identity_path,
            br#"{"schema_version":1,"installation_id":"invalid"}"#,
        )
        .unwrap();
        deliver_with(&events, &valid, true, &launcher);
        assert!(launcher.processes.lock().unwrap().is_empty());
    }
}
