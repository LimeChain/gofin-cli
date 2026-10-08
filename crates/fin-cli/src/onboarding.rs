//! Guided public connection metadata. Secret entry remains in protected enrollment.

use clap::ValueEnum;
use fin_discovery::{AddConnectionArgs, BinanceRole, CredentialType, Provider};
use fin_domain::FinError;
use fin_presentation::safe_text;
use std::io::{self, BufRead, IsTerminal, Read, Write};

pub(crate) fn guided_add(provider: Option<Provider>) -> Result<AddConnectionArgs, FinError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        return Err(FinError::new(
            "HUMAN_TERMINAL_REQUIRED",
            "onboarding",
            "Guided connection setup requires your own interactive Terminal.",
            "Run fin connections add in your Terminal, or inspect fin connections add --help for public metadata flags.",
        ));
    }
    guided_add_with(&mut io::stdin().lock(), &mut io::stderr().lock(), provider)
}

fn guided_add_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    provider: Option<Provider>,
) -> Result<AddConnectionArgs, FinError> {
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
    fin_discovery::validate_name(&name)?;
    let mut args = AddConnectionArgs {
        provider,
        name,
        role: None,
        parent: None,
        email: None,
        network: None,
        address: None,
        credential_type: None,
        api_plan: None,
    };
    match args.provider {
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
    }
    // The same validation serves flags, schemas, and this guided path.
    fin_discovery::validate_add(&args)?;
    Ok(args)
}

fn choice<T: ValueEnum + PartialEq>(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    choices: &[T],
    default: &T,
) -> Result<T, FinError> {
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
            FinError::validation(format!(
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
) -> Result<String, FinError> {
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
            FinError::new(
                "ONBOARDING_INPUT_FAILED",
                "onboarding",
                "Could not read the connection detail.",
                "Retry fin connections add in your Terminal.",
            )
        })?;
    if count == 0 {
        return Err(FinError::new(
            "ONBOARDING_CANCELLED",
            "onboarding",
            "Connection setup ended before all details were entered.",
            "Run fin connections add when ready to continue.",
        ));
    }
    if count > 1024 {
        return Err(FinError::validation(
            "The connection detail is too long. Enter public metadata only.",
        ));
    }
    let value = String::from_utf8(bytes)
        .map_err(|_| FinError::validation("Connection details must be valid text."))?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value.to_owned()
    })
}

fn output_error() -> FinError {
    FinError::new(
        "ONBOARDING_OUTPUT_FAILED",
        "onboarding",
        "Could not display the connection prompt.",
        "Retry fin connections add in your Terminal.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(text: &str) -> (Result<AddConnectionArgs, FinError>, String) {
        let mut output = Vec::new();
        let result = guided_add_with(&mut Cursor::new(text.as_bytes()), &mut output, None);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn main_defaults_build_the_shared_validated_args_without_secret_prompts() {
        let (result, output) = run("\nmain\n\n\n");
        let args = result.unwrap();
        assert_eq!(args.provider, Provider::Binance);
        assert_eq!(args.name, "main");
        assert_eq!(args.role, Some(BinanceRole::Main));
        assert_eq!(args.credential_type, Some(CredentialType::Hmac));
        assert!(args.parent.is_none() && args.email.is_none());
        fin_discovery::validate_add(&args).unwrap();
        assert!(!output.contains("API key:"));
        assert!(!output.contains("Private key:"));
        assert!(!output.contains("secret:"));
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn subaccount_parent_link_is_optional_and_collects_public_relationship_fields() {
        let (result, output) = run("binance\nsavings\nsub\nmain\nchild@example.invalid\ned25519\n");
        let args = result.unwrap();
        assert_eq!(args.role, Some(BinanceRole::Sub));
        assert_eq!(args.parent.as_deref(), Some("main"));
        assert_eq!(args.email.as_deref(), Some("child@example.invalid"));
        assert_eq!(args.credential_type, Some(CredentialType::Ed25519));
        assert!(output.contains("Parent main connection name"));
    }

    #[test]
    fn subaccount_can_be_enrolled_without_master_metadata() {
        let (result, output) = run("binance\nsavings\nsub\n\nhmac\n");
        let args = result.unwrap();
        assert_eq!(args.role, Some(BinanceRole::Sub));
        assert!(args.parent.is_none() && args.email.is_none());
        assert_eq!(args.credential_type, Some(CredentialType::Hmac));
        assert!(output.contains("leave blank for standalone subaccount access"));
    }

    #[test]
    fn evm_signing_and_read_only_reuse_wallet_without_network_prompt() {
        let (result, output) = run("evm\nwallet\n\n\n");
        let args = result.unwrap();
        assert!(args.network.is_none());
        assert!(output.contains("Provider (binance/evm/coingecko)"));
        assert!(!output.contains("Network"));
        assert!(!output.contains("Private key:"));
        assert_eq!(args.credential_type, Some(CredentialType::PrivateKey));
        assert!(args.address.is_none());
        let address = "0x1111111111111111111111111111111111111111";
        let (result, _) = run(&format!("ethereum\nwatch\nread_only\n{address}\n"));
        assert_eq!(result.unwrap().address.as_deref(), Some(address));
    }

    #[test]
    fn coingecko_guided_add_collects_plan_without_a_secret_prompt() {
        for (answer, expected) in [("", "demo"), ("pro", "pro")] {
            let (result, output) = run(&format!("coingecko\npricing\n{answer}\n"));
            let args = result.unwrap();
            assert_eq!(args.provider, Provider::CoinGecko);
            assert_eq!(args.credential_type, Some(CredentialType::ApiKey));
            assert_eq!(args.api_plan.as_deref(), Some(expected));
            assert!(output.contains("API plan (demo/pro) [demo]"));
            assert!(!output.contains("API key:"));
            assert!(!output.contains("Network"));
            fin_discovery::validate_add(&args).unwrap();
        }
        assert_eq!(
            run("coingecko\npricing\nenterprise\n").0.unwrap_err().code,
            "INVALID_INPUT"
        );
        assert_eq!(
            run("coingecko\npricing\n").0.unwrap_err().code,
            "ONBOARDING_CANCELLED"
        );
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

    #[test]
    fn positional_provider_skips_only_provider_question() {
        let mut output = Vec::new();
        let mut input = Cursor::new(b"master\nmain\ned25519\n");
        let args = guided_add_with(&mut input, &mut output, Some(Provider::Binance)).unwrap();
        assert_eq!(args.provider, Provider::Binance);
        assert_eq!(args.name, "master");
        assert_eq!(args.credential_type, Some(CredentialType::Ed25519));
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("Provider"));
        assert!(output.contains("Connection name"));
    }
}
