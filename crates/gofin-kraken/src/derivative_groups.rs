//! One native contract position chain may contain several approved batch orders.
use crate::{observation::exact, profile::error};
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{
    EffectConstraint, FinancialEffect, FinancialIntent, PositionKind, PreparedOperation, Quantity,
    Result, SignedQuantity,
};
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn signed_decimal(value: &Value) -> Result<BigInt> {
    let text = exact(value)?;
    let amount = Quantity::from_decimal(text.trim_start_matches('-'), 36)?;
    Ok(if text.starts_with('-') {
        -BigInt::from(amount.integer())
    } else {
        BigInt::from(amount.integer())
    })
}

pub(crate) fn collate(
    prepared: &[PreparedOperation],
    intent: &mut FinancialIntent,
) -> Result<Vec<Value>> {
    let mut groups: BTreeMap<String, Vec<&PreparedOperation>> = BTreeMap::new();
    for scope in prepared
        .iter()
        .filter(|scope| scope.binding["derivative"].is_object())
    {
        groups
            .entry(
                scope.binding["derivative"]["symbol"]
                    .as_str()
                    .unwrap()
                    .into(),
            )
            .or_default()
            .push(scope);
    }
    let mut constraints = Vec::new();
    let mut baselines = Vec::new();
    for (symbol, scopes) in groups {
        let first = &scopes[0].binding["derivative"];
        let before = SignedQuantity::new(first["positionBefore"].as_str().unwrap())?.integer();
        let mut buys = Quantity::zero();
        let mut sells = Quantity::zero();
        for scope in &scopes {
            let order = &scope.binding["derivative"];
            if order["positionBefore"] != first["positionBefore"]
                || order["accountUid"] != first["accountUid"]
                || order["positionLimit"] != first["positionLimit"]
            {
                return Err(error(
                    "KRAKEN_BATCH_BASELINE_CHANGED",
                    "Contract baseline or authenticated account changed while preparing this batch.",
                ));
            }
            let size: Quantity = serde_json::from_value(order["size"].clone())
                .map_err(|_| error("KRAKEN_SUBMISSION_INVALID", "Batch size unavailable."))?;
            if scope.binding["parameters"]["side"] == "buy" {
                buys = buys.checked_add(&size)?;
            } else {
                sells = sells.checked_add(&size)?;
            }
        }
        let limit: Quantity =
            serde_json::from_value(first["positionLimit"].clone()).map_err(|_| {
                error(
                    "KRAKEN_EXPOSURE_LIMIT",
                    "Published contract exposure limit unavailable.",
                )
            })?;
        if (before.clone() + BigInt::from(buys.integer())).magnitude() > &limit.integer()
            || (before.clone() - BigInt::from(sells.integer())).magnitude() > &limit.integer()
        {
            return Err(error(
                "KRAKEN_EXPOSURE_LIMIT",
                "The entire batch worst-case one-sided contract exposure exceeds the published native maximum.",
            ));
        }
        let total = buys.checked_add(&sells)?;
        for constraint in &scopes[0].intent.expected {
            if let EffectConstraint::DerivativePosition {
                resource,
                position,
                asset,
                position_kind,
                ..
            } = constraint
            {
                let positive = if *position_kind == PositionKind::DerivativeLong {
                    &buys
                } else {
                    &sells
                };
                let negative = if *position_kind == PositionKind::DerivativeLong {
                    &sells
                } else {
                    &buys
                };
                let ranges = if !buys.is_zero() && !sells.is_zero() {
                    vec![
                        (
                            SignedQuantity::positive(&Quantity::zero()),
                            SignedQuantity::positive(positive),
                        ),
                        (
                            SignedQuantity::negative(negative),
                            SignedQuantity::positive(&Quantity::zero()),
                        ),
                    ]
                } else {
                    vec![(
                        SignedQuantity::negative(&total),
                        SignedQuantity::positive(&total),
                    )]
                };
                for (minimum_delta, maximum_delta) in ranges {
                    constraints.push(EffectConstraint::DerivativePosition {
                        resource: resource.clone(),
                        position: position.clone(),
                        asset: asset.clone(),
                        position_kind: position_kind.clone(),
                        minimum_delta,
                        maximum_delta,
                    });
                }
            }
        }
        let anchors = scopes[0]
            .binding
            .product_value(gofin_domain::BASELINE_ANCHORS_KEY)?
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_BATCH_BASELINE_CHANGED",
                    "Native contract anchors missing.",
                )
            })?;
        baselines.extend(anchors.iter().cloned());
        let _ = symbol;
    }
    intent
        .expected
        .retain(|e| !matches!(e, EffectConstraint::DerivativePosition { .. }));
    intent.expected.extend(constraints);
    Ok(baselines)
}

