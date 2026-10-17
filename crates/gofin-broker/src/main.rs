use clap::{Parser, Subcommand, error::ErrorKind};
use gofin_domain::GoFinError;
use gofin_presentation::Ui;
use serde_json::Value;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "gofin-broker", color = clap::ColorChoice::Never, version, about = "Protected GoFin broker service")]
struct Args {
    /// Machine mode: enrollment returns a human-terminal handoff.
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    verbose: bool,
    /// Export the shared operation registry without opening protected state.
    #[arg(long)]
    export_registry: bool,
    /// Export journal compatibility metadata without opening protected state.
    #[arg(long, conflicts_with = "export_registry")]
    export_storage_contract: bool,
    /// Internal, metadata-only guard for a verified public update.
    #[arg(long, hide = true)]
    hold_runtime_update: Option<u32>,
    /// Inspect root-staged executable dependencies before loading new code.
    #[arg(long, hide = true)]
    verify_update_bundle: Option<String>,
    /// Execute only the embedded, fixed-installation public updater.
    #[arg(long, hide = true)]
    runtime_update_release: Option<String>,
    /// Human approver account configured by the root installer.
    #[arg(long, requires = "approver_gid")]
    approver_uid: Option<u32>,
    /// Human group configured for the launchd activated socket.
    #[arg(long, requires = "approver_uid")]
    approver_gid: Option<u32>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Configure {
        #[arg(long)]
        request: String,
    },
    Enrollment {
        #[command(subcommand)]
        action: EnrollmentAction,
    },
    /// Enable financial approval bypass after a verified sudo Terminal review.
    YoloEnable,
    /// Disable financial approval bypass after a verified sudo Terminal review.
    YoloDisable,
}

#[derive(Debug, Subcommand)]
enum EnrollmentAction {
    PolicySet {
        #[arg(long)]
        request: String,
    },
    Initialize {
        #[arg(long)]
        request: String,
    },
    Add {
        #[arg(long)]
        request: String,
    },
    Update {
        #[arg(long)]
        request: String,
    },
    Remove {
        #[arg(long)]
        request: String,
    },
    Configure {
        #[arg(long)]
        request: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let mut request_value = false;
    let machine = arguments.iter().skip(1).any(|arg| {
        if request_value {
            request_value = false;
            return false;
        }
        request_value = arg == "--request";
        arg == "--json"
    });
    let args = match Args::try_parse_from(arguments) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            if machine {
                let _ = Ui::new(true, false)
                    .write_result(&serde_json::json!({"help":error.to_string()}), "help");
            } else {
                let _ = error.print();
            }
            return ExitCode::SUCCESS;
        }
        Err(_) => {
            let _ = Ui::new(machine, false).write_error(&GoFinError::new(
                "ARGUMENTS_INVALID",
                "cli",
                "The broker command line is invalid.",
                "Use gofin --help for the public command interface.",
            ));
            return ExitCode::FAILURE;
        }
    };
    let ui = Ui::new(args.json, args.verbose);
    let analytics = reports_analytics(&args);
    if analytics {
        // Only the installed binary enables analytics. Library composition and
        // development examples do not activate this process-wide boundary.
        gofin_analytics::enable_runtime();
        gofin_analytics::install_panic_hook(gofin_analytics::Component::Broker);
    }
    let operation = analytics_operation(&args);
    let report_failures = reports_failure_analytics(&args);
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if report_failures {
                // Ordinary IPC errors return to the CLI and are reported there.
                // These are process/privileged-entrypoint failures only; reporting
                // drops them until a protected human account has been established.
                gofin_analytics::report(&gofin_analytics::exception_events(
                    gofin_analytics::Component::Broker,
                    operation.as_deref(),
                    &error.code,
                    &error.stage,
                ));
            }
            let _ = ui.write_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn reports_analytics(args: &Args) -> bool {
    !args.export_registry
        && !args.export_storage_contract
        && args.hold_runtime_update.is_none()
        && args.verify_update_bundle.is_none()
}

