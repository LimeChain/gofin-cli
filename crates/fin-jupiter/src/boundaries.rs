//! Finite, protocol-independent bounds for Jupiter transaction candidates.

use fin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, FinError, ResourceAmount,
    ResourceId, Result, boundaries::BoundarySpec,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    NETWORK,
    swap::{self, SwapIntent, SwapRoute},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwapBounds {
    pub maximum_input_atoms: String,
    pub minimum_output_per_input_numerator: String,
    pub minimum_output_per_input_denominator: String,
    pub maximum_slippage_bps: u16,
    pub maximum_platform_fee_bps: u16,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
    pub maximum_quote_lifetime_seconds: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TriggerBounds {
    pub maximum_making_atoms: String,
    pub minimum_taking_per_making_numerator: String,
    pub minimum_taking_per_making_denominator: String,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
    pub maximum_order_lifetime_seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TriggerCancelBounds {
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
    #[serde(default)]
    pub recovery_parent_step_id: Option<String>,
}

pub(crate) fn format_atomic_quantity(atoms: &str, decimals: u8) -> String {
    let digits = atoms.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return digits.to_owned();
    }
    let padded;
    let digits = if digits.len() <= decimals {
        padded = format!("{}{}", "0".repeat(decimals + 1 - digits.len()), digits);
        padded.as_str()
    } else {
        digits
    };
    let split = digits.len() - decimals;
    let whole = &digits[..split];
    let fraction = digits[split..].trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}

fn format_sol_lamports(lamports: u64) -> String {
    format!("{} SOL", format_atomic_quantity(&lamports.to_string(), 9))
}

pub(crate) fn trigger_cancel_spec(
    wallet_name: &str,
    wallet_address: &str,
    order_key: &str,
    parent_operation_id: &str,
    parent_submission: &fin_domain::Submission,
    resource: &ResourceId,
    bounds_value: &Value,
) -> Result<BoundarySpec> {
    let bounds: TriggerCancelBounds = serde_json::from_value(bounds_value.clone())
        .map_err(|_| FinError::validation("Invalid Trigger cancellation boundary bounds."))?;
    if bounds.maximum_network_fee_lamports == 0
        || order_key.trim().is_empty()
        || parent_operation_id.trim().is_empty()
        || parent_submission.kind.trim().is_empty()
        || parent_submission.reference.trim().is_empty()
    {
        return Err(FinError::validation(
            "Trigger cancellation boundaries require an exact parent, order and positive fee cap.",
        ));
    }
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let mut maximum_debits = Vec::new();
    if bounds.maximum_account_funding_lamports > 0 {
        maximum_debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: fin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        });
    }
    Ok(BoundarySpec {
        fixed: json!({
            "operation":"jupiter.limit.cancel",
            "wallet_name":wallet_name,
            "wallet_address":wallet_address,
            "network":"solana",
            "cluster":"mainnet-beta",
            "order_key":order_key,
            "parent_operation_id":parent_operation_id,
            "parent_submission":parent_submission,
            "program":"j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X",
        }),
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| FinError::validation("Trigger cancellation bounds are invalid."))?,
        maximum_debits,
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: native,
                atoms: fin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Cancel exact Trigger order {order_key} for wallet {wallet_address}, bound to parent operation {parent_operation_id}; network fee at most {} and refundable account funding at most {}.",
            format_sol_lamports(bounds.maximum_network_fee_lamports),
            format_sol_lamports(bounds.maximum_account_funding_lamports),
        ),
    })
}

