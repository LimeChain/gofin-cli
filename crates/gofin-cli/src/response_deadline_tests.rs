use super::*;
use gofin_domain::execution_progress::{self, Stage};

#[test]
fn status_observations_remain_bounded_after_rpc_negotiation() {
    for request in [
        BrokerRequest::Status {
            id: "original".into(),
        },
        BrokerRequest::StatusSide {
            id: "original".into(),
            side: "destination".into(),
        },
    ] {
        let mut deadline = response_deadline::ResponseDeadline::new(Some(&request));
        let original = deadline.at();
        assert!(original.is_some());
        deadline.allow_rpc_read();
        assert_eq!(deadline.at(), original);
    }
}
#[cfg(target_os = "macos")]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn execution_request() -> BrokerRequest {
    BrokerRequest::Execute {
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
async fn unix_financial_response_reports_bounded_native_preparation_failure() {
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
                "REQUIRED_QUOTE_UNAVAILABLE",
                "preparation",
                "Synthetic executable quote is unavailable.",
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
        let BrokerRequest::Execute { request } = envelope.request else {
            panic!("unexpected request")
        };
        let result = gofin_protocol::progress::observe_financial_request(
            reader.get_mut(),
            true,
            false,
            engine.execute(request),
        )
        .await;
        let response = match result {
            Ok(record) => BrokerResponse::success(engine.operation_view(&record).unwrap()),
            Err(error) => BrokerResponse::failure(error),
        };
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
    let request = execution_request();
    // Plain JSON negotiates real engine progress without a presentation observer.
    // Unix socket readiness is driven outside Tokio's timer queue. Keep the
    // paused clock from jumping to the response deadline while the OS delivers
    // a ready frame, and advance only the adapter's controlled preparation.
    let clock_driver = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let started = tokio::time::Instant::now();
    let client = tokio::spawn(async move { Transport::call(&transport, request).await });
    while adapter.prepares.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(119)).await;
    let result = client.await.unwrap();
    clock_driver.abort();
    let code = match result {
        Ok(value) => value["error"]["code"].as_str().unwrap().to_owned(),
        Err(error) => error.code,
    };
    assert!(started.elapsed() <= FINANCIAL_WRITE_TIMEOUT);
    assert_eq!(code, "REQUIRED_QUOTE_UNAVAILABLE");
    assert_eq!(adapter.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
    server.await.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn lost_unix_financial_response_checks_history_without_replaying_execution() {
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
        assert!(matches!(envelope.request, BrokerRequest::Execute { .. }));
        dispatched.fetch_add(1, Ordering::SeqCst);
        drop(reader); // Lost response says nothing about whether financial work ran.
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let envelope = gofin_protocol::read_request_envelope(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(envelope.request, BrokerRequest::Operations));
        reader
            .get_mut()
            .write_all(
                &gofin_protocol::encode_response(&BrokerResponse::success(json!([]))).unwrap(),
            )
            .await
            .unwrap();
    });
    let transport = UnixTransport {
        progress_details: tokio::sync::OnceCell::new(),
        path,
        expected_uid: unsafe { libc::geteuid() },
    };
    let BrokerRequest::Execute { request } = execution_request() else {
        unreachable!()
    };
    let error = execute_with_recovery(&transport, request)
        .await
        .unwrap_err();
    assert_eq!(error.code, "OUTCOME_UNKNOWN");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    server.await.unwrap();
}
