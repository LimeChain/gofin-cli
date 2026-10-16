use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::{
    Adapter, AssetId, GoFinError, OperationRecord, ResourceId, Result, SnapshotScope, Status,
    Submission, SubmissionJournal,
};
use gofin_exchange::{
    ExchangeProfile,
    coinbase::{
        CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse,
        CoinbaseRoute, DispatchGuard,
    },
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

type SeenRequest = (CoinbaseEndpoint, Option<String>, Option<bool>);

struct FixtureApi {
    responses: Mutex<VecDeque<std::result::Result<CoinbaseResponse, CoinbaseError>>>,
    seen: Mutex<Vec<SeenRequest>>,
    permissions: Mutex<Value>,
    permission_reads: AtomicUsize,
    last_order: Mutex<Option<(u16, Vec<u8>, gofin_exchange::coinbase::CoinbasePagination)>>,
}
impl FixtureApi {
    fn new(values: impl IntoIterator<Item = Value>) -> Self {
        Self {
            responses: Mutex::new(
                values
                    .into_iter()
                    .map(|value| {
                        Ok(CoinbaseResponse {
                            status: 200,
                            body: serde_json::to_vec(&value).unwrap(),

                            pagination: Default::default(),
                        })
                    })
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
            permissions: Mutex::new(
                json!({"portfolio_uuid":"00000000-0000-4000-8000-000000000001","can_view":true,"can_trade":true,"can_transfer":true}),
            ),
            permission_reads: AtomicUsize::new(0),
            last_order: Mutex::new(None),
        }
    }
    fn fail_next(&self) {
        self.responses
            .lock()
            .unwrap()
            .push_front(Err(CoinbaseError::Transport));
    }
}
#[async_trait]
impl CoinbaseApi for FixtureApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        if endpoint == CoinbaseRoute::KeyPermissions {
            self.permission_reads.fetch_add(1, Ordering::Relaxed);
            return Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(&self.permissions.lock().unwrap().clone()).unwrap(),

                pagination: Default::default(),
            });
        }
        let allowed = guard.as_ref().map(|check| check());
        self.seen
            .lock()
            .unwrap()
            .push((endpoint.clone(), request.body, allowed));
        if allowed == Some(false) {
            return Err(CoinbaseError::GuardRejected);
        }
        let mut responses = self.responses.lock().unwrap();
        if matches!(endpoint, CoinbaseRoute::Order { .. }) {
            let next_is_order = responses.front().is_some_and(|item| {
                item.as_ref()
                    .ok()
                    .and_then(|response| serde_json::from_slice::<Value>(&response.body).ok())
                    .is_some_and(|value| value.get("order").is_some())
            });
            if next_is_order {
                let response = responses.pop_front().unwrap();
                if let Ok(response) = &response {
                    *self.last_order.lock().unwrap() = Some((
                        response.status,
                        response.body.clone(),
                        response.pagination.clone(),
                    ));
                }
                return response;
            }
            return self
                .last_order
                .lock()
                .unwrap()
                .as_ref()
                .map(|(status, body, pagination)| CoinbaseResponse {
                    status: *status,
                    body: body.clone(),
                    pagination: pagination.clone(),
                })
                .ok_or(CoinbaseError::Transport);
        }
        responses
            .pop_front()
            .unwrap_or(Err(CoinbaseError::Transport))
    }
}
struct FixturePrices;
#[async_trait]
impl UsdPriceSource for FixturePrices {
    async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: gofin_domain::now(),
            observed_at: gofin_domain::now(),
            source: "synthetic isolated test quote".into(),
        })
    }
}

#[derive(Default)]
struct ConfigJournal {
    submission: Mutex<Option<Submission>>,
    evidence: Mutex<Option<Value>>,
}
impl SubmissionJournal for ConfigJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn assert_candidate(&self, _prepared: &gofin_domain::PreparedOperation) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        *self.submission.lock().unwrap() = Some(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, _submission: &Submission, evidence: &Value) -> Result<()> {
        *self.evidence.lock().unwrap() = Some(evidence.clone());
        Ok(())
    }
}
fn profile(api: Arc<FixtureApi>) -> CoinbaseProfile {
    let account = CoinbaseAccount {
        name: "dev-coinbase".into(),
        resource: ResourceId("coinbase:portfolio:00000000-0000-4000-8000-000000000001".into()),
        credential_ref: "synthetic-credential-ref".into(),
        physical_identity: "00000000-0000-4000-8000-000000000001".into(),
        identity_verified: true,
        portfolio_id: Some("00000000-0000-4000-8000-000000000001".into()),
        product: CoinbaseProduct::Advanced,
    };
    let transport: Arc<dyn CoinbaseApi> = api;
    let profile = CoinbaseProfile::new(
        vec![account],
        BTreeMap::from([("synthetic-credential-ref".into(), transport)]),
    )
    .unwrap()
    .with_prices(Arc::new(FixturePrices));
    profile
        .attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    profile
}
fn profile_with_replaced_identity(api: Arc<FixtureApi>) -> CoinbaseProfile {
    let transport: Arc<dyn CoinbaseApi> = api;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "dev-coinbase".into(),
            resource: ResourceId("coinbase:portfolio:replacement".into()),
            credential_ref: "synthetic-credential-ref".into(),
            physical_identity: "replacement-physical-identity".into(),
            identity_verified: true,
            portfolio_id: Some("00000000-0000-4000-8000-000000000001".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("synthetic-credential-ref".into(), transport)]),
    )
    .unwrap();
    profile
        .attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    profile
}
fn order_input() -> Value {
    json!({"account":"dev-coinbase","requestId":"request-1","productId":"BTC-USD","side":"BUY",
        "orderConfiguration":{"market_market_ioc":{"quote_size":"99.50"}},
        "maximumSpend":"100","minimumReceive":"0.01","maximumFee":"1"})
}
fn product() -> Value {
    json!({"product":{"product_type":"SPOT","trading_disabled":false,"base_currency_id":"BTC","quote_currency_id":"USD","base_increment":"0.00000001","quote_increment":"0.01","price_increment":"0.01","base_min_size":"0.00000001","quote_min_size":"0.01"}})
}
fn preview(commission: &str) -> Value {
    json!({"order_total":"99.50","commission_total":commission,"errors":[]})
}

#[test]
fn order_submission_definitions_use_broker_generated_attempt_request_ids() {
    let api = Arc::new(FixtureApi::new(std::iter::empty::<Value>()));
    let profile = profile(api);
    for operation in [
        "coinbase.create-order",
        "coinbaseexchange.create-order",
        "coinbaseinternational.create-order",
        "coinbaseinternational.edit-order",
    ] {
        let definition = profile
            .definitions()
            .into_iter()
            .find(|definition| definition.name == operation)
            .unwrap_or_else(|| panic!("missing {operation}"));
        assert_eq!(
            definition.semantics["broker_generated_attempt_input_fields"],
            json!(["/requestId"]),
            "{operation}"
        );
    }
}

#[tokio::test]
async fn post_reservation_finalization_refreshes_order_and_submission_does_not_repeat_permission_read()
 {
    let api = Arc::new(FixtureApi::new([
        product(),
        preview("0.25"),
        product(),
        preview("0.25"),
    ]));
    let prof = profile(api.clone());
    let approved = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();

    let finalized = prof.finalize_after_reservation(&approved).await.unwrap();
    assert_eq!(finalized.input, approved.input);
    assert_eq!(finalized.binding, approved.binding);
    assert_eq!(finalized.intent, approved.intent);
    assert_eq!(finalized.expires_at, approved.expires_at);
    assert!(finalized.checks.iter().any(|check| {
        check.name == "advanced_trade_permission_finalized"
            && check.status == gofin_domain::CheckStatus::Passed
    }));
    assert_eq!(api.permission_reads.load(Ordering::Relaxed), 2);

    prof.plan_submission(&finalized).await.unwrap();
    assert_eq!(api.permission_reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn order_refresh_keeps_approved_binding_when_native_preview_observation_changes() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let approved = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let approved_binding = approved.binding.clone();

    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&product()).unwrap(),
            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&json!({
                "preview_id":"new-observation-only-id",
                "order_total":"99.60",
                "commission_total":"0.30",
                "errors":[]
            }))
            .unwrap(),
            pagination: Default::default(),
        }),
    ]);
    let refreshed = prof.refresh_prepared(&approved).await.unwrap();

    assert_eq!(refreshed.binding, approved_binding);
    assert_eq!(refreshed.input, approved.input);
    assert!(refreshed.checks.iter().any(|check| {
        check.name == "refreshed_order_preview"
            && check.detail.contains("new-observation-only-id")
            && check.detail.contains("commission 0.30")
    }));
}

#[tokio::test]
async fn order_refresh_rejects_a_fresh_preview_over_the_approved_fee_cap() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let approved = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();

    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&product()).unwrap(),
            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&json!({
                "preview_id":"over-cap-observation",
                "order_total":"99.60",
                "commission_total":"1.01",
                "errors":[]
            }))
            .unwrap(),
            pagination: Default::default(),
        }),
    ]);
    let error = prof.refresh_prepared(&approved).await.unwrap_err();
    assert_eq!(error.code, "COINBASE_FEE_EXCEEDS_BOUND");
}

#[tokio::test]
async fn explicit_native_order_rejection_is_terminal_without_history_replay() {
    let api = Arc::new(FixtureApi::new([
        product(),
        preview("0.25"),
        json!({
            "success":false,
            "error_response":{"error":"INSUFFICIENT_FUND","message":"Insufficient funds"}
        }),
    ]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = prof.plan_submission(&prepared).await.unwrap();
    let (mut record, _) = submitted_record(prepared.clone(), None);
    record.status = Status::Submitted;
    record.submission = None;
    record.dispatch_evidence = None;
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            record.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&record).unwrap(),
            &record.created_at,
        )
        .unwrap();
    store
        .persist_submission(
            &record.id,
            &gofin_store::SubmissionRow {
                kind: planned.submission.kind.clone(),
                reference: planned.submission.reference.clone(),
                data: planned.submission.data.clone(),
            },
            &gofin_domain::now(),
        )
        .unwrap();
    let journal = Arc::new(ConfigJournal::default());
    let response = prof
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();
    assert_eq!(response["status"], 200);
    let evidence = journal.evidence.lock().unwrap().clone().unwrap();
    store
        .persist_dispatch_evidence(
            &record.id,
            &gofin_store::SubmissionRow {
                kind: planned.submission.kind.clone(),
                reference: planned.submission.reference.clone(),
                data: planned.submission.data.clone(),
            },
            &evidence,
        )
        .unwrap();
    let persisted: OperationRecord =
        serde_json::from_value(store.get_record(&record.id).unwrap().unwrap()).unwrap();
    let result = prof
        .reconcile_submission(&persisted, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let proof = result
        .verified
        .as_ref()
        .expect("terminal rejection has typed empty-effects proof");
    assert!(proof.effects.is_empty());
    assert_eq!(proof.submission, planned.submission);
    assert_eq!(
        proof.proof_digest,
        gofin_domain::verified_effect_digest(proof).unwrap()
    );
    assert_eq!(
        result.evidence["nativeRejection"]["error_response"]["error"],
        "INSUFFICIENT_FUND"
    );
    assert_eq!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(endpoint, _, _)| matches!(endpoint, CoinbaseRoute::HistoricalOrders { .. }))
            .count(),
        0,
        "an explicit native rejection resolves the write without an absent-history inference"
    );
}

