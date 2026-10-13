//! Cumulative accounting before a financial action reaches terminal status.
use super::*;
#[cfg(test)]
use fin_domain::{AssetId, Quantity};

fn invalid(message: &str) -> FinError {
    FinError::new(
        "INVALID_PARTIAL_PROOF",
        "reconciliation",
        message,
        "Preserve the original submission and reservations; reconcile immutable complete receipt evidence.",
    )
}

pub(super) fn constraint_matches_progress(
    constraint: &EffectConstraint,
    effect: &FinancialEffect,
    partial: bool,
) -> Result<bool> {
    if let (
        EffectConstraint::CompoundSubmissionMutations { children: scope },
        FinancialEffect::CompoundSubmissionMutations { children: proof },
    ) = (constraint, effect)
    {
        return Ok(super::compound_mutations::matches(scope, proof, partial));
    }
    if let (
        EffectConstraint::GroupedSubmissionAmendments { group },
        FinancialEffect::GroupedSubmissionAmendments { proof },
    ) = (constraint, effect)
    {
        return Ok(grouped_amendments::matches(group, proof, partial));
    }
    if !partial {
        return constraint_matches(constraint, effect);
    }
    let mut relaxed = constraint.clone();
    match &mut relaxed {
        EffectConstraint::Conversion { minimum_output, .. } => {
            *minimum_output = fin_domain::Quantity::from_u64(0)
        }
        EffectConstraint::Transfer { minimum_credit, .. }
        | EffectConstraint::OriginMovement { minimum_credit, .. }
        | EffectConstraint::Credit { minimum_credit, .. } => {
            *minimum_credit = fin_domain::Quantity::from_u64(0)
        }
        EffectConstraint::DerivativePosition {
            minimum_delta,
            maximum_delta,
            ..
        }
        | EffectConstraint::MarginPosition {
            minimum_delta,
            maximum_delta,
            ..
        }
        | EffectConstraint::Settlement {
            minimum_delta,
            maximum_delta,
            ..
        } => {
            if minimum_delta.integer() > BigInt::zero() {
                *minimum_delta =
                    fin_domain::SignedQuantity::positive(&fin_domain::Quantity::from_u64(0));
            }
            if maximum_delta.integer() < BigInt::zero() {
                *maximum_delta =
                    fin_domain::SignedQuantity::positive(&fin_domain::Quantity::from_u64(0));
            }
        }
        _ => {}
    }
    constraint_matches(&relaxed, effect)
}

/// A ledger snapshot may grow; it may never retract or reverse proven amounts.
pub(super) fn validate_growth(old: Option<&VerifiedEffects>, new: &VerifiedEffects) -> Result<()> {
    let Some(old) = old else {
        return Ok(());
    };
    conversion_sequences::validate_growth(&old.effects, &new.effects)?;
    for old_effect in &old.effects {
        if let FinancialEffect::CompoundSubmissionMutations {
            children: old_children,
        } = old_effect
        {
            let next_children = new
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    FinancialEffect::CompoundSubmissionMutations { children } => Some(children),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            if old_children.iter().any(|old| !next_children.contains(&old)) {
                return Err(invalid(
                    "Cumulative proof changed or omitted an immutable native child outcome.",
                ));
            }
        }
    }
    for effect in &old.effects {
        if let FinancialEffect::GroupedSubmissionAmendments { proof: old_proof } = effect {
            let Some(new_proof) = new.effects.iter().find_map(|effect| match effect {
                FinancialEffect::GroupedSubmissionAmendments { proof }
                    if proof.operation_id == old_proof.operation_id =>
                {
                    Some(proof)
                }
                _ => None,
            }) else {
                return Err(invalid("A grouped child proof disappeared."));
            };
            if old_proof.submission != new_proof.submission
                || old_proof
                    .children
                    .iter()
                    .any(|old| !new_proof.children.contains(old))
            {
                return Err(invalid("An immutable grouped native outcome changed."));
            }
        }
    }
    let previous = quantities(&old.effects)?;
    let next = quantities(&new.effects)?;
    for (key, amount) in previous {
        let new_amount = next
            .get(&key)
            .ok_or_else(|| invalid("Cumulative proof omitted a proven economic effect."))?;
        if amount != num_bigint::BigInt::zero()
            && ((amount.sign() != new_amount.sign()) || new_amount.magnitude() < amount.magnitude())
        {
            return Err(invalid(
                "Cumulative proof retracted or reversed a proven amount.",
            ));
        }
    }
    Ok(())
}

