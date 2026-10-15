//! Conservative same-owner member revisions composed only from accepted proof.
use super::*;
use gofin_domain::compound_mutations::ChildMutationOutcome;
use gofin_domain::grouped_amendments::{
    GroupedAmendmentGroup, GroupedAmendmentProof, validate_replacements,
};
fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "GROUPED_AMENDMENT_INVALID",
        "validation",
        message,
        "Reconcile the original batch; obtain fresh exact approval for disjoint native member changes without replay.",
    )
}
pub(super) fn groups(intent: &FinancialIntent) -> Result<Vec<&GroupedAmendmentGroup>> {
    let rows = intent
        .expected
        .iter()
        .filter_map(|e| match e {
            EffectConstraint::GroupedSubmissionAmendments { group } => Some(group.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut owners = BTreeSet::new();
    if rows.len() > 25 || rows.iter().any(|g| !owners.insert(&g.operation_id)) {
        return Err(invalid("A mutation repeats its original-owner group."));
    }
    Ok(rows)
}
pub(super) fn group(intent: &FinancialIntent) -> Result<Option<&GroupedAmendmentGroup>> {
    let rows = groups(intent)?;
    match rows.as_slice() {
        [] => Ok(None),
        [g] => Ok(Some(*g)),
        _ => Err(invalid(
            "This pure grouped flow requires one original owner.",
        )),
    }
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
pub(super) fn accounting_scope(
    group: &GroupedAmendmentGroup,
    tags: &BTreeSet<String>,
) -> Result<gofin_domain::amendments::AmendmentScope> {
    gofin_domain::grouped_amendments::derive_accounting_scope(group, tags)
}
pub(super) fn matches(
    group: &GroupedAmendmentGroup,
    proof: &GroupedAmendmentProof,
    partial: bool,
) -> bool {
    if proof.operation_id != group.operation_id
        || proof.submission != group.submission
        || proof.children.is_empty()
        || (!partial && proof.children.len() != group.children.len())
    {
        return false;
    }
    let scope = group
        .children
        .iter()
        .map(|c| gofin_domain::compound_mutations::MutationChild {
            child_tag: c.child_tag.clone(),
            operation_id: group.operation_id.clone(),
            submission: group.submission.clone(),
            mutation: if c.mutation.is_amend() {
                gofin_domain::compound_mutations::ChildMutation::Amend {
                    revision: c.scope.clone(),
                }
            } else {
                gofin_domain::compound_mutations::ChildMutation::Cancel
            },
            incremental_valuation: None,
        })
        .collect::<Vec<_>>();
    compound_mutations::matches(&scope, &proof.children, partial)
        && proof.children.iter().all(|p| {
            !matches!(p.outcome, ChildMutationOutcome::Accepted { .. })
                || group
                    .children
                    .iter()
                    .find(|c| c.child_tag == p.child_tag)
                    .is_some_and(|c| {
                        gofin_domain::grouped_amendments::accepted_readback_matches(
                            c,
                            &p.native_evidence,
                        )
                    })
        })
}
impl Engine {
    pub(super) fn validate_grouped_amendments(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
        bounded: bool,
        attempt_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(group) = group(&prepared.intent)? else {
            return Ok(None);
        };
        if run_id.is_some() || bounded || prepared.intent.expected.len() != 1 {
            return Err(invalid(
                "Grouped authority changes require fresh exact approval and one original-owner contract.",
            ));
        }
        validate_replacements(group)?;
        if group.children.iter().any(|child| child.mutation.is_amend())
            && group.incremental_valuation.is_none()
        {
            return Err(invalid(
                "Each newly approved grouped amendment requires its independently priced incremental valuation; old submitted records remain recoverable without replay.",
            ));
        }
        let original = self.load_record(&group.operation_id)?;
        let context = self.amendment_context(&group.operation_id)?;
        let old = original
            .preview
            .as_ref()
            .ok_or_else(|| invalid("The original protected scope is missing."))?;
        let raw = context.effective_scope().cloned().unwrap_or_else(|| {
            gofin_domain::amendments::AmendmentScope {
                intent: old.intent.clone(),
                binding: old.binding.clone(),
                valuation: old.valuation.clone(),
            }
        });
        if !matches!(
            original.status,
            Status::Submitted | Status::Pending | Status::Unknown
        ) || original.run_id.is_some()
            || self.has_pending_amendment_except(&original.id, attempt_id)?
            || raw != *group.original_scope
            || original.submission.as_ref() != Some(&group.submission)
        {
            return Err(invalid(
                "The grouped preimage differs from its protected original scope.",
            ));
        }
        let input = self
            .compound_preparation_context(&prepared.input)?
            .ok_or_else(|| invalid("Grouped member ownership is missing."))?;
        if input.children.len() != group.children.len()
            || group.children.iter().any(|c| {
                input
                    .children
                    .get(&c.child_tag)
                    .is_none_or(|l| l.operation_id != group.operation_id)
            })
        {
            return Err(invalid(
                "Native tags differ from protected input ownership.",
            ));
        }
        for child in &group.children {
            if !child.mutation.is_amend() {
                if *child.scope != raw {
                    return Err(invalid(
                        "A grouped cancellation retains exactly its old scope and cannot grant new financial authority.",
                    ));
                }
                continue;
            }
            let mut virtual_child = prepared.clone();
            let before = aggregate_resource_amounts(&raw.intent.debits)?;
            let after = aggregate_resource_amounts(&child.scope.intent.debits)?;
            let costs = aggregate_cost_bounds(&raw.intent.maximum_costs)?;
            let new = aggregate_cost_bounds(&child.scope.intent.maximum_costs)?;
            virtual_child.intent = incremental_intent(
                &raw.intent,
                &child.scope.intent,
                &delta(&before, &after),
                &delta(&costs, &new),
            )?;
            virtual_child.intent.expected = vec![EffectConstraint::SubmissionAmendment {
                operation_id: group.operation_id.clone(),
                submission: group.submission.clone(),
                revision: child.scope.clone(),
            }];
            self.validate_submission_amendment_for_attempt(
                &virtual_child,
                None,
                false,
                attempt_id,
            )?;
        }
        let all = group.children.iter().map(|c| c.child_tag.clone()).collect();
        let derived = accounting_scope(group, &all)?;
        if derived != *group.approved_envelope {
            return Err(invalid(
                "The approved grouped envelope does not sum positive member increases and shared costs exactly.",
            ));
        }
        let before = aggregate_resource_amounts(&raw.intent.debits)?;
        let after = aggregate_resource_amounts(&derived.intent.debits)?;
        let old_costs = aggregate_cost_bounds(&raw.intent.maximum_costs)?;
        let new_costs = aggregate_cost_bounds(&derived.intent.maximum_costs)?;
        if aggregate_resource_amounts(&prepared.intent.debits)? != delta(&before, &after)
            || aggregate_cost_bounds(&prepared.intent.maximum_costs)?
                != delta(&old_costs, &new_costs)
            || prepared.intent.locks != raw.intent.locks
        {
            return Err(invalid(
                "The grouped reservation does not equal its conservative incremental scope.",
            ));
        }
        let has_amendment = group.children.iter().any(|c| c.mutation.is_amend());
        let mut virtual_envelope = prepared.clone();
        virtual_envelope.intent = derived.intent.clone();
        virtual_envelope.binding = derived.binding.clone();
        virtual_envelope.valuation = derived.valuation.clone();
        if let Some(binding) = virtual_envelope.binding.as_object_mut() {
            gofin_domain::product_names::remove(binding, gofin_domain::BASELINE_ANCHORS_KEY);
        }
        if has_amendment {
            validate_prepared(&virtual_envelope, &prepared.operation, Utc::now())?;
            validate_cost_bound_contract(&derived.intent)?;
            validate_cost_prices(
                &virtual_envelope,
                &prepared_cost_assets(&derived.intent),
                Utc::now(),
            )?;
        }
        let old_value = raw
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("Original valuation missing."))?;
        let full = derived.valuation.as_ref().unwrap();
        let own = prepared
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("Incremental valuation missing."))?;
        let total = |v: &gofin_domain::Valuation| {
            v.movement_usd_micros.integer()
                + v.maximum_fee_usd_micros.integer()
                + v.additional_allocation_usd_micros.integer()
        };
        if let Some(incremental) = &group.incremental_valuation {
            let mut floor = prepared.clone();
            floor.valuation = Some(incremental.clone());
            validate_price_observation(
                &incremental.observed_at,
                &incremental.source,
                Utc::now(),
                "VALUATION_UNAVAILABLE",
            )?;
            validate_price_rows(
                &incremental.prices,
                &incremental.prices.iter().map(|p| p.asset.clone()).collect(),
                Utc::now(),
            )?;
            validate_cost_prices(&floor, &prepared_cost_assets(&prepared.intent), Utc::now())?;
            if total(own) < total(incremental) {
                return Err(invalid(
                    "The group omits independently priced positive member authority increments.",
                ));
            }
        }

        let full_total = total(full);
        let old_total = total(old_value);
        if full_total < old_total {
            return Err(invalid(
                "Grouped conservative USD scope lost an original reservation.",
            ));
        }
        if total(own) < full_total - old_total {
            return Err(invalid(
                "Grouped USD reservation omits a positive member increase.",
            ));
        }
        if has_amendment
            && gofin_domain::policy::limit(&self.options.max_movement_and_cost_usd_micros)?
                .is_some_and(|n| {
                    full.movement_usd_micros.integer() + full.maximum_fee_usd_micros.integer()
                        > BigUint::from(n)
                })
        {
            return Err(invalid(
                "The full grouped scope exceeds the protected movement ceiling.",
            ));
        }
        Ok(Some(binding_hash(&to_value(group)?)?))
    }
}
pub(super) fn incremental_intent(
    original: &FinancialIntent,
    new: &FinancialIntent,
    debits: &BTreeMap<String, BigUint>,
    costs: &BTreeMap<String, BigUint>,
) -> Result<FinancialIntent> {
    let mut debit_rows = Vec::new();
    let mut cost_rows = Vec::new();
    let mut seen = BTreeSet::new();
    for row in &new.debits {
        let k = resource_asset_key(row)?;
        if seen.insert(k.clone())
            && let Some(n) = debits.get(&k)
        {
            let mut row = row.clone();
            row.amount.atoms = gofin_domain::Quantity::new(n.to_string())?;
            debit_rows.push(row);
        }
    }
    let mut seen = BTreeSet::new();
    for row in &new.maximum_costs {
        let k = cost_key(&row.location, &row.amount.asset, &row.treatment)?;
        if seen.insert(k.clone())
            && let Some(n) = costs.get(&k)
        {
            let mut row = row.clone();
            row.amount.atoms = gofin_domain::Quantity::new(n.to_string())?;
            cost_rows.push(row);
        }
    }
    Ok(FinancialIntent {
        debits: debit_rows,
        maximum_costs: cost_rows,
        expected: vec![],
        locks: original.locks.clone(),
    })
}
fn reserve_rows(intent: &FinancialIntent) -> Result<Vec<ReservationRow>> {
    let totals = aggregate_resource_amounts(&cash_reservation_amounts(
        &intent.debits,
        &intent.maximum_costs,
    )?)?;
    let mut rows = totals
        .into_iter()
        .map(|(resource, n)| ReservationRow {
            resource,
            units: n.to_string(),
            fee_units: "0".into(),
        })
        .collect::<Vec<_>>();
    rows.extend(intent.locks.iter().map(|r| ReservationRow {
        resource: format!("lock:{}", r.0),
        units: "0".into(),
        fee_units: "0".into(),
    }));
    Ok(rows)
}
impl Engine {
    pub(super) fn grouped_settlement_for(
        &self,
        record: &OperationRecord,
        group: &GroupedAmendmentGroup,
        proof: &GroupedAmendmentProof,
    ) -> Result<gofin_store::grouped_amendments::GroupedSettlement> {
        if !matches(group, proof, !record.status.terminal()) {
            return Err(invalid(
                "Grouped native proof differs from its exact ownership contract.",
            ));
        }
        // Standing edits require independent native audit/readback. The adapter
        // commits those exact rows in every immutable child evidence object.
        if proof.children.iter().any(|child| {
            matches!(
                child.outcome,
                ChildMutationOutcome::Accepted { successor: Some(_) }
            )
        }) {
            return Err(invalid(
                "A grouped successor requires a reviewed multi-successor lineage contract.",
            ));
        }
        let accepted = proof
            .children
            .iter()
            .filter(|c| matches!(c.outcome, ChildMutationOutcome::Accepted { .. }))
            .map(|c| c.child_tag.clone())
            .collect::<BTreeSet<_>>();
        let proven = proof
            .children
            .iter()
            .map(|c| c.child_tag.clone())
            .collect::<BTreeSet<_>>();
        let unknown = group
            .children
            .iter()
            .filter(|c| !proven.contains(&c.child_tag))
            .map(|c| c.child_tag.clone())
            .collect::<BTreeSet<_>>();
        let original = self.load_record(&group.operation_id)?;
        if original.status.terminal() {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if original.submission.as_ref() != Some(&group.submission)
                || !store
                    .mutation_outcomes_already_committed(&record.id, &original.id, &proof.children)
                    .map_err(map_store)?
                || !store
                    .operation_reservations(&original.id)
                    .map_err(map_store)?
                    .is_empty()
            {
                return Err(invalid(
                    "A terminal original can retain only its complete already-committed native outcomes.",
                ));
            }
            return Ok(gofin_store::grouped_amendments::GroupedSettlement {
                original_id: original.id.clone(),
                expected_original: to_value(&original)?,
                expected_original_reservations: vec![],
                revision: None,
                original_reservations: vec![],
                remaining_reservations: vec![],
            });
        }

        if original.submission.as_ref() != Some(&group.submission)
            || !matches!(
                original.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
        {
            return Err(invalid(
                "The original owner changed before group outcomes were committed.",
            ));
        }
        let expected_original_reservations = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_reservations(&original.id)
            .map_err(map_store)?;
        let submission = record
            .submission
            .as_ref()
            .ok_or_else(|| invalid("Grouped dispatch identity missing."))?;
        let mut context = self.amendment_context(&original.id)?;
        let revision = if accepted.is_empty() {
            None
        } else {
            let scope = accounting_scope(group, &accepted)?;
            if let Some(effects) = &original.effects {
                validate_effects_with_scope(
                    &original,
                    effects,
                    &Status::Pending,
                    true,
                    Some(&scope.intent),
                )?;
            }
            let revision = gofin_domain::amendments::ApprovedAmendment {
                amendment_operation_id: record.id.clone(),
                amendment_submission: submission.clone(),
                scope,
                successor: None,
                proof_digest: binding_hash(&to_value(proof)?)?,
            };
            // Only the newest accepted subset is effective; older subset proofs
            // remain durable revisions and keep their economic maxima forever.
            context.revisions.push(revision.clone());
            Some(revision)
        };
        let original_reservations = if revision.is_some() {
            reserve_rows(
                &amendments::accounting_intent(&context)?
                    .ok_or_else(|| invalid("Grouped accounting scope missing."))?,
            )?
        } else {
            expected_original_reservations.clone()
        };
        let old_debits = aggregate_resource_amounts(&group.original_scope.intent.debits)?;
        let old_costs = aggregate_cost_bounds(&group.original_scope.intent.maximum_costs)?;
        let unknown_scope = accounting_scope(group, &unknown)?;
        let debit_delta = delta(
            &old_debits,
            &aggregate_resource_amounts(&unknown_scope.intent.debits)?,
        );
        let accepted_amendment = group
            .children
            .iter()
            .any(|c| accepted.contains(&c.child_tag) && c.mutation.is_amend());
        let cost_delta = if !accepted_amendment {
            delta(
                &old_costs,
                &aggregate_cost_bounds(&unknown_scope.intent.maximum_costs)?,
            )
        } else {
            BTreeMap::new()
        };
        let remaining = incremental_intent(
            &group.original_scope.intent,
            &unknown_scope.intent,
            &debit_delta,
            &cost_delta,
        )?;
        Ok(gofin_store::grouped_amendments::GroupedSettlement {
            original_id: original.id.clone(),
            expected_original: to_value(&original)?,
            expected_original_reservations,
            revision,
            original_reservations,
            remaining_reservations: reserve_rows(&remaining)?,
        })
    }
    pub(super) fn grouped_settlements(
        &self,
        record: &OperationRecord,
    ) -> Result<Vec<gofin_store::grouped_amendments::GroupedSettlement>> {
        let Some(prepared) = &record.preview else {
            return Ok(Vec::new());
        };
        let scopes = groups(&prepared.intent)?;
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let verified = record
            .effects
            .as_ref()
            .ok_or_else(|| invalid("Grouped native outcomes remain unproved."))?;
        let mut results = Vec::new();
        for group in scopes {
            let proofs = verified
                .effects
                .iter()
                .filter_map(|e| match e {
                    FinancialEffect::GroupedSubmissionAmendments { proof }
                        if proof.operation_id == group.operation_id =>
                    {
                        Some(proof)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if let [proof] = proofs.as_slice() {
                results.push(self.grouped_settlement_for(record, group, proof)?);
            } else if proofs.is_empty() && !record.status.terminal() {
                continue;
            } else {
                return Err(invalid(
                    "A grouped outcome wrapper is missing or duplicated.",
                ));
            }
        }
        Ok(results)
    }
}
