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
        // Existing disposable credential exports remain importable after the
        // product rename; normalize keys before provider selection or decoding.
        let renamed_key = raw_key
            .strip_prefix("GOFIN_DEV_")
            .map(|suffix| format!("GOFIN_DEV_{suffix}"));
        let raw_key = renamed_key.as_deref().unwrap_or(raw_key);
        if provider
            .is_some_and(|selected| provider_hint(raw_key).is_some_and(|hint| hint != selected))
        {
            continue;
        }
        let Some(key) = canonical_key(raw_key) else {
            // Credential exports can contain variables for unrelated tools or
            // connectors. Import only explicitly supported GoFin keys; selected
            // provider parsing still avoids decoding unrelated credential values.
            continue;
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
    if let Some(suffix) = key.strip_prefix("GOFIN_DEV_") {
        return provider_hint(&format!("GOFIN_DEV_{suffix}"));
    }
    if key.starts_with("GOFIN_DEV_BINANCE_") || key.starts_with("BINANCE_") {
        Some("binance")
    } else if key.starts_with("GOFIN_DEV_EVM_") || key == "ETH_WALLET_PK" {
        Some("evm")
    } else if key.starts_with("GOFIN_DEV_SOLANA_") || key == "SOLANA_PRIVATE_KEY" {
        Some("solana")
    } else if key.starts_with("GOFIN_DEV_RELAY_") {
        Some("relay")
    } else {
        None
    }
}

fn provider_for(key: &str) -> Option<&'static str> {
    if key.starts_with("GOFIN_DEV_BINANCE_") {
        Some("binance")
    } else if key.starts_with("GOFIN_DEV_EVM_") {
        Some("evm")
    } else if key == "GOFIN_DEV_SOLANA_PRIVATE_KEY" {
        Some("solana")
    } else if key == "GOFIN_DEV_RELAY_API_KEY" {
        Some("relay")
    } else {
        None
    }
}

