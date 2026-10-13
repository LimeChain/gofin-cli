// Cross-branch regression: cumulative native proof must consume finite consent.
use super::*;
#[test]
fn native_progress_consumes_boundary_debits_and_costs_once_across_restart_and_terminal() {
    let source = resource("binance-account");
    let token = asset("binance", "ETH", 18);
    let fee = CostBound {
        location: AccountingLocation::Cash {
            resource: source.clone(),
        },
        amount: Amount {
            asset: token.clone(),
            atoms: quantity("10"),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let mut operation = record(
        vec![EffectConstraint::Debit {
            resource: source.clone(),
            asset: token.clone(),
            maximum_debit: quantity("100"),
        }],
        vec![fee.clone()],
    );
    let prepared = operation.preview.as_mut().unwrap();
    prepared.intent.debits[0].amount.atoms = quantity("100");
    let authorization = BoundaryAuthorization {
        id: "synthetic-progress-grant".into(),
        intent_id: "synthetic-progress-intent".into(),
        operation: operation.operation.clone(),
        bounds: json!({}),
        spec: BoundarySpec {
            fixed: json!({}),
            parameters: json!({}),
            maximum_debits: prepared.intent.debits.clone(),
            maximum_costs: vec![fee.clone()],
            summary: "Finite synthetic progress authority".into(),
        },
        approved_at: operation.created_at.clone(),
        expires_at: prepared.expires_at.clone(),
        valid_for_seconds: 60,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits: prepared.intent.debits.clone(),
        remaining_costs: vec![fee.clone()],
        spent_debits: vec![],
        spent_costs: vec![],
        replaces: None,
    };
    let attempt_hash = binding_hash(&boundaries::attempt_binding(prepared)).unwrap();
    inject_boundary_marker(prepared, &authorization, attempt_hash).unwrap();
    trading_rebind(&mut operation);
    let boundary = BoundaryAuthorizationRecord {
        authorization,
        approval: operation.approval.clone().unwrap(),
        request_fingerprint: "synthetic-progress-grant-fingerprint".into(),
        attempts: vec![operation.id.clone()],
        active_attempt: Some(operation.id.clone()),
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("finite-progress.sqlite");
    let mut store = Store::open(&path).unwrap();
    let mut unsubmitted = operation.clone();
    unsubmitted.submission = None;
    unsubmitted.status = Status::AwaitingApproval;
    store
        .create_request(
            &operation.id,
            &operation.request_id,
            &operation.fingerprint,
            &operation.operation,
            None,
            "awaiting_approval",
            &to_value(&unsubmitted).unwrap(),
            &operation.created_at,
        )
        .unwrap();
    let submission = operation.submission.as_ref().unwrap();
    store
        .persist_submission(
            &operation.id,
            &SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            &operation.created_at,
        )
        .unwrap();
    store
        .create_boundary_authorization(
            &boundary.authorization.id,
            "synthetic-progress-grant-request",
            &boundary.authorization.intent_id,
            &boundary.request_fingerprint,
            &to_value(&boundary).unwrap(),
            &operation.created_at,
        )
        .unwrap();
    store
        .reserve(
            &operation.id,
            &store_reservations_for_intent(operation.preview.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
    let engine = Engine::new(
        store,
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    operation = engine.load_record(&operation.id).unwrap();
    let effects = |debit: &str, cost: &str| {
        vec![
            FinancialEffect::CashDebit {
                resource: source.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity(debit),
                },
            },
            FinancialEffect::Cost {
                location: fee.location.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: quantity(cost),
                },
                category: "synthetic native fee".into(),
                treatment: fee.treatment.clone(),
            },
        ]
    };
    let first = progress_proof(
        &operation,
        1,
        "source-progress-1",
        &[("fill-1", "digest-1")],
        effects("40", "2"),
    );
    let pending = engine
        .apply_reconciliation(
            operation,
            Reconciliation {
                status: Status::Pending,
                evidence: first.evidence.clone(),
                verified: Some(first.clone()),
            },
        )
        .unwrap();
    let grant = engine
        .load_boundary_record(&boundary.authorization.id)
        .unwrap();
    assert_eq!(
        grant.authorization.spent_debits[0].amount.atoms,
        quantity("40")
    );
    assert_eq!(
        grant.authorization.spent_costs[0].amount.atoms,
        quantity("2")
    );
    assert_eq!(
        grant.authorization.remaining_debits[0].amount.atoms,
        quantity("60")
    );
    assert_eq!(
        grant.authorization.remaining_costs[0].amount.atoms,
        quantity("8")
    );
    drop(engine);
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let persisted = engine.load_record(&pending.id).unwrap();
    let duplicate = engine
        .apply_reconciliation(
            persisted,
            Reconciliation {
                status: Status::Pending,
                evidence: first.evidence.clone(),
                verified: Some(first),
            },
        )
        .unwrap();
    assert_eq!(
        engine
            .load_boundary_record(&boundary.authorization.id)
            .unwrap(),
        grant
    );
    let terminal = progress_proof(
        &duplicate,
        2,
        "source-progress-2",
        &[("fill-1", "digest-1"), ("fill-2", "digest-2")],
        effects("100", "5"),
    );
    let complete = engine
        .apply_reconciliation(
            duplicate,
            Reconciliation {
                status: Status::Completed,
                evidence: terminal.evidence.clone(),
                verified: Some(terminal),
            },
        )
        .unwrap();
    assert_eq!(complete.status, Status::Completed);
    let final_grant = engine
        .load_boundary_record(&boundary.authorization.id)
        .unwrap();
    assert_eq!(
        final_grant.authorization.spent_debits[0].amount.atoms,
        quantity("100")
    );
    assert_eq!(
        final_grant.authorization.spent_costs[0].amount.atoms,
        quantity("5")
    );
    assert_eq!(
        final_grant.authorization.remaining_debits[0].amount.atoms,
        quantity("0")
    );
    assert_eq!(
        final_grant.authorization.remaining_costs[0].amount.atoms,
        quantity("5")
    );
    assert!(final_grant.active_attempt.is_none());
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservation_count(&complete.id)
            .unwrap(),
        0
    );
}

#[test]
fn in_flight_progress_protocol_cannot_replace_immutable_native_events() {
    let source = resource("binance-account");
    let token = asset("binance", "ETH", 18);
    for first_is_sequence in [false, true] {
        let mut operation = record(
            vec![EffectConstraint::Debit {
                resource: source.clone(),
                asset: token.clone(),
                maximum_debit: quantity("100"),
            }],
            vec![],
        );
        operation.preview.as_mut().unwrap().intent.debits[0]
            .amount
            .atoms = quantity("100");
        trading_rebind(&mut operation);
        let mut store = Store::in_memory().unwrap();
        let mut unsubmitted = operation.clone();
        unsubmitted.status = Status::AwaitingApproval;
        unsubmitted.submission = None;
        store
            .create_request(
                &operation.id,
                &operation.request_id,
                &operation.fingerprint,
                &operation.operation,
                None,
                "awaiting_approval",
                &to_value(&unsubmitted).unwrap(),
                &operation.created_at,
            )
            .unwrap();
        let submission = operation.submission.as_ref().unwrap();
        store
            .persist_submission(
                &operation.id,
                &SubmissionRow {
                    kind: submission.kind.clone(),
                    reference: submission.reference.clone(),
                    data: submission.data.clone(),
                },
                &operation.created_at,
            )
            .unwrap();
        store
            .reserve(
                &operation.id,
                &store_reservations_for_intent(operation.preview.as_ref().unwrap()).unwrap(),
            )
            .unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        operation = engine.load_record(&operation.id).unwrap();
        let financial_effect = FinancialEffect::CashDebit {
            resource: source.clone(),
            amount: Amount {
                asset: token.clone(),
                atoms: quantity("40"),
            },
        };
        let bundle = |record: &OperationRecord, sequence: bool| {
            if sequence {
                progress_proof(
                    record,
                    1,
                    "native-snapshot-1",
                    &[("native-event-1", "digest-1")],
                    vec![financial_effect.clone()],
                )
            } else {
                let mut verified = proof(record, vec![financial_effect.clone()]);
                verified.evidence =
                    json!({"proofEvents":[{"eventId":"native-event-1","amount":"40"}]});
                verified.proof_digest = verified_effect_digest(&verified).unwrap();
                verified
            }
        };
        let first = bundle(&operation, first_is_sequence);
        let pending = engine
            .apply_reconciliation(
                operation,
                Reconciliation {
                    status: Status::Pending,
                    evidence: first.evidence.clone(),
                    verified: Some(first),
                },
            )
            .unwrap();
        let switched = bundle(&pending, !first_is_sequence);
        let error = engine
            .apply_reconciliation(
                pending.clone(),
                Reconciliation {
                    status: Status::Pending,
                    evidence: switched.evidence.clone(),
                    verified: Some(switched),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PROGRESS_PROOF");
        assert_eq!(engine.load_record(&pending.id).unwrap(), pending);
    }
}
