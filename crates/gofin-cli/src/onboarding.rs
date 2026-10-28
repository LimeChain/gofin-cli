//! Guided public connection metadata. Secret entry remains in protected enrollment.

use clap::ValueEnum;
use gofin_discovery::{AddConnectionArgs, BinanceRole, CredentialType, Provider};
use gofin_domain::GoFinError;
use gofin_presentation::safe_text;
use std::io::{self, BufRead, IsTerminal, Read, Write};

pub(crate) fn guided_add(provider: Option<Provider>) -> Result<AddConnectionArgs, GoFinError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        return Err(GoFinError::new(
            "HUMAN_TERMINAL_REQUIRED",
            "onboarding",
            "Guided connection setup requires your own interactive Terminal.",
            "Run gofin connections add in your Terminal, or inspect gofin connections add --help for public metadata flags.",
        ));
    }
    guided_add_with(&mut io::stdin().lock(), &mut io::stderr().lock(), provider)
}

fn guided_add_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    provider: Option<Provider>,
) -> Result<AddConnectionArgs, GoFinError> {
    writeln!(
        output,
        "Connection details only. Credentials are requested privately afterward."
    )
    .map_err(|_| output_error())?;
    let provider = match provider {
        Some(provider) => provider,
        None => choice(
            input,
            output,
            "Provider",
            Provider::value_variants(),
            &Provider::Binance,
        )?,
    };
    let name = prompt(input, output, "Connection name", None)?;
    gofin_discovery::validate_name(&name)?;
    let mut args = AddConnectionArgs {
        provider,
        name,
        role: None,
        parent: None,
        email: None,
        network: None,
        address: None,
        hyperliquid_scope: None,
        execution_address: None,
        credential_type: None,
        api_plan: None,
        account_id: None,
        default_wallet_account_id: None,
        portfolio_id: None,
    };
    match args.provider {
        Provider::Coinbase => {
            args.credential_type = Some(choice(
                input,
                output,
                "Credential type",
                &[CredentialType::Ed25519, CredentialType::Ecdsa],
                &CredentialType::Ed25519,
            )?);
            let portfolio = prompt(
                input,
                output,
                "Permissioned portfolio UUID (optional; verified from your key)",
                None,
            )?;
            if !portfolio.is_empty() {
                args.portfolio_id = Some(portfolio);
            }
        }
        Provider::CoinbaseExchange | Provider::CoinbaseInternational => {
            args.credential_type = Some(CredentialType::Hmac);
            let portfolio = prompt(
                input,
                output,
                "Exact portfolio/profile ID (optional when one is accessible)",
                None,
            )?;
            if !portfolio.is_empty() {
                args.portfolio_id = Some(portfolio);
            }
        }
        Provider::Kraken | Provider::KrakenFutures => {
            args.account_id = Some(prompt(
                input,
                output,
                "Public Kraken account ID (confirmed by you)",
                None,
            )?);
            if args.provider == Provider::Kraken {
                let wallet = prompt(
                    input,
                    output,
                    "Default Funding wallet account ID (your verified selection; optional, leave blank)",
                    None,
                )?;
                if !wallet.is_empty() {
                    args.default_wallet_account_id = Some(wallet);
                }
            }
            args.credential_type = Some(CredentialType::Hmac);
        }
        Provider::CoinGecko => {
            args.credential_type = Some(CredentialType::ApiKey);
            args.api_plan = Some(prompt(input, output, "API plan (demo/pro)", Some("demo"))?);
        }
        Provider::Binance => {
            let role = choice(
                input,
                output,
                "Account role",
                BinanceRole::value_variants(),
                &BinanceRole::Main,
            )?;
            args.role = Some(role);
            if role == BinanceRole::Sub {
                let parent = prompt(
                    input,
                    output,
                    "Parent main connection name (leave blank for standalone subaccount access)",
                    None,
                )?;
                if !parent.is_empty() {
                    args.parent = Some(parent);
                    args.email = Some(prompt(input, output, "Subaccount email", None)?);
                }
            }
            args.credential_type = Some(choice(
                input,
                output,
                "Credential type",
                &[
                    CredentialType::Hmac,
                    CredentialType::Rsa,
                    CredentialType::Ed25519,
                ],
                &CredentialType::Hmac,
            )?);
        }
        Provider::Evm => {
            let kind = choice(
                input,
                output,
                "Credential type",
                &[CredentialType::PrivateKey, CredentialType::ReadOnly],
                &CredentialType::PrivateKey,
            )?;
            args.credential_type = Some(kind);
            let label = if kind == CredentialType::ReadOnly {
                "Public wallet address"
            } else {
                "Public wallet address (optional; checked against signing key)"
            };
            let address = prompt(input, output, label, None)?;
            if !address.is_empty() {
                args.address = Some(address);
            }
        }
        Provider::Hyperliquid => {
            args.credential_type = Some(choice(
                input,
                output,
                "Credential type",
                &[CredentialType::PrivateKey, CredentialType::ReadOnly],
                &CredentialType::PrivateKey,
            )?);
            args.address = Some(prompt(input, output, "Hyperliquid owner address", None)?);
        }
        Provider::Solana => {
            let kind = choice(
                input,
                output,
                "Credential type",
                &[CredentialType::PrivateKey, CredentialType::ReadOnly],
                &CredentialType::PrivateKey,
            )?;
            args.credential_type = Some(kind);
            let label = if kind == CredentialType::ReadOnly {
                "Public Solana wallet address"
            } else {
                "Public Solana wallet address (optional; checked against signing key)"
            };
            let address = prompt(input, output, label, None)?;
            if !address.is_empty() {
                args.address = Some(address);
            }
        }
        Provider::Jupiter | Provider::Relay => args.credential_type = Some(CredentialType::ApiKey),
    }
    // The same validation serves flags, schemas, and this guided path.
    gofin_discovery::validate_add(&args)?;
    Ok(args)
}

