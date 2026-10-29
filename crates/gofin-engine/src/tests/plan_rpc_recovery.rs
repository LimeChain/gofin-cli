use std::{future::Future, task::Poll, time::Duration};

fn rpc_failure(
    stage: &'static str,
    code: &'static str,
    details: Value,
    clock: Arc<RecoveryClock>,
) -> Arc<CandidateFailure> {
    Arc::new(CandidateFailure {
        inner: SyntheticAdapter::new(),
        stage,
        failure_code: code,
        failure_details: details,
        failures_remaining: AtomicUsize::new(0),
        expire_on_failure: false,
        clock,
        block_preparation: None,
        reuse_window: None,
        candidate_lifetime: None,
        expire_candidate_once: AtomicBool::new(false),
        logical_operation_ids: None,
    })
}
fn rpc_request() -> OperationRequest {
    OperationRequest {
        request_id: "rpc-logical".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    }
}
fn cooldown() -> Value {
    json!({"retry_after_ms":1,"rpc_health":{"endpoints":[
        {"status":"unavailable","last_error_code":"RPC_HISTORICAL_STATE_UNAVAILABLE","retry_after_ms":null},
        {"status":"cooling_down","retry_after_ms":28891}
    ]}})
}

#[tokio::test(start_paused = true)]
async fn rpc_cooldown_waits_before_refresh_and_rechecks_authority() {
    for stage in ["preflight", "revalidation"] {
        for expire in [false, true] {
            let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
            let adapter = rpc_failure(
                stage,
                "RPC_HISTORICAL_STATE_UNAVAILABLE",
                cooldown(),
                clock.clone(),
            );
            let authorizer = Arc::new(FailSecondAuthorizer {
                calls: AtomicUsize::new(0),
            });
            let directory = tempfile::tempdir().unwrap();
            let (engine, plan_id) = recovery_plan_engine(
                &directory.path().join("rpc.sqlite"),
                adapter.clone(),
                authorizer.clone(),
                clock.clone(),
            )
            .await;
            adapter.failures_remaining.store(1, Ordering::SeqCst);
            adapter.inner.completed_response();
            let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
            let execution = gofin_domain::execution_progress::observe_events(
                observer,
                engine.execute_financial_plan_step(&plan_id, "transfer", rpc_request()),
            );
            tokio::pin!(execution);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(execution.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            assert_eq!(adapter.failures_remaining.load(Ordering::SeqCst), 0);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
            tokio::time::advance(Duration::from_millis(28890)).await;
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(execution.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            if expire {
                *clock.0.lock().unwrap() += chrono::Duration::hours(1);
            }
            tokio::time::advance(Duration::from_millis(1)).await;
            let result = execution.await;
            if expire {
                assert_eq!(result.unwrap_err().code, "PLAN_AUTHORIZATION_INACTIVE");
                assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
            } else {
                let completed = result.unwrap();
                assert_eq!(completed.status, Status::Completed);
                let duplicate = engine
                    .execute_financial_plan_step(&plan_id, "transfer", rpc_request())
                    .await
                    .unwrap();
                assert_eq!(duplicate.id, completed.id);
                assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            }
            assert_refresh_progress(
                &mut progress,
                "RPC_HISTORICAL_STATE_UNAVAILABLE",
                if stage == "preflight" {
                    gofin_domain::execution_progress::RefreshPhase::Preparation
                } else {
                    gofin_domain::execution_progress::RefreshPhase::Revalidation
                },
                1,
            );
            assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn rpc_recovery_is_bounded_and_never_retries_generic_rejection_or_submitted_write() {
    for (stage, code, details, refreshes) in [
        (
            "preflight",
            "RPC_HISTORICAL_STATE_UNAVAILABLE",
            cooldown(),
            2usize,
        ),
        (
            "preflight",
            "RPC_RATE_LIMITED",
            json!({"retry_after_ms":100}),
            2,
        ),
        (
            "preflight",
            "RPC_HISTORICAL_STATE_UNAVAILABLE",
            json!({"retry_after_ms":30001}),
            0,
        ),
        (
            "preflight",
            "RPC_RATE_LIMITED",
            json!({"retry_after_ms":30001,"rpc_health":{"endpoints":[{"status":"cooling_down","retry_after_ms":1}]}}),
            0,
        ),
        ("preflight", "RPC_RATE_LIMITED", json!({}), 0),
        ("preflight", "RPC_REQUEST_FAILED", cooldown(), 0),
        (
            "submission",
            "RPC_HISTORICAL_STATE_UNAVAILABLE",
            cooldown(),
            0,
        ),
    ] {
        let clock = Arc::new(RecoveryClock(Mutex::new(Utc::now())));
        let adapter = rpc_failure(stage, code, details.clone(), clock.clone());
        let authorizer = Arc::new(FailSecondAuthorizer {
            calls: AtomicUsize::new(0),
        });
        let directory = tempfile::tempdir().unwrap();
        let (engine, plan_id) = recovery_plan_engine(
            &directory.path().join("bounded.sqlite"),
            adapter.clone(),
            authorizer.clone(),
            clock,
        )
        .await;
        adapter.failures_remaining.store(10, Ordering::SeqCst);
        let (observer, mut progress) = gofin_domain::execution_progress::event_channel();
        let result = gofin_domain::execution_progress::observe_events(
            observer,
            engine.execute_financial_plan_step(&plan_id, "transfer", rpc_request()),
        )
        .await;
        if stage == "submission" {
            let record = result.unwrap();
            assert!(record.submission.is_some());
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            let duplicate = engine
                .execute_financial_plan_step(&plan_id, "transfer", rpc_request())
                .await
                .unwrap();
            assert_eq!(duplicate.id, record.id);
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(
                error.details.as_deref().unwrap()["refresh_count"],
                refreshes
            );
            if details["retry_after_ms"] == 30001 {
                assert_eq!(error.details.as_deref().unwrap()["retry_after_ms"], 30001);
                assert!(error.next_action.contains("cooldown"));
            }
            assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
        }
        assert_eq!(
            adapter.failures_remaining.load(Ordering::SeqCst),
            9 - refreshes
        );
        if refreshes > 0 {
            assert_refresh_progress(
                &mut progress,
                code,
                gofin_domain::execution_progress::RefreshPhase::Preparation,
                refreshes,
            );
        } else {
            while let Ok(event) = progress.try_recv() {
                assert!(event.candidate_refresh.is_none());
            }
        }
        assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
    }
}
