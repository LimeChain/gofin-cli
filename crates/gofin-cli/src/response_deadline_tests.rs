use super::*;
use gofin_domain::execution_progress::{self, Stage};
#[cfg(target_os = "macos")]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn execution_request() -> BrokerRequest {
    BrokerRequest::ExecutePlanStep {
        plan_id: "synthetic-plan".into(),
        step_id: "transfer".into(),
        request: OperationRequest {
            request_id: "logical-execution".into(),
            operation: "synthetic.trade".into(),
            input: json!({"account":"synthetic-account","units":"1"}),
            run_id: None,
        },
    }
}

#[tokio::test(start_paused = true)]
async fn financial_response_duplicate_progress_and_partial_frames_do_not_renew_idle_deadline() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let (tx, mut stages) = execution_progress::channel();
    let read = tokio::spawn(async move {
        execution_progress::observe(tx, read_response_with_policy(reader, &execution_request()))
            .await
    });
    writer
        .write_all(b"{\"progress\":\"preparing\"}\n")
        .await
        .unwrap();
    assert_eq!(stages.recv().await, Some(Stage::Preparing));
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(100)).await;
        writer
            .write_all(b"{\"progress\":\"preparing\"}\n")
            .await
            .unwrap();
        assert_eq!(stages.recv().await, Some(Stage::Preparing));
    }
    writer
        .write_all(b"{\"progress\":\"refreshing_candidate\"")
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(101)).await;
    assert_eq!(read.await.unwrap().unwrap_err().code, "BROKER_TIMEOUT");
}

#[tokio::test(start_paused = true)]
async fn financial_response_without_progress_keeps_original_deadline() {
    let (_writer, reader) = tokio::io::duplex(1024);
    let read =
        tokio::spawn(async move { read_response_with_policy(reader, &execution_request()).await });
    tokio::task::yield_now().await;
    tokio::time::advance(FINANCIAL_WRITE_TIMEOUT + Duration::from_millis(1)).await;
    assert_eq!(read.await.unwrap().unwrap_err().code, "BROKER_TIMEOUT");
}