fn canonical_key(key: &str) -> Option<&str> {
    for prefix in ["GOFIN_DEV_KRAKEN_", "GOFIN_DEV_KRAKENFUTURES_"] {
        if let Some(rest) = key.strip_prefix(prefix)
            && let Some((index, field)) = rest.split_once('_')
            && matches!(index, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8")
            && (matches!(field, "API_KEY" | "SECRET" | "ACCOUNT_ID" | "NAME")
                || (prefix == "GOFIN_DEV_KRAKEN_" && field == "DEFAULT_WALLET_ACCOUNT_ID"))
        {
            return Some(key);
        }
    }
    Some(match key {
        "BINANCE_MASTER_ACCOUNT_API_KEY" => "GOFIN_DEV_BINANCE_API_KEY",
        "BINANCE_MASTER_ACCOUNT_API_PEM" => "GOFIN_DEV_BINANCE_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_API_KEY" => "GOFIN_DEV_BINANCE_SUB_API_KEY",
        "BINANCE_SUB_ACCOUNT_API_PEM" => "GOFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_EMAIL" => "GOFIN_DEV_BINANCE_SUB_EMAIL",
        "ETH_WALLET_PK" => "GOFIN_DEV_EVM_PRIVATE_KEY",
        "SOLANA_PRIVATE_KEY" => "GOFIN_DEV_SOLANA_PRIVATE_KEY",
        "GOFIN_DEV_KRAKEN_API_KEY"
        | "GOFIN_DEV_KRAKEN_SECRET"
        | "GOFIN_DEV_KRAKEN_ACCOUNT_ID"
        | "GOFIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID"
        | "GOFIN_DEV_KRAKENFUTURES_API_KEY"
        | "GOFIN_DEV_KRAKENFUTURES_SECRET"
        | "GOFIN_DEV_KRAKENFUTURES_ACCOUNT_ID"
        | "GOFIN_DEV_BINANCE_API_KEY"
        | "GOFIN_DEV_BINANCE_SECRET"
        | "GOFIN_DEV_BINANCE_PRIVATE_KEY_PEM"
        | "GOFIN_DEV_BINANCE_UID"
        | "GOFIN_DEV_BINANCE_SUB_API_KEY"
        | "GOFIN_DEV_BINANCE_SUB_SECRET"
        | "GOFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM"
        | "GOFIN_DEV_BINANCE_SUB_UID"
        | "GOFIN_DEV_BINANCE_SUB_EMAIL"
        | "GOFIN_DEV_EVM_PRIVATE_KEY"
        | "GOFIN_DEV_EVM_ADDRESS"
        | "GOFIN_DEV_EVM_SECONDARY_PRIVATE_KEY"
        | "GOFIN_DEV_EVM_SECONDARY_ADDRESS"
        | "GOFIN_DEV_SOLANA_PRIVATE_KEY"
        | "GOFIN_DEV_RELAY_API_KEY" => key,
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
    fn legacy_gofin_exports_remain_importable_without_duplicate_aliases() {
        let values = parse(
            "GOFIN_DEV_KRAKEN_1_API_KEY=synthetic-key\nGOFIN_DEV_KRAKEN_1_SECRET=synthetic-secret\n",
        )
        .unwrap();
        assert_eq!(
            values["GOFIN_DEV_KRAKEN_1_API_KEY"].as_str(),
            "synthetic-key"
        );
        assert_eq!(
            values["GOFIN_DEV_KRAKEN_1_SECRET"].as_str(),
            "synthetic-secret"
        );
        assert!(
            parse("GOFIN_DEV_SOLANA_PRIVATE_KEY=first\nGOFIN_DEV_SOLANA_PRIVATE_KEY=second\n")
                .is_err()
        );
        let selected = parse_selected(
            "GOFIN_DEV_BINANCE_SECRET='broken\nGOFIN_DEV_SOLANA_PRIVATE_KEY=synthetic-key\n",
            Some("solana"),
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected["GOFIN_DEV_SOLANA_PRIVATE_KEY"].as_str(),
            "synthetic-key"
        );
        assert!(parse_selected("GOFIN_DEV_SOLANA_PRIVATE_KEY\n", Some("solana")).is_err());
    }

    #[test]
    fn parses_only_known_assignments_without_expansion() {
        let values =
            parse("# test\nGOFIN_DEV_BINANCE_API_KEY=abc\nGOFIN_DEV_BINANCE_SECRET='a=b'\n")
                .unwrap();
        assert_eq!(values["GOFIN_DEV_BINANCE_API_KEY"].as_str(), "abc");
        assert_eq!(values["GOFIN_DEV_BINANCE_SECRET"].as_str(), "a=b");
        assert!(parse("GOFIN_DEV_BINANCE_API_KEY=$(touch /tmp/nope)").is_err());
        assert!(
            parse("GOFIN_DEV_BINANCE_API_KEY=abc\nPATH=/tmp\nOTHER_TOOL_TOKEN=opaque value #1\n")
                .is_ok()
        );
        assert!(parse("GOFIN_DEV_BINANCE_API_KEY=x\nGOFIN_DEV_BINANCE_API_KEY=y").is_err());
        let aliases = parse("BINANCE_MASTER_ACCOUNT_API_KEY=master\nBINANCE_MASTER_ACCOUNT_API_PEM=-----BEGIN PRIVATE KEY-----\\nabc\\n-----END PRIVATE KEY-----\nETH_WALLET_PK=1111111111111111111111111111111111111111111111111111111111111111").unwrap();
        assert_eq!(aliases["GOFIN_DEV_BINANCE_API_KEY"].as_str(), "master");
        assert_eq!(
            aliases["GOFIN_DEV_BINANCE_PRIVATE_KEY_PEM"].as_str(),
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"
        );
        assert_eq!(
            aliases["GOFIN_DEV_EVM_PRIVATE_KEY"].as_str(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(parse("BINANCE_MASTER_ACCOUNT_API_KEY=x\nGOFIN_DEV_BINANCE_API_KEY=y").is_err());
        let email = parse("BINANCE_SUB_ACCOUNT_EMAIL=child@example.invalid").unwrap();
        assert_eq!(
            email["GOFIN_DEV_BINANCE_SUB_EMAIL"].as_str(),
            "child@example.invalid"
        );
        assert!(parse("BINANCE_SUB_ACCOUNT_EMAIL=child@example.invalid\nGOFIN_DEV_BINANCE_SUB_EMAIL=other@example.invalid").is_err());
    }

    #[test]
    fn indexed_kraken_groups_are_closed_and_bounded() {
        for product in ["KRAKEN", "KRAKENFUTURES"] {
            for index in 1..=8 {
                assert!(parse(&format!("GOFIN_DEV_{product}_{index}_API_KEY=synthetic-only\nGOFIN_DEV_{product}_{index}_NAME=fixture-name")).is_ok());
            }
            for index in ["0", "9", "01", "999"] {
                assert!(
                    parse(&format!(
                        "GOFIN_DEV_{product}_{index}_API_KEY=synthetic-only"
                    ))
                    .is_err()
                );
            }
            assert!(parse(&format!("GOFIN_DEV_{product}_1_URL=unsupported")).is_err());
        }
        assert!(parse("GOFIN_DEV_KRAKENFUTURES_1_DEFAULT_WALLET_ACCOUNT_ID=WDEFAULT").is_err());
        assert!(parse("GOFIN_DEV_KRAKEN_1_DEFAULT_WALLET_ACCOUNT_ID=WDEFAULT").is_ok());
    }
    #[test]
    fn errors_do_not_include_secret_values() {
        let error = parse("GOFIN_DEV_BINANCE_API_KEY=topsecret\nGOFIN_DEV_BINANCE_API_KEY=other")
            .unwrap_err();
        assert!(!error.contains("topsecret"));
        assert!(!error.contains("other"));
    }

    #[test]
    fn selected_provider_skips_other_credential_values_before_decoding() {
        let values = parse_selected(
            "GOFIN_DEV_EVM_PRIVATE_KEY=$(never execute)\nGOFIN_DEV_BINANCE_API_KEY='malformed\nUNKNOWN_SECRET=do not inspect\nGOFIN_DEV_SOLANA_PRIVATE_KEY=abc\n",
            Some("solana"),
        ).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["GOFIN_DEV_SOLANA_PRIVATE_KEY"].as_str(), "abc");
        assert!(
            parse_selected(
                "SOLANA_PRIVATE_KEY=first\nGOFIN_DEV_SOLANA_PRIVATE_KEY=second\n",
                Some("solana")
            )
            .is_err()
        );
        let ignored = parse_selected("GOFIN_DEV_EVM_PRIVATE_KEY\nUNKNOWN_SECRET=broken'quote\nBINANCE_MASTER_ACCOUNT_API_KEY=a\nGOFIN_DEV_SOLANA_PRIVATE_KEY=key", Some("solana")).unwrap();
        assert_eq!(ignored.len(), 1);
        assert!(parse_selected("GOFIN_DEV_SOLANA_PRIVATE_KEY\n", Some("solana")).is_err());
        assert!(parse_selected("GOFIN_DEV_SOLANA_PRIVATE_KEY='broken\n", Some("solana")).is_err());

        let relay = parse_selected(
            "GOFIN_DEV_EVM_PRIVATE_KEY=$(ignored)\nGOFIN_DEV_RELAY_API_KEY=SYNTHETIC-KEY\n",
            Some("relay"),
        )
        .unwrap();
        assert_eq!(relay.len(), 1);
        assert_eq!(relay["GOFIN_DEV_RELAY_API_KEY"].as_str(), "SYNTHETIC-KEY");
    }

    #[test]
    fn ignores_unrelated_export_fields_without_importing_them() {
        let values = parse("GOFIN_DEV_EVM_PRIVATE_KEY=synthetic\nKRAKEN_MASTER_API_KEY=ignored\nKRAKEN_PRIVATE_KEY=ignored-too\nKRAKEN_ACCOUNT_ID=account\nNEW_TOOL_TOKEN=opaque value\nPATH=/tmp/custom\n").unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["GOFIN_DEV_EVM_PRIVATE_KEY"].as_str(), "synthetic");
        assert!(!values.contains_key("KRAKEN_MASTER_API_KEY"));
        assert!(!values.contains_key("NEW_TOOL_TOKEN"));
        assert!(!values.contains_key("PATH"));
        assert!(parse("KRAKEN_MASTER_API_KEY=value").is_err());
        assert!(parse("NEW_TOOL_TOKEN=value").is_err());
    }
}
