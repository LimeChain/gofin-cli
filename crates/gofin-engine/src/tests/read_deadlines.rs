use super::*;
use tokio::sync::Notify;

/// A disposable adapter whose response is controlled by the test, not by a timer.
/// Dropping its read future must release all in-flight work.
struct ControlledReadAdapter {
    inner: SyntheticAdapter,
    rpc_reads: bool,
    started: Notify,
    release: Notify,
    reads: AtomicUsize,
    dropped: AtomicUsize,
    hold_reconciliation: AtomicBool,
    reconciliation_started: Notify,
    release_reconciliation: Notify,
    reconciliation_dropped: AtomicUsize,
}

struct ReadGuard<'a>(&'a AtomicUsize);

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl ControlledReadAdapter {
    fn new(rpc_reads: bool) -> Self {
        Self {
            inner: SyntheticAdapter::new(),
            rpc_reads,
            started: Notify::new(),
            release: Notify::new(),
            reads: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            hold_reconciliation: AtomicBool::new(false),
            reconciliation_started: Notify::new(),
            release_reconciliation: Notify::new(),
            reconciliation_dropped: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Adapter for ControlledReadAdapter {
    fn uses_rpc_reads(&self, _operation: &str, _input: &Value) -> bool {
        self.rpc_reads
    }

    fn definitions(&self) -> Vec<Definition> {
        let mut read = SyntheticAdapter::write_definition();
        read.name = "synthetic.balance".into();
        read.kind = OperationKind::Read;
        read.execution_actor = ExecutionActor::AgentReadable;
        vec![read, SyntheticAdapter::write_definition()]
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
        let _guard = ReadGuard(&self.dropped);
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(json!({"balance":"42"}))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.inner.prepare(operation, input).await
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.inner.revalidate(prepared).await
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.inner.submit(prepared, journal).await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        if self.hold_reconciliation.load(Ordering::SeqCst) {
            let _guard = ReadGuard(&self.reconciliation_dropped);
            self.reconciliation_started.notify_one();
            self.release_reconciliation.notified().await;
        }
        self.inner.reconcile(record).await
    }
}

struct CountingAuthorizer(AtomicUsize);

#[tokio::test(start_paused = true)]
async fn non_rpc_engine_read_retains_original_command_deadline() {
    let adapter = Arc::new(ControlledReadAdapter::new(false));
    let engine = engine(adapter.clone());
    let started = tokio::time::Instant::now();

    let error = engine
        .read("synthetic.balance", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(tokio::time::Instant::now() - started, READ_COMMAND_TIMEOUT);
    assert_eq!(error.code, "EVIDENCE_TIMEOUT");
    assert_eq!(error.stage, "read");
    assert_eq!(adapter.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn non_rpc_status_observation_retains_original_evidence_deadline() {
    let adapter = Arc::new(ControlledReadAdapter::new(false));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    let original = engine
        .execute(ordinary_request("non-rpc-status"))
        .await
        .unwrap();
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .reservations(&original.id)
        .unwrap();
    adapter.hold_reconciliation.store(true, Ordering::SeqCst);
    let started = tokio::time::Instant::now();

    let retained = engine.status(&original.id, true).await.unwrap();
    assert_eq!(
        tokio::time::Instant::now() - started,
        FINANCIAL_EVIDENCE_TIMEOUT
    );
    assert_eq!(retained.status, Status::Pending);
    assert_eq!(retained.submission, original.submission);
    assert_eq!(retained.approval, original.approval);
    assert_eq!(retained.error.as_ref().unwrap().code, "EVIDENCE_TIMEOUT");
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap(),
        reservations
    );
    assert_eq!(adapter.reconciliation_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rpc_scope_metadata_distinguishes_adapters_and_rejects_write_as_read() {
    for rpc_reads in [false, true] {
        let adapter = Arc::new(ControlledReadAdapter::new(rpc_reads));
        let engine = engine(adapter.clone());
        assert_eq!(
            engine
                .rpc_read_scope("synthetic.balance", &json!({}))
                .unwrap(),
            rpc_reads
        );
        let error = engine
            .rpc_read_scope("synthetic.transfer", &json!({}))
            .unwrap_err();
        assert_eq!(error.code, "WRITE_REQUIRES_APPROVAL");
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);

        let original = engine
            .execute(ordinary_request("scope-status"))
            .await
            .unwrap();
        assert_eq!(
            engine.status_rpc_read_scope(&original.id).unwrap(),
            rpc_reads
        );
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);

        adapter.inner.completed_response();
        let completed = engine.status(&original.id, true).await.unwrap();
        assert_eq!(completed.status, Status::Completed);
        assert!(!engine.status_rpc_read_scope(&original.id).unwrap());
        assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn future_rpc_plan_step_does_not_exempt_local_status_from_command_deadline() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    let request = serde_json::from_value(json!({
        "request_id":"future-rpc-plan",
        "valid_for_seconds":600,
        "plan":{
            "version":1,
            "name":"Synthetic future balance read",
            "steps":[{
                "step_id":"future-balance",
                "operation":"synthetic.balance",
                "input":{},
                "permitted_variations":{
                    "mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]
                },
                "completion":{"operation_succeeded":true},
                "funding_dependencies":[]
            }],
            "aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}
        }
    }))
    .unwrap();
    let plan = engine.authorize_financial_plan(request).await.unwrap();

    assert!(
        engine
            .rpc_read_scope("synthetic.balance", &json!({}))
            .unwrap()
    );
    assert!(!engine.financial_plan_rpc_read_scope(&plan.plan_id).unwrap());
    assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_plan_keeps_rpc_scope_only_until_original_attempt_settles() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    let prepared = adapter
        .inner
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    let credits = vec![
        gofin_domain::financial_plans::FinancialPlanFundingIdentity {
            resource: resource("ethereum-wallet"),
            asset: asset("ethereum:1", "native", 18),
        },
    ];
    let request = serde_json::from_value(json!({
        "request_id":"revoked-rpc-plan",
        "valid_for_seconds":600,
        "plan":{
            "version":1,
            "name":"Synthetic original transaction recovery",
            "steps":[{
                "step_id":"transfer",
                "operation":"synthetic.transfer",
                "input":{},
                "permitted_variations":{
                    "mode":"exact","maximum_debits":prepared.intent.debits,
                    "maximum_costs":[],"possible_credits":credits
                },
                "completion":{"operation_succeeded":true},
                "funding_dependencies":[]
            }],
            "aggregate_bounds":{"maximum_debits":prepared.intent.debits,"maximum_costs":[]}
        }
    }))
    .unwrap();
    let plan = engine.authorize_financial_plan(request).await.unwrap();
    assert!(!engine.financial_plan_rpc_read_scope(&plan.plan_id).unwrap());

    let original = engine
        .execute_financial_plan_step(
            &plan.plan_id,
            "transfer",
            OperationRequest {
                request_id: "original-rpc-plan-attempt".into(),
                operation: "synthetic.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending);
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .reservations(&original.id)
        .unwrap();
    assert!(!reservations.is_empty());
    engine.revoke_financial_plan(&plan.plan_id).await.unwrap();

    assert!(engine.financial_plan_rpc_read_scope(&plan.plan_id).unwrap());
    assert_eq!(
        engine.status(&original.id, false).await.unwrap().submission,
        original.submission
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap(),
        reservations
    );
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);

    adapter.inner.completed_response();
    let completed = engine.status(&original.id, true).await.unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.submission, original.submission);
    assert!(!engine.financial_plan_rpc_read_scope(&plan.plan_id).unwrap());
    assert!(!engine.status_rpc_read_scope(&original.id).unwrap());
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[async_trait]
impl Authorizer for CountingAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        SyntheticAuthorizer.authorize(challenge).await
    }
}

fn engine(adapter: Arc<ControlledReadAdapter>) -> Arc<Engine> {
    engine_with_authorizer(adapter, Arc::new(SyntheticAuthorizer))
}

fn engine_with_authorizer(
    adapter: Arc<ControlledReadAdapter>,
    authorizer: Arc<dyn Authorizer>,
) -> Arc<Engine> {
    Arc::new(
        Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter],
            authorizer,
            EngineOptions::default(),
        )
        .unwrap(),
    )
}

#[tokio::test(start_paused = true)]
async fn pure_engine_read_waits_beyond_old_command_deadline_then_returns() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let engine = engine(adapter.clone());
    let read = tokio::spawn(async move { engine.read("synthetic.balance", &json!({})).await });
    adapter.started.notified().await;

    tokio::time::advance(std::time::Duration::from_secs(121)).await;
    tokio::task::yield_now().await;
    assert!(
        !read.is_finished(),
        "a read must still wait after 120 seconds"
    );

    adapter.release.notify_one();
    assert_eq!(read.await.unwrap().unwrap(), json!({"balance":"42"}));
    assert_eq!(adapter.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelling_pure_engine_read_drops_pending_adapter_work() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let engine = engine(adapter.clone());
    let read = tokio::spawn(async move { engine.read("synthetic.balance", &json!({})).await });
    adapter.started.notified().await;

    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    assert_eq!(adapter.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pure_engine_read_still_rejects_financial_write_definitions() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let engine = engine(adapter.clone());

    let error = engine
        .read("synthetic.transfer", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "WRITE_REQUIRES_APPROVAL");
    assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn status_observation_waits_beyond_old_deadline_and_settles_original_identity() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    let original = engine
        .execute(ordinary_request("slow-status"))
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending);
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .reservations(&original.id)
        .unwrap();
    assert!(!reservations.is_empty());
    adapter.hold_reconciliation.store(true, Ordering::SeqCst);

    let status_engine = engine.clone();
    let id = original.id.clone();
    let status = tokio::spawn(async move { status_engine.status(&id, true).await });
    adapter.reconciliation_started.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(121)).await;
    tokio::task::yield_now().await;
    assert!(
        !status.is_finished(),
        "status evidence must still wait after 120 seconds"
    );
    assert_eq!(
        to_value(&engine.status(&original.id, true).await.unwrap()).unwrap(),
        to_value(&original).unwrap(),
        "another status call returns the durable record promptly while recovery is busy"
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap(),
        reservations
    );

    adapter.inner.completed_response();
    adapter.release_reconciliation.notify_one();
    let recovered = status.await.unwrap().unwrap();
    assert_eq!(recovered.id, original.id);
    assert_eq!(recovered.submission, original.submission);
    assert_eq!(recovered.approval, original.approval);
    assert_eq!(recovered.status, Status::Completed);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelling_status_observation_preserves_original_record_and_reservations() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    let original = engine
        .execute(ordinary_request("cancel-status"))
        .await
        .unwrap();
    let reservations = engine
        .store
        .lock()
        .unwrap()
        .reservations(&original.id)
        .unwrap();
    assert!(!reservations.is_empty());
    adapter.hold_reconciliation.store(true, Ordering::SeqCst);

    let status_engine = engine.clone();
    let id = original.id.clone();
    let status = tokio::spawn(async move { status_engine.status(&id, true).await });
    adapter.reconciliation_started.notified().await;
    status.abort();
    assert!(status.await.unwrap_err().is_cancelled());

    assert_eq!(adapter.reconciliation_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        to_value(&engine.status(&original.id, false).await.unwrap()).unwrap(),
        to_value(&original).unwrap()
    );
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap(),
        reservations
    );
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn post_dispatch_financial_reconciliation_retains_its_evidence_deadline() {
    let adapter = Arc::new(ControlledReadAdapter::new(true));
    let authorizer = Arc::new(CountingAuthorizer(AtomicUsize::new(0)));
    let engine = engine_with_authorizer(adapter.clone(), authorizer.clone());
    adapter.hold_reconciliation.store(true, Ordering::SeqCst);

    let started = tokio::time::Instant::now();
    let original = engine
        .execute(ordinary_request("bounded-financial-evidence"))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::Instant::now() - started,
        FINANCIAL_EVIDENCE_TIMEOUT
    );
    assert_eq!(original.status, Status::Pending);
    let error = original.error.as_ref().unwrap();
    assert_eq!(error.code, "EVIDENCE_TIMEOUT");
    assert_eq!(error.stage, "reconciliation");
    assert!(original.submission.is_some());
    assert!(
        !engine
            .store
            .lock()
            .unwrap()
            .reservations(&original.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
}
