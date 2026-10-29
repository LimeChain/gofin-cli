//! Presentation of exact swap intent. Provider quote prose never supplies identities.
use super::{ApprovalAccount, ApprovalLeg, ApprovalReview};
use crate::{AssetId, EffectConstraint, GoFinError, PreparedOperation, Quantity, Result};

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let conversions: Vec<_> = prepared
        .intent
        .expected
        .iter()
        .filter_map(|effect| {
            if let EffectConstraint::Conversion {
                resource,
                input,
                maximum_input,
                output,
                minimum_output,
            } = effect
            {
                Some((resource, input, maximum_input, output, minimum_output))
            } else {
                None
            }
        })
        .collect();
    let [(resource, input, maximum_input, output, minimum_output)] = conversions.as_slice() else {
        return Err(GoFinError::validation(
            "A swap approval needs one exact input and minimum output conversion.",
        ));
    };
    let alias = prepared.input["wallet"]
        .as_str()
        .or_else(|| prepared.input["account"].as_str())
        .unwrap_or(&resource.0);
    let account = ApprovalAccount {
        name: alias.to_owned(),
        domain: Some(network_label(&input.network)),
    };
    review.input = Some(ApprovalLeg {
        amount: Some(maximum_input.to_decimal(input.decimals)),
        asset: Some(super::approval_asset(input)),
        account: Some(account.clone()),
    });
    review.output = Some(ApprovalLeg {
        amount: Some(minimum_output.to_decimal(output.decimals)),
        asset: Some(super::approval_asset(output)),
        account: Some(account),
    });
    if let Some(recipient) = prepared.binding["recipient"].as_str() {
        let source_address = prepared.binding["wallet_address"]
            .as_str()
            .or_else(|| {
                prepared
                    .binding
                    .pointer("/transaction/from")
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or_else(|| resource.0.rsplit(':').next().unwrap_or(&resource.0));
        if !recipient.eq_ignore_ascii_case(source_address) {
            review.output.as_mut().expect("conversion output").account = Some(ApprovalAccount {
                name: recipient.to_owned(),
                domain: Some(network_label(&output.network)),
            });
        }
    }
    review.service = match prepared.operation.as_str() {
        "uniswap.swap" | "swap.execute" if prepared.binding["provider"] == "uniswap_api_v1" => {
            match prepared
                .binding
                .pointer("/quote/routing")
                .and_then(serde_json::Value::as_str)
            {
                Some("CLASSIC") => "Uniswap",
                Some("DUTCH_V2" | "DUTCH_V3") => "UniswapX",
                _ => {
                    return Err(GoFinError::validation(
                        "Uniswap API review lacks its execution family.",
                    ));
                }
            }
        }
        "jupiter.swap" => "Jupiter",
        "meteora.swap" => "Meteora",
        "uniswap.swap" => "Uniswap",
        "pancakeswap.swap" => "PancakeSwap",
        "sushi.swap" => "Sushi",
        "ramses.swap" => "Ramses",
        "coinbase.convert" => "Coinbase",
        "swap.execute" => prepared.binding["dex"]
            .as_str()
            .filter(|dex| !dex.is_empty())
            .ok_or_else(|| GoFinError::validation("Swap review lacks its selected DEX."))?,
        _ => &review.service,
    }
    .to_owned();
    let mut clauses = Vec::new();
    if let Some(bps) = prepared.input["maximum_slippage_bps"]
        .as_u64()
        .or_else(|| prepared.input["slippage_bps"].as_u64())
        .or_else(|| prepared.binding["approved_slippage_bps"].as_u64())
    {
        clauses.push(format!(
            "Slippage ≤{}%",
            Quantity::from_u64(bps).to_decimal(2)
        ));
    }
    if let Some(bps) = prepared.input["maximum_platform_fee_bps"].as_u64() {
        clauses.push(format!(
            "platform fee ≤{}%",
            Quantity::from_u64(bps).to_decimal(2)
        ));
    }
    for cost in &prepared.intent.maximum_costs {
        clauses.push(format!(
            "fees ≤{} {}",
            cost.amount.atoms.to_decimal(cost.amount.asset.decimals),
            super::prepared_asset_label(prepared, &cost.amount.asset)
        ));
    }
    for effect in &prepared.intent.expected {
        match effect {
            EffectConstraint::AccountFundingBudget {
                asset,
                maximum_debit,
                ..
            } => {
                clauses.push(format!(
                    "account funding ≤{} {}",
                    maximum_debit.to_decimal(asset.decimals),
                    super::prepared_asset_label(prepared, asset)
                ));
            }
            EffectConstraint::OptionalAccountFunding { asset, .. } => {
                // Use the approved aggregate cap, not the provider's current rent estimate.
                let cap = prepared.binding["maximum_account_funding_lamports"]
                    .as_u64()
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "Swap review lacks its approved account funding cap.",
                        )
                    })?;
                clauses.push(format!(
                    "account funding ≤{} {}",
                    Quantity::from_u64(cap).to_decimal(asset.decimals),
                    super::prepared_asset_label(prepared, asset)
                ));
            }
            _ => {}
        }
    }
    if prepared
        .binding
        .get("permit_template")
        .is_some_and(|value| !value.is_null())
        || (prepared.binding["provider"] == "uniswap_api_v1"
            && prepared
                .binding
                .pointer("/quote/permit_data")
                .is_some_and(|value| !value.is_null()))
    {
        clauses.push("permit limited to this swap".into());
    }
    review.other_details = (!clauses.is_empty()).then(|| format!("{}.", clauses.join("; ")));
    Ok(())
}

pub(super) fn network_label(network: &str) -> String {
    match network {
        "solana:mainnet-beta" => "Solana",
        "ethereum:1" => "Ethereum",
        "ethereum:8453" => "Base",
        "ethereum:42161" => "Arbitrum",
        "ethereum:10" => "Optimism",
        "ethereum:56" => "BNB Chain",
        "binance" => "Binance",
        "coinbase" => "Coinbase",
        other => other,
    }
    .to_owned()
}

pub(super) fn asset_label(asset: &AssetId) -> String {
    // Match chain, address and scale together; symbols alone never establish identity.
    let reference = asset.reference.to_ascii_lowercase();
    let evm = match (asset.network.as_str(), reference.as_str(), asset.decimals) {
        ("ethereum:1", "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", 6)
        | ("ethereum:8453", "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", 6)
        | ("ethereum:42161", "0xaf88d065e77c8cc2239327c5edb3a432268e5831", 6) => Some("USDC"),
        ("ethereum:1", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", 18) => Some("WETH"),
        _ => None,
    };
    if let Some(label) = evm {
        return label.into();
    }
    let label = match (
        asset.network.as_str(),
        asset.reference.as_str(),
        asset.decimals,
    ) {
        ("solana:mainnet-beta", "native", 9) => "SOL",
        ("solana:mainnet-beta", "So11111111111111111111111111111111111111112", 9) => "WSOL",
        ("solana:mainnet-beta", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6) => "USDC",
        ("solana:mainnet-beta", "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", 6) => "PYUSD",
        ("solana:mainnet-beta", "BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy", 9) => "BP",
        ("solana:mainnet-beta", "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", 5) => "BONK",
        ("ethereum:1" | "ethereum:8453" | "ethereum:42161" | "ethereum:10", "native", 18) => "ETH",
        ("ethereum:56", "native", 18) => "BNB",
        _ => &asset.reference,
    };
    label.to_owned()
}
