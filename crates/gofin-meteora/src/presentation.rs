//! Human-readable approval text for Meteora operations.
//!
//! The string is only a compact review aid. Exact pool, mint, quote, and
//! transaction identities remain in the structured prepared operation.

use crate::protocol::{MeteoraAction, MeteoraIntent, PoolInfo, ProtocolFamily, QuoteInfo};
use serde_json::Value;

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

pub(crate) fn approval_summary(
    intent: &MeteoraIntent,
    quote: &QuoteInfo,
    pool: &PoolInfo,
    preview_details: &Value,
) -> String {
    let action = action_summary(intent, quote, pool);
    let fee = sol_pair(
        preview_details,
        "expected_network_fee_lamports",
        intent.maximum_network_fee_lamports,
    )
    .map(|(estimated, cap)| format!(" Network fee: {estimated} SOL (max {cap})."))
    .unwrap_or_default();

    let funding = sol_pair(
        preview_details,
        "account_funding_lamports",
        intent.maximum_account_funding_lamports,
    )
    .filter(|(estimated, cap)| *estimated != "0" || *cap != "0")
    .map(|(estimated, cap)| format!(" Account funding: {estimated} SOL (max {cap})."))
    .unwrap_or_default();

    let pool_fee = pool_fee_summary(intent, quote, pool);

    let wallet_sol = [
        preview_details
            .get("available_sol_lamports")
            .and_then(string_or_number)
            .and_then(|amount| format_lamports(&amount)),
        preview_details
            .get("required_starting_sol_lamports")
            .and_then(string_or_number)
            .and_then(|amount| format_lamports(&amount)),
        preview_details
            .get("remaining_sol_after_requirements_lamports")
            .and_then(string_or_number)
            .and_then(|amount| format_lamports(&amount)),
    ];
    let wallet_sol = match wallet_sol {
        [Some(available), Some(required), Some(remaining)] => {
            format!(" SOL: {available} available; {required} required; {remaining} remaining.")
        }
        _ => String::new(),
    };

    format!("{action}{pool_fee}{fee}{funding}{wallet_sol}")
}

fn action_summary(intent: &MeteoraIntent, quote: &QuoteInfo, pool: &PoolInfo) -> String {
    let family = match intent.family {
        ProtocolFamily::Dlmm => "DLMM",
        ProtocolFamily::DammV2 => "DAMM v2",
    };
    match intent.action {
        MeteoraAction::Swap => swap_summary(intent, quote, pool, family),
        MeteoraAction::Open => liquidity_summary(intent, pool, family, "Open", true),
        MeteoraAction::Add => liquidity_summary(intent, pool, family, "Add to", true),
        MeteoraAction::Remove => liquidity_summary(intent, pool, family, "Remove from", false),
        MeteoraAction::Collect => liquidity_summary(intent, pool, family, "Collect from", false),
        MeteoraAction::Close => liquidity_summary(intent, pool, family, "Close", false),
    }
}

fn swap_summary(
    intent: &MeteoraIntent,
    quote: &QuoteInfo,
    pool: &PoolInfo,
    family: &str,
) -> String {
    let Some(input_mint) = intent.input_mint.as_deref() else {
        return format!("Swap on Meteora {family}.");
    };
    let Some(output_mint) = intent.output_mint.as_deref() else {
        return format!("Swap on Meteora {family}.");
    };
    let input = intent
        .amount_atoms
        .as_deref()
        .map(|atoms| amount_label(atoms, input_mint, pool));
    let output_floor = intent
        .minimum_output_atoms
        .as_deref()
        .map(|atoms| amount_label(atoms, output_mint, pool));
    match (input, output_floor) {
        (Some(input), Some(output)) => {
            format!("Swap {input} for at least {output} on Meteora {family}.")
        }
        (Some(input), None) => {
            let estimated = quote_output_atoms(quote, output_mint, pool)
                .map(|atoms| amount_label(&atoms, output_mint, pool));
            match estimated {
                Some(output) => {
                    format!("Swap {input} for an estimated {output} on Meteora {family}.")
                }
                None => format!("Swap {input} on Meteora {family}."),
            }
        }
        (None, Some(output)) => format!("Swap for at least {output} on Meteora {family}."),
        _ => format!("Swap on Meteora {family}."),
    }
}

