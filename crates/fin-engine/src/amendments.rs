//! Fresh exact authority revisions on one original settlement owner.
use super::*;
use fin_domain::amendments::{AmendmentContext, AmendmentScope, ApprovedAmendment};
fn invalid(message: &str) -> FinError {
    FinError::new(
        "AMENDMENT_TARGET_MISMATCH",
        "validation",
        message,
        "Inspect the original operation and obtain fresh exact approval for one cumulative order revision.",
    )
}
pub(super) fn target(intent: &FinancialIntent) -> Option<(&str, &Submission, &AmendmentScope)> {
    match intent.expected.as_slice() {
        [
            EffectConstraint::SubmissionAmendment {
                operation_id,
                submission,
                revision,
            },
        ] => Some((operation_id, submission, revision)),
        _ => None,
    }
}
fn economic_shape(intent: &FinancialIntent) -> Result<Value> {
    let mut expected = intent.expected.clone();
    for constraint in &mut expected {
        if let EffectConstraint::ConversionSequence { scope } = constraint {
            // Only these typed authority amounts vary. Unit identities,
            // stage order, fee funding and every remaining field stay exact.
            scope.maximum_external_input = fin_domain::Quantity::zero();
            for cap in &mut scope.maximum_costs {
                cap.amount.atoms = fin_domain::Quantity::zero();
            }
        }
    }
    let mut value = to_value(&expected)?;
    fn clean(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (name, value) in object {
                    if matches!(
                        name.as_str(),
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
                    ) {
                        *value = json!("approved-bound");
                    } else {
                        clean(value);
                    }
                }
            }
            Value::Array(rows) => {
                for value in rows {
                    clean(value)
                }
            }
            _ => {}
        }
    }
    clean(&mut value);
    Ok(value)
}
fn positive_difference(
    before: &BTreeMap<String, BigUint>,
    after: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    after
        .iter()
        .filter_map(|(key, amount)| {
            let prior = before.get(key).cloned().unwrap_or_default();
            (amount > &prior).then(|| (key.clone(), amount - prior))
        })
        .collect()
}
fn without_zeros(mut rows: BTreeMap<String, BigUint>) -> BTreeMap<String, BigUint> {
    rows.retain(|_, n| !n.is_zero());
    rows
}
fn same_assets(before: &BTreeMap<String, BigUint>, after: &BTreeMap<String, BigUint>) -> bool {
    before.keys().eq(after.keys())
}
fn merge_expected_bounds(target: &mut Value, revision: &Value) -> Result<()> {
    if target.get("kind") == Some(&json!("conversion_sequence"))
        || revision.get("kind") == Some(&json!("conversion_sequence"))
    {
        let EffectConstraint::ConversionSequence { scope: mut old } =
            from_value::<EffectConstraint>(target.clone())?
        else {
            return Err(invalid(
                "An accounting revision changed its sequence identity.",
            ));
        };
        let EffectConstraint::ConversionSequence { scope: new } =
            from_value::<EffectConstraint>(revision.clone())?
        else {
            return Err(invalid(
                "An accounting revision changed its sequence identity.",
            ));
        };
        if old.accounting_version != new.accounting_version
            || old.sequence_id != new.sequence_id
            || old.resource != new.resource
            || old.external_input != new.external_input
            || old.legs.len() != new.legs.len()
            || old.maximum_costs.len() != new.maximum_costs.len()
        {
            return Err(invalid(
                "An accounting revision changed its sequence identity or stage/fee count.",
            ));
        }
        old.maximum_external_input =
            std::cmp::max(old.maximum_external_input, new.maximum_external_input);
        for (old, new) in old.legs.iter_mut().zip(new.legs) {
            if old.id != new.id || old.input != new.input || old.output != new.output {
                return Err(invalid(
                    "An accounting revision changed a sequence stage identity.",
                ));
            }
            old.maximum_input = std::cmp::max(old.maximum_input.clone(), new.maximum_input);
            old.minimum_output = std::cmp::min(old.minimum_output.clone(), new.minimum_output);
        }
        for (old, new) in old.maximum_costs.iter_mut().zip(new.maximum_costs) {
            if old.amount.asset != new.amount.asset || old.funding != new.funding {
                return Err(invalid(
                    "An accounting revision changed a sequence fee unit or funding source.",
                ));
            }
            old.amount.atoms = std::cmp::max(old.amount.atoms.clone(), new.amount.atoms);
        }
        *target = to_value(&EffectConstraint::ConversionSequence { scope: old })?;
        return Ok(());
    }
    match (target, revision) {
        (Value::Object(old), Value::Object(new)) => {
            for (name, value) in new {
                let current = old
                    .get_mut(name)
                    .ok_or_else(|| invalid("An accounting revision changed effect identity."))?;
                if matches!(
                    name.as_str(),
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
                ) {
                    let a: BigInt = current
                        .as_str()
                        .ok_or_else(|| invalid("Revision bound is not exact integer atoms."))?
                        .parse()
                        .map_err(|_| invalid("Revision bound is invalid."))?;
                    let b: BigInt = value
                        .as_str()
                        .ok_or_else(|| invalid("Revision bound is not exact integer atoms."))?
                        .parse()
                        .map_err(|_| invalid("Revision bound is invalid."))?;
                    *current = json!(
                        if name.starts_with("minimum") {
                            std::cmp::min(a, b)
                        } else {
                            std::cmp::max(a, b)
                        }
                        .to_string()
                    );
                } else {
                    merge_expected_bounds(current, value)?;
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            if old.len() != new.len() {
                return Err(invalid("An accounting revision changed effect count."));
            }
            for (a, b) in old.iter_mut().zip(new) {
                merge_expected_bounds(a, b)?;
            }
        }
        (a, b) if a == b => {}
        _ => {
            return Err(invalid(
                "An accounting revision changed the economic effect shape.",
            ));
        }
    }
    Ok(())
}
pub(super) fn accounting_intent(context: &AmendmentContext) -> Result<Option<FinancialIntent>> {
    if context.revisions.is_empty() {
        return Ok(None);
    }
    let original = &context
        .original
        .as_ref()
        .ok_or_else(|| invalid("Original accounting scope is missing."))?
        .intent;
    let mut expected = to_value(&original.expected)?;
    let mut debit_caps = BTreeMap::<String, (ResourceAmount, BigUint)>::new();
    let mut cost_caps = BTreeMap::<String, (CostBound, BigUint)>::new();
    for intent in std::iter::once(original).chain(
        context
            .revisions
            .iter()
            .map(|revision| &revision.scope.intent),
    ) {
        let mut debits = BTreeMap::<String, (ResourceAmount, BigUint)>::new();
        for amount in &intent.debits {
            let key = resource_asset_key(amount)?;
            let row = debits
                .entry(key)
                .or_insert((amount.clone(), BigUint::zero()));
            row.1 += amount.amount.atoms.integer();
        }
        for (key, (template, total)) in debits {
            let row = debit_caps.entry(key).or_insert((template, total.clone()));
            row.1 = std::cmp::max(row.1.clone(), total);
        }
        let mut costs = BTreeMap::<String, (CostBound, BigUint)>::new();
        for amount in &intent.maximum_costs {
            let key = cost_key(&amount.location, &amount.amount.asset, &amount.treatment)?;
            let row = costs
                .entry(key)
                .or_insert((amount.clone(), BigUint::zero()));
            row.1 += amount.amount.atoms.integer();
        }
        for (key, (template, total)) in costs {
            let row = cost_caps.entry(key).or_insert((template, total.clone()));
            row.1 = std::cmp::max(row.1.clone(), total);
        }
        merge_expected_bounds(&mut expected, &to_value(&intent.expected)?)?;
    }
    let debits = debit_caps
        .into_values()
        .map(|(mut amount, total)| {
            amount.amount.atoms = fin_domain::Quantity::new(total.to_string())?;
            Ok(amount)
        })
        .collect::<Result<_>>()?;
    let maximum_costs = cost_caps
        .into_values()
        .map(|(mut amount, total)| {
            amount.amount.atoms = fin_domain::Quantity::new(total.to_string())?;
            Ok(amount)
        })
        .collect::<Result<_>>()?;
    Ok(Some(FinancialIntent {
        debits,
        maximum_costs,
        expected: from_value(expected)?,
        locks: original.locks.clone(),
    }))
}
impl Engine {
    pub(super) fn amendment_context(&self, id: &str) -> Result<AmendmentContext> {
        let record = self.load_record(id)?;
        Ok(AmendmentContext {
            original_evidence: Some(fin_domain::amendments::OriginalSubmissionEvidence::capture(
                &record,
            )),
            original: record.preview,
            revisions: self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .amendment_revisions(id)
                .map_err(map_store)?,
        })
    }
    pub(super) fn amendment_preparation_context(&self, input: &Value) -> Result<AmendmentContext> {
        if input.get("originalSubmission").is_none() {
            return Ok(AmendmentContext::default());
        }
        let id = input["operationId"]
            .as_str()
            .ok_or_else(|| invalid("A linked write requires its original operationId."))?;
        let submission: Submission = from_value(input["originalSubmission"].clone())?;
        let record = self.load_record(id)?;
        if record.submission.as_ref() != Some(&submission) {
            return Err(invalid(
                "The input original submission does not match the protected journal.",
            ));
        }
        self.amendment_context(id)
    }
    #[cfg(test)]
    pub(super) fn validate_submission_amendment(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
        bounded: bool,
    ) -> Result<Option<String>> {
        self.validate_submission_amendment_for_attempt(prepared, run_id, bounded, None)
    }
    pub(super) fn validate_submission_amendment_for_attempt(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
        bounded: bool,
        attempt_id: Option<&str>,
    ) -> Result<Option<String>> {
        if let Some(identity) =
            self.validate_mutation_batch(prepared, run_id, bounded, attempt_id)?
        {
            return Ok(Some(identity));
        }
        if grouped_amendments::group(&prepared.intent)?.is_some() {
            return self.validate_grouped_amendments(prepared, run_id, bounded, attempt_id);
        }
        if super::compound_mutations::children(&prepared.intent)?.is_some() {
            return self.validate_compound_mutations(prepared, run_id, bounded, attempt_id);
        }
        let Some((id, submission, scope)) = target(&prepared.intent) else {
            if prepared
                .intent
                .expected
                .iter()
                .any(|e| matches!(e, EffectConstraint::SubmissionAmendment { .. }))
            {
                return Err(invalid(
                    "A revision must link exactly one original submission.",
                ));
            }
            return Ok(None);
        };
        if run_id.is_some() || bounded {
            return Err(invalid(
                "Authority revisions require fresh exact approval outside historical runs or financial-plan grants.",
            ));
        }
        if self.has_pending_amendment_except(id, attempt_id)? {
            return Err(invalid(
                "Another unresolved amendment already owns this order mutation boundary.",
            ));
        }
        let original = self.load_record(id)?;
        let old = original
            .preview
            .as_ref()
            .ok_or_else(|| invalid("The original scope is missing."))?;
        if !matches!(
            original.status,
            Status::Submitted | Status::Pending | Status::Unknown
        ) || original.submission.as_ref() != Some(submission)
            || original.run_id.is_some()
            || target(&old.intent).is_some()
            || super::compound_mutations::children(&old.intent)?.is_some()
            || !grouped_amendments::groups(&old.intent)?.is_empty()
            || cancellation_target(&old.intent).is_some()
            || old.intent.locks != prepared.intent.locks
            || scope.intent.locks != old.intent.locks
            || scope.intent.expected.iter().any(|e| {
                matches!(
                    e,
                    EffectConstraint::SubmissionAmendment { .. }
                        | EffectConstraint::SubmissionCancellation { .. }
                )
            })
        {
            return Err(invalid(
                "The amendment must retain its unresolved original submission, custody and sole financial owner.",
            ));
        }
        if (old.binding.get(boundaries::BOUNDARY_METADATA_KEY).is_some()
            || old
                .binding
                .get(fin_domain::financial_plans::PLAN_METADATA_KEY)
                .is_some())
            && !intent_within(&scope.intent, &old.intent)?
        {
            return Err(invalid(
                "A separate exact revision cannot expand or weaken the original financial plan or boundary scope.",
            ));
        }
        let context = self.amendment_context(id)?;
        let previous = context
            .effective_scope()
            .map(|s| &s.intent)
            .unwrap_or(&old.intent);
        if economic_shape(previous)? != economic_shape(&scope.intent)? {
            return Err(invalid(
                "An amendment cannot introduce different assets, positions or economic effect types.",
            ));
        }
        let before = aggregate_resource_amounts(&previous.debits)?;
        let after = aggregate_resource_amounts(&scope.intent.debits)?;
        if !same_assets(&before, &after)
            || without_zeros(aggregate_resource_amounts(&prepared.intent.debits)?)
                != positive_difference(&before, &after)
        {
            return Err(invalid(
                "The amendment reserves exactly the positive principal increase over the previous approved cumulative scope.",
            ));
        }
        let before = aggregate_cost_bounds(&previous.maximum_costs)?;
        let after = aggregate_cost_bounds(&scope.intent.maximum_costs)?;
        if !same_assets(&before, &after)
            || without_zeros(aggregate_cost_bounds(&prepared.intent.maximum_costs)?)
                != positive_difference(&before, &after)
        {
            return Err(invalid(
                "The amendment reserves exactly the positive cost increase, preserving asset and cost treatment.",
            ));
        }
        let prior_binding = context
            .effective_scope()
            .map(|scope| &scope.binding)
            .unwrap_or(&old.binding);
        if prior_binding.get(fin_domain::BASELINE_ANCHORS_KEY)
            != scope.binding.get(fin_domain::BASELINE_ANCHORS_KEY)
        {
            return Err(invalid(
                "An amendment cannot replace the original broker-observed position baseline.",
            ));
        }
        let mut full = prepared.clone();
        full.intent = scope.intent.clone();
        full.binding = scope.binding.clone();
        full.valuation = scope.valuation.clone();
        if let Some(binding) = full.binding.as_object_mut() {
            binding.remove(fin_domain::BASELINE_ANCHORS_KEY);
        }
        validate_prepared(&full, &prepared.operation, Utc::now())?;
        validate_cost_bound_contract(&full.intent)?;
        validate_cost_prices(&full, &prepared_cost_assets(&full.intent), Utc::now())?;
        let valuation = full.valuation.as_ref().ok_or_else(|| {
            invalid("A cumulative revision requires its own full attributable valuation.")
        })?;
        let previous_valuation = context
            .effective_scope()
            .and_then(|scope| scope.valuation.as_ref())
            .or(old.valuation.as_ref())
            .ok_or_else(|| invalid("The previous cumulative scope valuation is missing."))?;
        let total = |value: &fin_domain::Valuation| {
            value.movement_usd_micros.integer()
                + value.maximum_fee_usd_micros.integer()
                + value.additional_allocation_usd_micros.integer()
        };
        let previous_usd = total(previous_valuation);
        let revised_usd = total(valuation);
        let own = prepared
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("The amendment's incremental valuation is missing."))?;
        if revised_usd > previous_usd && total(own) < revised_usd - previous_usd {
            return Err(invalid(
                "The amendment's rolling allocation reserve does not cover the full revised USD exposure increase.",
            ));
        }
        if fin_domain::policy::limit(&self.options.max_movement_and_cost_usd_micros)?.is_some_and(
            |limit| {
                valuation.movement_usd_micros.integer() + valuation.maximum_fee_usd_micros.integer()
                    > BigUint::from(limit)
            },
        ) {
            return Err(FinError::new(
                "MOVEMENT_LIMIT",
                "budget",
                "The full amended movement exceeds the personal per-movement ceiling.",
                "Reduce the cumulative order scope or change protected policy through the human configuration path.",
            ));
        }
        if let Some(verified) = &original.effects {
            validate_effects_with_scope(
                &original,
                verified,
                &Status::Pending,
                true,
                Some(&scope.intent),
            )?;
        }
        Ok(Some(id.into()))
    }
    pub(super) fn has_pending_amendment(&self, id: &str) -> Result<bool> {
        self.has_pending_amendment_except(id, None)
    }

    pub(super) fn commit_amendment(
        &self,
        record: &OperationRecord,
        settlement: &Settlement,
    ) -> Result<bool> {
        let Some(prepared) = record.preview.as_ref() else {
            return Ok(false);
        };
        let Some((original_id, original_submission, scope)) = target(&prepared.intent) else {
            return Ok(false);
        };
        if record.run_id.is_some()
            || prepared
                .binding
                .get(boundaries::BOUNDARY_METADATA_KEY)
                .is_some()
            || prepared
                .binding
                .get(fin_domain::financial_plans::PLAN_METADATA_KEY)
                .is_some()
        {
            return Err(invalid(
                "A bounded grant cannot settle a separate authority revision.",
            ));
        }
        if record.status != Status::Completed {
            return Ok(false);
        }
        let proof = record.effects.as_ref().ok_or_else(|| {
            invalid("The amendment has no correlated typed acknowledgment proof.")
        })?;
        let mutations = proof
            .effects
            .iter()
            .filter_map(|effect| {
                if let FinancialEffect::SubmissionAmendment {
                    operation_id,
                    submission,
                    successor,
                } = effect
                {
                    Some((operation_id, submission, successor))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if mutations.len() != 1
            || mutations[0].0 != original_id
            || mutations[0].1 != original_submission
            || proof.effects.iter().any(|e| {
                !matches!(
                    e,
                    FinancialEffect::SubmissionAmendment { .. } | FinancialEffect::Cost { .. }
                )
            })
        {
            return Err(invalid(
                "An amendment may prove its linked mutation and attributable fee, never its own trade fills.",
            ));
        }
        let original = self.load_record(original_id)?;
        if original.submission.as_ref() != Some(original_submission)
            || !matches!(
                original.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
        {
            return Err(invalid(
                "The original fill owner changed or reached terminal status before its amendment settled.",
            ));
        }
        let context = self.amendment_context(original_id)?;
        let prior = context.effective_scope().map(|s| &s.intent).unwrap_or(
            &original
                .preview
                .as_ref()
                .ok_or_else(|| invalid("Original preview missing."))?
                .intent,
        );
        if economic_shape(prior)? != economic_shape(&scope.intent)? {
            return Err(invalid(
                "The amendment proof changes the economic effect identity.",
            ));
        }
        if let Some(verified) = &original.effects {
            validate_effects_with_scope(
                &original,
                verified,
                &Status::Pending,
                true,
                Some(&scope.intent),
            )?;
        }
        let mut accounting = context.clone();
        accounting.revisions.push(ApprovedAmendment {
            amendment_operation_id: record.id.clone(),
            amendment_submission: record
                .submission
                .clone()
                .ok_or_else(|| invalid("Amendment submission missing."))?,
            scope: scope.clone(),
            successor: mutations[0].2.clone(),
            proof_digest: proof.proof_digest.clone(),
        });
        let accounting = accounting_intent(&accounting)?
            .ok_or_else(|| invalid("Accounting envelope missing."))?;
        let mut rows = cash_reservation_amounts(&accounting.debits, &accounting.maximum_costs)?
            .iter()
            .map(|amount| {
                Ok(ReservationRow {
                    resource: resource_asset_key(amount)?,
                    units: amount.amount.atoms.to_string(),
                    fee_units: "0".into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut amounts = BTreeMap::<String, BigUint>::new();
        for row in rows.drain(..) {
            *amounts.entry(row.resource).or_default() += parse_uint(&row.units)?;
        }
        rows.extend(amounts.into_iter().map(|(resource, n)| ReservationRow {
            resource,
            units: n.to_string(),
            fee_units: "0".into(),
        }));
        rows.extend(scope.intent.locks.iter().map(|resource| ReservationRow {
            resource: format!("lock:{}", resource.0),
            units: "0".into(),
            fee_units: "0".into(),
        }));
        let revision = ApprovedAmendment {
            amendment_operation_id: record.id.clone(),
            amendment_submission: record
                .submission
                .clone()
                .ok_or_else(|| invalid("Amendment submission missing."))?,
            scope: scope.clone(),
            successor: mutations[0].2.clone(),
            proof_digest: proof.proof_digest.clone(),
        };
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .commit_amendment_settlement(
                settlement,
                original_id,
                &to_value(&original)?,
                &revision,
                &rows,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(true)
    }
}

#[cfg(test)]
mod sequence_revision_tests {
    use super::*;
    use fin_domain::conversion_sequences::{
        ConversionCostBound, ConversionCostFunding, ConversionLegConstraint,
        ConversionSequenceConstraint,
    };
    fn asset(id: &str) -> fin_domain::AssetId {
        fin_domain::AssetId {
            network: "synthetic".into(),
            reference: id.into(),
            decimals: 6,
        }
    }
    fn intent() -> FinancialIntent {
        FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            locks: vec![ResourceId("synthetic-sequence".into())],
            expected: vec![EffectConstraint::ConversionSequence {
                scope: ConversionSequenceConstraint {
                    accounting_version: 1,
                    sequence_id: "immutable-sequence".into(),
                    resource: ResourceId("synthetic-sequence".into()),
                    external_input: asset("A"),
                    maximum_external_input: fin_domain::Quantity::from_u64(100),
                    legs: vec![
                        ConversionLegConstraint {
                            id: "parent".into(),
                            input: asset("A"),
                            maximum_input: fin_domain::Quantity::from_u64(100),
                            output: asset("B"),
                            minimum_output: fin_domain::Quantity::from_u64(90),
                        },
                        ConversionLegConstraint {
                            id: "child".into(),
                            input: asset("B"),
                            maximum_input: fin_domain::Quantity::from_u64(90),
                            output: asset("A"),
                            minimum_output: fin_domain::Quantity::from_u64(80),
                        },
                    ],
                    maximum_costs: vec![ConversionCostBound {
                        amount: Amount {
                            asset: asset("B"),
                            atoms: fin_domain::Quantity::from_u64(5),
                        },
                        funding: ConversionCostFunding::GeneratedCredit,
                    }],
                },
            }],
        }
    }
    #[test]
    fn sequence_amendment_changes_bounds_but_not_native_units_funding_or_stage_ownership() {
        let original = intent();
        let mut revised = original.clone();
        let EffectConstraint::ConversionSequence { scope } = &mut revised.expected[0] else {
            unreachable!()
        };
        scope.maximum_external_input = fin_domain::Quantity::from_u64(110);
        scope.legs[1].maximum_input = fin_domain::Quantity::from_u64(95);
        scope.legs[1].minimum_output = fin_domain::Quantity::from_u64(85);
        scope.maximum_costs[0].amount.atoms = fin_domain::Quantity::from_u64(7);
        assert_eq!(
            economic_shape(&original).unwrap(),
            economic_shape(&revised).unwrap()
        );
        let mut encoded = to_value(&original.expected).unwrap();
        merge_expected_bounds(&mut encoded, &to_value(&revised.expected).unwrap()).unwrap();
        let merged: Vec<EffectConstraint> = from_value(encoded).unwrap();
        let EffectConstraint::ConversionSequence { scope } = &merged[0] else {
            unreachable!()
        };
        assert_eq!(
            scope.maximum_external_input,
            fin_domain::Quantity::from_u64(110)
        );
        assert_eq!(
            scope.maximum_costs[0].amount.atoms,
            fin_domain::Quantity::from_u64(7)
        );
        assert_eq!(
            scope.legs[1].maximum_input,
            fin_domain::Quantity::from_u64(95)
        );
        assert_eq!(
            scope.legs[1].minimum_output,
            fin_domain::Quantity::from_u64(80),
            "earlier fills retain original floor"
        );
        for field in ["asset", "funding", "stage", "order", "version", "resource"] {
            let mut bad = revised.clone();
            let EffectConstraint::ConversionSequence { scope } = &mut bad.expected[0] else {
                unreachable!()
            };
            match field {
                "asset" => scope.maximum_costs[0].amount.asset = asset("C"),
                "funding" => scope.maximum_costs[0].funding = ConversionCostFunding::External,
                "stage" => scope.legs[1].id = "foreign-child".into(),
                "order" => scope.legs.swap(0, 1),
                "version" => scope.accounting_version = 2,
                _ => scope.resource = ResourceId("foreign".into()),
            }
            assert_ne!(
                economic_shape(&original).unwrap(),
                economic_shape(&bad).unwrap(),
                "{field}"
            );
            let mut encoded = to_value(&original.expected).unwrap();
            assert!(
                merge_expected_bounds(&mut encoded, &to_value(&bad.expected).unwrap()).is_err(),
                "{field}"
            );
        }
    }
}