#[tokio::test]
async fn local_authorization_gate_certificate_proves_no_dispatch() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = prof.plan_submission(&prepared).await.unwrap();
    let journal = Arc::new(FixtureJournal::default());
    let error = prof
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "INVALID_INPUT");
    let certificate = journal.evidence.lock().unwrap()[0].clone();
    assert_eq!(certificate["finNoDispatch"]["reason"], "authorization_gate");
    assert!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _, _)| !endpoint.mutating()),
        "the rejected local gate must not send a Coinbase write"
    );

    let (mut record, _) = submitted_record(prepared.clone(), None);
    record.submission = Some(planned.submission.clone());
    record.dispatch_evidence = Some(certificate);
    let result = prof
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let proof = result.verified.as_ref().expect("no-dispatch proof");
    assert!(proof.effects.is_empty());
    assert_eq!(proof.submission, planned.submission);
    assert_eq!(
        proof.proof_digest,
        gofin_domain::verified_effect_digest(proof).unwrap()
    );
    assert!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _, _)| !matches!(endpoint, CoinbaseRoute::HistoricalOrders { .. })),
        "a local certificate resolves without venue history"
    );
    record.status = Status::Failed;
    record.effects = Some(proof.clone());
    prof.attach_execution_state(Arc::new(StaticExecutionState(record)))
        .unwrap();
    assert!(
        prof.plan_submission(&prepared).await.is_ok(),
        "a terminal failure with validated broker no-dispatch proof may reuse its client ID"
    );
}

fn fill(order_id: &str, entry_id: &str, size: &str) -> Value {
    json!({"order_id":order_id,"entry_id":entry_id,"product_id":"BTC-USD","side":"BUY",
        "price":"100","size":size,"size_in_quote":false,"commission":"0.01"})
}

fn submitted_record(
    prepared: gofin_domain::PreparedOperation,
    order_id: Option<&str>,
) -> (OperationRecord, Submission) {
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let submission = Submission {
        kind: "coinbase".into(),
        reference: client_id.clone(),
        data: json!({"product":"coinbase","clientOrderId":client_id,"account":"dev-coinbase","productId":"BTC-USD"}),
    };
    let body = match order_id {
        Some(id) => json!({"success":true,"success_response":{"order_id":id}}),
        None => json!({"success":false,"error":"timeout"}),
    };
    let bytes = serde_json::to_vec(&body).unwrap();
    let record = OperationRecord {
        id: "operation-1".into(),
        request_id: "request-1".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: "coinbase.create-order".into(),
        input: prepared.input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&bytes),"bodySha256":hex::encode(sha2::Sha256::digest(&bytes))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    (record, submission)
}

fn open_order(client_id: &str, status: &str) -> Value {
    json!({"order":{"order_id":"venue-order","client_order_id":client_id,"product_id":"BTC-USD","status":status,"filled_size":"0.25","filled_value":"25"}})
}

fn authoritative_order(
    client_id: &str,
    status: &str,
    base: &str,
    quote: &str,
    fees: &str,
    quote_sized: bool,
) -> Value {
    json!({"order":{"order_id":"venue-order","client_order_id":client_id,"product_id":"BTC-USD","status":status,"settled":status == "FILLED","size_in_quote":quote_sized,"filled_size":base,"filled_value":quote,"total_fees":fees}})
}

fn cancellation_record(
    submission: Submission,
    prepared: gofin_domain::PreparedOperation,
) -> OperationRecord {
    OperationRecord {
        id: "cancel-operation".into(),
        request_id: "cancel-request".into(),
        fingerprint: "cancel-fingerprint".into(),
        fingerprint_version: 1,
        operation: "coinbase.cancel-order".into(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        input: prepared.input.clone(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

struct StaticExecutionState(OperationRecord);
impl gofin_domain::execution_state::ExecutionState for StaticExecutionState {
    fn allocate_nonce(&self, _: &str, _: u64) -> Result<u64> {
        Ok(1)
    }
    fn operation(&self, id: &str) -> Result<Option<OperationRecord>> {
        Ok((self.0.id == id).then(|| self.0.clone()))
    }
    fn operations_by_prepared_binding(
        &self,
        operation: &str,
        pointer: &str,
        value: &Value,
    ) -> Result<Vec<OperationRecord>> {
        let pointer = pointer.strip_prefix("/preview/binding").unwrap_or(pointer);
        Ok(
            if self.0.operation == operation
                && self.0.preview.as_ref().is_some_and(|p| {
                    p.binding
                        .pointer(pointer)
                        .is_some_and(|bound| bound == value)
                })
            {
                vec![self.0.clone()]
            } else {
                vec![]
            },
        )
    }
}

struct EmptyExecutionState;
impl gofin_domain::execution_state::ExecutionState for EmptyExecutionState {
    fn allocate_nonce(&self, _: &str, _: u64) -> Result<u64> {
        Ok(1)
    }
    fn operation(&self, _: &str) -> Result<Option<OperationRecord>> {
        Ok(None)
    }
    fn operations_by_prepared_binding(
        &self,
        _: &str,
        _: &str,
        _: &Value,
    ) -> Result<Vec<OperationRecord>> {
        Ok(vec![])
    }
}

#[derive(Default)]
struct FixtureJournal {
    authorized: std::sync::atomic::AtomicBool,
    revoke_after_first: std::sync::atomic::AtomicBool,
    authorization_checks: std::sync::atomic::AtomicUsize,
    submissions: Mutex<Vec<Submission>>,
    evidence: Mutex<Vec<Value>>,
}
impl FixtureJournal {
    fn authorized() -> Self {
        Self {
            authorized: std::sync::atomic::AtomicBool::new(true),
            ..Self::default()
        }
    }
}
impl SubmissionJournal for FixtureJournal {
    fn assert_authorized(&self) -> Result<()> {
        let check = self
            .authorization_checks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.authorized.load(std::sync::atomic::Ordering::SeqCst)
            && (!self
                .revoke_after_first
                .load(std::sync::atomic::Ordering::SeqCst)
                || check == 0)
        {
            Ok(())
        } else {
            Err(GoFinError::validation("Synthetic approval is expired."))
        }
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.submissions.lock().unwrap().push(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, _submission: &Submission, evidence: &Value) -> Result<()> {
        self.evidence.lock().unwrap().push(evidence.clone());
        Ok(())
    }
}

#[tokio::test]
async fn preparation_binds_exact_order_body_caps_fee_and_currency_identity() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prepared = profile(api.clone())
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    assert_eq!(prepared.binding["productId"], "BTC-USD");
    assert_eq!(prepared.binding["baseCurrencyId"], "BTC");
    assert_eq!(prepared.binding["quoteCurrencyId"], "USD");
    assert_eq!(
        prepared.intent.debits[0].amount.atoms.as_str(),
        format!("1{}", "0".repeat(38))
    );
    assert_eq!(
        prepared.intent.maximum_costs[0].amount.atoms.as_str(),
        format!("1{}", "0".repeat(36))
    );
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].0.path().unwrap(),
        "/api/v3/brokerage/market/products/BTC-USD"
    );
    assert_eq!(
        calls[1].0.path().unwrap(),
        "/api/v3/brokerage/orders/preview"
    );
    let body = calls[1].1.as_deref().unwrap();
    let sent: Value = serde_json::from_str(body).unwrap();
    assert_eq!(sent["product_id"], "BTC-USD");
    assert_eq!(sent["side"], "BUY");
    assert_eq!(
        sent["order_configuration"]["market_market_ioc"]["quote_size"],
        "99.50"
    );
    assert!(calls.iter().all(|(_, _, guard)| guard.is_none()));
}

#[tokio::test]
async fn sell_intent_debits_base_asset_and_caps_fee_in_quote_asset() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let mut input = order_input();
    input["side"] = json!("SELL");
    input["orderConfiguration"] = json!({"market_market_ioc":{"base_size":"0.50"}});
    input["maximumSpend"] = json!("0.50");
    input["minimumReceive"] = json!("49.00");
    let prepared = profile(api)
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap();
    assert_eq!(prepared.intent.debits[0].amount.asset.reference, "BTC");
    assert_eq!(prepared.intent.debits[0].amount.atoms.to_decimal(36), "0.5");
    assert_eq!(
        prepared.intent.maximum_costs[0].amount.asset.reference,
        "USD"
    );
    assert_eq!(
        prepared.intent.maximum_costs[0].amount.atoms.to_decimal(36),
        "1"
    );
    assert_eq!(
        prepared.intent.maximum_costs[0].treatment,
        gofin_domain::CostTreatment::DeductedFromCredit
    );
    assert!(matches!(
        &prepared.intent.expected[0],
        gofin_domain::EffectConstraint::Conversion { input, output, .. }
            if input.reference == "BTC" && output.reference == "USD"
    ));
}

#[tokio::test]
async fn sell_fill_fee_is_deducted_from_credit_and_net_minimum_is_enforced() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let mut input = order_input();
    input["side"] = json!("SELL");
    input["orderConfiguration"] = json!({"market_market_ioc":{"base_size":"0.50"}});
    input["maximumSpend"] = json!("0.50");
    input["minimumReceive"] = json!("49.90");
    let prepared = prof.prepare("coinbase.create-order", &input).await.unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    let sell_fill = json!({"order_id":"venue-order","entry_id":"sell-fill-1","product_id":"BTC-USD","side":"SELL","price":"100","size":"0.50","size_in_quote":false,"commission":"0.25"});
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&authoritative_order(
                &client_id, "FILLED", "0.5", "50", "0.25", false,
            ))
            .unwrap(),
            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(
                &json!({"fills":[sell_fill],"cursor":"","proof_token_required":false}),
            )
            .unwrap(),
            pagination: Default::default(),
        }),
    ]);

    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let verified = result.verified.unwrap();
    let effects = serde_json::to_value(verified.effects).unwrap();
    assert!(effects.as_array().unwrap().iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["treatment"] == "deducted_from_credit"
            && effect["amount"]["asset"]["reference"] == "USD"
            && effect["amount"]["atoms"] == format!("25{}", "0".repeat(34))
    }));
    assert_eq!(result.evidence["fillCount"], 1);
}

