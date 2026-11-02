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
    fail_first: AtomicBool,
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
            fail_first: AtomicBool::new(false),
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
        if self.fail_first.swap(false, Ordering::SeqCst) {
            return Err(GoFinError::validation("Synthetic pre-dispatch rejection."));
        }
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

struct TouchIdApproval(Arc<ManualClock>, AtomicUsize);
#[async_trait]
impl Authorizer for TouchIdApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::TouchId),
            yolo_provenance: None,
            mechanism: "touch-id".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: self.0.now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn standalone_touch_id_revalidation_after_prompt_deadline_keeps_all_checks() {
    let clock = Arc::new(ManualClock::new());
    let mut fixture = ReuseAdapter::new(clock.clone());
    fixture.window = None;
    fixture.advance_after_window = chrono::Duration::seconds(100);
    fixture.candidate_lifetime = Some(chrono::Duration::hours(1));
    let adapter = Arc::new(fixture);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(TouchIdApproval(clock.clone(), AtomicUsize::new(0))),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let result = engine
        .execute(ordinary_request("approval-revalidation-100s"))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Pending, "{:?}", result.error);
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
    let audit = result.approval.unwrap();
    assert_eq!(
        parse_time(&audit.expires_at).unwrap() - parse_time(&audit.approved_at).unwrap(),
        chrono::Duration::minutes(30)
    );
}

#[tokio::test]
async fn standalone_touch_id_expiry_still_blocks_late_admission() {
    let clock = Arc::new(ManualClock::new());
    let mut fixture = ReuseAdapter::new(clock.clone());
    fixture.window = None;
    fixture.advance_after_window = chrono::Duration::seconds(1800);
    fixture.candidate_lifetime = Some(chrono::Duration::hours(1));
    let adapter = Arc::new(fixture);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(TouchIdApproval(clock.clone(), AtomicUsize::new(0))),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let result = engine
        .execute(ordinary_request("approval-revalidation-1800s"))
        .await
        .unwrap();
    assert!(result.submission.is_none());
    assert_eq!(result.error.unwrap().code, "APPROVAL_EXPIRED");
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}

#[async_trait]
impl Authorizer for AdvancingApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
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

    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 1);
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
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
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
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
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
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
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
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
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
    assert_eq!(adapter.revalidations.load(Ordering::SeqCst), 2);
}

#[test]
fn standalone_touch_id_promotion_rejects_invalid_short_responses() {
    let now = Utc::now();
    let challenge = AuthorizationChallenge {
        review: None,
        purpose: "financial_write".into(),
        operation_id: "operation".into(),
        binding: json!({}),
        binding_hash: "binding".into(),
        summary: "test".into(),
        nonce: "nonce".into(),
        expires_at: (now + chrono::Duration::seconds(90)).to_rfc3339(),
    };
    let audit = ApprovalAudit {
        source: Some(gofin_domain::AuthorizationSource::TouchId),
        yolo_provenance: None,
        mechanism: "touch-id".into(),
        binding_hash: challenge.binding_hash.clone(),
        challenge_nonce: challenge.nonce.clone(),
        approved_at: now.to_rfc3339(),
        expires_at: challenge.expires_at.clone(),
    };
    validate_approval(&audit, &challenge, now).unwrap();
    let mut promoted = audit.clone();
    promote_standalone_touch_id_approval(&mut promoted, &challenge, now).unwrap();
    assert_eq!(
        parse_time(&promoted.expires_at).unwrap(),
        now + chrono::Duration::minutes(30)
    );
    assert_eq!(promoted.binding_hash, audit.binding_hash);
    assert_eq!(promoted.challenge_nonce, audit.challenge_nonce);
    for invalid in [
        "invalid".to_owned(),
        (now + chrono::Duration::seconds(1)).to_rfc3339(),
        (now - chrono::Duration::seconds(1)).to_rfc3339(),
    ] {
        let mut bad = audit.clone();
        bad.approved_at = invalid;
        assert!(promote_standalone_touch_id_approval(&mut bad, &challenge, now).is_err());
    }
    let mut late = audit.clone();
    assert!(
        promote_standalone_touch_id_approval(
            &mut late,
            &challenge,
            now + chrono::Duration::seconds(90)
        )
        .is_err()
    );
    let mut bad = audit.clone();
    bad.challenge_nonce = "other".into();
    assert!(validate_approval(&bad, &challenge, now).is_err());
    let mut non_touch = audit.clone();
    non_touch.source = None;
    promote_standalone_touch_id_approval(&mut non_touch, &challenge, now).unwrap();
    assert_eq!(non_touch.expires_at, challenge.expires_at);
}

