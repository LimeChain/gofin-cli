use super::*;

struct LocalLocks;
#[async_trait::async_trait]
impl gofin_domain::Adapter for LocalLocks {
    fn definitions(&self) -> Vec<gofin_domain::Definition> {
        vec![PostPersistGuardAdapter::definition(), GenericReadAdapter::definition()]
    }
    fn plan_exclusive_resources(&self, _: &str, input: &Value) -> Result<Option<Vec<ResourceId>>> {
        Ok(input["known_lock"].as_str().map(|lock| vec![ResourceId(lock.into())]))
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        panic!("review must not execute reads")
    }
    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        panic!("review must not prepare a future-funded write")
    }
    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        panic!("review must not revalidate")
    }
    async fn submit(&self, _: &PreparedOperation, _: Arc<dyn gofin_domain::SubmissionJournal>) -> Result<Submission> {
        panic!("review must not submit")
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        panic!("review must not reconcile")
    }
}
struct CountApprovals(AtomicUsize);
#[async_trait::async_trait]
impl gofin_domain::Authorizer for CountApprovals {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<gofin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        gofin_domain::Authorizer::authorize(&PlanTestAuthorizer, challenge).await
    }
}
fn setup() -> (Engine, Arc<Mutex<Store>>, Arc<CountApprovals>) {
    let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
    let approvals = Arc::new(CountApprovals(AtomicUsize::new(0)));
    let engine = Engine::new_with_shared_store(store.clone(), vec![Arc::new(LocalLocks)], approvals.clone(), EngineOptions::default()).unwrap();
    (engine, store, approvals)
}
fn request() -> FinancialPlanAuthorizationRequest {
    let mut request = post_persist_plan_request("early-reservation");
    request.plan.steps[0].input["known_lock"] = json!("acct");
    request
}
fn seed(store: &Mutex<Store>, resource: &str, status: Status) -> Value {
    let mut record = settled_effect_record(vec![]);
    record.id = "original-unknown".into();
    record.request_id = "original-request".into();
    record.fingerprint = "original-fingerprint".into();
    record.status = status;
    record.effects = None;
    let mut prepared = funded_prepared("0", "0");
    prepared.intent = FinancialIntent { locks: vec![ResourceId(resource.into())], ..Default::default() };
    record.preview = Some(prepared);
    let value = to_value(&record).unwrap();
    let status = serde_json::to_value(&record.status).unwrap();
    store.lock().unwrap().create_request(
        &record.id, &record.request_id, &record.fingerprint, &record.operation,
        None, status.as_str().unwrap(), &value, &now()
    ).unwrap();
    value
}

#[tokio::test]
async fn known_conflict_rejects_preview_and_approval_without_changing_original() {
    let (engine, store, approvals) = setup();
    let original = seed(&store, "acct", Status::Unknown);
    for error in [
        engine.preview_financial_plan(request()).await.unwrap_err(),
        engine.authorize_financial_plan(request()).await.unwrap_err(),
    ] {
        assert_eq!(error.code, "RESOURCE_RESERVED");
        let details = error.details.unwrap();
        assert_eq!(details["blocking_operation_id"], "original-unknown");
        assert_eq!(details["blocking_status"], "unknown");
        assert_eq!(details["step_id"], "write");
        assert_eq!(details["before_plan_approval"], true);
        assert_eq!(details["conflicting_resources"], json!(["acct"]));
    }
    assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
    assert_eq!(store.lock().unwrap().get_record("original-unknown").unwrap(), Some(original));
}

#[tokio::test]
async fn unrelated_and_completed_records_do_not_prevent_review_or_approval() {
    for (resource, status) in [("other-account", Status::Unknown), ("acct", Status::Completed)] {
        let (engine, store, approvals) = setup();
        let original = seed(&store, resource, status);
        let mut request = request();
        request.expected_review_hash = Some(engine.preview_financial_plan(request.clone()).await.unwrap().review_hash);
        engine.authorize_financial_plan(request).await.unwrap();
        assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
        assert_eq!(store.lock().unwrap().get_record("original-unknown").unwrap(), Some(original));
    }
}

#[tokio::test]
async fn unknown_caller_caps_and_future_steps_are_not_treated_as_known_locks() {
    let (engine, store, _) = setup();
    seed(&store, "acct", Status::Unknown);
    let mut request = request();
    request.plan.steps[0].input.as_object_mut().unwrap().remove("known_lock");
    request.plan.steps[0].permitted_variations["maximum_debits"] = json!([resource_amount("acct", "USDT", "100")]);
    request.plan.aggregate_bounds.maximum_debits = vec![resource_amount("acct", "USDT", "100")];
    engine.preview_financial_plan(request.clone()).await.unwrap();
    let mut future = request.plan.steps[0].clone();
    future.step_id = "future-write".into();
    future.input["known_lock"] = json!("acct");
    request.plan.steps.push(future);
    request.plan.aggregate_bounds.maximum_debits[0].amount.atoms = Quantity::new("200").unwrap();
    engine.preview_financial_plan(request).await.unwrap();
}

#[tokio::test]
async fn conflict_arriving_after_preview_is_caught_before_approval_and_in_status() {
    let (engine, store, approvals) = setup();
    let mut request = request();
    request.expected_review_hash = Some(engine.preview_financial_plan(request.clone()).await.unwrap().review_hash);
    let authorized = engine.authorize_financial_plan(request.clone()).await.unwrap();
    seed(&store, "acct", Status::Unknown);
    request.request_id = "new-plan-after-conflict".into();
    assert_eq!(engine.authorize_financial_plan(request).await.unwrap_err().code, "RESOURCE_RESERVED");
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    let status = engine.financial_plan_status(&authorized.plan_id).await.unwrap();
    assert!(status.next_eligible_step_id.is_none());
    assert!(!status.authorization_timing.unwrap().can_prepare_next_action);
    assert_eq!(status.steps[0].state, FinancialPlanStepProgressState::Blocked);
    assert!(status.blockers.iter().any(|message| message.contains("original-unknown")));
}

#[test]
fn completed_prefix_and_preceding_reads_preserve_ordered_recovery_opportunity() {
    let (engine, store, _) = setup();
    seed(&store, "acct", Status::Unknown);
    let mut request = request();
    let mut suffix = request.plan.steps[0].clone();
    suffix.step_id = "suffix".into();
    suffix.input["known_lock"] = json!("other");
    request.plan.steps.push(suffix);
    engine.validate_next_plan_reservation(&request.plan, &["write".into()]).unwrap();
    request.plan.steps[0].operation = "future.read".into();
    request.plan.steps[0].input = json!({"account":"acct"});
    request.plan.steps[1].input["known_lock"] = json!("acct");
    engine.validate_next_plan_reservation(&request.plan, &[]).unwrap();
}
