use std::collections::BTreeMap;
use zeroize::Zeroizing;

const MAX_ENV_FILE: usize = 32 * 1024;

/// Parse the intentionally small .env subset accepted by the development broker.
/// No interpolation, command execution, `export`, or arbitrary process settings.
pub fn parse(source: &str) -> Result<BTreeMap<String, Zeroizing<String>>, String> {
    if source.len() > MAX_ENV_FILE {
        return Err("credential file exceeds the supported size".into());
    }
    let mut values = BTreeMap::new();
    for (line_number, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, raw_value) = line.split_once('=').ok_or_else(|| {
            format!(
                "credential file has an invalid assignment on line {}",
                line_number + 1
            )
        })?;
        let raw_key = key.trim();
        let Some(key) = canonical_key(raw_key) else {
            // Credential exports can contain variables for unrelated tools or
            // connectors. Import only the explicitly supported Fin keys and
            // ignore everything else without parsing or retaining its value.
            continue;
        };
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

fn canonical_key(key: &str) -> Option<&str> {
    Some(match key {
        "BINANCE_MASTER_ACCOUNT_API_KEY" => "FIN_DEV_BINANCE_API_KEY",
        "BINANCE_MASTER_ACCOUNT_API_PEM" => "FIN_DEV_BINANCE_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_API_KEY" => "FIN_DEV_BINANCE_SUB_API_KEY",
        "BINANCE_SUB_ACCOUNT_API_PEM" => "FIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM",
        "BINANCE_SUB_ACCOUNT_EMAIL" => "FIN_DEV_BINANCE_SUB_EMAIL",
        "ETH_WALLET_PK" => "FIN_DEV_EVM_PRIVATE_KEY",
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
        | "FIN_DEV_EVM_SECONDARY_ADDRESS" => key,
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
        assert!(
            parse("FIN_DEV_BINANCE_API_KEY=abc\nPATH=/tmp\nOTHER_TOOL_TOKEN=opaque value #1\n")
                .is_ok()
        );
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
    fn ignores_unrelated_export_fields_without_importing_them() {
        let values = parse(
            "FIN_DEV_EVM_PRIVATE_KEY=synthetic\nKRAKEN_MASTER_API_KEY=ignored\nKRAKEN_PRIVATE_KEY=ignored-too\nKRAKEN_ACCOUNT_ID=account\nNEW_TOOL_TOKEN=opaque value\nPATH=/tmp/custom\n",
        )
        .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values["FIN_DEV_EVM_PRIVATE_KEY"].as_str(), "synthetic");
        assert!(!values.contains_key("KRAKEN_MASTER_API_KEY"));
        assert!(!values.contains_key("NEW_TOOL_TOKEN"));
        assert!(!values.contains_key("PATH"));
        assert!(parse("KRAKEN_MASTER_API_KEY=value").is_err());
        assert!(parse("NEW_TOOL_TOKEN=value").is_err());
    }
}
