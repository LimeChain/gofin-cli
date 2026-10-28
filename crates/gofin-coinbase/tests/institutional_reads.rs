use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use gofin_domain::ResourceId;
use gofin_exchange::coinbase::{
    CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse, DispatchGuard,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

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
