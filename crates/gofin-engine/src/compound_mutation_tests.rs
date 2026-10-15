// Included inside the engine's synthetic test module; no live credentials.
fn compound_input(children: &[gofin_domain::compound_mutations::MutationChild]) -> Value {
    json!({"amendments":children.iter().map(|child|json!({"orderTag":child.child_tag,"operationId":child.operation_id,"originalSubmission":child.submission})).collect::<Vec<_>>()})
}
fn compound_scope(originals: &[OperationRecord]) -> PreparedOperation {
    use gofin_domain::compound_mutations::{ChildMutation, MutationChild};
    let mut prepared = originals[0].preview.clone().unwrap();
    let mut children = Vec::new();
    let mut debits = Vec::new();
    let mut locks = BTreeSet::new();
    for (index, original) in originals.iter().enumerate() {
        let revision = synthetic_amendment(
            original,
            "2000000000000000000",
            "2000000",
            "1000000000000000000",
        );
        let EffectConstraint::SubmissionAmendment {
            revision: scope, ..
        } = revision.intent.expected[0].clone()
        else {
            unreachable!()
        };
        children.push(MutationChild {
            child_tag: format!("child-{index}"),
            operation_id: original.id.clone(),
            submission: original.submission.clone().unwrap(),
            mutation: ChildMutation::Amend { revision: scope },
            incremental_valuation: revision.valuation.clone(),
        });
        debits.extend(revision.intent.debits);
        locks.extend(
            original
                .preview
                .as_ref()
                .unwrap()
                .intent
                .locks
                .iter()
                .cloned(),
        );
    }
    prepared.input = compound_input(&children);
    prepared.binding = json!({"synthetic_compound":children});
    prepared.intent = FinancialIntent {
        debits,
        maximum_costs: Vec::new(),
        expected: vec![EffectConstraint::CompoundSubmissionMutations { children }],
        locks: locks.into_iter().collect(),
    };
    prepared.valuation.as_mut().unwrap().movement_usd_micros =
        quantity(&(originals.len() * 1000000).to_string());
    prepared
}
async fn compound_originals(engine: &Engine) -> Vec<OperationRecord> {
    let first = engine
        .execute(OperationRequest {
            request_id: "synthetic-compound-original-1".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    let mut second = first.clone();
    second.id = "synthetic-compound-original-2".into();
    second.request_id = second.id.clone();
    second.fingerprint = second.id.clone();
    second.status = Status::AwaitingApproval;
    second.submission = None;
    second.effects = None;
    let mut preview = second.preview.take().unwrap();
    let encoded = serde_json::to_string(&preview)
        .unwrap()
        .replace("binance-account", "secondary-account");
    preview = serde_json::from_str(&encoded).unwrap();
    second.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(&preview)).unwrap();
    second.preview = Some(preview.clone());
    engine
        .store
        .lock()
        .unwrap()
        .create_request(
            &second.id,
            &second.request_id,
            &second.fingerprint,
            &second.operation,
            None,
            "awaiting_approval",
            &to_value(&second).unwrap(),
            &now(),
        )
        .unwrap();
    engine
        .reserve_ordinary_operation(&mut second, &preview, None, None)
        .unwrap();
    let submission = Submission {
        kind: "synthetic_tx".into(),
        reference: "synthetic-original-2-tx".into(),
        data: json!({"synthetic":true}),
    };
    engine
        .store
        .lock()
        .unwrap()
        .persist_submission(
            &second.id,
            &SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            &now(),
        )
        .unwrap();
    second.submission = Some(submission);
    second.status = Status::Pending;
    engine.update_record(&second).unwrap();
    vec![first, second]
}
fn compound_receipt(
    mutation: &OperationRecord,
    children: Vec<gofin_domain::compound_mutations::MutationChildProof>,
) -> VerifiedEffects {
    let events=children.iter().map(|child|json!({"eventId":format!("synthetic-child:{}",child.child_tag),"outcome":child})).collect::<Vec<_>>();
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: mutation.submission.clone().unwrap(),
        effects: vec![FinancialEffect::CompoundSubmissionMutations { children }],
        evidence: json!({"proofEvents":events}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    verified
}
#[tokio::test]
async fn compound_validation_enforces_aggregate_increases_and_journal_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let engine = synthetic_engine(
        &directory.path().join("compound-validation.sqlite"),
        Arc::new(SyntheticAdapter::new()),
    );
    let originals = compound_originals(&engine).await;
    let prepared = compound_scope(&originals);
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
    assert!(
        engine
            .validate_submission_amendment(&prepared, Some("run"), false)
            .is_err()
    );
    let mut bad = prepared.clone();
    bad.valuation.as_mut().unwrap().movement_usd_micros = quantity("1000000");
    assert!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .is_err()
    );
    let mut bad = prepared.clone();
    bad.intent.debits.pop();
    assert!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .is_err()
    );
    let mut bad = prepared.clone();
    bad.input["amendments"][0]["originalSubmission"]["reference"] = json!("unowned");
    assert!(
        engine
            .validate_submission_amendment(&bad, None, false)
            .is_err()
    );
}
#[tokio::test]
async fn compound_partial_children_survive_restart_without_duplicate_revisions_or_fill_ownership() {
    use gofin_domain::compound_mutations::{ChildMutationOutcome, MutationChildProof};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("compound-progress.sqlite");
    let engine = synthetic_engine(&path, Arc::new(SyntheticAdapter::new()));
    let originals = compound_originals(&engine).await;
    let prepared = compound_scope(&originals);
    let mut mutation = originals[0].clone();
    mutation.id = "synthetic-compound-mutation".into();
    mutation.request_id = mutation.id.clone();
    mutation.fingerprint = mutation.id.clone();
    mutation.input = prepared.input.clone();
    mutation.preview = Some(prepared.clone());
    mutation.status = Status::AwaitingApproval;
    mutation.submission = None;
    mutation.effects = None;
    mutation.last_reconciliation_evidence = None;
    mutation.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(&prepared)).unwrap();
    engine
        .store
        .lock()
        .unwrap()
        .create_request(
            &mutation.id,
            &mutation.request_id,
            &mutation.fingerprint,
            &mutation.operation,
            None,
            "awaiting_approval",
            &to_value(&mutation).unwrap(),
            &now(),
        )
        .unwrap();
    engine
        .validate_submission_amendment_for_attempt(&prepared, None, false, Some(&mutation.id))
        .unwrap();
    engine
        .reserve_ordinary_operation(&mut mutation, &prepared, None, None)
        .unwrap();
    let submission = Submission {
        kind: "synthetic_tx".into(),
        reference: "synthetic-compound-dispatch".into(),
        data: json!({"synthetic":true}),
    };
    engine
        .store
        .lock()
        .unwrap()
        .persist_submission(
            &mutation.id,
            &SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            &now(),
        )
        .unwrap();
    mutation.submission = Some(submission);
    mutation.status = Status::Submitted;
    mutation = engine.load_record(&mutation.id).unwrap();
    let first = MutationChildProof {
        child_tag: "child-0".into(),
        operation_id: originals[0].id.clone(),
        submission: originals[0].submission.clone().unwrap(),
        outcome: ChildMutationOutcome::Accepted { successor: None },
        native_evidence: json!({"synthetic":"accepted native amendment"}),
    };
    let verified = compound_receipt(&mutation, vec![first.clone()]);
    let partial = engine
        .apply_reconciliation(
            mutation,
            Reconciliation {
                status: Status::Pending,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            },
        )
        .unwrap();
    assert_eq!(
        engine
            .amendment_context(&originals[0].id)
            .unwrap()
            .revisions
            .len(),
        1
    );
    assert!(
        engine
            .amendment_context(&originals[1].id)
            .unwrap()
            .revisions
            .is_empty()
    );
    assert!(!engine.has_pending_amendment(&originals[0].id).unwrap());
    assert!(engine.has_pending_amendment(&originals[1].id).unwrap());
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .reservation_count(&partial.id)
            .unwrap()
            > 0
    );
    let missing = compound_receipt(&partial, vec![first.clone()]);
    assert!(
        engine
            .apply_reconciliation(
                partial.clone(),
                Reconciliation {
                    status: Status::Completed,
                    evidence: missing.evidence.clone(),
                    verified: Some(missing)
                }
            )
            .is_err()
    );
    drop(engine);
    let engine = synthetic_engine(&path, Arc::new(SyntheticAdapter::new()));
    let restored = engine.load_record(&partial.id).unwrap();
    assert_eq!(restored.effects, partial.effects);
    let mut changed = first.clone();
    changed.native_evidence = json!({"synthetic":"conflicting outcome"});
    let receipt = compound_receipt(&restored, vec![changed]);
    assert!(
        engine
            .apply_reconciliation(
                restored.clone(),
                Reconciliation {
                    status: Status::Pending,
                    evidence: receipt.evidence.clone(),
                    verified: Some(receipt)
                }
            )
            .is_err()
    );
    let second = MutationChildProof {
        child_tag: "child-1".into(),
        operation_id: originals[1].id.clone(),
        submission: originals[1].submission.clone().unwrap(),
        outcome: ChildMutationOutcome::Rejected {
            code: "insufficientFunds".into(),
        },
        native_evidence: json!({"synthetic":"explicit native rejection"}),
    };
    let verified = compound_receipt(&restored, vec![first, second]);
    let completed = engine
        .apply_reconciliation(
            restored,
            Reconciliation {
                status: Status::Failed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            },
        )
        .unwrap();
    assert_eq!(completed.status, Status::Failed);
    assert_eq!(
        engine
            .amendment_context(&originals[0].id)
            .unwrap()
            .revisions
            .len(),
        1
    );
    assert!(
        engine
            .amendment_context(&originals[1].id)
            .unwrap()
            .revisions
            .is_empty()
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservation_count(&completed.id)
            .unwrap(),
        0
    );
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "4000000");
    let mut expired = completed.clone();
    expired.updated_at = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
    engine.update_record(&expired).unwrap();
    assert_eq!(
        engine.allocation_used_usd_micros().unwrap(),
        "4000000",
        "whole compound incremental charge remains counted while any original remains unresolved"
    );
    for original in &originals {
        assert_eq!(engine.load_record(&original.id).unwrap(), *original);
        assert!(!engine.has_pending_amendment(&original.id).unwrap());
    }
}