fn reports_failure_analytics(args: &Args) -> bool {
    // The installer owns initialization failures; update preparation/replacement
    // likewise own their lifecycle failure and exception together. The main
    // boundary still installs the panic hook for these entrypoints.
    reports_analytics(args)
        && args.runtime_update_release.is_none()
        && !matches!(
            args.command,
            Some(Command::Enrollment {
                action: EnrollmentAction::Initialize { .. }
            })
        )
}

fn analytics_operation(args: &Args) -> Option<String> {
    let request = match args.command.as_ref()? {
        Command::Configure { request }
        | Command::Enrollment {
            action: EnrollmentAction::Configure { request },
        } => request,
        _ => return None,
    };
    // This is the only request field considered for analytics. The shared event
    // builder subsequently accepts only names from its compiled operation vocabulary.
    let request = validate_public_enrollment_request(request).ok()?;
    request.get("operation")?.as_str().map(str::to_owned)
}

async fn run(args: Args) -> gofin_domain::Result<()> {
    let ui = Ui::new(args.json, args.verbose);
    if let Some(tag) = args.runtime_update_release {
        if args.json {
            return Err(GoFinError::new(
                "HUMAN_TERMINAL_REQUIRED",
                "update",
                "Updates require your own interactive Terminal.",
                "Run gofin update yourself without --json.",
            ));
        }
        return gofin_broker::runtime_update::install_release(&tag).await;
    }
    if let Some(directory) = args.verify_update_bundle {
        return gofin_broker::runtime_update::verify_staged_bundle(std::path::Path::new(
            &directory,
        ));
    }
    if let Some(parent) = args.hold_runtime_update {
        return gofin_broker::runtime_update::hold(parent).await;
    }
    if args.export_storage_contract {
        println!(
            "{}",
            serde_json::json!({"journal_schema_version": gofin_store::supported_schema_version()})
        );
        return Ok(());
    }
    if args.export_registry {
        let definitions = gofin_broker::operation_definitions();
        let bytes = serde_json::to_vec_pretty(&definitions).map_err(|_| {
            GoFinError::new(
                "REGISTRY_EXPORT_FAILED",
                "registry",
                "The static registry could not be serialized.",
                "Inspect the compiled operation definitions.",
            )
        })?;
        println!("{}", String::from_utf8_lossy(&bytes));
        return Ok(());
    }

    if let Some(Command::Configure { ref request }) = args.command {
        if args.json {
            return Err(GoFinError::new(
                "HUMAN_TERMINAL_REQUIRED",
                "configuration",
                "Configuration requires your own interactive Terminal.",
                "Run gofin configure yourself without --json.",
            ));
        }
        let public = validate_public_enrollment_request(request)?;
        let request: gofin_protocol::ConfigureRequest = serde_json::from_value(public)
            .map_err(|_| GoFinError::validation("Use the typed public configuration request."))?;
        let ui = Ui::new(false, args.verbose);
        let result = gofin_broker::configure::handle_with_ui(request, &ui).await?;
        if result["status"] == "acknowledged" {
            let _ = ui.success("Venue configuration acknowledged.");
        } else {
            let _ =
                ui.warning("Configuration outcome is unresolved; keep its original request ID.");
        }
        println!(
            "Attempt: {} | Request: {}",
            result["id"].as_str().unwrap_or(""),
            result["requestId"].as_str().unwrap_or("")
        );
        println!("{}", result["nextAction"].as_str().unwrap_or(""));
        if args.verbose {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).map_err(|_| GoFinError::validation(
                    "Configuration result could not be displayed."
                ))?
            );
        }
        return Ok(());
    }
    if matches!(
        &args.command,
        Some(Command::YoloEnable | Command::YoloDisable)
    ) {
        if args.json {
            return Err(GoFinError::new(
                "HUMAN_TERMINAL_REQUIRED",
                "authorization",
                "Yolo changes require your own verified interactive Terminal.",
                "Run the Yolo command yourself without --json.",
            ));
        }
        let enabled = matches!(&args.command, Some(Command::YoloEnable));
        let uid = gofin_broker::yolo_management_uid()?;
        gofin_analytics::set_broker_user(uid);
        let terminal = gofin_human_terminal::HumanTerminal::open_yolo(uid)?;
        let confirmation = if enabled {
            let warning = "DANGER: Yolo lets agents submit financial transactions and plans without further human approval, including trades, transfers, withdrawals, bridges, supplies, collateral changes, borrowing, and repayments, within remaining configured capabilities and financial limits. Funds may be lost.";
            ui.warning(warning).map_err(|_| {
                GoFinError::new(
                    "OUTPUT_FAILED",
                    "authorization",
                    "The Yolo danger warning could not be displayed.",
                    "Retry from a verified Terminal.",
                )
            })?;
            Some(terminal.confirm_yolo(
                "",
                std::time::Instant::now() + std::time::Duration::from_secs(90),
            )?)
        } else {
            terminal.finish()?;
            None
        };
        let state =
            gofin_broker::yolo_runtime::set_protected_enabled_after_review(enabled, || {
                match confirmation {
                    Some(confirmation) => confirmation.validate_for(uid),
                    None => Ok(()),
                }
            })?;
        let message = if state.enabled {
            "Protected Yolo enabled. Financial operations use bounded policy and dispatch checks without per-write approval."
        } else {
            "Protected Yolo disabled. New Yolo admissions are blocked; already admitted work may still finish."
        };
        ui.success(message).map_err(|_| {
            GoFinError::new(
                "OUTPUT_FAILED",
                "authorization",
                "Yolo state changed, but its result could not be displayed.",
                "Run gofin yolo status before retrying.",
            )
        })?;
        return Ok(());
    }
    if let Some(Command::Enrollment { action }) = args.command {
        if args.json {
            return Err(GoFinError::new(
                "HUMAN_TERMINAL_REQUIRED",
                "enrollment",
                "Enrollment requires your own interactive Terminal.",
                "Run gofin policy set, gofin connections add or gofin setup yourself without --json.",
            ));
        }
        let ui = Ui::new(false, args.verbose);
        let (action_name, request) = match action {
            EnrollmentAction::PolicySet { request } => ("policy_set", request),
            EnrollmentAction::Initialize { request } => ("initialize", request),
            EnrollmentAction::Add { request } => ("add", request),
            EnrollmentAction::Update { request } => ("update", request),
            EnrollmentAction::Remove { request } => ("remove", request),
            EnrollmentAction::Configure { request } => ("configure", request),
        };
        let request = validate_public_enrollment_request(&request)?;
        let result = gofin_broker::enrollment::handle_with_ui(action_name, request, &ui).await?;
        if action_name == "configure" {
            let operation = result["operation"]
                .as_str()
                .unwrap_or("Venue configuration");
            let status = result["status"].as_str().unwrap_or("recorded");
            let message = format!("{operation}: {status}");
            let displayed = if matches!(status, "acknowledged" | "completed") {
                ui.success(&message)
            } else {
                ui.warning(&message)
            };
            displayed.map_err(|_| {
                GoFinError::validation("The configuration result could not be displayed; inspect its original attempt ID before retrying.")
            })?;
            if let Some(id) = result["id"]
                .as_str()
                .filter(|id| id.starts_with("configuration:"))
            {
                println!("Inspect: gofin status {id}");
            }
            ui.detail(&serde_json::to_string(&result).unwrap_or_default());
            return Ok(());
        }
        if action_name == "policy_set" {
            ui.success("Financial limits updated.").map_err(|_| {
                GoFinError::validation(
                    "Limits may be saved; run gofin policy show to inspect them.",
                )
            })?;
            ui.write_result(&result, "policy").map_err(|_| {
                GoFinError::validation("Limits were saved; run gofin policy show to inspect them.")
            })?;
            return Ok(());
        }
        let output = render_enrollment_result(result.clone(), false)?;
        let summary = output.lines().next().unwrap_or_default();
        ui.success(summary).map_err(|_| {
            GoFinError::validation(
                "Could not display enrollment result; inspect connection status before retrying.",
            )
        })?;
        for warning in result["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(result["cleanup_warning"].as_str())
        {
            let _ = ui.warning(warning);
        }
        if let Some(next) = result["next_action"].as_str() {
            ui.detail(next);
        }
        return Ok(());
    }

    let runtime = gofin_broker::protected_runtime().await?;
    if let Some(uid) = args.approver_uid
        && uid != runtime.configuration().approver_uid
    {
        return Err(GoFinError::new(
            "APPROVER_UID_MISMATCH",
            "startup",
            "Installer approver UID differs from protected configuration.",
            "Repair the launchd service and protected config through the root installer.",
        ));
    }
    if args.approver_gid == Some(0) {
        return Err(GoFinError::new(
            "APPROVER_GROUP_INVALID",
            "startup",
            "The configured human socket group must be nonzero.",
            "Repair the launchd socket group through the root installer.",
        ));
    }
    gofin_broker::serve_protected(runtime).await
}

