use super::profile::{
    coinbase_order_boundary, decimal_le, decimal_product_atoms_exact, decimal_product_ge,
    decimal_product_ge_sum, decimal_product_le, edit_order_configuration_matches, exact_atoms,
    validate_coinbase_boundary,
};
use super::*;
use async_trait::async_trait;
use gofin_domain::{Adapter, SnapshotScope};
use gofin_exchange::ExchangeProfile;
use gofin_exchange::coinbase::{CoinbaseError, CoinbaseQuery, DispatchGuard};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[test]
fn coinbase_transport_errors_have_distinct_secret_free_fin_codes() {
    let cases = [
        (
            CoinbaseError::GuardRejected,
            "COINBASE_DISPATCH_GUARD_REJECTED",
        ),
        (
            CoinbaseError::InvalidCredential,
            "COINBASE_CREDENTIAL_INVALID",
        ),
        (CoinbaseError::InvalidRequest, "COINBASE_ROUTE_INVALID"),
        (CoinbaseError::Transport, "COINBASE_TRANSPORT_UNCERTAIN"),
    ];
    for (error, expected_code) in cases {
        let mapped = super::profile::coinbase_request_error(error);
        assert_eq!(mapped.code, expected_code);
        assert!(!mapped.message.to_ascii_lowercase().contains("secret"));
        assert!(!mapped.message.contains("api_key"));
    }
}

#[derive(Default)]
struct MockApi {
    calls: Mutex<Vec<(CoinbaseEndpoint, Option<String>)>>,
}
#[async_trait]
impl CoinbaseApi for MockApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        _guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.calls.lock().unwrap().push((endpoint, request.body));
        Ok(CoinbaseResponse::new(200,br#"{"accounts":[{"currency":"USDC","available_balance":{"value":"0.123456789012345678901234567890123456"}}]}"#.to_vec()))
    }
}
fn profile(api: Arc<MockApi>) -> CoinbaseProfile {
    let account = CoinbaseAccount {
        name: "primary".into(),
        resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
        credential_ref: "credential:1".into(),
        physical_identity: "p-1".into(),
        identity_verified: true,
        portfolio_id: Some("p-1".into()),
        product: CoinbaseProduct::Advanced,
    };
    CoinbaseProfile::new(
        vec![account],
        BTreeMap::from([("credential:1".into(), api as Arc<dyn CoinbaseApi>)]),
    )
    .unwrap()
}

#[test]
fn linked_edit_requires_unchanged_predecessor_size_price_side_and_portfolio() {
    let order = json!({
        "side":"BUY",
        "retail_portfolio_id":"p-1",
        "order_configuration":{"limit_limit_gtc":{"base_size":"2.00","limit_price":"10.25"}}
    });
    assert!(edit_order_configuration_matches(
        &order,
        "2.00",
        "10.25",
        "BUY",
        Some("p-1")
    ));
    assert!(!edit_order_configuration_matches(
        &order,
        "3.00",
        "10.25",
        "BUY",
        Some("p-1")
    ));
    assert!(!edit_order_configuration_matches(
        &order,
        "2.00",
        "10.50",
        "BUY",
        Some("p-1")
    ));
    assert!(!edit_order_configuration_matches(
        &order,
        "2.00",
        "10.25",
        "SELL",
        Some("p-1")
    ));
    assert!(!edit_order_configuration_matches(
        &order,
        "2.00",
        "10.25",
        "BUY",
        Some("p-2")
    ));
}

struct PreviewPrices;
#[async_trait]
impl UsdPriceSource for PreviewPrices {
    async fn quote(&self, _asset: &gofin_domain::AssetId) -> gofin_domain::Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: gofin_domain::now(),
            observed_at: gofin_domain::now(),
            source: "synthetic preview test quote".into(),
        })
    }
}

struct DepositAddressConfigApi;
#[async_trait]
impl CoinbaseApi for DepositAddressConfigApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        _guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let body = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":"p-1","can_view":true,"can_trade":true,"can_transfer":true})
            }
            CoinbaseRoute::Account { account_id } => {
                assert_eq!(account_id, "btc-account");
                json!({"account":{"uuid":"btc-account","currency":{"code":"BTC"},"portfolio_id":"p-1","ready":true,"active":true}})
            }
            other => panic!("unexpected configuration preparation route: {other:?}"),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}