#[tokio::test]
async fn expired_unsubmitted_compound_releases_only_its_own_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let engine = synthetic_engine(
        &directory.path().join("compound-expiry.sqlite"),
        Arc::new(SyntheticAdapter::new()),
    );
    let originals = compound_originals(&engine).await;
    let prepared = compound_scope(&originals);
    let mut mutation = originals[0].clone();
    mutation.id = "synthetic-unsubmitted-compound".into();
    mutation.request_id = mutation.id.clone();
    mutation.fingerprint = mutation.id.clone();
    mutation.input = prepared.input.clone();
    mutation.preview = Some(prepared.clone());
    mutation.status = Status::AwaitingApproval;
    mutation.submission = None;
    mutation.effects = None;
    mutation.last_reconciliation_evidence = None;
    mutation.approval = None;
    engine
        .store
        .lock()
        .unwrap()
        .create_request(
            &mutation.id,
            &mutation.request_id,
            &mutation.fingerprint,
            &mutation.operation,
            None,
            "awaiting_approval",
            &to_value(&mutation).unwrap(),
            &now(),
        )
        .unwrap();
    engine
        .reserve_ordinary_operation(&mut mutation, &prepared, None, None)
        .unwrap();
    assert!(engine.has_pending_amendment(&originals[0].id).unwrap());
    mutation.status = Status::Expired;
    mutation.updated_at = now();
    engine
        .commit_terminal(
            mutation.clone(),
            json!({"synthetic":"approval expired before dispatch"}),
            json!([]),
            None,
        )
        .unwrap();
    assert_eq!(
        engine.load_record(&mutation.id).unwrap().status,
        Status::Expired
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservation_count(&mutation.id)
            .unwrap(),
        0
    );
    for original in &originals {
        assert_eq!(engine.load_record(&original.id).unwrap(), *original);
        assert!(!engine.has_pending_amendment(&original.id).unwrap());
        assert!(
            engine
                .store
                .lock()
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
    }
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "2000000");
    engine
        .validate_submission_amendment(&prepared, None, false)
        .unwrap();
}