// These functions compare each immutable authorization dimension explicitly;
// keeping the scalars visible avoids accidentally dropping one from a shared
// request bundle during candidate validation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_trigger_cancel_candidate(
    spec: &BoundarySpec,
    wallet_name: &str,
    wallet_address: &str,
    order_key: &str,
    parent_operation_id: &str,
    parent_submission: &fin_domain::Submission,
    fee_lamports: u64,
    funding_lamports: u64,
) -> Result<()> {
    let bounds: TriggerCancelBounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| FinError::validation("Approved Trigger cancellation boundary is invalid."))?;
    if spec.fixed["operation"] != "jupiter.limit.cancel"
        || spec.fixed["wallet_name"] != wallet_name
        || spec.fixed["wallet_address"] != wallet_address
        || (bounds.recovery_parent_step_id.is_none() && spec.fixed["order_key"] != order_key)
        || (bounds.recovery_parent_step_id.is_some()
            && (spec.fixed["order_key"] != "__fin_plan_recovery_order_key__"
                || order_key.trim().is_empty()))
        || (bounds.recovery_parent_step_id.is_none()
            && (spec.fixed["parent_operation_id"] != parent_operation_id
                || spec.fixed["parent_submission"] != json!(parent_submission)))
        || (bounds.recovery_parent_step_id.is_some()
            && (spec.fixed["parent_operation_id"] != "__fin_plan_recovery_parent__"
                || spec.fixed["parent_submission"]["data"]["recovery_parent"] != true))
        || spec.fixed["network"] != "solana"
        || spec.fixed["cluster"] != "mainnet-beta"
        || spec.fixed["program"] != "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X"
        || fee_lamports > bounds.maximum_network_fee_lamports
        || funding_lamports > bounds.maximum_account_funding_lamports
    {
        return Err(violation(
            "exact_order_parent_or_cost_caps",
            &spec.fixed,
            &json!({"wallet":wallet_address,"order_key":order_key,"parent_operation_id":parent_operation_id,"fee_lamports":fee_lamports,"account_funding_lamports":funding_lamports}),
        ));
    }
    Ok(())
}

