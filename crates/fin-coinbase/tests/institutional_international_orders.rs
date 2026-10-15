use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use fin_domain::{AssetId, OperationRecord, ResourceId, Status, Submission};
use fin_exchange::ExchangeProfile;
use fin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, CoinbaseRoute,
    DispatchGuard,
};
use fin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct StaticPrices;
#[async_trait]
impl UsdPriceSource for StaticPrices {
    async fn quote(&self, _asset: &AssetId) -> fin_domain::Result<UsdQuote> {
        let now = chrono::Utc::now().to_rfc3339();
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: now.clone(),
            observed_at: now,
            source: "institutional-intl-test".into(),
        })
    }
}
struct Api {
    queue: Mutex<VecDeque<CoinbaseResponse>>,
    calls: Mutex<Vec<(CoinbaseEndpoint, Option<String>, bool)>>,
}
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        self.calls
            .lock()
            .unwrap()
            .push((endpoint, request.body, guard.is_some()));
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
fn response(value: Value) -> CoinbaseResponse {
    CoinbaseResponse::new(200, serde_json::to_vec(&value).unwrap())
}
fn instrument() -> Value {
    json!({"instrument_id":"inst-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","type":"SPOT","mode":"STANDARD","base_asset_id":"asset-btc-9","base_asset_uuid":"btc-uuid","base_asset_name":"BTC","quote_asset_id":"asset-usdc-9","quote_asset_uuid":"usdc-uuid","quote_asset_name":"USDC","base_increment":"0.001","quote_increment":"0.01","min_quantity":"0.001","trading_state":"TRADING"})
}
fn responses() -> Vec<CoinbaseResponse> {
    vec![
        response(instrument()),
        response(
            json!([{"instrument_type":"SPOT","maker_fee_rate":"0.001","taker_fee_rate":"0.002"}]),
        ),
        response(
            json!([{"asset_id":"asset-usdc-9","asset_uuid":"usdc-uuid","asset_name":"USDC","quantity":"100","hold":"0","transfer_hold":"0","pledged_collateral_quantity":"0","loan":"0"}]),
        ),
    ]
}
fn api(mut queue: Vec<CoinbaseResponse>) -> Arc<Api> {
    Arc::new(Api {
        queue: Mutex::new(queue.drain(..).collect()),
        calls: Mutex::new(Vec::new()),
    })
}
fn profile(api: Arc<Api>) -> CoinbaseProfile {
    let transport: Arc<dyn CoinbaseApi> = api;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "intl-main".into(),
            resource: ResourceId("coinbaseinternational:portfolio:portfolio-9".into()),
            credential_ref: "intl-credential".into(),
            physical_identity: "client:portfolio:portfolio-9".into(),
            identity_verified: true,
            portfolio_id: Some("portfolio-9".into()),
            product: CoinbaseProduct::International,
        }],
        [("intl-credential".into(), transport)].into(),
    )
    .unwrap()
    .with_prices(Arc::new(StaticPrices));
    profile
        .attach_execution_state(Arc::new(NoAmendments))
        .unwrap();
    profile
}

