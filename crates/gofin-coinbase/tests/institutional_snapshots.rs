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
async fn exchange_scoped_snapshot_keeps_native_subaccount_id_and_separates_available_from_hold() {
    let (profile, api) = profile(
        CoinbaseProduct::Exchange,
        vec![
            json!([
                {"id":"currency-account-1","profile_id":"portfolio-17","currency":"USD","available":"3.25","hold":"1.75","balance":"5.00"}
            ]),
            json!({"overview":{"collateral_value":"0","withdrawal_restricted":false},"loans":[]}),
        ],
    );
    let snapshot = profile
        .snapshot_scoped(&cash_scope(CoinbaseProduct::Exchange, "USD"))
        .await
        .unwrap();
    assert!(snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(
        snapshot.state.holdings[0].amount.atoms.to_decimal(36),
        "3.25"
    );
    assert_eq!(
        snapshot.state.holdings[0].amount.asset.network,
        "coinbaseexchange"
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|line| line.contains("currency-account-1") && line.contains("held:1.75"))
    );
    assert!(matches!(
        api.seen.lock().unwrap().as_slice(),
        [
            CoinbaseEndpoint::ExchangeAccounts,
            CoinbaseEndpoint::ExchangeLoanLendingOverview
        ]
    ));
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
async fn international_scoped_snapshot_proves_enrolled_portfolio_and_separates_all_unavailable_amounts()
 {
    let (profile, api) = profile(
        CoinbaseProduct::International,
        vec![
            json!({"portfolios":[{"portfolio_id":"portfolio-17"}]}),
            json!({"summary":{"balance":10,"borrow":"0","accrued_interest":"0","rolling_debt":"0"},"balances":[
            {"asset_id":"asset-native-7","asset_uuid":"uuid-7","asset_name":"USDC","quantity":"10","hold":"1","transfer_hold":"2","pledged_collateral_quantity":"3","loan":"1","max_withdraw_amount":"4"}
        ],"positions":[]}),
        ],
    );
    let snapshot = profile
        .snapshot_scoped(&cash_scope(CoinbaseProduct::International, "USDC"))
        .await
        .unwrap();
    assert!(!snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(snapshot.state.holdings[0].amount.atoms.to_decimal(36), "4");
    assert_eq!(
        snapshot.state.holdings[0].amount.asset.network,
        "coinbaseinternational"
    );
    assert!(
        snapshot
            .pending
            .iter()
            .any(|line| line.contains("loan, collateral, or derivative exposure"))
    );
    let evidence = snapshot
        .coverage
        .iter()
        .find(|line| line.contains("asset-native-7") && line.contains(":symbol:"))
        .unwrap();
    for component in [
        "uuid-7",
        "available:4",
        "hold:1",
        "transfer_hold:2",
        "pledged:3",
        "loan_liability:1",
        "total:10",
    ] {
        assert!(
            evidence.contains(component),
            "missing {component} in {evidence}"
        );
    }
    assert!(matches!(api.seen.lock().unwrap().as_slice(), [
        CoinbaseEndpoint::InternationalAccounts,
        CoinbaseEndpoint::InternationalPortfolioDetail { portfolio_id }
    ] if portfolio_id == "portfolio-17"));
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

#[tokio::test]
async fn institutional_unscoped_cash_snapshot_is_honest_about_unmodeled_debt_and_positions() {
    let (profile, _) = profile(
        CoinbaseProduct::International,
        vec![
            json!({"portfolios":[{"portfolio_id":"portfolio-17"}]}),
            json!({"balances":[],"positions":[{"instrument_uuid":"perp-1","net_size":"1"}],"summary":{"borrow":2,"accrued_interest":0,"rolling_debt":0}}),
        ],
    );
    let snapshot = profile.snapshot().await.unwrap();
    assert!(!snapshot.complete);
    assert!(
        snapshot
            .pending
            .iter()
            .any(|line| line.contains("debt, collateral, and position exposure"))
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|line| line.contains("positions, loans, margin, and collateral"))
    );
}