struct AbsentUsdcSnapshotApi;
#[async_trait]
impl CoinbaseApi for AbsentUsdcSnapshotApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        _guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let body = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":"p-1","can_view":true,"can_trade":true})
            }
            CoinbaseRoute::LegacyCurrencies => json!({"data":[{"id":"USD"}]}),
            CoinbaseRoute::LegacyCryptoCurrencies => json!({"data":[{"id":"USDC"}]}),
            CoinbaseRoute::Accounts { .. } => json!({"accounts":[],"has_next":false}),
            CoinbaseRoute::HistoricalOrders { .. } => json!({"orders":[],"has_next":false}),
            other => panic!("unexpected scoped snapshot route {other:?}"),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}

#[tokio::test]
async fn scoped_snapshot_proves_zero_for_metadata_known_currency_without_account_row() {
    let api: Arc<dyn CoinbaseApi> = Arc::new(AbsentUsdcSnapshotApi);
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "snapshot-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("snapshot-key".into(), api)]),
    )
    .unwrap();
    let scope = SnapshotScope {
        resources: vec![gofin_domain::ResourceId("coinbase:portfolio:p-1".into())],
        networks: vec!["coinbase".into()],
        assets: vec![gofin_domain::AssetId {
            network: "coinbase".into(),
            reference: "USDC".into(),
            decimals: 36,
        }],
    };
    let snapshot = profile.snapshot_scoped(&scope).await.unwrap();
    assert!(snapshot.complete);
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(snapshot.state.holdings[0].amount.asset.reference, "USDC");
    assert!(snapshot.state.holdings[0].amount.atoms.is_zero());
    assert!(
        snapshot
            .coverage
            .contains(&"coinbase:portfolio:p-1".to_owned())
    );
    assert!(snapshot.coverage.iter().any(|row| row.contains(
        "coinbase:currency-account-absent:USDC:zero-available-after-complete-enumeration"
    )));
    assert_eq!(
        profile.connections()[0].network.as_deref(),
        Some("coinbase")
    );
}

#[tokio::test]
async fn address_creation_is_human_configuration_with_exact_account_binding() {
    let api: Arc<dyn CoinbaseApi> = Arc::new(DepositAddressConfigApi);
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "address-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("address-key".into(), api)]),
    )
    .unwrap();
    let definition = profile
        .definitions()
        .into_iter()
        .find(|d| d.name == "coinbase.create-deposit-address")
        .unwrap();
    assert_eq!(definition.kind, gofin_domain::OperationKind::Configuration);
    assert_eq!(
        definition.execution_actor,
        gofin_domain::ExecutionActor::HumanInteractive
    );
    let adapter = gofin_exchange::ExchangeAdapter::new(profile);
    let prepared = adapter
        .prepare_configuration(
            "coinbase.create-deposit-address",
            &json!({"account":"primary","accountId":"btc-account","requestId":"receive-btc-1"}),
        )
        .await
        .unwrap();
    assert_eq!(prepared.binding["currency"], "BTC");
    assert_eq!(prepared.binding["currencyAccountId"], "btc-account");
    assert!(prepared.intent.debits.is_empty());
    assert!(
        prepared
            .warnings
            .iter()
            .any(|warning| warning.contains("never replayed"))
    );
}