fn render_enrollment_result(value: Value, json: bool) -> gofin_domain::Result<String> {
    use gofin_discovery::enrollment_contract::{EnrollmentOutcome, PublicConnection};
    let invalid = || {
        GoFinError::new(
            "ENROLLMENT_RESULT_FAILED",
            "enrollment",
            "The enrollment result could not be displayed.",
            "Inspect protected configuration before retrying; the change may already be active.",
        )
    };
    if json {
        return serde_json::to_string(&value).map_err(|_| invalid());
    }
    let result: EnrollmentOutcome = serde_json::from_value(value).map_err(|_| invalid())?;
    if !result.configuration_authorized
        || result.financial_movement
        || result.future_financial_authority
    {
        return Err(invalid());
    }
    let mut output = if result.action == "initialize" {
        "GoFin configuration approved.".to_owned()
    } else {
        let name = match result.connection.as_ref().ok_or_else(invalid)? {
            PublicConnection::Binance { name, .. }
            | PublicConnection::Ethereum { name, .. }
            | PublicConnection::Evm { name, .. }
            | PublicConnection::CoinGecko { name, .. }
            | PublicConnection::Hyperliquid { name, .. }
            | PublicConnection::Kraken { name, .. }
            | PublicConnection::KrakenFutures { name, .. }
            | PublicConnection::Coinbase { name, .. }
            | PublicConnection::CoinbaseExchange { name, .. }
            | PublicConnection::CoinbaseInternational { name, .. }
            | PublicConnection::Solana { name, .. }
            | PublicConnection::Papertrade { name, .. }
            | PublicConnection::Jupiter { name, .. }
            | PublicConnection::Relay { name, .. } => name,
        };
        let verb = match result.action.as_str() {
            "add" => "added",
            "update" => "updated",
            "remove" => "removed",
            _ => return Err(invalid()),
        };
        format!("Connection {name:?} {verb}.")
    };
    for warning in result.warnings.iter().chain(result.cleanup_warning.iter()) {
        output.push_str("\nWarning: ");
        output.push_str(warning);
    }
    Ok(output)
}

