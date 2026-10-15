use async_trait::async_trait;
use fin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use fin_domain::ResourceId;
use fin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbasePagination, CoinbaseRequest,
    CoinbaseResponse, CoinbaseRoute, DispatchGuard,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct ReadApi {
    body: Vec<u8>,
    seen: Mutex<Vec<(CoinbaseEndpoint, Option<String>)>>,
}
#[async_trait]
impl CoinbaseApi for ReadApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none(), "reads never receive a dispatch guard");
        self.seen.lock().unwrap().push((endpoint, request.body));
        Ok(CoinbaseResponse {
            status: 200,
            body: self.body.clone(),

            pagination: Default::default(),
        })
    }
}
fn profile(product: CoinbaseProduct, api: Arc<ReadApi>) -> CoinbaseProfile {
    let portfolio = "portfolio-17";
    let account = CoinbaseAccount {
        name: "institutional-test".into(),
        resource: ResourceId(format!("{}:portfolio:{portfolio}", product.provider())),
        credential_ref: "test-credential".into(),
        physical_identity: portfolio.into(),
        identity_verified: true,
        portfolio_id: Some(portfolio.into()),
        product,
    };
    let transport: Arc<dyn CoinbaseApi> = api;
    CoinbaseProfile::new(
        vec![account],
        [("test-credential".into(), transport)].into(),
    )
    .unwrap()
}
fn api(body: Value) -> Arc<ReadApi> {
    Arc::new(ReadApi {
        body: serde_json::to_vec(&body).unwrap(),
        seen: Mutex::new(Vec::new()),
    })
}

struct PageApi {
    responses: Mutex<VecDeque<CoinbaseResponse>>,
    seen: Mutex<Vec<CoinbaseEndpoint>>,
}
#[async_trait]
impl CoinbaseApi for PageApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        assert!(request.body.is_none());
        assert!(guard.is_none());
        self.seen.lock().unwrap().push(endpoint);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(CoinbaseError::Transport)
    }
}

#[tokio::test]
async fn exchange_fill_read_uses_institutional_transport_and_preserves_native_json() {
    let native =
        json!([{"trade_id":"t-1","profile_id":"portfolio-17","price":"1.23000000000000000001"}]);
    let api = api(native.clone());
    let profile = profile(CoinbaseProduct::Exchange, api.clone());
    let result = institutional::read(
        &profile,
        "coinbaseexchange.get-fills",
        &json!({"account":"institutional-test"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        result,
        json!({"data":native,"pagination":{"after":null,"before":null}})
    );
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].0,
        CoinbaseRoute::ExchangeFillsFiltered {
            query: fin_exchange::coinbase::CoinbaseQuery {
                profile_id: Some("portfolio-17".into()),
                ..Default::default()
            }
        }
    );
    assert_eq!(seen[0].1, None);
}

