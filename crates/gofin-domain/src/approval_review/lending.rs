//! Presentation of lending and vault limits already validated by adapters.
use super::{ApprovalAccount, ApprovalAction, ApprovalLeg, ApprovalReview, swap};
use crate::{EffectConstraint, GoFinError, PreparedOperation, Quantity, Result};
use serde_json::Value;

fn identity_label(value: String) -> String {
    let evm_address = value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit());
    let solana_address = (32..=44).contains(&value.len())
        && value.bytes().all(|byte| {
            b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains(&byte)
        });
    if evm_address || solana_address {
        format!("{}…{}", &value[..6], &value[value.len() - 4..])
    } else {
        value
    }
}

pub(super) fn asset_identity_label(prepared: &PreparedOperation, asset: &crate::AssetId) -> String {
    let label = super::prepared_asset_label(prepared, asset);
    if label == asset.reference {
        identity_label(label)
    } else {
        label
    }
}

fn scalar(value: &Value, key: &str) -> Option<String> {
    let value = value.get(key)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .or_else(|| value.as_bool().map(|b| b.to_string()))
}

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let binding = &prepared.binding;
    if prepared.operation.starts_with("kamino.lending.") {
        return klend(prepared, review);
    }
    if matches!(
        prepared.operation.as_str(),
        "kamino.deposit" | "kamino.withdraw"
    ) {
        return kamino_vault(prepared, review);
    }
    let input = &prepared.input;
    review.market = scalar(binding, "pool")
        .or_else(|| scalar(binding, "vault"))
        .or_else(|| scalar(binding, "market"))
        .or_else(|| scalar(input, "market"))
        .map(identity_label);
    if let Some(wallet) = scalar(input, "wallet") {
        for leg in [&mut review.input, &mut review.output]
            .into_iter()
            .flatten()
        {
            leg.account = Some(ApprovalAccount {
                name: wallet.clone(),
                domain: leg
                    .account
                    .as_ref()
                    .and_then(|a| a.domain.clone())
                    .or_else(|| {
                        scalar(&binding["token"], "network")
                            .map(|network| swap::network_label(&network))
                    }),
            });
        }
    }
    if prepared.operation == "aave.collateral" {
        let token: crate::AssetId =
            serde_json::from_value(binding["token"].clone()).map_err(|_| {
                GoFinError::validation("Aave collateral review lacks reserve token identity.")
            })?;
        let wallet = scalar(input, "wallet").ok_or_else(|| {
            GoFinError::validation("Aave collateral review lacks wallet identity.")
        })?;
        let enabled = binding["enabled"].as_bool().ok_or_else(|| {
            GoFinError::validation("Aave collateral review lacks frozen enabled state.")
        })?;
        review.input = Some(ApprovalLeg {
            amount: None,
            asset: Some(super::approval_asset(&token)),
            account: Some(ApprovalAccount {
                name: wallet,
                domain: Some(swap::network_label(&token.network)),
            }),
        });
        let detail = format!("Collateral enabled: {enabled}");
        review.other_details = Some(match review.other_details.take() {
            Some(costs) => format!("{detail}; {costs}"),
            None => detail,
        });
    }
    let mut details = Vec::new();
    if let Some(existing) = review.other_details.take() {
        details.push(existing);
    }
    for (key, label) in [
        ("min_health_factor", "Health ≥"),
        ("max_debt_usd", "Debt USD ≤"),
        ("max_debt_to_collateral_bps", "Debt/collateral bps ≤"),
        ("min_shares", "Shares ≥"),
        ("max_shares", "Shares ≤"),
        ("accept_prevailing_value", "Accept prevailing value"),
    ] {
        if let Some(value) = scalar(input, key) {
            details.push(format!("{label}: {value}"));
        }
    }
    if prepared.operation.starts_with("aave.") {
        if binding["repay_all"].as_bool() == Some(true) {
            details.push("Repay all debt up to bound".into());
        }
        if binding["all_amount"].as_bool() == Some(true) && prepared.operation == "aave.withdraw" {
            details.push("Withdraw all up to bound".into());
        }
    }
    if prepared.operation == "morpho.rewards.claim" {
        if let Some(output) = review.output.as_mut() {
            output.amount = None;
            output.asset = None;
        }
        let claims = binding["claims"]
            .as_array()
            .filter(|claims| !claims.is_empty())
            .ok_or_else(|| GoFinError::validation("Reward review lacks selected claims."))?;
        let mut amounts = Vec::new();
        for claim in claims {
            let token: crate::AssetId = serde_json::from_value(claim["token"].clone())
                .map_err(|_| GoFinError::validation("Reward review lacks token identity."))?;
            let payout = scalar(claim, "payout")
                .ok_or_else(|| GoFinError::validation("Reward review lacks estimated payout."))?;
            amounts.push(format!(
                "{} {}",
                Quantity::new(payout)?.to_decimal(token.decimals),
                super::prepared_asset_label(prepared, &token)
            ));
        }
        details.push(format!(
            "Estimated rewards: {}; payout variable",
            amounts.join(", ")
        ));
    }
    if prepared.operation == "morpho.redeem" {
        let shares = scalar(binding, "shares")
            .ok_or_else(|| GoFinError::validation("Morpho redeem review lacks shares."))?;
        let share: crate::AssetId = serde_json::from_value(binding["share_asset"].clone())
            .map_err(|_| GoFinError::validation("Morpho redeem review lacks share identity."))?;
        details.push(format!(
            "Redeem {} shares; payout variable",
            Quantity::new(shares)?.to_decimal(share.decimals)
        ));
    }
    review.other_details = (!details.is_empty()).then(|| details.join("; "));
    if prepared.operation.starts_with("aave.") && review.market.is_none() {
        return Err(GoFinError::validation(
            "Aave approval is missing the selected market identity.",
        ));
    }
    Ok(())
}

