//! Synthetic delayed-dispatch tests; no native approval or network access.
use super::*;
use gofin_domain::{Quantity, Valuation};

struct RejectYoloAuthorizer;

#[async_trait::async_trait]
impl gofin_domain::Authorizer for RejectYoloAuthorizer {
    async fn authorize(
        &self,
        _challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        Err(GoFinError::validation("test authorizer is not used"))
    }

    fn acquire_submission_authority(
        &self,
        _audit: &ApprovalAudit,
    ) -> Result<Box<dyn gofin_domain::SubmissionAuthorityLease>> {
        Err(GoFinError::new(
            "YOLO_DISABLED",
            "authorization",
            "Yolo is disabled.",
            "Use a standard approval.",
        ))
    }
}

#[test]
fn a_live_grant_cannot_dispatch_after_its_price_evidence_expires_in_a_queue() {
    let asset = gofin_domain::AssetId {
        network: "SYNTHETIC".into(),
        reference: "SYNTHETIC".into(),
        decimals: 18,
    };
    let resource = ResourceId("SYNTHETIC:account".into());
    let amount = Amount {
        asset: asset.clone(),
        atoms: gofin_domain::Quantity::from_decimal("1", 18).unwrap(),
    };
    let debit = ResourceAmount {
        resource: resource.clone(),
        amount: amount.clone(),
    };
    let spec = gofin_domain::boundaries::BoundarySpec {
        fixed: json!({"account":"SYNTHETIC"}),
        parameters: json!({"maximum_quantity":"1"}),
        maximum_debits: vec![debit.clone()],
        maximum_costs: vec![],
        summary: "SYNTHETIC bounded debit".into(),
    };
    let binding =
        boundaries::authorization_binding("synthetic.debit", "SYNTHETIC-intent", &spec, 600);
    let hash = binding_hash(&binding).unwrap();
    let audit = ApprovalAudit {
        source: None,
        yolo_provenance: None,
        mechanism: "SYNTHETIC-test-only".into(),
        binding_hash: hash.clone(),
        challenge_nonce: "a".repeat(64),
        approved_at: now(),
        expires_at: (Utc::now() + chrono::Duration::seconds(90)).to_rfc3339(),
    };
    let authorization = BoundaryAuthorization {
        authorization_source: None,
        yolo_provenance: None,
        id: "SYNTHETIC-grant".into(),
        intent_id: "SYNTHETIC-intent".into(),
        operation: "synthetic.debit".into(),
        bounds: json!({"maximum_quantity":"1"}),
        spec: spec.clone(),
        approved_at: now(),
        expires_at: (Utc::now() + chrono::Duration::seconds(600)).to_rfc3339(),
        valid_for_seconds: 600,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits: vec![debit.clone()],
        remaining_costs: vec![],
        spent_debits: vec![],
        spent_costs: vec![],
        replaces: None,
    };
    let original_grant = BoundaryAuthorizationRecord {
        authorization: authorization.clone(),
        approval: audit.clone(),
        request_fingerprint: "SYNTHETIC-grant-fingerprint".into(),
        attempts: vec![],
        active_attempt: None,
    };
    let mut claimed_grant = original_grant.clone();
    claimed_grant.active_attempt = Some("SYNTHETIC-attempt".into());
    claimed_grant.attempts.push("SYNTHETIC-attempt".into());
    let mut prepared = PreparedOperation {
        operation: "synthetic.debit".into(),
        operation_version: 1,
        action_deadline: None,
        input: json!({"amount":"1"}),
        binding: json!({"amount":"1"}),
        summary: "SYNTHETIC debit".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: now(),
        expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        intent: FinancialIntent {
            debits: vec![debit],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![resource],
        },
        valuation: Some(Valuation {
            movement_usd_micros: Quantity::new("1000000").unwrap(),
            maximum_fee_usd_micros: Quantity::zero(),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: now(),
            source: "SYNTHETIC price feed".into(),
            prices: vec![AssetPrice {
                asset,
                unit_atoms: amount.atoms,
                usd_micros_per_unit: Quantity::new("1000000").unwrap(),
                observed_at: now(),
                source: "SYNTHETIC price feed".into(),
            }],
        }),
    };
    let attempt_hash = binding_hash(&boundaries::attempt_binding(&prepared)).unwrap();
    inject_boundary_marker(&mut prepared, &authorization, attempt_hash).unwrap();
    let record = OperationRecord {
        id: "SYNTHETIC-attempt".into(),
        request_id: "SYNTHETIC-attempt-request".into(),
        fingerprint: "SYNTHETIC-attempt-fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: prepared.input.clone(),
        status: Status::AwaitingApproval,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared.clone()),
        approval: Some(audit.clone()),
        authorization_source: None,
        submission: None,
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let mut store = Store::in_memory().unwrap();
    store
        .create_boundary_authorization(
            &authorization.id,
            "SYNTHETIC-grant-request",
            &authorization.intent_id,
            &original_grant.request_fingerprint,
            &to_value(&original_grant).unwrap(),
            &now(),
        )
        .unwrap();
    store
        .create_boundary_attempt(
            &authorization.id,
            &to_value(&original_grant).unwrap(),
            &to_value(&claimed_grant).unwrap(),
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            &to_value(&record).unwrap(),
            &now(),
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let mut journal = OperationSubmissionJournal {
        operation_id: record.id.clone(),
        authorization_route: SubmissionAuthorizationRoute::Financial,
        expected_binding_hash: hash,
        approval: audit,
        authorizer: None,
        boundary_authorization_id: Some(authorization.id.clone()),
        boundary_authorization_snapshot: Some(claimed_grant),
        financial_plan_id: None,
        financial_plan_snapshot: None,
        adapter: None,
        options: EngineOptions::default(),
        clock: Arc::new(gofin_domain::clock::SystemClock),
        consumed: AtomicBool::new(false),
        store: store.clone(),
        record: Mutex::new(record),
    };
    // Even a correctly bound sudo audit cannot admit a financial submission.
    journal.approval.source = Some(gofin_domain::AuthorizationSource::Sudo);
    let rejected_submission = Submission {
        kind: "SYNTHETIC".into(),
        reference: "SYNTHETIC-sudo-rejected".into(),
        data: json!({}),
    };
    assert_eq!(
        journal.assert_authorized().unwrap_err().code,
        "SUDO_CONFIGURATION_ONLY"
    );
    assert_eq!(
        journal.assert_candidate(&prepared).unwrap_err().code,
        "SUDO_CONFIGURATION_ONLY"
    );
    assert_eq!(
        journal
            .persist_submission(&rejected_submission)
            .unwrap_err()
            .code,
        "SUDO_CONFIGURATION_ONLY"
    );
    assert!(!journal.consumed.load(Ordering::SeqCst));
    // Boundary candidate validation has its own route and must still observe
    // protected Yolo revocation before admission.
    journal.approval.source = Some(gofin_domain::AuthorizationSource::Yolo);
    journal.approval.yolo_provenance = Some(gofin_domain::YoloAuthorizationProvenance {
        revision: 3,
        generation: "SYNTHETIC-generation".into(),
        checked_at: now(),
    });
    journal.authorizer = Some(Arc::new(RejectYoloAuthorizer));
    assert_eq!(
        journal.assert_candidate(&prepared).unwrap_err().code,
        "YOLO_DISABLED"
    );
    journal.approval.source = None;
    journal.approval.yolo_provenance = None;
    journal.authorizer = None;
    journal.assert_candidate(&prepared).unwrap();
    // Model time passing after dispatch preparation, without a two-minute sleep.
    let old = (Utc::now() - chrono::Duration::seconds(121)).to_rfc3339();
    {
        let mut record = journal.record.lock().unwrap();
        let value = record.preview.as_mut().unwrap().valuation.as_mut().unwrap();
        value.observed_at = old.clone();
        value.prices[0].observed_at = old;
    }
    let submission = Submission {
        kind: "SYNTHETIC".into(),
        reference: "SYNTHETIC-external-id".into(),
        data: json!({"test_only":true}),
    };
    let error = journal.persist_submission(&submission).unwrap_err();
    assert!(
        error.code.contains("STALE")
            || error.code.contains("VALUATION")
            || error.message.contains("expired"),
        "{error:?}"
    );
    assert!(
        parse_time(&authorization.expires_at).unwrap() > Utc::now(),
        "Consent remains live; it is quote evidence that expired."
    );
    assert!(!journal.consumed.load(Ordering::SeqCst));
    assert!(
        store
            .lock()
            .unwrap()
            .get_record("SYNTHETIC-attempt")
            .unwrap()
            .unwrap()["submission"]
            .is_null()
    );
}
