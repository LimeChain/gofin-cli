use clap::{Parser, Subcommand, error::ErrorKind};
use fin_domain::FinError;
use serde_json::Value;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "fin-broker", version, about = "Protected Fin broker service")]
struct Args {
    /// Export the shared operation registry without opening protected state.
    #[arg(long)]
    export_registry: bool,
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
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(_) => {
            eprintln!("ARGUMENTS_INVALID: The broker command line is invalid.");
            return ExitCode::FAILURE;
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}: {}", error.code, error.message);
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> fin_domain::Result<()> {
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
        let (action_name, request) = match action {
            EnrollmentAction::Initialize { request } => ("initialize", request),
            EnrollmentAction::Add { request } => ("add", request),
            EnrollmentAction::Update { request } => ("update", request),
            EnrollmentAction::Remove { request } => ("remove", request),
        };
        let request = validate_public_enrollment_request(&request)?;
        let result = fin_broker::enrollment::handle(action_name, request).await?;
        let output = serde_json::to_string(&result).map_err(|_| FinError::new(
            "ENROLLMENT_RESULT_FAILED",
            "enrollment",
            "The safe enrollment result could not be encoded.",
            "Inspect protected configuration and retry only after confirming its current state.",
        ))?;
        println!("{output}");
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