#[tokio::test]
async fn account_reads_keep_exact_decimal_strings_and_reject_unknown_parameters() {
    let api = Arc::new(MockApi::default());
    let profile = profile(api.clone());
    let response = profile
        .read(
            "coinbase.get-accounts",
            &json!({"account":"primary","cursor":"abc","limit":10}),
        )
        .await
        .unwrap();
    assert_eq!(
        response["accounts"][0]["available_balance"]["value"].as_str(),
        Some("0.123456789012345678901234567890123456")
    );
    {
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0].0,
            CoinbaseRoute::Accounts {
                query: CoinbaseQuery {
                    limit: Some(10),
                    ..
                }
            }
        ));
    }
    let err = profile
        .read(
            "coinbase.get-accounts",
            &json!({"account":"primary","arbitrary":{"path":"/anything"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "INVALID_INPUT");
    assert_eq!(api.calls.lock().unwrap().len(), 1);
}

#[test]
fn order_schema_hides_multi_child_configs_without_an_authoritative_child_fill_graph() {
    let definition = contracts::definitions()
        .into_iter()
        .find(|definition| definition.name == "coinbase.create-order")
        .unwrap();
    let variants = definition.input_schema["properties"]["orderConfiguration"]["oneOf"]
        .as_array()
        .unwrap();
    let names = variants
        .iter()
        .filter_map(|variant| variant["required"].as_array()?.first()?.as_str())
        .collect::<Vec<_>>();
    assert!(!names.iter().any(|name| {
        matches!(
            *name,
            "twap_limit_gtd" | "trigger_bracket_gtc" | "trigger_bracket_gtd"
        )
    }));
    assert!(names.contains(&"limit_limit_gtc"));
}

#[test]
fn every_discovered_operation_has_a_closed_schema_and_unique_name() {
    let defs = contracts::definitions();
    let names = defs
        .iter()
        .map(|d| d.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), defs.len());
    assert!(defs.len() >= 25);
    for definition in defs {
        assert_eq!(
            definition.input_schema["additionalProperties"],
            Value::Bool(false),
            "{}",
            definition.name
        );
        assert!(
            definition.input_schema["properties"].is_object(),
            "{}",
            definition.name
        );
    }
}

#[test]
fn batch_cancel_and_cfm_setting_are_discovered_with_closed_human_configuration_contracts() {
    let defs = contracts::definitions();
    let batch = defs
        .iter()
        .find(|d| d.name == "coinbase.cancel-orders")
        .unwrap();
    assert_eq!(batch.input_schema["properties"]["orders"]["maxItems"], 100);
    assert_eq!(
        batch.input_schema["properties"]["orders"]["items"]["additionalProperties"],
        false
    );
    let setting = defs
        .iter()
        .find(|d| d.name == "coinbase.set-cfm-margin-setting")
        .unwrap();
    assert_eq!(setting.kind, gofin_domain::OperationKind::Configuration);
    assert_eq!(
        setting.execution_actor,
        gofin_domain::ExecutionActor::HumanInteractive
    );
    assert_eq!(
        setting.input_schema["properties"]["setting"]["enum"],
        json!([
            "INTRADAY_MARGIN_SETTING_STANDARD",
            "INTRADAY_MARGIN_SETTING_INTRADAY"
        ])
    );
    assert_eq!(setting.input_schema["additionalProperties"], false);
}

#[test]
fn order_amounts_preserve_exact_atoms_and_reject_rounding() {
    assert_eq!(exact_atoms("1.00000001", 8).unwrap().as_str(), "100000001");
    assert_eq!(exact_atoms("0.00000001", 8).unwrap().as_str(), "1");
    assert!(exact_atoms("0.000000001", 8).is_err());
    assert!(decimal_le("1.000000000000000001", "1.000000000000000002").unwrap());
    assert!(decimal_product_le("0.1", "0.2", "0.02").unwrap());
    assert!(!decimal_product_le("0.1", "0.2001", "0.02").unwrap());
}

#[test]
fn fill_quote_value_multiplication_never_truncates_accounting_atoms() {
    assert_eq!(
        decimal_product_atoms_exact("0.01", "0.25", 36)
            .unwrap()
            .as_str(),
        exact_atoms("0.0025", 36).unwrap().as_str()
    );
    assert!(
        decimal_product_atoms_exact("0.00000000000000000001", "0.00000000000000000001", 36)
            .is_err()
    );
}

#[test]
fn price_derived_minimum_receive_compares_exact_products() {
    assert!(decimal_product_ge("0.02", "100", "2").unwrap());
    assert!(!decimal_product_ge("0.019", "100", "2").unwrap());
    assert!(decimal_product_le("25", "0.096", "2.40").unwrap());
    assert!(decimal_product_ge_sum("2", "50", "60", "1").unwrap());
    assert!(!decimal_product_ge_sum("2", "50", "100", "1").unwrap());
}

#[test]
fn boundary_candidate_cannot_lower_the_approved_receipt_floor() {
    let resource = gofin_domain::ResourceId("coinbase:portfolio:p-1".into());
    let base = profile_asset("HBAR");
    let _quote = profile_asset("USD");
    let mut prepared = gofin_domain::PreparedOperation {
        operation: "coinbase.create-order".into(),
        input: json!({
            "account":"primary","requestId":"floor-test","productId":"HBAR-USD",
            "side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"2"}},
            "maximumSpend":"2","minimumReceive":"60","maximumFee":"1"
        }),
        binding: json!({
            "account":"primary","physicalIdentity":"p-1","portfolioId":"p-1",
            "productId":"HBAR-USD","baseCurrencyId":"HBAR","quoteCurrencyId":"USD"
        }),
        summary: "test".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: gofin_domain::FinancialIntent {
            debits: vec![gofin_domain::ResourceAmount {
                resource: resource.clone(),
                amount: gofin_domain::Amount {
                    asset: base.clone(),
                    atoms: exact_atoms("2", 36).unwrap(),
                },
            }],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![resource],
        },
        operation_version: 1,
        action_deadline: None,
    };
    let approved = coinbase_order_boundary(
        &prepared,
        &json!({
            "maximumQuantity":"2","minimumPrice":"50","maximumFees":{"USD":"1"}
        }),
    )
    .unwrap();
    assert!(validate_coinbase_boundary(&approved, &prepared).is_ok());
    prepared.input["minimumReceive"] = json!("55");
    assert_eq!(
        validate_coinbase_boundary(&approved, &prepared)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
}

#[test]
fn price_bounded_fok_orders_keep_the_limit_and_configuration_in_authority() {
    let resource = gofin_domain::ResourceId("coinbase:portfolio:p-1".into());
    let base = profile_asset("HBAR");
    let mut prepared = gofin_domain::PreparedOperation {
        operation: "coinbase.create-order".into(),
        input: json!({
            "account":"primary","requestId":"fok-test","productId":"HBAR-USD",
            "side":"SELL","orderConfiguration":{"limit_limit_fok":{"base_size":"2","limit_price":"50"}},
            "maximumSpend":"2","minimumReceive":"97","maximumFee":"1"
        }),
        binding: json!({
            "account":"primary","physicalIdentity":"p-1","portfolioId":"p-1",
            "productId":"HBAR-USD","baseCurrencyId":"HBAR","quoteCurrencyId":"USD"
        }),
        summary: "test".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: gofin_domain::FinancialIntent {
            debits: vec![gofin_domain::ResourceAmount {
                resource: resource.clone(),
                amount: gofin_domain::Amount {
                    asset: base,
                    atoms: exact_atoms("2", 36).unwrap(),
                },
            }],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![resource],
        },
        operation_version: 1,
        action_deadline: None,
    };
    let approved = coinbase_order_boundary(
        &prepared,
        &json!({"maximumQuantity":"2","minimumPrice":"49","maximumFees":{"USD":"1"}}),
    )
    .unwrap();
    assert_eq!(approved.fixed["configuration"], "limit_limit_fok");
    assert!(validate_coinbase_boundary(&approved, &prepared).is_ok());

    prepared.input["orderConfiguration"]["limit_limit_fok"]["limit_price"] = json!("48");
    assert_eq!(
        validate_coinbase_boundary(&approved, &prepared)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
    prepared.input["orderConfiguration"] = json!({
        "limit_limit_gtc":{"base_size":"2","limit_price":"50"}
    });
    assert_eq!(
        validate_coinbase_boundary(&approved, &prepared)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
}

#[test]
fn stop_limit_trigger_controls_remain_fixed_inside_a_price_bounded_authority() {
    let resource = gofin_domain::ResourceId("coinbase:portfolio:p-1".into());
    let prepared = gofin_domain::PreparedOperation {
        operation: "coinbase.create-order".into(),
        input: json!({"account":"primary","requestId":"stop-test","productId":"HBAR-USD","side":"SELL","orderConfiguration":{"stop_limit_stop_limit_gtc":{"base_size":"2","limit_price":"50","stop_price":"52","stop_direction":"STOP_DIRECTION_STOP_UP"}},"maximumSpend":"2","minimumReceive":"97","maximumFee":"1"}),
        binding: json!({"account":"primary","resource":resource,"physicalIdentity":"p-1","portfolioId":"p-1","productId":"HBAR-USD","baseCurrencyId":"HBAR","quoteCurrencyId":"USD"}),
        summary: "test".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: gofin_domain::now(),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(45)).to_rfc3339(),
        valuation: None,
        intent: gofin_domain::FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![resource],
        },
        operation_version: 1,
        action_deadline: None,
    };
    let boundary = coinbase_order_boundary(
        &prepared,
        &json!({"maximumQuantity":"2","minimumPrice":"49","maximumFees":{"USD":"1"}}),
    )
    .unwrap();
    assert!(validate_coinbase_boundary(&boundary, &prepared).is_ok());
    let mut changed = prepared;
    changed.input["orderConfiguration"]["stop_limit_stop_limit_gtc"]["stop_price"] = json!("53");
    assert_eq!(
        validate_coinbase_boundary(&boundary, &changed)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
}

fn profile_asset(reference: &str) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "coinbase".into(),
        reference: reference.into(),
        decimals: 36,
    }
}

struct InsufficientFundsPreviewApi;
#[async_trait]
impl CoinbaseApi for InsufficientFundsPreviewApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        let body = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":"p-1","can_view":true,"can_trade":true})
            }
            CoinbaseRoute::Product { product_id } => {
                assert_eq!(product_id, "BTC-USDC");
                json!({"product":{"product_type":"SPOT","trading_disabled":false,"base_currency_id":"BTC","quote_currency_id":"USDC","base_increment":"0.00000001","quote_increment":"0.0000001","price_increment":"0.01","base_min_size":"0.00000001","quote_min_size":"0.0000001"}})
            }
            CoinbaseRoute::OrderPreview => json!({
                "base_size":"0.00002","quote_size":"1.6742402","order_total":"1.682611401",
                "commission_total":"0.008371201","errs":["PREVIEW_INSUFFICIENT_FUND"]
            }),
            _ => panic!(
                "unexpected route in native insufficient-funds preview fixture: {endpoint:?}"
            ),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
        ))
    }
}