#[cfg(target_os = "macos")]
#[tokio::test(start_paused = true)]
async fn unix_financial_response_survives_three_bounded_engine_preparations() {
    use gofin_domain::financial_plans::*;
    use gofin_domain::{
        Adapter, OperationRecord, PreparedOperation, Reconciliation, Submission, SubmissionJournal,
    };
    struct SlowPrices {
        prepares: AtomicUsize,
    }
    #[async_trait]
    impl Adapter for SlowPrices {
        fn definitions(&self) -> Vec<gofin_domain::Definition> {
            vec![synthetic_boundary_definition()]
        }
        async fn read(&self, _: &str, _: &Value) -> Result<Value, GoFinError> {
            unreachable!()
        }
        async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation, GoFinError> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(119)).await;
            Err(GoFinError::new(
                "PRICE_STALE",
                "valuation",
                "Synthetic provider quote remains stale.",
                "Wait for fresh provider evidence.",
            ))
        }
        async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation, GoFinError> {
            unreachable!("stale preparation cannot reach revalidation")
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> Result<Submission, GoFinError> {
            panic!("stale preparation must not submit")
        }
        async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation, GoFinError> {
            unreachable!()
        }
    }
    let adapter = Arc::new(SlowPrices {
        prepares: AtomicUsize::new(0),
    });
    let approvals = Arc::new(SyntheticBoundaryAuthorizer(AtomicUsize::new(0)));
    let engine = Arc::new(
        gofin_engine::Engine::new(
            gofin_store::Store::in_memory().unwrap(),
            vec![adapter.clone()],
            approvals.clone(),
            gofin_engine::EngineOptions::default(),
        )
        .unwrap(),
    );
    let debit = json!({"resource":"synthetic:cash","amount":{"asset":{"network":"synthetic","reference":"USD","decimals":6},"atoms":"1000000"}});
    let mut plan: FinancialPlanSpec = serde_json::from_str(sample_plan_json()).unwrap();
    plan.steps[0].operation = "synthetic.trade".into();
    plan.steps[0].input = json!({"account":"synthetic-account","units":"1"});
    plan.steps[0].permitted_variations =
        json!({"mode":"exact","maximum_debits":[debit],"maximum_costs":[],"possible_credits":[]});
    plan.aggregate_bounds.maximum_debits = serde_json::from_value(json!([debit])).unwrap();
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-plan-authority".into(),
        plan,
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    authorization.expected_review_hash = Some(
        engine
            .preview_financial_plan(authorization.clone())
            .await
            .unwrap()
            .review_hash,
    );
    let plan = engine
        .authorize_financial_plan(authorization)
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = gofin_protocol::read_request_envelope(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(envelope.progress);
        assert!(!envelope.cancellable_read());
        let BrokerRequest::ExecutePlanStep {
            plan_id,
            step_id,
            request,
        } = envelope.request
        else {
            panic!("unexpected request")
        };
        let result = gofin_protocol::progress::observe_request(
            reader.get_mut(),
            true,
            false,
            engine.execute_financial_plan_step(&plan_id, &step_id, request),
        )
        .await
        .unwrap();
        let response = BrokerResponse::failure(result.unwrap_err());
        reader
            .get_mut()
            .write_all(&gofin_protocol::encode_response(&response).unwrap())
            .await
            .unwrap();
    });
    let transport = UnixTransport {
        progress_details: tokio::sync::OnceCell::new(),
        path,
        expected_uid: unsafe { libc::geteuid() },
    };
    let mut request = execution_request();
    if let BrokerRequest::ExecutePlanStep { plan_id, .. } = &mut request {
        *plan_id = plan.plan_id;
    }
    // This deliberately has no presentation observer. Internal transport progress
    // must protect plain --json just as it protects terminal/stream output.
    let started = tokio::time::Instant::now();
    let client = tokio::spawn(async move { Transport::call(&transport, request).await });
    // Drive the real engine's bounded retry path while allowing local socket I/O.
    // Paused Tokio time advances independently of provider/authority wall clocks.
    let error = client.await.unwrap().unwrap_err();
    assert!(started.elapsed() > FINANCIAL_WRITE_TIMEOUT);
    assert_eq!(error.code, "PRICE_STALE");
    assert_eq!(error.details.as_ref().unwrap()["refresh_count"], 2);
    assert_eq!(adapter.prepares.load(Ordering::SeqCst), 3);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    server.await.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn lost_unix_financial_response_checks_plan_status_without_replaying_execution() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let dispatched = executions.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = gofin_protocol::read_request_envelope(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(
            envelope.progress,
            "plain JSON execution negotiates internal progress"
        );
        assert!(!envelope.cancellable_read());
        assert!(matches!(
            envelope.request,
            BrokerRequest::ExecutePlanStep { .. }
        ));
        dispatched.fetch_add(1, Ordering::SeqCst);
        drop(reader); // Lost response says nothing about whether financial work ran.
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = gofin_protocol::read_request_envelope(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(envelope.request, BrokerRequest::PlanStatus { .. }));
        reader
            .get_mut()
            .write_all(
                &gofin_protocol::encode_response(&BrokerResponse::success(json!({"steps":[]})))
                    .unwrap(),
            )
            .await
            .unwrap();
    });
    let transport = UnixTransport {
        progress_details: tokio::sync::OnceCell::new(),
        path,
        expected_uid: unsafe { libc::geteuid() },
    };
    let BrokerRequest::ExecutePlanStep {
        plan_id,
        step_id,
        request,
    } = execution_request()
    else {
        unreachable!()
    };
    let error = execute_plan_step_with_recovery(&transport, plan_id, step_id, request)
        .await
        .unwrap_err();
    assert_eq!(error.code, "OUTCOME_UNKNOWN");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    server.await.unwrap();
}