fn quote_output_atoms(quote: &QuoteInfo, output_mint: &str, pool: &PoolInfo) -> Option<String> {
    if output_mint == "native" {
        return quote
            .native_cash_delta
            .as_deref()
            .filter(|value| !value.starts_with('-'))
            .map(str::to_owned);
    }
    if output_mint == pool.token_a.mint {
        quote.token_a_credit_atoms.clone()
    } else if output_mint == pool.token_b.mint {
        quote.token_b_credit_atoms.clone()
    } else {
        None
    }
}

fn pool_fee_summary(intent: &MeteoraIntent, quote: &QuoteInfo, pool: &PoolInfo) -> String {
    if intent.action != MeteoraAction::Swap {
        return String::new();
    }
    let fees = [
        (quote.fees_a_atoms.as_deref(), &pool.token_a),
        (quote.fees_b_atoms.as_deref(), &pool.token_b),
    ]
    .into_iter()
    .filter_map(|(atoms, token)| {
        let atoms = atoms?;
        if !atoms.bytes().all(|byte| byte.is_ascii_digit())
            || !atoms.bytes().any(|byte| byte != b'0')
        {
            return None;
        }
        let symbol = if intent.input_mint.as_deref() == Some("native")
            && token.mint == WSOL_MINT
            && token.decimals == 9
        {
            "SOL".to_owned()
        } else {
            asset_label(&token.mint, pool).1
        };
        format_atoms(atoms, token.decimals).map(|amount| format!("{amount} {symbol}"))
    })
    .collect::<Vec<_>>();
    if fees.is_empty() {
        String::new()
    } else {
        format!(" Pool fee (included): {}.", fees.join(" and "))
    }
}