#[tokio::test]
async fn native_insufficient_funds_preview_is_rejected_for_writes_but_deferred_for_read_only_plan_review()
 {
    let api: Arc<dyn CoinbaseApi> = Arc::new(InsufficientFundsPreviewApi);
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "preview-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("preview-key".into(), api)]),
    )
    .unwrap()
    .with_prices(Arc::new(PreviewPrices));
    let input = json!({"account":"primary","requestId":"funded-preview","productId":"BTC-USDC","side":"BUY",
        "orderConfiguration":{"limit_limit_fok":{"base_size":"0.00002","limit_price":"83712.01"}},
        "maximumSpend":"1.70","minimumReceive":"0.00002","maximumFee":"0.01"});
    let err = profile
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap_err();
    assert_eq!(err.code, "COINBASE_ORDER_PREVIEW_REJECTED");
    let permission = profile
        .prepare_plan_permission(
            "coinbase.create-order",
            &input,
            &json!({
                "maximumSpend":"1.70","maximumPrice":"85000","maximumFees":{"USDC":"0.01"}
            }),
        )
        .await
        .unwrap();
    assert!(
        permission
            .spec
            .maximum_debits
            .iter()
            .any(|debit| debit.amount.atoms.as_str() == exact_atoms("1.70", 36).unwrap().as_str())
    );
    let warnings = profile
        .validate_plan_feasibility("coinbase.create-order", &input)
        .await
        .unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("PREVIEW_INSUFFICIENT_FUND"))
    );
}