#[tokio::test]
async fn exchange_fill_read_exhausts_header_cursor_pages_without_losing_native_rows() {
    let first = json!([{"trade_id":1,"profile_id":"portfolio-17"}]);
    let second = json!([{"trade_id":2,"profile_id":"portfolio-17"}]);
    let mut page_one = CoinbaseResponse::new(200, serde_json::to_vec(&first).unwrap());
    page_one.pagination = CoinbasePagination {
        after: Some("cursor-one".into()),
        before: None,
    };
    let page_two = CoinbaseResponse::new(200, serde_json::to_vec(&second).unwrap());
    let api = Arc::new(PageApi {
        responses: Mutex::new(VecDeque::from([page_one, page_two])),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "institutional-test".into(),
            resource: ResourceId("coinbaseexchange:portfolio:portfolio-17".into()),
            credential_ref: "test-credential".into(),
            physical_identity: "portfolio-17".into(),
            identity_verified: true,
            portfolio_id: Some("portfolio-17".into()),
            product: CoinbaseProduct::Exchange,
        }],
        [("test-credential".into(), transport)].into(),
    )
    .unwrap();
    let output = institutional::read(
        &profile,
        "coinbaseexchange.get-fills",
        &json!({"account":"institutional-test"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output["data"], json!([first[0], second[0]]));
    assert_eq!(output["pagination"]["after"], Value::Null);
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        matches!(&seen[0], CoinbaseRoute::ExchangeFillsFiltered { query } if query.after.is_none())
    );
    assert!(
        matches!(&seen[1], CoinbaseRoute::ExchangeFillsFiltered { query } if query.after.as_deref() == Some("cursor-one"))
    );
    assert!(api.responses.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exchange_order_history_is_profile_scoped_and_exhausts_cursor_pages() {
    let first = json!([{"id":"order-1","profile_id":"portfolio-17","product_id":"BTC-USD"}]);
    let second = json!([{"id":"order-2","profile_id":"portfolio-17","product_id":"BTC-USD"}]);
    let mut page_one = CoinbaseResponse::new(200, serde_json::to_vec(&first).unwrap());
    page_one.pagination.after = Some("order-cursor-1".into());
    let page_two = CoinbaseResponse::new(200, serde_json::to_vec(&second).unwrap());
    let api = Arc::new(PageApi {
        responses: Mutex::new(VecDeque::from([page_one, page_two])),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "institutional-test".into(),
            resource: ResourceId("coinbaseexchange:portfolio:portfolio-17".into()),
            credential_ref: "test-credential".into(),
            physical_identity: "portfolio-17".into(),
            identity_verified: true,
            portfolio_id: Some("portfolio-17".into()),
            product: CoinbaseProduct::Exchange,
        }],
        [("test-credential".into(), transport)].into(),
    )
    .unwrap();
    let output = institutional::read(
        &profile,
        "coinbaseexchange.get-orders",
        &json!({"account":"institutional-test","productId":"BTC-USD","status":"open","limit":100}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output["data"], json!([first[0], second[0]]));
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        matches!(&seen[0], CoinbaseRoute::ExchangeOrdersList { query } if query.profile_id.as_deref() == Some("portfolio-17") && query.product_id.as_deref() == Some("BTC-USD") && query.status.as_deref() == Some("open") && query.limit == Some(100) && query.after.is_none())
    );
    assert!(
        matches!(&seen[1], CoinbaseRoute::ExchangeOrdersList { query } if query.profile_id.as_deref() == Some("portfolio-17") && query.after.as_deref() == Some("order-cursor-1"))
    );
}

#[tokio::test]
async fn exchange_order_history_rejects_rows_from_another_profile() {
    let api = api(json!([{"id":"order-1","profile_id":"other-profile"}]));
    let profile = profile(CoinbaseProduct::Exchange, api.clone());
    let result = institutional::read(
        &profile,
        "coinbaseexchange.get-orders",
        &json!({"account":"institutional-test"}),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(api.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn international_fills_are_scoped_to_the_enrolled_portfolio_path() {
    let native = json!({"results":[],"pagination":{"result_limit":100,"result_offset":0}});
    let api = api(native.clone());
    let profile = profile(CoinbaseProduct::International, api.clone());
    let result = institutional::read(
        &profile,
        "coinbaseinternational.get-fills",
        &json!({"account":"institutional-test"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        result,
        json!({"data":native,"pagination":{"after":null,"before":null}})
    );
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].0,
        CoinbaseRoute::InternationalPortfolioFillsFiltered {
            portfolio_id: "portfolio-17".into(),
            query: fin_exchange::coinbase::CoinbaseQuery {
                result_limit: Some(100),
                ..Default::default()
            }
        }
    );
    assert_eq!(seen[0].0.method(), "GET");
    assert!(!seen[0].0.mutating());
}

#[tokio::test]
async fn institutional_reads_reject_unverified_or_cross_product_accounts_before_io() {
    let api = api(json!([]));
    let mut invalid = CoinbaseAccount {
        name: "institutional-test".into(),
        resource: ResourceId("coinbaseexchange:portfolio:portfolio-17".into()),
        credential_ref: "test-credential".into(),
        physical_identity: "portfolio-17".into(),
        identity_verified: false,
        portfolio_id: Some("portfolio-17".into()),
        product: CoinbaseProduct::Exchange,
    };
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let p = CoinbaseProfile::new(
        vec![invalid.clone()],
        [("test-credential".into(), transport.clone())].into(),
    )
    .unwrap();
    let denied = institutional::read(
        &p,
        "coinbaseexchange.get-fills",
        &json!({"account":"institutional-test"}),
    )
    .await;
    assert!(denied.is_err());
    assert!(api.seen.lock().unwrap().is_empty());
    invalid.identity_verified = true;
    let p = CoinbaseProfile::new(
        vec![invalid],
        [("test-credential".into(), transport)].into(),
    )
    .unwrap();
    let wrong = institutional::read(
        &p,
        "coinbaseexchange.get-fills",
        &json!({"account":"not-enrolled"}),
    )
    .await;
    assert!(wrong.is_err());
    assert!(api.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exchange_order_read_rejects_a_response_from_another_profile() {
    let api = api(json!({"id":"order-1","profile_id":"different-profile"}));
    let profile = profile(CoinbaseProduct::Exchange, api.clone());
    let result = institutional::read(
        &profile,
        "coinbaseexchange.get-order",
        &json!({"account":"institutional-test","id":"order-1"}),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(api.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn order_reads_reject_a_native_order_id_mismatch_even_with_correct_account_scope() {
    let exchange_api = api(json!({
        "id":"different-order",
        "profile_id":"portfolio-17"
    }));
    let exchange_profile = profile(CoinbaseProduct::Exchange, exchange_api);
    assert!(
        institutional::read(
            &exchange_profile,
            "coinbaseexchange.get-order",
            &json!({"account":"institutional-test","id":"requested-order"}),
        )
        .await
        .is_err()
    );

    let international_api = api(json!({
        "order_id":"different-order",
        "portfolio_id":"portfolio-17"
    }));
    let international_profile = profile(CoinbaseProduct::International, international_api);
    assert!(
        institutional::read(
            &international_profile,
            "coinbaseinternational.get-order",
            &json!({"account":"institutional-test","id":"requested-order"}),
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn international_transfer_history_is_portfolio_filtered_and_native_shape_is_preserved() {
    let native = json!({
        "results":[{"transfer_uuid":"transfer-1","transfer_type":"INTERNAL","status":"PROCESSED","amount":"2.5","asset":"USDC","from_portfolio":{"uuid":"portfolio-17"},"to_portfolio":{"uuid":"portfolio-18"}}],
        "pagination":{"result_limit":50,"result_offset":0}
    });
    let api = api(native.clone());
    let profile = profile(CoinbaseProduct::International, api.clone());
    let result = institutional::read(
        &profile,
        "coinbaseinternational.get-transfers",
        &json!({"account":"institutional-test","type":"INTERNAL","limit":50}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, native);
    let seen = api.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].0,
        CoinbaseRoute::InternationalTransfers {
            query: fin_exchange::coinbase::CoinbaseQuery {
                transfer_type: Some("INTERNAL".into()),
                result_limit: Some(50),
                portfolios: Some("portfolio-17".into()),
                ..Default::default()
            }
        }
    );
}

#[tokio::test]
async fn international_transfer_history_rejects_unbound_portfolio_rows() {
    let api = api(
        json!({"results":[{"transfer_uuid":"transfer-1","from_portfolio":{"uuid":"other"},"to_portfolio":{"uuid":"another"}}]}),
    );
    let profile = profile(CoinbaseProduct::International, api);
    let result = institutional::read(
        &profile,
        "coinbaseinternational.get-transfers",
        &json!({"account":"institutional-test"}),
    )
    .await;
    assert!(result.is_err());
}
