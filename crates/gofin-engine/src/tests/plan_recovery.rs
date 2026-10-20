//! Small recovery contract fixtures; no network or native approval.
use super::*;
use gofin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanFundingIdentity,
    FinancialPlanSpec, FinancialPlanStepSpec, FinancialStepCompletion,
};

struct RecoveryClock(Mutex<DateTime<Utc>>);
impl gofin_domain::clock::Clock for RecoveryClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

struct CandidateFailure {
    inner: SyntheticAdapter,
    stage: &'static str,
    failure_code: &'static str,
    failure_details: Value,
    failures_remaining: AtomicUsize,
    expire_on_failure: bool,
    clock: Arc<RecoveryClock>,
    block_preparation: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    reuse_window: Option<std::time::Duration>,
    candidate_lifetime: Option<chrono::Duration>,
    expire_candidate_once: AtomicBool,
    logical_operation_ids: Option<Arc<Mutex<Vec<Option<String>>>>>,
}

impl CandidateFailure {
    fn fail(&self, stage: &str) -> Result<()> {
        let failed = self.stage == stage
            && self
                .failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    if remaining > 0 {
                        Some(remaining - 1)
                    } else {
                        None
                    }
                })
                .is_ok();
        if failed {
            if self.expire_on_failure {
                *self.clock.0.lock().unwrap() += chrono::Duration::hours(1);
            }
            let mut details = self.failure_details.clone();
            details["synthetic_diagnostic"] = json!(true);
            return Err(GoFinError::new(
                self.failure_code,
                if self.failure_code == "PRICE_STALE" {
                    "valuation"
                } else {
                    stage
                },
                "Synthetic candidate failure.",
                "Obtain fresh approval.",
            )
            .with_details(details));
        }
        Ok(())
    }
}

#[async_trait]
impl Adapter for CandidateFailure {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.inner.read(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.fail("preflight")?;
        if let Some((entered, release)) = &self.block_preparation {
            entered.notify_one();
            release.notified().await;
        }
        let mut prepared = self.inner.prepare(operation, input).await?;
        let now = *self.clock.0.lock().unwrap();
        prepared.observed_at = now.to_rfc3339();
        prepared.expires_at = (now
            + self
                .candidate_lifetime
                .unwrap_or_else(|| chrono::Duration::minutes(2)))
        .to_rfc3339();
        if let Some(valuation) = &mut prepared.valuation {
            valuation.observed_at = now.to_rfc3339();
        }
        Ok(prepared)
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &gofin_domain::PreparationContext,
    ) -> Result<PreparedOperation> {
        if let Some(ids) = &self.logical_operation_ids {
            ids.lock()
                .unwrap()
                .push(context.logical_operation_id.clone());
        }
        self.prepare(operation, input).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let now = *self.clock.0.lock().unwrap();
        let expired = DateTime::parse_from_rfc3339(&prepared.expires_at)
            .is_ok_and(|expires_at| expires_at <= now);
        if expired {
            return Err(GoFinError::new(
                "PREVIEW_EXPIRED",
                "revalidation",
                "Synthetic prepared candidate expired before dispatch.",
                "Prepare a fresh candidate under the active plan.",
            ));
        }
        self.fail("revalidation")?;
        self.inner.revalidate(prepared).await
    }
    fn preparation_reuse_window(
        &self,
        _prepared: &PreparedOperation,
    ) -> Option<std::time::Duration> {
        if self.expire_candidate_once.swap(false, Ordering::SeqCst) {
            *self.clock.0.lock().unwrap() += chrono::Duration::seconds(121);
        }
        self.reuse_window
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let submission = self.inner.submit(prepared, journal).await?;
        self.fail("submission")?;
        Ok(submission)
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.inner.reconcile(record).await
    }
}

struct JournalExpiryAdapter {
    inner: SyntheticAdapter,
    clock: Arc<RecoveryClock>,
    no_send_attempts: usize,
    submissions: AtomicUsize,
    prepares: AtomicUsize,
    network_sends: AtomicUsize,
    expire_authority_after_proof: AtomicBool,
    fail_verification: AtomicBool,
}