struct ConvertApi {
    realized_output: &'static str,
    realized_fee: &'static str,
}
struct BadConvertQuoteApi;
struct CreatedConvertApi(ConvertApi);
struct IdFieldOnlyConvertApi(ConvertApi);
#[async_trait]
impl CoinbaseApi for IdFieldOnlyConvertApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let mut response = self.0.request(endpoint.clone(), request, guard).await?;
        if matches!(endpoint, CoinbaseRoute::Account { .. }) {
            let mut body: Value = serde_json::from_slice(&response.body).unwrap();
            body["account"]["id"] = body["account"]["uuid"].take();
            response.body = serde_json::to_vec(&body).unwrap();
        }
        Ok(response)
    }
}
#[async_trait]
impl CoinbaseApi for CreatedConvertApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let mut response = self.0.request(endpoint.clone(), request, guard).await?;
        if matches!(endpoint, CoinbaseRoute::ConvertTradeExact { .. }) {
            let mut body: Value = serde_json::from_slice(&response.body).unwrap();
            body["trade"]["status"] = json!("TRADE_STATUS_CREATED");
            response.body = serde_json::to_vec(&body).unwrap();
        }
        Ok(response)
    }
}
#[async_trait]
impl CoinbaseApi for BadConvertQuoteApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let mut response = ConvertApi {
            realized_output: "0.0002",
            realized_fee: "0.1",
        }
        .request(endpoint.clone(), request, guard)
        .await?;
        if endpoint == CoinbaseRoute::ConvertQuote {
            let mut body: Value = serde_json::from_slice(&response.body).unwrap();
            body["trade"]["source"]["coinbase_account"]["account_id"] = json!("other-account");
            body["trade"]["total"]["value"] = json!("10.2");
            response.body = serde_json::to_vec(&body).unwrap();
        }
        Ok(response)
    }
}
#[async_trait]
impl CoinbaseApi for ConvertApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        let body = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":"p-1","can_view":true,"can_trade":true})
            }
            CoinbaseRoute::Account { account_id } => {
                let currency = if account_id == "usd-id" { "USD" } else { "BTC" };
                json!({"account":{"uuid":account_id,"currency":{"code":currency},"portfolio_id":"p-1","ready":true,"active":true,"available_balance":{"value":"100","currency":currency}}})
            }
            CoinbaseRoute::ConvertQuote => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                assert_eq!(body["from_account"], "USD");
                assert_eq!(body["to_account"], "BTC");
                assert_eq!(body["amount"], "10");
                json!({"trade":{"id":"trade-1","status":"TRADE_STATUS_CREATED","user_entered_amount":{"value":"10","currency":"USD"},"amount":{"value":"0.0002","currency":"BTC"},"subtotal":{"value":"10","currency":"USD"},"total":{"value":"10.1","currency":"USD"},"source":{"coinbase_account":{"account_id":"usd-id"}},"target":{"coinbase_account":{"account_id":"btc-id"}},"total_fee":{"amount":{"value":"0.1","currency":"USD"}}}})
            }
            CoinbaseRoute::ConvertTradeExact { trade_id, .. } => {
                let total = if self.realized_fee == "0.11" {
                    "10.11"
                } else {
                    "10.1"
                };
                json!({"trade":{"id":trade_id,"status":"TRADE_STATUS_COMPLETED","user_entered_amount":{"value":"10","currency":"USD"},"amount":{"value":self.realized_output,"currency":"BTC"},"subtotal":{"value":"10","currency":"USD"},"total":{"value":total,"currency":"USD"},"source":{"coinbase_account":{"account_id":"usd-id"}},"target":{"coinbase_account":{"account_id":"btc-id"}},"total_fee":{"amount":{"value":self.realized_fee,"currency":"USD"}}}})
            }
            _ => panic!("unexpected Coinbase conversion route"),
        };
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&body).unwrap(),

            pagination: Default::default(),
        })
    }
}

