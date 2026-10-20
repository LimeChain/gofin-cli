use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};
use std::{
    io::{BufRead, Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc as channel,
    },
};
use zeroize::Zeroizing;

struct Nonces(AtomicU64);
impl NonceAllocator for Nonces {
    fn allocate(&self, _: &str, clock: u64) -> Result<u64, CcxtError> {
        Ok(self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |previous| {
                Some(previous.max(clock) + 1)
            })
            .unwrap()
            .max(clock))
    }
}
struct Captured {
    target: String,
    headers: std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
}

fn captured_nonce(wire: &Captured) -> u64 {
    if wire.headers["content-type"] == "application/json" {
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        body["nonce"].as_str().unwrap().parse().unwrap()
    } else {
        let body = String::from_utf8(wire.body.clone()).unwrap();
        reqwest::Url::parse(&format!("https://gofin.invalid/?{body}"))
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "nonce")
            .unwrap()
            .1
            .parse()
            .unwrap()
    }
}

fn server(response: &str) -> (String, channel::Receiver<Captured>) {
    server_bytes(response.as_bytes(), "application/json")
}
fn server_bytes(response: &[u8], content_type: &str) -> (String, channel::Receiver<Captured>) {
    server_status(response, content_type, 200)
}
fn server_status(
    response: &[u8],
    content_type: &str,
    status: u16,
) -> (String, channel::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (send, receive) = channel::channel();
    let response = response.to_owned();
    let content_type = content_type.to_owned();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let target = line.split_whitespace().nth(1).unwrap().to_owned();
        let mut headers = std::collections::BTreeMap::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.insert(k.to_ascii_lowercase(), v.trim().to_owned());
        }
        let mut body = vec![
            0;
            headers
                .get("content-length")
                .map(|s| s.parse().unwrap())
                .unwrap_or(0)
        ];
        reader.read_exact(&mut body).unwrap();
        let _ = send.send(Captured {
            target,
            headers,
            body,
        });
        write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len()).unwrap();
        stream.write_all(&response).unwrap();
    });
    (base, receive)
}

#[tokio::test]
async fn current_funding_business_errors_are_rejected_without_private_messages() {
    let (base, capture) = server(
        r#"{"error":{"code":"invalid_argument","message":"synthetic-private-request-detail"}}"#,
    );
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
    let failure = client
        .request(
            KrakenEndpoint::KrakenFundingGetFundingV1Networks,
            Some(credentials()),
            serde_json::json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind, CcxtErrorKind::Rejected);
    assert_eq!(failure.http_status, Some(200));
    assert_eq!(
        failure.funding_error_code,
        Some(crate::ccxt::FundingErrorCode::InvalidArgument)
    );
    assert!(!format!("{failure:?}").contains("synthetic-private"));
    assert!(
        capture
            .recv()
            .unwrap()
            .target
            .starts_with("/funding/v1/networks")
    );
}

