//! Generated close principal is financed only by native entry credit; fees have exact external caps.
use crate::{
    profile::error,
    trading::{SpotOrder, TradingControls},
};
use fin_domain::conversion_sequences::{
    ConversionCost, ConversionCostBound, ConversionCostFunding, ConversionLeg,
    ConversionLegConstraint, ConversionLegStatus, ConversionSequence, ConversionSequenceConstraint,
};
use fin_domain::{
    AccountingLocation, Amount, CostBound, CostTreatment, EffectConstraint, FinancialEffect,
    FinancialIntent, Quantity, ResourceId, Result, VerifiedEffects,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
fn leg(order: &SpotOrder, minimum_output: Quantity) -> ConversionLegConstraint {
    ConversionLegConstraint {
        id: order.client_id.clone(),
        input: if order.buy {
            order.quote.clone()
        } else {
            order.base.clone()
        },
        maximum_input: order.maximum_spend.clone(),
        output: if order.buy {
            order.base.clone()
        } else {
            order.quote.clone()
        },
        minimum_output,
    }
}
pub(crate) fn prepare(
    resource: &ResourceId,
    orders: &[SpotOrder],
    conditions: &[Value],
    controls: &TradingControls,
    intent: &mut FinancialIntent,
) -> Result<Vec<ConversionSequenceConstraint>> {
    if conditions.is_empty() {
        return Ok(vec![]);
    }
    let old = intent.clone();
    let mut scopes = Vec::new();
    let mut consumed = BTreeSet::new();
    for condition in conditions {
        let parent = condition["parentIndex"]
            .as_u64()
            .ok_or_else(|| error("KRAKEN_PREVIEW_INVALID", "Conditional entry index missing."))?
            as usize;
        let child = condition["childIndex"]
            .as_u64()
            .ok_or_else(|| error("KRAKEN_PREVIEW_INVALID", "Conditional child index missing."))?
            as usize;
        let root = orders
            .get(parent)
            .ok_or_else(|| error("KRAKEN_PREVIEW_INVALID", "Conditional entry scope missing."))?;
        let close = orders
            .get(child)
            .ok_or_else(|| error("KRAKEN_PREVIEW_INVALID", "Conditional child scope missing."))?;
        if !consumed.insert(parent) || !consumed.insert(child) {
            return Err(error(
                "KRAKEN_PREVIEW_INVALID",
                "Conditional scopes overlap.",
            ));
        }
        let minimum = match &old.expected[parent] {
            EffectConstraint::Conversion { minimum_output, .. } => minimum_output.clone(),
            _ => {
                return Err(error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Entry conversion contract missing.",
                ));
            }
        };
        let fees = [root.base.clone(), root.quote.clone()]
            .into_iter()
            .map(|asset| -> Result<ConversionCostBound> {
                Ok(ConversionCostBound {
                    amount: Amount {
                        atoms: Quantity::from_decimal(
                            controls.maximum_fees.get(&asset.reference).ok_or_else(|| {
                                error(
                                    "KRAKEN_FEE_BOUNDS_REQUIRED",
                                    "Each sequence fee asset needs its exact cap.",
                                )
                            })?,
                            36,
                        )?,
                        asset,
                    },
                    funding: ConversionCostFunding::External,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let root_leg = leg(root, minimum);
        scopes.push(ConversionSequenceConstraint {
            accounting_version: 1,
            sequence_id: format!("kraken:conditional:{}", root.client_id),
            resource: resource.clone(),
            external_input: root_leg.input.clone(),
            maximum_external_input: root.maximum_spend.clone(),
            legs: vec![root_leg, leg(close, Quantity::zero())],
            maximum_costs: fees,
        });
    }
    intent.debits = old
        .debits
        .into_iter()
        .enumerate()
        .filter_map(|(index, debit)| {
            let is_child = conditions
                .iter()
                .any(|c| c["childIndex"].as_u64() == Some(index as u64));
            (!is_child).then_some(debit)
        })
        .collect();
    intent.expected = old
        .expected
        .into_iter()
        .enumerate()
        .filter_map(|(index, e)| (!consumed.contains(&index)).then_some(e))
        .collect();
    intent.maximum_costs.clear();
    for scope in &scopes {
        intent.expected.push(EffectConstraint::ConversionSequence {
            scope: scope.clone(),
        });
        intent
            .maximum_costs
            .extend(scope.maximum_costs.iter().map(|cap| CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: cap.amount.clone(),
                treatment: CostTreatment::AdditionalDebit,
            }));
    }
    // Independent entries retain their existing native fee contract as well.
    if consumed.len() != orders.len() {
        intent.maximum_costs.extend(old.maximum_costs);
    }
    Ok(scopes)
}

pub(crate) fn convert(
    original: &Value,
    current: &Value,
    bundle: &mut VerifiedEffects,
) -> Result<()> {
    if original.get("conversionSequences").is_none() || bundle.effects.is_empty() {
        return Ok(());
    }
    let scopes: Vec<ConversionSequenceConstraint> =
        serde_json::from_value(original["conversionSequences"].clone()).map_err(|_| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Frozen conversion sequences missing.",
            )
        })?;
    let resource = scopes
        .first()
        .ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Frozen sequence scopes are empty.",
            )
        })?
        .resource
        .clone();
    let orders: Vec<SpotOrder> = serde_json::from_value(current["orders"].clone())
        .map_err(|_| error("KRAKEN_SUBMISSION_INVALID", "Native order scopes missing."))?;
    let mut rows: BTreeMap<String, (Amount, Amount, Vec<ConversionCost>)> = BTreeMap::new();
    let mut retained = Vec::new();
    let mut index = 0usize;
    let mut active = None;
    for effect in std::mem::take(&mut bundle.effects) {
        match effect {
            FinancialEffect::Conversion {
                spent, received, ..
            } => {
                let id = orders
                    .get(index)
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_SEQUENCE_PROOF_INVALID",
                            "A native proof conversion has no original owner.",
                        )
                    })?
                    .client_id
                    .clone();
                index += 1;
                active = Some(id.clone());
                rows.insert(id, (spent, received, Vec::new()));
            }
            FinancialEffect::Cost {
                amount,
                category,
                treatment,
                ..
            } => {
                let id = active.as_ref().ok_or_else(|| {
                    error(
                        "KRAKEN_SEQUENCE_PROOF_INVALID",
                        "Fee proof lacks an exact native leg.",
                    )
                })?;
                rows.get_mut(id).unwrap().2.push(ConversionCost {
                    leg_id: id.clone(),
                    amount,
                    category,
                    treatment,
                    funding: ConversionCostFunding::External,
                });
            }
            other => retained.push(other),
        }
    }
    let evidence = bundle.evidence["orders"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for scope in scopes {
        let mut legs = Vec::new();
        let mut costs = Vec::new();
        for approved in &scope.legs {
            let (spent, received, fees) = rows.remove(&approved.id).ok_or_else(|| {
                error(
                    "KRAKEN_SEQUENCE_PROOF_INVALID",
                    "Complete cumulative native leg proof missing.",
                )
            })?;
            let order = orders.iter().find(|o| o.client_id == approved.id).unwrap();
            let terminal = if order.uncreated_child {
                false
            } else {
                let matching = evidence
                    .iter()
                    .filter(|e| {
                        order.order_ids.iter().any(|id| e["orderId"] == *id)
                            || e["order"]["cl_ord_id"] == order.client_id
                    })
                    .collect::<Vec<_>>();
                !matching.is_empty()
                    && matching.iter().all(|e| {
                        matches!(
                            e["order"]["status"].as_str(),
                            Some("closed" | "canceled" | "expired")
                        )
                    })
            };
            legs.push(ConversionLeg {
                id: approved.id.clone(),
                status: if terminal {
                    ConversionLegStatus::Terminal
                } else {
                    ConversionLegStatus::Pending
                },
                spent,
                received,
            });
            costs.extend(fees);
        }
        retained.push(FinancialEffect::ConversionSequence {
            execution: ConversionSequence {
                accounting_version: 1,
                sequence_id: scope.sequence_id,
                resource: scope.resource,
                legs,
                costs,
            },
        });
    }
    for (_id, (spent, received, costs)) in rows {
        retained.push(FinancialEffect::Conversion {
            resource: resource.clone(),
            spent,
            received,
        });
        retained.extend(costs.into_iter().map(|c| FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: c.amount,
            category: c.category,
            treatment: c.treatment,
        }));
    }
    bundle.effects = retained;
    Ok(())
}