#[tokio::test]
async fn conversion_preparation_binds_quote_accounts_fees_and_commit_identity() {
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "conversion-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([(
            "conversion-key".into(),
            Arc::new(ConvertApi {
                realized_output: "0.0002",
                realized_fee: "0.1",
            }) as Arc<dyn CoinbaseApi>,
        )]),
    )
    .unwrap();
    let input = json!({"account":"primary","requestId":"conversion-1","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"});
    let prepared = profile.prepare("coinbase.convert", &input).await.unwrap();
    assert_eq!(prepared.binding["tradeId"], "trade-1");
    assert_eq!(prepared.binding["quote"]["amount"]["value"], "0.0002");
    assert_eq!(
        prepared.binding["requestBodyExact"],
        r#"{"from_account":"USD","to_account":"BTC"}"#
    );
    assert_eq!(
        prepared.intent.maximum_costs[0].amount.atoms.as_str(),
        exact_atoms("0.1", 36).unwrap().as_str()
    );
}

#[tokio::test]
async fn conversion_refresh_revalidates_the_original_trade_id_instead_of_requoting() {
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "conversion-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([(
            "conversion-key".into(),
            Arc::new(CreatedConvertApi(ConvertApi {
                realized_output: "0.0002",
                realized_fee: "0.1",
            })) as Arc<dyn CoinbaseApi>,
        )]),
    )
    .unwrap();
    let input = json!({"account":"primary","requestId":"conversion-refresh-1","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"});
    let approved = profile.prepare("coinbase.convert", &input).await.unwrap();
    let original_binding = approved.binding.clone();
    let refreshed = profile.refresh_prepared(&approved).await.unwrap();
    assert_eq!(refreshed.binding, original_binding);
    assert_eq!(refreshed.binding["tradeId"], "trade-1");
    assert!(refreshed.checks.iter().any(|check| {
        check.name == "refreshed_conversion_quote"
            && check.status == gofin_domain::CheckStatus::Passed
    }));
}

#[tokio::test]
async fn conversion_account_preflight_requires_native_v2_uuid_field() {
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "conversion-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([(
            "conversion-key".into(),
            Arc::new(IdFieldOnlyConvertApi(ConvertApi {
                realized_output: "0.0002",
                realized_fee: "0.1",
            })) as Arc<dyn CoinbaseApi>,
        )]),
    )
    .unwrap();
    let input = json!({"account":"primary","requestId":"conversion-account-uuid","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"});
    let error = profile
        .prepare("coinbase.convert", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_CURRENCY_ACCOUNT_MISMATCH");
}