fn quantities(effects: &[FinancialEffect]) -> Result<BTreeMap<String, num_bigint::BigInt>> {
    let mut result = BTreeMap::new();
    for effect in effects {
        // Realized PnL and funding are signed net aggregates across immutable
        // bookings. Later opposite bookings may reduce or reverse the net.
        // Gross attributable losses remain separately capped typed Costs.
        if matches!(
            effect,
            FinancialEffect::Settlement { .. }
                | FinancialEffect::ConversionSequence { .. }
                | FinancialEffect::CompoundSubmissionMutations { .. }
                | FinancialEffect::GroupedSubmissionAmendments { .. }
        ) {
            continue;
        }
        let mut value = to_value(effect)?;
        // Anchors are observations of resulting positions, not added quantities.
        if let Some(object) = value.as_object_mut() {
            object.remove("anchor");
            // Gross exposure legs preserve direction independently. A later
            // closing fill may lower the net position, but cannot erase an
            // earlier proved opening leg or reuse its approved movement budget.
            if let FinancialEffect::PositionChange { delta, .. }
            | FinancialEffect::DebtChange { delta, .. } = effect
            {
                if delta.magnitude().is_zero() {
                    continue;
                }
                object.insert(
                    "_growth_direction".into(),
                    json!(if delta.is_negative() {
                        "negative"
                    } else {
                        "positive"
                    }),
                );
            }
        }
        let mut amounts = Vec::new();
        extract(&mut value, "", &mut amounts)?;
        let shape = binding_hash(&value)?;
        for (path, amount) in amounts {
            if amount != BigInt::zero() {
                *result.entry(format!("{shape}:{path}")).or_default() += amount;
            }
        }
    }
    Ok(result)
}

