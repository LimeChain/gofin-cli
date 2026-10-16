use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use gofin_domain::{OperationRecord, ResourceId, Status};
use gofin_exchange::coinbase::CoinbaseRoute;
use gofin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, DispatchGuard,
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
        _endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        _guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}
fn response(body: Value) -> CoinbaseResponse {
    CoinbaseResponse::new(200, serde_json::to_vec(&body).unwrap())
}
fn intl_profile(api: Arc<Api>) -> CoinbaseProfile {
    let transport: Arc<dyn CoinbaseApi> = api;
    CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "intl".into(),
            resource: ResourceId("coinbaseinternational:portfolio:portfolio-1".into()),
            credential_ref: "key".into(),
            physical_identity: "portfolio-1".into(),
            identity_verified: true,
            portfolio_id: Some("portfolio-1".into()),
            product: CoinbaseProduct::International,
        }],
        [("key".into(), transport)].into(),
    )
    .unwrap()
}

#[test]
fn international_configuration_routes_are_closed_and_mutating() {
    let cases = [
        (
            CoinbaseRoute::InternationalPortfolioCreate,
            "POST",
            "/api/v1/portfolios",
        ),
        (
            CoinbaseRoute::InternationalPortfolioUpdate {
                portfolio_id: "p/1".into(),
            },
            "PUT",
            "/api/v1/portfolios/p%2F1",
        ),
        (
            CoinbaseRoute::InternationalPortfolioPatch {
                portfolio_id: "p1".into(),
            },
            "PATCH",
            "/api/v1/portfolios/p1",
        ),
        (
            CoinbaseRoute::InternationalPortfolioMargin,
            "POST",
            "/api/v1/portfolios/margin",
        ),
        (
            CoinbaseRoute::InternationalCrossCollateral {
                portfolio_id: "p1".into(),
            },
            "POST",
            "/api/v1/portfolios/p1/cross-collateral-enabled",
        ),
        (
            CoinbaseRoute::InternationalAutoMargin {
                portfolio_id: "p1".into(),
            },
            "POST",
            "/api/v1/portfolios/p1/auto-margin-enabled",
        ),
        (
            CoinbaseRoute::ExchangeProfileTransfer {
                transfer_id: "t1".into(),
            },
            "GET",
            "/transfers/t1",
        ),
    ];

    for (route, method, path) in cases {
        assert_eq!(route.method(), method);
        assert_eq!(route.path().unwrap(), path);
        assert_eq!(route.mutating(), method != "GET");
    }
    let history = CoinbaseRoute::ExchangeProfileTransfers {
        query: gofin_exchange::coinbase::CoinbaseQuery {
            transfer_type: Some("internal_withdraw".into()),
            profile_id: Some("p1".into()),
            ..Default::default()
        },
    };
    assert_eq!(history.method(), "GET");
    assert_eq!(
        history.path().unwrap(),
        "/transfers?profile_id=p1&type=internal_withdraw"
    );
}

#[tokio::test]
async fn international_margin_is_prepared_exactly_and_reconciled_by_portfolio_readback() {
    let api = Arc::new(Api(Mutex::new(VecDeque::from([
        response(json!({"summary":{"margin_override":0.2}})),
        response(json!({"summary":{"margin_override":0.3}})),
    ]))));
    let profile = intl_profile(api);
    let input = json!({"account":"intl","requestId":"margin-1","portfolioId":"portfolio-1","marginOverride":"0.3"});
    let prepared = institutional::prepare(
        &profile,
        "coinbaseinternational.set-margin-override",
        &input,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prepared.binding["previousMargin"], "0.2");
    assert_eq!(
        prepared.binding["requestBodyExact"],
        "{\"margin_override\":\"0.3\",\"portfolio\":\"portfolio-1\"}"
    );
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        planned.request.endpoint,
        CoinbaseRoute::InternationalPortfolioMargin
    );
    assert_eq!(
        planned.request.exact_body,
        prepared.binding["requestBodyExact"]
    );
    let response_bytes =
        serde_json::to_vec(&json!({"portfolio":"portfolio-1","margin_override":"0.3"})).unwrap();
    let record = OperationRecord {
        id: "operation-1".into(),
        request_id: "margin-1".into(),
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
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&response_bytes),"bodySha256":hex::encode(sha2::Sha256::digest(&response_bytes))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let reconciliation = institutional::reconcile(&profile, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconciliation.status, Status::Completed);
    assert_eq!(
        reconciliation.evidence["readback"]["summary"]["margin_override"],
        0.3
    );
}