#[tokio::test]
async fn compound_falling_prices_cannot_offset_independent_positive_quantity_increments() {
    use gofin_domain::compound_mutations::ChildMutation;
    let directory = tempfile::tempdir().unwrap();
    let engine = synthetic_engine(
        &directory.path().join("compound-falling-prices.sqlite"),
        Arc::new(SyntheticAdapter::new()),
    );
    let originals = compound_originals(&engine).await;
    let mut prepared = compound_scope(&originals);
    let EffectConstraint::CompoundSubmissionMutations { children } =
        &mut prepared.intent.expected[0]
    else {
        unreachable!()
    };
    for child in children {
        let ChildMutation::Amend { revision } = &mut child.mutation else {
            unreachable!()
        };
        // Quantity doubles while the market price falls by half: the full
        // scalar stays flat, but each independently valued new unit costs $1.
        revision.valuation.as_mut().unwrap().movement_usd_micros = quantity("1000000");
        let mut incremental = revision.valuation.clone().unwrap();
        incremental.movement_usd_micros = quantity("1000000");
        child.incremental_valuation = Some(incremental);
    }
    assert!(
        engine
            .validate_submission_amendment(&prepared, None, false)
            .is_ok()
    );
    let mut under_reserved = prepared.clone();
    under_reserved
        .valuation
        .as_mut()
        .unwrap()
        .movement_usd_micros = quantity("1000000");
    assert_eq!(
        engine
            .validate_submission_amendment(&under_reserved, None, false)
            .unwrap_err()
            .code,
        "COMPOUND_MUTATION_INVALID"
    );
    let mut missing = prepared.clone();
    let EffectConstraint::CompoundSubmissionMutations { children } =
        &mut missing.intent.expected[0]
    else {
        unreachable!()
    };
    children[0].incremental_valuation = None;
    assert_eq!(
        engine
            .validate_submission_amendment(&missing, None, false)
            .unwrap_err()
            .code,
        "COMPOUND_MUTATION_INVALID"
    );
    let mut stale = prepared;
    let EffectConstraint::CompoundSubmissionMutations { children } = &mut stale.intent.expected[0]
    else {
        unreachable!()
    };
    children[0]
        .incremental_valuation
        .as_mut()
        .unwrap()
        .observed_at = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    assert_eq!(
        engine
            .validate_submission_amendment(&stale, None, false)
            .unwrap_err()
            .code,
        "VALUATION_UNAVAILABLE"
    );
}