#[tokio::test]
async fn conversion_rejects_quote_with_wrong_native_account_or_fee_total() {
    let api: Arc<dyn CoinbaseApi> = Arc::new(BadConvertQuoteApi);
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "primary".into(),
            resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
            credential_ref: "bad-conversion-key".into(),
            physical_identity: "p-1".into(),
            identity_verified: true,
            portfolio_id: Some("p-1".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("bad-conversion-key".into(), api)]),
    )
    .unwrap();
    let error = profile
        .prepare(
            "coinbase.convert",
            &json!({"account":"primary","requestId":"conversion-wrong-account","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_CONVERSION_BOUND_EXCEEDED");
}

#[tokio::test]
async fn completed_conversion_preserves_realized_effects_when_terminal_bounds_fail() {
    let account = CoinbaseAccount {
        name: "primary".into(),
        resource: gofin_domain::ResourceId("coinbase:portfolio:p-1".into()),
        credential_ref: "conversion-key".into(),
        physical_identity: "p-1".into(),
        identity_verified: true,
        portfolio_id: Some("p-1".into()),
        product: CoinbaseProduct::Advanced,
    };
    let api = Arc::new(ConvertApi {
        realized_output: "0.00018",
        realized_fee: "0.11",
    });
    let profile = CoinbaseProfile::new(
        vec![account],
        BTreeMap::from([("conversion-key".into(), api as Arc<dyn CoinbaseApi>)]),
    )
    .unwrap();
    let prepared = profile
        .prepare(
            "coinbase.convert",
            &json!({"account":"primary","requestId":"conversion-terminal","fromCurrency":"USD","fromAccountId":"usd-id","toCurrency":"BTC","toAccountId":"btc-id","amount":"10","maximumSpend":"10","minimumReceive":"0.00019","maximumFee":"0.1"}),
        )
        .await
        .unwrap();
    let submission = gofin_domain::Submission {
        kind: "coinbase".into(),
        reference: "trade-1".into(),
        data: json!({"binding":prepared.binding}),
    };
    let record = gofin_domain::OperationRecord {
        id: "operation-1".into(),
        request_id: "conversion-terminal".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: "coinbase.convert".into(),
        input: prepared.input.clone(),
        status: gofin_domain::Status::Pending,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission.clone()),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let reconciliation = profile
        .reconcile_submission(&record, &submission)
        .await
        .unwrap();
    assert_eq!(reconciliation.status, gofin_domain::Status::Failed);
    let effects = reconciliation.verified.unwrap().effects;
    assert_eq!(effects.len(), 2);
    assert!(matches!(
        effects[0],
        gofin_domain::FinancialEffect::Conversion { .. }
    ));
    assert!(matches!(
        effects[1],
        gofin_domain::FinancialEffect::Cost { .. }
    ));
}

struct MoveApi {
    portfolio: &'static str,
    currency_account: &'static str,
    currency: &'static str,
    initial: &'static str,
    settled: &'static str,
    reads: Mutex<usize>,
    portfolio_ids: Vec<&'static str>,
    can_transfer: bool,
}
#[async_trait]
impl CoinbaseApi for MoveApi {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _request: CoinbaseRequest,
        _guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let response = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":self.portfolio,"can_view":true,"can_trade":true,"can_transfer":self.can_transfer})
            }
            CoinbaseRoute::Portfolios => {
                json!({"portfolios":self.portfolio_ids.iter().map(|uuid|json!({"uuid":uuid})).collect::<Vec<_>>()})
            }
            CoinbaseRoute::Account { account_id } => {
                assert_eq!(account_id, self.currency_account);
                let mut reads = self.reads.lock().unwrap();
                let value = if *reads == 0 {
                    self.initial
                } else {
                    self.settled
                };
                *reads += 1;
                json!({"account":{"id":self.currency_account,"currency":{"code":self.currency},"portfolio_uuid":self.portfolio,"ready":true,"active":true,"available_balance":{"value":value,"currency":self.currency}}})
            }
            _ => panic!("unexpected portfolio move route"),
        };
        Ok(CoinbaseResponse::new(
            200,
            serde_json::to_vec(&response).unwrap(),
        ))
    }
}