pub(crate) fn trigger_spec(
    wallet_name: &str,
    wallet_address: &str,
    input_asset: &AssetId,
    output_asset: &AssetId,
    resource: &ResourceId,
    bounds_value: &Value,
    _now_unix: i64,
) -> Result<BoundarySpec> {
    let bounds: TriggerBounds = serde_json::from_value(bounds_value.clone())
        .map_err(|_| FinError::validation("Invalid Jupiter Trigger boundary bounds."))?;
    let maximum = fin_domain::Quantity::new(&bounds.maximum_making_atoms)?;
    let numerator = fin_domain::Quantity::new(&bounds.minimum_taking_per_making_numerator)?;
    let denominator = fin_domain::Quantity::new(&bounds.minimum_taking_per_making_denominator)?;
    if maximum.is_zero()
        || numerator.is_zero()
        || denominator.is_zero()
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_account_funding_lamports == 0
        || bounds.maximum_order_lifetime_seconds == 0
        || bounds.maximum_order_lifetime_seconds > 30 * 24 * 60 * 60
    {
        return Err(FinError::validation(
            "Trigger boundaries require finite positive escrow, rate, fee, funding and expiry caps.",
        ));
    }
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    Ok(BoundarySpec {
        fixed: json!({
            "operation":"jupiter.limit.create",
            "wallet_name":wallet_name,
            "wallet_address":wallet_address,
            "network":"solana",
            "cluster":"mainnet-beta",
            "input_asset":input_asset,
            "output_asset":output_asset,
            "recipient":wallet_address,
            "program":"j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X",
            "maximum_order_lifetime_seconds":bounds.maximum_order_lifetime_seconds,
        }),
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| FinError::validation("Trigger boundary bounds are invalid."))?,
        maximum_debits: vec![
            ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: input_asset.clone(),
                    atoms: maximum,
                },
            },
            ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: fin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
                },
            },
        ],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: native,
                atoms: fin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Create a noncustodial Trigger V1 order from {} to {} for wallet {}; at most {} {}, minimum output rate {} {} per {} {}, network fee at most {}, refundable account funding at most {}, and expiry within {} seconds of dispatch.",
            input_asset.reference,
            output_asset.reference,
            wallet_address,
            format_atomic_quantity(&bounds.maximum_making_atoms, input_asset.decimals),
            input_asset.reference,
            format_atomic_quantity(
                &bounds.minimum_taking_per_making_numerator,
                output_asset.decimals,
            ),
            output_asset.reference,
            format_atomic_quantity(
                &bounds.minimum_taking_per_making_denominator,
                input_asset.decimals,
            ),
            input_asset.reference,
            format_sol_lamports(bounds.maximum_network_fee_lamports),
            format_sol_lamports(bounds.maximum_account_funding_lamports),
            bounds.maximum_order_lifetime_seconds
        ),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_trigger_candidate(
    spec: &BoundarySpec,
    wallet_name: &str,
    wallet_address: &str,
    input_asset: &AssetId,
    output_asset: &AssetId,
    making: u64,
    taking: u64,
    expiry_unix: i64,
    fee_lamports: u64,
    funding_lamports: u64,
    now_unix: i64,
) -> Result<()> {
    if spec.fixed["operation"] != "jupiter.limit.create"
        || spec.fixed["wallet_name"] != wallet_name
        || spec.fixed["wallet_address"] != wallet_address
        || spec.fixed["network"] != "solana"
        || spec.fixed["cluster"] != "mainnet-beta"
        || spec.fixed["input_asset"] != json!(input_asset)
        || spec.fixed["output_asset"] != json!(output_asset)
        || spec.fixed["recipient"] != wallet_address
        || spec.fixed["program"] != "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X"
    {
        return Err(violation(
            "fixed_identity",
            &spec.fixed,
            &json!({"wallet":wallet_address,"input_asset":input_asset,"output_asset":output_asset}),
        ));
    }
    let bounds: TriggerBounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| FinError::validation("Approved Trigger boundary is invalid."))?;
    let maximum = fin_domain::Quantity::new(&bounds.maximum_making_atoms)?;
    let numerator = fin_domain::Quantity::new(&bounds.minimum_taking_per_making_numerator)?;
    let denominator = fin_domain::Quantity::new(&bounds.minimum_taking_per_making_denominator)?;
    if making == 0 || fin_domain::Quantity::from_u64(making) > maximum {
        return Err(violation(
            "maximum_making_atoms",
            &json!(maximum),
            &json!(making),
        ));
    }
    if fin_domain::Quantity::from_u64(taking)
        < fin_domain::Quantity::from_u64(making).mul_div_ceil(&numerator, &denominator)?
    {
        return Err(violation(
            "minimum_taking_rate",
            &json!({"numerator":numerator,"denominator":denominator}),
            &json!({"making":making,"taking":taking}),
        ));
    }
    let latest_expiry = now_unix
        .checked_add(i64::from(bounds.maximum_order_lifetime_seconds))
        .ok_or_else(|| FinError::validation("Trigger expiry boundary overflows Unix time."))?;
    if expiry_unix <= now_unix || expiry_unix > latest_expiry {
        return Err(violation(
            "order_expiry",
            &json!({"now_unix":now_unix,"maximum_lifetime_seconds":bounds.maximum_order_lifetime_seconds,"latest_expiry_unix":latest_expiry}),
            &json!(expiry_unix),
        ));
    }
    if fee_lamports > bounds.maximum_network_fee_lamports {
        return Err(violation(
            "maximum_network_fee_lamports",
            &json!(bounds.maximum_network_fee_lamports),
            &json!(fee_lamports),
        ));
    }
    if funding_lamports > bounds.maximum_account_funding_lamports {
        return Err(violation(
            "maximum_account_funding_lamports",
            &json!(bounds.maximum_account_funding_lamports),
            &json!(funding_lamports),
        ));
    }
    Ok(())
}