impl JournalExpiryAdapter {
    fn new(clock: Arc<RecoveryClock>, no_send_attempts: usize) -> Self {
        Self {
            inner: SyntheticAdapter::new(),
            clock,
            no_send_attempts,
            submissions: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            network_sends: AtomicUsize::new(0),
            expire_authority_after_proof: AtomicBool::new(false),
            fail_verification: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Adapter for JournalExpiryAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.inner.read(operation, input).await
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        let mut prepared = self.inner.prepare(operation, input).await?;
        let at = gofin_domain::clock::Clock::now(self.clock.as_ref());
        prepared.observed_at = at.to_rfc3339();
        if let Some(valuation) = &mut prepared.valuation {
            valuation.observed_at = at.to_rfc3339();
        }
        Ok(prepared)
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.inner.revalidate(prepared).await
    }

    async fn submit(
        &self,
        _prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let attempt = self.submissions.fetch_add(1, Ordering::SeqCst);
        let submission = Submission {
            kind: "synthetic_tx".into(),
            reference: format!("synthetic-expiry-{attempt}"),
            data: json!({"message":"bound"}),
        };
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        if attempt < self.no_send_attempts {
            journal.persist_dispatch_evidence(
                &submission,
                &json!({"no_network_send":true,"reason_code":"SOLANA_BLOCKHASH_EXPIRED"}),
            )?;
        } else {
            self.network_sends.fetch_add(1, Ordering::SeqCst);
            journal.persist_dispatch_evidence(
                &submission,
                &json!({"chain_signature":"synthetic-signature"}),
            )?;
        }
        Ok(submission)
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let submission = record
            .submission
            .as_ref()
            .ok_or_else(|| internal("synthetic submission missing"))?
            .clone();
        let attempt = submission
            .reference
            .strip_prefix("synthetic-expiry-")
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| internal("synthetic expiry attempt id invalid"))?;
        let not_dispatched = attempt < self.no_send_attempts;
        let status = if not_dispatched {
            Status::Failed
        } else {
            Status::Completed
        };
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission,
            effects: if not_dispatched {
                vec![]
            } else {
                vec![transfer_effect()]
            },
            evidence: if not_dispatched {
                json!({"state":"not_dispatched","reason_code":"SOLANA_BLOCKHASH_EXPIRED"})
            } else {
                json!({"state":"finalized"})
            },
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }

    async fn verify_non_dispatch_expiry(
        &self,
        record: &OperationRecord,
    ) -> Result<Option<gofin_domain::VerifiedNonDispatchExpiry>> {
        if self.fail_verification.load(Ordering::SeqCst) {
            return Err(GoFinError::new(
                "SYNTHETIC_PROOF_UNAVAILABLE",
                "reconciliation",
                "Synthetic proof verification failed.",
                "Inspect the original attempt.",
            ));
        }
        if record.status != Status::Failed
            || record
                .dispatch_evidence
                .as_ref()
                .and_then(|value| value.get("no_network_send"))
                .and_then(Value::as_bool)
                != Some(true)
        {
            return Ok(None);
        }
        let (Some(submission), Some(effects)) = (&record.submission, &record.effects) else {
            return Ok(None);
        };
        if self
            .expire_authority_after_proof
            .swap(false, Ordering::SeqCst)
        {
            *self.clock.0.lock().unwrap() += chrono::Duration::hours(1);
        }
        Ok(Some(
            gofin_domain::VerifiedNonDispatchExpiry::from_verified_effects(
                &record.id,
                submission,
                effects,
                gofin_domain::CandidateExpiryKind::TransactionLifetime,
            )?,
        ))
    }
}