#[tokio::test]
async fn cancellation_rejects_mismatched_identity_and_already_terminal_orders() {
    let input = json!({"account":"dev-coinbase","requestId":"cancel-request","venueOrderId":"venue-order","clientOrderId":"client-order","productId":"BTC-USD"});
    let mismatch = Arc::new(FixtureApi::new([
        json!({"order":{"order_id":"venue-order","client_order_id":"another-client","product_id":"BTC-USD","status":"OPEN"}}),
    ]));
    let error = profile(mismatch.clone())
        .prepare("coinbase.cancel-order", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_ORDER_IDENTITY_MISMATCH");
    assert_eq!(mismatch.seen.lock().unwrap().len(), 1);

    let terminal = Arc::new(FixtureApi::new([open_order("client-order", "FILLED")]));
    let error = profile(terminal.clone())
        .prepare("coinbase.cancel-order", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_ORDER_NOT_OPEN");
    assert_eq!(terminal.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_pending_race_then_terminal_proves_cancellation_of_original() {
    let mut input = json!({"account":"dev-coinbase","requestId":"cancel-request","venueOrderId":"venue-order","clientOrderId":"client-order","productId":"BTC-USD"});
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let original_input = order_input();
    let original_prepared = profile
        .prepare("coinbase.create-order", &original_input)
        .await
        .unwrap();
    let client_id = original_prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    input["clientOrderId"] = json!(client_id);
    let original_submission = profile
        .plan_submission(&original_prepared)
        .await
        .unwrap()
        .submission;
    let original_record = OperationRecord {
        id: "original-operation".into(),
        request_id: "original-request".into(),
        fingerprint: "original-fingerprint".into(),
        fingerprint_version: 1,
        operation: "coinbase.create-order".into(),
        input: original_input,
        status: Status::Pending,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(original_prepared),
        approval: None,
        authorization_source: None,
        submission: Some(original_submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &original_record.id,
            &original_record.request_id,
            &original_record.fingerprint,
            &original_record.operation,
            original_record.run_id.as_deref(),
            "pending",
            &serde_json::to_value(&original_record).unwrap(),
            &original_record.created_at,
        )
        .unwrap();
    profile
        .attach_execution_state(Arc::new(
            gofin_store::execution_state::ExecutionStateRepository(store.share()),
        ))
        .unwrap();
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&open_order(&client_id, "PARTIALLY_FILLED")).unwrap(),

            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: br#"{"results":[{"success":true,"order_id":"venue-order"}]}"#.to_vec(),

            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&open_order(&client_id, "PENDING_CANCEL")).unwrap(),

            pagination: Default::default(),
        }),
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&open_order(&client_id, "CANCELLED")).unwrap(),

            pagination: Default::default(),
        }),
    ]);
    let prepared = profile
        .prepare("coinbase.cancel-order", &input)
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    assert_eq!(planned.request.endpoint.method(), "POST");
    assert_eq!(
        planned.request.exact_body,
        r#"{"order_ids":["venue-order"]}"#
    );
    let journal = Arc::new(FixtureJournal::authorized());
    journal.persist_submission(&planned.submission).unwrap();
    profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();

    let record = cancellation_record(planned.submission.clone(), prepared);
    let pending = profile
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(pending.status, Status::Pending);
    assert!(pending.verified.is_none());
    let completed = profile
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    let verified = completed.verified.unwrap();
    assert_eq!(verified.submission, planned.submission);
    assert!(
        matches!(&verified.effects[0],gofin_domain::FinancialEffect::SubmissionCancellation{operation_id,submission} if operation_id=="original-operation" && submission.reference==client_id)
    );
    assert_eq!(completed.evidence["accountingOwner"], "original_operation");
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[3].0.method(), "POST");
    assert_eq!(calls[3].2, Some(true));
}

#[tokio::test]
async fn preparation_rejects_preview_fee_above_bound_before_authorization() {
    let api = Arc::new(FixtureApi::new([product(), preview("1.01")]));
    let error = profile(api.clone())
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_FEE_EXCEEDS_BOUND");
    assert_eq!(api.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn order_preparation_requires_the_key_bound_trade_permission() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    api.permissions.lock().unwrap()["can_trade"] = json!(false);
    let error = profile(api.clone())
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_KEY_PERMISSION_DENIED");
    assert!(
        api.seen.lock().unwrap().is_empty(),
        "permission denial must precede product/preview requests"
    );
}

#[tokio::test]
async fn product_metadata_transport_failure_stops_before_preview() {
    let api = Arc::new(FixtureApi::new([]));
    api.fail_next();
    let error = profile(api.clone())
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_TRANSPORT_UNCERTAIN");
    assert_eq!(api.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn expired_authority_is_rechecked_by_the_transport_guard_before_dispatch() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let journal = Arc::new(FixtureJournal::authorized());
    journal
        .revoke_after_first
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "INVALID_INPUT");
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].0.method(), "POST");
    assert_eq!(calls[2].2, Some(false));
    assert_eq!(
        journal.evidence.lock().unwrap()[0]["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
}

#[tokio::test]
async fn expired_prepared_candidate_is_rejected_immediately_before_transport_dispatch() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let mut planned = prof.plan_submission(&prepared).await.unwrap();
    planned.request.candidate.expires_at = "2000-01-01T00:00:00Z".into();
    let journal = Arc::new(FixtureJournal::authorized());
    let error = prof
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_DISPATCH_GUARD_REJECTED");
    {
        let calls = api.seen.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].2, Some(false));
    }
    let certificate = journal.evidence.lock().unwrap()[0].clone();
    assert_eq!(
        certificate["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
    assert_eq!(certificate["finNoDispatch"]["version"], 1);

    // The dispatch identity was persisted before the transport guard ran. The
    // sealed certificate must therefore resolve the submission locally and
    // make its client order ID reusable only after validation through Store.
    let (mut record, submission) = submitted_record(prepared.clone(), None);
    record.submission = Some(planned.submission.clone());
    record.dispatch_evidence = Some(certificate);
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            record.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&record).unwrap(),
            &record.created_at,
        )
        .unwrap();
    store
        .persist_submission(
            &record.id,
            &gofin_store::SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            &gofin_domain::now(),
        )
        .unwrap();
    store
        .persist_dispatch_evidence(
            &record.id,
            &gofin_store::SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            record.dispatch_evidence.as_ref().unwrap(),
        )
        .unwrap();
    let persisted: OperationRecord =
        serde_json::from_value(store.get_record(&record.id).unwrap().unwrap()).unwrap();
    assert_eq!(persisted.dispatch_evidence, record.dispatch_evidence);
    prof.attach_execution_state(Arc::new(
        gofin_store::execution_state::ExecutionStateRepository(store.share()),
    ))
    .unwrap();
    let result = prof
        .reconcile_submission(&persisted, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.as_ref().unwrap().effects.is_empty());
    assert!(
        api.seen.lock().unwrap().iter().all(|(endpoint, _, _)| {
            !matches!(endpoint, CoinbaseRoute::HistoricalOrders { .. })
        })
    );
    record.status = Status::Failed;
    record.effects = result.verified;
    prof.attach_execution_state(Arc::new(StaticExecutionState(record)))
        .unwrap();
    assert!(prof.plan_submission(&prepared).await.is_ok());
}

#[tokio::test]
async fn bodyless_deposit_address_no_dispatch_certificate_reconciles_from_store() {
    let account_id = "currency-account-1";
    let account_state = json!({
        "account": {"uuid":account_id,"portfolio_uuid":"00000000-0000-4000-8000-000000000001",
            "ready":true,"active":true,"currency":{"code":"BTC"}}
    });
    let api = Arc::new(FixtureApi::new([
        account_state.clone(),
        account_state.clone(),
        account_state,
    ]));
    let prof = profile(api.clone());
    let input = json!({"account":"dev-coinbase","accountId":account_id,"requestId":"deposit-address-guard"});
    let prepared = prof
        .prepare("coinbase.create-deposit-address", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["requestBodyExact"], "");
    let mut planned = prof.plan_submission(&prepared).await.unwrap();
    assert!(planned.request.exact_body.is_empty());
    planned.request.candidate.expires_at = "2000-01-01T00:00:00Z".into();
    let journal = Arc::new(FixtureJournal::authorized());
    let error = prof
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_DISPATCH_GUARD_REJECTED");
    let certificate = journal.evidence.lock().unwrap()[0].clone();
    assert_eq!(
        certificate["finNoDispatch"]["reason"],
        "transport_guard_rejected"
    );
    assert!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _, allowed)| {
                !matches!(endpoint, CoinbaseRoute::DepositAddressCreate { .. })
                    || *allowed == Some(false)
            }),
        "the native create route is rejected by the guard before HTTP emission"
    );

    let mut record = OperationRecord {
        id: "deposit-address-op".into(),
        request_id: input["requestId"].as_str().unwrap().into(),
        fingerprint: "deposit-address-fingerprint".into(),
        fingerprint_version: 1,
        operation: planned.request.candidate.operation.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        input: input.clone(),
        preview: Some(planned.request.candidate.clone()),
        approval: None,
        authorization_source: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(certificate.clone()),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            record.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&record).unwrap(),
            &record.created_at,
        )
        .unwrap();
    let row = gofin_store::SubmissionRow {
        kind: planned.submission.kind.clone(),
        reference: planned.submission.reference.clone(),
        data: planned.submission.data.clone(),
    };
    store
        .persist_submission(&record.id, &row, &gofin_domain::now())
        .unwrap();
    store
        .persist_dispatch_evidence(&record.id, &row, &certificate)
        .unwrap();
    let persisted: OperationRecord =
        serde_json::from_value(store.get_record(&record.id).unwrap().unwrap()).unwrap();
    prof.attach_execution_state(Arc::new(
        gofin_store::execution_state::ExecutionStateRepository(store.share()),
    ))
    .unwrap();
    let result = prof
        .reconcile_submission(&persisted, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.as_ref().unwrap().effects.is_empty());
    assert_eq!(
        result.verified.as_ref().unwrap().submission,
        planned.submission
    );
    assert!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _, allowed)| {
                !matches!(endpoint, CoinbaseRoute::DepositAddressCreate { .. })
                    || *allowed == Some(false)
            }),
        "the validated proof resolves without sending or replaying the address creation"
    );
    record.status = Status::Failed;
    record.effects = result.verified;
    prof.attach_execution_state(Arc::new(StaticExecutionState(record)))
        .unwrap();
    assert!(
        prof.plan_submission(&prepared).await.is_ok(),
        "only the validated empty-effects proof releases the request identity"
    );
}

#[tokio::test]
async fn accepted_submission_persists_the_original_client_id_and_exact_response_bytes() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let journal = Arc::new(FixtureJournal::authorized());
    journal.persist_submission(&planned.submission).unwrap();
    let response_bytes = br#"{"success":true,"order_id":"venue-order"}"#;
    api.responses
        .lock()
        .unwrap()
        .push_back(Ok(CoinbaseResponse {
            status: 200,
            body: response_bytes.to_vec(),

            pagination: Default::default(),
        }));
    let result = profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();
    profile
        .validate_response(&planned.submission, &result)
        .unwrap();
    assert_eq!(
        journal.submissions.lock().unwrap()[0].reference,
        prepared.binding["clientOrderId"]
    );
    let evidence = journal.evidence.lock().unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["httpStatus"], 200);
    let captured = hex::decode(evidence[0]["bodyHex"].as_str().unwrap()).unwrap();
    assert_eq!(captured, response_bytes);
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls[2].0, planned.request.endpoint);
    assert_eq!(calls[2].2, Some(true));
}

