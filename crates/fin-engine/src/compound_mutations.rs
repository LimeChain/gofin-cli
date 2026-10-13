//! One immutable dispatch with independently journalled mutation children.
use super::*;
use fin_domain::compound_mutations::{
    ChildMutation, ChildMutationOutcome, CompoundAmendmentContext, LinkedAmendmentContext,
    MutationChild, MutationChildProof,
};

fn invalid(message: &str) -> FinError {
    FinError::new(
        "COMPOUND_MUTATION_INVALID",
        "validation",
        message,
        "Inspect every original operation and obtain fresh exact approval for this concrete batch; reconcile uncertain children without replay.",
    )
}

pub(super) fn children(intent: &FinancialIntent) -> Result<Option<&[MutationChild]>> {
    let rows = intent
        .expected
        .iter()
        .filter_map(|effect| match effect {
            EffectConstraint::CompoundSubmissionMutations { children } => Some(children.as_slice()),
            _ => None,
        })
        .collect::<Vec<_>>();
    match rows.as_slice() {
        [] => Ok(None),
        [row] if !row.is_empty() && row.len() <= 50 => Ok(Some(row)),
        _ => Err(invalid(
            "A compound batch requires one bounded child ownership contract.",
        )),
    }
}

pub(super) fn matches(
    scope: &[MutationChild],
    proof: &[MutationChildProof],
    partial: bool,
) -> bool {
    if proof.is_empty() || (!partial && scope.len() != proof.len()) {
        return false;
    }
    let mut tags = BTreeSet::new();
    proof.iter().all(|proved| {
        tags.insert(&proved.child_tag)
            && scope
                .iter()
                .find(|row| row.child_tag == proved.child_tag)
                .is_some_and(|row| {
                    row.operation_id == proved.operation_id
                        && row.submission == proved.submission
                        && proved
                            .native_evidence
                            .as_object()
                            .is_some_and(|e| !e.is_empty())
                        && match (&row.mutation, &proved.outcome) {
                            (_, ChildMutationOutcome::Rejected { code }) => {
                                !code.is_empty()
                                    && code.len() <= 64
                                    && code
                                        .bytes()
                                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                            }
                            (
                                ChildMutation::Cancel,
                                ChildMutationOutcome::Accepted { successor },
                            ) => successor.is_none(),
                            (
                                ChildMutation::Amend { .. },
                                ChildMutationOutcome::Accepted { successor },
                            ) => successor.as_ref().is_none_or(|next| {
                                next.kind == row.submission.kind
                                    && !next.reference.is_empty()
                                    && next != &row.submission
                            }),
                        }
                })
    })
}
pub(super) fn failed_matches(intent: &FinancialIntent, effect: &FinancialEffect) -> Result<bool> {
    let FinancialEffect::CompoundSubmissionMutations { children: proof } = effect else {
        return Ok(false);
    };
    Ok(children(intent)?.is_some_and(|scope| matches(scope, proof, false)))
}

fn difference(
    before: &BTreeMap<String, BigUint>,
    after: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    after
        .iter()
        .filter_map(|(key, quantity)| {
            let old = before.get(key).cloned().unwrap_or_default();
            (quantity > &old).then(|| (key.clone(), quantity - old))
        })
        .collect()
}
fn add(target: &mut BTreeMap<String, BigUint>, source: BTreeMap<String, BigUint>) {
    for (key, quantity) in source {
        *target.entry(key).or_default() += quantity;
    }
}
fn reserve_rows(intent: &FinancialIntent) -> Result<Vec<ReservationRow>> {
    let mut amounts = BTreeMap::<String, BigUint>::new();
    for row in cash_reservation_amounts(&intent.debits, &intent.maximum_costs)? {
        *amounts.entry(resource_asset_key(&row)?).or_default() += row.amount.atoms.integer();
    }
    let mut rows = amounts
        .into_iter()
        .map(|(resource, amount)| ReservationRow {
            resource,
            units: amount.to_string(),
            fee_units: "0".into(),
        })
        .collect::<Vec<_>>();
    rows.extend(
        ordinary_policy::resources(intent)
            .iter()
            .map(|resource| ReservationRow {
                resource: format!("lock:{}", resource.0),
                units: "0".into(),
                fee_units: "0".into(),
            }),
    );
    Ok(rows)
}

