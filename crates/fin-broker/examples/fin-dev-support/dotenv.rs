use std::collections::BTreeMap;
use zeroize::Zeroizing;

const MAX_ENV_FILE: usize = 32 * 1024;

/// Parse the intentionally small .env subset accepted by the development broker.
/// No interpolation, command execution, `export`, or arbitrary process settings.
#[cfg(test)]
pub fn parse(source: &str) -> Result<BTreeMap<String, Zeroizing<String>>, String> {
    parse_selected(source, None)
}

/// Parse only one provider's recognized fields. Unselected assignment values
/// are never decoded or copied into memory as credential strings.
pub fn parse_selected(
    source: &str,
    provider: Option<&str>,
) -> Result<BTreeMap<String, Zeroizing<String>>, String> {
    if source.len() > MAX_ENV_FILE {
        return Err("credential file exceeds the supported size".into());
    }
    let mut values = BTreeMap::new();
    for (line_number, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            if provider
                .is_some_and(|selected| provider_hint(line).is_none_or(|hint| hint != selected))
            {
                continue;
            }
            return Err(format!(
                "credential file has an invalid assignment on line {}",
                line_number + 1
            ));
        };
        let raw_key = key.trim();
        if provider
            .is_some_and(|selected| provider_hint(raw_key).is_some_and(|hint| hint != selected))
        {
            continue;
        }
        let Some(key) = canonical_key(raw_key) else {
            if provider.is_some() {
                continue;
            }
            return Err(format!(
                "credential file has an unsupported key on line {}",
                line_number + 1
            ));
        };
        if provider.is_some_and(|selected| provider_for(key) != Some(selected)) {
            continue;
        }
        let value =
            parse_value(raw_value.trim(), key.ends_with("_PRIVATE_KEY_PEM")).ok_or_else(|| {
                format!(
                    "credential file has an invalid value on line {}",
                    line_number + 1
                )
            })?;
        if values
            .insert(key.to_owned(), Zeroizing::new(value))
            .is_some()
        {
            return Err(format!(
                "credential file repeats a key on line {}",
                line_number + 1
            ));
        }
    }
    if values.is_empty() {
        return Err("credential file contains no supported credentials".into());
    }
    Ok(values)
}

fn provider_hint(key: &str) -> Option<&'static str> {
    if key.starts_with("FIN_DEV_BINANCE_") || key.starts_with("BINANCE_") {
        Some("binance")
    } else if key.starts_with("FIN_DEV_EVM_") || key == "ETH_WALLET_PK" {
        Some("evm")
    } else if key.starts_with("FIN_DEV_SOLANA_") || key == "SOLANA_PRIVATE_KEY" {
        Some("solana")
    } else {
        None
    }
}

fn provider_for(key: &str) -> Option<&'static str> {
    if key.starts_with("FIN_DEV_BINANCE_") {
        Some("binance")
    } else if key.starts_with("FIN_DEV_EVM_") {
        Some("evm")
    } else if key == "FIN_DEV_SOLANA_PRIVATE_KEY" {
        Some("solana")
    } else {
        None
    }
}

fn canonical_key(key: &str) -> Option<&str> {
    Some(match key {
        "BINANCE_MASTER_ACCOUNT_API_KEY" => "FIN_DEV_BINANCE_API_KEY",
        "BINANCE_MASTER_ACCOUNT_API_PEM" => "FIN_DEV_BINANCE_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_API_KEY" => "FIN_DEV_BINANCE_SUB_API_KEY",
        "BINANCE_SUB_ACCOUNT_API_PEM" => "FIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_EMAIL" => "FIN_DEV_BINANCE_SUB_EMAIL",
        "ETH_WALLET_PK" => "FIN_DEV_EVM_PRIVATE_KEY",
        "SOLANA_PRIVATE_KEY" => "FIN_DEV_SOLANA_PRIVATE_KEY",
        "FIN_DEV_BINANCE_API_KEY"
        | "FIN_DEV_BINANCE_SECRET"
        | "FIN_DEV_BINANCE_PRIVATE_KEY_PEM"
        | "FIN_DEV_BINANCE_UID"
        | "FIN_DEV_BINANCE_SUB_API_KEY"
        | "FIN_DEV_BINANCE_SUB_SECRET"
        | "FIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM"
        | "FIN_DEV_BINANCE_SUB_UID"
        | "FIN_DEV_BINANCE_SUB_EMAIL"
        | "FIN_DEV_EVM_PRIVATE_KEY"
        | "FIN_DEV_EVM_ADDRESS"
        | "FIN_DEV_EVM_SECONDARY_PRIVATE_KEY"
        | "FIN_DEV_EVM_SECONDARY_ADDRESS"
        | "FIN_DEV_SOLANA_PRIVATE_KEY" => key,
        _ => return None,
    })
}