#[tokio::test]
async fn dispatch_transport_error_has_safe_distinct_code_after_submission_identity_is_planned() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = prof.plan_submission(&prepared).await.unwrap();
    api.fail_next();

    let error = prof
        .dispatch(&planned.request, Arc::new(ConfigJournal::default()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_TRANSPORT_UNCERTAIN");
    assert!(error.message.contains("Reconcile"));
    assert!(error.details.is_none());
}

#[tokio::test]
async fn lost_order_reconciliation_reports_safe_request_error_code_without_replay() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let (record, submission) = submitted_record(prepared, None);
    api.fail_next();

    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(
        result.evidence["requestErrorCode"],
        "COINBASE_TRANSPORT_UNCERTAIN"
    );
    assert!(
        result.evidence["reason"]
            .as_str()
            .unwrap()
            .contains("will not be replayed")
    );
}

#[tokio::test]
async fn lost_dispatch_response_keeps_the_durable_client_id_and_never_replays_automatically() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let journal = Arc::new(FixtureJournal::authorized());

    // The journal write is the durable dispatch identity and must precede the
    // first network attempt. A timeout is ambiguous, so the caller cannot get
    // a replacement client ID or silently retry the POST.
    journal.persist_submission(&planned.submission).unwrap();
    let error = profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap_err();

    assert_eq!(error.code, "COINBASE_TRANSPORT_UNCERTAIN");
    assert!(error.message.contains("Reconcile"));
    assert_eq!(journal.submissions.lock().unwrap().len(), 1);
    assert_eq!(
        journal.submissions.lock().unwrap()[0].reference,
        prepared.binding["clientOrderId"]
    );
    assert!(journal.evidence.lock().unwrap().is_empty());
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].0.method(), "POST");
    assert_eq!(calls[2].2, Some(true));
}

#[tokio::test]
async fn advanced_snapshot_uses_bound_portfolio_and_keeps_currency_account_id_as_coverage() {
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD","max_precision":2}]}),
        json!({"data":[{"id":"USDC","max_precision":6}]}),
        json!({"accounts":[{"uuid":"currency-account-uuid","currency":"USDC","active":true,"ready":true,
            "available_balance":{"value":"12.34","currency":"USDC"},
            "hold":{"value":"1.66","currency":"USDC"}}],"has_next":false}),
        json!({"orders":[{"order_id":"open-order-1"}],"has_next":false}),
    ]));
    let snapshot = profile(api.clone()).snapshot().await.unwrap();
    assert!(
        !snapshot.complete,
        "an unscoped Advanced snapshot must not claim derivatives, loans or collateral were observed"
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|item| item.contains("not observed"))
    );
    {
        let calls = api.seen.lock().unwrap();
        assert_eq!(calls[0].0, CoinbaseRoute::LegacyCurrencies);
        assert_eq!(calls[1].0, CoinbaseRoute::LegacyCryptoCurrencies);
        assert!(matches!(
            &calls[2].0,
            CoinbaseRoute::Accounts { query }
                if query.portfolio_id.as_deref() == Some("00000000-0000-4000-8000-000000000001")
        ));
    }
    assert_eq!(snapshot.state.holdings.len(), 1);
    let holding = &snapshot.state.holdings[0];
    assert_eq!(
        holding.resource.0,
        "coinbase:portfolio:00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(holding.amount.asset.network, "coinbase");
    assert_eq!(holding.amount.asset.reference, "USDC");
    assert_eq!(holding.amount.asset.decimals, 36);
    assert_eq!(
        holding.amount.atoms.as_str(),
        "12340000000000000000000000000000000000"
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|item| item == "coinbase:currency-account:currency-account-uuid")
    );
    let scoped = profile(Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD","max_precision":2}]}),
        json!({"data":[{"id":"USDC","max_precision":6}]}),
        json!({"accounts":[{"uuid":"currency-account-uuid","currency":"USDC","active":true,"ready":true,
            "available_balance":{"value":"12.34","currency":"USDC"},
            "hold":{"value":"1.66","currency":"USDC"}}],"has_next":false}),
        json!({"orders":[{"order_id":"open-order-1"}],"has_next":false}),
    ])))
    .snapshot_scoped(&SnapshotScope {
        resources: vec![],
        networks: vec!["coinbase".into()],
        assets: vec![],
    })
    .await
    .unwrap();
    assert!(
        scoped.complete,
        "the requested Coinbase cash scope is fully observed"
    );
    assert_eq!(
        snapshot.state.in_flight,
        vec!["coinbase:order:open-order-1"]
    );
}

#[tokio::test]
async fn scoped_known_currency_without_account_is_proved_zero_on_coinbase_network() {
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD","max_precision":2}]}),
        json!({"data":[{"id":"USDC","max_precision":6}]}),
        json!({"accounts":[],"has_next":false}),
        json!({"orders":[],"has_next":false}),
    ]));
    let scope = SnapshotScope {
        resources: vec![ResourceId(
            "coinbase:portfolio:00000000-0000-4000-8000-000000000001".into(),
        )],
        networks: vec!["coinbase".into()],
        assets: vec![AssetId {
            network: "coinbase".into(),
            reference: "USDC".into(),
            decimals: 36,
        }],
    };
    let snapshot = profile(api.clone()).snapshot_scoped(&scope).await.unwrap();
    assert!(snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(snapshot.state.holdings[0].amount.asset.reference, "USDC");
    assert!(snapshot.state.holdings[0].amount.atoms.is_zero());
    assert!(snapshot.coverage.iter().any(|line| {
        line == "coinbase:currency-account-absent:USDC:zero-available-after-complete-enumeration"
    }));
    assert!(
        snapshot
            .coverage
            .contains(&"coinbase:portfolio:00000000-0000-4000-8000-000000000001".to_owned())
    );
    assert_eq!(
        snapshot
            .state
            .holdings
            .iter()
            .filter(|h| h.amount.asset.network == "coinbaseinternational")
            .count(),
        0
    );
}

#[tokio::test]
async fn scoped_cash_snapshot_contains_zero_euro_and_absent_usdc_with_bound_resource() {
    let resource = ResourceId("coinbase:portfolio:00000000-0000-4000-8000-000000000001".into());
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD"},{"id":"EUR"}]}),
        json!({"data":[{"id":"BTC"},{"id":"USDC"}]}),
        json!({"accounts":[
            {"uuid":"btc-account","currency":"BTC","active":true,"ready":true,"available_balance":{"value":"0","currency":"BTC"}},
            {"uuid":"eur-account","currency":"EUR","active":true,"ready":true,"available_balance":{"value":"2","currency":"EUR"}}
        ],"has_next":false}),
        json!({"orders":[],"has_next":false}),
    ]));
    let scope = SnapshotScope {
        resources: vec![resource.clone()],
        networks: vec!["coinbase".into()],
        assets: ["BTC", "EUR", "USDC"]
            .into_iter()
            .map(|reference| AssetId {
                network: "coinbase".into(),
                reference: reference.into(),
                decimals: 36,
            })
            .collect(),
    };
    let snapshot = profile(api).snapshot_scoped(&scope).await.unwrap();
    assert!(snapshot.complete);
    assert!(snapshot.coverage.contains(&resource.0));
    for (symbol, amount) in [("BTC", "0"), ("EUR", "2"), ("USDC", "0")] {
        let holding = snapshot
            .state
            .holdings
            .iter()
            .find(|row| row.amount.asset.reference == symbol)
            .unwrap();
        assert_eq!(holding.resource, resource);
        assert_eq!(holding.amount.atoms.to_decimal(36), amount);
    }
}

#[tokio::test]
async fn snapshot_pagination_requires_explicit_terminal_boolean() {
    let account_page_missing_flag = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD"}]}),
        json!({"data":[{"id":"USDC"}]}),
        json!({"accounts":[]}),
    ]));
    let error = profile(account_page_missing_flag)
        .snapshot()
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PAGINATION_INVALID");

    let order_page_missing_flag = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD"}]}),
        json!({"data":[{"id":"USDC"}]}),
        json!({"accounts":[],"has_next":false}),
        json!({"orders":[]}),
    ]));
    let error = profile(order_page_missing_flag)
        .snapshot()
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PAGINATION_INVALID");
}

#[tokio::test]
async fn advanced_snapshot_uses_available_balance_without_fabricating_hold() {
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD","max_precision":2}]}),
        json!({"data":[{"id":"USDC","max_precision":6}]}),
        json!({"accounts":[{"uuid":"currency-account-uuid","currency":"USDC","active":true,"ready":true,
            "available_balance":{"value":"12.34","currency":"USDC"}}],"has_next":false}),
        json!({"orders":[],"has_next":false}),
    ]));
    let snapshot = profile(api.clone()).snapshot().await.unwrap();
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(
        snapshot.state.holdings[0].amount.atoms.to_decimal(36),
        "12.34"
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|item| item.contains("available_balance"))
    );
    assert_eq!(api.seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn advanced_snapshot_rejects_an_account_row_from_a_different_portfolio() {
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD"}]}),
        json!({"data":[{"id":"USDC"}]}),
        json!({"accounts":[{"uuid":"currency-account-uuid","currency":"USDC","portfolio_id":"other-portfolio","active":true,"ready":true,
            "total_balance":{"value":"12.34","currency":"USDC"}}],"has_next":false}),
    ]));
    let error = profile(api.clone()).snapshot().await.unwrap_err();
    assert_eq!(error.code, "COINBASE_PORTFOLIO_MISMATCH");
    assert_eq!(api.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn held_coinbase_funds_are_not_reported_as_spendable_without_open_orders() {
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":"USD"}]}),
        json!({"data":[{"id":"USDC"}]}),
        json!({"accounts":[{"uuid":"currency-account-uuid","currency":"USDC","active":true,"ready":true,
            "available_balance":{"value":"0","currency":"USDC"},
            "hold":{"value":"5.00","currency":"USDC"},
            "total_balance":{"value":"5.00","currency":"USDC"}}],"has_next":false}),
        json!({"orders":[],"has_next":false}),
    ]));
    let snapshot = profile(api).snapshot().await.unwrap();
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert!(snapshot.state.holdings[0].amount.atoms.is_zero());
    assert!(snapshot.state.in_flight.is_empty());
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|item| item.to_ascii_lowercase().contains("available"))
    );
}

