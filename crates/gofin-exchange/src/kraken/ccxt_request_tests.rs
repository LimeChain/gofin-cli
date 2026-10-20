//! Independent wire regressions for the CCXT request-construction boundary.
use super::{KrakenEndpoint, NonceAllocator, ccxt_request};
use crate::ccxt::{CcxtError, Credentials};
use serde_json::{Map, Value, json};
use std::sync::Mutex;
use zeroize::Zeroizing;

struct DurableNonces(Mutex<u64>);
impl NonceAllocator for DurableNonces {
    fn allocate(&self, digest: &str, clock: u64) -> Result<u64, CcxtError> {
        assert_eq!(digest.len(), 64);
        let mut high_water = self.0.lock().unwrap();
        *high_water = high_water.checked_add(1).unwrap().max(clock);
        Ok(*high_water)
    }
}
fn credentials() -> Credentials {
    Credentials {
        api_key: Zeroizing::new("synthetic-ccxt-api-key".into()),
        secret: Zeroizing::new("c3ludGhldGljLW5vbi1saXZlLWtleQ==".into()),
    }
}
fn nonce(wire: &super::signing::WireRequest) -> u64 {
    let body = wire.body.as_deref().expect("private Spot body");
    if body.starts_with('{') {
        let value: Value = serde_json::from_str(body).unwrap();
        return value["nonce"].as_str().unwrap().parse().unwrap();
    }
    reqwest::Url::parse(&format!("https://gofin.invalid/?{body}"))
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "nonce")
        .unwrap()
        .1
        .parse()
        .unwrap()
}