fn kamino_vault(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let binding = &prepared.binding;
    let intent = &binding["intent"];
    let vault = scalar(intent, "vault")
        .ok_or_else(|| GoFinError::validation("Kamino vault review lacks its vault."))?;
    let wallet = scalar(intent, "wallet")
        .ok_or_else(|| GoFinError::validation("Kamino vault review lacks its wallet."))?;
    let shares = &binding["vault"]["shares"];
    let decimals = shares["decimals"]
        .as_u64()
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| GoFinError::validation("Kamino vault review lacks share decimals."))?;
    let share_mint = scalar(shares, "mint")
        .ok_or_else(|| GoFinError::validation("Kamino vault review lacks share identity."))?;
    let mut details = Vec::new();
    if prepared.operation == "kamino.withdraw" {
        let atoms = scalar(intent, "shares_atoms").ok_or_else(|| {
            GoFinError::validation("Kamino vault review lacks withdrawal shares.")
        })?;
        details.push(format!(
            "Redeem {} shares {}",
            Quantity::new(atoms)?.to_decimal(decimals),
            identity_label(share_mint)
        ));
        details.push("Receipts variable".into());
        let assets = binding["vault"]["assets"].as_array().ok_or_else(|| {
            GoFinError::validation("Kamino vault review lacks underlying assets.")
        })?;
        let mut displayed = std::collections::BTreeMap::<String, String>::new();
        for token in assets {
            let mint = scalar(token, "mint").ok_or_else(|| {
                GoFinError::validation("Kamino vault review lacks underlying mint.")
            })?;
            let decimals = token["decimals"]
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| {
                    GoFinError::validation("Kamino vault review lacks underlying decimals.")
                })?;
            let label = asset_identity_label(
                prepared,
                &crate::AssetId {
                    network: "solana:mainnet-beta".into(),
                    reference: mint.clone(),
                    decimals,
                },
            );
            if displayed
                .insert(label.clone(), mint.clone())
                .is_some_and(|previous| previous != mint)
            {
                return Err(GoFinError::validation(
                    "Kamino vault underlying asset labels are ambiguous.",
                ));
            }
            details.push(label);
        }
        // Credits are unconstrained zero floors; do not present zero as an expected payout.
        if let Some(output) = review.output.as_mut() {
            output.amount = Some("0".into());
        }
    } else {
        let amounts = intent["amounts"]
            .as_array()
            .ok_or_else(|| GoFinError::validation("Kamino vault review lacks deposit assets."))?;
        if amounts.len() > 1 {
            for row in amounts.iter().skip(1) {
                let token = &row["token"];
                let mint = scalar(token, "mint").ok_or_else(|| {
                    GoFinError::validation("Kamino vault review lacks deposit mint.")
                })?;
                let decimals = token["decimals"]
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| {
                        GoFinError::validation("Kamino vault review lacks deposit decimals.")
                    })?;
                let atoms = scalar(row, "atoms").ok_or_else(|| {
                    GoFinError::validation("Kamino vault review lacks deposit amount.")
                })?;
                details.push(format!(
                    "Deposit ≤ {} {}",
                    Quantity::new(atoms)?.to_decimal(decimals),
                    super::prepared_asset_label(
                        prepared,
                        &crate::AssetId {
                            network: "solana:mainnet-beta".into(),
                            reference: mint,
                            decimals
                        }
                    )
                ));
            }
        }
        if let Some(atoms) = scalar(intent, "minimum_shares_atoms") {
            details.push(format!(
                "Shares ≥ {}",
                Quantity::new(atoms)?.to_decimal(decimals)
            ));
        }
        if intent["stake"].as_bool() == Some(true) {
            details.push("Stake shares".into());
        }
    }
    review.market = Some(identity_label(vault));
    for leg in [&mut review.input, &mut review.output]
        .into_iter()
        .flatten()
    {
        leg.account = Some(ApprovalAccount {
            name: wallet.clone(),
            domain: Some("Solana".into()),
        });
    }
    if let Some(costs) = review.other_details.take() {
        details.push(costs);
    }
    review.other_details = Some(details.join("; "));
    Ok(())
}

