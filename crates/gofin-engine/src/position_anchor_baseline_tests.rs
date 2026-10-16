//! Position-anchor bootstrapping must use exact, immutable pre-dispatch evidence.
use super::*;
use async_trait::async_trait;
use gofin_domain::{FinancialState, PositionState, Quantity, SignedQuantity};
use std::sync::atomic::{AtomicUsize, Ordering};

const RESOURCE: &str = "solana:mainnet-beta:wallet:SYNTHETIC";
const POSITION: &str = "account-funding:SYNTHETIC-ATA";

struct PositionObserver {
    resource: ResourceId,
    definition_name: String,
    positions: Vec<PositionState>,
    coverage: Vec<String>,
    calls: AtomicUsize,
    fail_snapshot: bool,
}

#[async_trait]
impl Adapter for PositionObserver {
    fn definitions(&self) -> Vec<Definition> {
        vec![Definition {
            name: self.definition_name.clone(),
            description: "Synthetic account closure observer fixture".into(),
            kind: OperationKind::Write,
            execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic fixture".into(),
            completion: "synthetic fixture".into(),
            semantics: json!({}),
        }]
    }

    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        vec![gofin_domain::ConnectionMetadata {
            name: "synthetic-observer".into(),
            provider: "synthetic".into(),
            resource: self.resource.clone(),
            role: None,
            parent: None,
            network: Some("solana".into()),
            address: Some("SYNTHETIC".into()),
            credential_ref: None,
            signing_enabled: false,
            details: json!({"cluster":"mainnet-beta"}),
        }]
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_snapshot {
            return Err(GoFinError::new(
                "OBSERVER_READ_TIMEOUT",
                "snapshot",
                "SYNTHETIC_PRIVATE_OBSERVER_ERROR",
                "SYNTHETIC_PRIVATE_OBSERVER_RECOVERY",
            )
            .with_details(json!({
                "private":"SYNTHETIC_PRIVATE_OBSERVER_DETAILS",
                "method":"getMultipleAccounts",
                "attempt":2,
                "elapsed_ms":731,
                "stage":"http_status",
                "http_status":429,
                "rpc_code":-32005,
                "url":"https://user:secret@provider.invalid/token",
                "params":["SYNTHETIC_PRIVATE_ADDRESS"],
            })));
        }
        Ok(StateSnapshot {
            state: FinancialState {
                positions: self.positions.clone(),
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: self.coverage.clone(),
            pending: vec![],
        })
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        if scope.resources != vec![self.resource.clone()] {
            return Err(GoFinError::validation(
                "Unexpected synthetic observer scope.",
            ));
        }
        self.snapshot().await
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(GoFinError::validation("Unused fixture read."))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Unused fixture prepare."))
    }

    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Unused fixture revalidate."))
    }

    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(GoFinError::validation("Unused fixture submit."))
    }

    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(GoFinError::validation("Unused fixture reconcile."))
    }
}

struct PositionLifecycleAdapter {
    observer: PositionObserver,
    finalized: std::sync::atomic::AtomicBool,
    submissions: AtomicUsize,
    clock: Option<Arc<PositionTestClock>>,
    snapshot_advance: chrono::Duration,
}

struct PositionTestClock(Mutex<chrono::DateTime<chrono::Utc>>);

impl gofin_domain::clock::Clock for PositionTestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }
}

