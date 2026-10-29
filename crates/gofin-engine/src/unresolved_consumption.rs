//! Costs can be proved before a policy violation has a settleable execution.
use super::*;
use gofin_domain::unresolved_consumption::{ConsumptionReason, ConsumptionScope, digest, read};
fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "INVALID_UNRESOLVED_CONSUMPTION",
        "reconciliation",
        message,
        "Preserve the original submission and reservations; recover independently correlated native cost evidence.",
    )
}
fn totals(costs: &[FinancialEffect]) -> Result<BTreeMap<String, (CostBound, BigUint)>> {
    let mut result = BTreeMap::new();
    for effect in costs {
        let FinancialEffect::Cost {
            location,
            amount,
            category,
            treatment,
        } = effect
        else {
            return Err(invalid(
                "Unresolved consumption grants no cash, position, principal or credit authority.",
            ));
        };
        if category.is_empty() || category.len() > 128 || amount.atoms.is_zero() {
            return Err(invalid(
                "Each proved consumed cost requires positive exact units and an attributable category.",
            ));
        }
        let key = cost_key(location, &amount.asset, treatment)?;
        let row = result.entry(key).or_insert_with(|| {
            (
                CostBound {
                    location: location.clone(),
                    amount: amount.clone(),
                    treatment: treatment.clone(),
                },
                BigUint::zero(),
            )
        });
        row.1 += amount.atoms.integer();
    }
    Ok(result)
}
impl Engine {
    pub(super) fn commit_unresolved_cost_consumption(
        &self,
        record: &OperationRecord,
        reconciliation: &Reconciliation,
        intent: &FinancialIntent,
    ) -> Result<Option<OperationRecord>> {
        let Some(mut proof) = read(&reconciliation.evidence)? else {
            return Ok(None);
        };
        if reconciliation.status != Status::Unknown
            || (reconciliation.verified.is_some()
                && proof.scope != ConsumptionScope::NewMutationFinancialScope)
            || record.status.terminal()
            || proof.evidence_version != 1
            || record.submission.as_ref() != Some(&proof.submission)
            || proof.costs.is_empty()
            || digest(&proof)? != proof.proof_digest
        {
            return Err(invalid(
                "Consumption proof must retain one unresolved immutable submission without settleable effects.",
            ));
        }
        validate_observation(
            &proof.observed_at,
            Utc::now(),
            "INVALID_UNRESOLVED_CONSUMPTION",
        )?;
        if parse_time(&proof.observed_at)?
            < parse_time(&record.created_at)? - chrono::Duration::seconds(5)
        {
            return Err(invalid(
                "Cost evidence predates this immutable financial attempt.",
            ));
        }
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| invalid("Original cost valuation is missing."))?;
        let scoped;
        let (intent, valuation) = if proof.scope == ConsumptionScope::NewMutationFinancialScope {
            if grouped_amendments::groups(&prepared.intent)?.is_empty()
                && compound_mutations::children(&prepared.intent)?.is_none()
            {
                return Err(invalid(
                    "A new mutation financial scope requires protected original-owner mutation contracts.",
                ));
            }
            scoped = serde_json::from_value::<gofin_domain::amendments::AmendmentScope>(
                prepared.binding["mutationNewWriteScope"].clone(),
            )
            .map_err(|_| {
                invalid("The protected new-write financial partition is missing or malformed.")
            })?;
            let ordinary = prepared
                .intent
                .expected
                .iter()
                .filter(|effect| {
                    !matches!(
                        effect,
                        EffectConstraint::GroupedSubmissionAmendments { .. }
                            | EffectConstraint::CompoundSubmissionMutations { .. }
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            if scoped.intent.expected != ordinary {
                return Err(invalid(
                    "The consumed new-write partition differs from the protected financial remainder.",
                ));
            }
            (
                &scoped.intent,
                scoped
                    .valuation
                    .as_ref()
                    .ok_or_else(|| invalid("New-write cost valuation is missing."))?,
            )
        } else {
            (
                intent,
                prepared
                    .valuation
                    .as_ref()
                    .ok_or_else(|| invalid("Original exact cost prices are missing."))?,
            )
        };
        let observed = totals(&proof.costs)?;
        let approved = aggregate_cost_bounds(&intent.maximum_costs)?;
        let assets = proof
            .costs
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::Cost { amount, .. } => Some(amount.asset.clone()),
                _ => None,
            })
            .collect();
        validate_price_rows(
            &valuation.prices,
            &assets,
            frozen_valuation_observation(valuation, &assets)?,
        )?;
        let mut additional = BigUint::zero();
        let mut overrun = false;
        for (key, (bound, amount)) in &observed {
            let cap = approved.get(key).ok_or_else(|| invalid("An unapproved cost asset, origin or treatment cannot extend recorded consumption."))?;
            if amount > cap {
                overrun = true;
                let mut excess = bound.amount.clone();
                excess.atoms = gofin_domain::Quantity::new((amount - cap).to_string())?;
                additional += ordinary_policy::amount_usd(&excess, &valuation.prices)?;
            }
        }
        if !overrun && proof.reason != ConsumptionReason::ProvenUnsettledCost {
            return Err(invalid(
                "Use the ordinary verified-effect path for costs inside their approved ceiling.",
            ));
        }
        if let Some(prior) = &record.effects {
            for (key, (_, old)) in totals(
                &conversion_sequences::project_effects(&prior.effects)?
                    .into_iter()
                    .filter(|effect| matches!(effect, FinancialEffect::Cost { amount, .. } if !amount.atoms.is_zero()))
                    .collect::<Vec<_>>(),
            )? {
                if observed.get(&key).is_none_or(|(_, new)| new < &old) {
                    return Err(invalid(
                        "Consumption evidence omitted an already proved cumulative cost.",
                    ));
                }
            }
        }
        if let Some(previous) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .unresolved_consumption_usd(&record.id)
            .map_err(map_store)?
        {
            additional = std::cmp::max(additional, parse_uint(&previous)?);
        }
        if let Some(verified) = &reconciliation.verified {
            let financial = |effects: &[FinancialEffect]| {
                effects
                    .iter()
                    .filter(|effect| {
                        !matches!(
                            effect,
                            FinancialEffect::GroupedSubmissionAmendments { .. }
                                | FinancialEffect::CompoundSubmissionMutations { .. }
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if financial(&verified.effects)
                != record
                    .effects
                    .as_ref()
                    .map(|old| financial(&old.effects))
                    .unwrap_or_default()
            {
                return Err(invalid(
                    "A breached financial partition cannot introduce or retract financial ledger effects while marker outcomes advance.",
                ));
            }
            if verified.submission != proof.submission
                || verified_effect_digest(verified)? != verified.proof_digest
            {
                return Err(invalid(
                    "Combined marker progress changed the immutable submission or proof digest.",
                ));
            }
        }
        let mut retained = record.clone();
        retained.status = Status::Unknown;
        retained.updated_at = now();
        retained.last_reconciliation_evidence = Some(reconciliation.evidence.clone());
        let mut breach = GoFinError::new(
            if overrun {
                "COST_BOUND_EXCEEDED"
            } else {
                "EXECUTION_COSTS_UNSETTLED"
            },
            "budget",
            if overrun {
                format!(
                    "Proven venue costs exceed the approved ceiling; ${} of additional consumption remains counted. The unresolved financial scope keeps its approved limits and reservations.",
                    gofin_domain::Quantity::new(additional.to_string())?.to_decimal(6)
                )
            } else {
                "Correlated venue costs remain unsettled inside the reserved per-asset ceilings. The unresolved financial scope keeps its approved limits and reservations.".into()
            },
            "Inspect the immutable consumed-cost proof and original unresolved identity; do not resubmit.",
        );
        breach.details = Some(Box::new(
            json!({"additional_usd_micros":additional.to_string(),"financial_effects_settled":false,"authority_expanded":false}),
        ));
        retained.error = Some(breach);
        let mut events = BTreeMap::<String, Value>::new();
        let previous = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .unresolved_consumption_proof(&record.id)
            .map_err(map_store)?;
        for evidence in std::iter::once(&proof.evidence)
            .chain(
                reconciliation
                    .verified
                    .as_ref()
                    .map(|verified| &verified.evidence),
            )
            .chain(record.effects.as_ref().map(|old| &old.evidence))
            .chain(previous.as_ref().map(|old| &old.evidence))
        {
            for event in evidence["proofEvents"].as_array().into_iter().flatten() {
                let id = event["eventId"]
                    .as_str()
                    .ok_or_else(|| invalid("Combined native event lacks immutable identity."))?;
                if let Some(old) = events.insert(id.into(), event.clone())
                    && old != *event
                {
                    return Err(invalid(
                        "Combined cost and marker proofs conflict on an immutable native event.",
                    ));
                }
            }
        }
        let merged_events = to_value(&events.into_values().collect::<Vec<_>>())?;
        proof.evidence["proofEvents"] = merged_events.clone();
        proof.proof_digest = digest(&proof)?;
        if let Some(verified) = &reconciliation.verified {
            let mut verified = verified.clone();
            verified.evidence["proofEvents"] = merged_events;
            verified.proof_digest = verified_effect_digest(&verified)?;
            // Validate markers against the FULL approved parent, not only its
            // new financial partition; monetary rows are exact old snapshots.
            partial_proofs::validate_pending(record, &verified)?;
            retained.effects = Some(verified.clone());
            let groups = self.grouped_settlements(&retained)?;
            let children = self
                .compound_child_settlements(&retained)?
                .unwrap_or_default();
            let remaining = self.remaining_mutation_reservations(&retained, &groups)?;
            let consumption = gofin_store::unresolved_consumption::ConsumptionRetention {
                proof,
                additional_usd_micros: additional.to_string(),
            };
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .commit_mutation_batch_progress_with_consumption(
                    &record.id,
                    &to_value(record)?,
                    &to_value(&retained)?,
                    &to_value(&verified)?,
                    &to_value(&verified.effects)?,
                    &groups,
                    &children,
                    &remaining,
                    &consumption,
                    &retained.updated_at,
                )
                .map_err(map_store)?;
            return Ok(Some(retained));
        }
        // Keep late manual-plan spending charged before the original-record
        // commit. A crash between commits can only retain excess conservative
        // usage; replay uses the immutable per-operation maximum once.
        self.record_human_consumed_costs(record, &proof)?;
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .commit_unresolved_consumption(
                &record.id,
                &to_value(record)?,
                &to_value(&retained)?,
                &proof,
                &additional.to_string(),
                &retained.updated_at,
            )
            .map_err(map_store)?;
        Ok(Some(retained))
    }
    pub(super) fn validate_consumed_costs_before_terminal(
        &self,
        record: &OperationRecord,
        verified: &VerifiedEffects,
    ) -> Result<()> {
        let old = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .unresolved_consumption_proof(&record.id)
            .map_err(map_store)?;
        let Some(old) = old else { return Ok(()) };
        let effects = conversion_sequences::project_effects(&verified.effects)?;
        let current = totals(
            &effects
                .into_iter()
                .filter(|effect| matches!(effect, FinancialEffect::Cost { amount, .. } if !amount.atoms.is_zero()))
                .collect::<Vec<_>>(),
        )?;
        for (key, (_, amount)) in totals(&old.costs)? {
            if current
                .get(&key)
                .is_none_or(|(_, current)| current < &amount)
            {
                return Err(invalid(
                    "Terminal proof cannot erase independently proved consumed costs.",
                ));
            }
        }
        Ok(())
    }
}
pub(super) fn record_charge(
    store: &gofin_store::Store,
    record: &OperationRecord,
) -> Result<BigUint> {
    let base = ordinary_policy::record_charge(record)?;
    let additional = store
        .unresolved_consumption_usd(&record.id)
        .map_err(map_store)?
        .map(|value| parse_uint(&value))
        .transpose()?
        .unwrap_or_default();
    let human = store
        .operation_resolution(&record.id)
        .map_err(map_store)?
        .and_then(|value| {
            value["review"]["impact"]["additional_allocation_usd_micros"]
                .as_str()
                .map(str::to_owned)
        })
        .map(|value| parse_uint(&value))
        .transpose()?
        .unwrap_or_default();
    let observed = store
        .operation_resolution(&record.id)
        .map_err(map_store)?
        .map(|value| {
            value["external_confirmation"]["observations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["additional_allocation_usd_micros"].as_str())
                .map(parse_uint)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .max()
        .unwrap_or_default();
    Ok(base + std::cmp::max(observed, std::cmp::max(additional, human)))
}