#[tokio::test]
async fn export_transport_preserves_non_utf8_and_json_looking_artifact_bytes() {
    for bytes in [vec![0x50, 0x4b, 0, 0xff, 0xfe], b"42".to_vec()] {
        let (base, capture) = server_bytes(&bytes, "application/octet-stream");
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
        let response = client
            .request(
                KrakenEndpoint::KrakenPrivatePostRetrieveExport,
                Some(credentials()),
                serde_json::json!({"id":"synthetic-export"}),
                None,
            )
            .await
            .unwrap();
        match response {
            KrakenResponse::Artifact { bytes: actual, .. } => assert_eq!(actual, bytes),
            _ => panic!("export must preserve original bytes"),
        }
        assert_eq!(capture.recv().unwrap().target, "/0/private/RetrieveExport");
    }
}
fn credentials() -> Credentials {
    Credentials {
        api_key: Zeroizing::new("synthetic-only-key".into()),
        secret: Zeroizing::new("c3ludGhldGljLW5vbi1saXZlLWtleQ==".into()),
    }
}
fn hmac(data: &[u8]) -> String {
    let key = STANDARD.decode("c3ludGhldGljLW5vbi1saXZlLWtleQ==").unwrap();
    let mut mac = Hmac::<Sha512>::new_from_slice(&key).unwrap();
    mac.update(data);
    STANDARD.encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn funding_signature_matches_captured_path_query_and_body() {
    for (endpoint, input) in [
        (
            KrakenEndpoint::KrakenFundingGetFundingV1MethodsDirection,
            serde_json::json!({"direction":"deposit","asset":{"class":"currency","name":"USDC"}}),
        ),
        (
            KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,
            serde_json::json!({"amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"0.123456789012345678901234567890"}},"scope":{"method_id":"synthetic-method"},"address_id":"synthetic-address"}),
        ),
    ] {
        let (base, received) = server("{\"result\":{}}");
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
        client
            .request(
                endpoint,
                Some(credentials()),
                input,
                if endpoint.mutating() {
                    Some(Arc::new(|| true))
                } else {
                    None
                },
            )
            .await
            .unwrap();
        let wire = received.recv().unwrap();
        let mut digest = Sha256::new();
        digest.update(wire.headers["api-nonce"].as_bytes());
        digest.update(&wire.body);
        let mut signed = wire.target.into_bytes();
        signed.extend_from_slice(&digest.finalize());
        assert_eq!(wire.headers["api-sign"], hmac(&signed));
    }
}
#[tokio::test]
async fn spot_json_batch_and_form_signatures_match_captured_wire() {
    for (endpoint, input) in [
        (
            KrakenEndpoint::KrakenPrivatePostBalance,
            serde_json::json!({}),
        ),
        (
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch,
            serde_json::json!({"orders":[{"volume":"0.000000000000000001","ordertype":"limit","type":"buy","price":"10.1"}],"pair":"XBTUSD"}),
        ),
    ] {
        let (base, received) = server("{\"error\":[],\"result\":{}}");
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
        client
            .request(
                endpoint,
                Some(credentials()),
                input,
                if endpoint.mutating() {
                    Some(Arc::new(|| true))
                } else {
                    None
                },
            )
            .await
            .unwrap();
        let wire = received.recv().unwrap();
        assert!(wire.target.starts_with("/0/private/"));
        let mut digest = Sha256::new();
        digest.update(captured_nonce(&wire).to_string().as_bytes());
        digest.update(&wire.body);
        let mut data = wire.target.into_bytes();
        data.extend_from_slice(&digest.finalize());
        assert_eq!(wire.headers["api-sign"], hmac(&data));
    }
}
#[tokio::test]
async fn futures_batch_and_post_signature_match_body_and_omit_nonce() {
    for (endpoint, input) in [
        (
            KrakenEndpoint::KrakenfuturesPrivatePostBatchorder,
            serde_json::json!({"batchOrder":[{"order":"send","order_tag":"1","size":"1.000000000000000001","symbol":"PI_XBTUSD","side":"buy","orderType":"lmt","limitPrice":"1"}]}),
        ),
        (
            KrakenEndpoint::KrakenfuturesPrivatePutLeveragepreferences,
            serde_json::json!({"symbol":"PI_XBTUSD","maxLeverage":"2"}),
        ),
    ] {
        let (base, received) = server("{\"result\":\"success\"}");
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
        client
            .request(
                endpoint,
                Some(credentials()),
                input,
                Some(Arc::new(|| true)),
            )
            .await
            .unwrap();
        let wire = received.recv().unwrap();
        assert!(!wire.headers.contains_key("nonce"));
        let mut data = wire.body;
        data.extend_from_slice(wire.target.strip_prefix("/derivatives").unwrap().as_bytes());
        assert_eq!(wire.headers["authent"], hmac(&Sha256::digest(data)));
    }
}
#[tokio::test]
async fn venue_authentication_errors_are_safe_and_raw_decimals_survive() {
    let (base, _) = server("{\"error\":[\"EAPI:Invalid key\"],\"result\":null}");
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
    let err = client
        .request(
            KrakenEndpoint::KrakenPrivatePostBalance,
            Some(credentials()),
            serde_json::json!({}),
            None,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, CcxtErrorKind::Authentication);
    assert!(!err.to_string().contains("synthetic-only"));
    let (base, _) = server(
        "{\"error\":[],\"result\":{\"amount\":0.12345678901234567890123456789,\"id\":90071992547409931234}}",
    );
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
    let KrakenResponse::Json(data) = client
        .request(
            KrakenEndpoint::KrakenPrivatePostBalance,
            Some(credentials()),
            serde_json::json!({}),
            None,
        )
        .await
        .unwrap()
    else {
        panic!("JSON expected")
    };
    assert_eq!(
        data["result"]["amount"].to_string(),
        "0.12345678901234567890123456789"
    );
    assert_eq!(data["result"]["id"].to_string(), "90071992547409931234");
}
#[tokio::test]
async fn denied_guard_seals_no_dispatch_before_signing_or_http_and_never_sends() {
    for before_signing in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let nonces = Arc::new(Nonces(AtomicU64::new(1234)));
        let client = KrakenClient::loopback(nonces.clone(), base).unwrap();
        let checks = Arc::new(AtomicU64::new(0));
        let guard_checks = checks.clone();
        let result = client
            .request(
                KrakenEndpoint::KrakenPrivatePostAddOrder,
                Some(credentials()),
                serde_json::json!({}),
                Some(Arc::new(move || {
                    guard_checks.fetch_add(1, Ordering::SeqCst) < u64::from(!before_signing)
                })),
            )
            .await;
        let KrakenResponse::Rejected { kind, evidence } = result.unwrap() else {
            panic!("Missing sealed local no-dispatch receipt")
        };
        assert_eq!(kind, CcxtErrorKind::PermissionDenied);
        assert_eq!(evidence["evidenceVersion"], 2);
        assert_eq!(evidence["receiptSource"], "local_dispatch_guard");
        assert_eq!(evidence["dispatchOccurred"], false);
        assert!(evidence["httpStatus"].is_null());
        assert_eq!(
            evidence["phase"],
            if before_signing {
                "before_signing"
            } else {
                "before_http_dispatch"
            }
        );
        assert_eq!(
            evidence["requestParametersSha256"],
            hex::encode(Sha256::digest(b"{}"))
        );
        if before_signing {
            assert_eq!(nonces.0.load(Ordering::SeqCst), 1234);
        } else {
            assert!(nonces.0.load(Ordering::SeqCst) > 1234);
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[tokio::test]
async fn whole_request_rejection_is_sealed_to_exact_unsigned_parameters() {
    for (endpoint, body) in [
        (
            KrakenEndpoint::KrakenPrivatePostAddOrder,
            r#"{"error":["EOrder:Insufficient funds"],"result":null}"#,
        ),
        (
            KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,
            r#"{"error":{"code":"invalid_argument","message":"synthetic-private-detail"}}"#,
        ),
        (
            KrakenEndpoint::KrakenfuturesPrivatePostSendorder,
            r#"{"result":"error","error":"insufficientFunds"}"#,
        ),
    ] {
        let (base, capture) = server(body);
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
        let params = serde_json::json!({"synthetic":"exact review 0.000000000000000001"});
        let response = client
            .request(
                endpoint,
                Some(credentials()),
                params.clone(),
                Some(Arc::new(|| true)),
            )
            .await
            .unwrap();
        let KrakenResponse::Rejected { evidence, .. } = response else {
            panic!("expected sealed no-mutation receipt")
        };
        assert_eq!(evidence["endpoint"], endpoint.method());
        assert_eq!(evidence["classification"], "whole_request_no_mutation");
        assert_eq!(
            evidence["requestParametersSha256"],
            hex::encode(Sha256::digest(serde_json::to_vec(&params).unwrap()))
        );
        assert_eq!(
            evidence["responseSha256"],
            hex::encode(Sha256::digest(body.as_bytes()))
        );
        assert!(!evidence.to_string().contains("synthetic-private"));
        capture.recv().unwrap();
    }
}
#[tokio::test]
async fn partial_batch_business_failures_cannot_seal_whole_request_failure() {
    let (base, capture) = server(
        r#"{"result":"success","batchStatus":[{"status":"placed","order_id":"SYNTHETIC-1"},{"status":"insufficientFunds"}]}"#,
    );
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
    let response = client
        .request(
            KrakenEndpoint::KrakenfuturesPrivatePostBatchorder,
            Some(credentials()),
            serde_json::json!({"batchOrder":[]}),
            Some(Arc::new(|| true)),
        )
        .await
        .unwrap();
    assert!(matches!(response, KrakenResponse::Json(_)));
    capture.recv().unwrap();
}

#[tokio::test]
async fn spot_wallet_query_scope_has_official_location_and_legacy_path_signature() {
    for endpoint in [
        KrakenEndpoint::KrakenPrivatePostBalance,
        KrakenEndpoint::KrakenPrivatePostQueryLedgers,
        KrakenEndpoint::KrakenPrivatePostOpenPositions,
    ] {
        let (base, capture) = server(r#"{"error":[],"result":{}}"#);
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
        client
            .request(
                endpoint,
                Some(credentials()),
                serde_json::json!({"account_id":"synthetic wallet/1","rebase_multiplier":"base"}),
                None,
            )
            .await
            .unwrap();
        let wire = capture.recv().unwrap();
        assert_eq!(
            wire.target,
            format!("/0{}?account_id=synthetic+wallet%2F1", endpoint.path())
        );
        assert!(!String::from_utf8_lossy(&wire.body).contains("account_id"));
        let mut digest = Sha256::new();
        digest.update(captured_nonce(&wire).to_string().as_bytes());
        digest.update(&wire.body);
        let mut signed = format!("/0{}", endpoint.path()).into_bytes();
        signed.extend_from_slice(&digest.finalize());
        assert_eq!(wire.headers["api-sign"], hmac(&signed));
    }
}
#[tokio::test]
async fn funding_post_wallet_scope_is_signed_query_not_json_payload() {
    let (base, capture) = server(r#"{"result":{}}"#);
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1234))), base).unwrap();
    client.request(KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals,Some(credentials()),serde_json::json!({"account_id":"synthetic-wallet","amount":{"asset_amount":{"asset":{"class":"currency","name":"USDC"},"amount":"1.000000000000000001"}}}),Some(Arc::new(||true))).await.unwrap();
    let wire = capture.recv().unwrap();
    assert_eq!(
        wire.target,
        "/funding/v1/withdrawals?account_id=synthetic-wallet"
    );
    let body: Value = serde_json::from_slice(&wire.body).unwrap();
    assert!(body.get("account_id").is_none());
    assert_eq!(
        body["amount"]["asset_amount"]["amount"],
        "1.000000000000000001"
    );
    let mut digest = Sha256::new();
    digest.update(wire.headers["api-nonce"].as_bytes());
    digest.update(&wire.body);
    let mut signed = wire.target.into_bytes();
    signed.extend_from_slice(&digest.finalize());
    assert_eq!(wire.headers["api-sign"], hmac(&signed));
}

#[tokio::test]
async fn derivatives_query_and_post_array_options_are_repeated_and_signed_exactly() {
    for endpoint in [
        KrakenEndpoint::KrakenfuturesPublicGetInstruments,
        KrakenEndpoint::KrakenfuturesPublicGetTickers,
        KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus,
    ] {
        let (base, capture) = server(r#"{"result":"success"}"#);
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
        let (field, items) = if endpoint == KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus {
            ("cliOrdIds", vec!["SYNTHETIC-1", "SYNTHETIC-2"])
        } else {
            ("contractType", vec!["futures_inverse", "flexible_futures"])
        };
        client
            .request(
                endpoint,
                endpoint.private().then(credentials),
                serde_json::json!({field:items}),
                None,
            )
            .await
            .unwrap();
        let wire = capture.recv().unwrap();
        let encoded = if endpoint.verb() == "GET" {
            wire.target.split_once('?').unwrap().1.as_bytes()
        } else {
            &wire.body
        };
        assert_eq!(
            std::str::from_utf8(encoded).unwrap(),
            format!("{field}={}&{field}={}", items[0], items[1])
        );
        if endpoint.private() {
            let mut signed = encoded.to_vec();
            signed.extend_from_slice(wire.target.strip_prefix("/derivatives").unwrap().as_bytes());
            assert_eq!(wire.headers["authent"], hmac(&Sha256::digest(signed)));
        }
    }
}

#[tokio::test]
async fn successful_http_and_assessment_do_not_turn_futures_business_rejection_into_an_old_order() {
    for (endpoint, field) in [
        (
            KrakenEndpoint::KrakenfuturesPrivatePostSendorder,
            "sendStatus",
        ),
        (
            KrakenEndpoint::KrakenfuturesPrivatePostEditorder,
            "editStatus",
        ),
    ] {
        let body =
            serde_json::json!({"result":"success",field:{"status":"insufficientAvailableFunds"}})
                .to_string();
        let (base, capture) = server(&body);
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
        let response = client
            .request(
                endpoint,
                Some(credentials()),
                serde_json::json!({"cliOrdId":"SYNTHETIC-REUSED"}),
                Some(Arc::new(|| true)),
            )
            .await
            .unwrap();
        assert!(matches!(response, KrakenResponse::Rejected { .. }));
        capture.recv().unwrap();
    }
}
#[test]
fn contradictory_execution_events_and_unknown_status_cannot_prove_no_mutation() {
    let e = KrakenEndpoint::KrakenfuturesPrivatePostSendorder;
    assert!(!no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"iocWouldNotExecute","orderEvents":{"malformed":"REJECT"}}})
    ));
    assert!(no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"iocWouldNotExecute","orderEvents":[{"type":"REJECT","order":{"filled":0.0}}]}})
    ));
    assert!(!no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"unrecognizedNewStatus"}})
    ));
    assert!(!no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"insufficientAvailableFunds","orderEvents":[{"type":"EXECUTION"}]}})
    ));
    assert!(!no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"iocWouldNotExecute","orderEvents":[{"type":"REJECT","order":{"filled":"1"}}]}})
    ));
    assert!(no_fill_futures_rejection(
        e,
        &serde_json::json!({"result":"success","sendStatus":{"status":"iocWouldNotExecute","orderEvents":[{"type":"REJECT","order":{"filled":0}}]}})
    ));
}