fn move_profile() -> CoinbaseProfile {
    let source = Arc::new(MoveApi {
        portfolio: "p-source",
        currency_account: "source-usdc",
        currency: "USDC",
        initial: "100",
        settled: "90",
        reads: Mutex::new(0),
        portfolio_ids: vec!["p-source", "p-target"],
        can_transfer: true,
    });
    let target = Arc::new(MoveApi {
        portfolio: "p-target",
        currency_account: "target-usdc",
        currency: "USDC",
        initial: "0",
        settled: "10",
        reads: Mutex::new(0),
        portfolio_ids: vec!["p-target"],
        can_transfer: false,
    });
    CoinbaseProfile::new(
        vec![
            CoinbaseAccount {
                name: "source".into(),
                resource: gofin_domain::ResourceId("coinbase:portfolio:p-source".into()),
                credential_ref: "source-key".into(),
                physical_identity: "p-source".into(),
                identity_verified: true,
                portfolio_id: Some("p-source".into()),
                product: CoinbaseProduct::Advanced,
            },
            CoinbaseAccount {
                name: "target".into(),
                resource: gofin_domain::ResourceId("coinbase:portfolio:p-target".into()),
                credential_ref: "target-key".into(),
                physical_identity: "p-target".into(),
                identity_verified: true,
                portfolio_id: Some("p-target".into()),
                product: CoinbaseProduct::Advanced,
            },
        ],
        BTreeMap::from([
            ("source-key".into(), source as Arc<dyn CoinbaseApi>),
            ("target-key".into(), target as Arc<dyn CoinbaseApi>),
        ]),
    )
    .unwrap()
}

#[tokio::test]
async fn portfolio_move_binds_both_scopes_and_requires_native_receipt_plus_exact_balance_deltas() {
    let profile = move_profile();
    let input = json!({"account":"source","targetAccount":"target","requestId":"move-1","currency":"USDC","sourceCurrencyAccountId":"source-usdc","targetCurrencyAccountId":"target-usdc","amount":"10","minimumAmount":"10","maximumAmount":"11"});
    let prepared = profile
        .prepare("coinbase.move-portfolio-funds", &input)
        .await
        .unwrap();
    assert_eq!(prepared.binding["sourceBalanceBefore"], "100");
    assert_eq!(prepared.binding["targetBalanceBefore"], "0");
    assert_eq!(prepared.intent.expected.len(), 1);
    let boundary = profile
        .prepare_boundaries(
            &prepared,
            &json!({"minimumAmount":"10","maximumAmount":"11"}),
        )
        .unwrap();
    assert!(
        profile
            .validate_boundary_candidate(&boundary, &prepared)
            .is_ok()
    );
    let mut widened = prepared.clone();
    widened.input["amount"] = json!("9.99");
    assert_eq!(
        profile
            .validate_boundary_candidate(&boundary, &widened)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
    let planned = profile.plan_submission(&prepared).await.unwrap();
    assert!(matches!(
        planned.request.endpoint,
        CoinbaseRoute::PortfolioMoveFunds
    ));
    assert_eq!(
        planned.request.exact_body,
        r#"{"funds":{"currency":"USDC","value":"10"},"source_portfolio_uuid":"p-source","target_portfolio_uuid":"p-target"}"#
    );
    let receipt = br#"{"source_portfolio_uuid":"p-source","target_portfolio_uuid":"p-target"}"#;
    let record = gofin_domain::OperationRecord {
        id: "op-move".into(),
        request_id: "move-1".into(),
        fingerprint: "fp".into(),
        fingerprint_version: 1,
        operation: "coinbase.move-portfolio-funds".into(),
        input: prepared.input.clone(),
        status: gofin_domain::Status::Submitted,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: Some(
            json!({"httpStatus":200,"bodyHex":hex::encode(receipt),"bodySha256":hex::encode(sha2::Sha256::digest(receipt))}),
        ),
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
    assert_eq!(result.status, gofin_domain::Status::Completed);
    assert!(matches!(
        result.verified.unwrap().effects.as_slice(),
        [gofin_domain::FinancialEffect::Transfer { .. }]
    ));
}

#[tokio::test]
async fn portfolio_move_without_captured_receipt_remains_unknown_even_when_balances_look_settled() {
    let profile = move_profile();
    let prepared = profile.prepare("coinbase.move-portfolio-funds", &json!({"account":"source","targetAccount":"target","requestId":"move-lost","currency":"USDC","sourceCurrencyAccountId":"source-usdc","targetCurrencyAccountId":"target-usdc","amount":"10","minimumAmount":"10","maximumAmount":"10"})).await.unwrap();
    let planned = profile.plan_submission(&prepared).await.unwrap();
    let record = gofin_domain::OperationRecord {
        id: "op-move-lost".into(),
        request_id: "move-lost".into(),
        fingerprint: "fp".into(),
        fingerprint_version: 1,
        operation: "coinbase.move-portfolio-funds".into(),
        input: prepared.input.clone(),
        status: gofin_domain::Status::Unknown,
        created_at: gofin_domain::now(),
        updated_at: gofin_domain::now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(planned.submission.clone()),
        dispatch_evidence: None,
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
    assert_eq!(result.status, gofin_domain::Status::Unknown);
    assert!(result.verified.is_none());
}