fn klend(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let intent = &prepared.binding["intent"];
    let token = &intent["token"];
    let mint = scalar(token, "mint")
        .ok_or_else(|| GoFinError::validation("KLend review is missing its frozen token mint."))?;
    let decimals = token["decimals"]
        .as_u64()
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| GoFinError::validation("KLend review is missing token decimals."))?;
    let wallet = scalar(intent, "wallet")
        .ok_or_else(|| GoFinError::validation("KLend review is missing its wallet."))?;
    let asset = crate::AssetId {
        network: "solana:mainnet-beta".into(),
        reference: mint,
        decimals,
    };
    let account = Some(ApprovalAccount {
        name: wallet,
        domain: Some("Solana".into()),
    });
    let make_leg = |amount: &Quantity| ApprovalLeg {
        amount: Some(amount.to_decimal(decimals)),
        asset: Some(super::ApprovalAsset {
            name: asset_identity_label(prepared, &asset),
            identifier: asset.clone(),
        }),
        account: account.clone(),
    };
    if matches!(
        review.action,
        ApprovalAction::LendingBorrow | ApprovalAction::LendingWithdraw
    ) {
        let mut matches = prepared
            .intent
            .expected
            .iter()
            .filter_map(|effect| match effect {
                EffectConstraint::Credit {
                    asset: credit_asset,
                    minimum_credit,
                    ..
                } if credit_asset == &asset => Some(minimum_credit),
                _ => None,
            });
        let amount = matches
            .next()
            .ok_or_else(|| GoFinError::validation("KLend review lacks the principal receipt."))?;
        if matches.next().is_some() {
            return Err(GoFinError::validation(
                "KLend review has ambiguous principal receipts.",
            ));
        }
        review.input = None;
        review.output = Some(make_leg(amount));
    } else {
        let mut matches = prepared
            .intent
            .debits
            .iter()
            .filter(|d| d.amount.asset == asset);
        let debit = matches
            .next()
            .ok_or_else(|| GoFinError::validation("KLend review lacks the principal debit."))?;
        if matches.next().is_some() {
            return Err(GoFinError::validation(
                "KLend review has ambiguous principal debits.",
            ));
        }
        review.input = Some(make_leg(&debit.amount.atoms));
        review.output = None;
    }
    review.market = scalar(intent, "market").map(identity_label);
    let reserve = scalar(intent, "reserve")
        .ok_or_else(|| GoFinError::validation("KLend review lacks the reserve."))?;
    let obligation = scalar(intent, "obligation")
        .ok_or_else(|| GoFinError::validation("KLend review lacks the obligation."))?;
    let mut details = vec![
        format!("Res. {}", identity_label(reserve)),
        format!("Obl. {}", identity_label(obligation)),
    ];
    let protocol_fee = scalar(intent, "maximum_protocol_fee_atoms")
        .map(Quantity::new)
        .transpose()?;
    let expected_debt = format!(
        "kamino:lending:{}:{}:{}:debt",
        scalar(intent, "market").unwrap_or_default(),
        scalar(intent, "obligation").unwrap_or_default(),
        scalar(intent, "reserve").unwrap_or_default()
    );
    let wallet_resource = scalar(&prepared.binding, "wallet_address")
        .map(|owner| format!("solana:mainnet-beta:{owner}"));
    let is_debt_fee = |cost: &crate::CostBound| {
        prepared.operation == "kamino.lending.borrow"
            && matches!(&cost.location, crate::AccountingLocation::Position { resource, position, position_kind: crate::PositionKind::VariableDebt } if Some(resource.0.as_str()) == wallet_resource.as_deref() && position == &expected_debt)
            && prepared.intent.expected.iter().any(|effect| matches!(effect,
                EffectConstraint::OriginMovement { from, debit_asset, .. } if from == &cost.location && debit_asset == &asset))
            && cost.treatment == crate::CostTreatment::IncludedInDebit
            && cost.amount.asset == asset
            && protocol_fee.as_ref().is_some_and(|fee| cost.amount.atoms.to_decimal(cost.amount.asset.decimals) == fee.to_decimal(decimals))
    };
    let matching_included_fee = prepared
        .intent
        .maximum_costs
        .iter()
        .filter(|cost| is_debt_fee(cost))
        .count()
        == 1;
    for (key, label) in [
        ("maximum_debt_increase_atoms", "Debt ≤"),
        ("maximum_protocol_fee_atoms", "Fee ≤"),
    ] {
        if let Some(value) = scalar(intent, key) {
            let value = Quantity::new(value)?;
            let asset_label = asset_identity_label(prepared, &asset);
            let label = if key == "maximum_protocol_fee_atoms" && matching_included_fee {
                "Incl. fee ≤"
            } else {
                label
            };
            details.push(format!(
                "{label} {} {}",
                value.to_decimal(decimals),
                asset_label
            ));
        }
    }
    if let Some(value) = scalar(intent, "maximum_account_funding_lamports") {
        details.push(format!(
            "Fund ≤ {} SOL",
            Quantity::new(value)?.to_decimal(9)
        ));
    }
    let risk = &intent["risk_limits"];
    for (key, label) in [
        ("minimum_health_factor_bps", "HF ≥ bps"),
        ("maximum_debt_to_collateral_bps", "D/C ≤ bps"),
    ] {
        if let Some(value) = scalar(risk, key) {
            details.push(format!("{label} {value}"));
        }
    }
    if let Some(value) = scalar(risk, "maximum_debt_usd_micros") {
        details.push(format!(
            "Debt ≤ USD {}",
            Quantity::new(value)?.to_decimal(6)
        ));
    }
    if review.other_details.take().is_some() {
        for cost in &prepared.intent.maximum_costs {
            if matching_included_fee && is_debt_fee(cost) {
                continue;
            }
            let label = match cost.treatment {
                crate::CostTreatment::VenueCalculatedAdditionalDebit => {
                    "Venue fee not capped; reserve"
                }
                crate::CostTreatment::IncludedInDebit => "Incl. cost ≤",
                crate::CostTreatment::DeductedFromCredit => "Deducted cost ≤",
                crate::CostTreatment::AdditionalDebit => "Fee ≤",
            };
            details.push(format!(
                "{label}: {} {}",
                cost.amount.atoms.to_decimal(cost.amount.asset.decimals),
                asset_identity_label(prepared, &cost.amount.asset)
            ));
        }
    }
    review.other_details = Some(details.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Amount, AssetId, FinancialIntent, ResourceAmount, ResourceId,
        approval_review::approval_review,
    };
    use serde_json::json;
    #[test]
    fn kamino_mint_fallback_keeps_full_bound_identity_and_rejects_unrelated_labels() {
        let mint = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";
        let other = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
        let mut p = PreparedOperation {
            operation: "kamino.withdraw".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"intent":{"wallet":"desk","vault":"vault","shares_atoms":"1000000"},"vault":{"shares":{"mint":"235TWhH74D3h88QY9hb6yaSriAyaBzsPAzf9Azm5YGob","decimals":6},"assets":[{"mint":mint,"decimals":9},{"mint":other,"decimals":6}]},"approval_asset_metadata":[{"name":"Spoofed","identifier":{"network":"solana:mainnet-beta","reference":other,"decimals":9}}]}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent {
                expected: vec![EffectConstraint::Credit {
                    to: ResourceId("owner".into()),
                    asset: AssetId {
                        network: "solana:mainnet-beta".into(),
                        reference: mint.into(),
                        decimals: 9,
                    },
                    minimum_credit: Quantity::zero(),
                }],
                ..Default::default()
            },
        };
        let first = approval_review(&p).unwrap();
        let details = first.other_details.unwrap();
        assert!(details.contains("mSoLzY…m7So") && !details.contains("Spoofed"));
        assert!(details.contains("Redeem 1 shares"));
        let original = crate::binding_hash(&crate::authorization_binding(&p)).unwrap();
        p.binding["vault"]["assets"][1]["mint"] =
            json!("mSoLzYAxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");
        p.binding["vault"]["assets"][1]["decimals"] = json!(9);
        assert!(approval_review(&p).is_err());
        p.binding["vault"]["assets"][0]["mint"] =
            json!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7Sp");
        assert_ne!(
            original,
            crate::binding_hash(&crate::authorization_binding(&p)).unwrap()
        );
        assert_ne!(p.binding["vault"]["assets"][0]["mint"], mint);
    }
    #[test]
    fn aave_collateral_shows_frozen_reserve_and_enabled_state() {
        let p = PreparedOperation {
            operation: "aave.collateral".into(),
            input: json!({"wallet":"desk","enabled":false}),
            binding: json!({"pool":"pool","enabled":true,"token":{"network":"ethereum:1","reference":"USDC","decimals":6}}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent::default(),
        };
        let review = approval_review(&p).unwrap();
        let leg = review.input.unwrap();
        assert_eq!(leg.asset.as_ref().map(|a| a.name.as_str()), Some("USDC"));
        assert_eq!(leg.account.unwrap().name, "desk");
        assert!(
            review
                .other_details
                .unwrap()
                .contains("Collateral enabled: true")
        );
    }
    #[test]
    fn morpho_claim_lists_each_selected_reward_and_variable_payout() {
        let token =
            |reference: &str| json!({"network":"ethereum:1","reference":reference,"decimals":6});
        let p = PreparedOperation {
            operation: "morpho.rewards.claim".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"claims":[{"token":token("USDC"),"payout":"1000000"},{"token":token("USDT"),"payout":"2000000"}]}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent {
                expected: vec![EffectConstraint::Credit {
                    to: ResourceId("owner".into()),
                    asset: AssetId {
                        network: "ethereum:1".into(),
                        reference: "USDT".into(),
                        decimals: 6,
                    },
                    minimum_credit: Quantity::zero(),
                }],
                ..Default::default()
            },
        };
        let review = approval_review(&p).unwrap();
        let output = review.output.unwrap();
        assert!(output.amount.is_none() && output.asset.is_none());
        assert_eq!(output.account.unwrap().name, "desk");
        let details = review.other_details.unwrap();
        assert!(details.contains("1 USDC, 2 USDT"));
        assert!(details.contains("payout variable"));
    }
    #[test]
    fn klend_borrow_does_not_confuse_rent_with_principal() {
        let usdc = AssetId {
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
        let mut p = PreparedOperation {
            operation: "kamino.lending.borrow".into(),
            input: json!({"wallet":"desk"}),
            binding: json!({"intent":{"wallet":"desk","market":"market","reserve":"reserve","obligation":"obligation","token":{"mint":usdc.reference,"decimals":6},"maximum_debt_increase_atoms":"11000000","maximum_protocol_fee_atoms":"10000","maximum_account_funding_lamports":1000,"risk_limits":{"minimum_health_factor_bps":15000,"maximum_debt_usd_micros":"100000000","maximum_debt_to_collateral_bps":5000}}}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: sol.clone(),
                        atoms: Quantity::from_u64(1000),
                    },
                }],
                expected: vec![
                    EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: usdc,
                        minimum_credit: Quantity::from_u64(10000000),
                    },
                    EffectConstraint::Credit {
                        to: resource,
                        asset: sol,
                        minimum_credit: Quantity::from_u64(1000),
                    },
                ],
                ..Default::default()
            },
        };
        let review = approval_review(&p).unwrap();
        assert!(review.input.is_none());
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("10"));
        assert_eq!(output.asset.as_ref().map(|a| a.name.as_str()), Some("USDC"));
        let details = review.other_details.unwrap();
        assert!(
            details.contains("Debt ≤ 11 USDC")
                && details.contains("Fund ≤ 0.000001 SOL")
                && details.contains("HF ≥ bps 15000")
        );
        let mint = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
        let mut credit = match &p.intent.expected[0] {
            EffectConstraint::Credit { asset, .. } => asset.clone(),
            _ => unreachable!(),
        };
        credit.reference = mint.into();
        if let EffectConstraint::Credit { asset, .. } = &mut p.intent.expected[0] {
            *asset = credit.clone();
        }
        p.binding["intent"]["token"]["mint"] = json!(mint);
        p.binding["wallet_address"] = json!("owner");
        p.binding["approval_asset_metadata"] = json!([{"name":"Spoofed","identifier":{"network":"solana:mainnet-beta","reference":mint,"decimals":9}}]);
        let debt_location = crate::AccountingLocation::Position {
            resource: ResourceId("solana:mainnet-beta:owner".into()),
            position: "kamino:lending:market:obligation:reserve:debt".into(),
            position_kind: crate::PositionKind::VariableDebt,
        };
        p.intent.expected.push(EffectConstraint::OriginMovement {
            from: debt_location.clone(),
            to: crate::AccountingLocation::Cash {
                resource: ResourceId("owner".into()),
            },
            debit_asset: credit.clone(),
            credit_asset: credit.clone(),
            maximum_debit: Quantity::from_u64(11000000),
            minimum_credit: Quantity::from_u64(10000000),
        });
        p.intent.maximum_costs.push(crate::CostBound {
            location: debt_location,
            amount: Amount {
                asset: credit.clone(),
                atoms: Quantity::from_u64(10000),
            },
            treatment: crate::CostTreatment::IncludedInDebit,
        });
        let review = approval_review(&p).unwrap();
        let displayed = review.output.unwrap().asset.unwrap();
        assert_eq!(displayed.identifier, credit);
        assert_eq!(displayed.name, "Es9vMF…wNYB");
        let details = review.other_details.unwrap();
        assert!(details.contains("Debt ≤ 11 Es9vMF…wNYB") && !details.contains("Spoofed"));
        assert!(
            details.contains("Incl. fee ≤ 0.01 Es9vMF…wNYB") && !details.contains("Incl. cost ≤")
        );
        for changed in 0..5 {
            let mut different = p.clone();
            let cost = &mut different.intent.maximum_costs[0];
            match changed {
                0 => cost.amount.atoms = Quantity::from_u64(20000),
                1 => cost.amount.asset.reference = "another-mint".into(),
                2 => cost.treatment = crate::CostTreatment::AdditionalDebit,
                3 => {
                    cost.location = crate::AccountingLocation::Cash {
                        resource: ResourceId("owner".into()),
                    }
                }
                _ => {
                    if let crate::AccountingLocation::Position { position, .. } = &mut cost.location
                    {
                        *position = "other-debt".into();
                    }
                }
            }
            let details = approval_review(&different).unwrap().other_details.unwrap();
            assert!(details.contains("Fee ≤ 0.01 Es9vMF…wNYB"));
            assert!(details.contains(if changed == 2 {
                "Fee ≤:"
            } else {
                "Incl. cost ≤:"
            }));
        }
        let mut repay = p.clone();
        repay.operation = "kamino.lending.repay".into();
        repay.intent.debits.push(ResourceAmount {
            resource: ResourceId("owner".into()),
            amount: Amount {
                asset: credit.clone(),
                atoms: Quantity::from_u64(10000),
            },
        });
        repay.intent.maximum_costs[0].location = crate::AccountingLocation::Cash {
            resource: ResourceId("owner".into()),
        };
        let repay_details = approval_review(&repay).unwrap().other_details.unwrap();
        assert!(
            repay_details.contains("Fee ≤ 0.01") && repay_details.contains("Incl. cost ≤: 0.01")
        );
        let hash = crate::binding_hash(&crate::authorization_binding(&p)).unwrap();
        p.binding["intent"]["token"]["mint"] =
            json!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYC");
        assert_ne!(
            hash,
            crate::binding_hash(&crate::authorization_binding(&p)).unwrap()
        );
    }
}
