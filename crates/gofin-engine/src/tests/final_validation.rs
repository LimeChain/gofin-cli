use super::*;

struct CountingAdapter {
    inner: SyntheticAdapter,
    preparations: AtomicUsize,
    revalidations: AtomicUsize,
    submissions: AtomicUsize,
    change: bool,
    clock: Option<Arc<AdvancingClock>>,
}
#[async_trait]
impl Adapter for CountingAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.inner.read(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        self.inner.prepare(operation, input).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.revalidations.fetch_add(1, Ordering::SeqCst);
        let mut fresh = prepared.clone();
        if self.change {
            fresh.binding["amount"] = json!("2");
        }
        Ok(fresh)
    }
    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        panic!("Engine must hand off its completed final validation")
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        journal.assert_candidate(validated.prepared())?;
        if let Some(clock) = &self.clock {
            *clock.0.lock().unwrap() += chrono::Duration::minutes(3);
        }
        self.submissions.fetch_add(1, Ordering::SeqCst);
        self.inner.submit(validated.prepared(), journal).await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.inner.reconcile(record).await
    }
}

#[tokio::test]
async fn engine_hands_off_one_final_validation_only_after_binding_checks() {
    for change in [false, true] {
        let adapter = Arc::new(CountingAdapter {
            inner: SyntheticAdapter::new(),
            preparations: AtomicUsize::new(0),
            revalidations: AtomicUsize::new(0),
            submissions: AtomicUsize::new(0),
            change,
            clock: None,
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let request = OperationRequest {
            request_id: "synthetic-single-final-validation".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        };
        let result = engine.execute(request.clone()).await.unwrap();
        assert_eq!(adapter.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
        assert_eq!(
            adapter.submissions.load(Ordering::SeqCst),
            usize::from(!change)
        );
        if change {
            assert_eq!(result.error.unwrap().code, "REQUEST_CHANGED");
        } else {
            assert_eq!(result.status, Status::Pending);
            let _ = engine.execute(request).await.unwrap();
            assert_eq!(
                adapter.submissions.load(Ordering::SeqCst),
                1,
                "retry must reconcile its original identity"
            );
        }
    }
}

struct AdvancingClock(Mutex<chrono::Duration>);
impl gofin_domain::clock::Clock for AdvancingClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now() + *self.0.lock().unwrap()
    }
}

#[tokio::test]
async fn transport_delay_rechecks_candidate_expiry_before_durable_submission() {
    let clock = Arc::new(AdvancingClock(Mutex::new(chrono::Duration::zero())));
    let adapter = Arc::new(CountingAdapter {
        inner: SyntheticAdapter::new(),
        preparations: AtomicUsize::new(0),
        revalidations: AtomicUsize::new(0),
        submissions: AtomicUsize::new(0),
        change: false,
        clock: Some(clock.clone()),
    });
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let result = engine
        .execute(OperationRequest {
            request_id: "synthetic-delayed-dispatch-expiry".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(result.error.unwrap().code, "PREVIEW_EXPIRED");
    assert!(result.submission.is_none());
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}
