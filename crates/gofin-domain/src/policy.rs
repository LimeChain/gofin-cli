//! Exact USD policy values (zero means no personal ceiling).
use crate::{GoFinError, Result};
pub const DEFAULT_POLICY_LIMIT_USD_MICROS: u64 = 100_000_000;
pub const MOVEMENT_INCLUDES: &str = "Gross movement plus maximum execution costs.";
pub const ALLOCATION_INCLUDES: &str = "Installation-wide rolling 24-hour gross movement plus maximum execution costs and additional borrowed allocation across all accounts and commands. Submitted operations count for at least 24 hours after settlement; unresolved submissions keep their allocation until reconciled. No netting of sales, credits or returns.";

pub fn limit(configured: &str) -> Result<Option<u64>> {
    let value = policy_micros(configured)?;
    Ok((value != 0).then_some(value))
}

fn invalid() -> GoFinError {
    GoFinError::new(
        "INVALID_POLICY_AMOUNT",
        "configuration",
        "USD limits must be nonnegative exact decimals with at most six fractional digits and fit in unsigned 64-bit USD micros.",
        "Use decimal dollars such as 1000 or 1000.25; 0 means no personal limit; no signs, exponent, separators or unlimited text.",
    )
}

pub fn policy_micros(value: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    value.parse::<u64>().ok().ok_or_else(invalid)
}

pub fn parse_usd(value: &str) -> Result<String> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || whole.len() > 14
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let whole = whole.parse::<u64>().map_err(|_| invalid())?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().map_err(|_| invalid())? * 10u64.pow(6 - fraction.len() as u32)
    };
    whole
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(fractional))
        .map(|v| v.to_string())
        .ok_or_else(invalid)
}

pub fn format_usd(micros: &str) -> Result<String> {
    let value = policy_micros(micros)?;
    let whole = value / 1_000_000;
    let fraction = value % 1_000_000;
    if fraction == 0 {
        Ok(whole.to_string())
    } else {
        Ok(format!(
            "{whole}.{}",
            format!("{fraction:06}").trim_end_matches('0')
        ))
    }
}

/// Human display must distinguish zero's sentinel meaning from zero spending authority.
pub fn format_limit(micros: &str) -> Result<String> {
    let dollars = format_usd(micros)?;
    Ok(if dollars == "0" {
        "No limit".into()
    } else {
        format!("${dollars} USD")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_positive_finite_usd() {
        for (input, micros) in [
            ("0", "0"),
            ("0.000000", "0"),
            ("1000", "1000000000"),
            ("0.000001", "1"),
            ("001.250000", "1250000"),
            ("18446744073709.551615", "18446744073709551615"),
        ] {
            assert_eq!(parse_usd(input).unwrap(), micros);
            assert_eq!(parse_usd(&format_usd(micros).unwrap()).unwrap(), micros);
        }
        for input in [
            "",
            "-1",
            "+1",
            "1e3",
            "NaN",
            "inf",
            "unlimited",
            " 1",
            "1 ",
            ".1",
            "1.",
            "1.0000001",
            "1,000",
            "1.2.3",
            "18446744073709.551616",
            "9999999999999999999999999",
        ] {
            assert!(parse_usd(input).is_err(), "{input}");
        }
    }
    #[test]
    fn finite_and_zero_personal_limits() {
        assert_eq!(limit("5000000000").unwrap(), Some(5_000_000_000));
        assert_eq!(limit("0").unwrap(), None);
        assert_eq!(format_limit("0").unwrap(), "No limit");
    }
}
