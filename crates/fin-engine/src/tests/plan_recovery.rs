//! Small recovery contract fixtures; no network or native approval.
use super::*;
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanFundingIdentity,
    FinancialPlanSpec, FinancialPlanStepSpec, FinancialStepCompletion,
};

struct RecoveryClock(Mutex<DateTime<Utc>>);
impl fin_domain::clock::Clock for RecoveryClock {
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
            return Err(FinError::new(
                self.failure_code,
                stage,
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
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let now = *self.clock.0.lock().unwrap();
        let expired = DateTime::parse_from_rfc3339(&prepared.expires_at)
            .is_ok_and(|expires_at| expires_at <= now);
        if expired {
            return Err(FinError::new(
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

async fn recovery_plan_engine(
    path: &std::path::Path,
    adapter: Arc<CandidateFailure>,
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
                assert_eq!(operation.as_ref().unwrap().status, Status::Unknown);
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
    let completed = engine
        .execute_financial_plan_step(&plan_id, "transfer", request.clone())
        .await
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
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
    assert_eq!(completed.status, Status::Completed);
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
    let operation = engine
        .execute_financial_plan_step(&plan_id, "transfer", request.clone())
        .await
        .unwrap();
    assert_eq!(operation.status, Status::Failed);
    assert_eq!(operation.error.as_ref().unwrap().code, failure_code);
    assert_eq!(adapter.failures_remaining.load(Ordering::SeqCst), 7);
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
    assert_eq!(uncertain.status, Status::Unknown);
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
