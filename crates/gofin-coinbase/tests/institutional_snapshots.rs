use async_trait::async_trait;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::{AssetId, ResourceId, SnapshotScope};
use gofin_exchange::{
    ExchangeProfile,
    coinbase::{
        CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseRequest, CoinbaseResponse,
        DispatchGuard,
    },
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Api {
    responses: Mutex<VecDeque<Value>>,
    seen: Mutex<Vec<CoinbaseEndpoint>>,
}
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        assert!(request.body.is_none());
        self.seen.lock().unwrap().push(endpoint);
        let body = serde_json::to_vec(
            &self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(CoinbaseError::Transport)?,
        )
        .unwrap();
        Ok(CoinbaseResponse::new(200, body))
    }
}
fn account(product: CoinbaseProduct) -> CoinbaseAccount {
    CoinbaseAccount {
        name: "institutional".into(),
        resource: ResourceId(format!("{}:portfolio:portfolio-17", product.provider())),
        credential_ref: "key".into(),
        physical_identity: "portfolio-17".into(),
        identity_verified: true,
        portfolio_id: Some("portfolio-17".into()),
        product,
    }
}
fn profile(product: CoinbaseProduct, responses: Vec<Value>) -> (CoinbaseProfile, Arc<Api>) {
    let api = Arc::new(Api {
        responses: Mutex::new(responses.into()),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn CoinbaseApi> = api.clone();
    let profile =
        CoinbaseProfile::new(vec![account(product)], [("key".into(), transport)].into()).unwrap();
    (profile, api)
}
fn cash_scope(product: CoinbaseProduct, reference: &str) -> SnapshotScope {
    SnapshotScope {
        resources: vec![],
        networks: vec![product.provider().into()],
        assets: vec![AssetId {
            network: product.provider().into(),
            reference: reference.into(),
            decimals: 36,
        }],
    }
}

#[tokio::test]
async fn exchange_snapshot_rejects_wrong_profile_and_inconsistent_native_totals() {
    for row in [
        json!({"id":"a1","profile_id":"other-profile","currency":"USD","available":"3","hold":"2","balance":"5"}),
        json!({"id":"a1","profile_id":"portfolio-17","currency":"USD","available":"3","hold":"2","balance":"6"}),
    ] {
        let (profile, _) = profile(CoinbaseProduct::Exchange, vec![json!([row])]);
        assert!(
            profile
                .snapshot_scoped(&cash_scope(CoinbaseProduct::Exchange, "USD"))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn exchange_snapshot_does_not_infer_zero_for_a_missing_currency_account() {
    let (profile, _) = profile(
        CoinbaseProduct::Exchange,
        vec![
            json!([
                {"id":"currency-account-1","profile_id":"portfolio-17","currency":"EUR","available":"4","hold":"0","balance":"4"}
            ]),
            json!({"overview":{"collateral_value":"0","withdrawal_restricted":false},"loans":[]}),
        ],
    );
    let snapshot = profile
        .snapshot_scoped(&cash_scope(CoinbaseProduct::Exchange, "USD"))
        .await
        .unwrap();
    assert!(!snapshot.complete);
    assert!(snapshot.state.holdings.is_empty());
    assert!(
        snapshot
            .pending
            .iter()
            .any(|line| line.contains("no zero balance was inferred"))
    );
}

#[tokio::test]
async fn international_snapshot_rejects_wrong_or_ambiguous_portfolio_listing_before_detail() {
    for portfolios in [
        json!([{"portfolio_id":"other"}]),
        json!([{"portfolio_id":"portfolio-17"},{"portfolio_id":"portfolio-17"}]),
    ] {
        let (profile, api) = profile(
            CoinbaseProduct::International,
            vec![json!({"portfolios":portfolios})],
        );
        assert!(
            profile
                .snapshot_scoped(&cash_scope(CoinbaseProduct::International, "USDC"))
                .await
                .is_err()
        );
        assert_eq!(api.seen.lock().unwrap().len(), 1);
    }
}