pub(super) fn failed_execution_matches(
    expected: &[EffectConstraint],
    effect: &FinancialEffect,
) -> Result<bool> {
    if !matches!(
        effect,
        FinancialEffect::Conversion { .. }
            | FinancialEffect::PositionChange { .. }
            | FinancialEffect::DebtChange { .. }
            | FinancialEffect::Settlement { .. }
    ) {
        return Ok(false);
    }
    for constraint in expected {
        if constraint_matches_progress(constraint, effect, true)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn extract(
    value: &mut Value,
    path: &str,
    quantities: &mut Vec<(String, num_bigint::BigInt)>,
) -> Result<()> {
    if let Some(object) = value.as_object_mut() {
        for (key, value) in object {
            let path = format!("{path}/{key}");
            if matches!(key.as_str(), "atoms" | "delta") {
                let amount = value
                    .as_str()
                    .ok_or_else(|| invalid("Proof quantity is not exact integer atoms."))?
                    .parse::<num_bigint::BigInt>()
                    .map_err(|_| invalid("Proof contains invalid integer atoms."))?;
                quantities.push((path, amount));
                *value = json!("0");
            } else {
                extract(value, &path, quantities)?;
            }
        }
    } else if let Some(array) = value.as_array_mut() {
        for (index, value) in array.iter_mut().enumerate() {
            extract(value, &format!("{path}/{index}"), quantities)?;
        }
    }
    Ok(())
}

pub(super) fn validate_pending(record: &OperationRecord, verified: &VerifiedEffects) -> Result<()> {
    validate_pending_with_scope(record, verified, None)
}
pub(super) fn validate_pending_with_scope(
    record: &OperationRecord,
    verified: &VerifiedEffects,
    scope: Option<&FinancialIntent>,
) -> Result<()> {
    if record.run_id.is_some() {
        return Err(invalid(
            "Historical run accounting does not accept partial settlement snapshots.",
        ));
    }
    let events = verified
        .evidence
        .get("proofEvents")
        .and_then(Value::as_array)
        .filter(|events| !events.is_empty())
        .ok_or_else(|| invalid("A partial proof requires immutable receipt event identities."))?;
    let mut ids = BTreeSet::new();
    for event in events {
        let id = event
            .get("eventId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| invalid("A receipt event lacks its stable identity."))?;
        if !ids.insert(id) {
            return Err(invalid(
                "Duplicate receipt event identity in partial proof.",
            ));
        }
    }
    for effect in &verified.effects {
        if !matches!(
            effect,
            FinancialEffect::Conversion { .. }
                | FinancialEffect::ConversionSequence { .. }
                | FinancialEffect::Cost { .. }
                | FinancialEffect::PositionChange { .. }
                | FinancialEffect::DebtChange { .. }
                | FinancialEffect::Settlement { .. }
                | FinancialEffect::CompoundSubmissionMutations { .. }
                | FinancialEffect::GroupedSubmissionAmendments { .. }
                | FinancialEffect::CashDebit { .. }
                | FinancialEffect::UsableCredit { .. }
        ) {
            return Err(invalid(
                "This effect requires terminal settlement evidence.",
            ));
        }
    }
    validate_growth(record.effects.as_ref(), verified)?;
    // Retain the original approval binding. Relax only completion constraints,
    // using a separate validator argument rather than mutating the bound intent.
    validate_effects_with_scope(record, verified, &Status::Pending, true, scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_proofs_cannot_retract_reverse_or_change_asset_identity() {
        let amount = |atoms| Amount {
            asset: AssetId {
                network: "synthetic".into(),
                reference: "A".into(),
                decimals: 18,
            },
            atoms: Quantity::from_u64(atoms),
        };
        let effects = |atoms| {
            vec![FinancialEffect::CashDebit {
                resource: ResourceId("account".into()),
                amount: amount(atoms),
            }]
        };
        let old = quantities(&effects(2)).unwrap();
        let new = quantities(&effects(3)).unwrap();
        assert_eq!(
            old.keys().collect::<Vec<_>>(),
            new.keys().collect::<Vec<_>>()
        );
        assert!(old.values().next().unwrap() < new.values().next().unwrap());
    }
    fn exposure(delta: &str, debt: bool) -> FinancialEffect {
        let asset = AssetId {
            network: "synthetic".into(),
            reference: "EXACT".into(),
            decimals: 0,
        };
        let delta = fin_domain::SignedQuantity::new(delta).unwrap();
        if debt {
            FinancialEffect::DebtChange {
                resource: ResourceId("account".into()),
                debt: "loan".into(),
                asset,
                delta,
                anchor: None,
            }
        } else {
            FinancialEffect::PositionChange {
                resource: ResourceId("account".into()),
                position: "market".into(),
                asset,
                delta,
                scaled_receipt: None,
                anchor: None,
            }
        }
    }
    fn bundle(effects: Vec<FinancialEffect>) -> VerifiedEffects {
        VerifiedEffects {
            evidence_version: 1,
            submission: Submission {
                kind: "synthetic".into(),
                reference: "original".into(),
                data: json!({}),
            },
            effects,
            evidence: json!({}),
            observed_at: now(),
            proof_digest: String::new(),
        }
    }
    #[test]
    fn gross_exposure_legs_grow_independently_when_net_position_shrinks() {
        for debt in [false, true] {
            let old = bundle(vec![exposure("2", debt)]);
            let next = bundle(vec![exposure("2", debt), exposure("-1", debt)]);
            validate_growth(Some(&old), &next).unwrap();
            let zero = bundle(vec![exposure("0", debt)]);
            validate_growth(Some(&zero), &next).unwrap();
            for effects in [
                vec![exposure("1", debt)],
                vec![exposure("-3", debt)],
                vec![exposure("1", debt), exposure("-5", debt)],
            ] {
                assert_eq!(
                    validate_growth(Some(&old), &bundle(effects))
                        .unwrap_err()
                        .code,
                    "INVALID_PARTIAL_PROOF"
                );
            }
            let close = bundle(vec![exposure("2", debt), exposure("-2", debt)]);
            validate_growth(Some(&next), &close).unwrap();
            assert_eq!(
                validate_growth(Some(&next), &old).unwrap_err().code,
                "INVALID_PARTIAL_PROOF"
            );
        }
    }
}