#[tokio::test]
async fn every_closed_endpoint_retains_its_exact_method_and_concrete_path() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(4_000_000_000_000));
    let mut failures = Vec::new();
    for &endpoint in KrakenEndpoint::ALL {
        let mut parameters = Map::new();
        let mut path = endpoint.path().to_owned();
        while let Some(start) = path.find('{') {
            let end = start + path[start..].find('}').unwrap();
            let key = path[start + 1..end].to_owned();
            let value = format!("test_{key}");
            parameters.insert(key, Value::String(value.clone()));
            path.replace_range(start..=end, &value);
        }
        let result = ccxt_request::request(
            endpoint,
            Value::Object(parameters),
            endpoint.private().then_some(&credentials),
            &nonces,
            None,
        )
        .await;
        let wire = match result {
            Ok(wire) => wire,
            Err(error) => {
                failures.push(format!("{endpoint:?}: {error:?}"));
                continue;
            }
        };
        assert_eq!(wire.method.as_str(), endpoint.verb(), "{endpoint:?}");
        let expected = reqwest::Url::parse(&format!("{}{path}", endpoint.base())).unwrap();
        let actual = reqwest::Url::parse(&wire.url).unwrap();
        assert_eq!(actual.scheme(), expected.scheme(), "{endpoint:?}");
        assert_eq!(actual.host_str(), expected.host_str(), "{endpoint:?}");
        assert_eq!(actual.path(), expected.path(), "{endpoint:?}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn private_nonce_survives_new_request_core_and_clock_rollback() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(4_000_000_000_000));
    let mut observed = Vec::new();
    for _ in 0..3 {
        // Each request constructs a new CCXT core. Only the protected allocator
        // survives, as it does after a broker restart.
        let wire = ccxt_request::request(
            KrakenEndpoint::KrakenPrivatePostDepositAddresses,
            json!({"asset":"USDC","method":"USDC - Ethereum (Unified)"}),
            Some(&credentials),
            &nonces,
            None,
        )
        .await
        .unwrap();
        observed.push(nonce(&wire));
        assert_eq!(observed.last().copied(), Some(*nonces.0.lock().unwrap()));
    }
    assert!(observed.windows(2).all(|pair| pair[1] > pair[0]));
}

struct RacingNonces(Mutex<(usize, u64)>);
impl NonceAllocator for RacingNonces {
    fn allocate(&self, _: &str, clock: u64) -> Result<u64, CcxtError> {
        let mut state = self.0.lock().unwrap();
        state.0 += 1;
        state.1 = if state.0 <= 2 {
            // A separate client has reserved ahead of the locally signed nonce.
            clock + 10_000
        } else {
            (state.1 + 1).max(clock)
        };
        Ok(state.1)
    }
}

#[tokio::test]
async fn nonce_race_resigns_locally_until_exact_emitted_nonce_is_durable() {
    let credentials = credentials();
    let nonces = RacingNonces(Mutex::new((0, 0)));
    let wire = ccxt_request::request(
        KrakenEndpoint::KrakenPrivatePostBalance,
        json!({}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await
    .unwrap();
    let state = nonces.0.lock().unwrap();
    assert!(state.0 >= 2, "must exercise durable high-water collision");
    assert_eq!(nonce(&wire), state.1);
}

struct ExhaustedNonces;
impl NonceAllocator for ExhaustedNonces {
    fn allocate(&self, _: &str, _: u64) -> Result<u64, CcxtError> {
        Ok(u64::MAX)
    }
}

#[tokio::test]
async fn unrepresentable_durable_nonce_fails_before_request_can_dispatch() {
    let credentials = credentials();
    let result = ccxt_request::request(
        KrakenEndpoint::KrakenPrivatePostBalance,
        json!({}),
        Some(&credentials),
        &ExhaustedNonces,
        None,
    )
    .await;
    assert!(result.is_err());
}

struct ContendedNonces(Mutex<usize>);
impl NonceAllocator for ContendedNonces {
    fn allocate(&self, _: &str, clock: u64) -> Result<u64, CcxtError> {
        *self.0.lock().unwrap() += 1;
        Ok(clock + 10_000)
    }
}

#[tokio::test]
async fn persistent_nonce_contention_exhausts_a_finite_local_budget() {
    let credentials = credentials();
    let nonces = ContendedNonces(Mutex::new(0));
    let result = ccxt_request::request(
        KrakenEndpoint::KrakenPrivatePostBalance,
        json!({}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await;
    assert!(result.is_err());
    let calls = *nonces.0.lock().unwrap();
    assert!(calls > 1 && calls <= 3, "unbounded nonce construction");
}

#[tokio::test]
async fn funding_and_api_permission_extensions_preserve_their_wire_contracts() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(4_000_000_000_000));
    let wire = ccxt_request::request(
        KrakenEndpoint::KrakenFundingGetFundingV2DepositAddresses,
        json!({"limit":500,"scope":{"method_id":"27ede8db-804b-4d91-8e25-46b7b9668730"}}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await
    .unwrap();
    assert!(wire.url.contains("/funding/v2/deposit/addresses?"));
    assert!(wire.url.contains("scope%5Bmethod_id%5D="));
    assert_eq!(wire.headers["api-nonce"], "4000000000001");
    assert!(wire.body.is_none());
    let wire = ccxt_request::request(
        KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check,
        json!({}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await
    .unwrap();
    assert!(wire.url.ends_with("/api/auth/v1/api-keys/v3/check"));
    assert!(wire.headers.contains_key("authent"));
    assert!(!wire.headers.contains_key("nonce"));
}

#[tokio::test]
async fn futures_batch_and_repeated_array_fields_preserve_exact_encoding() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(0));
    let wire = ccxt_request::request(
        KrakenEndpoint::KrakenfuturesPrivatePostBatchorder,
        json!({"orders":[{"size":"1.000000000000000001","cliOrdId":"test λ"}]}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await
    .unwrap();
    let body = wire.body.unwrap();
    assert!(body.starts_with("json=%7B"));
    assert!(body.contains("%CE%BB"));
    assert!(!wire.url.contains('?'));
    assert!(!wire.headers.contains_key("nonce"));
    let wire = ccxt_request::request(
        KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus,
        json!({"orderIds":["one","two"],"cliOrdIds":["first","second"]}),
        Some(&credentials),
        &nonces,
        None,
    )
    .await
    .unwrap();
    assert_eq!(wire.method, reqwest::Method::POST);
    assert!(!wire.url.contains('?'));
    let url =
        reqwest::Url::parse(&format!("https://gofin.invalid/?{}", wire.body.unwrap())).unwrap();
    let query: Vec<_> = url.query_pairs().collect();
    assert_eq!(query.iter().filter(|(key, _)| key == "orderIds").count(), 2);
    assert_eq!(
        query.iter().filter(|(key, _)| key == "cliOrdIds").count(),
        2
    );
}

#[tokio::test]
async fn ccxt_fallback_never_accepts_caller_nonce_or_address_creation() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(0));
    assert!(!ccxt_request::needs_extension(
        KrakenEndpoint::KrakenPrivatePostDepositAddresses,
        &json!({"asset":"USDC","method":"USDC - Ethereum (Unified)"})
    ));
    for input in [
        json!({"asset":"USDC","nonce":"1"}),
        json!({"asset":"USDC","otp":"123456"}),
        json!({"asset":"USDC","new":true}),
        json!({"asset":"USDC","new":"true"}),
    ] {
        let result = ccxt_request::request(
            KrakenEndpoint::KrakenPrivatePostDepositAddresses,
            input,
            Some(&credentials),
            &nonces,
            None,
        )
        .await;
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn path_substitution_cannot_change_endpoint_or_inject_query_parameters() {
    let credentials = credentials();
    let nonces = DurableNonces(Mutex::new(0));
    let endpoint = KrakenEndpoint::KrakenfuturesPublicGetInstrumentsSymbolStatus;
    for value in ["", ".", ".."] {
        let result =
            ccxt_request::request(endpoint, json!({"symbol":value}), None, &nonces, None).await;
        assert!(result.is_err(), "invalid path segment {value:?}");
    }
    for value in ["../accounts", "x?apiKey=attacker", "x#fragment", "%2e%2e"] {
        let result =
            ccxt_request::request(endpoint, json!({"symbol":value}), None, &nonces, None).await;
        // Rejecting unsupported symbol text is allowed. If supported, the
        // reserved characters must remain literal data in exactly one segment.
        if let Ok(wire) = result {
            let url = reqwest::Url::parse(&wire.url).unwrap();
            let path: Vec<_> = url.path_segments().unwrap().collect();
            assert_eq!(path.len(), 6);
            assert_eq!(path[3], "instruments");
            assert_eq!(path[5], "status");
            assert_eq!(url.query(), None);
            assert_eq!(url.fragment(), None);
            assert_eq!(url.host_str(), Some("futures.kraken.com"));
            assert_eq!(
                percent_encoding::percent_decode_str(path[4])
                    .decode_utf8()
                    .unwrap(),
                value
            );
        }
    }
    // No private credentials are needed to build a public request.
    assert!(
        ccxt_request::request(
            endpoint,
            json!({"symbol":"PI_XBTUSD"}),
            Some(&credentials),
            &nonces,
            None,
        )
        .await
        .is_err()
    );
}