#[async_trait]
impl Adapter for PositionLifecycleAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.observer.definitions()
    }

    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.observer.snapshot_identities()
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.observer.snapshot().await
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        let snapshot = self.observer.snapshot_scoped(scope).await?;
        if let Some(clock) = &self.clock {
            *clock.0.lock().unwrap() += self.snapshot_advance;
        }
        Ok(snapshot)
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        let mut prepared = prepared();
        prepared.operation = operation.to_owned();
        prepared.input = input.clone();
        prepared.valuation = Some(gofin_domain::Valuation {
            movement_usd_micros: Quantity::zero(),
            maximum_fee_usd_micros: Quantity::zero(),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: now(),
            source: "synthetic position release".into(),
            prices: vec![],
        });
        if let Some(clock) = &self.clock {
            prepared.expires_at = (gofin_domain::clock::Clock::now(clock.as_ref())
                + chrono::Duration::seconds(5))
            .to_rfc3339();
        }
        Ok(prepared)
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        _: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let submission = Submission {
            kind: "synthetic-position".into(),
            reference: "SYNTHETIC-POSITION-SIGNATURE".into(),
            data: json!({"message_hash":"synthetic-message-hash"}),
        };
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        self.submissions.fetch_add(1, Ordering::SeqCst);
        Ok(submission)
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        if !self.finalized.load(Ordering::SeqCst) {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"phase":"synthetic pending"}),
                verified: None,
            });
        }
        let resource = resource();
        let native = asset();
        let amount = Quantity::new("2039280").unwrap();
        let mut post_anchor = anchor("0");
        post_anchor.observed_at = now();
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![
                FinancialEffect::PositionChange {
                    resource: resource.clone(),
                    position: POSITION.into(),
                    asset: native.clone(),
                    delta: SignedQuantity::new("-2039280").unwrap(),
                    scaled_receipt: None,
                    anchor: Some(post_anchor),
                },
                FinancialEffect::OriginMovement {
                    from: AccountingLocation::Position {
                        resource: resource.clone(),
                        position: POSITION.into(),
                        position_kind: PositionKind::RefundableAccountFunding,
                    },
                    to: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    debit: Amount {
                        asset: native.clone(),
                        atoms: amount.clone(),
                    },
                    credit: Amount {
                        asset: native.clone(),
                        atoms: amount.clone(),
                    },
                },
                FinancialEffect::UsableCredit {
                    to: resource,
                    amount: Amount {
                        asset: native,
                        atoms: amount,
                    },
                },
            ],
            evidence: json!({"fixture":"verified finalized account close"}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}

fn resource() -> ResourceId {
    ResourceId(RESOURCE.into())
}

fn asset() -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "native".into(),
        decimals: 9,
    }
}

fn anchor(principal: &str) -> PositionAnchor {
    PositionAnchor {
        resource: resource(),
        position: POSITION.into(),
        position_kind: PositionKind::RefundableAccountFunding,
        principal: Amount {
            asset: asset(),
            atoms: Quantity::new(principal).unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"source":"synthetic finalized account data"}),
        observed_at: now(),
    }
}

fn position(anchor: PositionAnchor) -> PositionState {
    PositionState {
        resource: anchor.resource.clone(),
        position: anchor.position.clone(),
        kind: anchor.position_kind.clone(),
        amount: anchor.principal.clone(),
        scaled_receipt: anchor.scaled_receipt.clone(),
        anchor: Some(anchor),
    }
}

fn prepared() -> PreparedOperation {
    let position = POSITION.to_owned();
    let at = chrono::Utc::now();
    PreparedOperation {
        operation: "synthetic.close_account".into(),
        input: json!({"account":"SYNTHETIC-ATA"}),
        binding: json!({"account":"SYNTHETIC-ATA","amount":"2039280"}),
        summary: "Close the synthetic account".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: at.to_rfc3339(),
        expires_at: (at + chrono::Duration::minutes(2)).to_rfc3339(),
        valuation: None,
        action_deadline: None,
        intent: FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![
                EffectConstraint::PositionOfKind {
                    resource: resource(),
                    position: position.clone(),
                    asset: asset(),
                    position_kind: PositionKind::RefundableAccountFunding,
                    direction: Direction::Decrease,
                    maximum_change: Quantity::new("2039280").unwrap(),
                },
                EffectConstraint::OriginMovement {
                    from: AccountingLocation::Position {
                        resource: resource(),
                        position,
                        position_kind: PositionKind::RefundableAccountFunding,
                    },
                    to: AccountingLocation::Cash {
                        resource: resource(),
                    },
                    debit_asset: asset(),
                    credit_asset: asset(),
                    maximum_debit: Quantity::new("2039280").unwrap(),
                    minimum_credit: Quantity::new("2039280").unwrap(),
                },
                EffectConstraint::Credit {
                    to: resource(),
                    asset: asset(),
                    minimum_credit: Quantity::new("2039280").unwrap(),
                },
            ],
            locks: vec![resource()],
        },
        operation_version: 1,
    }
}

