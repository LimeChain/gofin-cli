use super::*;
use gofin_domain::clock::Clock;

struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn new() -> Self {
        // Keep the engine's injected clock ahead of the fixture adapter's
        // SystemClock timestamps so its observations are never future-dated.
        Self(Mutex::new(Utc::now() + chrono::Duration::seconds(10)))
    }

    fn advance(&self, duration: chrono::Duration) {
        *self.0.lock().unwrap() += duration;
    }
}

impl gofin_domain::clock::Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

struct ReuseAdapter {
    inner: SyntheticAdapter,
    clock: Arc<ManualClock>,
    preparation_delay: std::time::Duration,
    window: Option<std::time::Duration>,
    candidate_lifetime: Option<chrono::Duration>,
    candidate_observation_age: Option<chrono::Duration>,
    stale_price_after_window: bool,
    advance_after_window: chrono::Duration,
    revalidations: AtomicUsize,
}

impl ReuseAdapter {
    fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            inner: SyntheticAdapter::new(),
            clock,
            preparation_delay: std::time::Duration::ZERO,
            window: Some(std::time::Duration::from_secs(30)),
            candidate_lifetime: None,
            candidate_observation_age: None,
            stale_price_after_window: false,
            advance_after_window: chrono::Duration::zero(),
            revalidations: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Adapter for ReuseAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.inner.read(operation, input).await
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if !self.preparation_delay.is_zero() {
            tokio::time::sleep(self.preparation_delay).await;
        }
        let mut prepared = self.inner.prepare(operation, input).await?;
        if let Some(lifetime) = self.candidate_lifetime {
            prepared.expires_at = (self.clock.now() + lifetime).to_rfc3339();
        }
        if let Some(age) = self.candidate_observation_age {
            let observed_at = (self.clock.now() - age).to_rfc3339();
            prepared.observed_at = observed_at.clone();
            if let Some(valuation) = &mut prepared.valuation {
                valuation.observed_at = observed_at.clone();
                valuation.prices.push(gofin_domain::AssetPrice {
                    asset: asset("ethereum:1", "native", 18),
                    usd_micros_per_unit: quantity("1000000"),
                    unit_atoms: quantity("1000000000000000000"),
                    observed_at: observed_at.clone(),
                    source: "synthetic generic quote".into(),
                });
            }
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        if self.stale_price_after_window {
            let observed_at = self.clock.now() - chrono::Duration::seconds(119);
            let valuation = prepared.valuation.as_mut().expect("synthetic valuation");
            valuation.prices.push(gofin_domain::AssetPrice {
                asset: asset("ethereum:1", "native", 18),
                usd_micros_per_unit: quantity("1000000"),
                unit_atoms: quantity("1000000000000000000"),
                observed_at: observed_at.to_rfc3339(),
                source: "synthetic generic quote".into(),
            });
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }

    fn preparation_reuse_window(
        &self,
        _prepared: &PreparedOperation,
    ) -> Option<std::time::Duration> {
        self.clock.advance(self.advance_after_window);
        self.window
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.revalidations.fetch_add(1, Ordering::SeqCst);
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

struct AdvancingApproval {
    clock: Arc<ManualClock>,
    delay: std::time::Duration,
}

#[async_trait]
impl Authorizer for AdvancingApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(ApprovalAudit {
            mechanism: "evidence-reuse-test".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: self.clock.now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

async fn execute_with(
    adapter: Arc<ReuseAdapter>,
    clock: Arc<ManualClock>,
    approval_delay: std::time::Duration,
    request_id: &str,
) -> OperationRecord {
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(AdvancingApproval {
            clock: clock.clone(),
            delay: approval_delay,
        }),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    engine
        .execute(OperationRequest {
            request_id: request_id.into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn fresh_evidence_is_reused_without_widening_exact_intent() {
    let clock = Arc::new(ManualClock::new());
    let adapter = Arc::new(ReuseAdapter::new(clock.clone()));
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-fresh",
    )
    .await;

    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 0);
    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    let intent = &result.preview.unwrap().intent;
    assert_eq!(
        intent.debits[0].amount.atoms,
        quantity("1000000000000000000")
    );
    assert_eq!(intent.expected, vec![transfer_constraint()]);
}

#[tokio::test(start_paused = true)]
async fn preparation_age_starts_before_slow_adapter_work() {
    let clock = Arc::new(ManualClock::new());
    let mut adapter = ReuseAdapter::new(clock.clone());
    adapter.preparation_delay = std::time::Duration::from_secs(31);
    let adapter = Arc::new(adapter);
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-slow-preparation",
    )
    .await;

    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn delayed_human_approval_falls_back_to_fresh_adapter_validation() {
    let clock = Arc::new(ManualClock::new());
    let adapter = Arc::new(ReuseAdapter::new(clock.clone()));
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::from_secs(31),
        "evidence-reuse-delayed-approval",
    )
    .await;

    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn older_but_generically_fresh_observation_falls_back_to_revalidation() {
    let clock = Arc::new(ManualClock::new());
    let mut adapter = ReuseAdapter::new(clock.clone());
    adapter.candidate_observation_age = Some(chrono::Duration::seconds(45));
    let adapter = Arc::new(adapter);
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-old-observation",
    )
    .await;

    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn expired_candidate_is_revalidated_and_never_restamped() {
    let clock = Arc::new(ManualClock::new());
    let mut adapter = ReuseAdapter::new(clock.clone());
    adapter.candidate_lifetime = Some(chrono::Duration::seconds(1));
    adapter.advance_after_window = chrono::Duration::seconds(2);
    let adapter = Arc::new(adapter);
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-expired-candidate",
    )
    .await;

    assert_eq!(result.error.unwrap().code, "PREVIEW_EXPIRED");
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn expired_price_observation_is_not_extended_by_reuse() {
    let clock = Arc::new(ManualClock::new());
    let mut adapter = ReuseAdapter::new(clock.clone());
    adapter.stale_price_after_window = true;
    adapter.advance_after_window = chrono::Duration::seconds(2);
    let adapter = Arc::new(adapter);
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-expired-price",
    )
    .await;

    assert_eq!(result.error.unwrap().code, "STALE_VALUATION");
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn adapters_without_a_reuse_window_keep_final_revalidation() {
    let clock = Arc::new(ManualClock::new());
    let mut adapter = ReuseAdapter::new(clock.clone());
    adapter.window = None;
    let adapter = Arc::new(adapter);
    let result = execute_with(
        adapter.clone(),
        clock,
        std::time::Duration::ZERO,
        "evidence-reuse-unsupported",
    )
    .await;

    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
}