#[tokio::test]
async fn completed_order_reconciles_exact_fill_and_fee_effects() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    // The venue detail must attest the same durable client identity.
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "0.5", "50", "0.01", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.5")],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);

    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let verified = result.verified.unwrap();
    assert_eq!(verified.submission, submission);
    assert_eq!(result.evidence["fillCount"], 1);
    assert_eq!(
        result.evidence["proofEvents"][0]["eventId"],
        "coinbase:fill:fill-1"
    );
    assert_eq!(verified.evidence["reconciledOrderId"], "venue-order");
    assert_eq!(result.evidence["settlement_progress"]["sequence"], 1);
    assert!(!verified.effects.is_empty());
    let encoded_effects = serde_json::to_value(&verified.effects).unwrap();
    assert!(encoded_effects.as_array().unwrap().iter().any(|effect| {
        effect["kind"] == "conversion"
            && effect["spent"]["asset"]["reference"] == "USD"
            && effect["spent"]["atoms"] == format!("5{}", "0".repeat(37))
            && effect["received"]["asset"]["reference"] == "BTC"
            && effect["received"]["atoms"] == format!("5{}", "0".repeat(35))
    }));
    assert!(encoded_effects.as_array().unwrap().iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["amount"]["asset"]["reference"] == "USD"
            && effect["amount"]["atoms"] == format!("1{}", "0".repeat(34))
    }));
    assert_eq!(api.seen.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn reconciliation_fails_closed_when_realized_spend_or_fee_exceeds_approval() {
    for (size, commission, expected_reason) in [("1.0001", "0.01", "spend"), ("0.5", "1.01", "fee")]
    {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        *api.responses.lock().unwrap() = VecDeque::from([
            Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", size, if size == "1.0001" { "100.01" } else { "50" }, commission, false)).unwrap(),
            pagination: Default::default(),}),
            Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[{"order_id":"venue-order","entry_id":"over-cap-fill","product_id":"BTC-USD","side":"BUY","price":"100","size":size,"size_in_quote":false,"commission":commission}],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
        ]);
        let result = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(
            result.status,
            Status::Failed,
            "{expected_reason}: {result:?}"
        );
        assert_eq!(
            result.evidence["withinAuthorizedCosts"], false,
            "{expected_reason}"
        );
        assert!(
            result.verified.is_some(),
            "actual execution effects must remain auditable"
        );
    }
}

#[tokio::test]
async fn reconciliation_uses_native_quote_sized_fill_flag_and_authoritative_order_totals() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "5", "10", "0.01", true)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[{"order_id":"venue-order","entry_id":"quote-sized-fill","product_id":"BTC-USD","side":"BUY","price":"2","size":"10","size_in_quote":true,"commission":"0.01"}],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);
    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let effects = serde_json::to_value(result.verified.unwrap().effects).unwrap();
    assert!(effects.as_array().unwrap().iter().any(|effect| {
        effect["kind"] == "conversion"
            && effect["spent"]["asset"]["reference"] == "USD"
            && effect["spent"]["atoms"] == format!("10{}", "0".repeat(36))
            && effect["received"]["asset"]["reference"] == "BTC"
            && effect["received"]["atoms"] == format!("5{}", "0".repeat(36))
    }));
}

#[tokio::test]
async fn terminal_reconciliation_requires_complete_authoritative_aggregates_and_stable_order_snapshot()
 {
    for (label, order, fill_row) in [
        (
            "missing fees",
            json!({"order":{"order_id":"venue-order","client_order_id":"CID","product_id":"BTC-USD","status":"FILLED","settled":true,"size_in_quote":false,"filled_size":"0.5","filled_value":"50"}}),
            fill("venue-order", "fill-1", "0.5"),
        ),
        (
            "short fill page",
            json!({"order":{"order_id":"venue-order","client_order_id":"CID","product_id":"BTC-USD","status":"FILLED","settled":true,"size_in_quote":false,"filled_size":"0.5","filled_value":"50","total_fees":"0.02"}}),
            fill("venue-order", "fill-1", "0.25"),
        ),
        (
            "contradictory order totals",
            json!({"order":{"order_id":"venue-order","client_order_id":"CID","product_id":"BTC-USD","status":"FILLED","settled":true,"size_in_quote":false,"filled_size":"0.5","filled_value":"49","total_fees":"0.01"}}),
            fill("venue-order", "fill-1", "0.5"),
        ),
    ] {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        let mut order = order;
        order["order"]["client_order_id"] = json!(client_id);
        *api.responses.lock().unwrap() = VecDeque::from([
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(&order).unwrap(),
                pagination: Default::default(),
            }),
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(
                    &json!({"fills":[fill_row],"cursor":"","proof_token_required":false}),
                )
                .unwrap(),
                pagination: Default::default(),
            }),
        ]);
        let result = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown, "{label}: {result:?}");
        assert!(result.verified.is_none(), "{label}");
    }

    // An order can change between the first detail read and the complete fill
    // walk. Even if its totals still match, that race cannot prove settlement.
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "0.5", "50", "0.01", false)).unwrap(), pagination: Default::default() }),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.5")],"cursor":"","proof_token_required":false})).unwrap(), pagination: Default::default() }),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "CANCELLED", "0.5", "50", "0.01", false)).unwrap(), pagination: Default::default() }),
    ]);
    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn filled_order_requires_native_settlement_before_credit_is_complete() {
    for (settled, expected) in [(Some(false), Status::Pending), (None, Status::Unknown)] {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        let mut order = authoritative_order(&client_id, "FILLED", "0.5", "50", "0.01", false);
        if let Some(settled) = settled {
            order["order"]["settled"] = json!(settled);
        } else {
            order["order"].as_object_mut().unwrap().remove("settled");
        }
        *api.responses.lock().unwrap() = VecDeque::from([
            Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&order).unwrap(), pagination: Default::default() }),
            Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.5")],"cursor":"","proof_token_required":false})).unwrap(), pagination: Default::default() }),
        ]);
        let result = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, expected);
        if expected == Status::Unknown {
            assert!(result.verified.is_none());
        }
    }
}

#[tokio::test]
async fn fills_requiring_eu_sca_proof_remain_unknown_without_using_the_rows() {
    for proof_token_required in [json!(true), json!("true")] {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        *api.responses.lock().unwrap() = VecDeque::from([
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(&authoritative_order(
                    &client_id, "FILLED", "0.5", "50", "0.01", false,
                ))
                .unwrap(),
                pagination: Default::default(),
            }),
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(&json!({
                    "fills":[fill("venue-order","fill-1","0.5")],
                    "cursor":"",
                    "proof_token_required":proof_token_required
                }))
                .unwrap(),
                pagination: Default::default(),
            }),
        ]);
        let result = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
        assert_eq!(
            api.seen.lock().unwrap().len(),
            4,
            "must stop at the fill page"
        );
    }
}

#[tokio::test]
async fn reconciliation_keeps_fill_unknown_when_native_size_in_quote_flag_is_missing() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "0.5", "50", "0.01", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[{"order_id":"venue-order","entry_id":"ambiguous-size-fill","product_id":"BTC-USD","side":"BUY","price":"2","size":"10","commission":"0.01"}],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);
    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn sell_commission_above_gross_proceeds_is_an_additional_debit_and_failed() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let mut input = order_input();
    input["side"] = json!("SELL");
    input["orderConfiguration"] = json!({"market_market_ioc":{"base_size":"0.50"}});
    input["maximumSpend"] = json!("0.50");
    input["minimumReceive"] = json!("1");
    input["maximumFee"] = json!("60");
    let prepared = prof.prepare("coinbase.create-order", &input).await.unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, Some("venue-order"));
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "0.5", "50", "51", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[{"order_id":"venue-order","entry_id":"fee-exceeds-proceeds","product_id":"BTC-USD","side":"SELL","price":"100","size":"0.50","size_in_quote":false,"commission":"51"}],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);
    let result = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let effects = serde_json::to_value(result.verified.unwrap().effects).unwrap();
    assert!(effects.as_array().unwrap().iter().any(|effect| {
        effect["kind"] == "cost"
            && effect["treatment"] == "additional_debit"
            && effect["amount"]["asset"]["reference"] == "USD"
            && effect["amount"]["atoms"] == format!("51{}", "0".repeat(36))
    }));
    assert_eq!(result.evidence["meetsMinimum"], false);
}

#[tokio::test]
async fn reconciliation_rejects_authoritative_order_id_or_product_mismatch_before_fills() {
    for (returned_id, returned_product) in
        [("another-order", "BTC-USD"), ("venue-order", "ETH-USD")]
    {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        *api.responses.lock().unwrap() = VecDeque::from([Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&json!({"order":{"order_id":returned_id,"client_order_id":client_id,"product_id":returned_product,"status":"FILLED","size_in_quote":false}})).unwrap(),

            pagination: Default::default(),})]);

        let result = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(result.status, Status::Unknown);
        assert!(result.verified.is_none());
        assert_eq!(
            api.seen.lock().unwrap().len(),
            3,
            "must stop before fill query"
        );
    }
}

#[tokio::test]
async fn reconciliation_does_not_accept_fills_with_missing_product_or_side_identity() {
    for (field, replacement) in [
        ("product_id", None),
        ("side", None),
        ("product_id", Some("ETH-USD")),
        ("side", Some("SELL")),
    ] {
        let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
        let prof = profile(api.clone());
        let prepared = prof
            .prepare("coinbase.create-order", &order_input())
            .await
            .unwrap();
        let client_id = prepared.binding["clientOrderId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (record, submission) = submitted_record(prepared, Some("venue-order"));
        let mut row = json!({"order_id":"venue-order","entry_id":"fill-1","product_id":"BTC-USD","side":"BUY","price":"100","size":"0.5","size_in_quote":false,"commission":"0.01"});
        if let Some(replacement) = replacement {
            row[field] = json!(replacement);
        } else {
            row.as_object_mut().unwrap().remove(field);
        }
        *api.responses.lock().unwrap() = VecDeque::from([
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(&authoritative_order(
                    &client_id, "FILLED", "0.5", "50", "0.01", false,
                ))
                .unwrap(),
                pagination: Default::default(),
            }),
            Ok(CoinbaseResponse {
                status: 200,
                body: serde_json::to_vec(
                    &json!({"fills":[row],"cursor":"","proof_token_required":false}),
                )
                .unwrap(),
                pagination: Default::default(),
            }),
        ]);
        let error = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "COINBASE_FILL_IDENTITY_INVALID",
            "invalid {field}"
        );
        assert_eq!(
            api.seen.lock().unwrap().len(),
            4,
            "must stop after fill page"
        );
    }
}

#[tokio::test]
async fn partial_order_reconciliation_paginates_and_rejects_changed_fill_identity() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api.clone());
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (mut record, submission) = submitted_record(prepared, Some("venue-order"));
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "PARTIALLY_FILLED", "0.5", "50", "0.02", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.25")],"cursor":"next-page","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-2","0.25")],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);
    let partial = prof
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(partial.status, Status::Pending);
    let verified = partial.verified.unwrap();
    assert_eq!(partial.evidence["fillCount"], 2);
    assert_eq!(partial.evidence["settlement_progress"]["sequence"], 1);
    assert!(!verified.effects.is_empty());
    {
        let calls = api.seen.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(
            calls[2]
                .0
                .path()
                .unwrap()
                .contains("orders/historical/venue-order")
        );
        let fills_path = calls[3].0.path().unwrap();
        assert!(fills_path.contains("order_ids=venue-order"));
        assert!(!fills_path.contains("product_ids="));
        assert!(calls[4].0.path().unwrap().contains("cursor=next-page"));
    }

    // Later observations may add fills, but immutable evidence for an already
    // consumed fill ID cannot change across reconciliation attempts.
    record.effects = Some(verified);
    let conflict_api = Arc::new(FixtureApi::new([]));
    let conflict_profile = profile(conflict_api.clone());
    // Reuse the stable prepared identity embedded in the durable record.
    *conflict_api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&submission.reference, "PARTIALLY_FILLED", "0.26", "26", "0.01", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.26")],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);
    let error = conflict_profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_FILL_CONFLICT");
}