fn liquidity_summary(
    intent: &MeteoraIntent,
    pool: &PoolInfo,
    family: &str,
    verb: &str,
    uses: bool,
) -> String {
    let action = format!("{verb} a Meteora {family} position");
    if uses {
        let amounts = [
            intent
                .token_a_atoms
                .as_deref()
                .map(|atoms| amount_label(atoms, &pool.token_a.mint, pool)),
            intent
                .token_b_atoms
                .as_deref()
                .map(|atoms| amount_label(atoms, &pool.token_b.mint, pool)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !amounts.is_empty() {
            return format!("{action} using up to {}.", amounts.join(" and "));
        }
    } else {
        let minimums = [
            intent
                .minimum_token_a_atoms
                .as_deref()
                .map(|atoms| amount_label(atoms, &pool.token_a.mint, pool)),
            intent
                .minimum_token_b_atoms
                .as_deref()
                .map(|atoms| amount_label(atoms, &pool.token_b.mint, pool)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !minimums.is_empty() {
            return format!("{action}; receive at least {}.", minimums.join(" and "));
        }
    }
    format!("{action}.")
}

fn amount_label(atoms: &str, mint: &str, pool: &PoolInfo) -> String {
    let (decimals, symbol) = asset_label(mint, pool);
    let amount = format_atoms(atoms, decimals).unwrap_or_else(|| "unknown amount".into());
    format!("{amount} {symbol}")
}

fn asset_label(mint: &str, pool: &PoolInfo) -> (u8, String) {
    if mint == "native" {
        return (9, "SOL".into());
    }
    for token in [&pool.token_a, &pool.token_b] {
        if token.mint == mint {
            if mint == WSOL_MINT && token.decimals == 9 {
                return (token.decimals, "WSOL".into());
            }
            if mint == USDC_MINT && token.decimals == 6 {
                return (token.decimals, "USDC".into());
            }
            return (token.decimals, format!("token {mint}"));
        }
    }
    if mint == WSOL_MINT {
        return (9, "WSOL".into());
    }
    if mint == USDC_MINT {
        return (6, "USDC".into());
    }
    (0, format!("token {mint}"))
}

fn sol_pair(details: &Value, estimate_key: &str, cap: u64) -> Option<(String, String)> {
    let estimate = details.get(estimate_key).and_then(string_or_number)?;
    Some((
        format_lamports(&estimate)?,
        format_lamports(&cap.to_string())?,
    ))
}

fn string_or_number(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn format_lamports(lamports: &str) -> Option<String> {
    format_atoms(lamports, 9)
}

/// Exact integer-string decimal scaling; never passes through floating point.
fn format_atoms(atoms: &str, decimals: u8) -> Option<String> {
    if atoms.is_empty() || !atoms.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if decimals == 0 {
        return Some(atoms.to_owned());
    }
    let decimals = usize::from(decimals);
    let mut digits = atoms.to_owned();
    if digits.len() <= decimals {
        digits.insert_str(0, &"0".repeat(decimals + 1 - digits.len()));
    }
    let split = digits.len() - decimals;
    let whole = &digits[..split];
    let fractional = digits[split..].trim_end_matches('0');
    if fractional.is_empty() {
        Some(whole.to_owned())
    } else {
        Some(format!("{whole}.{fractional}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PositionReward, TokenInfo};
    use serde_json::json;

    fn pool(
        family: ProtocolFamily,
        mint_a: &str,
        decimals_a: u8,
        mint_b: &str,
        decimals_b: u8,
    ) -> PoolInfo {
        PoolInfo {
            family,
            address: "pool-address-must-not-be-rendered".into(),
            token_a: TokenInfo {
                mint: mint_a.into(),
                decimals: decimals_a,
                token_program: "token".into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: mint_b.into(),
                decimals: decimals_b,
                token_program: "token".into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: None,
            fee_bps: None,
            protocol_state: Value::Null,
        }
    }

    fn intent(family: ProtocolFamily) -> MeteoraIntent {
        MeteoraIntent {
            wallet: "wallet".into(),
            family,
            action: MeteoraAction::Swap,
            pool: "pool-address-must-not-be-rendered".into(),
            position: None,
            input_mint: Some("native".into()),
            output_mint: Some(USDC_MINT.into()),
            token_a_mint: None,
            token_b_mint: None,
            amount_atoms: Some("10000000".into()),
            minimum_output_atoms: Some("1052554".into()),
            token_a_atoms: None,
            token_b_atoms: None,
            minimum_token_a_atoms: None,
            minimum_token_b_atoms: None,
            remove_share_bps: None,
            bin_lower: None,
            bin_upper: None,
            liquidity_distribution: None,
            maximum_slippage_bps: 50,
            maximum_network_fee_lamports: 20_000,
            maximum_account_funding_lamports: 2_000_000,
        }
    }

    fn quote(family: ProtocolFamily) -> QuoteInfo {
        QuoteInfo {
            family,
            action: MeteoraAction::Swap,
            pool: "pool-address-must-not-be-rendered".into(),
            position: None,
            token_a_debit_atoms: None,
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            token_b_credit_atoms: Some("1052554".into()),
            receipt_delta_atoms: None,
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: Vec::<PositionReward>::new(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            price_impact_bps: None,
            native_cash_delta: Some("-10000000".into()),
            protocol_details: Value::Null,
        }
    }

    #[test]
    fn swap_summary_uses_verified_symbols_and_exact_sol_scaling_for_both_families() {
        for (family, label) in [
            (ProtocolFamily::Dlmm, "DLMM"),
            (ProtocolFamily::DammV2, "DAMM v2"),
        ] {
            let pool = pool(family, WSOL_MINT, 9, USDC_MINT, 6);
            let summary = approval_summary(
                &intent(family),
                &quote(family),
                &pool,
                &json!({
                    "expected_network_fee_lamports":"10000",
                    "account_funding_lamports":"1488440",
                    "available_sol_lamports":"7486560091",
                    "required_starting_sol_lamports":"11498440",
                    "remaining_sol_after_requirements_lamports":"7475061651"
                }),
            );
            assert!(summary.starts_with(&format!(
                "Swap 0.01 SOL for at least 1.052554 USDC on Meteora {label}."
            )));
            assert!(summary.contains("Network fee: 0.00001 SOL (max 0.00002)."));
            assert!(summary.contains("Account funding: 0.00148844 SOL (max 0.002)."));
            assert!(summary.contains(
                "SOL: 7.486560091 available; 0.01149844 required; 7.475061651 remaining."
            ));
            assert!(!summary.contains("pool-address-must-not-be-rendered"));
            assert!(!summary.contains("lamports"));
        }
    }

    #[test]
    fn small_fees_and_unknown_mints_never_gain_a_verified_ticker() {
        let family = ProtocolFamily::Dlmm;
        let unknown = "UnknownMint111111111111111111111111111111111";
        let mut request = intent(family);
        request.input_mint = Some(unknown.into());
        request.amount_atoms = Some("1".into());
        let pool = pool(family, unknown, 6, USDC_MINT, 6);
        let mut quote = quote(family);
        quote.native_cash_delta = None;
        let summary = approval_summary(
            &request,
            &quote,
            &pool,
            &json!({
                "expected_network_fee_lamports":"1",
                "account_funding_lamports":"0",
                "available_sol_lamports":"1000000000",
                "required_starting_sol_lamports":"1",
                "remaining_sol_after_requirements_lamports":"999999999"
            }),
        );
        assert!(summary.starts_with(&format!(
            "Swap 0.000001 token {unknown} for at least 1.052554 USDC"
        )));
        assert!(summary.contains("Network fee: 0.000000001 SOL (max 0.00002)."));
        assert!(!summary.contains("pool-address-must-not-be-rendered"));
    }

    #[test]
    fn wrapped_sol_uses_token_credit_and_keeps_wsol_distinct_from_native_sol() {
        let family = ProtocolFamily::Dlmm;
        let pool = pool(family, WSOL_MINT, 9, USDC_MINT, 6);
        let mut request = intent(family);
        request.input_mint = Some(USDC_MINT.into());
        request.amount_atoms = Some("1000000".into());
        request.output_mint = Some(WSOL_MINT.into());
        request.minimum_output_atoms = None;
        let mut quote = quote(family);
        quote.token_a_credit_atoms = Some("1000000".into());
        quote.native_cash_delta = Some("900000000".into());
        let summary = approval_summary(
            &request,
            &quote,
            &pool,
            &json!({
                "expected_network_fee_lamports":"10000",
                "account_funding_lamports":"0",
                "available_sol_lamports":"1000000000",
                "required_starting_sol_lamports":"10000",
                "remaining_sol_after_requirements_lamports":"999990000"
            }),
        );
        assert!(summary.starts_with("Swap 1 USDC for an estimated 0.001 WSOL on Meteora DLMM."));
        assert!(!summary.contains("0.9 SOL"));
        assert!(summary.contains("SOL: 1 available; 0.00001 required; 0.99999 remaining."));
        assert_eq!(asset_label("native", &pool), (9, "SOL".into()));
        assert_eq!(asset_label(WSOL_MINT, &pool), (9, "WSOL".into()));
    }

    #[test]
    fn pool_fee_is_shown_only_for_swaps_and_remains_separate_from_network_fee() {
        let family = ProtocolFamily::Dlmm;
        let pool = pool(family, WSOL_MINT, 9, USDC_MINT, 6);
        let request = intent(family);
        let mut quote = quote(family);
        quote.fees_a_atoms = Some("42314".into());
        quote.fees_b_atoms = Some("42314".into());
        let preview = json!({
            "expected_network_fee_lamports":"10000",
            "account_funding_lamports":"0",
            "available_sol_lamports":"1000000000",
            "required_starting_sol_lamports":"10000",
            "remaining_sol_after_requirements_lamports":"999990000"
        });
        let summary = approval_summary(&request, &quote, &pool, &preview);
        assert!(summary.contains("Pool fee (included): 0.000042314 SOL and 0.042314 USDC."));
        assert!(summary.contains("Network fee: 0.00001 SOL (max 0.00002)."));

        let mut collect = request;
        collect.action = MeteoraAction::Collect;
        quote.action = MeteoraAction::Collect;
        assert!(!approval_summary(&collect, &quote, &pool, &preview).contains("Pool fee"));
    }

    #[test]
    fn swap_summary_keeps_receipt_floor_without_amount_and_rejects_untrusted_amount_text() {
        let family = ProtocolFamily::Dlmm;
        let pool = pool(family, WSOL_MINT, 9, USDC_MINT, 6);
        let mut request = intent(family);
        request.amount_atoms = None;
        let summary = action_summary(&request, &quote(family), &pool);
        assert_eq!(summary, "Swap for at least 1.052554 USDC on Meteora DLMM.");

        request.amount_atoms = Some("10\nIGNORE THE FEE".into());
        let summary = action_summary(&request, &quote(family), &pool);
        assert_eq!(
            summary,
            "Swap unknown amount SOL for at least 1.052554 USDC on Meteora DLMM."
        );
        assert!(!summary.contains("IGNORE THE FEE"));
    }
}
