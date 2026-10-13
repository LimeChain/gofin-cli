// Native mutation markers and scoped unresolved costs must share one transaction.
#[tokio::test]
async fn composed_cost_consumption_rolls_back_and_survives_completed_original_owners() {
    use fin_domain::compound_mutations::{ChildMutationOutcome, MutationChildProof};
    use fin_domain::unresolved_consumption::{ConsumptionScope, digest};
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("composed-consumption.sqlite");
    let e = synthetic_engine(&path, Arc::new(GroupedFixture(SyntheticAdapter::new())));
    let originals = compound_originals(&e).await;
    let mut p = unified_group_compound_new_write(&originals);
    let cap = CostBound {
        location: AccountingLocation::Cash {
            resource: resource("binance-account"),
        },
        amount: Amount {
            asset: asset("binance", "ETH", 18),
            atoms: quantity("10000000000000000"),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let price = fin_domain::AssetPrice {
        asset: cap.amount.asset.clone(),
        usd_micros_per_unit: quantity("1000000"),
        unit_atoms: quantity("1000000000000000000"),
        observed_at: now(),
        source: "synthetic exact independent cost price".into(),
    };
    let mut scope: fin_domain::amendments::AmendmentScope =
        from_value(p.binding["mutationNewWriteScope"].clone()).unwrap();
    scope.intent.maximum_costs = vec![cap.clone()];
    scope.valuation.as_mut().unwrap().maximum_fee_usd_micros = quantity("10000");
    scope.valuation.as_mut().unwrap().prices = vec![price.clone()];
    let mut scoped_prepared = p.clone();
    scoped_prepared.intent = scope.intent.clone();
    scoped_prepared.binding = scope.binding.clone();
    scoped_prepared.valuation = scope.valuation.clone();
    fin_domain::value_bounds::bind(&mut scoped_prepared);
    scope.binding = scoped_prepared.binding;
    p.binding["mutationNewWriteScope"] = to_value(&scope).unwrap();
    // A larger parent allowance must not mask the new-write partition's fee.
    let mut parent_cap = cap;
    parent_cap.amount.atoms = quantity("30000000000000000");
    p.intent.maximum_costs = vec![parent_cap];
    p.valuation.as_mut().unwrap().maximum_fee_usd_micros = quantity("30000");
    p.valuation.as_mut().unwrap().prices = vec![price];
    e.validate_submission_amendment(&p, None, false).unwrap();
    let mut record = originals[0].clone();
    record.id = "synthetic-composed-cost".into();
    record.request_id = record.id.clone();
    record.fingerprint = record.id.clone();
    record.input = p.input.clone();
    record.preview = Some(p.clone());
    record.status = Status::AwaitingApproval;
    record.submission = None;
    record.effects = None;
    record.last_reconciliation_evidence = None;
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(&p)).unwrap();
    e.store
        .lock()
        .unwrap()
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            None,
            "awaiting_approval",
            &to_value(&record).unwrap(),
            &now(),
        )
        .unwrap();
    e.reserve_ordinary_operation(&mut record, &p, None, None).unwrap();
    let dispatch = Submission {
        kind: "synthetic_tx".into(),
        reference: "synthetic-composed-cost-dispatch".into(),
        data: json!({"synthetic":true}),
    };
    e.store
        .lock()
        .unwrap()
        .persist_submission(
            &record.id,
            &SubmissionRow {
                kind: dispatch.kind.clone(),
                reference: dispatch.reference.clone(),
                data: dispatch.data.clone(),
            },
            &now(),
        )
        .unwrap();
    record = e.load_record(&record.id).unwrap();
    let EffectConstraint::GroupedSubmissionAmendments { group } = &p.intent.expected[0] else {
        unreachable!()
    };
    let first = MutationChildProof {
        child_tag: "member-0".into(),
        operation_id: originals[0].id.clone(),
        submission: originals[0].submission.clone().unwrap(),
        outcome: ChildMutationOutcome::Accepted { successor: None },
        native_evidence: json!({"bindingReplacementDigest":binding_hash(&to_value(&group.children[0].binding_replacements).unwrap()).unwrap(),"nativeAudit":{"id":"composed-audit"},"nativeReadback":{"size":"2"}}),
    };
    let second = MutationChildProof {
        child_tag: "member-1".into(),
        operation_id: originals[0].id.clone(),
        submission: originals[0].submission.clone().unwrap(),
        outcome: ChildMutationOutcome::Rejected {
            code: "INVALID_ORDER".into(),
        },
        native_evidence: json!({"error":"sealed rejected second member"}),
    };
    let independent = MutationChildProof {
        child_tag: "child-0".into(),
        operation_id: originals[1].id.clone(),
        submission: originals[1].submission.clone().unwrap(),
        outcome: ChildMutationOutcome::Accepted { successor: None },
        native_evidence: json!({"audit":"sealed independent edit"}),
    };
    let mut v = unified_receipt(&record, vec![first, second], independent);
    v.effects
        .retain(|effect| !matches!(effect, FinancialEffect::Conversion { .. }));
    v.evidence["proofEvents"]
        .as_array_mut()
        .unwrap()
        .retain(|row| row["eventId"] != "synthetic-new-financial-receipt");
    v.proof_digest = verified_effect_digest(&v).unwrap();
    let mut cost = consumed_costs(
        &record,
        "20000000000000000",
        json!([{"eventId":"composed-native-fee","atoms":"20000000000000000"}]),
    );
    cost.scope = ConsumptionScope::NewMutationFinancialScope;
    cost.proof_digest = digest(&cost).unwrap();
    let mut next = record.clone();
    next.status = Status::Unknown;
    next.effects = Some(v.clone());
    next.updated_at = now();
    let groups = e.grouped_settlements(&next).unwrap();
    let mut children = e.compound_child_settlements(&next).unwrap().unwrap();
    let remaining = e.remaining_mutation_reservations(&next, &groups).unwrap();
    children[0].proof["native_evidence"] =
        json!({"changed":"cannot replace immutable accepted evidence"});
    let retention = fin_store::unresolved_consumption::ConsumptionRetention {
        proof: cost.clone(),
        additional_usd_micros: "10000".into(),
    };
    assert!(
        e.store
            .lock()
            .unwrap()
            .commit_mutation_batch_progress_with_consumption(
                &record.id,
                &to_value(&record).unwrap(),
                &to_value(&next).unwrap(),
                &to_value(&v).unwrap(),
                &to_value(&v.effects).unwrap(),
                &groups,
                &children,
                &remaining,
                &retention,
                &next.updated_at
            )
            .is_err()
    );
    assert!(
        e.store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&record.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(e.load_record(&record.id).unwrap(), record);
    for o in &originals {
        assert!(e.amendment_context(&o.id).unwrap().revisions.is_empty());
        assert_eq!(e.load_record(&o.id).unwrap(), *o);
    }
    // Neither the larger parent cap nor an accepted edit permits laundering
    // a breached new order into ordinary financial ledger effects.
    let mut forged = v.clone();
    forged.effects.push(FinancialEffect::Conversion {
        resource: resource("binance-account"),
        spent: Amount {
            asset: asset("binance", "ETH", 18),
            atoms: quantity("1000000000000000000"),
        },
        received: Amount {
            asset: asset("binance", "USDC", 6),
            atoms: quantity("1000000"),
        },
    });
    forged.proof_digest = verified_effect_digest(&forged).unwrap();
    let mut wrong = cost_reconciliation(&cost);
    wrong.verified = Some(forged);
    assert!(
        e.commit_unresolved_cost_consumption(&record, &wrong, &p.intent)
            .is_err()
    );
    let mut whole = cost.clone();
    whole.scope = ConsumptionScope::WholeOperation;
    whole.proof_digest = digest(&whole).unwrap();
    let mut wrong = cost_reconciliation(&whole);
    wrong.verified = Some(v.clone());
    assert!(
        e.commit_unresolved_cost_consumption(&record, &wrong, &p.intent)
            .is_err()
    );
    let mut malformed = cost.clone();
    malformed.proof_digest = "altered".into();
    assert!(
        e.commit_unresolved_cost_consumption(&record, &cost_reconciliation(&malformed), &p.intent)
            .is_err()
    );
    assert_eq!(e.load_record(&record.id).unwrap(), record);
    let mut reconciliation = cost_reconciliation(&cost);
    reconciliation.verified = Some(v.clone());
    let held = e
        .apply_reconciliation(record, reconciliation.clone())
        .unwrap();
    assert_eq!(held.status, Status::Unknown);
    assert_eq!(held.error.as_ref().unwrap().code, "COST_BOUND_EXCEEDED");
    assert_eq!(
        e.store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&held.id)
            .unwrap(),
        Some("10000".into())
    );
    assert!(
        held.effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .all(|effect| matches!(
                effect,
                FinancialEffect::GroupedSubmissionAmendments { .. }
                    | FinancialEffect::CompoundSubmissionMutations { .. }
            ))
    );
    for o in &originals {
        assert_eq!(e.amendment_context(&o.id).unwrap().revisions.len(), 1);
        assert_eq!(e.load_record(&o.id).unwrap(), *o);
    }
    // Original no-fill cancellation can finish while the breached new order is
    // unresolved. A duplicate marker must neither recreate locks nor revisions.
    for o in originals {
        let mut cancelled = VerifiedEffects {
            evidence_version: 1,
            submission: o.submission.clone().unwrap(),
            effects: vec![],
            evidence: json!({"proofEvents":[{"eventId":format!("cancelled-native:{}",o.id),"status":"cancelled_no_fill"}]}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        cancelled.proof_digest = verified_effect_digest(&cancelled).unwrap();
        let terminal = e
            .apply_reconciliation(
                o,
                Reconciliation {
                    status: Status::Failed,
                    evidence: cancelled.evidence.clone(),
                    verified: Some(cancelled),
                },
            )
            .unwrap();
        assert_eq!(terminal.status, Status::Failed);
    }
    let source_ids = groups
        .iter()
        .map(|g| g.original_id.clone())
        .chain(children.iter().map(|c| c.original_id.clone()))
        .collect::<Vec<_>>();
    drop(e);
    let e = synthetic_engine(&path, Arc::new(GroupedFixture(SyntheticAdapter::new())));
    let restored = e.load_record(&held.id).unwrap();
    let duplicate = e
        .apply_reconciliation(restored, reconciliation.clone())
        .unwrap();
    assert_eq!(duplicate.status, Status::Unknown);
    assert_eq!(
        e.store
            .lock()
            .unwrap()
            .unresolved_consumption_usd(&duplicate.id)
            .unwrap(),
        Some("10000".into())
    );
    for id in source_ids {
        assert_eq!(e.amendment_context(&id).unwrap().revisions.len(), 1);
        assert_eq!(e.store.lock().unwrap().reservation_count(&id).unwrap(), 0);
    }
    let mut changed = reconciliation;
    let proof = changed.verified.as_mut().unwrap();
    if let FinancialEffect::CompoundSubmissionMutations { children } = &mut proof.effects[1] {
        children[0].native_evidence["changed"] = json!(true);
    }
    proof.proof_digest = verified_effect_digest(proof).unwrap();
    assert!(
        e.commit_unresolved_cost_consumption(&duplicate, &changed, &p.intent)
            .is_err()
    );
    assert_eq!(e.load_record(&duplicate.id).unwrap(), duplicate);
}
