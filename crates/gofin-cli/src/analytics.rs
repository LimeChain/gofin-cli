//! Analytics observes public command boundaries, never individual broker calls.
use super::*;
use gofin_analytics::{CommandContext, EventSink, Outcome};

#[derive(Subcommand, Debug)]
pub(crate) enum Action {
    /// Enable anonymous usage and error reporting for this account.
    Enable,
    /// Disable reporting, including installation, updates and broker panics.
    Disable,
    /// Show the local preference and whether this build can send analytics.
    Status,
}

pub(crate) fn manage(action: Action, development: bool) -> Result<Value, GoFinError> {
    let mut status = match action {
        Action::Status => gofin_analytics::status(),
        Action::Enable | Action::Disable => {
            gofin_analytics::set_enabled(matches!(action, Action::Enable)).map_err(|_| {
                GoFinError::new("ANALYTICS_SETTINGS_UNAVAILABLE", "analytics",
                    "The local analytics preference could not be saved.",
                    "Check your account's ~/.config/gofin directory permissions and retry without sudo.")
            })?
        }
    };
    if development {
        status.effective_enabled = false;
    }
    let message = status_message(&status, development);
    let mut result = serde_json::to_value(status).map_err(|_| generic_input_error())?;
    result["message"] = json!(message);
    Ok(result)
}

