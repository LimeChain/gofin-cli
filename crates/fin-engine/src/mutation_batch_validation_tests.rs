// Included in the existing synthetic Engine module. No live adapter or key.
fn composed_group_scope(originals: &[OperationRecord], new_write: bool) -> PreparedOperation {
    let mut prepared = originals[0].preview.clone().unwrap();
    prepared.intent = FinancialIntent::default();
    let mut rows = Vec::new();
    let mut locks = BTreeSet::new();
    let mut usd = BigUint::zero();
    for (index, original) in originals.iter().enumerate() {
        let mut p = grouped_prepared(original, true);
        let EffectConstraint::GroupedSubmissionAmendments { group } = &mut p.intent.expected[0]
        else {
            unreachable!()
        };
        for child in &mut group.children {
            child.child_tag = format!("owner-{index}:{}", child.child_tag);
            rows.push(json!({"orderTag":child.child_tag,"operationId":original.id,"originalSubmission":original.submission}));
        }
        prepared.intent.debits.extend(p.intent.debits);
        prepared.intent.maximum_costs.extend(p.intent.maximum_costs);
        prepared.intent.expected.extend(p.intent.expected);
        locks.extend(p.intent.locks);
        let value = p.valuation.unwrap();
        usd += value.movement_usd_micros.integer()
            + value.maximum_fee_usd_micros.integer()
            + value.additional_allocation_usd_micros.integer();
    }
    prepared.binding = json!({"syntheticComposed":true});
    if new_write {
        let new = originals[0].preview.as_ref().unwrap();
        prepared.binding["mutationNewWriteScope"] = json!(fin_domain::amendments::AmendmentScope {
            binding: new.binding.clone(),
            intent: new.intent.clone(),
            valuation: new.valuation.clone()
        });
        prepared.intent.debits.extend(new.intent.debits.clone());
        prepared
            .intent
            .maximum_costs
            .extend(new.intent.maximum_costs.clone());
        prepared.intent.expected.extend(new.intent.expected.clone());
        locks.extend(new.intent.locks.clone());
        let value = new.valuation.as_ref().unwrap();
        usd += value.movement_usd_micros.integer()
            + value.maximum_fee_usd_micros.integer()
            + value.additional_allocation_usd_micros.integer();
    }
    prepared.input = json!({"amendments":rows});
    prepared.intent.locks = locks.into_iter().collect();
    let value = prepared.valuation.as_mut().unwrap();
    value.movement_usd_micros = quantity(&usd.to_string());
    value.maximum_fee_usd_micros = Quantity::zero();
    value.additional_allocation_usd_micros = Quantity::zero();
    prepared
}
#[tokio::test]
async fn composed_mutation_authority_sums_distinct_groups_and_new_financial_scope_without_reusing_budget()
 {
    let directory = tempfile::tempdir().unwrap();
    let engine = synthetic_engine(
        &directory.path().join("composed-validation.sqlite"),
        Arc::new(GroupedFixture(SyntheticAdapter::new())),
    );
    let originals = compound_originals(&engine).await;
    let prepared = composed_group_scope(&originals, true);
    assert!(
        engine
            .validate_submission_amendment(&prepared, None, false)
            .unwrap()
            .is_some()
    );
    assert!(
        engine
            .validate_submission_amendment(&prepared, None, true)
            .is_err()
    );
    let mut bad = prepared.clone();
    bad.valuation.as_mut().unwrap().movement_usd_micros = quantity("4000000");
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
    let mut bad = prepared.clone();
    bad.intent.debits.pop();
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
    let mut bad = prepared.clone();
    bad.binding
        .as_object_mut()
        .unwrap()
        .remove("mutationNewWriteScope");
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
    let mut bad = prepared.clone();
    if let EffectConstraint::GroupedSubmissionAmendments { group } = &mut bad.intent.expected[1] {
        group.children[0].child_tag = "owner-0:member-0".into();
    }
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
    let mut bad = prepared.clone();
    if let EffectConstraint::GroupedSubmissionAmendments { group } = &mut bad.intent.expected[1] {
        group.incremental_valuation = None;
    }
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
}
#[tokio::test]
async fn composed_group_and_independent_owner_require_all_positive_increments_and_exact_input_tags()
{
    let directory = tempfile::tempdir().unwrap();
    let engine = synthetic_engine(
        &directory.path().join("group-compound-validation.sqlite"),
        Arc::new(GroupedFixture(SyntheticAdapter::new())),
    );
    let originals = compound_originals(&engine).await;
    let mut prepared = composed_group_scope(&originals[..1], false);
    let child_prepared = compound_scope(&originals[1..]);
    prepared
        .intent
        .debits
        .extend(child_prepared.intent.debits.clone());
    prepared
        .intent
        .maximum_costs
        .extend(child_prepared.intent.maximum_costs.clone());
    prepared
        .intent
        .expected
        .extend(child_prepared.intent.expected.clone());
    let mut locks = prepared
        .intent
        .locks
        .iter()
        .chain(&child_prepared.intent.locks)
        .cloned()
        .collect::<BTreeSet<_>>();
    prepared.intent.locks = std::mem::take(&mut locks).into_iter().collect();
    prepared.input["amendments"].as_array_mut().unwrap().extend(
        child_prepared.input["amendments"]
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
    );
    if let EffectConstraint::CompoundSubmissionMutations { children } =
        &mut prepared.intent.expected[1]
    {
        for child in children {
            let mut value = child_prepared.valuation.clone().unwrap();
            value.movement_usd_micros = quantity("1000000");
            child.incremental_valuation = Some(value);
        }
    }
    prepared.valuation.as_mut().unwrap().movement_usd_micros = quantity("3000000");
    assert!(
        engine
            .validate_submission_amendment(&prepared, None, false)
            .unwrap()
            .is_some()
    );
    let mut bad = prepared.clone();
    bad.valuation.as_mut().unwrap().movement_usd_micros = quantity("2000000");
    assert_eq!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .unwrap_err()
            .code,
        "MUTATION_BATCH_INVALID"
    );
    let mut bad = prepared.clone();
    bad.input["amendments"].as_array_mut().unwrap().push(json!({"orderTag":"unclaimed","operationId":originals[1].id,"originalSubmission":originals[1].submission}));
    assert!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .is_err()
    );
}