#[tokio::test]
async fn signing_and_nonce_failures_seal_no_http_dispatch_but_transport_loss_does_not() {
    struct BrokenNonces;
    impl NonceAllocator for BrokenNonces {
        fn allocate(&self, _: &str, _: u64) -> Result<u64, CcxtError> {
            Err(failure(CcxtErrorKind::Unavailable))
        }
    }
    for broken_nonce in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let nonces: Arc<dyn NonceAllocator> = if broken_nonce {
            Arc::new(BrokenNonces)
        } else {
            Arc::new(Nonces(AtomicU64::new(1)))
        };
        let client =
            KrakenClient::loopback(nonces, format!("http://{}", listener.local_addr().unwrap()))
                .unwrap();
        let mut key = credentials();
        if !broken_nonce {
            key.secret = Zeroizing::new("synthetic-invalid-base64!".into());
        }
        let response = client
            .request(
                KrakenEndpoint::KrakenPrivatePostAddOrder,
                Some(key),
                serde_json::json!({"volume":"0.01"}),
                Some(Arc::new(|| true)),
            )
            .await
            .unwrap();
        let KrakenResponse::Rejected { kind, evidence } = response else {
            panic!("missing local no-dispatch receipt")
        };
        assert_eq!(kind, CcxtErrorKind::Rejected);
        assert_eq!(evidence["evidenceVersion"], 3);
        assert_eq!(evidence["receiptSource"], "local_signing_failure");
        assert_eq!(evidence["dispatchOccurred"], false);
        assert_eq!(
            evidence["requestParametersSha256"],
            hex::encode(Sha256::digest(br#"{"volume":"0.01"}"#))
        );
        assert!(!evidence.to_string().contains("synthetic-invalid"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    // A peer that receives the request and closes without a response might have
    // accepted it. This must never produce a local no-dispatch receipt.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let peer = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).unwrap() > 0);
    });
    let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
    let error = client
        .request(
            KrakenEndpoint::KrakenPrivatePostAddOrder,
            Some(credentials()),
            serde_json::json!({}),
            Some(Arc::new(|| true)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, CcxtErrorKind::Unavailable);
    peer.join().unwrap();
}

#[tokio::test]
async fn claimed_address_errors_preserve_only_closed_diagnostics_and_exact_query() {
    for (status, code, expected, kind) in [
        (
            400,
            "invalid_argument",
            Some(crate::ccxt::FundingErrorCode::InvalidArgument),
            CcxtErrorKind::Rejected,
        ),
        (
            403,
            "permission_denied",
            Some(crate::ccxt::FundingErrorCode::PermissionDenied),
            CcxtErrorKind::PermissionDenied,
        ),
        (
            503,
            "unavailable",
            Some(crate::ccxt::FundingErrorCode::Unavailable),
            CcxtErrorKind::Rejected,
        ),
        (
            400,
            "private-account-identifier",
            None,
            CcxtErrorKind::Rejected,
        ),
    ] {
        let response = serde_json::json!({"error":{"code":code,"message":"private-token-address-request-details"}}).to_string();
        let (base, capture) = server_status(response.as_bytes(), "application/json", status);
        let client = KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap();
        let failure = client
            .request(
                KrakenEndpoint::KrakenFundingGetFundingV2DepositAddresses,
                Some(credentials()),
                serde_json::json!({"scope":{"method_id":"synthetic-method"},"limit":500}),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.http_status, Some(status));
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.funding_error_code, expected);
        assert!(!format!("{failure:?}").contains("private-"));
        let request = capture.recv().unwrap();
        let url = reqwest::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
        assert_eq!(url.path(), "/funding/v2/deposit/addresses");
        let fields = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(fields.get("scope[method_id]").unwrap(), "synthetic-method");
        assert_eq!(fields.get("limit").unwrap(), "500");
        assert!(request.body.is_empty());
    }
}
