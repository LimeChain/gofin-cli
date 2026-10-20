use super::*;
use ccxt_request::Core;
use std::time::{Duration, Instant};

fn auth_for(host: Host) -> CoinbaseAuth {
    match host {
        Host::App => {
            let signing = EdSigningKey::from_bytes(&[0x42; 32]);
            let mut bytes = vec![0x42; 32];
            bytes.extend_from_slice(signing.verifying_key().as_bytes());
            CoinbaseAuth::CdpEd25519 {
                api_key: "organizations/test/apiKeys/test".into(),
                private_key_base64: STANDARD.encode(bytes),
            }
        }
        Host::Exchange => CoinbaseAuth::ExchangeHmac {
            api_key: "test-key".into(),
            secret: STANDARD.encode([0x42; 32]),
            passphrase: "test-passphrase".into(),
        },
        Host::International => CoinbaseAuth::InternationalHmac {
            api_key: "test-key".into(),
            secret: STANDARD.encode([0x42; 32]),
            passphrase: "test-passphrase".into(),
        },
    }
}
fn wire(route: &CoinbaseRoute, body: &str) -> ccxt_request::Wire {
    let auth = auth_for(route.host());
    let mut core = Core::new(&auth);
    let binding = core
        .binding(route)
        .unwrap_or_else(|e| panic!("binding {route:?}: {e}"));
    core.construct(&auth, route, body, &binding)
        .unwrap_or_else(|e| panic!("signing {route:?}: {e}"))
}
fn verify_hmac(wire: &ccxt_request::Wire, route: &CoinbaseRoute, body: &str) {
    let timestamp = wire.headers["cb-access-timestamp"].to_str().unwrap();
    let mut mac = HmacSha256::new_from_slice(&[0x42; 32]).unwrap();
    mac.update(
        format!(
            "{timestamp}{}{}{body}",
            route.method(),
            route.path().unwrap()
        )
        .as_bytes(),
    );
    assert_eq!(
        wire.headers["cb-access-sign"].to_str().unwrap(),
        STANDARD.encode(mac.finalize().into_bytes())
    );
    assert_eq!(wire.body, body);
    assert_eq!(wire.url, route.url().unwrap());
}

#[test]
fn every_native_read_uses_pinned_registry_and_preserves_exact_scoped_url() {
    assert_eq!(reads::READS.len(), 167);
    for read in reads::READS {
        let route = CoinbaseRoute::NativeRead {
            route: read.route,
            parameters: read
                .parameters
                .iter()
                .map(|_| "opaque value/%".to_owned())
                .collect(),
            query: CoinbaseQuery::default(),
        };
        let auth = auth_for(route.host());
        let mut core = Core::new(&auth);
        let binding = core
            .binding(&route)
            .unwrap_or_else(|e| panic!("{}: {e}", read.operation));
        assert!(!binding.missing_address_read, "{}", read.operation);
        let signed = core
            .construct(&auth, &route, "", &binding)
            .unwrap_or_else(|e| panic!("{}: {e}", read.operation));
        assert_eq!(signed.url, route.url().unwrap(), "{}", read.operation);
        if read.public {
            assert!(signed.headers.get("authorization").is_none());
            assert!(signed.headers.get("cb-access-sign").is_none());
        } else if !matches!(route.host(), Host::App) {
            verify_hmac(&signed, &route, "");
        }
    }
}

#[test]
fn exchange_and_international_sign_exact_query_and_frozen_body() {
    let cases = [
        (
            CoinbaseRoute::ExchangeCancelOrderScoped {
                order_id: "order/id".into(),
                product_id: "BTC-USD".into(),
                profile_id: "profile 1".into(),
            },
            "",
        ),
        (
            CoinbaseRoute::InternationalCancelOrderScoped {
                order_id: "order/id".into(),
                portfolio_id: "portfolio 1".into(),
            },
            "",
        ),
        (
            CoinbaseRoute::InternationalOrdersFiltered {
                query: CoinbaseQuery {
                    portfolio_id: Some("portfolio 1".into()),
                    instrument: Some("BTC-PERP".into()),
                    result_limit: Some(50),
                    ..Default::default()
                },
            },
            "",
        ),
        (
            CoinbaseRoute::ExchangeOrders,
            r#"{"client_oid":"original-id","price":"0.000000000000000001","size":"100.000000000000000001"}"#,
        ),
        (
            CoinbaseRoute::InternationalOrders,
            r#"{"client_order_id":"original-id","size":"100.000000000000000001"}"#,
        ),
        // Pinned JSON conversion cannot preserve numeric decimal lexemes or
        // caller whitespace/key order. The explicit exact-body shim must.
        (
            CoinbaseRoute::ExchangeOrders,
            r#"{ "size": 100.000000000000000001, "client_oid": "original-id" }"#,
        ),
        (
            CoinbaseRoute::InternationalOrders,
            r#"{ "size": 100.000000000000000001, "client_order_id": "original-id" }"#,
        ),
        (CoinbaseRoute::ExchangeReportCreate, "{}"),
        (CoinbaseRoute::ExchangeReportCreate, " { } "),
        (CoinbaseRoute::InternationalPortfolioCreate, "{}"),
    ];
    for (route, body) in cases {
        verify_hmac(&wire(&route, body), &route, body);
    }
}

