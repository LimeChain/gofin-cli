struct PreparationStackAdapter {
    inner: SyntheticAdapter,
    invalid_prepared: bool,
    failures: AtomicUsize,
    executor_depths: Mutex<Vec<usize>>,
}

#[async_trait]
impl Adapter for PreparationStackAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.inner.read(operation, input).await
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        // Count live executor poll frames, rather than relying on a machine's
        // stack size or inducing an abort. Recursive boxed futures still add
        // these frames; successive iterations of one executor do not.
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        self.executor_depths.lock().unwrap().push(
            backtrace
                .lines()
                .filter(|line| line.contains("execute_financial_plan_step_serial"))
                .count(),
        );
        let fail = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if fail && !self.invalid_prepared {
            return Err(GoFinError::new(
                "PREVIEW_EXPIRED",
                "preflight",
                "Synthetic preparation expired.",
                "Refresh within the approved scope.",
            ));
        }
        let mut prepared = self.inner.prepare(operation, input).await?;
        if fail {
            prepared.expires_at = "2000-01-01T00:00:00Z".into();
        }
        Ok(prepared)
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
        self.inner.reconcile(record).await
    }
}

#[tokio::test]
async fn preparation_and_validation_refreshes_keep_constant_poll_depth_and_budget() {
    for invalid_prepared in [false, true] {
        for failures in [2, 10] {
            let adapter = Arc::new(PreparationStackAdapter {
                inner: SyntheticAdapter::new(),
                invalid_prepared,
                failures: AtomicUsize::new(0),
                executor_depths: Mutex::new(vec![]),
            });
            let authorizer = Arc::new(FailSecondAuthorizer {
                calls: AtomicUsize::new(0),
            });
            let directory = tempfile::tempdir().unwrap();
            let (engine, plan_id) = recovery_plan_engine(
                &directory.path().join("preparation-stack.sqlite"),
                adapter.clone(),
                authorizer.clone(),
                Arc::new(RecoveryClock(Mutex::new(Utc::now()))),
            )
            .await;
            let engine = engine.with_clock(Arc::new(gofin_domain::clock::SystemClock));
            adapter.executor_depths.lock().unwrap().clear();
            adapter.failures.store(failures, Ordering::SeqCst);
            adapter.inner.completed_response();
            let request = OperationRequest {
                request_id: "bounded-preparation-stack".into(),
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
            let depths = adapter.executor_depths.lock().unwrap().clone();
            assert_eq!(depths.len(), 3);
            assert!(depths[0] > 0, "fixture must observe executor symbols");
            assert!(
                depths.iter().all(|depth| *depth == depths[0]),
                "refresh must not nest executor poll frames: {depths:?}"
            );
            assert_refresh_progress(
                &mut progress,
                "PREVIEW_EXPIRED",
                gofin_domain::execution_progress::RefreshPhase::Preparation,
                2,
            );
            if failures == 2 {
                let completed = result.unwrap();
                assert_eq!(completed.status, Status::Completed, "{:?}", completed.error);
                let duplicate = engine
                    .execute_financial_plan_step(&plan_id, "transfer", request)
                    .await
                    .unwrap();
                assert_eq!(duplicate.id, completed.id);
                assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.code, "PREVIEW_EXPIRED");
                assert_eq!(error.details.as_deref().unwrap()["refresh_count"], 2);
                assert_eq!(error.details.as_deref().unwrap()["refresh_exhausted"], true);
                assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
            }
            assert_eq!(authorizer.calls.load(Ordering::SeqCst), 1);
        }
    }
}