fn observer(
    definition_name: &str,
    positions: Vec<PositionState>,
    coverage: Vec<String>,
) -> Arc<PositionObserver> {
    Arc::new(PositionObserver {
        resource: resource(),
        definition_name: definition_name.into(),
        positions,
        coverage,
        calls: AtomicUsize::new(0),
        fail_snapshot: false,
    })
}

#[test]
fn legacy_position_and_debt_requirements_bind_supported_anchor_kinds() {
    let legacy = position_anchor_requirements(&FinancialIntent {
        expected: vec![EffectConstraint::Position {
            resource: resource(),
            position: POSITION.into(),
            asset: asset(),
            direction: Direction::Decrease,
            maximum_change: Quantity::new("1").unwrap(),
        }],
        ..Default::default()
    });
    assert_eq!(legacy.len(), 1);
    let mut supply = anchor("1");
    supply.position_kind = PositionKind::Supply;
    assert!(legacy[0].matches(&supply));
    let mut collateral = anchor("1");
    collateral.position_kind = PositionKind::LockedCollateral;
    assert!(!legacy[0].matches(&collateral));

    let debt = position_anchor_requirements(&FinancialIntent {
        expected: vec![EffectConstraint::Debt {
            resource: resource(),
            debt: POSITION.into(),
            asset: asset(),
            direction: Direction::Decrease,
            maximum_change: Quantity::new("1").unwrap(),
        }],
        ..Default::default()
    });
    assert_eq!(debt.len(), 1);
    let mut variable_debt = anchor("1");
    variable_debt.position_kind = PositionKind::VariableDebt;
    assert!(debt[0].matches(&variable_debt));
    assert!(!debt[0].matches(&collateral));
}

#[test]
fn negative_derivative_delta_can_open_a_new_short_without_a_prior_anchor() {
    let requirements = position_anchor_requirements(&FinancialIntent {
        expected: vec![EffectConstraint::DerivativePosition {
            resource: resource(),
            position: POSITION.into(),
            asset: asset(),
            position_kind: PositionKind::DerivativeShort,
            minimum_delta: SignedQuantity::new("-5").unwrap(),
            maximum_delta: SignedQuantity::new("-1").unwrap(),
        }],
        ..Default::default()
    });

    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].kind, Some(PositionKind::DerivativeShort));
    assert!(!requirements[0].requires_prior);
}

fn build_engine(observers: Vec<Arc<PositionObserver>>) -> Engine {
    Engine::new(
        Store::in_memory().unwrap(),
        observers
            .into_iter()
            .map(|observer| observer as Arc<dyn Adapter>)
            .collect(),
        Arc::new(NoPositionApproval),
        EngineOptions::default(),
    )
    .unwrap()
}

struct NoPositionApproval;
#[async_trait]
impl gofin_domain::Authorizer for NoPositionApproval {
    async fn authorize(
        &self,
        _: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        Err(GoFinError::validation(
            "No approval is used by these tests.",
        ))
    }
}

