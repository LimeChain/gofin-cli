use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use gofin_domain::execution_state::ExecutionState;
use gofin_domain::{AssetId, OperationRecord, ResourceId, Status, Submission};
use gofin_exchange::{
    ExchangeProfile,
    coinbase::{
        CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse,
        CoinbaseRoute, DispatchGuard,
    },
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct StaticPrices;
#[async_trait]
impl UsdPriceSource for StaticPrices {
    async fn quote(&self, _asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
        let now = chrono::Utc::now().to_rfc3339();
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: now.clone(),
            observed_at: now,
            source: "synthetic-institutional-test".into(),
        })
    }
}

struct ExchangeApi {
    queued: Mutex<VecDeque<CoinbaseResponse>>,
    seen: Mutex<Vec<CoinbaseEndpoint>>,
    order: Mutex<Value>,
}
#[async_trait]
impl CoinbaseApi for ExchangeApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(
            guard.is_none(),
            "read/reconcile requests do not receive write guards"
        );
        self.seen.lock().unwrap().push(endpoint.clone());
        if let CoinbaseRoute::ExchangeOrderByClientId { client_oid } = endpoint {
            assert_eq!(request.body, None);
            let mut order = self.order.lock().unwrap().clone();
            order["client_oid"] = json!(client_oid);
            return Ok(CoinbaseResponse::new(
                200,
                serde_json::to_vec(&order).unwrap(),
            ));
        }
        self.queued
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}

fn response(value: Value) -> CoinbaseResponse {
    CoinbaseResponse::new(200, serde_json::to_vec(&value).unwrap())
}
fn profile(api: Arc<ExchangeApi>) -> CoinbaseProfile {
    let transport: Arc<dyn CoinbaseApi> = api;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "exchange-main".into(),
            resource: ResourceId("coinbaseexchange:portfolio:profile-17".into()),
            credential_ref: "exchange-key".into(),
            physical_identity: "profile-17".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        [("exchange-key".into(), transport)].into(),
    )
    .unwrap()
    .with_prices(Arc::new(StaticPrices));
    profile
        .attach_execution_state(Arc::new(EmptyJournal))
        .unwrap();
    profile
}
struct EmptyJournal;
impl ExecutionState for EmptyJournal {
    fn allocate_nonce(&self, _: &str, _: u64) -> gofin_domain::Result<u64> {
        Ok(1)
    }
    fn operation(&self, _: &str) -> gofin_domain::Result<Option<OperationRecord>> {
        Ok(None)
    }
    fn operations_by_prepared_binding(
        &self,
        _: &str,
        _: &str,
        _: &Value,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        Ok(vec![])
    }
}
struct Journal(Vec<OperationRecord>);
impl ExecutionState for Journal {
    fn allocate_nonce(&self, _: &str, _: u64) -> gofin_domain::Result<u64> {
        Ok(1)
    }
    fn operation(&self, id: &str) -> gofin_domain::Result<Option<OperationRecord>> {
        Ok(self.0.iter().find(|record| record.id == id).cloned())
    }
    fn operations_by_prepared_binding(
        &self,
        operation: &str,
        pointer: &str,
        value: &Value,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        Ok(self
            .0
            .iter()
            .filter(|record| {
                record.operation == operation
                    && record.preview.as_ref().and_then(|prepared| {
                        prepared
                            .binding
                            .pointer(pointer.strip_prefix("/preview/binding").unwrap_or(pointer))
                    }) == Some(value)
            })
            .cloned()
            .collect())
    }
}
fn input() -> Value {
    json!({"account":"exchange-main","requestId":"order-17","productId":"BTC-USD","side":"BUY","baseSize":"0.1","limitPrice":"100","maximumSpend":"11","minimumReceive":"0.1","maximumFee":"0.1","postOnly":false})
}
fn product() -> Value {
    json!({"id":"BTC-USD","base_currency":"BTC","quote_currency":"USD","base_increment":"0.001","price_increment":"0.01","base_min_size":"0.001","status":"online","trading_disabled":false})
}
fn order() -> Value {
    json!({"id":"venue-order-17","profile_id":"profile-17","product_id":"BTC-USD","side":"buy","time_in_force":"GTC","status":"done","filled_size":"0.1","executed_value":"10","fill_fees":"0.02"})
}
fn fill(trade_id: i64, size: &str, fee: &str) -> Value {
    json!({"trade_id":trade_id,"order_id":"venue-order-17","profile_id":"profile-17","product_id":"BTC-USD","side":"buy","size":size,"price":"100","fee":fee,"settled":true})
}
fn api(fill_pages: Vec<CoinbaseResponse>) -> Arc<ExchangeApi> {
    let mut rows = VecDeque::from([
        response(product()),
        response(json!({"maker_fee_rate":"0.001","taker_fee_rate":"0.002"})),
        response(
            json!([{"id":"usd-account","profile_id":"profile-17","currency":"USD","available":"100","hold":"0"}]),
        ),
    ]);
    rows.extend(fill_pages);
    Arc::new(ExchangeApi {
        queued: Mutex::new(rows),
        seen: Mutex::new(Vec::new()),
        order: Mutex::new(order()),
    })
}

