use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use fin_domain::{OperationRecord, PreparedOperation, ResourceId, Status, Submission};
use fin_exchange::{
    ExchangeProfile,
    coinbase::{
        CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse,
        CoinbaseRoute, DispatchGuard,
    },
};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Api(Mutex<VecDeque<CoinbaseResponse>>);
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        assert_eq!(
            endpoint,
            CoinbaseRoute::ExchangeOrder {
                order_id: "venue-order-1".into()
            }
        );
        assert!(request.body.is_none());
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
struct GuardApi {
    responses: Mutex<VecDeque<CoinbaseResponse>>,
    calls: Mutex<Vec<CoinbaseEndpoint>>,
}
#[async_trait]
impl CoinbaseApi for GuardApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.calls.lock().unwrap().push(endpoint.clone());
        if let Some(check) = guard
            && !check()
        {
            return Err(CoinbaseError::GuardRejected);
        }
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
#[derive(Default)]
struct Journal {
    submission: Mutex<Option<Submission>>,
    evidence: Mutex<Option<Value>>,
}
impl fin_domain::SubmissionJournal for Journal {
    fn assert_authorized(&self) -> fin_domain::Result<()> {
        Ok(())
    }
    fn assert_candidate(&self, _: &PreparedOperation) -> fin_domain::Result<()> {
        Ok(())
    }
    fn persist_submission(&self, submission: &Submission) -> fin_domain::Result<()> {
        *self.submission.lock().unwrap() = Some(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(
        &self,
        _: &Submission,
        evidence: &Value,
    ) -> fin_domain::Result<()> {
        *self.evidence.lock().unwrap() = Some(evidence.clone());
        Ok(())
    }
}
fn response(status: &str) -> CoinbaseResponse {
    CoinbaseResponse::new(
        200,
        serde_json::to_vec(&json!({
            "id":"venue-order-1","client_oid":"client-1","profile_id":"profile-1",
            "product_id":"BTC-USD","side":"buy","status":status,
        "filled_size":if status == "open" {"0"} else {"0.01"},"executed_value":"1","fill_fees":"0"
        }))
        .unwrap(),
    )
}
fn account() -> CoinbaseAccount {
    CoinbaseAccount {
        name: "exchange-main".into(),
        resource: ResourceId("coinbaseexchange:portfolio:profile-1".into()),
        credential_ref: "exchange-key".into(),
        physical_identity: "profile-1".into(),
        identity_verified: true,
        portfolio_id: None,
        product: CoinbaseProduct::Exchange,
    }
}
fn prepared_parent() -> (PreparedOperation, Submission) {
    let submission = Submission {
        kind: "coinbase".into(),
        reference: "client-1".into(),
        data: json!({"product":"coinbaseexchange","clientOrderId":"client-1","account":"exchange-main","productId":"BTC-USD"}),
    };
    let prepared = PreparedOperation {
        operation: "coinbaseexchange.create-order".into(),
        input: json!({"account":"exchange-main"}),
        binding: json!({"account":"exchange-main","physicalIdentity":"profile-1","productId":"BTC-USD","clientOrderId":"client-1"}),
        summary: "parent order".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: fin_domain::now(),
        expires_at: chrono::Utc::now().to_rfc3339(),
        valuation: None,
        intent: fin_domain::FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![],
        },
        operation_version: 1,
        action_deadline: None,
    };
    (prepared, submission)
}
fn record(
    id: &str,
    prepared: PreparedOperation,
    submission: Submission,
    dispatch_evidence: Option<Value>,
) -> OperationRecord {
    OperationRecord {
        id: id.into(),
        request_id: "req-1".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission),
        dispatch_evidence,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
struct State(OperationRecord);
impl fin_domain::execution_state::ExecutionState for State {
    fn allocate_nonce(&self, _: &str, _: u64) -> fin_domain::Result<u64> {
        Ok(1)
    }
    fn operation(&self, id: &str) -> fin_domain::Result<Option<OperationRecord>> {
        Ok((self.0.id == id).then(|| self.0.clone()))
    }
    fn operations_by_prepared_binding(
        &self,
        operation: &str,
        pointer: &str,
        value: &Value,
    ) -> fin_domain::Result<Vec<OperationRecord>> {
        Ok(
            if operation == "coinbaseexchange.create-order"
                && pointer == "/preview/binding/clientOrderId"
                && self
                    .0
                    .preview
                    .as_ref()
                    .is_some_and(|p| p.binding["clientOrderId"] == *value)
            {
                vec![self.0.clone()]
            } else {
                vec![]
            },
        )
    }
}

#[tokio::test]
async fn exchange_cancel_binds_one_journal_parent_and_waits_for_terminal_order_state() {
    let (parent_prepared, parent_submission) = prepared_parent();
    let parent = record("parent-1", parent_prepared, parent_submission, None);
    let api: Arc<dyn CoinbaseApi> = Arc::new(Api(Mutex::new(VecDeque::from([
        response("open"),
        response("canceled"),
    ]))));
    let profile =
        CoinbaseProfile::new(vec![account()], [("exchange-key".into(), api)].into()).unwrap();
    profile
        .attach_execution_state(Arc::new(State(parent)))
        .unwrap();
    let input = json!({"account":"exchange-main","requestId":"cancel-1","orderId":"venue-order-1","clientOrderId":"client-1","productId":"BTC-USD"});
    let prepared = profile
        .prepare("coinbaseexchange.cancel-order", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["operationId"], "parent-1");
    assert!(
        matches!(prepared.intent.expected.as_slice(),[fin_domain::EffectConstraint::SubmissionCancellation{operation_id,..}] if operation_id=="parent-1")
    );
    let plan = profile.plan_submission(&prepared).await.unwrap();
    assert_eq!(
        plan.request.endpoint,
        CoinbaseRoute::ExchangeCancelOrderScoped {
            order_id: "venue-order-1".into(),
            product_id: "BTC-USD".into(),
            profile_id: "profile-1".into()
        }
    );
    assert!(plan.request.exact_body.is_empty());
    let submission = plan.submission;
    let receipt = serde_json::to_vec("venue-order-1").unwrap();
    let cancel_record = record(
        "cancel-1",
        prepared,
        submission.clone(),
        Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
    );
    let result = profile
        .reconcile_submission(&cancel_record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert!(
        matches!(result.verified.unwrap().effects.as_slice(),[fin_domain::FinancialEffect::SubmissionCancellation{operation_id,..}] if operation_id=="parent-1")
    );
}

#[tokio::test]
async fn exchange_cancel_without_native_dispatch_receipt_never_certifies_completion() {
    let (parent_prepared, parent_submission) = prepared_parent();
    let parent = record("parent-1", parent_prepared, parent_submission, None);
    let api: Arc<dyn CoinbaseApi> = Arc::new(Api(Mutex::new(VecDeque::from([
        response("open"),
        response("canceled"),
    ]))));
    let profile =
        CoinbaseProfile::new(vec![account()], [("exchange-key".into(), api)].into()).unwrap();
    profile
        .attach_execution_state(Arc::new(State(parent)))
        .unwrap();
    let input = json!({"account":"exchange-main","requestId":"cancel-2","orderId":"venue-order-1","clientOrderId":"client-1","productId":"BTC-USD"});
    let prepared = profile
        .prepare("coinbaseexchange.cancel-order", &input)
        .await
        .unwrap();
    let plan = profile.plan_submission(&prepared).await.unwrap();
    let submission = plan.submission;
    let cancel_record = record("cancel-2", prepared, submission.clone(), None);
    let result = profile
        .reconcile_submission(&cancel_record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn bodyless_cancel_no_dispatch_certificate_reconciles_from_store_without_replay() {
    let (parent_prepared, parent_submission) = prepared_parent();
    let parent = record("parent-1", parent_prepared, parent_submission, None);
    let api = Arc::new(GuardApi {
        responses: Mutex::new(VecDeque::from([response("open")])),
        calls: Mutex::new(Vec::new()),
    });
    let profile = CoinbaseProfile::new(
        vec![account()],
        [("exchange-key".into(), api.clone() as Arc<dyn CoinbaseApi>)].into(),
    )
    .unwrap();
    profile
        .attach_execution_state(Arc::new(State(parent)))
        .unwrap();
    let input = json!({"account":"exchange-main","requestId":"cancel-no-send","orderId":"venue-order-1","clientOrderId":"client-1","productId":"BTC-USD"});
    let prepared = profile
        .prepare("coinbaseexchange.cancel-order", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["requestBodyExact"], "");
    let mut plan = profile.plan_submission(&prepared).await.unwrap();
    assert!(plan.request.exact_body.is_empty());
    plan.request.candidate.expires_at = "2000-01-01T00:00:00Z".into();
    let journal = Arc::new(Journal::default());
    assert!(
        profile
            .dispatch(&plan.request, journal.clone())
            .await
            .is_err()
    );
    let certificate = journal.evidence.lock().unwrap().clone().unwrap();
    assert_eq!(
        certificate["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
    {
        let calls = api.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|endpoint| endpoint.mutating()).count(),
            1,
            "one fake transport invocation reached the guard"
        );
    }

    let cancel_record = record(
        "cancel-no-send",
        plan.request.candidate.clone(),
        plan.submission.clone(),
        Some(certificate.clone()),
    );
    let mut store = fin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &cancel_record.id,
            &cancel_record.request_id,
            &cancel_record.fingerprint,
            &cancel_record.operation,
            cancel_record.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&cancel_record).unwrap(),
            &cancel_record.created_at,
        )
        .unwrap();
    let row = fin_store::SubmissionRow {
        kind: plan.submission.kind.clone(),
        reference: plan.submission.reference.clone(),
        data: plan.submission.data.clone(),
    };
    store
        .persist_submission(&cancel_record.id, &row, &fin_domain::now())
        .unwrap();
    store
        .persist_dispatch_evidence(&cancel_record.id, &row, &certificate)
        .unwrap();
    let persisted: OperationRecord =
        serde_json::from_value(store.get_record(&cancel_record.id).unwrap().unwrap()).unwrap();
    profile
        .attach_execution_state(Arc::new(
            fin_store::execution_state::ExecutionStateRepository(store.share()),
        ))
        .unwrap();
    let result = profile
        .reconcile_submission(&persisted, &plan.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.as_ref().unwrap().effects.is_empty());
    assert_eq!(
        result.verified.as_ref().unwrap().submission,
        plan.submission
    );
    assert_eq!(
        api.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|endpoint| endpoint.mutating())
            .count(),
        1,
        "certificate reconciliation never resends cancel"
    );
}

#[tokio::test]
async fn exchange_cancel_uses_native_cancel_receipt_when_unmatched_order_record_is_purged() {
    let (parent_prepared, parent_submission) = prepared_parent();
    let parent = record("parent-1", parent_prepared, parent_submission, None);
    let api: Arc<dyn CoinbaseApi> = Arc::new(Api(Mutex::new(VecDeque::from([
        response("open"),
        CoinbaseResponse::new(404, b"{}".to_vec()),
    ]))));
    let profile =
        CoinbaseProfile::new(vec![account()], [("exchange-key".into(), api)].into()).unwrap();
    profile
        .attach_execution_state(Arc::new(State(parent)))
        .unwrap();
    let input = json!({"account":"exchange-main","requestId":"cancel-purged","orderId":"venue-order-1","clientOrderId":"client-1","productId":"BTC-USD"});
    let prepared = profile
        .prepare("coinbaseexchange.cancel-order", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["filledSizeBefore"], "0");
    let plan = profile.plan_submission(&prepared).await.unwrap();
    let submission = plan.submission;
    let receipt = serde_json::to_vec("venue-order-1").unwrap();
    let cancel_record = record(
        "cancel-purged",
        prepared,
        submission.clone(),
        Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
    );
    let result = profile
        .reconcile_submission(&cancel_record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
}

#[tokio::test]
async fn exchange_fill_race_does_not_certify_a_cancelled_remainder() {
    let (parent_prepared, parent_submission) = prepared_parent();
    let parent = record("parent-1", parent_prepared, parent_submission, None);
    let api: Arc<dyn CoinbaseApi> = Arc::new(Api(Mutex::new(VecDeque::from([
        response("open"),
        response("done"),
    ]))));
    let profile =
        CoinbaseProfile::new(vec![account()], [("exchange-key".into(), api)].into()).unwrap();
    profile
        .attach_execution_state(Arc::new(State(parent)))
        .unwrap();
    let input = json!({"account":"exchange-main","requestId":"cancel-race","orderId":"venue-order-1","clientOrderId":"client-1","productId":"BTC-USD"});
    let prepared = profile
        .prepare("coinbaseexchange.cancel-order", &input)
        .await
        .unwrap();
    let plan = profile.plan_submission(&prepared).await.unwrap();
    let submission = plan.submission;
    let receipt = serde_json::to_vec("venue-order-1").unwrap();
    let cancel_record = record(
        "cancel-race",
        prepared,
        submission.clone(),
        Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
    );
    let result = profile
        .reconcile_submission(&cancel_record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.is_none());
}