pub(crate) fn swap_spec(
    intent: &SwapIntent,
    wallet_address: &str,
    input_asset: &AssetId,
    output_asset: &AssetId,
    bounds_value: &Value,
) -> Result<BoundarySpec> {
    let bounds: SwapBounds = serde_json::from_value(bounds_value.clone())
        .map_err(|_| FinError::validation("Invalid Jupiter swap boundary bounds."))?;
    let cap = fin_domain::Quantity::new(&bounds.maximum_input_atoms)?;
    let rate_num = fin_domain::Quantity::new(&bounds.minimum_output_per_input_numerator)?;
    let rate_den = fin_domain::Quantity::new(&bounds.minimum_output_per_input_denominator)?;
    if cap.is_zero()
        || rate_num.is_zero()
        || rate_den.is_zero()
        || bounds.maximum_slippage_bps > 10_000
        || bounds.maximum_platform_fee_bps > 10_000
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_quote_lifetime_seconds == 0
        || bounds.maximum_quote_lifetime_seconds > 600
    {
        return Err(FinError::validation(
            "Jupiter swap boundaries require positive finite amount/rate/fee caps and a quote lifetime up to ten minutes.",
        ));
    }
    if input_asset.network != NETWORK || output_asset.network != NETWORK {
        return Err(FinError::validation(
            "Jupiter swap boundary assets must be on Solana mainnet-beta.",
        ));
    }
    let route = match intent.route {
        SwapRoute::MetaAggregator => "meta_aggregator",
        SwapRoute::Router => "router",
    };
    let fixed = json!({
        "operation":"jupiter.swap",
        "wallet_name":intent.wallet,
        "wallet_address":wallet_address,
        "network":"solana",
        "cluster":"mainnet-beta",
        "input_asset":input_asset,
        "output_asset":output_asset,
        "recipient":wallet_address,
        "route":route,
    });
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let mut maximum_debits = vec![ResourceAmount {
        resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
        amount: Amount {
            asset: input_asset.clone(),
            atoms: cap,
        },
    }];
    if bounds.maximum_account_funding_lamports > 0 {
        maximum_debits.push(ResourceAmount {
            resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
            amount: Amount {
                asset: native.clone(),
                atoms: fin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        });
    }
    Ok(BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| FinError::validation("Jupiter boundary bounds are invalid."))?,
        maximum_debits,
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
            },
            amount: Amount {
                asset: native,
                atoms: fin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Swap {} for {} to wallet {}; fixed route {}; input cap {} {}, minimum output rate {} {} per {} {}, maximum slippage {} bps, platform fee {} bps, network fee {}, account funding {}, quote lifetime {} seconds.",
            input_asset.reference,
            output_asset.reference,
            wallet_address,
            route,
            format_atomic_quantity(&bounds.maximum_input_atoms, input_asset.decimals),
            input_asset.reference,
            format_atomic_quantity(
                &bounds.minimum_output_per_input_numerator,
                output_asset.decimals,
            ),
            output_asset.reference,
            format_atomic_quantity(
                &bounds.minimum_output_per_input_denominator,
                input_asset.decimals,
            ),
            input_asset.reference,
            bounds.maximum_slippage_bps,
            bounds.maximum_platform_fee_bps,
            format_sol_lamports(bounds.maximum_network_fee_lamports),
            format_sol_lamports(bounds.maximum_account_funding_lamports),
            bounds.maximum_quote_lifetime_seconds,
        ),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_swap_candidate(
    spec: &BoundarySpec,
    intent: &SwapIntent,
    wallet_address: &str,
    input_asset: &AssetId,
    output_asset: &AssetId,
    actual_output_atoms: u64,
    actual_platform_fee_bps: u16,
    actual_platform_fee_mint: Option<&str>,
) -> Result<()> {
    let expected_route = match intent.route {
        SwapRoute::MetaAggregator => "meta_aggregator",
        SwapRoute::Router => "router",
    };
    let fixed = json!({
        "operation":"jupiter.swap",
        "wallet_name":intent.wallet,
        "wallet_address":wallet_address,
        "network":"solana",
        "cluster":"mainnet-beta",
        "input_asset":input_asset,
        "output_asset":output_asset,
        "recipient":wallet_address,
        "route":expected_route,
    });
    if spec.fixed != fixed {
        return Err(violation("fixed_identity", &spec.fixed, &fixed));
    }
    let bounds: SwapBounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| FinError::validation("Approved Jupiter swap boundary is invalid."))?;
    let amount = fin_domain::Quantity::new(&intent.amount_atoms)?;
    let cap = fin_domain::Quantity::new(&bounds.maximum_input_atoms)?;
    let numerator = fin_domain::Quantity::new(&bounds.minimum_output_per_input_numerator)?;
    let denominator = fin_domain::Quantity::new(&bounds.minimum_output_per_input_denominator)?;
    let minimum_output = fin_domain::Quantity::new(&intent.minimum_output_atoms)?;
    if amount > cap {
        return Err(violation(
            "maximum_input_atoms",
            &json!(cap),
            &json!(amount),
        ));
    }
    if intent.maximum_slippage_bps > bounds.maximum_slippage_bps {
        return Err(violation(
            "maximum_slippage_bps",
            &json!(bounds.maximum_slippage_bps),
            &json!(intent.maximum_slippage_bps),
        ));
    }
    if intent.maximum_platform_fee_bps > bounds.maximum_platform_fee_bps
        || actual_platform_fee_bps > bounds.maximum_platform_fee_bps
    {
        return Err(violation(
            "maximum_platform_fee_bps",
            &json!(bounds.maximum_platform_fee_bps),
            &json!(actual_platform_fee_bps),
        ));
    }
    if actual_platform_fee_bps > 0
        && actual_platform_fee_mint
            .is_none_or(|mint| mint != input_asset.reference && mint != output_asset.reference)
    {
        return Err(violation(
            "platform_fee_mint",
            &json!([input_asset.reference, output_asset.reference]),
            &json!(actual_platform_fee_mint),
        ));
    }
    if intent.maximum_network_fee_lamports > bounds.maximum_network_fee_lamports {
        return Err(violation(
            "maximum_network_fee_lamports",
            &json!(bounds.maximum_network_fee_lamports),
            &json!(intent.maximum_network_fee_lamports),
        ));
    }
    if intent.maximum_account_funding_lamports > bounds.maximum_account_funding_lamports {
        return Err(violation(
            "maximum_account_funding_lamports",
            &json!(bounds.maximum_account_funding_lamports),
            &json!(intent.maximum_account_funding_lamports),
        ));
    }
    let min_from_rate = amount.mul_div_ceil(&numerator, &denominator)?;
    if minimum_output < min_from_rate
        || fin_domain::Quantity::from_u64(actual_output_atoms) < minimum_output
    {
        return Err(violation(
            "minimum_output_rate",
            &json!(min_from_rate),
            &json!(minimum_output),
        ));
    }
    // The provider-frozen candidate expiry is separately bounded by the
    // transaction preparation deadline and checked immediately before send.
    let _ = swap::validate_intent(intent)?;
    Ok(())
}