fn status_message(status: &gofin_analytics::SettingsStatus, development: bool) -> String {
    if !status.enabled {
        match status.reason.as_str() {
            "disabled_by_user" => "Analytics is disabled for this account.".into(),
            "malformed_settings" => "Analytics is disabled because the local preference file is malformed. Run gofin analytics enable or disable to replace it.".into(),
            "unsafe_settings" => "Analytics is disabled because the local preference path has unsafe ownership, permissions or links. Check ~/.config/gofin/analytics.json.".into(),
            "root_settings_unavailable" | "account_home_unavailable" => "Analytics is disabled because the invoking account could not be resolved. Run this command as your own account without sudo.".into(),
            _ => "Analytics is disabled because the local preference could not be read. Check ~/.config/gofin/analytics.json permissions.".into(),
        }
    } else if development {
        "Analytics preference is enabled. This isolated development environment does not send analytics.".into()
    } else if !status.release_enabled {
        "Analytics preference is enabled. This development build does not send analytics.".into()
    } else {
        "Anonymous analytics is enabled. Disable with gofin analytics disable.".into()
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum LifecycleKind {
    Install,
    Update,
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum LifecycleOutcome {
    Attempted,
    Succeeded,
    Failed,
    Canceled,
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum Stage {
    Prepare,
    Verify,
    Configure,
    Service,
    Path,
    Download,
    Compatibility,
    Replace,
    Restart,
    Complete,
}
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum Rollback {
    NotNeeded,
    Restored,
    Uncertain,
}
#[derive(Args, Debug)]
pub(crate) struct LifecycleArgs {
    #[arg(long, value_enum)]
    kind: LifecycleKind,
    #[arg(long, value_enum)]
    outcome: LifecycleOutcome,
    #[arg(long, value_enum)]
    stage: Stage,
    #[arg(long)]
    from_version: Option<String>,
    #[arg(long)]
    to_version: Option<String>,
    #[arg(long, value_enum)]
    rollback: Option<Rollback>,
}

pub(crate) fn internal(command: &Command) -> Option<i32> {
    match command {
        Command::InternalAnalyticsSend => {
            gofin_analytics::enable_runtime();
            Some(gofin_analytics::run_helper())
        }
        Command::InternalAnalyticsOutcome { exit_code } => {
            gofin_analytics::enable_runtime();
            Some(gofin_analytics::run_command_helper(*exit_code))
        }
        Command::InternalAnalyticsLifecycle(args) => {
            use gofin_analytics::{Lifecycle as L, LifecycleOutcome as O};
            gofin_analytics::enable_runtime();
            let kind = match args.kind {
                LifecycleKind::Install => L::Install,
                LifecycleKind::Update => L::Update,
            };
            let outcome = match args.outcome {
                LifecycleOutcome::Attempted => O::Attempted,
                LifecycleOutcome::Succeeded => O::Succeeded,
                LifecycleOutcome::Failed => O::Failed,
                LifecycleOutcome::Canceled => O::Canceled,
            };
            let stage = match args.stage {
                Stage::Prepare => "prepare",
                Stage::Verify => "verify",
                Stage::Configure => "configure",
                Stage::Service => "service",
                Stage::Path => "path",
                Stage::Download => "download",
                Stage::Compatibility => "compatibility",
                Stage::Replace => "replace",
                Stage::Restart => "restart",
                Stage::Complete => "complete",
            };
            let rollback = args.rollback.map(|value| match value {
                Rollback::NotNeeded => gofin_analytics::Rollback::NotNeeded,
                Rollback::Restored => gofin_analytics::Rollback::Restored,
                Rollback::Uncertain => gofin_analytics::Rollback::Uncertain,
            });
            gofin_analytics::report(&gofin_analytics::lifecycle_events(
                kind,
                outcome,
                args.from_version.as_deref(),
                args.to_version.as_deref(),
                Some(stage),
                rollback,
            ));
            Some(0)
        }
        _ => None,
    }
}

pub(crate) fn context(command: &Command, machine: bool) -> Option<CommandContext> {
    let (name, operation, input): (&str, Option<&str>, Option<&str>) = match command {
        Command::Analytics { .. }
        | Command::InternalAnalyticsSend
        | Command::InternalAnalyticsOutcome { .. }
        | Command::InternalAnalyticsLifecycle(_)
        | Command::InternalDaemonStart
        | Command::InternalUpdateCheck
        | Command::InternalCompletions { .. }
        | Command::InternalSkillsRefresh
        | Command::Completions { .. } => return None,
        Command::Read(args) => (
            "read",
            Some(&args.operation.operation),
            Some(&args.operation.input),
        ),
        Command::Preview(args) => ("preview", Some(&args.operation), Some(&args.input)),
        Command::Execute(args) => (
            if args.dry_run { "preview" } else { "execute" },
            Some(&args.operation),
            Some(&args.input),
        ),
        Command::Configure(args) => (
            if args.dry_run { "preview" } else { "configure" },
            Some(&args.operation),
            Some(&args.input),
        ),
        Command::Authorizations { command } => match command {
            AuthorizationsCommand::Authorize(args) => (
                "authorizations.authorize",
                Some(&args.operation),
                Some(&args.input),
            ),
            AuthorizationsCommand::Status { .. } => ("authorizations.status", None, None),
            AuthorizationsCommand::Cancel { .. } => ("authorizations.cancel", None, None),
        },
        Command::Plans { command } => match command {
            PlansCommand::Authorize(args) => ("plans.authorize", None, Some(&args.input)),
            PlansCommand::Preview(args) => ("plans.preview", None, Some(&args.input)),
            PlansCommand::Status { .. } => ("plans.status", None, None),
            PlansCommand::Cancel { .. } => ("plans.cancel", None, None),
        },
        Command::Yolo { command } => (
            match command {
                YoloCommand::Enable => "yolo.on",
                YoloCommand::Disable => "yolo.off",
                YoloCommand::Status => "yolo.status",
            },
            None,
            None,
        ),
        Command::Daemon { command } => (
            match command {
                DaemonCommand::Start => "daemon.start",
                DaemonCommand::Stop => "daemon.stop",
                DaemonCommand::Status => "daemon.status",
            },
            None,
            None,
        ),
        Command::Policy { command } => (
            match command {
                PolicyCommand::Show => "policy.show",
                PolicyCommand::Set(_) => "policy.set",
            },
            None,
            None,
        ),
        Command::Connections { command } => (
            match command {
                None | Some(ConnectionsCommand::List) => "connections.list",
                Some(ConnectionsCommand::Add(_)) => "connections.add",
                Some(ConnectionsCommand::Generate(_)) => "connections.generate",
                Some(ConnectionsCommand::Update(_)) => "connections.update",
                Some(ConnectionsCommand::Remove(_)) => "connections.remove",
            },
            None,
            None,
        ),
        Command::Skill { command } => (
            match command {
                SkillCommand::Install(_) => "skill.install",
                SkillCommand::Inspect(_) => "skill.inspect",
                SkillCommand::Update(_) => "skill.update",
                SkillCommand::Remove(_) => "skill.remove",
            },
            None,
            None,
        ),
        Command::Snapshot { input } => ("snapshot", Some("gofin.snapshot"), input.as_deref()),
        command => (command_name(command), None, None),
    };
    // Raw JSON is used only to extract reviewed fields; the context retains no input.
    let input = input.and_then(|value| serde_json::from_str::<Value>(value).ok());
    let mut context = CommandContext::new(name, operation, input.as_ref(), machine);
    if let Command::Execute(args) = command
        && !args.dry_run
    {
        use gofin_analytics::ExecutionMode;
        let mode = if args.plan.is_some() {
            ExecutionMode::Plan
        } else if args.authorization.is_some() {
            ExecutionMode::BoundedAuthorization
        } else {
            ExecutionMode::Standalone
        };
        context = context.with_execution_mode(mode);
    }
    Some(context)
}

pub(crate) fn notice(command: &Command, machine: bool, development: bool, ui: &Ui) {
    use std::io::IsTerminal;
    if !development
        && !machine
        && context(command, machine).is_some()
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::io::stdout().is_terminal()
        && let Some(message) = gofin_analytics::notice()
    {
        let _ = ui.warning(message);
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn privileged_context(action: &PrivilegedAction) -> CommandContext {
    let (command, operation, input) = match action {
        PrivilegedAction::Setup => ("setup", None, None),
        PrivilegedAction::PolicySet(_) => ("policy.set", None, None),
        PrivilegedAction::Configure(request) => (
            "configure",
            Some(request.operation.as_str()),
            Some(&request.input),
        ),
        PrivilegedAction::Add(_) => ("connections.add", None, None),
        PrivilegedAction::Generate(_) => ("connections.generate", None, None),
        PrivilegedAction::Update(_) => ("connections.update", None, None),
        PrivilegedAction::Remove(_) => ("connections.remove", None, None),
        PrivilegedAction::YoloEnable => ("yolo.on", None, None),
        PrivilegedAction::YoloDisable => ("yolo.off", None, None),
    };
    CommandContext::new(command, operation, input, false)
}

pub(crate) struct Observation<'a> {
    context: Option<CommandContext>,
    started: chrono::DateTime<chrono::Utc>,
    execution: bool,
    development: bool,
    machine: bool,
    sink: &'a dyn EventSink,
}
impl<'a> Observation<'a> {
    pub fn new(
        arguments: &[std::ffi::OsString],
        development: bool,
        machine: bool,
        sink: &'a dyn EventSink,
    ) -> Self {
        let internal = arguments.iter().any(|arg| {
            arg == "analytics"
                || arg
                    .to_str()
                    .is_some_and(|s| s.starts_with("internal-") || s == "completions")
        });
        Self {
            context: (!development && !internal)
                .then(|| CommandContext::new("unknown", None, None, machine)),
            started: chrono::Utc::now(),
            execution: false,
            development,
            machine,
            sink,
        }
    }
    pub fn help(&mut self, version: bool) {
        if self.context.is_some() {
            self.context = Some(CommandContext::new(
                if version { "version" } else { "help" },
                None,
                None,
                self.machine,
            ));
        }
    }
    pub fn command(&mut self, command: &Command, machine: bool) {
        self.execution = matches!(command, Command::Execute(args) if !args.dry_run);
        self.context = if self.development {
            None
        } else {
            context(command, machine)
        };
    }
    pub fn complete(
        mut self,
        result: Result<Value, GoFinError>,
        render: impl FnOnce(Result<Value, GoFinError>) -> i32,
    ) -> i32 {
        let Some(mut context) = self.context.take() else {
            return render(result);
        };
        let mut record_outcome = None;
        let error = match &result {
            Ok(value) => {
                context.observe_result(value);
                if self.execution {
                    record_outcome = match value.get("status").and_then(Value::as_str) {
                        Some("failed" | "rejected" | "expired") => Some(Outcome::Failed),
                        Some("cancelled" | "canceled") => Some(Outcome::Canceled),
                        _ => None,
                    };
                    fresh_execution_error(value, self.started)
                } else {
                    None
                }
            }
            Err(error) => Some((error.code.clone(), error.stage.clone())),
        };
        let code = render(result);
        let outcome = if (code == 0 && record_outcome == Some(Outcome::Canceled))
            || matches!(code, 129 | 130 | 143)
            || error.as_ref().is_some_and(|(code, _)| {
                matches!(
                    code.as_str(),
                    "CANCELED"
                        | "CANCELLED"
                        | "HUMAN_CANCELED"
                        | "APPROVAL_CANCELED"
                        | "AUTHORIZATION_CANCELLED"
                        | "ENROLLMENT_CANCELLED"
                        | "USER_CANCELLED"
                )
            }) {
            Outcome::Canceled
        } else if code != 0 || error.is_some() {
            Outcome::Failed
        } else {
            record_outcome.unwrap_or(Outcome::Succeeded)
        };
        let error = error
            .as_ref()
            .map(|(code, stage)| (code.as_str(), stage.as_str()))
            .or_else(|| {
                (code != 0 && outcome == Outcome::Failed).then_some(("OUTPUT_WRITE_FAILED", "cli"))
            });
        self.sink.send(&context.events(outcome, error));
        code
    }
}

// Execution may return an OperationRecord with an error through an otherwise
// successful RPC response. Only a record created during this invocation supplies
// a fresh exception. Replays, recovery of old attempts and status reads retain
// their result status without re-reporting historical errors. The timestamp and
// every identity/message stay local; the shared crate allowlists code and stage.
fn fresh_execution_error(
    value: &Value,
    started: chrono::DateTime<chrono::Utc>,
) -> Option<(String, String)> {
    let created = chrono::DateTime::parse_from_rfc3339(value.get("created_at")?.as_str()?).ok()?;
    if created < started || created > chrono::Utc::now() {
        return None;
    }
    let operation = value.get("operation")?.as_str()?;
    if gofin_analytics::safe_operation(operation) == "unknown" {
        return None;
    }
    let error = value.get("error")?;
    Some((
        error.get("code")?.as_str()?.into(),
        error.get("stage")?.as_str()?.into(),
    ))
}

/// exec removes the original CLI. An immutable unprivileged shell observes sudo
/// completion and reports only the already-sanitized context, never its arguments.
#[cfg(any(target_os = "macos", test))]
const HANDOFF: &str = r#"
set -eu
gofin_reporter=$1
gofin_context=$2
shift 2
cleanup() {
  gofin_status=$?
  trap - EXIT HUP INT TERM
  /usr/bin/sudo -K || true
  if [ -n "$gofin_context" ]; then
    printf '%s' "$gofin_context" | "$gofin_reporter" internal-analytics-command --exit-code "$gofin_status" >/dev/null 2>&1 || true
  fi
  exit "$gofin_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM
"$@"
"#;

#[cfg(any(target_os = "macos", test))]
pub(crate) fn wrap_privileged(
    command: &std::process::Command,
    context: &CommandContext,
    reporter: &std::path::Path,
) -> std::process::Command {
    let mut wrapper = std::process::Command::new("/bin/bash");
    wrapper
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args(["-c", HANDOFF, "gofin-handoff"])
        .arg(reporter)
        .arg(serde_json::to_string(context).unwrap_or_default())
        .arg(command.get_program())
        .args(command.get_args());
    wrapper
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Sink(Mutex<Vec<Value>>);
    impl EventSink for Sink {
        fn send(&self, events: &[gofin_analytics::Event]) {
            self.0
                .lock()
                .unwrap()
                .extend(events.iter().map(gofin_analytics::Event::capture));
        }
    }
    struct NoBroker(bool);
    #[test]
    fn execution_modes_follow_cli_route_without_collecting_authority_ids() {
        for (flags, expected) in [
            (vec![], "standalone"),
            (
                vec!["--authorization", "private-grant"],
                "bounded_authorization",
            ),
            (
                vec!["--plan", "private-plan", "--step", "private-step"],
                "plan",
            ),
            (
                vec![
                    "--plan",
                    "private-plan",
                    "--step",
                    "private-step",
                    "--replaces-attempt",
                    "private-attempt",
                ],
                "plan",
            ),
            (
                vec![
                    "--authorization",
                    "private-grant",
                    "--replaces-attempt",
                    "private-attempt",
                ],
                "bounded_authorization",
            ),
        ] {
            let mut args = vec![
                "gofin",
                "execute",
                "bridge.execute",
                "--input",
                "{}",
                "--id",
                "private-request",
                "--json",
            ];
            args.extend(flags);
            let cli = Cli::try_parse_from(&args).unwrap();
            for failed in [false, true] {
                let sink = Sink::default();
                let mut observation = Observation::new(&[], false, true, &sink);
                observation.command(&cli.command, true);
                let result = if failed {
                    Err(GoFinError::new(
                        "INVALID_INPUT",
                        "validation",
                        "private-message",
                        "private-guidance",
                    ))
                } else {
                    Ok(json!({"status":"pending"}))
                };
                observation.complete(result, |_| i32::from(failed));
                let events = sink.0.lock().unwrap();
                let commands: Vec<_> = events
                    .iter()
                    .filter(|event| event["event"] == "gofin.command_completed")
                    .collect();
                assert_eq!(commands.len(), 1);
                let properties = &commands[0]["properties"];
                assert_eq!(properties["command"], "execute");
                assert_eq!(properties["operation"], "bridge.execute");
                assert_eq!(properties["execution_mode"], expected);
                assert_eq!(
                    properties["outcome"],
                    if failed { "failed" } else { "succeeded" }
                );
                assert!(
                    !serde_json::to_string(&*events)
                        .unwrap()
                        .contains("private-")
                );
            }
            let sink = Sink::default();
            let mut observation = Observation::new(&[], true, true, &sink);
            observation.command(&cli.command, true);
            observation.complete(Ok(Value::Null), |_| 0);
            assert!(sink.0.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn previews_and_status_are_not_classified_as_executions() {
        for args in [
            vec!["gofin", "preview", "bridge.execute", "--input", "{}"],
            vec!["gofin", "status", "private-operation"],
            vec!["gofin", "plans", "status", "private-plan"],
            vec![
                "gofin",
                "execute",
                "bridge.execute",
                "--input",
                "{}",
                "--dry-run",
            ],
            vec![
                "gofin",
                "execute",
                "bridge.execute",
                "--input",
                "{}",
                "--dry-run",
                "--plan",
                "private-plan",
                "--step",
                "private-step",
            ],
            vec![
                "gofin",
                "execute",
                "bridge.execute",
                "--input",
                "{}",
                "--dry-run",
                "--authorization",
                "private-grant",
            ],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            let event = context(&cli.command, false)
                .unwrap()
                .events(Outcome::Succeeded, None)[0]
                .capture();
            assert!(event["properties"].get("execution_mode").is_none());
            assert_ne!(event["properties"]["command"], "execute");
        }
    }

    #[test]
    fn every_published_operation_is_in_the_checked_analytics_vocabulary() {
        for definition in financial_catalog() {
            let name = definition["name"].as_str().unwrap();
            assert_eq!(gofin_analytics::safe_operation(name), name);
        }
        assert_eq!(
            gofin_analytics::safe_operation("coinbase.private-account-secret"),
            "unknown"
        );
    }
    #[async_trait]
    impl Client for NoBroker {
        fn development_mode(&self) -> bool {
            self.0
        }
        async fn execute(&self, _: OperationRequest) -> Result<Value, GoFinError> {
            panic!("no financial dispatch")
        }
        async fn call(&self, _: BrokerRequest) -> Result<Value, GoFinError> {
            panic!("no broker access")
        }
    }
    #[tokio::test]
    async fn parse_failure_reports_once_without_echoing_arguments() {
        let sink = Sink::default();
        let code = run_with_analytics(
            ["gofin", "private-wallet-secret", "--json"]
                .map(Into::into)
                .into(),
            &NoBroker(false),
            &sink,
        )
        .await;
        assert_eq!(code, 1);
        let events = serde_json::to_string(&*sink.0.lock().unwrap()).unwrap();
        assert!(!events.contains("private-wallet-secret"));
        assert_eq!(events.matches("gofin.command_completed").count(), 1);
    }
    #[tokio::test]
    async fn isolated_development_emits_nothing_even_on_errors() {
        for arguments in [
            vec!["gofin", "invalid", "--json"],
            vec!["gofin", "update", "--json"],
        ] {
            let sink = Sink::default();
            run_with_analytics(
                arguments.into_iter().map(Into::into).collect(),
                &NoBroker(true),
                &sink,
            )
            .await;
            assert!(sink.0.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn yolo_actions_are_distinct_and_controls_are_offline() {
        for action in ["on", "off", "status"] {
            let cli = Cli::try_parse_from(["gofin", "yolo", action]).unwrap();
            let events = context(&cli.command, false)
                .unwrap()
                .events(Outcome::Succeeded, None);
            assert!(
                serde_json::to_string(&events)
                    .unwrap()
                    .contains(&format!("yolo.{action}"))
            );
        }
        for action in ["enable", "disable", "status"] {
            let cli = Cli::try_parse_from(["gofin", "analytics", action, "--json"]).unwrap();
            assert!(context(&cli.command, true).is_none());
            assert!(!human_only(&cli.command));
            assert!(development_command_allowed(&cli.command));
        }
    }
    #[test]
    fn output_failure_is_a_failed_command_and_recovery_does_not_duplicate() {
        let sink = Sink::default();
        let args = ["gofin", "execute"].map(Into::into);
        let observation = Observation::new(&args, false, false, &sink);
        assert_eq!(
            observation.complete(Ok(json!({"recovered":true})), |_| 1),
            1
        );
        let events = serde_json::to_string(&*sink.0.lock().unwrap()).unwrap();
        assert_eq!(events.matches("gofin.command_completed").count(), 1);
        assert!(events.contains("failed"));
    }

    #[test]
    fn execution_records_report_fresh_failures_and_preserve_historical_status() {
        let now = chrono::Utc::now();
        for (command, created, status, expected_outcome, exception) in [
            ("execute", now, "failed", "failed", true),
            ("execute", now, "unknown", "failed", true),
            (
                "execute",
                now - chrono::Duration::days(1),
                "failed",
                "failed",
                false,
            ),
            ("status", now, "failed", "succeeded", false),
            ("execute", now, "cancelled", "canceled", false),
        ] {
            let sink = Sink::default();
            let args = if command == "execute" {
                vec![
                    "gofin",
                    "execute",
                    "bridge.execute",
                    "--input",
                    "{}",
                    "--id",
                    "private-id",
                    "--json",
                ]
            } else {
                vec!["gofin", "status", "private-id", "--json"]
            };
            let cli = Cli::try_parse_from(&args).unwrap();
            let mut observation = Observation::new(&[], false, true, &sink);
            observation.started = now - chrono::Duration::seconds(1);
            observation.command(&cli.command, true);
            let result = json!({
                "operation":"bridge.execute", "id":"private-id", "status":status,
                "created_at":created.to_rfc3339(),
                "error":{"code":"INVALID_INPUT", "stage":"validation", "message":"private-message"}
            });
            assert_eq!(observation.complete(Ok(result), |_| 0), 0);
            let events = sink.0.lock().unwrap();
            assert_eq!(events[0]["properties"]["outcome"], expected_outcome);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["event"] == "$exception")
                    .count(),
                usize::from(exception)
            );
            assert_eq!(events[0]["properties"]["result_status"], status);
            assert!(
                !serde_json::to_string(&*events)
                    .unwrap()
                    .contains("private-")
            );
        }
    }

    #[test]
    fn pending_is_not_settlement_and_future_or_missing_timestamps_are_not_fresh() {
        let sink = Sink::default();
        let cli = Cli::try_parse_from([
            "gofin",
            "execute",
            "bridge.execute",
            "--input",
            "{}",
            "--id",
            "private-id",
            "--json",
        ])
        .unwrap();
        let mut observation = Observation::new(&[], false, true, &sink);
        observation.command(&cli.command, true);
        observation.complete(
            Ok(json!({"operation":"bridge.execute", "status":"pending"})),
            |_| 0,
        );
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["properties"]["outcome"], "succeeded");
        assert_eq!(events[0]["properties"]["result_status"], "pending");
        let now = chrono::Utc::now();
        let mut result = json!({"operation":"bridge.execute", "error":{"code":"INVALID_INPUT","stage":"validation"}});
        assert!(fresh_execution_error(&result, now).is_none());
        result["created_at"] = json!((now + chrono::Duration::days(1)).to_rfc3339());
        assert!(fresh_execution_error(&result, now).is_none());
    }

    #[test]
    fn cancellations_and_status_explanations_remain_local_and_sanitized() {
        for code in [
            "AUTHORIZATION_CANCELLED",
            "ENROLLMENT_CANCELLED",
            "USER_CANCELLED",
        ] {
            let sink = Sink::default();
            let observation = Observation::new(&[], false, true, &sink);
            assert_eq!(
                observation.complete(
                    Err(GoFinError::new(code, "authorization", "private", "private")),
                    |_| 1
                ),
                1
            );
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["properties"]["outcome"], "canceled");
        }
        let mut status = gofin_analytics::SettingsStatus {
            enabled: true,
            effective_enabled: false,
            release_enabled: false,
            reason: "enabled_by_default".into(),
        };
        assert!(status_message(&status, false).contains("development build"));
        assert!(status_message(&status, true).contains("isolated development"));
        status.enabled = false;
        status.reason = "malformed_settings".into();
        assert!(status_message(&status, false).contains("malformed"));
    }
    #[test]
    fn wrapper_keeps_privileged_arguments_out_of_reporting_context() {
        let mut command = std::process::Command::new("/usr/bin/sudo");
        command.args(["--", "broker", "--request", "sensitive-request"]);
        let context = CommandContext::new("connections.add", None, None, false);
        let wrapped = wrap_privileged(&command, &context, std::path::Path::new("/fixed/gofin"));
        let args: Vec<_> = wrapped
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(wrapped.get_program(), "/bin/bash");
        assert!(!args[4].contains("sensitive-request"));
        assert_eq!(args.last().unwrap(), "sensitive-request");
        assert!(HANDOFF.contains("exit \"$gofin_status\""));
        assert!(HANDOFF.contains("internal-analytics-command"));
    }
}