pub(crate) fn verify_and_aggregate(
    binding: &Value,
    events: &[Value],
    effects: Vec<FinancialEffect>,
) -> Result<Vec<FinancialEffect>> {
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for scope in binding["derivativeBatch"]
        .as_array()
        .ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original batch scopes unavailable.",
            )
        })?
        .iter()
        .filter(|s| s["derivative"].is_object())
    {
        groups
            .entry(scope["derivative"]["symbol"].as_str().unwrap().into())
            .or_default()
            .push(scope);
    }
    let mut final_positions: BTreeMap<String, BigInt> = BTreeMap::new();
    for (symbol, scopes) in &groups {
        if scopes.len() < 2 {
            continue;
        }
        let clients = scopes
            .iter()
            .map(|s| s["derivative"]["clientId"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let mut chain = events
            .iter()
            .filter(|e| {
                e["execution"]["order"]["tradeable"] == *symbol
                    && e["execution"]["order"]["clientId"]
                        .as_str()
                        .is_some_and(|client| clients.contains(client))
            })
            .collect::<Vec<_>>();
        chain.sort_by_key(|e| {
            (
                e["execution"]["timestamp"].as_i64().unwrap_or(i64::MIN),
                e["eventId"].as_str().unwrap_or(""),
            )
        });
        let mut position =
            SignedQuantity::new(scopes[0]["derivative"]["positionBefore"].as_str().unwrap())?
                .integer();
        let mut identities = BTreeSet::new();
        for event in chain {
            if event["execution"]["timestamp"].as_i64().is_none()
                || !identities.insert(event["eventId"].as_str().ok_or_else(|| {
                    error(
                        "KRAKEN_EXECUTION_PROOF_PENDING",
                        "Native group execution identity missing.",
                    )
                })?)
            {
                return Err(error(
                    "KRAKEN_EXECUTION_DUPLICATE",
                    "Grouped contract fills require unique timestamped execution identities.",
                ));
            }
            let update = &event["positionUpdate"];
            if signed_decimal(&update["oldPosition"])? != position {
                return Err(error(
                    "KRAKEN_POSITION_CHAIN_CHANGED",
                    "Owned batch fills do not form one contiguous native contract chain from the approved baseline; unrelated activity cannot be adopted.",
                ));
            }
            position = signed_decimal(&update["newPosition"])?;
        }
        final_positions.insert(symbol.clone(), position);
    }
    let observed_at = gofin_domain::now();
    let mut positions: BTreeMap<String, FinancialEffect> = BTreeMap::new();
    let mut other = Vec::new();
    for mut effect in effects {
        if let FinancialEffect::PositionChange {
            resource,
            position,
            asset,
            delta,
            anchor: Some(anchor),
            ..
        } = &mut effect
            && let Some(final_position) = final_positions.get(position)
        {
            let value = match anchor.position_kind {
                PositionKind::DerivativeLong => {
                    if final_position.is_positive() {
                        final_position.clone()
                    } else {
                        BigInt::zero()
                    }
                }
                PositionKind::DerivativeShort => {
                    if final_position.is_negative() {
                        -final_position.clone()
                    } else {
                        BigInt::zero()
                    }
                }
                _ => {
                    return Err(error(
                        "KRAKEN_POSITION_IDENTITY_MISMATCH",
                        "A derivative group produced a non-contract anchor.",
                    ));
                }
            };
            anchor.principal.atoms = Quantity::new(value.to_string())?;
            anchor.observed_at = observed_at.clone();
            anchor.evidence = json!({"approvedBatchBaseline":binding.product_value(gofin_domain::BASELINE_ANCHORS_KEY)?,"ownedExecutionChain":events});
            let key = gofin_domain::binding_hash(&json!([
                resource,
                position,
                asset,
                anchor.position_kind,
                delta.integer().is_negative()
            ]))?;
            if let Some(FinancialEffect::PositionChange {
                delta: old,
                anchor: old_anchor,
                ..
            }) = positions.get_mut(&key)
            {
                *old = SignedQuantity::new((old.integer() + delta.integer()).to_string())?;
                *old_anchor = Some(anchor.clone());
            } else {
                positions.insert(key, effect);
            }
            continue;
        }
        other.push(effect);
    }
    let mut grouped_effects = positions.into_values().collect::<Vec<_>>();
    // Both approved direction buckets are explicit, including a direction without a fill.
    // Emit nonzero gross legs first so a zero does not match the other signed range.
    for (symbol, scopes) in &groups {
        let opposite = scopes.iter().any(|s| s["parameters"]["side"] == "buy")
            && scopes.iter().any(|s| s["parameters"]["side"] == "sell");
        if !opposite {
            continue;
        }
        for kind in [PositionKind::DerivativeLong, PositionKind::DerivativeShort] {
            let matching = grouped_effects
                .iter()
                .filter(|effect| {
                    matches!(effect,
                FinancialEffect::PositionChange {position, anchor:Some(anchor), ..}
                if position == symbol && anchor.position_kind == kind)
                })
                .cloned()
                .collect::<Vec<_>>();
            let nonzero = matching
                .iter()
                .filter(|e| {
                    matches!(e,
                FinancialEffect::PositionChange {delta,..} if !delta.is_zero())
                })
                .count();
            let target = 2usize;
            let missing = target.saturating_sub(matching.len());
            for _ in 0..missing {
                let mut zero = matching
                    .first()
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_POSITION_PROOF_PENDING",
                            "Batch direction anchor unavailable.",
                        )
                    })?
                    .clone();
                if let FinancialEffect::PositionChange { delta, .. } = &mut zero {
                    *delta = SignedQuantity::positive(&Quantity::zero());
                }
                grouped_effects.push(zero);
            }
            // A source order can emit a zero along with the opposite nonzero bucket;
            // retain exactly the two approved cumulative direction buckets.
            if nonzero == 2 {
                grouped_effects.retain(|e| !matches!(e, FinancialEffect::PositionChange {position,delta,anchor:Some(anchor),..}
                    if position == symbol && anchor.position_kind == kind && delta.is_zero()));
            }
        }
    }
    grouped_effects.sort_by_key(
        |e| matches!(e, FinancialEffect::PositionChange {delta,..} if delta.is_zero()),
    );
    other.extend(grouped_effects);
    Ok(other)
}