#[tokio::test]
async fn international_portfolio_admin_and_flag_operations_bind_to_enrolled_identity() {
    let empty = json!([]);
    let api = Arc::new(Api(Mutex::new(VecDeque::from([
        response(empty.clone()),
        response(
            json!([{"portfolio_id":"portfolio-1","name":"before","cross_collateral_enabled":false,"auto_margin_enabled":false}]),
        ),
        response(
            json!([{"portfolio_id":"portfolio-1","name":"before","cross_collateral_enabled":false,"auto_margin_enabled":false}]),
        ),
    ]))));
    let profile = intl_profile(api);
    let create = institutional::prepare(
        &profile,
        "coinbaseinternational.create-portfolio",
        &json!({"account":"intl","requestId":"create-1","name":"ops"}),
    )
    .await
    .unwrap()
    .unwrap();
    let create_plan = institutional::plan_submission(&profile, &create)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        create_plan.request.endpoint,
        CoinbaseRoute::InternationalPortfolioCreate
    );
    assert_eq!(create_plan.request.exact_body, "{\"name\":\"ops\"}");
    let flag = institutional::prepare(
        &profile,
        "coinbaseinternational.set-cross-collateral",
        &json!({"account":"intl","requestId":"cross-1","portfolioId":"portfolio-1","enabled":true}),
    )
    .await
    .unwrap()
    .unwrap();
    let flag_plan = institutional::plan_submission(&profile, &flag)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        flag_plan.request.endpoint,
        CoinbaseRoute::InternationalCrossCollateral {
            portfolio_id: "portfolio-1".into()
        }
    );
    assert_eq!(flag_plan.request.exact_body, "{\"enabled\":\"true\"}");
    assert_eq!(flag.binding["previousEnabled"], false);
}

#[tokio::test]
async fn uncertain_international_portfolio_create_is_unknown_and_not_replanned() {
    let api = Arc::new(Api(Mutex::new(VecDeque::from([response(json!([]))]))));
    let profile = intl_profile(api);
    let prepared = institutional::prepare(
        &profile,
        "coinbaseinternational.create-portfolio",
        &json!({"account":"intl","requestId":"create-uncertain","name":"ops"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&profile, &prepared)
        .await
        .unwrap()
        .unwrap();
    let record = OperationRecord {
        id: "operation-uncertain".into(),
        request_id: "create-uncertain".into(),
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
        submission: Some(planned.submission.clone()),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let reconciliation = institutional::reconcile(&profile, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reconciliation.status, Status::Unknown);
    assert_eq!(reconciliation.evidence["replay"], "forbidden");
}

#[tokio::test]
async fn portfolio_reconciliation_fails_closed_on_replaced_account_alias_and_bad_receipt_digest() {
    let api = Arc::new(Api(Mutex::new(VecDeque::from([response(json!([]))]))));
    let original = intl_profile(api.clone());
    let prepared = institutional::prepare(
        &original,
        "coinbaseinternational.create-portfolio",
        &json!({"account":"intl","requestId":"create-map-change","name":"ops"}),
    )
    .await
    .unwrap()
    .unwrap();
    let planned = institutional::plan_submission(&original, &prepared)
        .await
        .unwrap()
        .unwrap();
    let bytes =
        serde_json::to_vec(&json!({"portfolio":{"portfolio_id":"new-portfolio","name":"ops"}}))
            .unwrap();
    let mut record = OperationRecord {
        id: "operation-map-change".into(),
        request_id: "create-map-change".into(),
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
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(&bytes),"bodySha256":hex::encode(sha2::Sha256::digest(&bytes))}),
        ),
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let replacement: Arc<dyn CoinbaseApi> = api;
    let replaced = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "intl".into(),
            resource: ResourceId("coinbaseinternational:portfolio:replaced".into()),
            credential_ref: "key".into(),
            physical_identity: "replaced".into(),
            identity_verified: true,
            portfolio_id: Some("replaced".into()),
            product: CoinbaseProduct::International,
        }],
        [("key".into(), replacement)].into(),
    )
    .unwrap();
    let result = institutional::reconcile(&replaced, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(
        result.evidence["reason"]
            .as_str()
            .unwrap()
            .contains("Current enrolled")
    );

    record.dispatch_evidence =
        Some(json!({"httpStatus":200,"bodyHex":hex::encode(&bytes),"bodySha256":"00"}));
    let result = institutional::reconcile(&original, &record, &planned.submission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(
        result.evidence["reason"]
            .as_str()
            .unwrap()
            .contains("digest")
    );
}

#[tokio::test]
async fn international_loan_read_is_scoped_to_exact_enrolled_portfolio() {
    let native = json!([{"portfolio_id":"portfolio-1","asset":"BTC","total_loan":"1.25"}]);
    let api = Arc::new(Api(Mutex::new(VecDeque::from([response(native.clone())]))));
    let profile = intl_profile(api);
    let result = institutional::read(
        &profile,
        "coinbaseinternational.get-loans",
        &json!({"account":"intl","portfolioId":"portfolio-1"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, native);
    assert!(
        institutional::read(
            &profile,
            "coinbaseinternational.get-loans",
            &json!({"account":"intl","portfolioId":"other"})
        )
        .await
        .is_err()
    );
}
