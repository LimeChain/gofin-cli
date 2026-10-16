//! Lifecycle observation never connects to the activation socket. Only explicit
//! start/stop invoke the peer-authenticated broker; success reflects launchd state.
use super::{Client, DaemonCommand};
use gofin_domain::GoFinError;
use serde_json::Value;

#[cfg(any(target_os = "macos", test))]
use {async_trait::async_trait, gofin_protocol::BrokerRequest, serde_json::json};

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Observation {
    registered: bool,
    pid: Option<u32>,
}

#[cfg(any(target_os = "macos", test))]
impl Observation {
    fn result(self, action: &str) -> Value {
        json!({"action":action,"status":if self.pid.is_some() {"running"} else {"stopped"},"registered":self.registered,"pid":self.pid})
    }
}

#[cfg(any(target_os = "macos", test))]
#[async_trait]
trait ServiceManager {
    async fn observe(&self) -> Result<Observation, GoFinError>;
    fn stop_requested(&self) -> Result<bool, GoFinError>;
    async fn wait(&self);
}

#[cfg(any(target_os = "macos", test))]
fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "daemon",
        message,
        "Check gofin daemon status; use gofin doctor after starting it.",
    )
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn check_activation(
    request: &BrokerRequest,
    stopped: impl FnOnce() -> Result<bool, GoFinError>,
) -> Result<(), GoFinError> {
    if matches!(
        request,
        BrokerRequest::DaemonStart | BrokerRequest::DaemonStop
    ) {
        return Ok(());
    }
    if stopped()? {
        return Err(GoFinError::new(
            "DAEMON_STOPPED",
            "daemon",
            "GoFin daemon is stopped.",
            "Run gofin daemon start, then retry the same request ID.",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn parse_observation(code: i32, output: &[u8]) -> Result<Observation, GoFinError> {
    if matches!(code, 3 | 113) {
        return Ok(Observation {
            registered: false,
            pid: None,
        });
    }
    let invalid = || {
        error(
            "DAEMON_STATUS_UNAVAILABLE",
            "Could not inspect GoFin daemon status.",
        )
    };
    if code != 0 || output.len() > 65_536 {
        return Err(invalid());
    }
    let text = std::str::from_utf8(output).map_err(|_| invalid())?;
    if !text
        .trim_start()
        .starts_with("system/io.gofin-cli.broker = {")
    {
        return Err(invalid());
    }
    let mut state = None;
    let mut pid = None;
    // Only direct job fields, not nested process/endpoint diagnostics.
    let mut depth = 0i32;
    for line in text.lines() {
        let line = line.trim();
        if depth == 1 {
            if let Some(value) = line.strip_prefix("state = ") {
                state = Some(value);
            }
            if let Some(value) = line.strip_prefix("pid = ") {
                let value: u32 = value.parse().map_err(|_| invalid())?;
                if value == 0 || pid.is_some() {
                    return Err(invalid());
                }
                pid = Some(value);
            }
        }
        if line.ends_with('{') {
            depth += 1;
        }
        if line == "}" {
            depth -= 1;
        }
        if depth < 0 {
            return Err(invalid());
        }
    }
    if depth != 0 || state.is_none() || state == Some("running") && pid.is_none() {
        return Err(invalid());
    }
    Ok(Observation {
        registered: true,
        pid,
    })
}

#[cfg(any(target_os = "macos", test))]
async fn run_with(
    command: DaemonCommand,
    client: &dyn Client,
    manager: &dyn ServiceManager,
) -> Result<Value, GoFinError> {
    let before = manager.observe().await?;
    let (action, expected_running) = match command {
        DaemonCommand::Status => return Ok(before.result("status")),
        DaemonCommand::Start => {
            if !before.registered {
                return Err(GoFinError::new(
                    "DAEMON_NOT_INSTALLED",
                    "daemon",
                    "GoFin daemon is not installed.",
                    "Run gofin setup from the reviewed bundle.",
                ));
            }
            let response = client.call(BrokerRequest::DaemonStart).await?;
            if response["status"] != "running" {
                return Err(error(
                    "DAEMON_START_FAILED",
                    "GoFin daemon did not confirm startup.",
                ));
            }
            ("start", true)
        }
        DaemonCommand::Stop => {
            if !before.registered {
                return Ok(before.result("stop"));
            }
            // Even an idle registered job needs durable stop intent, otherwise
            // the next normal request would activate its socket again.
            if !manager.stop_requested()? {
                let response = client.call(BrokerRequest::DaemonStop).await?;
                if !matches!(response["status"].as_str(), Some("stopping" | "stopped")) {
                    return Err(error(
                        "DAEMON_STOP_FAILED",
                        "GoFin daemon did not confirm stop.",
                    ));
                }
            }
            ("stop", false)
        }
    };
    for attempt in 0..20 {
        let observed = manager.observe().await?;
        if observed.pid.is_some() == expected_running && (!expected_running || observed.registered)
        {
            return Ok(observed.result(action));
        }
        if attempt < 19 {
            manager.wait().await;
        }
    }
    Err(error(
        if expected_running {
            "DAEMON_START_FAILED"
        } else {
            "DAEMON_STOP_PENDING"
        },
        if expected_running {
            "GoFin daemon is not running after startup."
        } else {
            "GoFin daemon is still stopping; stop intent is retained."
        },
    ))
}

#[cfg(target_os = "macos")]
struct Launchd;

#[cfg(target_os = "macos")]
#[async_trait]
impl ServiceManager for Launchd {
    fn stop_requested(&self) -> Result<bool, GoFinError> {
        gofin_protocol::daemon::stop_requested()
    }
    async fn observe(&self) -> Result<Observation, GoFinError> {
        let mut command = tokio::process::Command::new("/bin/launchctl");
        command
            .args(["print", gofin_protocol::daemon::SERVICE_TARGET])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(3), command.output())
            .await
            .map_err(|_| {
                error(
                    "DAEMON_STATUS_UNAVAILABLE",
                    "Daemon status inspection timed out.",
                )
            })?
            .map_err(|_| {
                error(
                    "DAEMON_STATUS_UNAVAILABLE",
                    "Could not inspect GoFin daemon status.",
                )
            })?;
        parse_observation(output.status.code().unwrap_or(-1), &output.stdout)
    }
    async fn wait(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(target_os = "macos")]
pub(super) async fn run(command: DaemonCommand, client: &dyn Client) -> Result<Value, GoFinError> {
    run_with(command, client, &Launchd).await
}

/// Observe the protected launchd job without connecting to its socket. A
/// caller may use this to decide whether an optional read is worth making;
/// connecting first can activate the socket-registered service.
#[cfg(target_os = "macos")]
pub(super) async fn is_running() -> Result<bool, GoFinError> {
    Ok(Launchd.observe().await?.pid.is_some())
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn run(_: DaemonCommand, _: &dyn Client) -> Result<Value, GoFinError> {
    Err(GoFinError::new(
        "PROTECTED_PROVIDER_UNAVAILABLE",
        "daemon",
        "Daemon lifecycle is unavailable on this platform.",
        "Use the reviewed macOS installation.",
    ))
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn is_running() -> Result<bool, GoFinError> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::OperationRequest;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeManager {
        observations: Mutex<VecDeque<Observation>>,
        waits: Mutex<usize>,
        stopped: bool,
    }
    #[async_trait]
    impl ServiceManager for FakeManager {
        fn stop_requested(&self) -> Result<bool, GoFinError> {
            Ok(self.stopped)
        }
        async fn observe(&self) -> Result<Observation, GoFinError> {
            let mut values = self.observations.lock().unwrap();
            if values.len() > 1 {
                Ok(values.pop_front().unwrap())
            } else {
                Ok(*values.front().unwrap())
            }
        }
        async fn wait(&self) {
            *self.waits.lock().unwrap() += 1;
        }
    }
    struct FakeClient {
        calls: Mutex<Vec<BrokerRequest>>,
        response: Result<Value, GoFinError>,
    }
    #[async_trait]
    impl Client for FakeClient {
        async fn call(&self, request: BrokerRequest) -> Result<Value, GoFinError> {
            self.calls.lock().unwrap().push(request);
            self.response.clone()
        }
        async fn execute(&self, _: OperationRequest) -> Result<Value, GoFinError> {
            panic!("lifecycle must never execute a financial operation")
        }
    }
    const STOPPED: Observation = Observation {
        registered: true,
        pid: None,
    };
    const RUNNING: Observation = Observation {
        registered: true,
        pid: Some(123),
    };
    fn manager(values: Vec<Observation>) -> FakeManager {
        FakeManager {
            observations: Mutex::new(values.into()),
            waits: Mutex::new(0),
            stopped: false,
        }
    }
    fn client(status: &str) -> FakeClient {
        FakeClient {
            calls: Mutex::new(vec![]),
            response: Ok(json!({"status":status})),
        }
    }

    #[test]
    fn ordinary_requests_are_blocked_before_socket_activation_and_marker_errors_propagate() {
        for request in [
            BrokerRequest::Doctor,
            BrokerRequest::Capabilities,
            BrokerRequest::Connections,
            BrokerRequest::Operations,
            BrokerRequest::Snapshot,
            BrokerRequest::Status {
                id: "SYNTHETIC".into(),
            },
        ] {
            assert_eq!(
                check_activation(&request, || Ok(true)).unwrap_err().code,
                "DAEMON_STOPPED"
            );
            assert!(check_activation(&request, || Ok(false)).is_ok());
            assert_eq!(
                check_activation(&request, || Err(error("DAEMON_MARKER_INVALID", "Invalid")))
                    .unwrap_err()
                    .code,
                "DAEMON_MARKER_INVALID"
            );
        }
        for request in [BrokerRequest::DaemonStart, BrokerRequest::DaemonStop] {
            assert!(
                check_activation(&request, || panic!(
                    "lifecycle handlers must handle their own persisted state"
                ))
                .is_ok()
            );
        }
    }

    #[test]
    fn parses_running_idle_absent_and_rejects_unobserved_or_nested_pids() {
        assert_eq!(
            parse_observation(
                0,
                b"system/io.gofin-cli.broker = {\n state = running\n pid = 123\n}\n"
            )
            .unwrap(),
            RUNNING
        );
        assert_eq!(parse_observation(0, b"system/io.gofin-cli.broker = {\n state = not running\n endpoints = {\n pid = 999\n }\n}\n").unwrap(), STOPPED);
        for code in [3, 113] {
            assert!(!parse_observation(code, b"").unwrap().registered);
        }
        for (code, text) in [
            (1, "permission denied"),
            (0, "system/other = {}"),
            (0, "system/io.gofin-cli.broker = {\n state = running\n}"),
            (
                0,
                "system/io.gofin-cli.broker = {\n state = running\n pid = no\n}",
            ),
        ] {
            assert_eq!(
                parse_observation(code, text.as_bytes()).unwrap_err().code,
                "DAEMON_STATUS_UNAVAILABLE"
            );
        }
    }

    #[tokio::test]
    async fn status_never_calls_or_wakes_broker_or_runs_doctor() {
        for observed in [
            STOPPED,
            RUNNING,
            Observation {
                registered: false,
                pid: None,
            },
        ] {
            let client = client("must not be used");
            let result = run_with(DaemonCommand::Status, &client, &manager(vec![observed]))
                .await
                .unwrap();
            assert_eq!(result, observed.result("status"));
            assert!(client.calls.lock().unwrap().is_empty());
            assert!(result.get("financialReadiness").is_none());
        }
    }

    #[tokio::test]
    async fn start_activates_and_waits_for_observed_running_result() {
        let client = client("running");
        let manager = manager(vec![STOPPED, STOPPED, RUNNING]);
        assert_eq!(
            run_with(DaemonCommand::Start, &client, &manager)
                .await
                .unwrap(),
            RUNNING.result("start")
        );
        assert_eq!(
            *client.calls.lock().unwrap(),
            vec![BrokerRequest::DaemonStart]
        );
        assert_eq!(*manager.waits.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn stop_idle_or_running_job_requests_persistent_stop_and_waits_for_exit() {
        for before in [STOPPED, RUNNING] {
            let client = client("stopping");
            let manager = manager(vec![before, RUNNING, STOPPED]);
            assert_eq!(
                run_with(DaemonCommand::Stop, &client, &manager)
                    .await
                    .unwrap(),
                STOPPED.result("stop")
            );
            assert_eq!(
                *client.calls.lock().unwrap(),
                vec![BrokerRequest::DaemonStop]
            );
            assert_eq!(*manager.waits.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn failures_never_claim_success_or_repeat_activation() {
        for (command, response, observed, code) in [
            (
                DaemonCommand::Start,
                "running",
                STOPPED,
                "DAEMON_START_FAILED",
            ),
            (
                DaemonCommand::Stop,
                "stopping",
                RUNNING,
                "DAEMON_STOP_PENDING",
            ),
            (
                DaemonCommand::Start,
                "already_running_or_socket_activated",
                RUNNING,
                "DAEMON_START_FAILED",
            ),
        ] {
            let client = client(response);
            let result = run_with(command, &client, &manager(vec![RUNNING, observed])).await;
            assert_eq!(result.unwrap_err().code, code);
            assert_eq!(client.calls.lock().unwrap().len(), 1);
        }
        let client = FakeClient {
            calls: Mutex::new(vec![]),
            response: Err(error("BROKER_UNAVAILABLE", "Failed")),
        };
        assert_eq!(
            run_with(DaemonCommand::Start, &client, &manager(vec![STOPPED]))
                .await
                .unwrap_err()
                .code,
            "BROKER_UNAVAILABLE"
        );
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repeated_stop_and_status_never_reactivate_explicitly_stopped_service() {
        let client = client("must not be used");
        let mut manager = manager(vec![STOPPED]);
        manager.stopped = true;
        for command in [
            DaemonCommand::Stop,
            DaemonCommand::Stop,
            DaemonCommand::Status,
        ] {
            assert_eq!(
                run_with(command, &client, &manager).await.unwrap()["status"],
                "stopped"
            );
        }
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_service_cannot_start_and_stop_is_idempotent() {
        let missing = Observation {
            registered: false,
            pid: None,
        };
        let client = client("must not be used");
        assert_eq!(
            run_with(DaemonCommand::Start, &client, &manager(vec![missing]))
                .await
                .unwrap_err()
                .code,
            "DAEMON_NOT_INSTALLED"
        );
        assert_eq!(
            run_with(DaemonCommand::Stop, &client, &manager(vec![missing]))
                .await
                .unwrap(),
            missing.result("stop")
        );
        assert!(client.calls.lock().unwrap().is_empty());
    }
}
