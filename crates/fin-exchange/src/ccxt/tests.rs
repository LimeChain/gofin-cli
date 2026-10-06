use super::*;
use base64::Engine;
use sha2::Sha256;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Server {
    url: String,
    count: Arc<AtomicUsize>,
    captured: std::sync::mpsc::Receiver<String>,
    worker: std::thread::JoinHandle<()>,
}
impl Server {
    fn new(status: u16, body: &'static str, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let (tx, captured) = std::sync::mpsc::channel();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let end = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < end {
                if let Ok((mut stream, _)) = listener.accept() {
                    seen.fetch_add(1, Ordering::SeqCst);
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut bytes = vec![];
                    let mut chunk = [0; 4096];
                    loop {
                        let n = stream.read(&mut chunk).unwrap();
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&bytes);
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|v| v.parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if body.len() >= length {
                                break;
                            }
                        }
                    }
                    tx.send(String::from_utf8(bytes).unwrap()).ok();
                    std::thread::sleep(delay);
                    let location = if status == 302 {
                        "Location: http://127.0.0.1:9/replay\r\n"
                    } else {
                        ""
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fixture\r\n{location}Content-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            url,
            count,
            captured,
            worker,
        }
    }
    fn request(&self) -> String {
        self.captured.recv_timeout(Duration::from_secs(4)).unwrap()
    }
    fn finish(self) -> usize {
        self.worker.join().unwrap();
        self.count.load(Ordering::SeqCst)
    }
}
fn credentials(secret: String) -> Option<Credentials> {
    Some(Credentials {
        api_key: Zeroizing::new("SYNTHETIC-LOCAL-ONLY".into()),
        secret: Zeroizing::new(secret),
    })
}
fn params() -> Vec<(String, String)> {
    vec![
        (
            "amount".into(),
            "12345678901234567890.123456789012345678".into(),
        ),
        ("asset".into(), "USDC".into()),
        ("fromAccountType".into(), "SPOT".into()),
        ("toAccountType".into(), "SPOT".into()),
        ("clientTranId".into(), "synthetic-local-only".into()),
    ]
}
fn signed_body(request: &str) -> (&str, String) {
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let (payload, signature) = body.rsplit_once("&signature=").unwrap();
    let decoded = reqwest::Url::parse(&format!("http://127.0.0.1/?sig={signature}"))
        .unwrap()
        .query_pairs()
        .next()
        .unwrap()
        .1
        .into_owned();
    (payload, decoded)
}
#[tokio::test]
async fn official_binance_hmac_preserves_long_amount_and_large_identifiers() {
    use hmac::{Hmac, Mac};
    let server = Server::new(
        200,
        r#"{"tranId":18446744073709551615,"amount":"12345678901234567890.123456789012345678"}"#,
        Duration::ZERO,
    );
    let client = CcxtClient::for_test(&server.url).unwrap();
    let value = client
        .request(
            ExchangeId::Binance,
            Method::BinanceUniversalTransfer,
            credentials("SYNTHETIC-HMAC-SECRET".into()),
            params(),
        )
        .await
        .unwrap();
    assert_eq!(value["tranId"], 18446744073709551615u64);
    let request = server.request();
    assert!(request.starts_with("POST /sapi/v1/sub-account/universalTransfer "));
    let (payload, signature) = signed_body(&request);
    assert!(payload.contains("amount=12345678901234567890.123456789012345678"));
    let mut mac = Hmac::<Sha256>::new_from_slice(b"SYNTHETIC-HMAC-SECRET").unwrap();
    mac.update(payload.as_bytes());
    mac.verify_slice(&hex::decode(signature).unwrap()).unwrap();
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn official_binance_ed25519_signs_exact_wire_payload() {
    use ed25519_dalek::{Verifier, pkcs8::EncodePrivateKey};
    let key = ed25519_dalek::SigningKey::from_bytes(&[19; 32]);
    let pem = key.to_pkcs8_pem(Default::default()).unwrap();
    let server = Server::new(200, r#"{"id":"synthetic"}"#, Duration::ZERO);
    CcxtClient::for_test(&server.url)
        .unwrap()
        .request(
            ExchangeId::Binance,
            Method::BinanceStandardWithdrawal,
            credentials(pem.to_string()),
            params(),
        )
        .await
        .unwrap();
    let request = server.request();
    let (payload, signature) = signed_body(&request);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .unwrap();
    key.verifying_key()
        .verify(
            payload.as_bytes(),
            &ed25519_dalek::Signature::from_slice(&bytes).unwrap(),
        )
        .unwrap();
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn official_binance_rsa_signs_exact_wire_payload() {
    use rsa::{pkcs8::EncodePrivateKey, signature::Verifier};
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::StdRng::seed_from_u64(53), 1024).unwrap();
    use rand::SeedableRng;
    let pem = key.to_pkcs8_pem(Default::default()).unwrap();
    let server = Server::new(200, r#"{"id":"synthetic"}"#, Duration::ZERO);
    CcxtClient::for_test(&server.url)
        .unwrap()
        .request(
            ExchangeId::Binance,
            Method::BinanceStandardWithdrawal,
            credentials(pem.to_string()),
            params(),
        )
        .await
        .unwrap();
    let request = server.request();
    let (payload, signature) = signed_body(&request);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .unwrap();
    rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key.to_public_key())
        .verify(
            payload.as_bytes(),
            &rsa::pkcs1v15::Signature::try_from(bytes.as_slice()).unwrap(),
        )
        .unwrap();
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn timeout_write_never_replays() {
    let server = Server::new(200, r#"{"tranId":1}"#, Duration::from_millis(300));
    let error = CcxtClient::for_test_with_timeout(&server.url, Duration::from_millis(100))
        .unwrap()
        .request(
            ExchangeId::Binance,
            Method::BinanceUniversalTransfer,
            credentials("SYNTHETIC".into()),
            params(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, CcxtErrorKind::Timeout);
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn redirects_never_replay_write() {
    let server = Server::new(302, "", Duration::ZERO);
    let result = CcxtClient::for_test(&server.url)
        .unwrap()
        .request(
            ExchangeId::Binance,
            Method::BinanceUniversalTransfer,
            credentials("SYNTHETIC".into()),
            params(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn kraken_read_uses_official_unsigned_endpoint() {
    let server = Server::new(
        200,
        r#"{"error":[],"result":{"XXBTZUSD":{"c":["60000.123456789012345678","1"]}}}"#,
        Duration::ZERO,
    );
    let value = CcxtClient::for_test(&server.url)
        .unwrap()
        .request(
            ExchangeId::Kraken,
            Method::KrakenTicker,
            None,
            vec![("pair".into(), "XBTUSD".into())],
        )
        .await
        .unwrap();
    assert_eq!(
        value["result"]["XXBTZUSD"]["c"][0],
        "60000.123456789012345678"
    );
    let request = server.request();
    assert!(request.starts_with("GET /0/public/Ticker?pair=XBTUSD "));
    assert!(!request.to_lowercase().contains("api-key"));
    assert_eq!(server.finish(), 1);
}
#[tokio::test]
async fn raw_observer_preserves_decimal_numbers_and_ids_without_dynamic_conversion() {
    let server = Server::new(
        200,
        r#"{"id":18446744073709551615,"fee":0.123456789012345678901234567890}"#,
        Duration::ZERO,
    );
    let value = CcxtClient::for_test(&server.url)
        .unwrap()
        .request(ExchangeId::Kraken, Method::KrakenAssetPairs, None, vec![])
        .await
        .unwrap();
    assert_eq!(value["id"], 18446744073709551615u64);
    assert!(value["fee"].is_number());
    assert_eq!(value["fee"].to_string(), "0.123456789012345678901234567890");
    assert_eq!(server.finish(), 1);
    assert!(CcxtClient::for_test("https://api.binance.com").is_err());
}

#[tokio::test]
async fn every_allowlisted_binance_method_routes_through_ccxt() {
    let server = Server::new(200, "{}", Duration::ZERO);
    let cases = [
        (
            Method::BinanceApiRestrictions,
            "GET",
            "/sapi/v1/account/apiRestrictions",
        ),
        (Method::BinanceAccount, "GET", "/api/v3/account"),
        (
            Method::BinanceSubAccountList,
            "GET",
            "/sapi/v1/sub-account/list",
        ),
        (
            Method::BinanceSubAccountSpotAssets,
            "GET",
            "/sapi/v4/sub-account/assets",
        ),
        (
            Method::BinanceCapitalConfig,
            "GET",
            "/sapi/v1/capital/config/getall",
        ),
        (
            Method::BinanceDepositAddress,
            "GET",
            "/sapi/v1/capital/deposit/address",
        ),
        (
            Method::BinanceWithdrawAddressList,
            "GET",
            "/sapi/v1/capital/withdraw/address/list",
        ),
        (
            Method::BinanceDepositHistory,
            "GET",
            "/sapi/v1/capital/deposit/hisrec",
        ),
        (
            Method::BinanceWithdrawalHistory,
            "GET",
            "/sapi/v1/capital/withdraw/history",
        ),
        (
            Method::BinanceUniversalTransferHistory,
            "GET",
            "/sapi/v1/sub-account/universalTransfer",
        ),
        (
            Method::BinanceSubAccountTransferHistory,
            "GET",
            "/sapi/v1/sub-account/transfer/subUserHistory",
        ),
        (
            Method::BinanceUniversalTransfer,
            "POST",
            "/sapi/v1/sub-account/universalTransfer",
        ),
        (
            Method::BinanceStandardWithdrawal,
            "POST",
            "/sapi/v1/capital/withdraw/apply",
        ),
        (
            Method::BinanceTravelRuleRequirements,
            "GET",
            "/sapi/v1/localentity/questionnaire-requirements",
        ),
        (Method::BinanceTickerPrice, "GET", "/api/v3/ticker/price"),
        (
            Method::BinanceSpotExchangeInfo,
            "GET",
            "/api/v3/exchangeInfo",
        ),
        (
            Method::BinanceSpotCommission,
            "GET",
            "/api/v3/account/commission",
        ),
        (Method::BinanceSpotOpenOrders, "GET", "/api/v3/openOrders"),
        (Method::BinanceSpotOrder, "GET", "/api/v3/order"),
        (Method::BinanceSpotTrades, "GET", "/api/v3/myTrades"),
        (Method::BinanceSpotPlaceOrder, "POST", "/api/v3/order"),
        (Method::BinanceSpotCancelOrder, "DELETE", "/api/v3/order"),
        (
            Method::BinanceFuturesExchangeInfo,
            "GET",
            "/fapi/v1/exchangeInfo",
        ),
        (Method::BinanceFuturesAccount, "GET", "/fapi/v3/account"),
        (
            Method::BinanceFuturesAccountConfig,
            "GET",
            "/fapi/v1/accountConfig",
        ),
        (
            Method::BinanceFuturesSymbolConfig,
            "GET",
            "/fapi/v1/symbolConfig",
        ),
        (
            Method::BinanceFuturesPositions,
            "GET",
            "/fapi/v3/positionRisk",
        ),
        (
            Method::BinanceFuturesPositionMode,
            "GET",
            "/fapi/v1/positionSide/dual",
        ),
        (
            Method::BinanceFuturesCommission,
            "GET",
            "/fapi/v1/commissionRate",
        ),
        (Method::BinanceFuturesFeeBurn, "GET", "/fapi/v1/feeBurn"),
        (Method::BinanceFuturesOrder, "GET", "/fapi/v1/order"),
        (
            Method::BinanceFuturesOpenOrders,
            "GET",
            "/fapi/v1/openOrders",
        ),
        (Method::BinanceFuturesTrades, "GET", "/fapi/v1/userTrades"),
        (Method::BinanceFuturesIncome, "GET", "/fapi/v1/income"),
        (
            Method::BinanceFuturesMarkPrice,
            "GET",
            "/fapi/v1/premiumIndex",
        ),
        (
            Method::BinanceFuturesLeverageBrackets,
            "GET",
            "/fapi/v1/leverageBracket",
        ),
        (Method::BinanceFuturesPlaceOrder, "POST", "/fapi/v1/order"),
        (
            Method::BinanceFuturesCancelOrder,
            "DELETE",
            "/fapi/v1/order",
        ),
        (
            Method::BinanceWalletTransfer,
            "POST",
            "/sapi/v1/asset/transfer",
        ),
        (
            Method::BinanceWalletTransferHistory,
            "GET",
            "/sapi/v1/asset/transfer",
        ),
    ];
    let expected_count = cases.len();
    for (method, verb, path) in cases {
        let key = if method.private() {
            credentials("SYNTHETIC".into())
        } else {
            None
        };
        let client = CcxtClient::for_test(&server.url).unwrap();
        let result = if method.requires_guard() {
            client
                .request_guarded(ExchangeId::Binance, method, key, vec![], Arc::new(|| true))
                .await
        } else {
            client
                .request(ExchangeId::Binance, method, key, vec![])
                .await
        };
        result.unwrap_or_else(|error| panic!("{method:?}: {error}"));
        let wire = server.request();
        let first = wire.lines().next().unwrap();
        assert!(
            first.starts_with(&format!("{verb} {path}")),
            "{method:?}: {first}"
        );
    }
    assert_eq!(server.finish(), expected_count);
}
#[tokio::test]
async fn raw_observer_keeps_http_and_integer_error_evidence_without_sensitive_messages() {
    let server = Server::new(
        400,
        r#"{"code":-2010,"msg":"SYNTHETIC-SENSITIVE-MESSAGE"}"#,
        Duration::ZERO,
    );
    let error = CcxtClient::for_test(&server.url)
        .unwrap()
        .request(
            ExchangeId::Binance,
            Method::BinanceAccount,
            credentials("SYNTHETIC".into()),
            vec![],
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, Some(400));
    assert_eq!(error.exchange_code, Some(-2010));
    assert!(!format!("{error:?}").contains("SYNTHETIC"));
    assert_eq!(server.finish(), 1);
}

#[tokio::test]
async fn queued_write_rechecks_authorization_immediately_before_signing() {
    let server = Server::new(200, "{}", Duration::from_millis(150));
    let client = CcxtClient::for_test(&server.url).unwrap();
    let first = client.clone();
    let first = tokio::spawn(async move {
        first
            .request(
                ExchangeId::Binance,
                Method::BinanceTickerPrice,
                None,
                vec![],
            )
            .await
    });
    while server.count.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let expiry = std::time::Instant::now() + Duration::from_millis(30);
    let guard = Arc::new(move || std::time::Instant::now() < expiry);
    assert!(guard());
    let result = client
        .request_guarded(
            ExchangeId::Binance,
            Method::BinanceUniversalTransfer,
            credentials("SYNTHETIC".into()),
            params(),
            guard,
        )
        .await
        .unwrap_err();
    assert_eq!(result.kind, CcxtErrorKind::PermissionDenied);
    assert_eq!(result.http_status, None);
    first.await.unwrap().unwrap();
    assert_eq!(server.finish(), 1);
}

#[cfg(all(
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
type PublicCase = (ExchangeId, Method, Vec<(String, String)>, &'static str);
#[cfg(all(
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
fn additional_public_cases() -> Vec<PublicCase> {
    vec![
        (
            ExchangeId::Hyperliquid,
            Method::HyperliquidMetaAndAssetContexts,
            vec![("type".into(), "metaAndAssetCtxs".into())],
            "POST /info ",
        ),
        (
            ExchangeId::Okx,
            Method::OkxInstruments,
            vec![("instType".into(), "SPOT".into())],
            "GET /api/v5/public/instruments?instType=SPOT ",
        ),
        (
            ExchangeId::Okx,
            Method::OkxTicker,
            vec![("instId".into(), "BTC-USDT".into())],
            "GET /api/v5/market/ticker?instId=BTC-USDT ",
        ),
        (
            ExchangeId::Coinbase,
            Method::CoinbaseProducts,
            vec![("product_type".into(), "SPOT".into())],
            "GET /api/v3/brokerage/market/products?product_type=SPOT ",
        ),
        (
            ExchangeId::Coinbase,
            Method::CoinbaseTicker,
            vec![("product_id".into(), "BTC-USD".into())],
            "GET /api/v3/brokerage/market/products/BTC-USD/ticker ",
        ),
        (
            ExchangeId::Bybit,
            Method::BybitInstruments,
            vec![("category".into(), "spot".into())],
            "GET /v5/market/instruments-info?category=spot ",
        ),
        (
            ExchangeId::Bybit,
            Method::BybitTicker,
            vec![
                ("category".into(), "spot".into()),
                ("symbol".into(), "BTCUSDT".into()),
            ],
            "GET /v5/market/tickers?category=spot&symbol=BTCUSDT ",
        ),
    ]
}

#[cfg(all(
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
#[tokio::test]
async fn additional_public_methods_use_official_unsigned_routes_and_exact_json() {
    let mut servers = Vec::new();
    for (exchange, method, params, path) in additional_public_cases() {
        let (body, pointer) = match method {
            Method::HyperliquidMetaAndAssetContexts => (
                r#"[{"universe":[{"name":"BTC","szDecimals":5}]},[{"markPx":"60000.123456789012345678901","price":"60000.123456789012345678901","number":0.123456789012345678901,"id":18446744073709551615}]]"#,
                "/1/0",
            ),
            Method::OkxInstruments | Method::OkxTicker => (
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","price":"60000.123456789012345678901","number":0.123456789012345678901,"id":18446744073709551615}]}"#,
                "/data/0",
            ),
            Method::CoinbaseProducts => (
                r#"{"products":[{"product_id":"BTC-USD","price":"60000.123456789012345678901","number":0.123456789012345678901,"id":18446744073709551615}],"num_products":1}"#,
                "/products/0",
            ),
            Method::CoinbaseTicker => (
                r#"{"trades":[{"product_id":"BTC-USD","price":"60000.123456789012345678901","number":0.123456789012345678901,"id":18446744073709551615}]}"#,
                "/trades/0",
            ),
            Method::BybitInstruments | Method::BybitTicker => (
                r#"{"retCode":0,"retMsg":"OK","result":{"category":"spot","list":[{"symbol":"BTCUSDT","price":"60000.123456789012345678901","number":0.123456789012345678901,"id":18446744073709551615}]}}"#,
                "/result/list/0",
            ),
            _ => unreachable!(),
        };
        let server = Server::new(200, body, Duration::ZERO);
        let result = CcxtClient::for_test(&server.url)
            .unwrap()
            .request(exchange, method, None, params)
            .await
            .unwrap_or_else(|e| panic!("{method:?}: {e}"));
        let entry = result.pointer(pointer).unwrap();
        assert_eq!(entry["price"], "60000.123456789012345678901");
        assert_eq!(entry["number"].to_string(), "0.123456789012345678901");
        assert_eq!(entry["id"], u64::MAX);
        let wire = server.request();
        assert!(
            wire.starts_with(path),
            "{method:?}: {}",
            wire.lines().next().unwrap()
        );
        let lower = wire.to_ascii_lowercase();
        assert!(
            !lower.contains("authorization:")
                && !lower.contains("api-key")
                && !lower.contains("apikey")
                && !lower.contains("signature=")
        );
        if exchange == ExchangeId::Hyperliquid {
            assert_eq!(
                serde_json::from_str::<Value>(wire.split_once("\r\n\r\n").unwrap().1).unwrap(),
                serde_json::json!({"type":"metaAndAssetCtxs"})
            );
        }
        servers.push(server);
    }
    for server in servers {
        assert_eq!(server.finish(), 1);
    }
}

#[cfg(all(
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
#[tokio::test]
async fn additional_venues_redact_errors_and_reject_private_and_credentialed_requests() {
    let server = Server::new(
        400,
        r#"{"msg":"SYNTHETIC-PRIVATE-UPSTREAM-MESSAGE"}"#,
        Duration::ZERO,
    );
    for (exchange, method, params, _) in additional_public_cases() {
        let client = CcxtClient::for_test(&server.url).unwrap();
        let error = client
            .request(exchange, method, None, params.clone())
            .await
            .unwrap_err();
        assert_eq!(error.http_status, Some(400));
        assert!(!format!("{error:?} {error}").contains("SYNTHETIC"));
        assert_eq!(
            client
                .request(
                    exchange,
                    Method::BinanceAccount,
                    credentials("SYNTHETIC".into()),
                    vec![]
                )
                .await
                .unwrap_err()
                .kind,
            CcxtErrorKind::InvalidRequest
        );
        assert_eq!(
            client
                .request(exchange, method, credentials("SYNTHETIC".into()), params)
                .await
                .unwrap_err()
                .kind,
            CcxtErrorKind::InvalidRequest
        );
    }
    assert_eq!(server.finish(), 7);
}

/// GET/DELETE sign the query; POST signs the body. Reconstruct the exact
/// transport payload rather than parsing amounts through any numeric type.
fn signed_payload(request: &str) -> (String, String) {
    if request.starts_with("POST ") {
        let (payload, signature) = signed_body(request);
        (payload.to_owned(), signature)
    } else {
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let query = target.split_once('?').unwrap().1;
        let (payload, signature) = query.rsplit_once("&signature=").unwrap();
        (payload.to_owned(), signature.to_owned())
    }
}

#[tokio::test]
async fn trading_signatures_preserve_exact_decimals_and_encoded_identifiers() {
    use hmac::{Hmac, Mac};
    let mut servers = Vec::new();
    for method in [
        Method::BinanceSpotPlaceOrder,
        Method::BinanceFuturesPlaceOrder,
        Method::BinanceSpotCancelOrder,
        Method::BinanceFuturesCancelOrder,
        Method::BinanceSpotOrder,
        Method::BinanceFuturesOrder,
    ] {
        let server = Server::new(
            200,
            r#"{"orderId":18446744073709551615,"price":"12345678901234567890.123456789012345678"}"#,
            Duration::ZERO,
        );
        let client = CcxtClient::for_test(&server.url).unwrap();
        let params = vec![
            ("symbol".into(), "BTCUSDT".into()),
            ("quantity".into(), "0.000000000000000001".into()),
            (
                "price".into(),
                "12345678901234567890.123456789012345678".into(),
            ),
            ("newClientOrderId".into(), "fin-local:/._-".into()),
            ("origClientOrderId".into(), "synthetic &+=%/é".into()),
            ("orderId".into(), "18446744073709551615".into()),
            ("reduceOnly".into(), "true".into()),
        ];
        let result = if method.requires_guard() {
            client
                .request_guarded(
                    ExchangeId::Binance,
                    method,
                    credentials("SYNTHETIC-HMAC-SECRET".into()),
                    params,
                    Arc::new(|| true),
                )
                .await
        } else {
            client
                .request(
                    ExchangeId::Binance,
                    method,
                    credentials("SYNTHETIC-HMAC-SECRET".into()),
                    params,
                )
                .await
        }
        .unwrap();
        assert_eq!(result["orderId"], u64::MAX);
        let request = server.request();
        let (payload, signature) = signed_payload(&request);
        let (timestamp, exact_params) = payload.split_once('&').unwrap();
        assert!(
            timestamp
                .strip_prefix("timestamp=")
                .unwrap()
                .parse::<u64>()
                .is_ok()
        );
        assert_eq!(
            exact_params,
            "symbol=BTCUSDT&quantity=0.000000000000000001&price=12345678901234567890.123456789012345678&newClientOrderId=fin-local%3A%2F._-&origClientOrderId=synthetic%20%26%2B%3D%25%2F%C3%A9&orderId=18446744073709551615&reduceOnly=true&recvWindow=5000"
        );
        assert!(payload.contains("quantity=0.000000000000000001"));
        assert!(payload.contains("price=12345678901234567890.123456789012345678"));
        assert!(payload.contains("orderId=18446744073709551615"));
        assert!(payload.contains("newClientOrderId=fin-local%3A%2F._-"));
        assert!(payload.contains("origClientOrderId=synthetic%20%26%2B%3D%25%2F%C3%A9"));
        assert!(payload.contains("reduceOnly=true"));
        let mut mac = Hmac::<Sha256>::new_from_slice(b"SYNTHETIC-HMAC-SECRET").unwrap();
        mac.update(payload.as_bytes());
        mac.verify_slice(&hex::decode(signature).unwrap()).unwrap();
        servers.push(server);
    }
    for server in servers {
        assert_eq!(server.finish(), 1);
    }
}

#[tokio::test]
async fn trading_writes_require_guard_and_expired_guard_sends_nothing() {
    let server = Server::new(200, "{}", Duration::ZERO);
    let client = CcxtClient::for_test(&server.url).unwrap();
    for method in [
        Method::BinanceSpotPlaceOrder,
        Method::BinanceSpotCancelOrder,
        Method::BinanceFuturesPlaceOrder,
        Method::BinanceFuturesCancelOrder,
        Method::BinanceWalletTransfer,
    ] {
        let error = client
            .request(
                ExchangeId::Binance,
                method,
                credentials("SYNTHETIC".into()),
                vec![],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CcxtErrorKind::PermissionDenied);
        let error = client
            .request_guarded(
                ExchangeId::Binance,
                method,
                credentials("SYNTHETIC".into()),
                vec![],
                Arc::new(|| false),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CcxtErrorKind::PermissionDenied);
        assert_eq!(error.http_status, None);
    }
    assert_eq!(server.finish(), 0);
}

#[tokio::test]
async fn trading_write_errors_never_retry_and_keep_safe_exchange_evidence() {
    let mut servers = Vec::new();
    for method in [
        Method::BinanceSpotPlaceOrder,
        Method::BinanceFuturesPlaceOrder,
        Method::BinanceSpotCancelOrder,
        Method::BinanceFuturesCancelOrder,
    ] {
        for status in [400, 429, 500, 302] {
            let server = Server::new(
                status,
                r#"{"code":-2010,"msg":"SYNTHETIC-SECRET-UPSTREAM"}"#,
                Duration::ZERO,
            );
            let result = CcxtClient::for_test(&server.url)
                .unwrap()
                .request_guarded(
                    ExchangeId::Binance,
                    method,
                    credentials("SYNTHETIC".into()),
                    vec![],
                    Arc::new(|| true),
                )
                .await
                .unwrap_err();
            assert_eq!(result.http_status, Some(status));
            assert_eq!(result.exchange_code, Some(-2010));
            assert!(!format!("{result:?} {result}").contains("SYNTHETIC"));
            servers.push(server);
        }
    }
    for server in servers {
        assert_eq!(server.finish(), 1);
    }
}

#[tokio::test]
async fn trading_write_timeout_never_retries() {
    let mut servers = Vec::new();
    for method in [
        Method::BinanceSpotPlaceOrder,
        Method::BinanceFuturesPlaceOrder,
    ] {
        let server = Server::new(200, "{}", Duration::from_millis(300));
        let error = CcxtClient::for_test_with_timeout(&server.url, Duration::from_millis(100))
            .unwrap()
            .request_guarded(
                ExchangeId::Binance,
                method,
                credentials("SYNTHETIC".into()),
                vec![],
                Arc::new(|| true),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CcxtErrorKind::Timeout);
        servers.push(server);
    }
    for server in servers {
        assert_eq!(server.finish(), 1);
    }
}

#[tokio::test]
async fn trading_reads_preserve_bounded_history_and_large_ids_in_signed_query() {
    use hmac::{Hmac, Mac};
    let server = Server::new(200, "[]", Duration::ZERO);
    for method in [
        Method::BinanceSpotTrades,
        Method::BinanceFuturesTrades,
        Method::BinanceFuturesIncome,
    ] {
        CcxtClient::for_test(&server.url)
            .unwrap()
            .request(
                ExchangeId::Binance,
                method,
                credentials("SYNTHETIC-HMAC-SECRET".into()),
                vec![
                    ("symbol".into(), "BTCUSDT".into()),
                    ("orderId".into(), "18446744073709551615".into()),
                    ("startTime".into(), "1770000000001".into()),
                    ("endTime".into(), "1770000000999".into()),
                    ("limit".into(), "1000".into()),
                ],
            )
            .await
            .unwrap();
        let wire = server.request();
        assert!(wire.starts_with("GET "));
        let (payload, signature) = signed_payload(&wire);
        assert_eq!(
            payload.split_once('&').unwrap().1,
            "symbol=BTCUSDT&orderId=18446744073709551615&startTime=1770000000001&endTime=1770000000999&limit=1000&recvWindow=5000"
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(b"SYNTHETIC-HMAC-SECRET").unwrap();
        mac.update(payload.as_bytes());
        mac.verify_slice(&hex::decode(signature).unwrap()).unwrap();
    }
    assert_eq!(server.finish(), 3);
}

#[tokio::test]
async fn queued_trading_order_and_cancel_expiry_prevents_signing_and_network() {
    let mut servers = Vec::new();
    for method in [
        Method::BinanceSpotPlaceOrder,
        Method::BinanceFuturesCancelOrder,
    ] {
        let server = Server::new(200, "{}", Duration::from_millis(150));
        let client = CcxtClient::for_test(&server.url).unwrap();
        let first = client.clone();
        let first = tokio::spawn(async move {
            first
                .request(
                    ExchangeId::Binance,
                    Method::BinanceTickerPrice,
                    None,
                    vec![],
                )
                .await
        });
        while server.count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let expiry = std::time::Instant::now() + Duration::from_millis(30);
        let guard = Arc::new(move || std::time::Instant::now() < expiry);
        assert!(guard());
        let error = client
            .request_guarded(
                ExchangeId::Binance,
                method,
                credentials("SYNTHETIC".into()),
                vec![],
                guard,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CcxtErrorKind::PermissionDenied);
        first.await.unwrap().unwrap();
        servers.push(server);
    }
    for server in servers {
        assert_eq!(server.finish(), 1);
    }
}