struct NoAmendments;
impl fin_domain::execution_state::ExecutionState for NoAmendments {
    fn allocate_nonce(&self, _: &str, _: u64) -> fin_domain::Result<u64> {
        Ok(1)
    }
    fn operation(&self, _: &str) -> fin_domain::Result<Option<OperationRecord>> {
        Ok(None)
    }
    fn operations_by_prepared_binding(
        &self,
        _: &str,
        _: &str,
        _: &Value,
    ) -> fin_domain::Result<Vec<OperationRecord>> {
        Ok(vec![])
    }
}
fn input() -> Value {
    json!({"account":"intl-main","requestId":"order-9","instrument":"BTC-USDC","side":"BUY","baseSize":"0.1","limitPrice":"100","maximumSpend":"10.2","minimumReceive":"0.1","maximumFee":"0.02","postOnly":false})
}
fn order(client: &str) -> Value {
    order_state(client, "DONE", "0")
}
fn order_state(client: &str, status: &str, leaves: &str) -> Value {
    json!({"order_id":"venue-order-9","client_order_id":client,"portfolio_uuid":"portfolio-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","side":"BUY","type":"LIMIT","tif":"GTC","price":"100","size":"0.1","order_status":status,"exec_qty":"0.1","fee":"0.02","leaves_qty":leaves})
}
fn fill(client: &str) -> Value {
    json!({"portfolio_uuid":"portfolio-9","fill_id":"fill-9","order_id":"venue-order-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","client_order_id":client,"fill_price":"100","fill_qty":"0.1","side":"BUY","fee":"0.02","fee_asset":"USDC"})
}
fn record(prepared: fin_domain::PreparedOperation, submission: Submission) -> OperationRecord {
    OperationRecord {
        id: "intl-order-op".into(),
        request_id: "order-9".into(),
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
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

struct ParentState(OperationRecord);
impl fin_domain::execution_state::ExecutionState for ParentState {
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
        let matching_value = self.0.preview.as_ref().is_some_and(|p| match pointer {
            "/preview/binding/clientOrderId" => p.binding["clientOrderId"] == *value,
            "/preview/binding/successorClientOrderId" => {
                p.binding["successorClientOrderId"] == *value
            }
            _ => false,
        });
        Ok((self.0.operation == operation && matching_value)
            .then(|| self.0.clone())
            .into_iter()
            .collect())
    }
}

#[tokio::test]
async fn international_spot_order_uses_locally_verified_bounds_and_enrolled_scope() {
    let transport = api(responses());
    let profile_obj = profile(transport.clone());
    let prepared = profile_obj
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    assert_eq!(prepared.binding["portfolioId"], "portfolio-9");
    assert_eq!(prepared.binding["estimatedFee"], "0.02");
    let body: Value =
        serde_json::from_str(prepared.binding["requestBodyExact"].as_str().unwrap()).unwrap();
    assert!(prepared.binding.get("previewBodyExact").is_none());
    assert!(prepared.binding.get("preview").is_none());
    assert!(prepared.checks[0].detail.contains("checked locally"));
    let client = body["client_order_id"].as_str().unwrap();
    assert_eq!(client.len(), 36);
    assert_eq!(client, client.to_ascii_lowercase());
    assert_eq!(body["client_order_id"], prepared.binding["clientOrderId"]);
    assert_eq!(body["instrument"], "BTC-USDC");
    let planned = profile_obj.plan_submission(&prepared).await.unwrap();
    assert_eq!(planned.request.endpoint, CoinbaseRoute::InternationalOrders);
    assert_eq!(planned.request.product, CoinbaseProduct::International);
    assert_eq!(
        planned.request.exact_body,
        prepared.binding["requestBodyExact"].as_str().unwrap()
    );
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(matches!(calls[2].0, CoinbaseEndpoint::NativeRead { .. }));
    assert!(!calls[2].2);
}

#[tokio::test]
async fn international_plan_rejects_client_id_with_durable_submission() {
    let api = api(responses());
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let client = prepared.binding["clientOrderId"].as_str().unwrap();
    let mut competing = record(
        prepared.clone(),
        Submission {
            kind: "coinbase".into(),
            reference: client.into(),
            data: json!({"product":"coinbaseinternational","account":"intl-main","portfolioId":"portfolio-9","clientOrderId":client,"instrument":"BTC-USDC"}),
        },
    );
    competing.id = "already-submitted-order".into();
    profile
        .attach_execution_state(Arc::new(ParentState(competing)))
        .unwrap();

    let error = profile
        .plan_submission(&prepared)
        .await
        .err()
        .expect("duplicate native INTL ID must not dispatch");
    assert_eq!(error.code, "COINBASE_CLIENT_ORDER_ID_REUSED");
    assert_eq!(api.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn international_order_dispatch_planning_requires_protected_journal() {
    let api = api(responses());
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "intl-main".into(),
            resource: ResourceId("coinbaseinternational:portfolio:portfolio-9".into()),
            credential_ref: "intl-credential".into(),
            physical_identity: "client:portfolio:portfolio-9".into(),
            identity_verified: true,
            portfolio_id: Some("portfolio-9".into()),
            product: CoinbaseProduct::International,
        }],
        [("intl-credential".into(), transport)].into(),
    )
    .unwrap()
    .with_prices(Arc::new(StaticPrices));
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();

    let error = profile
        .plan_submission(&prepared)
        .await
        .err()
        .expect("planning without journal must fail closed");
    assert_eq!(error.code, "EXECUTION_STATE_UNAVAILABLE");
    assert_eq!(api.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn international_reconciliation_is_unknown_when_a_competing_record_claims_id() {
    let api = api(responses());
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let current = record(prepared.clone(), planned.submission.clone());
    let mut competing = record(prepared, planned.submission.clone());
    competing.id = "competing-order".into();
    profile
        .attach_execution_state(Arc::new(ParentState(competing)))
        .unwrap();

    let result = profile
        .reconcile_submission(&current, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert_eq!(api.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn international_spot_fok_binds_time_in_force_into_submission() {
    let transport = api(responses());
    let profile_obj = profile(transport.clone());
    let mut order = input();
    order["timeInForce"] = json!("FOK");
    let prepared = profile_obj
        .prepare("coinbaseinternational.create-order", &order)
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(prepared.binding["requestBodyExact"].as_str().unwrap()).unwrap();
    assert_eq!(body["tif"], "FOK");
    assert_eq!(prepared.binding["timeInForce"], "FOK");
    assert!(prepared.summary.contains("FOK"));

    let post_only_api = api(responses());
    let post_only_profile = profile(post_only_api.clone());
    order["postOnly"] = json!(true);
    assert!(
        post_only_profile
            .prepare("coinbaseinternational.create-order", &order)
            .await
            .is_err()
    );
    assert!(post_only_api.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn international_fok_reconciliation_rejects_an_order_with_changed_tif() {
    let transport = api(responses());
    let profile_obj = profile(transport.clone());
    let mut order_input = input();
    order_input["timeInForce"] = json!("FOK");
    let prepared = profile_obj
        .prepare("coinbaseinternational.create-order", &order_input)
        .await
        .unwrap();
    let client = prepared.binding["clientOrderId"].as_str().unwrap();
    let plan = profile_obj.plan_submission(&prepared).await.unwrap();
    transport.queue.lock().unwrap().extend([
        response(json!({"results":[{"order_id":"venue-order-9","client_order_id":client,"portfolio_uuid":"portfolio-9","symbol":"BTC-USDC"}]})),
        response(json!({"order_id":"venue-order-9","client_order_id":client,"portfolio_uuid":"portfolio-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","side":"BUY","type":"LIMIT","tif":"GTC","price":"100","size":"0.1","order_status":"DONE","exec_qty":"0.1","fee":"0.02","leaves_qty":"0"})),
    ]);
    let operation_record = record(prepared, plan.submission.clone());
    let result = profile_obj
        .reconcile_submission(&operation_record, &plan.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(
        !transport
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(endpoint, _, _)| matches!(
                endpoint,
                CoinbaseRoute::InternationalPortfolioFillsFiltered { .. }
            ))
    );
}

#[tokio::test]
async fn international_spot_reconciliation_requires_exact_authoritative_order_and_fee_asset() {
    let api = api(responses());
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let client = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    api.queue.lock().unwrap().extend([
        response(json!({"results":[{"order_id":"venue-order-9","client_order_id":client,"portfolio_uuid":"portfolio-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC"}]})),
        response(order(&client)),
        response(json!({"pagination":{"result_limit":100,"result_offset":0},"results":[fill(&client)]})),
    ]);
    let submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let record = record(prepared, submission.clone());
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
    let verified = result.verified.unwrap();
    assert!(verified.effects.iter().any(|e|matches!(e,fin_domain::FinancialEffect::Conversion{spent,received,..}if spent.atoms.to_decimal(36)=="10"&&spent.asset.network=="coinbaseinternational"&&received.atoms.to_decimal(36)=="0.1"&&received.asset.network=="coinbaseinternational")));
    assert!(verified.effects.iter().any(|e|matches!(e,fin_domain::FinancialEffect::Cost{amount,..}if amount.asset.reference=="usdc-uuid"&&amount.asset.network=="coinbaseinternational"&&amount.atoms.to_decimal(36)=="0.02")));
    let calls = api.calls.lock().unwrap();
    assert!(matches!(
        calls[3].0,
        CoinbaseRoute::InternationalOrdersFiltered { .. }
    ));
    assert!(matches!(
        calls[4].0,
        CoinbaseRoute::InternationalOrderScoped { .. }
    ));
    assert!(matches!(
        calls[5].0,
        CoinbaseRoute::InternationalPortfolioFillsFiltered { .. }
    ));
}

#[tokio::test]
async fn lost_intl_order_receipt_recovers_by_client_id_fills_without_replay() {
    let api = api(responses());
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let client = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut fill_row = fill(&client);
    fill_row["order_id"] = json!("43877033468085758");
    let order_doc = json!({"order_id":43877033468085758_u64,"client_order_id":client,"portfolio_uuid":"portfolio-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","side":"BUY","tif":"GTC","order_status":"DONE","exec_qty":"0.1","fee":"0.02"});
    api.queue.lock().unwrap().extend([
        response(json!({"results":[]})),
        response(json!({"pagination":{"result_limit":100,"result_offset":0},"results":[fill_row.clone()]})),
        response(order_doc),
        response(json!({"pagination":{"result_limit":100,"result_offset":0},"results":[fill_row]})),
    ]);
    let submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let record = record(prepared, submission.clone());
    let result = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed, "{:#?}", result.evidence);
    let calls = api.calls.lock().unwrap();
    assert!(matches!(
        calls[3].0,
        CoinbaseRoute::InternationalOrdersFiltered { .. }
    ));
    assert!(
        matches!(calls[4].0,CoinbaseRoute::InternationalPortfolioFillsFiltered{ref query,..}if query.client_order_id.as_deref()==Some(submission.reference.as_str())&&query.order_id.is_none())
    );
    assert!(
        matches!(&calls[5].0,CoinbaseRoute::InternationalOrderScoped{order_id,..}if order_id=="43877033468085758")
    );
    assert!(
        matches!(calls[6].0,CoinbaseRoute::InternationalPortfolioFillsFiltered{ref query,..}if query.order_id.as_deref()==Some("43877033468085758"))
    );
}

#[tokio::test]
async fn international_edit_is_linked_price_only_and_requires_receipt_plus_scoped_readback() {
    let api = api(responses());
    let profile = profile(api.clone());
    let original = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let original_submission = profile.plan_submission(&original).await.unwrap().submission;
    let mut parent = record(original.clone(), original_submission.clone());
    parent.id = "parent-order".into();
    profile
        .attach_execution_state(Arc::new(ParentState(parent.clone())))
        .unwrap();
    let linked = fin_domain::amendments::AmendmentContext {
        original_evidence: Some(fin_domain::amendments::OriginalSubmissionEvidence::capture(
            &parent,
        )),
        original: Some(original),
        revisions: vec![],
    };
    api.queue.lock().unwrap().push_back(response(order_state(
        &original_submission.reference,
        "WORKING",
        "0.1",
    )));
    let edit_input = json!({"account":"intl-main","requestId":"edit-9","operationId":"parent-order","orderId":"venue-order-9","clientOrderId":original_submission.reference,"instrument":"BTC-USDC","newPrice":"99"});
    let edit = fin_coinbase::institutional::prepare_with_linked_context(
        &profile,
        "coinbaseinternational.edit-order",
        &edit_input,
        &linked,
    )
    .await
    .unwrap()
    .unwrap();
    let successor = edit.binding["successorClientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let planned = profile.plan_submission(&edit).await.unwrap();
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::InternationalEditOrder {
            order_id: "venue-order-9".into()
        }
    );
    assert_eq!(planned.request.product, CoinbaseProduct::International);
    assert_eq!(
        planned.request.exact_body,
        fin_domain::canonical_json(
            &json!({"client_order_id":successor,"portfolio":"portfolio-9","price":"99"})
        )
        .unwrap()
    );
    assert_eq!(planned.submission.reference, successor);

    let receipt_value = json!({"order_id":"venue-order-9","client_order_id":successor,"portfolio_uuid":"portfolio-9","instrument_uuid":"instrument-uuid","symbol":"BTC-USDC","side":"BUY","type":"LIMIT","tif":"GTC","price":"99","size":"0.1","order_status":"WORKING","leaves_qty":"0.1"});
    let receipt = serde_json::to_vec(&receipt_value).unwrap();
    let edit_record = OperationRecord {
        id: "edit-operation".into(),
        request_id: "edit-9".into(),
        fingerprint: "edit-fingerprint".into(),
        fingerprint_version: 1,
        operation: edit.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: edit.input.clone(),
        preview: Some(edit),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    api.queue.lock().unwrap().push_back(response(receipt_value));
    let result = profile
        .reconcile_submission(&edit_record, &planned.submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert!(
        matches!(result.verified.unwrap().effects.as_slice(), [fin_domain::FinancialEffect::SubmissionAmendment { operation_id, submission, .. }] if operation_id == "parent-order" && submission == &original_submission)
    );
}

#[tokio::test]
async fn international_edit_rejects_a_successor_client_id_already_submitted_elsewhere() {
    let api = api(responses());
    let profile = profile(api.clone());
    let original = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let original_submission = profile.plan_submission(&original).await.unwrap().submission;
    let mut parent = record(original.clone(), original_submission.clone());
    parent.id = "parent-order".into();
    profile
        .attach_execution_state(Arc::new(ParentState(parent.clone())))
        .unwrap();
    let linked = fin_domain::amendments::AmendmentContext {
        original_evidence: Some(fin_domain::amendments::OriginalSubmissionEvidence::capture(
            &parent,
        )),
        original: Some(original),
        revisions: vec![],
    };
    api.queue.lock().unwrap().push_back(response(order_state(
        &original_submission.reference,
        "WORKING",
        "0.1",
    )));
    let edit_input = json!({"account":"intl-main","requestId":"edit-duplicate","operationId":"parent-order","orderId":"venue-order-9","clientOrderId":original_submission.reference,"instrument":"BTC-USDC","newPrice":"99"});
    let edit = institutional::prepare_with_linked_context(
        &profile,
        "coinbaseinternational.edit-order",
        &edit_input,
        &linked,
    )
    .await
    .unwrap()
    .unwrap();
    let successor = edit.binding["successorClientOrderId"].as_str().unwrap();
    let mut competing = record(
        edit.clone(),
        Submission {
            kind: "coinbase".into(),
            reference: successor.into(),
            data: json!({"product":"coinbaseinternational","clientOrderId":successor}),
        },
    );
    competing.id = "competing-edit".into();
    profile
        .attach_execution_state(Arc::new(ParentState(competing)))
        .unwrap();

    let error = profile
        .plan_submission(&edit)
        .await
        .err()
        .expect("duplicate successor client ID must be rejected");
    assert_eq!(error.code, "COINBASE_CLIENT_ORDER_ID_REUSED");
}

#[tokio::test]
async fn international_edit_rejects_changed_or_non_gtc_open_order() {
    for (mut response_order, expected_code) in [
        (
            order_state("client", "WORKING", "0.1"),
            "COINBASE_EDIT_TARGET_CHANGED",
        ),
        (
            order_state("client", "WORKING", "0.1"),
            "COINBASE_EDIT_TARGET_CHANGED",
        ),
    ] {
        let api = api(responses());
        let profile = profile(api.clone());
        let original = profile
            .prepare("coinbaseinternational.create-order", &input())
            .await
            .unwrap();
        let original_submission = profile.plan_submission(&original).await.unwrap().submission;
        let mut parent = record(original.clone(), original_submission.clone());
        parent.id = "parent-order".into();
        profile
            .attach_execution_state(Arc::new(ParentState(parent.clone())))
            .unwrap();
        let linked = fin_domain::amendments::AmendmentContext {
            original_evidence: Some(fin_domain::amendments::OriginalSubmissionEvidence::capture(
                &parent,
            )),
            original: Some(original),
            revisions: vec![],
        };
        if expected_code == "COINBASE_EDIT_TARGET_CHANGED"
            && response_order["tif"] == "GTC"
            && response_order["price"] == "100"
        {
            response_order["tif"] = json!("IOC");
        }
        api.queue
            .lock()
            .unwrap()
            .push_back(response(response_order));
        let edit_input = json!({"account":"intl-main","requestId":"edit-9","operationId":"parent-order","orderId":"venue-order-9","clientOrderId":original_submission.reference,"instrument":"BTC-USDC","newPrice":"99"});
        let err = fin_coinbase::institutional::prepare_with_linked_context(
            &profile,
            "coinbaseinternational.edit-order",
            &edit_input,
            &linked,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, expected_code);
    }
}

#[tokio::test]
async fn international_order_prepare_rejects_non_spot_and_balance_holds() {
    let mut queued = responses();
    queued[0] = response({
        let mut v = instrument();
        v["type"] = json!("PERP");
        v
    });
    let err = profile(api(queued))
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap_err();
    assert_eq!(err.code, "COINBASE_INSTRUMENT_UNAVAILABLE");
    let mut queued = responses();
    queued[2] = response(
        json!([{"asset_id":"asset-usdc-9","asset_uuid":"usdc-uuid","asset_name":"USDC","quantity":"100","hold":"99","transfer_hold":"0","pledged_collateral_quantity":"0","loan":"0"}]),
    );
    let err = profile(api(queued))
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap_err();
    assert_eq!(err.code, "COINBASE_BALANCE_INSUFFICIENT");
}

#[tokio::test]
async fn named_plan_defers_only_international_balance_shortfall_after_local_validation() {
    let mut queued = responses();
    queued[2] = response(json!([{
        "asset_id":"asset-usdc-9", "asset_uuid":"usdc-uuid", "asset_name":"USDC",
        "quantity":"0", "hold":"0", "transfer_hold":"0",
        "pledged_collateral_quantity":"0", "loan":"100"
    }]));
    let profile = profile(api(queued));
    let prepared = institutional::prepare_order_for_plan(
        &profile,
        "coinbaseinternational.create-order",
        &input(),
    )
    .await
    .unwrap()
    .expect("International order is handled by the plan helper");
    assert_eq!(prepared.operation, "coinbaseinternational.create-order");
    assert_eq!(prepared.binding["quoteAssetId"], "usdc-uuid");
}

#[tokio::test]
async fn named_plan_does_not_defer_international_fee_cap_failures() {
    let mut queued = responses();
    queued[1] = response(json!([{
        "instrument_type":"SPOT", "maker_fee_rate":"0.003", "taker_fee_rate":"0.004"
    }]));
    let profile = profile(api(queued));
    let error = institutional::prepare_order_for_plan(
        &profile,
        "coinbaseinternational.create-order",
        &input(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "COINBASE_FEE_CAP_TOO_LOW");
}

#[tokio::test]
async fn international_cancel_links_original_order_and_requires_receipt_plus_terminal_get() {
    let api = api(responses());
    let profile = profile(api.clone());
    let prepared = profile
        .prepare("coinbaseinternational.create-order", &input())
        .await
        .unwrap();
    let client = prepared.binding["clientOrderId"]
        .as_str()
        .unwrap()
        .to_owned();
    let original_submission = profile.plan_submission(&prepared).await.unwrap().submission;
    let mut parent = record(prepared, original_submission);
    parent.id = "parent-order".into();
    profile
        .attach_execution_state(Arc::new(ParentState(parent)))
        .unwrap();
    api.queue
        .lock()
        .unwrap()
        .push_back(response(order_state(&client, "WORKING", "0.1")));
    let cancel_input = json!({"account":"intl-main","requestId":"cancel-9","orderId":"venue-order-9","clientOrderId":client,"instrument":"BTC-USDC"});
    let cancel = profile
        .prepare("coinbaseinternational.cancel-order", &cancel_input)
        .await
        .unwrap();
    assert_eq!(cancel.binding["operationId"], "parent-order");
    let planned = profile.plan_submission(&cancel).await.unwrap();
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::InternationalCancelOrderScoped {
            order_id: "venue-order-9".into(),
            portfolio_id: "portfolio-9".into()
        }
    );
    assert!(planned.request.exact_body.is_empty());
    let submission = planned.submission;
    let receipt = serde_json::to_vec(&order_state(&client, "DONE", "0")).unwrap();
    let cancel_record = OperationRecord {
        id: "cancel-operation".into(),
        request_id: "cancel-9".into(),
        fingerprint: "cancel-fingerprint".into(),
        fingerprint_version: 1,
        operation: cancel.operation.clone(),
        status: Status::Submitted,
        created_at: fin_domain::now(),
        updated_at: fin_domain::now(),
        input: cancel.input.clone(),
        preview: Some(cancel),
        approval: None,
        submission: Some(submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&receipt),"bodySha256":hex::encode(sha2::Sha256::digest(&receipt))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    api.queue
        .lock()
        .unwrap()
        .push_back(response(order_state(&client, "DONE", "0")));
    let result = profile
        .reconcile_submission(&cancel_record, &submission)
        .await
        .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert!(
        matches!(result.verified.unwrap().effects.as_slice(), [fin_domain::FinancialEffect::SubmissionCancellation { operation_id, .. }] if operation_id == "parent-order")
    );
}
