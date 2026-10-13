//! Lossless family projections for revisions of a mixed native batch.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProfile,
    observation::result,
    profile::error,
    trading::{SpotOrder, TradingControls},
};
use fin_domain::{
    Amount, AssetId, EffectConstraint, FinancialIntent, PreparedOperation, Quantity,
    ResourceAmount, Result,
    amendments::{AmendmentContext, AmendmentScope},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn invalid(message: &str) -> fin_domain::FinError {
    error("KRAKEN_MIXED_REVISION_SCOPE_INVALID", message)
}
fn family_scope(scope: &AmendmentScope, family: &str) -> Result<AmendmentScope> {
    let mixed = &scope.binding["mixedSpotMargin"];
    if !matches!(family, "cash" | "margin") || !mixed[family].is_object() {
        return Err(invalid(
            "Original mixed batch family identity is unavailable.",
        ));
    }
    Ok(AmendmentScope {
        binding: mixed[family].clone(),
        intent: serde_json::from_value(mixed[format!("{family}Intent")].clone())
            .map_err(|_| invalid("Original exact family financial intent is unavailable."))?,
        valuation: serde_json::from_value(mixed[format!("{family}Valuation")].clone())
            .map_err(|_| invalid("Original exact family valuation is unavailable."))?,
    })
}
/// This projects trusted journal metadata, never caller-supplied authority.
pub(crate) fn project_family_context(
    context: &AmendmentContext,
    family: &str,
) -> Result<AmendmentContext> {
    let mut projected = context.clone();
    if let Some(original) = &mut projected.original {
        let scope = family_scope(
            &AmendmentScope {
                binding: original.binding.clone(),
                intent: original.intent.clone(),
                valuation: original.valuation.clone(),
            },
            family,
        )?;
        original.binding = scope.binding;
        original.intent = scope.intent;
        original.valuation = scope.valuation;
    }
    for (revision, original) in projected.revisions.iter_mut().zip(&context.revisions) {
        revision.scope = family_scope(&original.scope, family)?;
        if let Some(successor) = &mut revision.successor {
            let index = successor.data["batchOrderIndex"].as_u64().ok_or_else(|| {
                invalid("Mixed legacy successor requires its exact original native member index.")
            })?;
            let members = original.scope.binding["mixedSpotMargin"]["members"]
                .as_array()
                .ok_or_else(|| invalid("Original mixed member ownership map is missing."))?;
            let member = members
                .iter()
                .find(|member| member["index"] == index)
                .ok_or_else(|| invalid("Legacy successor is outside the original mixed batch."))?;
            if member["family"] == family {
                successor.data["batchOrderIndex"] = member["familyIndex"].clone();
            } else {
                revision.successor = None;
            }
        }
    }
    Ok(projected)
}
fn sum_debits(
    rows: &[ResourceAmount],
) -> Result<BTreeMap<(fin_domain::ResourceId, AssetId), Quantity>> {
    let mut totals = BTreeMap::new();
    for row in rows {
        let value = totals
            .entry((row.resource.clone(), row.amount.asset.clone()))
            .or_insert_with(Quantity::zero);
        *value = value.checked_add(&row.amount.atoms)?;
    }
    Ok(totals)
}
fn original_scope(context: &AmendmentContext) -> Result<AmendmentScope> {
    context
        .effective_scope()
        .cloned()
        .or_else(|| {
            context.original.as_ref().map(|p| AmendmentScope {
                binding: p.binding.clone(),
                intent: p.intent.clone(),
                valuation: p.valuation.clone(),
            })
        })
        .ok_or_else(|| invalid("Mixed revision requires the protected original scope."))
}
fn principal(binding: &Value, account: &KrakenAccount) -> Result<Vec<ResourceAmount>> {
    let mut rows = Vec::new();
    let cash: Vec<SpotOrder> =
        serde_json::from_value(binding["mixedSpotMargin"]["cash"]["orders"].clone())
            .map_err(|_| invalid("Original cash exposure quantities are unavailable."))?;
    for order in cash {
        rows.push(ResourceAmount {
            resource: account.resource.clone(),
            amount: Amount {
                asset: if order.buy { order.quote } else { order.base },
                atoms: order.maximum_spend,
            },
        });
    }
    for order in binding["mixedSpotMargin"]["margin"]["margin"]["orders"]
        .as_array()
        .ok_or_else(|| invalid("Original margin exposure quantities are unavailable."))?
    {
        rows.push(ResourceAmount {
            resource: account.resource.clone(),
            amount: Amount {
                asset: serde_json::from_value(
                    order[if order["buy"] == true {
                        "quote"
                    } else {
                        "base"
                    }]
                    .clone(),
                )
                .map_err(|_| invalid("Original margin loan asset is unavailable."))?,
                atoms: serde_json::from_value(order["maximumLoan"].clone())
                    .map_err(|_| invalid("Original full margin loan quantity is unavailable."))?,
            },
        });
    }
    Ok(rows)
}
impl KrakenProfile {
    pub(crate) async fn prepare_mixed_batch_revision(
        &self,
        endpoint: E,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        context: &AmendmentContext,
    ) -> Result<PreparedOperation> {
        let previous = original_scope(context)?;
        let mut family_contexts = Vec::new();
        for family in ["cash", "margin"] {
            let projected = project_family_context(context, family)?;
            let scope = self
                .pinned_original_scope(&projected)
                .await?
                .ok_or_else(|| invalid("Protected original mixed family scope is unavailable."))?;
            family_contexts.push((family, projected, scope));
        }
        let open = self
            .json(
                E::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let native = result(&open)["open"]
            .as_object()
            .ok_or_else(|| invalid("Native mixed order ownership evidence is unavailable."))?;
        let mut targets = Vec::new();
        for (family, _, scope) in &family_contexts {
            let orders = if *family == "cash" {
                &scope.binding["orders"]
            } else {
                &scope.binding["margin"]["orders"]
            };
            for (index, order) in orders
                .as_array()
                .ok_or_else(|| invalid("Native mixed family orders are unavailable."))?
                .iter()
                .enumerate()
            {
                for id in order["orderIds"]
                    .as_array()
                    .ok_or_else(|| {
                        invalid("A mixed revision requires pinned original native IDs.")
                    })?
                    .iter()
                    .filter_map(Value::as_str)
                {
                    if let Some(row) = native.get(id) {
                        let selected = parameters["txid"]
                            .as_str()
                            .is_some_and(|selector| selector == id)
                            || parameters["cl_ord_id"]
                                .as_str()
                                .is_some_and(|selector| order["clientId"] == selector)
                            || parameters["txid"]
                                .as_i64()
                                .is_some_and(|selector| row["userref"].as_i64() == Some(selector));
                        if selected {
                            targets.push((*family, index));
                        }
                    }
                }
            }
        }
        if targets.len() != 1 {
            return Err(invalid(
                "Select exactly one open native member owned by the original mixed batch.",
            ));
        }
        let (family, index) = targets[0];
        let other = if family == "cash" { "margin" } else { "cash" };
        let selected = family_contexts
            .iter()
            .find(|(name, _, _)| *name == family)
            .unwrap();
        let untouched = family_contexts
            .iter()
            .find(|(name, _, _)| *name == other)
            .unwrap();
        let mut scope = selected.2.clone();
        let mut scoped_controls = controls.clone();
        let other_controls: TradingControls = serde_json::from_value(if other == "cash" {
            untouched.2.binding["controls"].clone()
        } else {
            untouched.2.binding["margin"]["controls"].clone()
        })
        .map_err(|_| invalid("Untouched family approval controls are unavailable."))?;
        let global_controls: TradingControls =
            serde_json::from_value(previous.binding["mixedSpotMargin"]["controls"].clone())
                .map_err(|_| invalid("Original mixed global controls are unavailable."))?;
        let mut global_fees = if controls.maximum_fees.is_empty() {
            global_controls.maximum_fees.clone()
        } else {
            controls.maximum_fees.clone()
        };
        if global_fees.keys().ne(global_controls.maximum_fees.keys()) {
            return Err(invalid(
                "A mixed revision must retain every original fee asset and cannot add unrelated currencies.",
            ));
        }
        scoped_controls.maximum_fees.clear();
        for (asset, cap) in &global_fees {
            let global = Quantity::from_decimal(cap, 36)?;
            let other = Quantity::from_decimal(
                other_controls
                    .maximum_fees
                    .get(asset)
                    .map(String::as_str)
                    .unwrap_or("0"),
                36,
            )?;
            if other > global {
                return Err(invalid(
                    "A global fee reduction cannot silently reduce an untouched family ceiling.",
                ));
            }
            scoped_controls
                .maximum_fees
                .insert(asset.clone(), global.checked_sub(&other)?.to_decimal(36));
        }
        if let Some(global) = controls.maximum_spend.as_deref() {
            let mut unchanged = Quantity::zero();
            let mut principal_assets = BTreeSet::new();
            let orders = if other == "cash" {
                &untouched.2.binding["orders"]
            } else {
                &untouched.2.binding["margin"]["orders"]
            };
            for order in orders
                .as_array()
                .ok_or_else(|| invalid("Untouched family exposure is unavailable."))?
            {
                if other == "cash" || order["closing"] == false {
                    principal_assets.insert(
                        serde_json::from_value::<AssetId>(
                            order[if order["buy"] == true {
                                "quote"
                            } else {
                                "base"
                            }]
                            .clone(),
                        )
                        .map_err(|_| invalid("Untouched principal asset identity is missing."))?,
                    );
                    let amount: Quantity = serde_json::from_value(
                        order[if other == "cash" {
                            "maximumSpend"
                        } else {
                            "maximumLoan"
                        }]
                        .clone(),
                    )
                    .map_err(|_| invalid("Untouched quote authority is unavailable."))?;
                    unchanged = unchanged.checked_add(&amount)?;
                }
            }
            let selected_orders = scope.binding[if family == "cash" { "orders" } else { "margin" }]
                .as_array()
                .or_else(|| scope.binding["margin"]["orders"].as_array())
                .ok_or_else(|| invalid("Selected principal scope is missing."))?;
            for order in selected_orders
                .iter()
                .filter(|order| family == "cash" || order["closing"] == false)
            {
                principal_assets.insert(
                    serde_json::from_value::<AssetId>(
                        order[if order["buy"] == true {
                            "quote"
                        } else {
                            "base"
                        }]
                        .clone(),
                    )
                    .map_err(|_| invalid("Selected principal asset identity is missing."))?,
                );
            }
            if principal_assets.len() != 1 {
                return Err(crate::contracts::unsupported_control(
                    endpoint,
                    vec!["/maximumSpend".into()],
                ));
            }
            let global = Quantity::from_decimal(global, 36)?;
            if unchanged > global {
                return Err(invalid(
                    "The whole quote ceiling cannot cover untouched cash and margin authority.",
                ));
            }
            scoped_controls.maximum_spend = Some(global.checked_sub(&unchanged)?.to_decimal(36));
        }
        if family == "cash"
            && ((controls.maximum_loss.is_some()
                && controls.maximum_loss != other_controls.maximum_loss)
                || (controls.maximum_profit.is_some()
                    && controls.maximum_profit != other_controls.maximum_profit))
        {
            return Err(invalid(
                "A cash member revision cannot silently revise the untouched margin family's PnL authority.",
            ));
        }
        if family == "cash" {
            // The global PnL ceilings were checked against the untouched margin
            // family above; they confer no authority on the cash projection.
            scoped_controls.maximum_loss = None;
            scoped_controls.maximum_profit = None;
        }
        let order_count = scope.binding[if family == "cash" { "orders" } else { "margin" }]
            .as_array()
            .map(Vec::len)
            .unwrap_or_else(|| {
                scope.binding["margin"]["orders"]
                    .as_array()
                    .map_or(0, Vec::len)
            });
        let singleton = order_count == 1;
        let mut scoped_input = input.clone();
        for (key, value) in serde_json::to_value(&scoped_controls)
            .map_err(|_| invalid("Projected family controls are invalid."))?
            .as_object()
            .unwrap()
        {
            if value.is_null() {
                scoped_input.as_object_mut().unwrap().remove(key);
            } else {
                scoped_input[key] = value.clone();
            }
        }
        if singleton {
            let mut row = scope.binding["parameters"]["orders"][0].clone();
            row["pair"] = scope.binding["parameters"]["pair"].clone();
            scope.binding["parameters"] = row;
        }
        let mut prepared = if family == "margin" {
            if endpoint == E::KrakenPrivatePostEditOrder {
                self.prepare_margin_legacy_edit(
                    &scoped_input,
                    account,
                    parameters,
                    &scoped_controls,
                    Some(&scope),
                )
                .await?
            } else {
                self.prepare_margin_amendment(
                    &scoped_input,
                    account,
                    parameters,
                    &scoped_controls,
                    Some(&scope),
                )
                .await?
            }
        } else if !singleton {
            self.prepare_spot_batch_revision(
                endpoint,
                &scoped_input,
                account,
                parameters,
                &scoped_controls,
                Some(&scope),
            )
            .await?
        } else if endpoint == E::KrakenPrivatePostEditOrder {
            self.prepare_spot_legacy_edit(
                &scoped_input,
                account,
                parameters,
                &scoped_controls,
                Some(&scope),
            )
            .await?
        } else {
            self.prepare_spot_amendment(
                &scoped_input,
                account,
                parameters,
                &scoped_controls,
                Some(&scope),
            )
            .await?
        };
        let (operation_id, submission, mut changed) = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment {
                    operation_id,
                    submission,
                    revision,
                } => Some((
                    operation_id.clone(),
                    submission.clone(),
                    (**revision).clone(),
                )),
                _ => None,
            })
            .ok_or_else(|| invalid("Selected family revision omitted its exact original owner."))?;
        if singleton {
            let mut row = changed.binding["parameters"].clone();
            row.as_object_mut()
                .ok_or_else(|| invalid("Native revised member is unavailable."))?
                .remove("pair");
            let mut native = selected.2.binding["parameters"].clone();
            native["orders"][0] = row;
            changed.binding["parameters"] = native;
        }
        let mut binding = previous.binding.clone();
        binding["mixedSpotMargin"][family] = changed.binding.clone();
        binding["mixedSpotMargin"][format!("{family}Intent")] = json!(changed.intent);
        binding["mixedSpotMargin"][format!("{family}Valuation")] = json!(changed.valuation);
        let members = binding["mixedSpotMargin"]["members"]
            .as_array()
            .ok_or_else(|| invalid("Original native member map is unavailable."))?;
        let member = members
            .iter()
            .find(|member| member["family"] == family && member["familyIndex"] == index)
            .ok_or_else(|| invalid("Selected native member does not match its family index."))?;
        let native_index = member["index"]
            .as_u64()
            .ok_or_else(|| invalid("Original native batch index is unavailable."))?
            as usize;
        binding["parameters"]["orders"][native_index] =
            changed.binding["parameters"]["orders"][index].clone();
        let mut full = FinancialIntent {
            locks: previous.intent.locks.clone(),
            ..Default::default()
        };
        let ordered_intents = if family == "cash" {
            [&changed.intent, &untouched.2.intent]
        } else {
            [&untouched.2.intent, &changed.intent]
        };
        for intent in ordered_intents {
            full.debits.extend(intent.debits.clone());
            full.maximum_costs.extend(intent.maximum_costs.clone());
            full.expected.extend(intent.expected.clone());
        }
        let before = sum_debits(&previous.intent.debits)?;
        let after = sum_debits(&full.debits)?;
        let mut own = prepared.intent.clone();
        own.debits.clear();
        for ((resource, asset), quantity) in after {
            let old = before
                .get(&(resource.clone(), asset.clone()))
                .cloned()
                .unwrap_or_default();
            if quantity > old {
                own.debits.push(ResourceAmount {
                    resource,
                    amount: Amount {
                        asset,
                        atoms: quantity.checked_sub(&old)?,
                    },
                });
            }
        }
        own.locks = full.locks.clone();
        own.expected.clear();
        let mut retained_global = controls.clone();
        retained_global.maximum_fees = std::mem::take(&mut global_fees);
        if retained_global.maximum_loss.is_none() {
            retained_global.maximum_loss = global_controls.maximum_loss;
        }
        if retained_global.maximum_profit.is_none() {
            retained_global.maximum_profit = global_controls.maximum_profit;
        }
        binding["mixedSpotMargin"]["controls"] = json!(retained_global);
        binding
            .as_object_mut()
            .unwrap()
            .remove(fin_domain::value_bounds::KEY);
        let valuation = self
            .financial_valuation(&principal(&binding, account)?, &full.maximum_costs)
            .await?;
        own.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id,
            submission,
            revision: Box::new(AmendmentScope {
                binding,
                valuation: Some(valuation),
                intent: full,
            }),
        });
        prepared.intent = own;
        prepared.input = input.clone();
        prepared.binding["amendmentPreviousScope"] = json!(previous);
        prepared.binding["mixedAmendmentContext"] = json!(context);
        prepared.binding["amendment"]["batchOrderIndex"] = json!(native_index);
        prepared.summary = format!(
            "Revise original mixed Kraken {family} member {}; preserve the untouched family, whole fee authority and original fill owner.",
            native_index + 1
        );
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}
