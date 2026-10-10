use clap::{Parser, Subcommand, error::ErrorKind};
use fin_domain::FinError;
use fin_presentation::Ui;
use serde_json::Value;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "fin-broker", color = clap::ColorChoice::Never, version, about = "Protected Fin broker service")]
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
    Enrollment {
        #[command(subcommand)]
        action: EnrollmentAction,
    },
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
            let _ = Ui::new(machine, false).write_error(&FinError::new(
                "ARGUMENTS_INVALID",
                "cli",
                "The broker command line is invalid.",
                "Use fin --help for the public command interface.",
            ));
            return ExitCode::FAILURE;
        }
    };
    let ui = Ui::new(args.json, args.verbose);
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = ui.write_error(&error);
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> fin_domain::Result<()> {
    if args.export_storage_contract {
        println!(
            "{}",
            serde_json::json!({"journal_schema_version": fin_store::supported_schema_version()})
        );
        return Ok(());
    }
    if args.export_registry {
        let definitions = fin_broker::operation_definitions();
        let bytes = serde_json::to_vec_pretty(&definitions).map_err(|_| {
            FinError::new(
                "REGISTRY_EXPORT_FAILED",
                "registry",
                "The static registry could not be serialized.",
                "Inspect the compiled operation definitions.",
            )
        })?;
        println!("{}", String::from_utf8_lossy(&bytes));
        return Ok(());
    }

    if let Some(Command::Enrollment { action }) = args.command {
        if args.json {
            return Err(FinError::new(
                "HUMAN_TERMINAL_REQUIRED",
                "enrollment",
                "Enrollment requires your own interactive Terminal.",
                "Run fin policy set, fin connections add or fin setup yourself without --json.",
            ));
        }
        let ui = Ui::new(false, args.verbose);
        let (action_name, request) = match action {
            EnrollmentAction::PolicySet { request } => ("policy_set", request),
            EnrollmentAction::Initialize { request } => ("initialize", request),
            EnrollmentAction::Add { request } => ("add", request),
            EnrollmentAction::Update { request } => ("update", request),
            EnrollmentAction::Remove { request } => ("remove", request),
        };
        let request = validate_public_enrollment_request(&request)?;
        let result = fin_broker::enrollment::handle_with_ui(action_name, request, &ui).await?;
        if action_name == "policy_set" {
            ui.success("Financial limits updated.").map_err(|_| {
                FinError::validation("Limits may be saved; run fin policy show to inspect them.")
            })?;
            ui.write_result(&result, "policy").map_err(|_| {
                FinError::validation("Limits were saved; run fin policy show to inspect them.")
            })?;
            return Ok(());
        }
        let output = render_enrollment_result(result.clone(), false)?;
        let summary = output.lines().next().unwrap_or_default();
        ui.success(summary).map_err(|_| {
            FinError::validation(
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

    let runtime = fin_broker::protected_runtime().await?;
    if let Some(uid) = args.approver_uid
        && uid != runtime.configuration().approver_uid
    {
        return Err(FinError::new(
            "APPROVER_UID_MISMATCH",
            "startup",
            "Installer approver UID differs from protected configuration.",
            "Repair the launchd service and protected config through the root installer.",
        ));
    }
    if args.approver_gid == Some(0) {
        return Err(FinError::new(
            "APPROVER_GROUP_INVALID",
            "startup",
            "The configured human socket group must be nonzero.",
            "Repair the launchd socket group through the root installer.",
        ));
    }
    fin_broker::serve_protected(runtime).await
}

fn render_enrollment_result(value: Value, json: bool) -> fin_domain::Result<String> {
    use fin_discovery::enrollment_contract::{EnrollmentOutcome, PublicConnection};
    let invalid = || {
        FinError::new(
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
        "Fin configuration approved.".to_owned()
    } else {
        let name = match result.connection.as_ref().ok_or_else(invalid)? {
            PublicConnection::Binance { name, .. }
            | PublicConnection::Ethereum { name, .. }
            | PublicConnection::Evm { name, .. }
            | PublicConnection::CoinGecko { name, .. } => name,
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

fn validate_public_enrollment_request(text: &str) -> fin_domain::Result<Value> {
    if text.len() > 65_536 {
        return Err(FinError::validation(
            "Public enrollment request exceeds its size limit.",
        ));
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| FinError::validation("Enrollment request must be public JSON."))?;
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
        return Err(FinError::validation(
            "Enrollment request must contain public metadata only; secret or approval fields are forbidden.",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn machine_enrollment_stops_before_metadata_or_protected_access() {
        let args = Args::try_parse_from([
            "fin-broker",
            "enrollment",
            "add",
            "--request",
            "INVALID_SYNTHETIC_INPUT",
            "--json",
        ])
        .unwrap();
        assert_eq!(run(args).await.unwrap_err().code, "HUMAN_TERMINAL_REQUIRED");
    }

    fn saved_connection(action: &str) -> Value {
        serde_json::json!({
            "action":action,
            "connection":{"provider":"binance","name":"main","resource":"binance:uid:123:SPOT","role":"main","parent":null,"email":null,"wallet_type":"SPOT","physical_identity":"123"},
            "checks":[{"name":"synthetic read check","status":"passed"}],
            "warnings":[],"cleanup_warning":null,"configuration_authorized":true,
            "financial_movement":false,"future_financial_authority":false,
            "next_action":"Run fin connections list --json and fin doctor --json."
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
            "fin-broker",
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