struct PositionApproval;
#[async_trait]
impl gofin_domain::Authorizer for PositionApproval {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        Ok(gofin_domain::ApprovalAudit {
            mechanism: "synthetic-position-test".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct CountingPositionApproval(AtomicUsize);

#[async_trait]
impl gofin_domain::Authorizer for CountingPositionApproval {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(gofin_domain::ApprovalAudit {
            mechanism: "synthetic-position-test".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn record(
    prepared: PreparedOperation,
    baseline: Option<PositionAnchorBaseline>,
) -> OperationRecord {
    OperationRecord {
        id: "synthetic-operation".into(),
        request_id: "synthetic-request".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: prepared.input.clone(),
        status: Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(Submission {
            kind: "synthetic".into(),
            reference: "SYNTHETIC-SIGNATURE".into(),
            data: json!({}),
        }),
        dispatch_evidence: None,
        position_baseline: baseline,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

#[tokio::test]
async fn observer_failure_keeps_safe_cause_without_private_transport_details() {
    let source = Arc::new(PositionObserver {
        resource: resource(),
        definition_name: "synthetic.close_account".into(),
        positions: vec![],
        coverage: vec![],
        calls: AtomicUsize::new(0),
        fail_snapshot: true,
    });
    let engine = build_engine(vec![source.clone()]);
    let error = engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared(),
            &PreparationContext::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "POSITION_BASELINE_OBSERVER_FAILED");
    let details = error.details.as_deref().unwrap();
    assert_eq!(details["observer_error_code"], "OBSERVER_READ_TIMEOUT");
    assert_eq!(details["observer_error_stage"], "snapshot");
    assert_eq!(details["observer_rpc_method"], "getMultipleAccounts");
    assert_eq!(details["observer_rpc_attempt"], 2);
    assert_eq!(details["observer_rpc_elapsed_ms"], 731);
    assert_eq!(details["observer_rpc_stage"], "http_status");
    assert_eq!(details["observer_rpc_http_status"], 429);
    assert_eq!(details["observer_rpc_code"], -32005);
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains("SYNTHETIC_PRIVATE_OBSERVER")
    );
    let rendered = serde_json::to_string(&error).unwrap();
    assert!(!rendered.contains("user:secret"));
    assert!(!rendered.contains("SYNTHETIC_PRIVATE_ADDRESS"));
    assert!(!rendered.contains("provider.invalid"));
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn observer_rpc_diagnostic_method_requires_known_solana_method() {
    let private_ascii_method = gofin_domain::GoFinError::new(
        "OBSERVER_READ_TIMEOUT",
        "snapshot",
        "private transport failure",
        "retry",
    )
    .with_details(json!({ "method": "SyntheticPrivateWalletAddress123" }));
    assert!(safe_position_observer_rpc_diagnostics(&private_ascii_method).is_none());

    let known_method = gofin_domain::GoFinError::new(
        "OBSERVER_READ_TIMEOUT",
        "snapshot",
        "transport failure",
        "retry",
    )
    .with_details(json!({ "method": "getMultipleAccounts" }));
    assert_eq!(
        safe_position_observer_rpc_diagnostics(&known_method).unwrap()["observer_rpc_method"],
        "getMultipleAccounts"
    );
}

#[tokio::test]
async fn validated_candidate_baseline_skips_unavailable_shared_observer() {
    let source = Arc::new(PositionObserver {
        resource: resource(),
        definition_name: "synthetic.close_account".into(),
        positions: vec![],
        coverage: vec![],
        calls: AtomicUsize::new(0),
        fail_snapshot: true,
    });
    let engine = build_engine(vec![source.clone()]);
    let mut prepared = prepared();
    let mut native = anchor("2039280");
    native.observed_at = prepared.observed_at.clone();
    prepared.binding[gofin_domain::BASELINE_ANCHORS_KEY] = json!([native.clone()]);

    let baseline = engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared,
            &PreparationContext::default(),
        )
        .await
        .unwrap();
    assert_eq!(baseline.unwrap().anchors, vec![native.clone()]);
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);

    // A malformed or stale native anchor fails before the observer is
    // consulted; the optimization does not turn invalid evidence into authority.
    let mut stale = prepared.clone();
    let mut stale_anchor = native;
    stale_anchor.observed_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    stale.binding[gofin_domain::BASELINE_ANCHORS_KEY] = json!([stale_anchor]);
    assert_eq!(
        engine
            .capture_position_baseline(
                "synthetic-operation",
                &stale,
                &PreparationContext::default(),
            )
            .await
            .unwrap_err()
            .code,
        "INVALID_POSITION_BASELINE"
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn external_account_close_baseline_is_bound_persisted_and_recovered_after_restart() {
    let observed = anchor("2039280");
    let source = observer(
        "synthetic.close_account",
        vec![position(observed.clone())],
        vec![RESOURCE.into()],
    );
    let engine = build_engine(vec![source.clone()]);
    let prepared = prepared();
    let baseline = engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared,
            &PreparationContext::default(),
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(baseline.version, 1);
    assert_eq!(baseline.operation_id, "synthetic-operation");
    assert_eq!(
        baseline.prepared_binding_hash,
        binding_hash(&prepared.binding).unwrap()
    );
    assert_eq!(baseline.scope, vec![resource()]);
    assert_eq!(baseline.coverage, vec![RESOURCE]);
    assert_eq!(baseline.anchors, vec![observed]);

    // The operation record is the durable recovery envelope. A reconstructed
    // engine consumes the saved baseline and does not ask the observer again.
    let persisted = to_value(&record(prepared.clone(), Some(baseline.clone()))).unwrap();
    let recovered: OperationRecord = from_value(persisted).unwrap();
    let restarted = build_engine(vec![source.clone()]);
    let context = restarted.record_preparation_context(&recovered).unwrap();
    assert_eq!(context.positions, baseline.anchors);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);

    let mut tampered = recovered;
    tampered.preview.as_mut().unwrap().binding["amount"] = json!("2039281");
    assert_eq!(
        restarted
            .record_preparation_context(&tampered)
            .unwrap_err()
            .code,
        "POSITION_BASELINE_INVALID"
    );
}

#[tokio::test]
async fn full_engine_pending_restart_settles_against_original_external_account_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("position-baseline.sqlite3");
    let adapter = Arc::new(PositionLifecycleAdapter {
        observer: PositionObserver {
            resource: resource(),
            definition_name: "synthetic.close_account".into(),
            positions: vec![position(anchor("2039280"))],
            coverage: vec![RESOURCE.into()],
            calls: AtomicUsize::new(0),
            fail_snapshot: false,
        },
        finalized: std::sync::atomic::AtomicBool::new(false),
        submissions: AtomicUsize::new(0),
        clock: None,
        snapshot_advance: chrono::Duration::zero(),
    });
    let request = OperationRequest {
        request_id: "synthetic-position-close".into(),
        operation: "synthetic.close_account".into(),
        input: json!({"account":"SYNTHETIC-ATA"}),
        run_id: None,
    };
    let first = Engine::new(
        Store::open(&db_path).unwrap(),
        vec![adapter.clone()],
        Arc::new(PositionApproval),
        EngineOptions::default(),
    )
    .unwrap();
    let pending = first.execute(request.clone()).await.unwrap();
    assert_eq!(pending.status, Status::Pending);
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    let saved_baseline = pending.position_baseline.clone().unwrap();
    assert_eq!(
        saved_baseline.anchors[0].principal.atoms.to_string(),
        "2039280"
    );
    first.recover_interrupted().unwrap();
    drop(first);

    adapter.finalized.store(true, Ordering::SeqCst);
    let restarted = Engine::new(
        Store::open(&db_path).unwrap(),
        vec![adapter.clone()],
        Arc::new(PositionApproval),
        EngineOptions::default(),
    )
    .unwrap();
    let completed = restarted.execute(request).await.unwrap();
    assert_eq!(completed.id, pending.id);
    assert_eq!(
        completed.status,
        Status::Completed,
        "error: {:?}",
        completed.error
    );
    assert_eq!(completed.position_baseline, Some(saved_baseline));
    assert_eq!(completed.submission, pending.submission);
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.observer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn candidate_expiry_during_position_baseline_prevents_authorization_and_submission() {
    let clock = Arc::new(PositionTestClock(Mutex::new(
        chrono::Utc::now() + chrono::Duration::seconds(10),
    )));
    let adapter = Arc::new(PositionLifecycleAdapter {
        observer: PositionObserver {
            resource: resource(),
            definition_name: "synthetic.close_account".into(),
            positions: vec![position(anchor("2039280"))],
            coverage: vec![RESOURCE.into()],
            calls: AtomicUsize::new(0),
            fail_snapshot: false,
        },
        finalized: std::sync::atomic::AtomicBool::new(false),
        submissions: AtomicUsize::new(0),
        clock: Some(clock.clone()),
        snapshot_advance: chrono::Duration::seconds(6),
    });
    let authorizer = Arc::new(CountingPositionApproval(AtomicUsize::new(0)));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let record = engine
        .execute(OperationRequest {
            request_id: "synthetic-position-baseline-expiry".into(),
            operation: "synthetic.close_account".into(),
            input: json!({"account":"SYNTHETIC-ATA"}),
            run_id: None,
        })
        .await
        .unwrap();

    assert_eq!(record.status, Status::Expired);
    assert_eq!(record.error.as_ref().unwrap().code, "PREVIEW_EXPIRED");
    assert!(record.position_baseline.is_some());
    assert!(record.submission.is_none());
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 0);
    let saved = engine.operations().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, record.id);
    assert_eq!(saved[0].status, Status::Expired);
}

#[tokio::test]
async fn missing_legacy_baseline_is_not_refreshed_after_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("legacy-position-operation.sqlite3");
    let adapter = Arc::new(PositionLifecycleAdapter {
        observer: PositionObserver {
            resource: resource(),
            definition_name: "synthetic.close_account".into(),
            positions: vec![position(anchor("2039280"))],
            coverage: vec![RESOURCE.into()],
            calls: AtomicUsize::new(0),
            fail_snapshot: false,
        },
        finalized: std::sync::atomic::AtomicBool::new(false),
        submissions: AtomicUsize::new(0),
        clock: None,
        snapshot_advance: chrono::Duration::zero(),
    });
    let request = OperationRequest {
        request_id: "synthetic-legacy-position-close".into(),
        operation: "synthetic.close_account".into(),
        input: json!({"account":"SYNTHETIC-ATA"}),
        run_id: None,
    };
    let first = Engine::new(
        Store::open(&db_path).unwrap(),
        vec![adapter.clone()],
        Arc::new(PositionApproval),
        EngineOptions::default(),
    )
    .unwrap();
    let pending = first.execute(request.clone()).await.unwrap();
    assert_eq!(pending.status, Status::Pending);
    assert!(pending.position_baseline.is_some());

    // Model a legacy persisted record from before this additive field existed.
    // Recovery must not take a new snapshot after dispatch and retrofit it.
    let mut legacy = pending.clone();
    legacy.position_baseline = None;
    first.update_record(&legacy).unwrap();
    first.recover_interrupted().unwrap();
    drop(first);

    adapter.finalized.store(true, Ordering::SeqCst);
    let restarted = Engine::new(
        Store::open(&db_path).unwrap(),
        vec![adapter.clone()],
        Arc::new(PositionApproval),
        EngineOptions::default(),
    )
    .unwrap();
    let unresolved = restarted.execute(request).await.unwrap();
    assert_eq!(unresolved.status, Status::Unknown);
    assert_eq!(
        unresolved.error.as_ref().unwrap().code,
        "PRIOR_POSITION_ANCHOR_REQUIRED"
    );
    assert!(unresolved.position_baseline.is_none());
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.observer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn historical_anchor_is_persisted_for_reconciliation_without_refreshing_observer_state() {
    let source = observer(
        "synthetic.historical",
        vec![position(anchor("9999999"))],
        vec![RESOURCE.into()],
    );
    let engine = build_engine(vec![source.clone()]);
    let historical = anchor("2039280");
    let captured = engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared(),
            &PreparationContext {
                logical_operation_id: None,
                positions: vec![historical.clone()],
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(captured.anchors, vec![historical]);
    assert_eq!(captured.scope, vec![resource()]);
    assert_eq!(captured.coverage, vec![RESOURCE]);
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn virtual_liquidity_baseline_rejects_later_same_unit_anchor() {
    let resource = resource();
    let position_id = "meteora:liquidity:dlmm:SYNTHETIC-POSITION:liquidity_units";
    let liquidity_asset = gofin_domain::AssetId {
        network: "solana:mainnet-beta".into(),
        reference: position_id.into(),
        decimals: 0,
    };
    let binding = gofin_domain::LiquidityTransitionBinding {
        version: 1,
        resource: resource.clone(),
        position: position_id.into(),
        asset: liquidity_asset.clone(),
        pre_transaction_units_atoms: Quantity::new("7").unwrap(),
    };
    let mut prepared = prepared();
    prepared.binding = json!({
        gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY:
            serde_json::to_value(&binding).unwrap(),
    });
    let mut record = record(prepared.clone(), None);
    record.created_at = "2026-10-14T10:00:00+00:00".into();

    let make_anchor = |observed_at: &str| PositionAnchor {
        resource: resource.clone(),
        position: position_id.into(),
        position_kind: PositionKind::Liquidity,
        principal: Amount {
            asset: liquidity_asset.clone(),
            atoms: Quantity::new("7").unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({
            "liquidity_position": {
                "version": 1,
                "family": "dlmm",
                "pool": "SYNTHETIC-POOL",
                "position": "SYNTHETIC-POSITION",
                "anchor_position_id": position_id,
                "observed_slot": 10,
                "pre_transaction_units_atoms": null,
                "liquidity_units_atoms": "7",
                "liquidity_units_known": true,
                "historical_principal_known": true,
                "token_a": {
                    "asset": {"network":"solana:mainnet-beta","reference":"SYNTHETIC-A","decimals":6},
                    "token_program":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "atoms":"10"
                },
                "token_b": {
                    "asset": {"network":"solana:mainnet-beta","reference":"SYNTHETIC-B","decimals":6},
                    "token_program":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "atoms":"20"
                },
                "backing_valuation": null,
                "history": []
            }
        }),
        observed_at: observed_at.into(),
    };
    let prior_context = PreparationContext {
        logical_operation_id: None,
        positions: vec![make_anchor("2026-10-14T09:59:00+00:00")],
    };
    let baseline = Engine::virtual_liquidity_baseline_from_context(
        &record,
        &prepared,
        &binding,
        &prior_context,
    )
    .unwrap()
    .unwrap();
    assert_eq!(baseline.anchors.len(), 1);
    assert_eq!(
        baseline.anchors[0].principal.atoms,
        Quantity::new("7").unwrap()
    );

    let later_context = PreparationContext {
        logical_operation_id: None,
        positions: vec![make_anchor("2026-10-14T10:01:00+00:00")],
    };
    assert!(
        Engine::virtual_liquidity_baseline_from_context(
            &record,
            &prepared,
            &binding,
            &later_context,
        )
        .is_err()
    );
}

#[tokio::test]
async fn wrong_position_kind_history_does_not_satisfy_a_typed_requirement() {
    let source = observer(
        "synthetic.wrong_kind",
        vec![position(anchor("2039280"))],
        vec![RESOURCE.into()],
    );
    let engine = build_engine(vec![source.clone()]);
    let mut wrong_kind = anchor("2039280");
    wrong_kind.position_kind = PositionKind::LockedCollateral;
    let captured = engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared(),
            &PreparationContext {
                logical_operation_id: None,
                positions: vec![wrong_kind],
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(captured.anchors.len(), 1);
    assert_eq!(
        captured.anchors[0].position_kind,
        PositionKind::RefundableAccountFunding
    );
    assert_eq!(captured.anchors[0].principal.atoms.to_string(), "2039280");
}

#[tokio::test]
async fn conflicting_observers_and_fuzzy_scope_are_rejected_before_dispatch() {
    let first = observer(
        "synthetic.conflict_a",
        vec![position(anchor("2039280"))],
        vec![RESOURCE.into()],
    );
    let second = observer(
        "synthetic.conflict_b",
        vec![position(anchor("2039281"))],
        vec![RESOURCE.into()],
    );
    let conflict_engine = build_engine(vec![first, second]);
    assert_eq!(
        conflict_engine
            .capture_position_baseline(
                "synthetic-operation",
                &prepared(),
                &PreparationContext::default(),
            )
            .await
            .unwrap_err()
            .code,
        "POSITION_BASELINE_CONFLICT"
    );

    let fuzzy = observer(
        "synthetic.fuzzy",
        vec![position(anchor("2039280"))],
        vec![format!("{RESOURCE}:extra")],
    );
    let fuzzy_engine = build_engine(vec![fuzzy]);
    let error = fuzzy_engine
        .capture_position_baseline(
            "synthetic-operation",
            &prepared(),
            &PreparationContext::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "POSITION_BASELINE_SCOPE_MISSING");
    assert_eq!(
        error.details.as_deref(),
        Some(&json!({
            "requested_resources": [RESOURCE],
            "observer_operations": ["synthetic.fuzzy"],
            "exact_resource_attested": false,
        }))
    );
}
