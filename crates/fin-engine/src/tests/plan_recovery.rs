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
    fail_once: AtomicBool,
    expire_on_failure: bool,
    clock: Arc<RecoveryClock>,
}

impl CandidateFailure {
    fn fail(&self, stage: &str) -> Result<()> {
        if self.stage == stage && self.fail_once.swap(false, Ordering::SeqCst) {
            if self.expire_on_failure {
                *self.clock.0.lock().unwrap() += chrono::Duration::hours(1);
            }
            return Err(FinError::new(
                self.failure_code,
                stage,
                "Synthetic transport timeout.",
                "Obtain fresh approval.",
            )
            .with_details(json!({"synthetic_diagnostic":true})));
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
        let mut prepared = self.inner.prepare(operation, input).await?;
        let now = *self.clock.0.lock().unwrap();
        prepared.observed_at = now.to_rfc3339();
        prepared.expires_at = (now + chrono::Duration::minutes(2)).to_rfc3339();
        if let Some(valuation) = &mut prepared.valuation {
            valuation.observed_at = now.to_rfc3339();
        }
        Ok(prepared)
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.fail("revalidation")?;
        self.inner.revalidate(prepared).await
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

#[tokio::test]
async fn candidate_failure_reuses_active_consent_but_uncertain_dispatch_only_reconciles() {
    for (stage, failure_code, expires) in [
        ("preflight", "EVIDENCE_TIMEOUT", false),
        ("revalidation", "EVIDENCE_TIMEOUT", false),
        ("revalidation", "PREVIEW_EXPIRED", true),
        ("revalidation", "STALE_VALUATION", false),
        ("revalidation", "VALUE_BOUND_EXCEEDED", false),
        ("revalidation", "SLIPPAGE_LIMIT", false),
        ("revalidation", "BINDING_CHANGED", false),
        ("revalidation", "NONCE_CHANGED", false),
        ("submission", "RPC_UNAVAILABLE", false),
    ] {
        let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
        let adapter = Arc::new(CandidateFailure {
            inner: SyntheticAdapter::new(),
            stage,
            failure_code,
            fail_once: AtomicBool::new(true),
            expire_on_failure: expires,
            clock: clock.clone(),
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