async fn recovery_plan_engine(
    path: &std::path::Path,
    adapter: Arc<dyn Adapter>,
    authorizer: Arc<FailSecondAuthorizer>,
    clock: Arc<RecoveryClock>,
) -> (Engine, String) {
    let engine = Engine::new(
        Store::open(path).unwrap(),
        vec![adapter.clone()],
        authorizer,
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    // Build consent from the baseline fixture before enabling execution faults.
    let prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    let credits = vec![FinancialPlanFundingIdentity {
        resource: resource("ethereum-wallet"),
        asset: asset("ethereum:1", "native", 18),
    }];
    let plan = engine
        .authorize_financial_plan(FinancialPlanAuthorizationRequest {
            request_id: "synthetic-consent".into(),
            plan: FinancialPlanSpec {
                version: 1,
                name: "Synthetic transfer recovery".into(),
                summary: None,
                steps: vec![FinancialPlanStepSpec {
                    step_id: "transfer".into(),
                    operation: "synthetic.transfer".into(),
                    input: json!({}),
                    permitted_variations: json!({
                        "mode":"exact",
                        "maximum_debits":prepared.intent.debits,
                        "maximum_costs":[],
                        "possible_credits":credits
                    }),
                    completion: FinancialStepCompletion {
                        principal_spend: None,
                        minimum_net_receipt: None,
                        operation_succeeded: true,
                    },
                    funding_dependencies: vec![],
                    action_deadline_seconds: None,
                }],
                aggregate_bounds: FinancialPlanBounds {
                    maximum_debits: prepared.intent.debits.clone(),
                    maximum_costs: vec![],
                },
            },
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        })
        .await
        .unwrap();
    (engine, plan.plan_id)
}

fn usd_ceiling_failure_details() -> Value {
    json!({
        "reason":"usd_ceiling_exceeded",
        "field":"maximum_fee_usd_micros",
        "observed_usd_micros":"10001",
        "maximum_usd_micros":"10000"
    })
}

#[tokio::test]
async fn persisted_dispatch_errors_reconcile_once_without_replaying_standalone_or_plan() {
    for named_plan in [false, true] {
        for outcome in ["completed", "failed", "unknown", "read_error"] {
            let adapter = Arc::new(PersistThenErrorAdapter::new());
            match outcome {
                "completed" | "failed" => {
                    adapter.inner.completed_response();
                    let mut proof = adapter.inner.response.lock().unwrap().clone();
                    if outcome == "failed" {
                        proof.status = Status::Failed;
                        let verified = proof.verified.as_mut().unwrap();
                        verified.effects.clear();
                        verified.evidence = json!({"fixture":"definitive no-mutation rejection"});
                        verified.proof_digest = verified_effect_digest(verified).unwrap();
                        proof.evidence = verified.evidence.clone();
                    }
                    adapter.push_reconciliation(Ok(proof));
                }
                "unknown" => adapter.push_reconciliation(Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"fixture":"no matching original submission evidence"}),
                    verified: None,
                })),
                "read_error" => adapter.push_reconciliation(Err(GoFinError::new(
                    "SYNTHETIC_READ_TIMEOUT",
                    "reconciliation",
                    "Original submission evidence is temporarily unavailable.",
                    "Reconcile the original identity.",
                ))),
                _ => unreachable!(),
            }
            let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
            let authorizer = Arc::new(FailSecondAuthorizer {
                calls: AtomicUsize::new(0),
            });
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("dispatch-recovery.sqlite");
            let (engine, plan_id) = if named_plan {
                let (engine, plan_id) =
                    recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone())
                        .await;
                (engine, Some(plan_id))
            } else {
                (
                    Engine::new(
                        Store::open(&path).unwrap(),
                        vec![adapter.clone()],
                        authorizer.clone(),
                        EngineOptions::default(),
                    )
                    .unwrap()
                    .with_clock(clock),
                    None,
                )
            };
            let engine = engine.with_clock(Arc::new(gofin_domain::clock::SystemClock));
            let request = OperationRequest {
                request_id: "synthetic-dispatch-recovery".into(),
                operation: "synthetic.transfer".into(),
                input: json!({}),
                run_id: None,
            };
            let recovered = if let Some(plan_id) = &plan_id {
                engine
                    .execute_financial_plan_step(plan_id, "transfer", request.clone())
                    .await
            } else {
                engine.execute(request.clone()).await
            }
            .unwrap();
            let expected = match outcome {
                "completed" => Status::Completed,
                "failed" => Status::Failed,
                _ => Status::Unknown,
            };
            assert_eq!(
                recovered.status, expected,
                "{named_plan} {outcome}: {:?}",
                recovered.error
            );
            assert_eq!(adapter.reconciliation_calls.load(Ordering::SeqCst), 1);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
            let reservations = engine
                .store
                .lock()
                .unwrap()
                .reservations(&recovered.id)
                .unwrap();
            assert_eq!(reservations.is_empty(), expected.terminal());
            if !expected.terminal() {
                assert_eq!(
                    recovered.error.as_ref().unwrap().code,
                    "VENUE_REQUEST_FAILED"
                );
                assert!(
                    engine
                        .allocation_used_usd_micros()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        > 0
                );
            }
            if outcome == "read_error" {
                assert_eq!(
                    recovered.last_reconciliation_evidence.as_ref().unwrap()["latestReconciliationError"]
                        ["code"],
                    "SYNTHETIC_READ_TIMEOUT"
                );
            }
            if let Some(plan_id) = &plan_id {
                let plan: FinancialPlanAuthorizationRecord = from_value(
                    engine
                        .store
                        .lock()
                        .unwrap()
                        .financial_plan(plan_id)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(plan.attempts.len(), 1);
            }
            let duplicate = if let Some(plan_id) = &plan_id {
                engine
                    .execute_financial_plan_step(plan_id, "transfer", request)
                    .await
            } else {
                engine.execute(request).await
            }
            .unwrap();
            assert_eq!(duplicate.id, recovered.id);
            assert_eq!(duplicate.submission, recovered.submission);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn candidate_failure_reuses_active_consent_but_uncertain_dispatch_only_reconciles() {
    for (stage, failure_code, failure_details, expires) in [
        ("revalidation", "VALUE_BOUND_EXCEEDED", json!({}), false),
        (
            "revalidation",
            "VALUE_BOUND_EXCEEDED",
            json!({"reason":"priced_asset_identity_changed"}),
            false,
        ),
        (
            "revalidation",
            "VALUE_BOUND_EXCEEDED",
            usd_ceiling_failure_details(),
            true,
        ),
        ("revalidation", "SLIPPAGE_LIMIT", json!({}), false),
        ("revalidation", "BINDING_CHANGED", json!({}), false),
        ("submission", "RPC_UNAVAILABLE", json!({}), false),
    ] {
        let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
        let adapter = Arc::new(CandidateFailure {
            inner: SyntheticAdapter::new(),
            stage,
            failure_code,
            failure_details,
            failures_remaining: AtomicUsize::new(1),
            expire_on_failure: expires,
            clock: clock.clone(),
            block_preparation: None,
            reuse_window: None,
            candidate_lifetime: None,
            expire_candidate_once: AtomicBool::new(false),
            logical_operation_ids: None,
        });
        let authorizer = Arc::new(FailSecondAuthorizer {
            calls: AtomicUsize::new(0),
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic-recovery.sqlite");
        let engine = Engine::new(
            Store::open(&path).unwrap(),
            vec![adapter.clone()],
            authorizer.clone(),
            EngineOptions::default(),
        )
        .unwrap()
        .with_clock(clock.clone());
        let prepared = adapter
            .inner
            .prepare("synthetic.transfer", &json!({}))
            .await
            .unwrap();
        let credits = vec![FinancialPlanFundingIdentity {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "native", 18),
        }];
        let plan = engine
            .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                request_id: "synthetic-consent".into(),
                plan: FinancialPlanSpec {
                    version: 1,
                    name: "Synthetic transfer recovery".into(),
                    summary: None,
                    steps: vec![FinancialPlanStepSpec {
                        step_id: "transfer".into(),
                        operation: "synthetic.transfer".into(),
                        input: json!({}),
                        permitted_variations: json!({"mode":"exact",
                        "maximum_debits":prepared.intent.debits,
                        "maximum_costs":[], "possible_credits":credits}),
                        completion: FinancialStepCompletion {
                            principal_spend: None,
                            minimum_net_receipt: None,
                            operation_succeeded: true,
                        },
                        funding_dependencies: vec![],
                        action_deadline_seconds: None,
                    }],
                    aggregate_bounds: FinancialPlanBounds {
                        maximum_debits: prepared.intent.debits.clone(),
                        maximum_costs: vec![],
                    },
                },
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            })
            .await
            .unwrap();
        let mut request = OperationRequest {
            request_id: "synthetic-attempt".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        };
        let result = engine
            .execute_financial_plan_step(&plan.plan_id, "transfer", request.clone())
            .await;
        let (error, operation) = if stage == "preflight" {
            (result.unwrap_err(), None)
        } else {
            let record = result.unwrap();
            (record.error.clone().unwrap(), Some(record))
        };
        assert_eq!(error.code, failure_code);
        let recorded_plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
            engine
                .store
                .lock()
                .unwrap()
                .financial_plan(&plan.plan_id)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(recorded_plan.attempts.len(), 1);
        let details = error.details.unwrap();
        assert_eq!(details["synthetic_diagnostic"], true);
        assert_eq!(details["reauthorization_required"], expires);
        assert_eq!(details["reconciliation_required"], stage == "submission");
        assert_eq!(details["new_attempt_required"], stage == "revalidation");
        assert_eq!(details["submission_recorded"], stage == "submission");
        if stage != "submission" {
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
            assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
        }
        if expires {
            let blocked = engine
                .execute_financial_plan_step(
                    &plan.plan_id,
                    "transfer",
                    OperationRequest {
                        request_id: "synthetic-after-expiry".into(),
                        ..request
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(blocked.code, "PLAN_AUTHORIZATION_INACTIVE");
        } else {
            if stage == "revalidation" {
                let duplicate = engine
                    .execute_financial_plan_step(&plan.plan_id, "transfer", request.clone())
                    .await
                    .unwrap();
                assert_eq!(duplicate.id, operation.as_ref().unwrap().id);
                assert!(duplicate.submission.is_none());
                request.request_id = "synthetic-refreshed-candidate".into();
            }
            if stage == "submission" {
                assert_eq!(operation.as_ref().unwrap().status, Status::Pending);
                assert!(
                    engine
                        .allocation_used_usd_micros()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        > 0
                );
                engine.revoke_financial_plan(&plan.plan_id).await.unwrap();
            }
            drop(engine);
            let reopened = Engine::new(
                Store::open(&path).unwrap(),
                vec![adapter.clone()],
                authorizer.clone(),
                EngineOptions::default(),
            )
            .unwrap()
            .with_clock(clock);
            adapter.inner.completed_response();
            let completed = reopened
                .execute_financial_plan_step(&plan.plan_id, "transfer", request)
                .await
                .unwrap();
            assert_eq!(
                completed.status,
                Status::Completed,
                "{stage}: {:?}",
                completed.error
            );
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            if stage == "submission" {
                assert_eq!(completed.id, operation.unwrap().id);
            }
        }
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn transient_plan_candidate_refresh_keeps_one_approval_and_one_dispatch() {
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch("EVIDENCE_TIMEOUT", json!({}))
        .await;
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch(
        "SOLANA_BLOCKHASH_EXPIRED",
        json!({}),
    )
    .await;
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch(
        "SOLANA_TX_EXPIRED",
        json!({}),
    )
    .await;
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch(
        "JUPITER_RFQ_EXPIRED",
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn usd_ceiling_transient_plan_candidate_refresh_keeps_one_approval_and_one_dispatch() {
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch(
        "VALUE_BOUND_EXCEEDED",
        usd_ceiling_failure_details(),
    )
    .await;
}

async fn check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch(
    failure_code: &'static str,
    failure_details: Value,
) {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "revalidation",
        failure_code,
        failure_details,
        failures_remaining: AtomicUsize::new(1),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: None,
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: None,
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-auto-refresh.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone()).await;
    let request = OperationRequest {
        request_id: "synthetic-logical-attempt".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    adapter.inner.completed_response();
    let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
    let completed = gofin_domain::execution_progress::observe_events(
        observer,
        engine.execute_financial_plan_step(&plan_id, "transfer", request.clone()),
    )
    .await
    .unwrap();
    assert_refresh_progress(
        &mut progress,
        failure_code,
        gofin_domain::execution_progress::RefreshPhase::Revalidation,
        1,
    );
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_ne!(completed.request_id, "synthetic-logical-attempt");
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    let plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
        engine
            .store
            .lock()
            .unwrap()
            .financial_plan(&plan_id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plan.attempts.len(), 2);
    assert!(plan.attempts.iter().all(|attempt| {
        attempt.logical_request_id.as_deref() == Some("synthetic-logical-attempt")
    }));
    assert_ne!(plan.attempts[0].attempt_id, plan.attempts[1].attempt_id);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .by_request("synthetic-logical-attempt")
            .unwrap()
            .is_none()
    );

    drop(engine);
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let duplicate = reopened
        .execute_financial_plan_step(&plan_id, "transfer", request)
        .await
        .unwrap();
    assert_eq!(duplicate.id, completed.id);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_plan_candidate_refresh_keeps_one_approval_and_one_dispatch() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let logical_operation_ids = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "never-fail",
        failure_code: "SYNTHETIC_UNUSED",
        failure_details: json!({}),
        failures_remaining: AtomicUsize::new(0),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: None,
        reuse_window: Some(std::time::Duration::from_secs(120)),
        candidate_lifetime: Some(chrono::Duration::seconds(120)),
        expire_candidate_once: AtomicBool::new(true),
        logical_operation_ids: Some(logical_operation_ids.clone()),
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-expired-auto-refresh.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone()).await;
    let request = OperationRequest {
        request_id: "synthetic-expired-logical-attempt".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    adapter.inner.completed_response();
    let completed = engine
        .execute_financial_plan_step(&plan_id, "transfer", request.clone())
        .await
        .unwrap();
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_ne!(completed.request_id, "synthetic-expired-logical-attempt");
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    let plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
        engine
            .store
            .lock()
            .unwrap()
            .financial_plan(&plan_id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plan.attempts.len(), 2);
    assert!(plan.attempts.iter().all(|attempt| {
        attempt.logical_request_id.as_deref() == Some("synthetic-expired-logical-attempt")
    }));
    assert_ne!(plan.attempts[0].attempt_id, plan.attempts[1].attempt_id);
    let captured_ids = logical_operation_ids.lock().unwrap().clone();
    assert_eq!(captured_ids.len(), 2);
    assert_eq!(captured_ids[0], captured_ids[1]);
    assert!(
        captured_ids[0]
            .as_deref()
            .is_some_and(|value| value.starts_with("plan-execution:"))
    );
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .by_request("synthetic-expired-logical-attempt")
            .unwrap()
            .is_none()
    );

    drop(engine);
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let duplicate = reopened
        .execute_financial_plan_step(&plan_id, "transfer", request)
        .await
        .unwrap();
    assert_eq!(duplicate.id, completed.id);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(logical_operation_ids.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn standalone_request_identity_is_recorded_once_and_duplicate_survives_store_restart() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let logical_operation_ids = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "never-fail",
        failure_code: "SYNTHETIC_UNUSED",
        failure_details: json!({}),
        failures_remaining: AtomicUsize::new(0),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: None,
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: Some(logical_operation_ids.clone()),
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-logical-request.sqlite");
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock.clone());
    adapter.inner.completed_response();
    let request = OperationRequest {
        request_id: "synthetic-logical-request".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    let completed = engine.execute(request.clone()).await.unwrap();
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_eq!(completed.request_id, request.request_id);
    assert_eq!(
        logical_operation_ids.lock().unwrap().as_slice(),
        &[Some("execution:synthetic-logical-request".into())]
    );

    drop(engine);
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let duplicate = reopened.execute(request).await.unwrap();
    assert_eq!(duplicate.id, completed.id);
    assert_eq!(logical_operation_ids.lock().unwrap().len(), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);

    // The optional context field was added after older records already
    // existed; deserialize those records as an empty logical identity.
    let legacy: gofin_domain::PreparationContext =
        serde_json::from_value(json!({"positions":[]})).unwrap();
    assert_eq!(legacy.logical_operation_id, None);
}

#[tokio::test]
async fn verified_journaled_no_dispatch_expiry_refreshes_once_and_duplicate_is_idempotent() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(JournalExpiryAdapter::new(clock.clone(), 1));
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-journal-expiry.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone()).await;
    let request = OperationRequest {
        request_id: "synthetic-journal-expiry-logical".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
    let completed = gofin_domain::execution_progress::observe_events(
        observer,
        engine.execute_financial_plan_step(&plan_id, "transfer", request.clone()),
    )
    .await
    .unwrap();
    let mut refreshes = Vec::new();
    while let Ok(event) = progress.try_recv() {
        if let Some(refresh) = event.candidate_refresh {
            refreshes.push(refresh);
        }
    }
    assert_eq!(
        refreshes,
        vec![gofin_domain::execution_progress::CandidateRefresh {
            reason_code: gofin_domain::execution_progress::RefreshReason::VerifiedNonDispatchExpiry,
            phase: gofin_domain::execution_progress::RefreshPhase::VerifiedNonDispatch,
            refresh_count: 1,
            refresh_limit: 2,
        }]
    );
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 2);
    assert_eq!(adapter.network_sends.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);

    let plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
        engine
            .store
            .lock()
            .unwrap()
            .financial_plan(&plan_id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plan.attempts.len(), 2);
    assert!(plan.attempts.iter().all(|attempt| {
        attempt.logical_request_id.as_deref() == Some("synthetic-journal-expiry-logical")
    }));
    let expired = engine.load_record(&plan.attempts[0].attempt_id).unwrap();
    assert_eq!(expired.status, Status::Failed);
    assert!(expired.submission.is_some());
    assert!(expired.effects.as_ref().unwrap().effects.is_empty());
    assert_eq!(
        expired.dispatch_evidence.as_ref().unwrap()["no_network_send"],
        true
    );
    assert_eq!(
        expired.effects.as_ref().unwrap().proof_digest,
        verified_effect_digest(expired.effects.as_ref().unwrap()).unwrap()
    );

    drop(engine);
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let duplicate = reopened
        .execute_financial_plan_step(&plan_id, "transfer", request)
        .await
        .unwrap();
    assert_eq!(duplicate.id, completed.id);
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 2);
    assert_eq!(adapter.network_sends.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn verified_journaled_expiry_stops_at_refresh_limit_with_actionable_details() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(JournalExpiryAdapter::new(clock.clone(), 3));
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-journal-expiry-limit.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock).await;
    let failed = engine
        .execute_financial_plan_step(
            &plan_id,
            "transfer",
            OperationRequest {
                request_id: "synthetic-journal-expiry-limit-logical".into(),
                operation: "synthetic.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 3);
    assert_eq!(adapter.network_sends.load(Ordering::SeqCst), 0);
    let error = failed.error.as_ref().unwrap();
    assert_eq!(error.code, "PLAN_CANDIDATE_REFRESH_EXHAUSTED");
    let details = error.details.as_deref().unwrap();
    assert_eq!(details["refresh_count"], 2);
    assert_eq!(details["refresh_limit"], 2);
    assert_eq!(details["refresh_exhausted"], true);
    assert_eq!(details["non_dispatch_proven"], true);
    assert_eq!(details["reauthorization_required"], false);
    assert!(error.next_action.contains("while conditions are unchanged"));
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn verified_journaled_expiry_does_not_refresh_after_plan_authority_expires() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(JournalExpiryAdapter::new(clock.clone(), 3));
    adapter
        .expire_authority_after_proof
        .store(true, Ordering::SeqCst);
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-expired-plan-after-proof.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock).await;
    let failed = engine
        .execute_financial_plan_step(
            &plan_id,
            "transfer",
            OperationRequest {
                request_id: "synthetic-expired-plan-after-proof".into(),
                operation: "synthetic.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.submission.is_some());
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.network_sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        failed.error.as_ref().unwrap().code,
        "PLAN_AUTHORIZATION_INACTIVE"
    );
    assert_eq!(
        failed.error.as_ref().unwrap().details.as_deref().unwrap()["reauthorization_required"],
        true
    );
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expiry_verifier_failure_preserves_terminal_record_without_retry() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(JournalExpiryAdapter::new(clock.clone(), 3));
    adapter.fail_verification.store(true, Ordering::SeqCst);
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-expiry-verifier-error.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock).await;
    let failed = engine
        .execute_financial_plan_step(
            &plan_id,
            "transfer",
            OperationRequest {
                request_id: "synthetic-expiry-verifier-error".into(),
                operation: "synthetic.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.submission.is_some());
    assert_eq!(failed.error, None);
    assert_eq!(adapter.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.network_sends.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_candidate_refresh_failure_stops_at_two_and_reports_count() {
    check_candidate_refresh_failure_stops_at_two_and_reports_count("NONCE_CHANGED", json!({}))
        .await;
}

#[tokio::test]
async fn usd_ceiling_persistent_candidate_refresh_failure_stops_at_two_and_reports_count() {
    check_candidate_refresh_failure_stops_at_two_and_reports_count(
        "VALUE_BOUND_EXCEEDED",
        usd_ceiling_failure_details(),
    )
    .await;
}

async fn check_candidate_refresh_failure_stops_at_two_and_reports_count(
    failure_code: &'static str,
    failure_details: Value,
) {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "revalidation",
        failure_code,
        failure_details,
        failures_remaining: AtomicUsize::new(10),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: None,
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: None,
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-auto-refresh-limit.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone()).await;
    let request = OperationRequest {
        request_id: "synthetic-persistent-logical-attempt".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
    let operation = gofin_domain::execution_progress::observe_events(
        observer,
        engine.execute_financial_plan_step(&plan_id, "transfer", request.clone()),
    )
    .await
    .unwrap();
    assert_refresh_progress(
        &mut progress,
        failure_code,
        gofin_domain::execution_progress::RefreshPhase::Revalidation,
        2,
    );
    assert_eq!(operation.status, Status::Failed);
    assert_eq!(operation.error.as_ref().unwrap().code, failure_code);
    assert_eq!(adapter.failures_remaining.load(Ordering::SeqCst), 7);
    assert!(
        operation
            .error
            .as_ref()
            .unwrap()
            .next_action
            .contains("do not repeat unchanged attempts with new IDs")
    );
    assert_eq!(
        operation
            .error
            .as_ref()
            .unwrap()
            .details
            .as_deref()
            .unwrap()["refresh_count"],
        2
    );
    assert_eq!(
        operation
            .error
            .as_ref()
            .unwrap()
            .details
            .as_deref()
            .unwrap()["refresh_exhausted"],
        true
    );
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    let plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
        engine
            .store
            .lock()
            .unwrap()
            .financial_plan(&plan_id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plan.attempts.len(), 3);

    drop(engine);
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let duplicate = reopened
        .execute_financial_plan_step(&plan_id, "transfer", request)
        .await
        .unwrap();
    assert_eq!(duplicate.id, operation.id);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uncertain_plan_child_is_reconciled_after_restart_without_replay() {
    check_plan_child_is_reconciled_after_restart_without_replay("RPC_UNAVAILABLE", json!({})).await;
}

#[tokio::test]
async fn usd_ceiling_uncertain_plan_child_is_reconciled_after_restart_without_replay() {
    check_plan_child_is_reconciled_after_restart_without_replay(
        "VALUE_BOUND_EXCEEDED",
        usd_ceiling_failure_details(),
    )
    .await;
}

async fn check_plan_child_is_reconciled_after_restart_without_replay(
    failure_code: &'static str,
    failure_details: Value,
) {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "submission",
        failure_code,
        failure_details,
        failures_remaining: AtomicUsize::new(1),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: None,
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: None,
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-uncertain-plan-child.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock.clone()).await;
    let request = OperationRequest {
        request_id: "synthetic-uncertain-logical-request".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    let uncertain = engine
        .execute_financial_plan_step(&plan_id, "transfer", request.clone())
        .await
        .unwrap();
    assert_eq!(uncertain.status, Status::Pending);
    assert_eq!(uncertain.error.as_ref().unwrap().code, failure_code);
    assert!(uncertain.submission.is_some());
    let recorded_plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
        engine
            .store
            .lock()
            .unwrap()
            .financial_plan(&plan_id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(recorded_plan.attempts.len(), 1);
    engine.revoke_financial_plan(&plan_id).await.unwrap();
    drop(engine);

    adapter.inner.completed_response();
    let reopened = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let completed = reopened
        .execute_financial_plan_step(&plan_id, "transfer", request)
        .await
        .unwrap();
    assert_eq!(completed.id, uncertain.id);
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn status_and_revoke_bypass_blocked_plan_preparation_and_revoke_prevents_dispatch() {
    let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let adapter = Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage: "never-fail",
        failure_code: "SYNTHETIC_UNUSED",
        failure_details: json!({}),
        failures_remaining: AtomicUsize::new(0),
        expire_on_failure: false,
        clock: clock.clone(),
        block_preparation: Some((entered.clone(), release.clone())),
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: None,
    });
    let authorizer = Arc::new(FailSecondAuthorizer {
        calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("synthetic-blocked-plan-preparation.sqlite");
    let (engine, plan_id) =
        recovery_plan_engine(&path, adapter.clone(), authorizer.clone(), clock).await;
    let engine = Arc::new(engine);
    let execution_engine = engine.clone();
    let execution_plan_id = plan_id.clone();
    let execution = tokio::spawn(async move {
        execution_engine
            .execute_financial_plan_step(
                &execution_plan_id,
                "transfer",
                OperationRequest {
                    request_id: "synthetic-blocked-logical-request".into(),
                    operation: "synthetic.transfer".into(),
                    input: json!({}),
                    run_id: None,
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("plan preparation should reach its blocking point");

    let observed = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        engine.financial_plan_status(&plan_id),
    )
    .await
    .expect("status should return while preparation is blocked")
    .unwrap();
    assert_eq!(
        observed.revision.status,
        FinancialPlanAuthorizationStatus::Active
    );
    let revoked = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        engine.revoke_financial_plan(&plan_id),
    )
    .await
    .expect("revocation should return while preparation is blocked")
    .unwrap();
    assert_eq!(
        revoked.revision.status,
        FinancialPlanAuthorizationStatus::Revoked
    );

    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
        .await
        .expect("execution should finish after preparation is released")
        .unwrap();
    assert!(result.is_err());
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn price_stale_revalidation_refreshes_once_and_preserves_duplicate_identity() {
    check_plan_candidate_refresh_keeps_one_approval_and_one_dispatch("PRICE_STALE", json!({}))
        .await;
}

#[tokio::test]
async fn price_stale_revalidation_stops_at_two_refreshes_without_dispatch() {
    check_candidate_refresh_failure_stops_at_two_and_reports_count("PRICE_STALE", json!({})).await;
}

#[tokio::test]
async fn price_stale_preparation_reuses_authority_and_exhausts_without_writes() {
    for failures in [1, 10] {
        let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
        let adapter = Arc::new(CandidateFailure {
            inner: SyntheticAdapter::new(),
            stage: "preflight",
            failure_code: "PRICE_STALE",
            failure_details: json!({}),
            failures_remaining: AtomicUsize::new(0),
            expire_on_failure: false,
            clock: clock.clone(),
            block_preparation: None,
            reuse_window: None,
            candidate_lifetime: None,
            expire_candidate_once: AtomicBool::new(false),
            logical_operation_ids: None,
        });
        let authorizer = Arc::new(FailSecondAuthorizer {
            calls: AtomicUsize::new(0),
        });
        let directory = tempfile::tempdir().unwrap();
        let (engine, plan_id) = recovery_plan_engine(
            &directory.path().join("price-stale.sqlite"),
            adapter.clone(),
            authorizer.clone(),
            clock,
        )
        .await;
        adapter.failures_remaining.store(failures, Ordering::SeqCst);
        adapter.inner.completed_response();
        let request = OperationRequest {
            request_id: "price-stale-logical".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        };
        let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
        let result = gofin_domain::execution_progress::observe_events(
            observer,
            engine.execute_financial_plan_step(&plan_id, "transfer", request.clone()),
        )
        .await;
        assert_refresh_progress(
            &mut progress,
            "PRICE_STALE",
            gofin_domain::execution_progress::RefreshPhase::Preparation,
            if failures == 1 { 1 } else { 2 },
        );
        if failures == 1 {
            let completed = result.unwrap();
            assert_eq!(completed.status, Status::Completed);
            let duplicate = engine
                .execute_financial_plan_step(&plan_id, "transfer", request)
                .await
                .unwrap();
            assert_eq!(duplicate.id, completed.id);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.code, "PRICE_STALE");
            assert_eq!(error.stage, "valuation");
            assert_eq!(error.details.as_deref().unwrap()["refresh_count"], 2);
            assert_eq!(error.details.as_deref().unwrap()["refresh_exhausted"], true);
            assert_eq!(adapter.failures_remaining.load(Ordering::SeqCst), 7);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
        }
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    }
}

fn assert_refresh_progress(
    receiver: &mut tokio::sync::mpsc::Receiver<gofin_domain::execution_progress::Event>,
    code: &str,
    phase: gofin_domain::execution_progress::RefreshPhase,
    expected: usize,
) {
    use gofin_domain::execution_progress::{CandidateRefresh, RefreshReason, Stage};
    let mut refreshes = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if event.stage == Stage::RefreshingCandidate {
            refreshes.push(
                event
                    .candidate_refresh
                    .expect("refresh includes bounded safe diagnosis"),
            );
        }
    }
    assert_eq!(refreshes.len(), expected);
    for (index, refresh) in refreshes.into_iter().enumerate() {
        assert_eq!(
            refresh,
            CandidateRefresh {
                reason_code: RefreshReason::from_code(code).unwrap(),
                phase,
                refresh_count: index + 1,
                refresh_limit: 2,
            }
        );
    }
}

mod rpc_recovery {
    use super::*;
    include!("plan_rpc_recovery.rs");
}

mod preparation_stack {
    use super::*;
    include!("plan_preparation_stack.rs");
}