#[tokio::test]
async fn named_plan_defers_exchange_balance_shortfall_but_normal_prep_rejects_it() {
    let api_with_no_available = || {
        let api = api(vec![]);
        let mut queue = api.queued.lock().unwrap();
        queue[2] = response(
            json!([{"id":"usd-account","profile_id":"profile-17","currency":"USD","available":"0","hold":"0"}]),
        );
        drop(queue);
        api
    };
    let profile_obj = profile(api_with_no_available());
    let error = profile_obj
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_BALANCE_INSUFFICIENT");

    let profile_obj = profile(api_with_no_available());
    let prepared = institutional::prepare_order_for_plan(
        &profile_obj,
        "coinbaseexchange.create-order",
        &input(),
    )
    .await
    .unwrap()
    .expect("Exchange order is handled by the plan helper");
    assert_eq!(prepared.operation, "coinbaseexchange.create-order");
    assert_eq!(prepared.binding["physicalIdentity"], "profile-17");
}
fn record(prepared: gofin_domain::PreparedOperation, submission: Submission) -> OperationRecord {
    OperationRecord {
        id: "exchange-order-operation".into(),
        request_id: "order-17".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
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

#[tokio::test]
async fn exchange_plan_rejects_a_client_id_with_an_existing_durable_submission() {
    let api = api(vec![]);
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    let client_order_id = prepared.binding["clientOrderId"].as_str().unwrap();
    let existing = record(
        prepared.clone(),
        Submission {
            kind: "coinbase".into(),
            reference: client_order_id.into(),
            data: json!({"product":"coinbaseexchange","clientOrderId":client_order_id,"account":"exchange-main","productId":"BTC-USD"}),
        },
    );
    let mut existing = existing;
    existing.id = "original-submission".into();
    profile
        .attach_execution_state(Arc::new(Journal(vec![existing])))
        .unwrap();

    let error = profile
        .plan_submission(&prepared)
        .await
        .err()
        .expect("duplicate native order ID must be rejected");
    assert_eq!(error.code, "COINBASE_CLIENT_ORDER_ID_REUSED");
    assert_eq!(api.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn exchange_reconciliation_stays_unknown_when_another_record_claims_client_id() {
    let api = api(vec![]);
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    let plan = profile.plan_submission(&prepared).await.unwrap();
    let submission = plan.submission;
    let current = record(prepared.clone(), submission.clone());
    let mut competing = record(prepared, submission.clone());
    competing.id = "competing-submission".into();
    profile
        .attach_execution_state(Arc::new(Journal(vec![competing])))
        .unwrap();

    let result = profile
        .reconcile_submission(&current, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(api.seen.lock().unwrap().len() == 3);
}

#[tokio::test]
async fn exchange_limit_order_binds_exact_body_and_reconciles_all_cursor_pages() {
    let mut first = response(json!([fill(1, "0.04", "0.008")]));
    first.pagination.after = Some("fill-cursor-1".into());
    let second = response(json!([fill(2, "0.06", "0.012")]));
    let api = api(vec![first, second]);
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    assert_eq!(prepared.binding["productId"], "BTC-USD");
    assert_eq!(prepared.binding["estimatedFee"], "0.02");
    assert_eq!(
        prepared.intent.debits[0].amount.asset.network,
        "coinbaseexchange"
    );
    let plan = profile.plan_submission(&prepared).await.unwrap();
    assert_eq!(plan.request.endpoint, CoinbaseRoute::ExchangeOrders);
    assert_eq!(plan.request.endpoint.method(), "POST");
    let body: Value = serde_json::from_str(&plan.request.exact_body).unwrap();
    assert_eq!(body["client_oid"], plan.submission.reference);
    assert_eq!(body["product_id"], "BTC-USD");
    assert_eq!(body["side"], "buy");
    let submission = plan.submission;
    let record = record(prepared, submission.clone());
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    let verified = result.verified.unwrap();
    assert!(verified.effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::Conversion { spent, received, .. } if spent.atoms.to_decimal(36) == "10" && spent.asset.network == "coinbaseexchange" && received.atoms.to_decimal(36) == "0.1" && received.asset.network == "coinbaseexchange")));
    assert!(verified.effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::Cost { amount, .. } if amount.atoms.to_decimal(36) == "0.02" && amount.asset.network == "coinbaseexchange")));
    let seen = api.seen.lock().unwrap();
    assert!(
        matches!(&seen[3], CoinbaseRoute::ExchangeOrderByClientId { client_oid } if client_oid.starts_with("fin-"))
    );
    assert!(
        matches!(&seen[4], CoinbaseRoute::ExchangeFillsFiltered { query } if query.after.is_none() && query.order_id.as_deref() == Some("venue-order-17"))
    );
    assert!(
        matches!(&seen[5], CoinbaseRoute::ExchangeFillsFiltered { query } if query.after.as_deref() == Some("fill-cursor-1"))
    );
}

#[tokio::test]
async fn exchange_fok_is_bound_into_exact_native_request_and_post_only_is_gtc_only() {
    let api_instance = api(vec![]);
    let profile_instance = profile(api_instance.clone());
    let mut request = input();
    request["timeInForce"] = json!("FOK");
    let prepared = profile_instance
        .prepare("coinbaseexchange.create-order", &request)
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(prepared.binding["requestBodyExact"].as_str().unwrap()).unwrap();
    assert_eq!(body["time_in_force"], "FOK");
    assert_eq!(prepared.binding["timeInForce"], "FOK");
    assert!(prepared.summary.contains("FOK"));
    let plan = profile_instance.plan_submission(&prepared).await.unwrap();
    let operation_record = record(prepared, plan.submission.clone());
    let reconciliation = profile_instance
        .reconcile_submission(&operation_record, &plan.submission)
        .await
        .unwrap();
    assert_eq!(reconciliation.status, Status::Unknown);
    assert!(
        api_instance
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|endpoint| !matches!(endpoint, CoinbaseRoute::ExchangeFillsFiltered { .. }))
    );

    let post_only_api = api(vec![]);
    let post_only_profile = profile(post_only_api.clone());
    request["postOnly"] = json!(true);
    assert!(
        post_only_profile
            .prepare("coinbaseexchange.create-order", &request)
            .await
            .is_err()
    );
    assert!(post_only_api.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exchange_fill_cursor_repetition_is_unknown_instead_of_certifying_a_truncated_page() {
    let mut first = response(json!([fill(1, "0.04", "0.008")]));
    first.pagination.after = Some("same-cursor".into());
    let mut second = response(json!([fill(2, "0.06", "0.012")]));
    second.pagination.after = Some("same-cursor".into());
    let api = api(vec![first, second]);
    let profile = profile(api);
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    let submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let record = record(prepared, submission.clone());
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
}

#[tokio::test]
async fn exchange_reconciliation_rejects_a_wrong_authoritative_product_before_fills() {
    let api = api(vec![response(json!([]))]);
    api.order.lock().unwrap()["product_id"] = json!("ETH-USD");
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    let submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let record = record(prepared, submission.clone());
    // Wrong product identity must stop before a second authenticated query.
    // The mock's authoritative order is normally bound to BTC-USD.
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(
        !api.seen
            .lock()
            .unwrap()
            .iter()
            .any(|route| matches!(route, CoinbaseRoute::ExchangeFillsFiltered { .. }))
    );
}

#[tokio::test]
async fn exchange_done_order_keeps_reservation_pending_until_fills_are_settled() {
    let unsettled = json!({"trade_id":1,"order_id":"venue-order-17","profile_id":"profile-17","product_id":"BTC-USD","side":"buy","size":"0.1","price":"100","fee":"0.02","settled":false});
    let api = api(vec![response(json!([unsettled]))]);
    let profile = profile(api);
    let prepared = profile
        .prepare("coinbaseexchange.create-order", &input())
        .await
        .unwrap();
    let submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let record = record(prepared, submission.clone());
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Pending);
    assert!(
        result.verified.is_some(),
        "cumulative fill evidence is retained while settlement is pending"
    );
}