#[tokio::test]
async fn standalone_touch_id_continuation_after_restart_preserves_original_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.db");
    let clock = Arc::new(ManualClock::new());
    let mut fixture = ReuseAdapter::new(clock.clone());
    fixture.window = None;
    fixture.candidate_lifetime = Some(chrono::Duration::hours(1));
    fixture.fail_first.store(true, Ordering::SeqCst);
    let adapter = Arc::new(fixture);
    let authorizer = Arc::new(TouchIdApproval(clock.clone(), AtomicUsize::new(0)));
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let original = engine
        .execute(ordinary_request("touchid-restart"))
        .await
        .unwrap();
    let expiry = original.approval.as_ref().unwrap().expires_at.clone();
    let token = engine
        .operation_view(&original)
        .unwrap()
        .pointer("/outcome/next_action/continuation_token")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    drop(engine);
    clock.advance(chrono::Duration::seconds(100));
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let restored = engine.status(&original.id, false).await.unwrap();
    assert_eq!(restored.approval.unwrap().expires_at, expiry);
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(10));
    let continued = invocation
        .scope(engine.continue_operation(&original.id, &token))
        .await
        .unwrap();
    assert!(continued["stage"]["submission"].is_object(), "{continued}");
    assert_eq!(authorizer.1.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn standalone_touch_id_explicit_reauthorization_gets_fresh_window_once() {
    let clock = Arc::new(ManualClock::new());
    let mut fixture = ReuseAdapter::new(clock.clone());
    fixture.window = None;
    fixture.candidate_lifetime = Some(chrono::Duration::hours(1));
    fixture.fail_first.store(true, Ordering::SeqCst);
    let adapter = Arc::new(fixture);
    let authorizer = Arc::new(TouchIdApproval(clock.clone(), AtomicUsize::new(0)));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let original = engine
        .execute(ordinary_request("touchid-reauthorize"))
        .await
        .unwrap();
    let original_expiry = original.approval.as_ref().unwrap().expires_at.clone();
    let token = engine
        .operation_view(&original)
        .unwrap()
        .pointer("/outcome/next_action/continuation_token")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();
    clock.advance(chrono::Duration::seconds(100));
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(10));
    let result = invocation
        .scope(engine.reauthorize_operation(&original.id, &token, original.input.clone()))
        .await
        .unwrap();
    assert_eq!(result["reauthorization"], "approved");
    let updated = engine.status(&original.id, false).await.unwrap();
    let audit = updated.approval.unwrap();
    assert_eq!(
        parse_time(&audit.expires_at).unwrap() - parse_time(&audit.approved_at).unwrap(),
        chrono::Duration::minutes(30)
    );
    assert!(parse_time(&audit.expires_at).unwrap() > parse_time(&original_expiry).unwrap());
    let duplicate = invocation
        .scope(engine.reauthorize_operation(&original.id, &token, original.input.clone()))
        .await
        .unwrap();
    assert_eq!(duplicate, result);
    assert_eq!(authorizer.1.load(Ordering::SeqCst), 2);
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn standalone_touch_id_does_not_extend_candidate_expiry() {
    let clock = Arc::new(ManualClock::new());
    let mut fixture = ReuseAdapter::new(clock.clone());
    fixture.window = Some(std::time::Duration::from_secs(300));
    fixture.advance_after_window = chrono::Duration::seconds(100);
    fixture.candidate_lifetime = Some(chrono::Duration::seconds(90));
    let adapter = Arc::new(fixture);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(TouchIdApproval(clock.clone(), AtomicUsize::new(0))),
        EngineOptions::default(),
    )
    .unwrap()
    .with_clock(clock);
    let result = engine
        .execute(ordinary_request("touchid-candidate-expired"))
        .await
        .unwrap();
    assert!(result.submission.is_none());
    assert_eq!(result.error.unwrap().code, "PREVIEW_EXPIRED");
    assert_eq!(adapter.inner.submissions.load(Ordering::SeqCst), 0);
}