#[test]
fn cdp_uses_verified_eddsa_token_not_pinned_hs256_jwt_stub() {
    use ed25519_dalek::{Signature, Verifier};
    let route = CoinbaseRoute::HistoricalOrders {
        query: CoinbaseQuery {
            product_ids: vec!["SOL-USD".into(), "BTC-USD".into()],
            order_ids: vec!["order a".into(), "order/b".into()],
            ..Default::default()
        },
    };
    let signed = wire(&route, "");
    assert_eq!(signed.url, route.url().unwrap());
    let token = signed.headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    let parts: Vec<_> = token.split('.').collect();
    let header: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(header["alg"], "EdDSA");
    assert_eq!(header["kid"], "organizations/test/apiKeys/test");
    assert!(
        header["nonce"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        claims["uri"],
        "GET api.coinbase.com/api/v3/brokerage/orders/historical/batch"
    );
    let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
    EdSigningKey::from_bytes(&[0x42; 32])
        .verifying_key()
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .unwrap();
}

#[test]
fn missing_exchange_address_get_reuses_native_signer_without_creating_address() {
    let route = CoinbaseRoute::ExchangeDepositAddresses {
        coinbase_account_id: "account/id".into(),
    };
    let auth = auth_for(route.host());
    let mut core = Core::new(&auth);
    let binding = core.binding(&route).unwrap();
    assert!(binding.missing_address_read);
    let signed = core.construct(&auth, &route, "", &binding).unwrap();
    assert_eq!(route.method(), "GET");
    verify_hmac(&signed, &route, "");
}

#[tokio::test]
async fn persistent_native_pacing_rechecks_authority_after_waiting() {
    let signer = ccxt_request::Client::new(Arc::new(auth_for(Host::App))).unwrap();
    signer
        .request(
            CoinbaseRoute::LegacyAccounts {
                query: CoinbaseQuery::default(),
            },
            String::new(),
            None,
        )
        .await
        .unwrap();
    let started = Instant::now();
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let guard: DispatchGuard = {
        let checks = checks.clone();
        Arc::new(move || {
            checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            started.elapsed() < Duration::from_millis(100)
        })
    };
    let result = signer
        .request(CoinbaseRoute::Orders, "{}".into(), Some(guard))
        .await;
    assert!(matches!(result, Err(CoinbaseError::GuardRejected)));
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn credentials_cannot_cross_product_and_bad_hmac_secret_fails_locally() {
    let auth = auth_for(Host::Exchange);
    let core = Core::new(&auth);
    assert!(matches!(
        core.binding(&CoinbaseRoute::KeyPermissions),
        Err(CoinbaseError::InvalidCredential)
    ));
    let invalid = CoinbaseAuth::ExchangeHmac {
        api_key: "secret-key-marker".into(),
        secret: "bad!secret".into(),
        passphrase: "secret-passphrase".into(),
    };
    let mut core = Core::new(&invalid);
    let route = CoinbaseRoute::ExchangeAccounts;
    let binding = core.binding(&route).unwrap();
    let error = core
        .construct(&invalid, &route, "", &binding)
        .err()
        .unwrap();
    assert!(matches!(error, CoinbaseError::InvalidCredential));
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn every_typed_route_uses_registry_with_only_declared_missing_address_read() {
    let routes = [
        CoinbaseRoute::KeyPermissions,
        CoinbaseRoute::LegacyCurrencies,
        CoinbaseRoute::LegacyCryptoCurrencies,
        CoinbaseRoute::LegacyAccounts {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Accounts {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Account {
            account_id: "opaque-id".into(),
        },
        CoinbaseRoute::Portfolios,
        CoinbaseRoute::PortfolioCreate,
        CoinbaseRoute::PortfolioDetail {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::PortfolioUpdate {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::PortfolioDelete {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::CfmSweepSchedule,
        CoinbaseRoute::CfmMarginSetting,
        CoinbaseRoute::CfmCancelSweeps,
        CoinbaseRoute::Products {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Product {
            product_id: "opaque-id".into(),
        },
        CoinbaseRoute::ProductBook {
            product_id: "opaque-id".into(),
        },
        CoinbaseRoute::OrderPreview,
        CoinbaseRoute::OrderEditPreview,
        CoinbaseRoute::Orders,
        CoinbaseRoute::OrderEdit,
        CoinbaseRoute::OrderCancel,
        CoinbaseRoute::OrderCancelBatch,
        CoinbaseRoute::Order {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::HistoricalOrders {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Fills {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Fees,
        CoinbaseRoute::DepositAddresses {
            account_id: "opaque-id".into(),
        },
        CoinbaseRoute::DepositAddressCreate {
            account_id: "opaque-id".into(),
        },
        CoinbaseRoute::Transactions {
            account_id: "opaque-id".into(),
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::Transaction {
            account_id: "opaque-id".into(),
            transaction_id: "opaque-id".into(),
        },
        CoinbaseRoute::TransactionSend {
            account_id: "opaque-id".into(),
        },
        CoinbaseRoute::ConvertQuote,
        CoinbaseRoute::ConvertTrade {
            trade_id: "opaque-id".into(),
        },
        CoinbaseRoute::ConvertTradeExact {
            trade_id: "opaque-id".into(),
            from_account: "opaque-id".into(),
            to_account: "opaque-id".into(),
        },
        CoinbaseRoute::ConvertCommit {
            trade_id: "opaque-id".into(),
        },
        CoinbaseRoute::PortfolioMoveFunds,
        CoinbaseRoute::ExchangeAccounts,
        CoinbaseRoute::ExchangeAccount {
            account_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeProfiles,
        CoinbaseRoute::ExchangeProfile {
            profile_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeProfileCreate,
        CoinbaseRoute::ExchangeProfileRename {
            profile_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeOrders,
        CoinbaseRoute::ExchangeOrdersList {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::ExchangeCancelOrder {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeCancelOrderScoped {
            order_id: "opaque-id".into(),
            product_id: "opaque-id".into(),
            profile_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeOrder {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeOrderByClientId {
            client_oid: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeFills,
        CoinbaseRoute::ExchangeFillsFiltered {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::ExchangeFees,
        CoinbaseRoute::ExchangeReportCreate,
        CoinbaseRoute::ExchangeLoanLendingOverview,
        CoinbaseRoute::ExchangeDepositAddresses {
            coinbase_account_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeDepositAddressCreate {
            coinbase_account_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeTransfers {
            account_id: "opaque-id".into(),
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::ExchangeProfileTransfers {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::ExchangeProfileTransfer {
            transfer_id: "opaque-id".into(),
        },
        CoinbaseRoute::ExchangeDeposit,
        CoinbaseRoute::ExchangeWithdraw,
        CoinbaseRoute::InternationalAccounts,
        CoinbaseRoute::InternationalOrders,
        CoinbaseRoute::InternationalOrdersList,
        CoinbaseRoute::InternationalOrdersFiltered {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::InternationalCancelOrder {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalCancelOrderScoped {
            order_id: "opaque-id".into(),
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioTransfer,
        CoinbaseRoute::InternationalPortfolioDetail {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioSummary {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioBalances {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioBalance {
            portfolio_id: "opaque-id".into(),
            asset: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioPositions {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioPosition {
            portfolio_id: "opaque-id".into(),
            instrument: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioLoans {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioLoan {
            portfolio_id: "opaque-id".into(),
            asset: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioLoanAvailability {
            portfolio_id: "opaque-id".into(),
            asset: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioMarginCallStatus {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioCreate,
        CoinbaseRoute::InternationalPortfolioUpdate {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioPatch {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioMargin,
        CoinbaseRoute::InternationalCrossCollateral {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalAutoMargin {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalTransfers {
            query: CoinbaseQuery::default(),
        },
        CoinbaseRoute::InternationalTransfer {
            transfer_uuid: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalEditOrder {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalOrder {
            order_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalOrderScoped {
            order_id: "opaque-id".into(),
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalFills,
        CoinbaseRoute::InternationalPortfolioFills {
            portfolio_id: "opaque-id".into(),
        },
        CoinbaseRoute::InternationalPortfolioFillsFiltered {
            portfolio_id: "opaque-id".into(),
            query: CoinbaseQuery::default(),
        },
    ];
    for route in routes {
        let auth = auth_for(route.host());
        let mut core = Core::new(&auth);
        let binding = core
            .binding(&route)
            .unwrap_or_else(|e| panic!("{route:?}: {e}"));
        assert_eq!(
            binding.missing_address_read,
            matches!(route, CoinbaseRoute::ExchangeDepositAddresses { .. })
        );
        let body = if matches!(route.method(), "POST" | "PUT" | "PATCH") {
            "{}"
        } else {
            ""
        };
        let signed = core
            .construct(&auth, &route, body, &binding)
            .unwrap_or_else(|e| panic!("{route:?}: {e}"));
        assert_eq!(signed.url, route.url().unwrap(), "{route:?}");
        if !matches!(route.host(), Host::App) {
            verify_hmac(&signed, &route, body);
        }
    }
}