fn parse_value(value: &str, pem: bool) -> Option<String> {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        if inner.contains(['\n', '\r', '$', '`']) {
            return None;
        }
        let decoded = inner.replace("\\\"", "\"").replace("\\\\", "\\");
        return decode_pem_newlines(decoded, pem);
    }
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        if inner.contains(['\n', '\r', '\'']) {
            return None;
        }
        return decode_pem_newlines(inner.to_owned(), pem);
    }
    if value.contains(['\t', '#', '$', '`', '\'', '"']) || (!pem && value.contains(' ')) {
        return None;
    }
    decode_pem_newlines(value.to_owned(), pem)
}

fn decode_pem_newlines(value: String, pem: bool) -> Option<String> {
    if !value.contains('\\') {
        return Some(value);
    }
    if !pem {
        return None;
    }
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.next()? != 'n' {
                return None;
            }
            decoded.push('\n');
        } else {
            decoded.push(ch);
        }
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_known_assignments_without_expansion() {
        let values =
            parse("# test\nFIN_DEV_BINANCE_API_KEY=abc\nFIN_DEV_BINANCE_SECRET='a=b'\n").unwrap();
        assert_eq!(values["FIN_DEV_BINANCE_API_KEY"].as_str(), "abc");
        assert_eq!(values["FIN_DEV_BINANCE_SECRET"].as_str(), "a=b");
        assert!(parse("FIN_DEV_BINANCE_API_KEY=$(touch /tmp/nope)").is_err());
        assert!(parse("PATH=/tmp").is_err());
        assert!(parse("FIN_DEV_BINANCE_API_KEY=x\nFIN_DEV_BINANCE_API_KEY=y").is_err());
        let aliases = parse("BINANCE_MASTER_ACCOUNT_API_KEY=master\nBINANCE_MASTER_ACCOUNT_API_PEM=-----BEGIN PRIVATE KEY-----\\nabc\\n-----END PRIVATE KEY-----\nETH_WALLET_PK=1111111111111111111111111111111111111111111111111111111111111111").unwrap();
        assert_eq!(aliases["FIN_DEV_BINANCE_API_KEY"].as_str(), "master");
        assert_eq!(
            aliases["FIN_DEV_BINANCE_PRIVATE_KEY_PEM"].as_str(),
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"
        );
        assert_eq!(
            aliases["FIN_DEV_EVM_PRIVATE_KEY"].as_str(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(parse("BINANCE_MASTER_ACCOUNT_API_KEY=x\nFIN_DEV_BINANCE_API_KEY=y").is_err());
        let email = parse("BINANCE_SUB_ACCOUNT_EMAIL=child@example.invalid").unwrap();
        assert_eq!(
            email["FIN_DEV_BINANCE_SUB_EMAIL"].as_str(),
            "child@example.invalid"
        );
        assert!(parse("BINANCE_SUB_ACCOUNT_EMAIL=child@example.invalid\nFIN_DEV_BINANCE_SUB_EMAIL=other@example.invalid").is_err());
    }

    #[test]
    fn errors_do_not_include_secret_values() {
        let error =
            parse("FIN_DEV_BINANCE_API_KEY=topsecret\nFIN_DEV_BINANCE_API_KEY=other").unwrap_err();
        assert!(!error.contains("topsecret"));
        assert!(!error.contains("other"));
    }

    #[test]
    fn selected_provider_skips_other_credential_values_before_decoding() {
        let values = parse_selected(
            "FIN_DEV_EVM_PRIVATE_KEY=$(never execute)\nFIN_DEV_BINANCE_API_KEY='malformed\nUNKNOWN_SECRET=do not inspect\nFIN_DEV_SOLANA_PRIVATE_KEY=abc\n",
            Some("solana"),
        )
        .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["FIN_DEV_SOLANA_PRIVATE_KEY"].as_str(), "abc");
        assert!(
            parse_selected(
                "SOLANA_PRIVATE_KEY=first\nFIN_DEV_SOLANA_PRIVATE_KEY=second\n",
                Some("solana"),
            )
            .is_err()
        );
        let ignored = parse_selected(
            "FIN_DEV_EVM_PRIVATE_KEY\nUNKNOWN_SECRET=broken'quote\nBINANCE_MASTER_ACCOUNT_API_KEY=a\nFIN_DEV_SOLANA_PRIVATE_KEY=key",
            Some("solana"),
        )
        .unwrap();
        assert_eq!(ignored.len(), 1);
        assert!(parse_selected("FIN_DEV_SOLANA_PRIVATE_KEY\n", Some("solana")).is_err());
        assert!(parse_selected("FIN_DEV_SOLANA_PRIVATE_KEY='broken\n", Some("solana")).is_err());
    }
}
