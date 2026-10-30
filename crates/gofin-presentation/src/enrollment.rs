//! Shared presentation for human connection enrollment in production and development.
use super::*;

impl Ui {
    /// Show the public enrollment result with semantic color and no secret fields.
    pub fn write_enrollment_result(&self, value: &Value) -> gofin_domain::Result<()> {
        self.write_enrollment_result_to(value, &mut io::stdout().lock(), &mut io::stderr().lock())
    }

    fn write_enrollment_result_to(
        &self,
        value: &Value,
        output: &mut impl Write,
        warnings: &mut impl Write,
    ) -> gofin_domain::Result<()> {
        let rendered = render_enrollment_result(value.clone(), false)?;
        let failure = |_| {
            GoFinError::new(
                "ENROLLMENT_RESULT_FAILED",
                "enrollment",
                "Could not display enrollment result.",
                "Inspect connection status before retrying; the change may already be active.",
            )
        };
        for line in rendered
            .lines()
            .take_while(|line| !line.starts_with("Warning: "))
        {
            self.message_to(&format!("✓ {line}"), "32", output)
                .map_err(failure)?;
        }
        for warning in value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(value["cleanup_warning"].as_str())
        {
            self.message_to(&format!("Warning: {warning}"), "33", warnings)
                .map_err(failure)?;
        }
        if let Some(next) = value["next_action"].as_str() {
            self.detail_to(next, warnings).map_err(failure)?;
        }
        Ok(())
    }
}

/// Render only the redacted public enrollment contract, never credential material.
pub fn render_enrollment_result(value: Value, json: bool) -> gofin_domain::Result<String> {
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
    } else if result.action == "generate" {
        let (network, name, address) = match result.connection.as_ref().ok_or_else(invalid)? {
            PublicConnection::Evm {
                name,
                address,
                signing_enabled: true,
                ..
            } => ("EVM", name, address),
            PublicConnection::Solana {
                name,
                address,
                signing_enabled: true,
                ..
            } => ("Solana", name, address),
            _ => return Err(invalid()),
        };
        format!("{network} wallet {name:?} generated.\nAddress: {address}")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(provider: &str) -> Value {
        let address = if provider == "evm" {
            "0x1111111111111111111111111111111111111111"
        } else {
            "11111111111111111111111111111111"
        };
        let mut connection = json!({"provider":provider,"name":"wallet","resource":format!("{provider}:{address}"),"address":address,"signing_enabled":true});
        if provider == "solana" {
            connection["network"] = json!("solana-mainnet");
        }
        json!({"action":"generate","connection":connection,"checks":[],"warnings":["Synthetic warning"],"cleanup_warning":null,"configuration_authorized":true,"financial_movement":false,"future_financial_authority":false,"next_action":"Run gofin connections list."})
    }

    #[test]
    fn generated_wallet_success_has_full_public_address_and_semantic_rows() {
        for provider in ["evm", "solana"] {
            let result = generated(provider);
            for (stdout_tty, stderr_tty, dumb, no_color, colored) in [
                (true, true, false, false, true),
                (true, true, false, true, false),
                (true, true, true, false, false),
                (false, false, false, false, false),
            ] {
                let ui = Ui {
                    machine: false,
                    verbose: false,
                    terminal: TerminalPolicy::from_environment(
                        stdout_tty, stderr_tty, dumb, no_color,
                    ),
                };
                let mut output = Vec::new();
                let mut warnings = Vec::new();
                ui.write_enrollment_result_to(&result, &mut output, &mut warnings)
                    .unwrap();
                let output = String::from_utf8(output).unwrap();
                let warnings = String::from_utf8(warnings).unwrap();
                assert!(output.contains("wallet \"wallet\" generated."));
                assert!(output.contains(&format!(
                    "Address: {}",
                    result["connection"]["address"].as_str().unwrap()
                )));
                assert_eq!(output.lines().count(), 2);
                assert!(!output.contains("private_key"));
                assert!(!output.contains("credential"));
                assert!(!output.contains("Synthetic warning"));
                assert!(warnings.contains("Warning: Synthetic warning"));
                assert!(!warnings.contains("Run gofin connections list"));
                if colored {
                    assert!(output.lines().all(|line| line.starts_with("\x1b[32m✓ ") && line.ends_with("\x1b[0m")));
                    assert!(warnings.starts_with("\x1b[33mWarning: "));
                } else {
                    assert!(!output.contains('\x1b'));
                    assert!(!warnings.contains('\x1b'));
                }
            }
        }
    }

    #[test]
    fn enrollment_human_rows_stay_quiet_in_machine_mode_and_reject_invalid_results() {
        let mut result = generated("evm");
        let ui = Ui {
            machine: true,
            verbose: true,
            terminal: TerminalPolicy::from_environment(true, true, false, false),
        };
        let mut output = Vec::new();
        let mut warnings = Vec::new();
        ui.write_enrollment_result_to(&result, &mut output, &mut warnings)
            .unwrap();
        assert!(output.is_empty());
        assert!(warnings.is_empty());
        result["configuration_authorized"] = json!(false);
        assert!(
            ui.write_enrollment_result_to(&result, &mut output, &mut warnings)
                .is_err()
        );
        assert!(output.is_empty());
        assert!(warnings.is_empty());
    }
}
