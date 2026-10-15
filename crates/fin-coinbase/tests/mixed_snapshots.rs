use async_trait::async_trait;
use fin_coinbase::{
    CoinbaseAccount, CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseProduct,
    CoinbaseProfile, CoinbaseRequest, CoinbaseResponse,
};
use fin_domain::{AssetId, ResourceId, SnapshotScope};
use fin_exchange::{ExchangeProfile, coinbase::DispatchGuard};
use serde_json::json;
use std::sync::Arc;

struct Api;
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        route: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        assert!(request.body.is_none());
        let body = match route.path()?.as_str() {
            "/accounts" => {
                json!([{ "id":"cash-e", "profile_id":"e", "currency":"USDC", "available":"3", "hold":"2", "balance":"5" }])
            }
            "/loans/lending-overview" => {
                json!({"overview":{"collateral_value":"0","withdrawal_restricted":false},"loans":[]})
            }
            "/api/v1/portfolios" => json!({"portfolios":[{"portfolio_id":"i"}]}),
            "/api/v1/portfolios/i/detail" => {
                json!({"summary":{"borrow":"0","accrued_interest":"0","rolling_debt":"0"},"balances":[{"asset_id":"native-i","asset_uuid":"uuid-i","asset_name":"USDC","quantity":"7","hold":"1","transfer_hold":"2","pledged_collateral_quantity":"0","loan":"0"}],"positions":[]})
            }
            other => panic!("unexpected native route {other}"),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}
fn profile() -> CoinbaseProfile {
    let accounts = [
        (CoinbaseProduct::Exchange, "e"),
        (CoinbaseProduct::International, "i"),
    ]
    .into_iter()
    .map(|(product, id)| CoinbaseAccount {
        name: id.into(),
        resource: ResourceId(format!("{}:portfolio:{id}", product.provider())),
        credential_ref: id.into(),
        physical_identity: id.into(),
        identity_verified: true,
        portfolio_id: Some(id.into()),
        product,
    })
    .collect();
    let api: Arc<dyn CoinbaseApi> = Arc::new(Api);
    CoinbaseProfile::new(
        accounts,
        [("e".into(), api.clone()), ("i".into(), api)].into(),
    )
    .unwrap()
}
#[tokio::test]
async fn mixed_cash_snapshots_keep_identical_symbols_in_distinct_product_resources() {
    let profile = profile();
    let scope = SnapshotScope {
        assets: ["coinbaseexchange", "coinbaseinternational"]
            .into_iter()
            .map(|network| AssetId {
                network: network.into(),
                reference: "USDC".into(),
                decimals: 36,
            })
            .collect(),
        ..Default::default()
    };
    let snapshot = profile.snapshot_scoped(&scope).await.unwrap();
    assert!(snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 2);
    let exchange = snapshot
        .state
        .holdings
        .iter()
        .find(|row| row.amount.asset.network == "coinbaseexchange")
        .unwrap();
    let international = snapshot
        .state
        .holdings
        .iter()
        .find(|row| row.amount.asset.network == "coinbaseinternational")
        .unwrap();
    assert_eq!(exchange.amount.atoms.to_decimal(36), "3");
    assert_eq!(international.amount.atoms.to_decimal(36), "4");
    assert_ne!(exchange.resource, international.resource);
    assert!(snapshot.coverage.iter().any(|line| line.contains("cash-e")));
    assert!(snapshot.coverage.iter().any(|line| line.contains("uuid-i")));
}
#[tokio::test]
async fn full_mixed_snapshot_observes_cash_but_discloses_unmodeled_financial_state() {
    let snapshot = profile().snapshot().await.unwrap();
    assert!(!snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 2);
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|line| line.contains("unmodeled lending and derivatives"))
    );
}
