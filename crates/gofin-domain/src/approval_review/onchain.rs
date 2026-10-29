//! Typed fields for on-chain position and request actions.
use super::{ApprovalAccount, ApprovalAction, ApprovalLeg, ApprovalReview};
use crate::{EffectConstraint, GoFinError, PreparedOperation, Quantity, Result};
use serde_json::Value;

fn string(v: &Value, key: &str) -> Option<String> {
    let v = v.get(key)?;
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_u64().map(|n| n.to_string()))
}
fn decimal(v: &Value, key: &str, decimals: u8) -> Result<Option<String>> {
    string(v, key)
        .map(|s| Quantity::new(s).map(|q| q.to_decimal(decimals)))
        .transpose()
}
pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let binding = &prepared.binding;
    review.market = string(binding, "market")
        .or_else(|| string(binding, "pool"))
        .or_else(|| binding.get("pool").and_then(|p| string(p, "address")))
        .or_else(|| string(binding, "symbol"));
    let account = prepared.input.get("wallet").and_then(Value::as_str);
    for leg in [&mut review.input, &mut review.output]
        .into_iter()
        .flatten()
    {
        if let Some(name) = account {
            let domain = leg
                .account
                .as_ref()
                .and_then(|a| a.domain.clone())
                .or_else(|| Some("Solana".into()));
            leg.account = Some(ApprovalAccount {
                name: name.into(),
                domain,
            });
        }
    }
    if prepared.operation == "jupiter.limit.create" {
        if let Some((input_asset, output_asset, maximum, minimum)) =
            prepared.intent.expected.iter().find_map(|e| match e {
                EffectConstraint::EscrowLifecycle {
                    input_asset,
                    output_asset,
                    maximum_wallet_debit,
                    minimum_output_per_input_numerator,
                    ..
                } => Some((
                    input_asset,
                    output_asset,
                    maximum_wallet_debit,
                    minimum_output_per_input_numerator,
                )),
                _ => None,
            })
        {
            let owner = account.unwrap_or("wallet");
            let account = ApprovalAccount {
                name: owner.into(),
                domain: Some("Solana".into()),
            };
            review.input = Some(ApprovalLeg {
                amount: Some(maximum.to_decimal(input_asset.decimals)),
                asset: Some(super::approval_asset(input_asset)),
                account: Some(account.clone()),
            });
            review.output = Some(ApprovalLeg {
                amount: Some(minimum.to_decimal(output_asset.decimals)),
                asset: Some(super::approval_asset(output_asset)),
                account: Some(account),
            });
            review.market = Some(format!(
                "{}/{}",
                super::prepared_asset_label(prepared, input_asset),
                super::prepared_asset_label(prepared, output_asset)
            ));
        } else {
            return Err(GoFinError::validation(
                "Trigger order review lacks the bounded escrow terms.",
            ));
        }
    }
    if prepared.operation == "jupiter.limit.cancel" {
        let mint = string(binding, "input_mint")
            .ok_or_else(|| GoFinError::validation("Trigger cancellation lacks its input mint."))?;
        let decimals = binding
            .get("input_decimals")
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GoFinError::validation("Trigger cancellation lacks input decimals."))?;
        let amount = string(binding, "amount_atoms").ok_or_else(|| {
            GoFinError::validation("Trigger cancellation lacks its remaining amount.")
        })?;
        let asset = crate::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: mint,
            decimals,
        };
        review.input = None;
        review.output = Some(ApprovalLeg {
            amount: Some(Quantity::new(amount)?.to_decimal(decimals)),
            asset: Some(super::approval_asset(&asset)),
            account: Some(ApprovalAccount {
                name: account.unwrap_or("wallet").into(),
                domain: Some("Solana".into()),
            }),
        });
    }
    if matches!(
        review.action,
        ApprovalAction::IncreasePosition
            | ApprovalAction::DecreasePosition
            | ApprovalAction::AddMargin
            | ApprovalAction::ReduceMargin
    ) {
        let mint = string(binding, "funding_mint")
            .ok_or_else(|| GoFinError::validation("Perps review lacks the funding mint."))?;
        let decimals = binding["funding_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GoFinError::validation("Perps review lacks funding decimals."))?;
        let asset = crate::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: mint,
            decimals,
        };
        let wallet = account.unwrap_or("wallet");
        let make_leg = |amount: &Quantity| ApprovalLeg {
            amount: Some(amount.to_decimal(decimals)),
            asset: Some(super::approval_asset(&asset)),
            account: Some(ApprovalAccount {
                name: wallet.into(),
                domain: Some("Solana".into()),
            }),
        };
        if matches!(
            review.action,
            ApprovalAction::IncreasePosition | ApprovalAction::AddMargin
        ) {
            let principal = prepared
                .intent
                .debits
                .iter()
                .find(|d| d.amount.asset == asset)
                .ok_or_else(|| GoFinError::validation("Perps review lacks collateral funding."))?;
            review.input = Some(make_leg(&principal.amount.atoms));
            review.output = None;
        } else {
            let principal = prepared
                .intent
                .expected
                .iter()
                .find_map(|effect| match effect {
                    EffectConstraint::Credit {
                        asset: credit_asset,
                        minimum_credit,
                        ..
                    } if credit_asset == &asset => Some(minimum_credit),
                    _ => None,
                })
                .ok_or_else(|| GoFinError::validation("Perps review lacks the payout floor."))?;
            review.input = None;
            review.output = Some(make_leg(principal));
        }
    }
    // Meteora exits emit pool-token and reward credits before any rent refund.
    // Keep every proved credit floor visible; a later refund is not the payout.
    let mut meteora_extra_credits = Vec::new();
    if prepared.operation.starts_with("meteora.position.") {
        let mut credits = prepared
            .intent
            .expected
            .iter()
            .take_while(|effect| matches!(effect, EffectConstraint::Credit { .. }));
        if let Some(EffectConstraint::Credit {
            to,
            asset,
            minimum_credit,
        }) = credits.next()
        {
            review.output = Some(ApprovalLeg {
                amount: Some(minimum_credit.to_decimal(asset.decimals)),
                asset: Some(super::approval_asset(asset)),
                account: Some(ApprovalAccount {
                    name: account.unwrap_or(&to.0).into(),
                    domain: Some("Solana".into()),
                }),
            });
        } else if matches!(
            review.action,
            ApprovalAction::RemoveLiquidity
                | ApprovalAction::CollectFees
                | ApprovalAction::ClosePosition
        ) {
            review.output = None;
        }
        for effect in credits {
            if let EffectConstraint::Credit {
                asset,
                minimum_credit,
                ..
            } = effect
            {
                meteora_extra_credits.push(format!(
                    "Receive ≥ {} {}",
                    minimum_credit.to_decimal(asset.decimals),
                    super::prepared_asset_label(prepared, asset)
                ));
            }
        }
    }
    let mut details = meteora_extra_credits;
    if prepared.operation.starts_with("meteora.position.")
        && let Some(position) = binding
            .pointer("/protocol_candidate/stable_position_address")
            .and_then(Value::as_str)
            .or_else(|| prepared.input.get("position").and_then(Value::as_str))
    {
        details.push(format!("Position {position}"));
    }
    if let Some(side) = binding.get("side").and_then(Value::as_u64) {
        details.push(match side {
            1 => "Long".to_owned(),
            2 => "Short".to_owned(),
            _ => return Err(GoFinError::validation("Unknown position side in approval.")),
        });
    }
    for (key, label) in [
        ("size_usd_atoms", "Size USD"),
        ("trigger_price_usd_atoms", "Trigger USD"),
    ] {
        if let Some(amount) = decimal(binding, key, 6)? {
            details.push(format!("{label} {amount}"));
        }
    }
    let mut usd_fees = Vec::new();
    for (key, label) in [
        ("maximum_protocol_fee_usd_atoms", "total"),
        ("position_and_borrow_fee_ceiling_usd_atoms", "pos+borrow"),
        ("collateral_swap_fee_ceiling_usd_atoms", "swap"),
    ] {
        if let Some(amount) = decimal(binding, key, 6)? {
            usd_fees.push(format!("{label} {amount}"));
        }
    }
    if !usd_fees.is_empty() {
        details.push(format!("USD fees ≤{}", usd_fees.join("/")));
    }
    if prepared.operation == "jupiter.perps.collateral.remove" {
        let amount = decimal(binding, "collateral_atoms", 6)?.ok_or_else(|| {
            GoFinError::validation("Perps collateral removal lacks its USD amount.")
        })?;
        details.push(format!("Collateral USD {amount}"));
    }
    if let Some(kind) = string(binding, "request_type") {
        details.push(format!("Request type {kind}"));
    }
    if binding.get("entire_position").and_then(Value::as_bool) == Some(true) {
        details.push("Entire position".into());
    }
    if let Some(above) = binding
        .get("trigger_above_threshold")
        .and_then(Value::as_bool)
    {
        details.push(if above { "Trigger ≥" } else { "Trigger ≤" }.into());
    }
    if let Some(cap) = string(binding, "maximum_protocol_fee_atoms") {
        let decimals = binding["collateral_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GoFinError::validation("Perps token fee cap lacks decimals."))?;
        let mint = string(binding, "collateral_mint")
            .ok_or_else(|| GoFinError::validation("Perps token fee cap lacks mint."))?;
        let asset = crate::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: mint,
            decimals,
        };
        details.push(format!(
            "Token fee≤{} {}",
            Quantity::new(cap)?.to_decimal(decimals),
            super::prepared_asset_label(prepared, &asset)
        ));
    }
    if let Some(floor) = string(binding, "minimum_net_collateral_atoms") {
        let decimals = binding["collateral_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GoFinError::validation("Perps collateral floor lacks decimals."))?;
        let mint = string(binding, "collateral_mint")
            .ok_or_else(|| GoFinError::validation("Perps collateral floor lacks mint."))?;
        let asset = crate::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: mint,
            decimals,
        };
        details.push(format!(
            "Collateral≥{} {}",
            Quantity::new(floor)?.to_decimal(decimals),
            super::prepared_asset_label(prepared, &asset)
        ));
    }
    if let Some(floor) = string(binding, "minimum_net_receive_atoms") {
        let decimals = binding["funding_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(|| GoFinError::validation("Perps payout floor lacks decimals."))?;
        let mint = string(binding, "funding_mint")
            .ok_or_else(|| GoFinError::validation("Perps payout floor lacks mint."))?;
        let asset = crate::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: mint,
            decimals,
        };
        details.push(format!(
            "Payout ≥ {} {}",
            Quantity::new(floor)?.to_decimal(decimals),
            super::prepared_asset_label(prepared, &asset)
        ));
    }
    if matches!(
        review.action,
        ApprovalAction::CancelOrder | ApprovalAction::EditOrder
    ) && let Some(request) = string(binding, "request").or_else(|| {
        if matches!(review.action, ApprovalAction::CancelOrder) {
            string(binding, "order_key")
        } else {
            None
        }
    }) {
        let short = if request.len() > 20 && request.is_ascii() {
            format!("{}…{}", &request[..6], &request[request.len() - 4..])
        } else {
            request
        };
        details.push(format!("Req {short}"));
    }
    for (key, label) in [
        ("remove_share_bps", "Remove share bps"),
        ("bin_lower", "Lower bin"),
        ("bin_upper", "Upper bin"),
    ] {
        if let Some(value) = string(&prepared.input, key) {
            details.push(format!("{label} {value}"));
        }
    }
    if let Some(bps) = string(binding, "maximum_slippage_bps") {
        details.push(format!("Slippage≤{bps}bp"));
    }
    if let Some(funding) = decimal(binding, "maximum_account_funding_lamports", 9)? {
        details.push(format!("Funding≤{funding} SOL"));
    }
    let funding_cap = string(binding, "maximum_account_funding_lamports")
        .map(Quantity::new)
        .transpose()?;
    for debit in prepared.intent.debits.iter().skip(1) {
        if debit.amount.asset.network == "solana:mainnet-beta"
            && debit.amount.asset.reference == "native"
            && debit.amount.asset.decimals == 9
            && funding_cap
                .as_ref()
                .is_some_and(|cap| debit.amount.atoms <= *cap)
        {
            continue;
        }
        details.push(format!(
            "Additional debit ≤ {} {}",
            debit.amount.atoms.to_decimal(debit.amount.asset.decimals),
            super::prepared_asset_label(prepared, &debit.amount.asset)
        ));
    }
    if prepared.operation.starts_with("jupiter.perps.") {
        let fee_cap = string(binding, "maximum_protocol_fee_usd_atoms")
            .map(Quantity::new)
            .transpose()?;
        for cost in &prepared.intent.maximum_costs {
            if cost.amount.asset.reference == "jupiter:margin-usd"
                && cost.amount.asset.decimals == 6
                && fee_cap.as_ref() == Some(&cost.amount.atoms)
            {
                continue;
            }
            details.push(format!(
                "Fee≤{} {}",
                cost.amount.atoms.to_decimal(cost.amount.asset.decimals),
                super::prepared_asset_label(prepared, &cost.amount.asset)
            ));
        }
        review.other_details = None;
    } else if let Some(costs) = review.other_details.take() {
        details.push(costs);
    }
    if !details.is_empty() {
        review.other_details = Some(details.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FinancialIntent, approval_review::approval_review};
    use serde_json::json;

    #[test]
    fn decrease_review_uses_payout_not_later_rent_refund() {
        use crate::{AssetId, Quantity, ResourceId};
        let payout = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            decimals: 6,
        };
        let sol = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        let resource = ResourceId("owner".into());
        let p = PreparedOperation {
            operation: "jupiter.perps.decrease_market".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"market":"SOL","side":1,"funding_mint":payout.reference,"funding_decimals":6,"size_usd_atoms":"100000000","minimum_net_receive_atoms":"9000000","maximum_protocol_fee_usd_atoms":"100000","position_and_borrow_fee_ceiling_usd_atoms":"90000","collateral_swap_fee_ceiling_usd_atoms":"10000"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent {
                expected: vec![
                    EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: payout,
                        minimum_credit: Quantity::from_u64(9000000),
                    },
                    EffectConstraint::Credit {
                        to: resource,
                        asset: sol,
                        minimum_credit: Quantity::from_u64(1000),
                    },
                ],
                ..Default::default()
            },
            operation_version: 1,
            action_deadline: None,
        };
        let mut collateral = p.clone();
        collateral.binding["collateral_atoms"] = json!("5000000");
        collateral.operation = "jupiter.perps.collateral.remove".into();
        let collateral_review = approval_review(&collateral).unwrap();
        assert_eq!(
            collateral_review
                .output
                .unwrap()
                .asset
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("USDC")
        );
        let review = approval_review(&p).unwrap();
        assert!(review.input.is_none());
        assert_eq!(
            review
                .output
                .unwrap()
                .asset
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("USDC")
        );
        let details = review.other_details.unwrap();
        assert!(details.contains("Payout ≥ 9 USDC") && details.contains("pos+borrow 0.09"));
    }
    #[test]
    fn perps_review_retains_side_size_trigger_and_cost_bound() {
        let p = PreparedOperation {
            operation: "jupiter.perps.limit.create".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"market":"SOL", "side":1, "size_usd_atoms":"100000000", "trigger_price_usd_atoms":"120000000", "trigger_above_threshold":true, "maximum_protocol_fee_usd_atoms":"500000", "maximum_account_funding_lamports":"1000"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let review = approval_review(&p).unwrap();
        assert_eq!(review.market.as_deref(), Some("SOL"));
        let details = review.other_details.unwrap();
        assert!(
            details.contains("Long")
                && details.contains("Size USD 100")
                && details.contains("Trigger USD 120")
                && details.contains("Trigger ≥")
                && details.contains("USD fees ≤total 0.5")
        );
    }
    #[test]
    fn meteora_exit_keeps_both_token_floors_before_rent_refund() {
        use crate::{AssetId, ResourceId};
        let asset = |reference: &str, decimals| AssetId {
            network: "solana:mainnet-beta".into(),
            reference: reference.into(),
            decimals,
        };
        let owner = ResourceId("owner".into());
        let credit = |asset, amount| EffectConstraint::Credit {
            to: owner.clone(),
            asset,
            minimum_credit: Quantity::from_u64(amount),
        };
        let mut p = PreparedOperation {
            operation: "meteora.position.remove".into(),
            input: json!({"wallet":"desk", "remove_share_bps":5000}),
            binding: json!({"pool":{"address":"pool"}}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent {
                expected: vec![
                    credit(asset("native", 9), 1000000000),
                    credit(
                        asset("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6),
                        2000000,
                    ),
                ],
                ..Default::default()
            },
            operation_version: 1,
            action_deadline: None,
        };
        let review = approval_review(&p).unwrap();
        assert_eq!(review.output.unwrap().amount.as_deref(), Some("1"));
        assert!(review.other_details.unwrap().contains("Receive ≥ 2 USDC"));
        p.intent.expected.push(EffectConstraint::PositionOfKind {
            resource: owner.clone(),
            position: "p".into(),
            asset: asset("liquidity", 0),
            position_kind: crate::PositionKind::Liquidity,
            direction: crate::Direction::Decrease,
            maximum_change: Quantity::from_u64(1),
        });
        p.intent.expected.push(credit(asset("native", 9), 123));
        let review = approval_review(&p).unwrap();
        assert_eq!(review.output.unwrap().amount.as_deref(), Some("1"));
        assert!(!review.other_details.unwrap().contains("0.000000123"));
    }
    #[test]
    fn trigger_cancel_parent_retains_remaining_escrow() {
        let p = PreparedOperation {
            operation: "jupiter.limit.cancel".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"order_key":"order", "input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "input_decimals":6, "amount_atoms":"2500000"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let review = approval_review(&p).unwrap();
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("2.5"));
        assert_eq!(output.asset.as_ref().map(|a| a.name.as_str()), Some("USDC"));
        assert!(review.other_details.unwrap().contains("order"));
    }
    #[test]
    fn realistic_perps_detail_budget_is_fail_closed() {
        let p = PreparedOperation {
            operation: "jupiter.perps.limit.create".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"market":"SOL","side":1,"size_usd_atoms":"100000000", "trigger_price_usd_atoms":"120000000", "trigger_above_threshold":true, "request":"11111111111111111111111111111111111111111111", "maximum_protocol_fee_usd_atoms":"500000", "maximum_account_funding_lamports":"10000"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let review = approval_review(&p).unwrap();
        assert!(review.other_details.is_some());
        for operation in [
            "jupiter.perps.tpsl.create",
            "jupiter.perps.limit.update",
            "jupiter.perps.tpsl.update",
            "jupiter.perps.limit.cancel",
            "jupiter.perps.tpsl.cancel",
        ] {
            let mut case = p.clone();
            case.operation = operation.into();
            assert!(approval_review(&case).is_ok(), "{operation}");
        }
        let mut increase = p.clone();
        increase.operation = "jupiter.perps.increase_market".into();
        increase.binding["funding_mint"] = json!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        increase.binding["funding_decimals"] = json!(6);
        increase.binding["trigger_price_usd_atoms"] = json!(null);
        increase.binding["trigger_above_threshold"] = json!(null);
        increase.intent.debits.push(crate::ResourceAmount {
            resource: crate::ResourceId("owner".into()),
            amount: crate::Amount {
                asset: crate::AssetId {
                    network: "solana:mainnet-beta".into(),
                    reference: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                    decimals: 6,
                },
                atoms: Quantity::from_u64(1000000),
            },
        });
        assert!(approval_review(&increase).is_ok());
        increase.binding["position_and_borrow_fee_ceiling_usd_atoms"] = json!("90000");
        increase.binding["collateral_swap_fee_ceiling_usd_atoms"] = json!("10000");
        increase.binding["collateral_mint"] = json!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        increase.binding["collateral_decimals"] = json!(6);
        increase.binding["maximum_protocol_fee_atoms"] = json!("500000");
        increase.binding["minimum_net_collateral_atoms"] = json!("900000");
        assert!(approval_review(&increase).is_ok());
        let mut too_many = p;
        too_many.binding["collateral_mint"] = json!("unknown-mint-111111111111111111111111111111");
        too_many.binding["collateral_decimals"] = json!(6);
        too_many.binding["maximum_protocol_fee_atoms"] = json!("500000");
        too_many.binding["minimum_net_collateral_atoms"] = json!("900000");
        too_many.binding["position_and_borrow_fee_ceiling_usd_atoms"] = json!("90000");
        too_many.binding["collateral_swap_fee_ceiling_usd_atoms"] = json!("10000");
        assert_eq!(
            approval_review(&too_many).unwrap_err().code,
            "APPROVAL_REVIEW_TOO_LARGE"
        );
    }
}