fn violation(field: &str, approved: &Value, candidate: &Value) -> FinError {
    fin_domain::boundaries::violation_error(
        &format!("Jupiter swap boundary {field} was exceeded."),
        fin_domain::boundaries::BoundaryViolation {
            constraint: field.into(),
            approved_bound: approved.clone(),
            candidate_value: candidate.clone(),
            evidence_time: None,
            reauthorization_required: true,
            reconciliation_required: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_summaries_format_atoms_and_sol_without_float_rounding() {
        assert_eq!(format_atomic_quantity("0", 6), "0");
        assert_eq!(format_atomic_quantity("1", 6), "0.000001");
        assert_eq!(format_atomic_quantity("1000000", 6), "1");
        assert_eq!(format_atomic_quantity("1234500", 6), "1.2345");
        assert_eq!(format_sol_lamports(10_000), "0.00001 SOL");
    }

    #[test]
    fn swap_boundary_fixes_identity_and_bounds_amount_rate_fees_and_funding() {
        let input = AssetId {
            network: NETWORK.into(),
            reference: swap::WRAPPED_SOL_MINT.into(),
            decimals: 9,
        };
        let output = AssetId {
            network: NETWORK.into(),
            reference: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            decimals: 6,
        };
        let mut intent = SwapIntent {
            wallet: "wallet".into(),
            network: "solana".into(),
            input_mint: input.reference.clone(),
            output_mint: output.reference.clone(),
            amount_atoms: "50".into(),
            minimum_output_atoms: "100".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 10,
            maximum_network_fee_lamports: 1000,
            maximum_account_funding_lamports: 500,
            route: SwapRoute::Router,
        };
        let bounds = json!({
            "maximum_input_atoms":"100",
            "minimum_output_per_input_numerator":"2",
            "minimum_output_per_input_denominator":"1",
            "maximum_slippage_bps":100,
            "maximum_platform_fee_bps":10,
            "maximum_network_fee_lamports":1000,
            "maximum_account_funding_lamports":500,
            "maximum_quote_lifetime_seconds":60
        });
        let spec = swap_spec(
            &intent,
            "11111111111111111111111111111111",
            &input,
            &output,
            &bounds,
        )
        .unwrap();
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                100,
                5,
                Some(&output.reference)
            )
            .is_ok()
        );
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111112",
                &input,
                &output,
                100,
                5,
                Some(&output.reference)
            )
            .is_err()
        );
        intent.amount_atoms = "101".into();
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                202,
                5,
                Some(&output.reference)
            )
            .is_err()
        );
        intent.amount_atoms = "50".into();
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                99,
                5,
                Some(&output.reference)
            )
            .is_err()
        );
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                100,
                11,
                Some(&output.reference)
            )
            .is_err()
        );
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                100,
                5,
                Some(&input.reference)
            )
            .is_ok()
        );
        assert!(
            validate_swap_candidate(
                &spec,
                &intent,
                "11111111111111111111111111111111",
                &input,
                &output,
                100,
                5,
                Some("11111111111111111111111111111112")
            )
            .is_err()
        );
    }

    #[test]
    fn trigger_boundary_fixes_escrow_rate_funding_and_finite_expiry() {
        let input = AssetId {
            network: NETWORK.into(),
            reference: "InputMint".into(),
            decimals: 6,
        };
        let output = AssetId {
            network: NETWORK.into(),
            reference: "OutputMint".into(),
            decimals: 6,
        };
        let wallet = "WalletAddress";
        let spec = trigger_spec(
            "main",
            wallet,
            &input,
            &output,
            &ResourceId(format!("solana:mainnet-beta:{wallet}")),
            &json!({
                "maximum_making_atoms":"1000",
                "minimum_taking_per_making_numerator":"9",
                "minimum_taking_per_making_denominator":"10",
                "maximum_network_fee_lamports":10000,
                "maximum_account_funding_lamports":5000000,
                "maximum_order_lifetime_seconds":3600
            }),
            1_800_000_000,
        )
        .unwrap();
        let later_review = trigger_spec(
            "main",
            wallet,
            &input,
            &output,
            &ResourceId(format!("solana:mainnet-beta:{wallet}")),
            &json!({
                "maximum_making_atoms":"1000",
                "minimum_taking_per_making_numerator":"9",
                "minimum_taking_per_making_denominator":"10",
                "maximum_network_fee_lamports":10000,
                "maximum_account_funding_lamports":5000000,
                "maximum_order_lifetime_seconds":3600
            }),
            1_800_000_030,
        )
        .unwrap();
        assert_eq!(spec, later_review);
        assert!(
            validate_trigger_candidate(
                &spec,
                "main",
                wallet,
                &input,
                &output,
                900,
                810,
                1_800_003_600,
                9000,
                4_000_000,
                1_800_000_000
            )
            .is_ok()
        );
        assert!(
            validate_trigger_candidate(
                &spec,
                "other",
                wallet,
                &input,
                &output,
                900,
                810,
                1_800_003_600,
                9000,
                4_000_000,
                1_800_000_000
            )
            .is_err()
        );
        assert!(
            validate_trigger_candidate(
                &spec,
                "main",
                wallet,
                &input,
                &output,
                900,
                800,
                1_800_003_600,
                9000,
                4_000_000,
                1_800_000_000
            )
            .is_err()
        );
        assert!(
            validate_trigger_candidate(
                &spec,
                "main",
                wallet,
                &input,
                &output,
                900,
                810,
                1_800_000_005,
                9000,
                4_000_000,
                1_800_000_010,
            )
            .is_err()
        );
        assert!(
            validate_trigger_candidate(
                &spec,
                "main",
                wallet,
                &input,
                &output,
                900,
                810,
                1_800_003_601,
                9000,
                4_000_000,
                1_800_000_000
            )
            .is_err()
        );
        assert!(
            validate_trigger_candidate(
                &spec,
                "main",
                wallet,
                &input,
                &output,
                900,
                810,
                1_800_003_600,
                10001,
                4_000_000,
                1_800_000_000
            )
            .is_err()
        );
    }

    #[test]
    fn trigger_cancel_boundary_binds_exact_parent_order_and_cost_caps() {
        let wallet = "wallet-address";
        let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
        let parent = fin_domain::Submission {
            kind: "solana".into(),
            reference: "solana:message:abc".into(),
            data: json!({"message_sha256":"abc"}),
        };
        let spec = trigger_cancel_spec(
            "main",
            wallet,
            "order-key",
            "parent-operation",
            &parent,
            &resource,
            &json!({
                "maximum_network_fee_lamports":10000,
                "maximum_account_funding_lamports":2000000
            }),
        )
        .unwrap();
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "order-key",
                "parent-operation",
                &parent,
                9000,
                1_000_000,
            )
            .is_ok()
        );
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "another-order",
                "parent-operation",
                &parent,
                9000,
                1_000_000,
            )
            .is_err()
        );
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "order-key",
                "different-parent",
                &parent,
                9000,
                1_000_000,
            )
            .is_err()
        );
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "order-key",
                "parent-operation",
                &parent,
                10001,
                1_000_000,
            )
            .is_err()
        );
    }

    #[test]
    fn plan_recovery_cancel_uses_parent_binding_for_dynamic_order_identity() {
        let wallet = "wallet-address";
        let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
        let parent = fin_domain::Submission {
            kind: "solana".into(),
            reference: "solana:message:abc".into(),
            data: json!({"message_sha256":"abc"}),
        };
        let spec = trigger_cancel_spec(
            "main",
            wallet,
            "__fin_plan_recovery_order_key__",
            "__fin_plan_recovery_parent__",
            &fin_domain::Submission {
                kind: "plan_recovery_parent".into(),
                reference: "create".into(),
                data: json!({"recovery_parent":true}),
            },
            &resource,
            &json!({
                "maximum_network_fee_lamports":10000,
                "maximum_account_funding_lamports":0,
                "recovery_parent_step_id":"create"
            }),
        )
        .unwrap();
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "on-chain-parent-order-key",
                "parent-operation-from-engine",
                &parent,
                9000,
                0,
            )
            .is_ok()
        );
        assert!(
            validate_trigger_cancel_candidate(
                &spec,
                "main",
                wallet,
                "",
                "parent-operation-from-engine",
                &parent,
                9000,
                0,
            )
            .is_err()
        );
    }
}