#[tokio::test]
async fn missing_stable_venue_order_id_stays_unknown_without_replay_or_reads() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let (record, submission) = submitted_record(prepared, None);
    api.responses
        .lock()
        .unwrap()
        .push_back(Ok(CoinbaseResponse {
            status: 200,
            body: br#"{"orders":[],"has_next":false}"#.to_vec(),

            pagination: Default::default(),
        }));
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert!(
        result.evidence["reason"]
            .as_str()
            .unwrap()
            .contains("non-execution is not proven")
    );
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls[2].0.path().unwrap().contains("product_ids=BTC-USD"));
}

#[tokio::test]
async fn lost_order_id_recovers_only_by_paginated_exact_client_identity() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let client_id = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (record, submission) = submitted_record(prepared, None);
    *api.responses.lock().unwrap() = VecDeque::from([
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"orders":[{"order_id":"other-order","client_order_id":"different-client"}],"has_next":true,"cursor":"history-next"})).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"orders":[{"order_id":"venue-order","client_order_id":client_id,"product_id":"BTC-USD"}],"has_next":false})).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&authoritative_order(&client_id, "FILLED", "0.5", "50", "0.01", false)).unwrap(),
            pagination: Default::default(),}),
        Ok(CoinbaseResponse { status: 200, body: serde_json::to_vec(&json!({"fills":[fill("venue-order","fill-1","0.5")],"cursor":"","proof_token_required":false})).unwrap(),
            pagination: Default::default(),}),
    ]);

    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.evidence["reconciledOrderId"], "venue-order");
    assert_eq!(result.evidence["fillCount"], 1);
    let calls = api.seen.lock().unwrap();
    assert_eq!(calls.len(), 7);
    assert!(calls[2].0.path().unwrap().contains("product_ids=BTC-USD"));
    assert!(calls[3].0.path().unwrap().contains("cursor=history-next"));
    assert!(
        calls[4]
            .0
            .path()
            .unwrap()
            .contains("orders/historical/venue-order")
    );
}