impl Engine {
    pub(super) fn compound_child_settlements(
        &self,
        record: &OperationRecord,
    ) -> Result<Option<Vec<fin_store::compound_amendments::CompoundChildSettlement>>> {
        let Some(prepared) = &record.preview else {
            return Ok(None);
        };
        let Some(scope) = children(&prepared.intent)? else {
            return Ok(None);
        };
        let Some(verified) = &record.effects else {
            return Ok(Some(Vec::new()));
        };
        let wrappers = verified
            .effects
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::CompoundSubmissionMutations { children } => Some(children),
                _ => None,
            })
            .collect::<Vec<_>>();
        if wrappers.len() > 1 {
            return Err(invalid(
                "A cumulative batch proof repeats its ownership wrapper.",
            ));
        }
        let proof = wrappers.first().map_or(&[][..], |proof| proof.as_slice());
        if !proof.is_empty() && !matches(scope, proof, !record.status.terminal()) {
            return Err(invalid(
                "Child outcome proof does not match its immutable ownership contract.",
            ));
        }
        if record.status.terminal() && proof.len() != scope.len() {
            return Err(invalid(
                "Uncertain child outcomes cannot release a terminal compound reservation.",
            ));
        }
        let submission = record
            .submission
            .as_ref()
            .ok_or_else(|| invalid("Compound dispatch identity is missing."))?;
        let mut rows = Vec::new();
        for proof in proof {
            let child = scope
                .iter()
                .find(|row| row.child_tag == proof.child_tag)
                .ok_or_else(|| invalid("Child scope missing."))?;
            let original = self.load_record(&child.operation_id)?;
            let expected_original_reservations = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .operation_reservations(&child.operation_id)
                .map_err(map_store)?;

            if original.status.terminal() {
                if original.submission.as_ref() != Some(&child.submission)
                    || !self
                        .store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?
                        .mutation_outcomes_already_committed(
                            &record.id,
                            &original.id,
                            std::slice::from_ref(proof),
                        )
                        .map_err(map_store)?
                    || !expected_original_reservations.is_empty()
                {
                    return Err(invalid(
                        "A terminal original can retain only its already-committed native outcome.",
                    ));
                }
                let retained_digest = binding_hash(&to_value(proof)?)?;
                rows.push(fin_store::compound_amendments::CompoundChildSettlement {
                    child_tag: child.child_tag.clone(),
                    original_id: original.id.clone(),
                    expected_original: to_value(&original)?,
                    expected_original_reservations: vec![],
                    revision: self
                        .amendment_context(&original.id)?
                        .revisions
                        .into_iter()
                        .find(|revision| {
                            revision.amendment_operation_id == record.id
                                && revision.proof_digest == retained_digest
                        }),
                    reservations: vec![],
                    proof: to_value(proof)?,
                });
                continue;
            }
            if original.submission.as_ref() != Some(&child.submission)
                || !matches!(
                    original.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
            {
                return Err(invalid(
                    "The original financial owner changed before its child outcome was committed.",
                ));
            }
            let mut context = self.amendment_context(&child.operation_id)?;
            let child_proof = to_value(proof)?;
            let revision = match (&child.mutation, &proof.outcome) {
                (
                    ChildMutation::Amend { revision },
                    ChildMutationOutcome::Accepted { successor },
                ) => {
                    if let Some(effects) = &original.effects {
                        validate_effects_with_scope(
                            &original,
                            effects,
                            &Status::Pending,
                            true,
                            Some(&revision.intent),
                        )?;
                    }
                    Some(fin_domain::amendments::ApprovedAmendment {
                        amendment_operation_id: record.id.clone(),
                        amendment_submission: submission.clone(),
                        scope: (**revision).clone(),
                        successor: successor.clone(),
                        proof_digest: binding_hash(&child_proof)?,
                    })
                }
                _ => None,
            };
            let reservations = if let Some(revision) = &revision {
                if !context
                    .revisions
                    .iter()
                    .any(|row| row.amendment_operation_id == record.id)
                {
                    context.revisions.push(revision.clone());
                }
                reserve_rows(
                    &amendments::accounting_intent(&context)?
                        .ok_or_else(|| invalid("The cumulative accounting envelope is missing."))?,
                )?
            } else {
                expected_original_reservations.clone()
            };
            rows.push(fin_store::compound_amendments::CompoundChildSettlement {
                child_tag: child.child_tag.clone(),
                original_id: child.operation_id.clone(),
                expected_original: to_value(&original)?,
                expected_original_reservations,
                revision,
                reservations,
                proof: child_proof,
            });
        }
        Ok(Some(rows))
    }

    pub(super) fn compound_preparation_context(
        &self,
        input: &Value,
    ) -> Result<Option<CompoundAmendmentContext>> {
        let Some(rows) = input.get("amendments") else {
            return Ok(None);
        };
        let rows = rows
            .as_array()
            .filter(|rows| !rows.is_empty() && rows.len() <= 50)
            .ok_or_else(|| invalid("Linked children must be a bounded nonempty array."))?;
        let mut context = CompoundAmendmentContext::default();
        for row in rows {
            let tag = row["orderTag"]
                .as_str()
                .filter(|tag| !tag.is_empty() && tag.len() <= 128)
                .ok_or_else(|| {
                    invalid("Every linked native child requires its exact order tag.")
                })?;
            let id = row["operationId"]
                .as_str()
                .ok_or_else(|| invalid("Every linked child requires its original operation ID."))?;
            if context.children.contains_key(tag) {
                return Err(invalid("A native child tag is duplicated."));
            }
            let submission: Submission = from_value(row["originalSubmission"].clone())?;
            let original = self.load_record(id)?;
            if original.submission.as_ref() != Some(&submission) {
                return Err(invalid(
                    "A linked submission differs from the protected journal.",
                ));
            }
            context.children.insert(
                tag.into(),
                LinkedAmendmentContext {
                    operation_id: id.into(),
                    context: self.amendment_context(id)?,
                },
            );
        }
        Ok(Some(context))
    }

    pub(super) fn validate_compound_mutations(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
        bounded: bool,
        attempt_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(children) = children(&prepared.intent)? else {
            return Ok(None);
        };
        if run_id.is_some() || bounded {
            return Err(invalid(
                "Compound authority mutations require fresh exact approval outside plans and historical runs.",
            ));
        }
        if prepared.intent.expected.iter().any(|effect| {
            matches!(
                effect,
                EffectConstraint::SubmissionAmendment { .. }
                    | EffectConstraint::SubmissionCancellation { .. }
            )
        }) {
            return Err(invalid(
                "A compound batch cannot mix two ownership contracts.",
            ));
        }
        let context = self
            .compound_preparation_context(&prepared.input)?
            .ok_or_else(|| invalid("The original journal context is missing."))?;
        if children.len() != context.children.len() {
            return Err(invalid(
                "Prepared children do not match the exact human input.",
            ));
        }
        let mut required_debits = BTreeMap::new();
        let mut required_costs = BTreeMap::new();
        let mut required_locks = BTreeSet::new();
        let mut owners = BTreeSet::new();
        let mut required_usd = BigUint::zero();
        for child in children {
            let linked = context
                .children
                .get(&child.child_tag)
                .filter(|linked| linked.operation_id == child.operation_id)
                .ok_or_else(|| {
                    invalid("A prepared native tag has different protected ownership.")
                })?;
            if !owners.insert(&child.operation_id) {
                return Err(invalid(
                    "One original owner cannot receive ambiguous sibling revisions.",
                ));
            }
            let original = self.load_record(&child.operation_id)?;
            let old = original
                .preview
                .as_ref()
                .ok_or_else(|| invalid("An original prepared scope is missing."))?;
            if original.submission.as_ref() != Some(&child.submission)
                || !matches!(
                    original.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
                || original.run_id.is_some()
                || crate::compound_mutations::children(&old.intent)?.is_some()
                || amendments::target(&old.intent).is_some()
                || cancellation_target(&old.intent).is_some()
            {
                return Err(invalid(
                    "Every child must retain one unresolved original financial owner.",
                ));
            }
            required_locks.extend(old.intent.locks.iter().cloned());
            match &child.mutation {
                ChildMutation::Cancel => {
                    if self.has_pending_amendment_except(&child.operation_id, attempt_id)? {
                        return Err(invalid(
                            "An unresolved mutation already owns the cancellation boundary.",
                        ));
                    }
                }
                ChildMutation::Amend { revision } => {
                    let previous = linked
                        .context
                        .effective_scope()
                        .map(|scope| &scope.intent)
                        .unwrap_or(&old.intent);
                    let debit_delta = difference(
                        &aggregate_resource_amounts(&previous.debits)?,
                        &aggregate_resource_amounts(&revision.intent.debits)?,
                    );
                    let cost_delta = difference(
                        &aggregate_cost_bounds(&previous.maximum_costs)?,
                        &aggregate_cost_bounds(&revision.intent.maximum_costs)?,
                    );
                    let total = |value: &fin_domain::Valuation| {
                        value.movement_usd_micros.integer()
                            + value.maximum_fee_usd_micros.integer()
                            + value.additional_allocation_usd_micros.integer()
                    };
                    let old_value = linked
                        .context
                        .effective_scope()
                        .and_then(|scope| scope.valuation.as_ref())
                        .or(old.valuation.as_ref())
                        .ok_or_else(|| invalid("The prior child valuation is missing."))?;
                    let new_value = revision
                        .valuation
                        .as_ref()
                        .ok_or_else(|| invalid("The revised child valuation is missing."))?;
                    let scalar_increase = if total(new_value) > total(old_value) {
                        total(new_value) - total(old_value)
                    } else {
                        BigUint::zero()
                    };
                    // Every new approval must contain the independently priced
                    // positive increment. Serde still accepts the absent field in
                    // older submitted records so reconciliation never replays them.
                    let incremental = child.incremental_valuation.as_ref().ok_or_else(|| {
                        invalid("Every newly prepared amendment requires its own positive incremental valuation.")
                    })?;
                    validate_price_observation(
                        &incremental.observed_at,
                        &incremental.source,
                        Utc::now(),
                        "VALUATION_UNAVAILABLE",
                    )?;
                    validate_price_rows(
                        &incremental.prices,
                        &incremental
                            .prices
                            .iter()
                            .map(|price| price.asset.clone())
                            .collect(),
                        Utc::now(),
                    )?;
                    required_usd += std::cmp::max(scalar_increase, total(incremental));
                    let mut virtual_child = prepared.clone();
                    let mut debit_keys = BTreeSet::new();
                    let mut cost_keys = BTreeSet::new();
                    virtual_child.intent = FinancialIntent {
                        debits: revision
                            .intent
                            .debits
                            .iter()
                            .filter_map(|row| {
                                let key = resource_asset_key(row).ok()?;
                                if !debit_keys.insert(key.clone()) {
                                    return None;
                                }
                                let amount = debit_delta.get(&key)?;
                                let mut row = row.clone();
                                row.amount.atoms =
                                    fin_domain::Quantity::new(amount.to_string()).ok()?;
                                Some(row)
                            })
                            .collect(),
                        maximum_costs: revision
                            .intent
                            .maximum_costs
                            .iter()
                            .filter_map(|row| {
                                let key =
                                    cost_key(&row.location, &row.amount.asset, &row.treatment)
                                        .ok()?;
                                if !cost_keys.insert(key.clone()) {
                                    return None;
                                }
                                let amount = cost_delta.get(&key)?;
                                let mut row = row.clone();
                                row.amount.atoms =
                                    fin_domain::Quantity::new(amount.to_string()).ok()?;
                                Some(row)
                            })
                            .collect(),
                        expected: vec![EffectConstraint::SubmissionAmendment {
                            operation_id: child.operation_id.clone(),
                            submission: child.submission.clone(),
                            revision: revision.clone(),
                        }],
                        locks: old.intent.locks.clone(),
                    };
                    if let Some(value) = &child.incremental_valuation {
                        virtual_child.valuation = Some(value.clone());
                        validate_cost_prices(
                            &virtual_child,
                            &prepared_cost_assets(&virtual_child.intent),
                            Utc::now(),
                        )?;
                    }
                    self.validate_submission_amendment_for_attempt(
                        &virtual_child,
                        None,
                        false,
                        attempt_id,
                    )?;
                    add(&mut required_debits, debit_delta);
                    add(&mut required_costs, cost_delta);
                }
            }
        }
        let covers = |actual: BTreeMap<String, BigUint>, required: &BTreeMap<String, BigUint>| {
            required
                .iter()
                .all(|(key, value)| actual.get(key).is_some_and(|available| available >= value))
        };
        if !covers(
            aggregate_resource_amounts(&prepared.intent.debits)?,
            &required_debits,
        ) || !covers(
            aggregate_cost_bounds(&prepared.intent.maximum_costs)?,
            &required_costs,
        ) || !required_locks.is_subset(&prepared.intent.locks.iter().cloned().collect())
        {
            return Err(invalid(
                "The batch does not reserve every positive child scope increase and original resource lock.",
            ));
        }
        let own = prepared
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("The compound incremental valuation is missing."))?;
        if own.movement_usd_micros.integer()
            + own.maximum_fee_usd_micros.integer()
            + own.additional_allocation_usd_micros.integer()
            < required_usd
        {
            return Err(invalid(
                "The compound rolling allocation does not cover all positive revised child exposures.",
            ));
        }
        Ok(Some(binding_hash(&to_value(&children)?)?))
    }

    pub(super) fn has_pending_amendment_except(
        &self,
        id: &str,
        except: Option<&str>,
    ) -> Result<bool> {
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        for value in store.list_records().map_err(map_store)? {
            let row: OperationRecord = from_value(value)?;
            if row.status.terminal() || row.id == id || except == Some(row.id.as_str()) {
                continue;
            }
            let Some(prepared) = &row.preview else {
                continue;
            };
            if amendments::target(&prepared.intent).is_some_and(|(owner, _, _)| owner == id) {
                return Ok(true);
            }
            if let Some(children) = children(&prepared.intent)? {
                for child in children.iter().filter(|child| child.operation_id == id) {
                    let proved = row.effects.as_ref().is_some_and(|verified| {
                        verified.effects.iter().any(|effect| match effect {
                            FinancialEffect::CompoundSubmissionMutations { children: proofs } => {
                                proofs.iter().any(|proof| {
                                    matches(
                                        std::slice::from_ref(child),
                                        std::slice::from_ref(proof),
                                        false,
                                    )
                                })
                            }
                            _ => false,
                        })
                    });
                    if !proved {
                        return Ok(true);
                    }
                }
            }
            for group in grouped_amendments::groups(&prepared.intent)?
                .iter()
                .filter(|group| group.operation_id == id)
            {
                let proved = row.effects.as_ref().is_some_and(|verified| {
                    verified.effects.iter().any(|effect| match effect {
                        FinancialEffect::GroupedSubmissionAmendments { proof } => {
                            grouped_amendments::matches(group, proof, false)
                        }
                        _ => false,
                    })
                });
                if !proved {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}
