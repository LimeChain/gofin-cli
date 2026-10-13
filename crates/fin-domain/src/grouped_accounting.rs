//! Pure conservative accounting shared by adapters and protected validators.
use super::{GroupedAmendmentGroup, compose_binding};
use crate::{
    CostBound, FinError, FinancialIntent, Quantity, ResourceAmount, Result,
    amendments::AmendmentScope,
};
use num_bigint::{BigInt, BigUint};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn invalid(message: &str) -> FinError {
    FinError::new(
        "GROUPED_AMENDMENT_INVALID",
        "validation",
        message,
        "Review every exact member change and preserve the original financial owner.",
    )
}
fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| invalid(&e.to_string()))
}
fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| invalid(&e.to_string()))
}
fn resource_asset_key(row: &ResourceAmount) -> Result<String> {
    serde_json::to_string(&(&row.resource, &row.amount.asset)).map_err(|e| invalid(&e.to_string()))
}
fn cost_key(
    location: &crate::AccountingLocation,
    asset: &crate::AssetId,
    treatment: &crate::CostTreatment,
) -> Result<String> {
    serde_json::to_string(&(location, asset, treatment)).map_err(|e| invalid(&e.to_string()))
}
fn aggregate_resource_amounts(rows: &[ResourceAmount]) -> Result<BTreeMap<String, BigUint>> {
    let mut result = BTreeMap::new();
    for row in rows {
        *result.entry(resource_asset_key(row)?).or_default() += row.amount.atoms.integer();
    }
    Ok(result)
}
fn aggregate_cost_bounds(rows: &[CostBound]) -> Result<BTreeMap<String, BigUint>> {
    let mut result = BTreeMap::new();
    for row in rows {
        *result
            .entry(cost_key(&row.location, &row.amount.asset, &row.treatment)?)
            .or_default() += row.amount.atoms.integer();
    }
    Ok(result)
}
fn delta(
    before: &BTreeMap<String, BigUint>,
    after: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    after
        .iter()
        .filter_map(|(k, n)| {
            let old = before.get(k).cloned().unwrap_or_default();
            (n > &old).then(|| (k.clone(), n - old))
        })
        .collect()
}
fn bound(name: &str) -> bool {
    matches!(
        name,
        "maximum_input"
            | "minimum_output"
            | "maximum_debit"
            | "minimum_credit"
            | "minimum_delta"
            | "maximum_delta"
            | "maximum_change"
            | "maximum_quantity"
            | "minimum_quantity"
            | "maximum_amount"
            | "minimum_amount"
    )
}
fn expected_envelope(original: &Value, children: &[Value]) -> Result<Value> {
    match original {
        Value::Object(object) => {
            if object.get("kind") == Some(&json!("settlement"))
                && children.windows(2).any(|rows| rows[0] != rows[1])
            {
                return Err(invalid(
                    "Same-owner children must share identical global settlement ceilings.",
                ));
            }
            let mut result = object.clone();
            for (name, before) in object {
                let values = children
                    .iter()
                    .map(|child| {
                        child
                            .get(name)
                            .cloned()
                            .ok_or_else(|| invalid("A member changed effect shape."))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let value = if bound(name) {
                    let old: BigInt = before
                        .as_str()
                        .ok_or_else(|| invalid("A financial bound lacks exact atoms."))?
                        .parse()
                        .map_err(|_| invalid("A financial bound is invalid."))?;
                    let numbers = values
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .ok_or_else(|| invalid("A bound changed representation."))?
                                .parse::<BigInt>()
                                .map_err(|_| invalid("A bound is invalid."))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let mut total = old.clone();
                    for number in if object.get("kind") == Some(&json!("settlement")) {
                        numbers.into_iter().take(1).collect()
                    } else {
                        numbers
                    } {
                        if name.starts_with("minimum") {
                            if name == "minimum_delta" {
                                if number < old {
                                    total += number - &old;
                                }
                            } else {
                                total = std::cmp::min(total, number);
                            }
                        } else if number > old {
                            total += number - &old;
                        }
                    }
                    json!(total.to_string())
                } else {
                    expected_envelope(before, &values)?
                };
                result.insert(name.clone(), value);
            }
            if children
                .iter()
                .any(|child| child.as_object().is_none_or(|o| o.len() != object.len()))
            {
                return Err(invalid("A member introduced effect identity fields."));
            }
            Ok(Value::Object(result))
        }
        Value::Array(rows) => {
            if children
                .iter()
                .any(|c| c.as_array().is_none_or(|a| a.len() != rows.len()))
            {
                return Err(invalid("A member changed economic effect count."));
            }
            rows.iter()
                .enumerate()
                .map(|(i, row)| {
                    expected_envelope(
                        row,
                        &children.iter().map(|c| c[i].clone()).collect::<Vec<_>>(),
                    )
                })
                .collect::<Result<Vec<_>>>()
                .map(Value::Array)
        }
        _ if children.iter().all(|c| c == original) => Ok(original.clone()),
        _ => Err(invalid(
            "A member changed economic assets or effect identity.",
        )),
    }
}
/// Compute a no-net accounting scope, independently from adapter-declared
/// economics. Decreases cannot finance sibling increases; global fees count once.
pub fn derive_accounting_scope(
    group: &GroupedAmendmentGroup,
    tags: &BTreeSet<String>,
) -> Result<AmendmentScope> {
    let selected = group
        .children
        .iter()
        .filter(|c| tags.contains(&c.child_tag) && c.mutation.is_amend())
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok((*group.original_scope).clone());
    }
    let original = &group.original_scope.intent;
    let scopes = selected.iter().map(|c| &c.scope.intent).collect::<Vec<_>>();
    let amendments = group
        .children
        .iter()
        .filter(|c| c.mutation.is_amend())
        .collect::<Vec<_>>();
    if amendments
        .windows(2)
        .any(|c| c[0].scope.intent.maximum_costs != c[1].scope.intent.maximum_costs)
    {
        return Err(invalid(
            "Same-owner children must declare identical full-original fee ceilings.",
        ));
    }
    let before = aggregate_resource_amounts(&original.debits)?;
    let mut debit_caps = before.clone();
    for scope in &scopes {
        let after = aggregate_resource_amounts(&scope.debits)?;
        if before.keys().ne(after.keys()) {
            return Err(invalid("A grouped revision changed debit asset identity."));
        }
        for (k, n) in delta(&before, &after) {
            *debit_caps.entry(k).or_default() += n;
        }
        if scope.locks != original.locks {
            return Err(invalid("A grouped revision changed custody locks."));
        }
    }
    let costs = aggregate_cost_bounds(&original.maximum_costs)?;
    let shared = aggregate_cost_bounds(&scopes[0].maximum_costs)?;
    if costs.keys().ne(shared.keys()) {
        return Err(invalid(
            "A grouped revision changed fee identity or treatment.",
        ));
    }
    let mut debits = Vec::new();
    let mut seen = BTreeSet::new();
    for row in &original.debits {
        let key = resource_asset_key(row)?;
        if seen.insert(key.clone()) {
            let mut row = row.clone();
            row.amount.atoms = Quantity::new(debit_caps[&key].to_string())?;
            debits.push(row);
        }
    }
    let mut maximum_costs = Vec::new();
    let mut seen = BTreeSet::new();
    for row in &original.maximum_costs {
        let key = cost_key(&row.location, &row.amount.asset, &row.treatment)?;
        if seen.insert(key.clone()) {
            let mut row = row.clone();
            row.amount.atoms = Quantity::new(
                std::cmp::max(costs[&key].clone(), shared[&key].clone()).to_string(),
            )?;
            maximum_costs.push(row);
        }
    }
    let expected = from_value(expected_envelope(
        &to_value(&original.expected)?,
        &scopes
            .iter()
            .map(|s| to_value(&s.expected))
            .collect::<Result<Vec<_>>>()?,
    )?)?;
    let mut valuation = group
        .approved_envelope
        .valuation
        .clone()
        .ok_or_else(|| invalid("The grouped valuation is missing."))?;
    let old = group
        .original_scope
        .valuation
        .as_ref()
        .ok_or_else(|| invalid("The original valuation is missing."))?;
    let mut movement = old.movement_usd_micros.integer();
    let mut allocation = old.additional_allocation_usd_micros.integer();
    let mut fee = old.maximum_fee_usd_micros.integer();
    for child in &selected {
        let new = child
            .scope
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("A member valuation is missing."))?;
        if new.movement_usd_micros > old.movement_usd_micros {
            movement += new.movement_usd_micros.integer() - old.movement_usd_micros.integer();
        }
        if new.additional_allocation_usd_micros > old.additional_allocation_usd_micros {
            allocation += new.additional_allocation_usd_micros.integer()
                - old.additional_allocation_usd_micros.integer();
        }
        fee = std::cmp::max(fee, new.maximum_fee_usd_micros.integer());
    }
    valuation.movement_usd_micros = Quantity::new(movement.to_string())?;
    valuation.additional_allocation_usd_micros = Quantity::new(allocation.to_string())?;
    valuation.maximum_fee_usd_micros = Quantity::new(fee.to_string())?;
    Ok(AmendmentScope {
        intent: FinancialIntent {
            debits,
            maximum_costs,
            expected,
            locks: original.locks.clone(),
        },
        binding: compose_binding(group, tags.iter().map(String::as_str))?,
        valuation: Some(valuation),
    })
}