#[tokio::test]
async fn portfolio_create_uses_separate_configuration_authority_and_exact_native_receipt() {
    use gofin_exchange::ExchangeAdapter;
    let api = Arc::new(FixtureApi::new([
        json!({"portfolio":{"uuid":"new-portfolio-uuid","name":"Treasury","type":"CONSUMER","deleted":false}}),
    ]));
    let adapter = ExchangeAdapter::new(profile(api.clone()));
    let candidate = adapter
        .prepare_configuration(
            "coinbase.create-portfolio",
            &json!({
                "account":"dev-coinbase","requestId":"create-treasury-1","name":"Treasury"
            }),
        )
        .await
        .unwrap();
    assert_eq!(candidate.operation, "coinbase.create-portfolio");
    assert!(candidate.intent.debits.is_empty());
    let journal = Arc::new(ConfigJournal::default());
    let submission = adapter
        .submit_configuration(&candidate, journal.clone())
        .await
        .unwrap();
    assert_eq!(submission.kind, "coinbase");
    assert_eq!(
        journal.evidence.lock().unwrap().as_ref().unwrap()["bodySha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    let record = OperationRecord {
        id: "config-op-1".into(),
        request_id: "create-treasury-1".into(),
        fingerprint: "config-fingerprint".into(),
        fingerprint_version: 1,
        operation: candidate.operation.clone(),
        input: candidate.input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(candidate.clone()),
        approval: None,
        authorization_source: None,
        submission: Some(submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = adapter.reconcile_configuration(&record).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(
        result.evidence["nativeReceipt"]["portfolio"]["uuid"],
        "new-portfolio-uuid"
    );
    assert!(
        api.seen
            .lock()
            .unwrap()
            .iter()
            .any(|(endpoint, body, guard)| {
                *endpoint == CoinbaseRoute::PortfolioCreate
                    && body.as_deref() == Some(r#"{"name":"Treasury"}"#)
                    && *guard == Some(true)
            })
    );
}

#[tokio::test]
async fn portfolio_rename_revalidates_the_bound_identity_and_native_resulting_name() {
    use gofin_exchange::ExchangeAdapter;
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let listed = json!({"portfolios":[{"uuid":portfolio_id,"name":"Trading","type":"CONSUMER","deleted":false}]});
    let api = Arc::new(FixtureApi::new([
        listed.clone(),
        listed,
        json!({"portfolio":{"uuid":portfolio_id,"name":"Trading","type":"CONSUMER","deleted":false}}),
        json!({"portfolio":{"uuid":portfolio_id,"name":"Savings","type":"CONSUMER","deleted":false}}),
    ]));
    let adapter = ExchangeAdapter::new(profile(api.clone()));
    let candidate = adapter
        .prepare_configuration(
            "coinbase.rename-portfolio",
            &json!({
                "account":"dev-coinbase","requestId":"rename-savings-1","newName":"Savings"
            }),
        )
        .await
        .unwrap();
    let journal = Arc::new(ConfigJournal::default());
    let submission = adapter
        .submit_configuration(&candidate, journal.clone())
        .await
        .unwrap();
    let record = OperationRecord {
        id: "rename-op-1".into(),
        request_id: "rename-savings-1".into(),
        fingerprint: "rename-fingerprint".into(),
        fingerprint_version: 1,
        operation: candidate.operation.clone(),
        input: candidate.input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(candidate),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = adapter.reconcile_configuration(&record).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    let seen = api.seen.lock().unwrap();
    assert!(seen.iter().any(|(endpoint, body, guard)| {
        matches!(endpoint, CoinbaseRoute::PortfolioUpdate { portfolio_id: id } if id == portfolio_id)
            && body.as_deref() == Some(r#"{"name":"Savings"}"#)
            && *guard == Some(true)
    }));
}

#[tokio::test]
async fn cfm_margin_setting_requires_configuration_contract_and_exact_native_readback() {
    let api = Arc::new(FixtureApi::new([
        json!({"setting":"INTRADAY_MARGIN_SETTING_UNSPECIFIED"}),
        json!({"setting":"INTRADAY_MARGIN_SETTING_UNSPECIFIED"}),
        json!({}),
        json!({"setting":"INTRADAY_MARGIN_SETTING_INTRADAY"}),
    ]));
    let profile = profile(api.clone());
    let input = json!({"account":"dev-coinbase","requestId":"margin-1","setting":"INTRADAY_MARGIN_SETTING_INTRADAY"});
    let prepared = profile
        .prepare("coinbase.set-cfm-margin-setting", &input)
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    assert!(matches!(
        planned.request.endpoint,
        CoinbaseRoute::CfmMarginSetting
    ));
    assert_eq!(
        planned.request.exact_body,
        r#"{"setting":"INTRADAY_MARGIN_SETTING_INTRADAY"}"#
    );
    let journal = Arc::new(ConfigJournal::default());
    journal.persist_submission(&planned.submission).unwrap();
    profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();
    let record = OperationRecord {
        id: "margin-operation".into(),
        request_id: "margin-1".into(),
        fingerprint: "fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = profile
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let seen = api.seen.lock().unwrap();
    assert!(seen.iter().any(|(route, body, guard)| matches!(
        route,
        CoinbaseRoute::CfmMarginSetting
    ) && body.as_deref()
        == Some(r#"{"setting":"INTRADAY_MARGIN_SETTING_INTRADAY"}"#)
        && *guard == Some(true)));
}

#[tokio::test]
async fn cfm_margin_reconciliation_rejects_replaced_mapping_and_corrupt_receipt_digest() {
    let api = Arc::new(FixtureApi::new([
        json!({"setting":"INTRADAY_MARGIN_SETTING_UNSPECIFIED"}),
        json!({"setting":"INTRADAY_MARGIN_SETTING_UNSPECIFIED"}),
        json!({}),
        json!({"setting":"INTRADAY_MARGIN_SETTING_INTRADAY"}),
    ]));
    let profile = profile(api.clone());
    let input = json!({"account":"dev-coinbase","requestId":"margin-recovery-1","setting":"INTRADAY_MARGIN_SETTING_INTRADAY"});
    let prepared = profile
        .prepare("coinbase.set-cfm-margin-setting", &input)
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let journal = Arc::new(ConfigJournal::default());
    journal.persist_submission(&planned.submission).unwrap();
    profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();
    let seen_before_reconciliation = api.seen.lock().unwrap().len();
    let mut evidence = journal.evidence.lock().unwrap().clone().unwrap();
    let record = OperationRecord {
        id: "margin-recovery-op".into(),
        request_id: "margin-recovery-1".into(),
        fingerprint: "margin-recovery-fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input,
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(evidence.clone()),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };

    let replaced = profile_with_replaced_identity(api.clone());
    let stale = replaced
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(stale.status, Status::Unknown);

    evidence["bodySha256"] = json!("0".repeat(64));
    let mut corrupt_record = record.clone();
    corrupt_record.dispatch_evidence = Some(evidence);
    let corrupt = profile
        .reconcile_submission(&corrupt_record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(corrupt.status, Status::Unknown);
    assert_eq!(
        api.seen.lock().unwrap().len(),
        seen_before_reconciliation,
        "unknown reconciliation performs no native readback or replay"
    );
}

#[tokio::test]
async fn portfolio_rename_reconciliation_rejects_replaced_mapping_and_corrupt_receipt_digest() {
    use gofin_exchange::ExchangeAdapter;
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let listed = json!({"portfolios":[{"uuid":portfolio_id,"name":"Trading","type":"CONSUMER","deleted":false}]});
    let api = Arc::new(FixtureApi::new([
        listed.clone(),
        listed.clone(),
        json!({"portfolio":{"uuid":portfolio_id,"name":"Trading","type":"CONSUMER","deleted":false}}),
        json!({"portfolio":{"uuid":portfolio_id,"name":"Savings","type":"CONSUMER","deleted":false}}),
    ]));
    let profile = profile(api.clone());
    let adapter = ExchangeAdapter::new(profile.clone());
    let input =
        json!({"account":"dev-coinbase","requestId":"rename-recovery-1","newName":"Savings"});
    let prepared = adapter
        .prepare_configuration("coinbase.rename-portfolio", &input)
        .await
        .unwrap();
    let journal = Arc::new(ConfigJournal::default());
    let submission = adapter
        .submit_configuration(&prepared, journal.clone())
        .await
        .unwrap();
    let seen_before_reconciliation = api.seen.lock().unwrap().len();
    let record = OperationRecord {
        id: "rename-recovery-op".into(),
        request_id: "rename-recovery-1".into(),
        fingerprint: "rename-recovery-fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input,
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };

    let replaced = profile_with_replaced_identity(api.clone());
    let stale = replaced
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(stale.status, Status::Unknown);

    let mut corrupt_record = record.clone();
    corrupt_record.dispatch_evidence.as_mut().unwrap()["bodySha256"] = json!("0".repeat(64));
    let corrupt = profile
        .reconcile_submission(&corrupt_record, &submission)
        .await
        .unwrap();
    assert_eq!(corrupt.status, Status::Unknown);
    assert_eq!(
        api.seen.lock().unwrap().len(),
        seen_before_reconciliation,
        "unknown reconciliation performs no native readback or replay"
    );
}

#[tokio::test]
async fn portfolio_create_reconciliation_rejects_replaced_mapping_and_corrupt_receipt_digest() {
    use gofin_exchange::ExchangeAdapter;
    let api = Arc::new(FixtureApi::new([json!({
        "portfolio":{"uuid":"new-portfolio-uuid","name":"Treasury","type":"CONSUMER","deleted":false}
    })]));
    let profile = profile(api.clone());
    let adapter = ExchangeAdapter::new(profile.clone());
    let input = json!({"account":"dev-coinbase","requestId":"create-recovery-1","name":"Treasury"});
    let prepared = adapter
        .prepare_configuration("coinbase.create-portfolio", &input)
        .await
        .unwrap();
    let journal = Arc::new(ConfigJournal::default());
    let submission = adapter
        .submit_configuration(&prepared, journal.clone())
        .await
        .unwrap();
    let seen_before_reconciliation = api.seen.lock().unwrap().len();
    let record = OperationRecord {
        id: "create-recovery-op".into(),
        request_id: "create-recovery-1".into(),
        fingerprint: "create-recovery-fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input,
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().clone(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };

    let replaced = profile_with_replaced_identity(api.clone());
    let stale = replaced
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(stale.status, Status::Unknown);

    let mut corrupt_record = record.clone();
    corrupt_record.dispatch_evidence.as_mut().unwrap()["bodySha256"] = json!("0".repeat(64));
    let corrupt = profile
        .reconcile_submission(&corrupt_record, &submission)
        .await
        .unwrap();
    assert_eq!(corrupt.status, Status::Unknown);
    assert_eq!(
        api.seen.lock().unwrap().len(),
        seen_before_reconciliation,
        "unknown reconciliation performs no native readback or replay"
    );
}

#[tokio::test]
async fn portfolio_delete_reconciliation_rejects_replaced_mapping_and_corrupt_receipt_digest() {
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let target_detail =
        json!({"portfolio":{"uuid":portfolio_id,"name":"Empty","type":"CONSUMER","deleted":false}});
    let mut responses = vec![target_detail, json!({"accounts":[],"has_next":false})];
    responses.extend((0..5).map(|_| json!({"orders":[],"has_next":false})));
    let api = Arc::new(FixtureApi::new(responses));
    let profile = profile(api.clone());
    let input = json!({"account":"dev-coinbase","requestId":"delete-recovery-1","portfolioId":portfolio_id});
    let prepared = profile
        .prepare("coinbase.delete-portfolio", &input)
        .await
        .unwrap();
    let submission = gofin_domain::Submission {
        kind: "coinbase".into(),
        reference: "delete-recovery-reference".into(),
        data: json!({"product":"coinbase","account":"dev-coinbase","binding":prepared.binding}),
    };
    let body = b"{}";
    let evidence = json!({
        "httpStatus":200,
        "bodyHex":hex::encode(body),
        "bodySha256":hex::encode(sha2::Sha256::digest(body))
    });
    let record = OperationRecord {
        id: "delete-recovery-op".into(),
        request_id: "delete-recovery-1".into(),
        fingerprint: "delete-recovery-fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input,
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(submission.clone()),
        dispatch_evidence: Some(evidence.clone()),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let seen_before_reconciliation = api.seen.lock().unwrap().len();

    let replaced = profile_with_replaced_identity(api.clone());
    let stale = replaced
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(stale.status, Status::Unknown);

    let mut corrupt_record = record.clone();
    corrupt_record.dispatch_evidence.as_mut().unwrap()["bodySha256"] = json!("0".repeat(64));
    let corrupt = profile
        .reconcile_submission(&corrupt_record, &submission)
        .await
        .unwrap();
    assert_eq!(corrupt.status, Status::Unknown);
    assert_eq!(
        api.seen.lock().unwrap().len(),
        seen_before_reconciliation,
        "unknown reconciliation performs no native readback or replay"
    );
}

#[tokio::test]
async fn portfolio_delete_requires_an_exact_enrolled_nondefault_empty_portfolio() {
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let target_detail =
        json!({"portfolio":{"uuid":portfolio_id,"name":"Empty","type":"CONSUMER","deleted":false}});
    let empty_accounts = json!({"accounts":[],"has_next":false});
    let mut responses = vec![target_detail.clone(), empty_accounts.clone()];
    responses.extend((0..5).map(|_| json!({"orders":[],"has_next":false})));
    let api = Arc::new(FixtureApi::new(responses));
    let prof = profile(api);
    prof.attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    let input = json!({"account":"dev-coinbase","requestId":"delete-empty-portfolio","portfolioId":portfolio_id});
    let prepared = prof
        .prepare("coinbase.delete-portfolio", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["configurationRoute"], "portfolios_delete");
    assert_eq!(prepared.binding["previousName"], "Empty");
    assert_eq!(prepared.binding["checkedAccountCount"], 0);
    assert_eq!(prepared.binding["checkedOpenOrderCount"], 0);

    let default = Arc::new(FixtureApi::new([
        json!({"portfolio":{"uuid":portfolio_id,"name":"Trading","type":"DEFAULT","deleted":false}}),
    ]));
    let default = profile(default);
    default
        .attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    let error = default
        .prepare("coinbase.delete-portfolio", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_DEFAULT_PORTFOLIO_PROTECTED");
}

#[tokio::test]
async fn portfolio_accounts_use_key_scoped_native_accounts_with_complete_pagination() {
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let account_row = |uuid: &str, currency: &str| {
        json!({
            "uuid":uuid,"currency":currency,"retail_portfolio_id":portfolio_id,
            "ready":true,"active":true,
            "available_balance":{"value":"0","currency":currency},
            "hold":{"value":"0","currency":currency}
        })
    };
    let api = Arc::new(FixtureApi::new([
        json!({"accounts":[account_row("usd-account","USD")],"has_next":true,"cursor":"page-2"}),
        json!({"accounts":[account_row("btc-account","BTC")],"has_next":false}),
    ]));
    let prof = profile(api.clone());
    let result = prof
        .read(
            "coinbase.get-portfolio-accounts",
            &json!({
                "account":"dev-coinbase","portfolioId":portfolio_id
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["accounts"].as_array().unwrap().len(), 2);
    let seen = api.seen.lock().unwrap();
    let account_reads = seen
        .iter()
        .filter(|(endpoint, _, _)| matches!(endpoint, CoinbaseRoute::Accounts { .. }))
        .collect::<Vec<_>>();
    assert_eq!(account_reads.len(), 2);
    assert!(
        matches!(&account_reads[0].0, CoinbaseRoute::Accounts { query } if query.portfolio_id.as_deref() == Some(portfolio_id) && query.cursor.is_none())
    );
    assert!(
        matches!(&account_reads[1].0, CoinbaseRoute::Accounts { query } if query.portfolio_id.as_deref() == Some(portfolio_id) && query.cursor.as_deref() == Some("page-2"))
    );
}

#[tokio::test]
async fn portfolio_delete_refuses_nonzero_holds_and_active_orders() {
    let portfolio_id = "00000000-0000-4000-8000-000000000001";
    let detail =
        json!({"portfolio":{"uuid":portfolio_id,"name":"Extra","type":"CONSUMER","deleted":false}});
    let held = Arc::new(FixtureApi::new([
        detail.clone(),
        json!({"accounts":[{"uuid":"currency-account-1","currency":"USDC","retail_portfolio_id":portfolio_id,"ready":true,"active":true,"available_balance":{"value":"0","currency":"USDC"},"hold":{"value":"0.01","currency":"USDC"}}],"has_next":false}),
    ]));
    let input = json!({"account":"dev-coinbase","requestId":"delete-held-portfolio","portfolioId":portfolio_id});
    let held = profile(held);
    held.attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    let error = held
        .prepare("coinbase.delete-portfolio", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PORTFOLIO_NOT_EMPTY");

    let open_orders = Arc::new(FixtureApi::new([
        detail,
        json!({"accounts":[],"has_next":false}),
        json!({"orders":[{"retail_portfolio_id":portfolio_id,"status":"OPEN"}],"has_next":false}),
    ]));
    let open_orders = profile(open_orders);
    open_orders
        .attach_execution_state(Arc::new(EmptyExecutionState))
        .unwrap();
    let error = open_orders
        .prepare("coinbase.delete-portfolio", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PORTFOLIO_NOT_EMPTY");
}

#[tokio::test]
async fn portfolio_delete_refuses_unresolved_fin_activity_for_bound_resource() {
    let order_api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let mut prepared_order = profile(order_api)
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    // Older durable Advanced previews predate the resource binding; their
    // bound portfolio UUID must still protect deletion.
    prepared_order
        .binding
        .as_object_mut()
        .unwrap()
        .remove("resource");
    let (record, _) = submitted_record(prepared_order, None);
    let delete_api = Arc::new(FixtureApi::new([]));
    let profile = profile(delete_api);
    profile
        .attach_execution_state(Arc::new(StaticExecutionState(record)))
        .unwrap();
    let error = profile
        .prepare(
            "coinbase.delete-portfolio",
            &json!({
                "account":"dev-coinbase",
                "requestId":"delete-with-unresolved-order",
                "portfolioId":"00000000-0000-4000-8000-000000000001"
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PORTFOLIO_FIN_ACTIVITY_UNRESOLVED");
}

#[tokio::test]
async fn portfolio_delete_checks_real_store_prepared_binding_pointers() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prepared = profile(api)
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let (record, _) = submitted_record(prepared, None);
    let record_value = serde_json::to_value(&record).unwrap();
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            record.run_id.as_deref(),
            "submitted",
            &record_value,
            &record.created_at,
        )
        .unwrap();
    let execution_state = Arc::new(gofin_store::execution_state::ExecutionStateRepository(
        store.share(),
    ));

    let delete_api = Arc::new(FixtureApi::new([]));
    let profile = profile(delete_api);
    profile.attach_execution_state(execution_state).unwrap();
    let error = profile
        .prepare(
            "coinbase.delete-portfolio",
            &json!({
                "account":"dev-coinbase",
                "requestId":"delete-with-real-store-order",
                "portfolioId":"00000000-0000-4000-8000-000000000001"
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_PORTFOLIO_FIN_ACTIVITY_UNRESOLVED");
}

#[tokio::test]
async fn durable_client_order_id_cannot_be_reused_but_pre_dispatch_attempt_can_retry() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let prof = profile(api);
    let prepared = prof
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let (record, submission) = submitted_record(prepared.clone(), None);
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &record.id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            record.run_id.as_deref(),
            "submitted",
            &serde_json::to_value(&record).unwrap(),
            &record.created_at,
        )
        .unwrap();
    store
        .persist_submission(
            &record.id,
            &gofin_store::SubmissionRow {
                kind: submission.kind,
                reference: submission.reference,
                data: submission.data,
            },
            &gofin_domain::now(),
        )
        .unwrap();
    prof.attach_execution_state(Arc::new(
        gofin_store::execution_state::ExecutionStateRepository(store.share()),
    ))
    .unwrap();
    let error = match prof.plan_submission(&prepared).await {
        Ok(_) => panic!("a durable client-order identity must not be reused"),
        Err(error) => error,
    };
    assert_eq!(error.code, "COINBASE_CLIENT_ORDER_ID_REUSED");

    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let retry_profile = profile(api);
    let retry = retry_profile
        .prepare("coinbase.create-order", &order_input())
        .await
        .unwrap();
    let (mut pre_dispatch, _) = submitted_record(retry.clone(), None);
    pre_dispatch.submission = None;
    pre_dispatch.dispatch_evidence = None;
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &pre_dispatch.id,
            &pre_dispatch.request_id,
            &pre_dispatch.fingerprint,
            &pre_dispatch.operation,
            pre_dispatch.run_id.as_deref(),
            "prepared",
            &serde_json::to_value(&pre_dispatch).unwrap(),
            &pre_dispatch.created_at,
        )
        .unwrap();
    retry_profile
        .attach_execution_state(Arc::new(
            gofin_store::execution_state::ExecutionStateRepository(store.share()),
        ))
        .unwrap();
    assert!(retry_profile.plan_submission(&retry).await.is_ok());
}

#[tokio::test]
async fn generic_international_portfolio_list_is_filtered_to_the_enrolled_scope() {
    let api = Arc::new(FixtureApi::new([json!({"portfolios":[
        {"portfolio_id":"intl-enrolled","name":"Bound"},
        {"portfolio_id":"other-portfolio","name":"Other"}
    ]})]));
    let transport: Arc<dyn CoinbaseApi> = api;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "intl".into(),
            resource: ResourceId("coinbaseinternational:portfolio:intl-enrolled".into()),
            credential_ref: "synthetic-credential-ref".into(),
            physical_identity: "intl-profile".into(),
            identity_verified: true,
            portfolio_id: Some("intl-enrolled".into()),
            product: CoinbaseProduct::International,
        }],
        BTreeMap::from([("synthetic-credential-ref".into(), transport)]),
    )
    .unwrap();
    let result = profile
        .read(
            "coinbaseinternational.read-v1-private-portfolios",
            &json!({"account":"intl"}),
        )
        .await
        .unwrap();
    assert_eq!(result["portfolios"].as_array().unwrap().len(), 1);
    assert_eq!(result["portfolios"][0]["portfolio_id"], "intl-enrolled");
}

#[tokio::test]
async fn app_v2_account_read_requires_exact_current_advanced_uuid_and_currency_join() {
    let id = "currency-account-uuid";
    let portfolio = "00000000-0000-4000-8000-000000000001";
    let api = Arc::new(FixtureApi::new([
        json!({"data":[{"id":id,"currency":{"code":"BTC"}}],"pagination":{"next_starting_after":null}}),
        json!({"accounts":[{"uuid":id,"currency":"BTC","active":true,"ready":true,"portfolio_uuid":portfolio}],"has_next":false,"cursor":""}),
        json!({"data":{"id":id,"currency":{"code":"BTC"}}}),
    ]));
    let prof = profile(api.clone());
    let response = prof
        .read(
            "coinbase.read-v2-private-accounts-account-id",
            &json!({"account":"dev-coinbase","account_id":id}),
        )
        .await
        .unwrap();
    assert_eq!(response["data"]["id"], id);
    {
        let seen = api.seen.lock().unwrap();
        assert!(matches!(seen[0].0, CoinbaseRoute::LegacyAccounts { .. }));
        assert!(matches!(seen[1].0, CoinbaseRoute::Accounts { .. }));
        assert!(matches!(seen[2].0, CoinbaseRoute::NativeRead { .. }));
    }

    let mismatch = Arc::new(FixtureApi::new([
        json!({"data":[{"id":id,"currency":{"code":"BTC"}}],"pagination":{"next_starting_after":null}}),
        json!({"accounts":[{"uuid":id,"currency":"ETH","active":true,"ready":true,"portfolio_uuid":portfolio}],"has_next":false,"cursor":""}),
    ]));
    let mismatch_profile = profile(mismatch);
    let error = mismatch_profile
        .read(
            "coinbase.read-v2-private-accounts-account-id",
            &json!({"account":"dev-coinbase","account_id":id}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_APP_ACCOUNT_SCOPE_MISMATCH");

    let wrong_key = Arc::new(FixtureApi::new([]));
    *wrong_key.permissions.lock().unwrap() = json!({
        "portfolio_uuid":"different-portfolio","can_view":true,"can_trade":true,"can_transfer":true
    });
    let profile = profile(wrong_key.clone());
    let error = profile
        .read(
            "coinbase.read-v2-private-accounts-account-id",
            &json!({"account":"dev-coinbase","account_id":id}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_KEY_PERMISSION_DENIED");
    assert!(wrong_key.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn advanced_batch_cancel_requires_linked_parent_native_result_and_order_readback() {
    let api = Arc::new(FixtureApi::new([product(), preview("0.25")]));
    let profile = profile(api.clone());
    let original_input = order_input();
    let original = profile
        .prepare("coinbase.create-order", &original_input)
        .await
        .unwrap();
    let original_submission = profile.plan_submission(&original).await.unwrap().submission;
    let client_id = original.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    *api.responses.lock().unwrap()=VecDeque::from([
        Ok(CoinbaseResponse{status:200,body:serde_json::to_vec(&open_order(&client_id,"OPEN")).unwrap(),pagination:Default::default()}),
        Ok(CoinbaseResponse{status:200,body:br#"{"results":[{"success":true,"order_id":"venue-order"}]}"#.to_vec(),pagination:Default::default()}),
        Ok(CoinbaseResponse{status:200,body:serde_json::to_vec(&json!({"order":{"order_id":"venue-order","client_order_id":client_id,"product_id":"BTC-USD","status":"CANCELLED","filled_size":"0","filled_value":"0"}})).unwrap(),pagination:Default::default()}),
    ]);
    let parent = OperationRecord {
        id: "parent-order-op".into(),
        request_id: "order-request".into(),
        fingerprint: "fp".into(),
        fingerprint_version: 1,
        operation: "coinbase.create-order".into(),
        input: original_input,
        status: Status::Pending,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(original),
        approval: None,
        authorization_source: None,
        submission: Some(original_submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    profile
        .attach_execution_state(Arc::new(StaticExecutionState(parent)))
        .unwrap();
    let input = json!({"account":"dev-coinbase","requestId":"batch-cancel-request","orders":[{"operationId":"parent-order-op","venueOrderId":"venue-order","clientOrderId":client_id,"productId":"BTC-USD"}]});
    let prepared = profile
        .prepare("coinbase.cancel-orders", &input)
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    assert_eq!(planned.request.endpoint, CoinbaseRoute::OrderCancelBatch);
    assert_eq!(
        planned.request.exact_body,
        r#"{"order_ids":["venue-order"]}"#
    );
    let journal = Arc::new(FixtureJournal::authorized());
    journal.persist_submission(&planned.submission).unwrap();
    profile
        .dispatch(&planned.request, journal.clone())
        .await
        .unwrap();
    let record = OperationRecord {
        id: "batch-cancel-op".into(),
        request_id: "batch-cancel-request".into(),
        fingerprint: "fp".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: input.clone(),
        status: Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        authorization_source: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: journal.evidence.lock().unwrap().last().cloned(),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = profile
        .reconcile_submission(&record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert!(
        matches!(&result.verified.unwrap().effects[0],gofin_domain::FinancialEffect::SubmissionCancellation{operation_id,..} if operation_id=="parent-order-op")
    );
}

#[tokio::test]
async fn negative_conversion_status_without_zero_effect_proof_remains_unknown_and_is_not_replayed()
{
    for status in [
        "TRADE_STATUS_FAILED",
        "FAILED",
        "TRADE_STATUS_REJECTED",
        "REJECTED",
        "TRADE_STATUS_EXPIRED",
        "EXPIRED",
    ] {
        let api = Arc::new(FixtureApi::new([
            json!({"account":{"uuid":"usd-id","currency":{"code":"USD"},"portfolio_id":"00000000-0000-4000-8000-000000000001","ready":true,"active":true}}),
            json!({"account":{"uuid":"btc-id","currency":{"code":"BTC"},"portfolio_id":"00000000-0000-4000-8000-000000000001","ready":true,"active":true}}),
            json!({"trade":{"id":"trade-1","status":"TRADE_STATUS_CREATED","user_entered_amount":{"value":"10","currency":"USD"},"amount":{"value":"0.0002","currency":"BTC"},"subtotal":{"value":"10","currency":"USD"},"total":{"value":"10.1","currency":"USD"},"source":{"coinbase_account":{"account_id":"usd-id"}},"target":{"coinbase_account":{"account_id":"btc-id"}},"total_fee":{"amount":{"value":"0.1","currency":"USD"}}}}),
        ]));
        let prof = profile(api.clone());
        let input = json!({"account":"dev-coinbase","requestId":"negative-status","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"});
        let prepared = prof.prepare("coinbase.convert", &input).await.unwrap();
        let submission = Submission {
            kind: "coinbase".into(),
            reference: "trade-1".into(),
            data: json!({"product":"coinbase","account":"dev-coinbase","tradeId":"trade-1","binding":prepared.binding}),
        };
        let record = OperationRecord {
            id: format!("negative-conversion-{status}"),
            request_id: "negative-status".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: "coinbase.convert".into(),
            input: prepared.input.clone(),
            status: Status::Submitted,
            created_at: gofin_domain::now(),
            updated_at: gofin_domain::now(),
            preview: Some(prepared),
            approval: None,
            authorization_source: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        *api.responses.lock().unwrap() = VecDeque::from([Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&json!({"trade":{"id":"trade-1","status":status,"user_entered_amount":{"value":"10","currency":"USD"},"amount":{"value":"0.0002","currency":"BTC"},"subtotal":{"value":"10","currency":"USD"},"total":{"value":"10.1","currency":"USD"},"source":{"coinbase_account":{"account_id":"usd-id"}},"target":{"coinbase_account":{"account_id":"btc-id"}},"total_fee":{"amount":{"value":"0.1","currency":"USD"}}}})).unwrap(),
            pagination: Default::default(),
        })]);

        let first = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(first.status, Status::Unknown, "{status}");
        assert!(
            first.verified.is_none(),
            "{status} cannot prove zero effect"
        );
        let second = prof
            .reconcile_submission(&record, &submission)
            .await
            .unwrap();
        assert_eq!(
            second.status,
            Status::Unknown,
            "{status} remains unresolved"
        );
        assert!(
            second.verified.is_none(),
            "{status} keeps the reservation unresolved"
        );
        let seen = api.seen.lock().unwrap();
        assert_eq!(
            seen.iter()
                .filter(|(endpoint, _, _)| matches!(
                    endpoint,
                    CoinbaseRoute::ConvertTradeExact { .. }
                ))
                .count(),
            2,
            "reconciliation only reads the original trade ID"
        );
        assert!(
            seen.iter().all(|(endpoint, _, _)| !endpoint.mutating()),
            "negative status must not trigger conversion commit replay"
        );
    }
}