fn choice<T: ValueEnum + PartialEq>(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    choices: &[T],
    default: &T,
) -> Result<T, GoFinError> {
    let names: Vec<_> = choices
        .iter()
        .filter_map(|value| value.to_possible_value())
        .map(|value| value.get_name().to_owned())
        .collect();
    let default = default.to_possible_value().expect("visible enum default");
    let value = prompt(
        input,
        output,
        &format!("{label} ({})", names.join("/")),
        Some(default.get_name()),
    )?;
    T::from_str(&value, true)
        .ok()
        .filter(|value| choices.contains(value))
        .ok_or_else(|| {
            GoFinError::validation(format!(
                "Choose {} for {}.",
                names.join(" or "),
                label.to_lowercase()
            ))
        })
}

fn prompt(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: Option<&str>,
) -> Result<String, GoFinError> {
    write!(output, "{}", safe_text(label)).map_err(|_| output_error())?;
    if let Some(default) = default {
        write!(output, " [{}]", safe_text(default)).map_err(|_| output_error())?;
    }
    write!(output, ": ")
        .and_then(|_| output.flush())
        .map_err(|_| output_error())?;

    // Every supported public field fits this bound. Never consume unbounded
    // pasted input or echo it in an error; this process must not collect secrets.
    let mut bytes = Vec::new();
    let count = input
        .take(1025)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| {
            GoFinError::new(
                "ONBOARDING_INPUT_FAILED",
                "onboarding",
                "Could not read the connection detail.",
                "Retry gofin connections add in your Terminal.",
            )
        })?;
    if count == 0 {
        return Err(GoFinError::new(
            "ONBOARDING_CANCELLED",
            "onboarding",
            "Connection setup ended before all details were entered.",
            "Run gofin connections add when ready to continue.",
        ));
    }
    if count > 1024 {
        return Err(GoFinError::validation(
            "The connection detail is too long. Enter public metadata only.",
        ));
    }
    let value = String::from_utf8(bytes)
        .map_err(|_| GoFinError::validation("Connection details must be valid text."))?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value.to_owned()
    })
}

fn output_error() -> GoFinError {
    GoFinError::new(
        "ONBOARDING_OUTPUT_FAILED",
        "onboarding",
        "Could not display the connection prompt.",
        "Retry gofin connections add in your Terminal.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(text: &str) -> (Result<AddConnectionArgs, GoFinError>, String) {
        let mut output = Vec::new();
        let result = guided_add_with(&mut Cursor::new(text.as_bytes()), &mut output, None);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn shared_validation_rejects_bad_address_network_relationship_and_name() {
        for input in [
            "evm\nwatch\nread_only\n\n",
            "evm\nwatch\nread_only\n0x0000000000000000000000000000000000000000\n",
            "evm\nwatch\nhmac\n\n",
            "binance\nchild\nsub\nmain\ninvalid-email\nhmac\n",
            "binance\nINVALID NAME\n",
        ] {
            assert_eq!(run(input).0.unwrap_err().code, "INVALID_INPUT");
        }
    }

    #[test]
    fn invalid_choice_never_echoes_pasted_input() {
        let input = "synthetic-not-a-credential\x1b[2J";
        let (result, output) = run(&format!("{input}\n"));
        let error = result.unwrap_err();
        assert!(!error.to_string().contains(input));
        assert!(!output.contains(input));
        let (result, _) = run("binance\nmain\nmain\nprivate_key\n");
        assert_eq!(result.unwrap_err().code, "INVALID_INPUT");
    }

    #[test]
    fn eof_at_every_prompt_cancels_without_retries() {
        for input in ["", "\n", "\nmain\n", "\nmain\n\n", "evm\nwallet\n\n"] {
            let (result, output) = run(input);
            assert_eq!(result.unwrap_err().code, "ONBOARDING_CANCELLED");
            assert!(output.lines().count() <= 2);
        }
    }

    #[test]
    fn oversized_input_and_output_failure_do_not_continue() {
        assert_eq!(run(&"x".repeat(1025)).0.unwrap_err().code, "INVALID_INPUT");
        struct Unwritable;
        impl Write for Unwritable {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("synthetic"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut input = Cursor::new(b"binance\nmain\n");
        assert_eq!(
            guided_add_with(&mut input, &mut Unwritable, None)
                .unwrap_err()
                .code,
            "ONBOARDING_OUTPUT_FAILED"
        );
        assert_eq!(input.position(), 0);
    }
}
