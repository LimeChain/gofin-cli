//! Papertrade reviews use the broker-frozen intent and typed effects.
use super::{ApprovalAction, ApprovalReview, leg};
use crate::{EffectConstraint, GoFinError, PreparedOperation, Result, SettlementCategory};
use serde_json::Value;

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GoFinError::validation(format!("Papertrade approval missing {key}.")))
}

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let mut details = Vec::new();
    if let Some(existing) = review.other_details.take() {
        details.push(existing);
    }
    match prepared.operation.as_str() {
        "papertrade.open" => {
            let request = &prepared.binding["intent_request"];
            let instrument = match request["instrument_id"].as_u64() {
                Some(0) => "BTC",
                Some(1) => "ETH",
                _ => {
                    return Err(GoFinError::validation(
                        "Papertrade approval missing instrument.",
                    ));
                }
            };
            let direction = match request["is_long"].as_bool() {
                Some(true) => "long",
                Some(false) => "short",
                _ => {
                    return Err(GoFinError::validation(
                        "Papertrade approval missing direction.",
                    ));
                }
            };
            let leverage = request["leverage"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| GoFinError::validation("Papertrade approval missing leverage."))?;
            review.market = Some(format!("{instrument} {direction} {leverage}x"));
            details.push("Market price at execution; no price limit".into());
        }
        "papertrade.close" => {
            let ids = prepared.binding["intent_request"]["position_ids"]
                .as_array()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| GoFinError::validation("Papertrade approval missing positions."))?;
            let ids = ids
                .iter()
                .map(|id| {
                    id.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                        GoFinError::validation("Papertrade approval has invalid position.")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            review.market = Some(format!("positions {}", ids.join(", ")));
        }
        "papertrade.cancel" => {
            review.market = Some(format!(
                "intent {}",
                required(&prepared.binding["intent_request"], "intent_id")?
            ))
        }
        "papertrade.claim" => {
            details.push("Rewards estimated; settlement may be zero".into());
        }
        "papertrade.stake" | "papertrade.unstake" => {}
        "papertrade.deposit" => {
            let output = prepared
                .intent
                .expected
                .iter()
                .find_map(|effect| match effect {
                    EffectConstraint::Settlement {
                        resource,
                        asset,
                        category: SettlementCategory::Funding,
                        minimum_delta,
                        ..
                    } if !minimum_delta.is_negative() => {
                        Some(leg(asset, &minimum_delta.magnitude(), resource))
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    GoFinError::validation("Papertrade deposit approval missing settlement.")
                })?;
            review.output = Some(output);
        }
        "papertrade.withdraw" => {}
        "papertrade.funding_continue" => {
            review.action = ApprovalAction::ExchangeWithdrawal;
            details.push(format!(
                "Continue withdrawal {}",
                required(&prepared.binding["funding_continuation"], "route_id")?
            ));
        }
        "papertrade.deposit.invalidate_nonce" => {
            review.market = Some(format!(
                "Core deposit nonce for {}",
                required(
                    &prepared.binding["core_deposit_nonce_recovery"],
                    "parent_operation_id"
                )?
            ));
        }
        _ => {
            return Err(GoFinError::validation(
                "Unknown Papertrade approval action.",
            ));
        }
    }
    // These exact resource forms are defined by the Papertrade adapter. Keep
    // the complete owner and account kind while avoiding repeated namespace
    // text that otherwise exceeds the native presentation budget.
    for side in [&mut review.input, &mut review.output]
        .into_iter()
        .flatten()
    {
        if let Some(account) = &mut side.account {
            let fields: Vec<_> = account.name.split(':').collect();
            let compact = match fields.as_slice() {
                ["hyperliquid", "mainnet", owner, "spot", "primary"] => {
                    Some(format!("Core spot primary {owner}"))
                }
                ["papertrade", "mainnet", owner, "trading_cash"] => {
                    Some(format!("Papertrade trading cash {owner}"))
                }
                ["papertrade", "mainnet", owner, "paper_wallet"] => {
                    Some(format!("PAPER wallet {owner}"))
                }
                ["papertrade", "mainnet", owner, "paper_staked"] => {
                    Some(format!("PAPER staked {owner}"))
                }
                _ => None,
            };
            if let Some(compact) = compact {
                account.name = compact;
            }
        }
    }
    review.other_details = (!details.is_empty()).then(|| details.join("; "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Amount, AssetId, FinancialIntent, Quantity, ResourceAmount, ResourceId, SignedQuantity,
        approval_review::approval_review,
    };
    use serde_json::json;

    #[test]
    fn all_papertrade_writes_render_their_frozen_scope() {
        let asset = AssetId {
            network: "ethereum:999".into(),
            reference: "USDC".into(),
            decimals: 6,
        };
        let source = ResourceId("source".into());
        let destination = ResourceId("destination".into());
        let quantity = Quantity::new("1000000").unwrap();
        let cases = [
            (
                "papertrade.open",
                json!({"intent_request":{"instrument_id":0,"is_long":true,"leverage":5}}),
                "BTC long 5x",
            ),
            (
                "papertrade.close",
                json!({"intent_request":{"position_ids":["position-1"]}}),
                "positions position-1",
            ),
            (
                "papertrade.cancel",
                json!({"intent_request":{"intent_id":"intent-1"}}),
                "intent intent-1",
            ),
            ("papertrade.stake", json!({}), "papertrade"),
            ("papertrade.unstake", json!({}), "papertrade"),
            ("papertrade.claim", json!({}), "Rewards estimated"),
            ("papertrade.deposit", json!({}), "papertrade"),
            ("papertrade.withdraw", json!({}), "papertrade"),
            (
                "papertrade.funding_continue",
                json!({"funding_continuation":{"route_id":"route-1"}}),
                "Continue withdrawal route-1",
            ),
            (
                "papertrade.deposit.invalidate_nonce",
                json!({"core_deposit_nonce_recovery":{"parent_operation_id":"parent-1"}}),
                "Core deposit nonce for parent-1",
            ),
        ];
        for (operation, binding, expected) in cases {
            let effect = if operation == "papertrade.deposit" {
                EffectConstraint::Settlement {
                    resource: destination.clone(),
                    asset: asset.clone(),
                    category: SettlementCategory::Funding,
                    minimum_delta: SignedQuantity::positive(&quantity),
                    maximum_delta: SignedQuantity::positive(&quantity),
                }
            } else {
                EffectConstraint::Transfer {
                    from: source.clone(),
                    to: destination.clone(),
                    debit_asset: asset.clone(),
                    credit_asset: asset.clone(),
                    maximum_debit: quantity.clone(),
                    minimum_credit: quantity.clone(),
                }
            };
            let prepared = PreparedOperation {
                operation: operation.into(),
                input: json!({}),
                binding,
                summary: String::new(),
                checks: vec![],
                warnings: vec![],
                observed_at: String::new(),
                expires_at: String::new(),
                valuation: None,
                intent: FinancialIntent {
                    debits: vec![ResourceAmount {
                        resource: source.clone(),
                        amount: Amount {
                            asset: asset.clone(),
                            atoms: quantity.clone(),
                        },
                    }],
                    expected: vec![effect],
                    ..Default::default()
                },
                operation_version: 1,
                action_deadline: None,
            };
            let review =
                approval_review(&prepared).unwrap_or_else(|e| panic!("{operation}: {e:?}"));
            let text = serde_json::to_string(&review).unwrap();
            assert!(text.contains(expected), "{operation}: {text}");
            if operation == "papertrade.deposit" {
                assert_eq!(review.output.unwrap().account.unwrap().name, "destination");
            }
            if operation == "papertrade.funding_continue" {
                assert_eq!(review.action, ApprovalAction::ExchangeWithdrawal);
            }
        }
    }
}