fn validate_public_enrollment_request(text: &str) -> gofin_domain::Result<Value> {
    if text.len() > 65_536 {
        return Err(GoFinError::validation(
            "Public enrollment request exceeds its size limit.",
        ));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| GoFinError::validation("Enrollment request must be public JSON."))?;
    fn inspect(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().all(|(key, child)| {
                let normalized = key.to_ascii_lowercase();
                !matches!(
                    normalized.as_str(),
                    "secret"
                        | "api_key"
                        | "private_key"
                        | "signed"
                        | "signature"
                        | "approval"
                        | "approval_response"
                        | "financial_state"
                ) && inspect(child)
            }),
            Value::Array(values) => values.iter().all(inspect),
            _ => true,
        }
    }
    if !value.is_object() || !inspect(&value) {
        return Err(GoFinError::validation(
            "Enrollment request must contain public metadata only; secret or approval fields are forbidden.",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_analytics_excludes_internal_maintenance_entrypoints() {
        for arguments in [
            vec!["gofin-broker", "--export-registry"],
            vec!["gofin-broker", "--export-storage-contract"],
            vec!["gofin-broker", "--hold-runtime-update", "123"],
            vec![
                "gofin-broker",
                "--verify-update-bundle",
                "/synthetic/bundle",
            ],
        ] {
            assert!(!reports_analytics(
                &Args::try_parse_from(arguments).unwrap()
            ));
        }
        for arguments in [
            vec!["gofin-broker"],
            vec!["gofin-broker", "--runtime-update-release", "v1.0.1-beta.1"],
            vec!["gofin-broker", "enrollment", "add", "--request", "{}"],
            vec!["gofin-broker", "configure", "--request", "{}"],
            vec!["gofin-broker", "yolo-enable"],
            vec!["gofin-broker", "yolo-disable"],
        ] {
            assert!(reports_analytics(&Args::try_parse_from(arguments).unwrap()));
        }
    }

    #[test]
    fn broker_analytics_inspects_only_public_configuration_operation() {
        let args = Args::try_parse_from([
            "gofin-broker",
            "configure",
            "--request",
            r#"{"operation":"kraken.account.set_position_mode","input":{"account":"synthetic-private-account"}}"#,
        ])
        .unwrap();
        assert_eq!(
            analytics_operation(&args).as_deref(),
            Some("kraken.account.set_position_mode")
        );
        for arguments in [
            vec![
                "gofin-broker",
                "enrollment",
                "add",
                "--request",
                r#"{"operation":"not-a-configuration-operation"}"#,
            ],
            vec!["gofin-broker", "configure", "--request", "invalid"],
            vec![
                "gofin-broker",
                "configure",
                "--request",
                r#"{"operation":"kraken.account.set_position_mode","secret":"synthetic-secret"}"#,
            ],
        ] {
            assert!(analytics_operation(&Args::try_parse_from(arguments).unwrap()).is_none());
        }
    }

    #[test]
    fn lifecycle_owners_retain_panic_reporting_without_duplicate_failure_events() {
        let update =
            Args::try_parse_from(["gofin-broker", "--runtime-update-release", "v1.0.1-beta.1"])
                .unwrap();
        assert!(reports_analytics(&update));
        assert!(!reports_failure_analytics(&update));
        let initialize = Args::try_parse_from([
            "gofin-broker",
            "enrollment",
            "initialize",
            "--request",
            "{}",
        ])
        .unwrap();
        assert!(reports_analytics(&initialize));
        assert!(!reports_failure_analytics(&initialize));
        let enrollment =
            Args::try_parse_from(["gofin-broker", "enrollment", "add", "--request", "{}"]).unwrap();
        assert!(reports_failure_analytics(&enrollment));
    }

    #[tokio::test]
    async fn machine_enrollment_stops_before_metadata_or_protected_access() {
        let args = Args::try_parse_from([
            "gofin-broker",
            "enrollment",
            "add",
            "--request",
            "INVALID_SYNTHETIC_INPUT",
            "--json",
        ])
        .unwrap();
        assert_eq!(run(args).await.unwrap_err().code, "HUMAN_TERMINAL_REQUIRED");

        let args = Args::try_parse_from([
            "gofin-broker",
            "enrollment",
            "configure",
            "--request",
            "INVALID_SYNTHETIC_INPUT",
            "--json",
        ])
        .unwrap();
        assert_eq!(run(args).await.unwrap_err().code, "HUMAN_TERMINAL_REQUIRED");
    }

    #[tokio::test]
    async fn machine_configuration_stops_before_input_parse_or_protected_access() {
        let args = Args::try_parse_from([
            "gofin-broker",
            "configure",
            "--request",
            "INVALID_SYNTHETIC_INPUT",
            "--json",
        ])
        .unwrap();
        let failure = run(args).await.unwrap_err();
        assert_eq!(failure.code, "HUMAN_TERMINAL_REQUIRED");
        assert_eq!(failure.stage, "configuration");
    }

    fn saved_connection(action: &str) -> Value {
        serde_json::json!({
            "action":action,
            "connection":{"provider":"binance","name":"main","resource":"binance:uid:123:SPOT","role":"main","parent":null,"email":null,"wallet_type":"SPOT","physical_identity":"123"},
            "checks":[{"name":"synthetic read check","status":"passed"}],
            "warnings":[],"cleanup_warning":null,"configuration_authorized":true,
            "financial_movement":false,"future_financial_authority":false,
            "next_action":"Run gofin connections list --json and gofin doctor --json."
        })
    }

    #[test]
    fn enrollment_result_defaults_to_concise_text_and_preserves_explicit_json() {
        for (action, verb) in [
            ("add", "added"),
            ("update", "updated"),
            ("remove", "removed"),
        ] {
            let result = saved_connection(action);
            assert_eq!(
                render_enrollment_result(result.clone(), false).unwrap(),
                format!("Connection \"main\" {verb}.")
            );
            assert_eq!(
                serde_json::from_str::<Value>(
                    &render_enrollment_result(result.clone(), true).unwrap()
                )
                .unwrap(),
                result
            );
        }
        let parsed = Args::try_parse_from([
            "gofin-broker",
            "enrollment",
            "add",
            "--request",
            "{}",
            "--json",
        ])
        .unwrap();
        assert!(parsed.json);
    }

    #[test]
    fn enrollment_result_renders_solana_and_jupiter_for_all_actions() {
        for (connection, name) in [
            (
                serde_json::json!({"provider":"solana","name":"observer","resource":"solana:mainnet-beta:observer","address":"SyntheticAddress","network":"solana:mainnet-beta","signing_enabled":false}),
                "observer",
            ),
            (
                serde_json::json!({"provider":"solana","name":"signer","resource":"solana:mainnet-beta:signer","address":"SyntheticSignerAddress","network":"solana:mainnet-beta","signing_enabled":true}),
                "signer",
            ),
            (
                serde_json::json!({"provider":"jupiter","name":"jupiter-service","resource":"jupiter:service","credential_configured":true}),
                "jupiter-service",
            ),
        ] {
            for (action, verb) in [
                ("add", "added"),
                ("update", "updated"),
                ("remove", "removed"),
            ] {
                let mut result = saved_connection(action);
                result["connection"] = connection.clone();
                assert_eq!(
                    render_enrollment_result(result.clone(), false).unwrap(),
                    format!("Connection {name:?} {verb}.")
                );
                assert_eq!(
                    serde_json::from_str::<Value>(
                        &render_enrollment_result(result.clone(), true).unwrap()
                    )
                    .unwrap(),
                    result
                );
            }
        }
    }

    #[test]
    fn human_result_keeps_warnings_and_never_calls_unapproved_configuration_successful() {
        let mut result = saved_connection("update");
        result["warnings"] = serde_json::json!(["Reading permission was not reported."]);
        result["cleanup_warning"] = serde_json::json!("An obsolete credential needs cleanup.");
        assert_eq!(
            render_enrollment_result(result.clone(), false).unwrap(),
            "Connection \"main\" updated.\nWarning: Reading permission was not reported.\nWarning: An obsolete credential needs cleanup."
        );
        result["configuration_authorized"] = serde_json::json!(false);
        assert!(render_enrollment_result(result, false).is_err());
    }

    #[test]
    fn public_enrollment_json_rejects_secret_fields_but_accepts_opaque_refs() {
        assert!(
            validate_public_enrollment_request(
                r#"{"name":"main","credential_ref":"550e8400-e29b-41d4-a716-446655440000"}"#
            )
            .is_ok()
        );
        assert!(validate_public_enrollment_request(r#"{"secret":"SYNTHETIC-SECRET"}"#).is_err());
        assert!(
            validate_public_enrollment_request(r#"{"nested":{"private_key":"SYNTHETIC-KEY"}}"#)
                .is_err()
        );
    }
}
