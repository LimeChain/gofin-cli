//! Synthetic, loopback-only HTTP fixtures. No production RPC or financial writes.
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

use super::{HttpRpc, RpcReadRequirements, SolanaRpc};
use crate::MAINNET_GENESIS_HASH;

struct Reply {
    status: u16,
    body: Value,
    retry_after: Option<&'static str>,
    delay: Duration,
    disconnect: bool,
    malformed: bool,
}

impl Reply {
    fn result(request: &Value, result: Value) -> Self {
        Self {
            status: 200,
            body: json!({"jsonrpc":"2.0", "id":request["id"], "result":result}),
            retry_after: None,
            delay: Duration::ZERO,
            disconnect: false,
            malformed: false,
        }
    }

    fn error(request: &Value, code: i64) -> Self {
        let mut reply = Self::result(request, Value::Null);
        reply.body = json!({"jsonrpc":"2.0", "id":request["id"], "error":{
            "code":code,"message":"private-provider-message","data":"private-provider-data"
        }});
        reply
    }
}

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(handler: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/private-rpc-path", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let logged = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let logged = logged.clone();
                let handler = handler.clone();
                connections.spawn(async move {
                    let Some(request) = read_request(&mut stream).await else { return };
                    logged.lock().unwrap().push(request.clone());
                    let reply = handler(&request);
                    tokio::time::sleep(reply.delay).await;
                    if reply.disconnect { return; }
                    let body = if reply.malformed { "private-invalid-json".to_owned() } else { reply.body.to_string() };
                    let retry_after = reply.retry_after.map(|value| format!("Retry-After: {value}\r\n")).unwrap_or_default();
                    let response = format!("HTTP/1.1 {} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry_after}Connection: close\r\n\r\n{body}", reply.status, body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
                while connections.try_join_next().is_some() {}
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    fn count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == method)
            .count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Value> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 1024 * 1024 {
            return None;
        }
        let Some(separator) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..separator]).ok()?;
        let length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })?;
        let start = separator + 4;
        if bytes.len() >= start + length {
            return serde_json::from_slice(&bytes[start..start + length]).ok();
        }
    }
}

fn wire_fixture() -> &'static (String, String) {
    static WIRE: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    WIRE.get_or_init(|| {
        use crate::key::{MessageSigner, SolanaSigner};
        use solana_message::{Message, VersionedMessage};
        use solana_signature::Signature;
        let key = ed25519_dalek::SigningKey::from_bytes(&[29; 32]);
        let mut keypair = [29; 64];
        keypair[32..].copy_from_slice(key.verifying_key().as_bytes());
        let signer = SolanaSigner::parse(&bs58::encode(keypair).into_string()).unwrap();
        let payer = signer.address().parse().unwrap();
        let message = VersionedMessage::Legacy(Message::new_with_blockhash(
            &[],
            Some(&payer),
            &solana_hash::Hash::default(),
        ));
        let signature = Signature::from(
            signer
                .sign_message(&crate::transaction::codec().serialize(&message).unwrap())
                .unwrap(),
        );
        let transaction = solana_transaction::versioned::VersionedTransaction {
            signatures: vec![signature],
            message,
        };
        (
            signature.to_string(),
            STANDARD.encode(crate::transaction::codec().serialize(&transaction).unwrap()),
        )
    })
}

fn ordinary(request: &Value) -> Reply {
    let result = match request["method"].as_str().unwrap() {
        "getGenesisHash" => json!(MAINNET_GENESIS_HASH),
        "getSlot" => json!(2000),
        "getBlockTime" => json!(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ),
        "getBalance" => json!({"context":{"slot":2000},"value":42}),
        "getTransaction" => {
            json!({"slot":1990,"meta":{"err":null,"fee":5000,"preBalances":[10000],"postBalances":[5000],"preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]},"transaction":[wire_fixture().1,"base64"]})
        }
        "getSignatureStatuses" => {
            json!({"context":{"slot":2000},"value":[{"slot":1990,"err":null,"confirmationStatus":"finalized","confirmations":null}]})
        }
        "getSignaturesForAddress" => json!([]),
        _ => json!(true),
    };
    Reply::result(request, result)
}

fn pool(servers: &[&Server]) -> Arc<HttpRpc> {
    HttpRpc::isolated_localhost_pool(servers.iter().map(|server| server.url.clone()).collect())
        .unwrap()
}

#[test]
fn production_pool_rejects_unsafe_urls_without_disclosing_them() {
    for endpoint in [
        "http://api.mainnet-beta.solana.com",
        "https://user:private-secret@example.com",
        "https://example.com/?token=private-secret",
        "https://example.com/#private-secret",
        "not-a-url",
        "file:///private-secret",
        "https://",
    ] {
        let error = HttpRpc::new(vec![endpoint.into()])
            .err()
            .expect("unsafe URL accepted");
        assert!(!format!("{error:?}").contains("private-secret"));
    }
    assert!(HttpRpc::new(Vec::new()).is_err());
    assert!(HttpRpc::isolated_localhost_pool(vec!["http://example.com/rpc".into()]).is_err());
}

#[tokio::test]
async fn wrong_genesis_never_contributes_read_evidence() {
    let wrong = Server::start(|request| {
        if request["method"] == "getGenesisHash" {
            Reply::result(request, json!("5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"))
        } else {
            ordinary(request)
        }
    })
    .await;
    let good = Server::start(ordinary).await;
    assert_eq!(
        pool(&[&wrong, &good])
            .request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .unwrap()["value"],
        42
    );
    assert_eq!(wrong.count("getBalance"), 0);
    assert_eq!(good.count("getGenesisHash"), 1);
}

#[tokio::test]
async fn all_wrong_genesis_providers_fail_closed() {
    let wrong = Server::start(|request| Reply::result(request, json!("devnet-genesis"))).await;
    assert!(
        pool(&[&wrong])
            .request("getBalance", json!([]))
            .await
            .is_err()
    );
    assert_eq!(wrong.count("getBalance"), 0);
}

#[tokio::test]
async fn malformed_json_and_mismatched_envelopes_allow_read_failover() {
    for malformed in [true, false] {
        let bad = Server::start(move |request| {
            let mut reply = ordinary(request);
            if request["method"] == "getBalance" {
                reply.malformed = malformed;
                if !malformed {
                    reply.body["id"] = json!("wrong-id");
                }
            }
            reply
        })
        .await;
        let good = Server::start(ordinary).await;
        assert_eq!(
            pool(&[&bad, &good])
                .request(
                    "getBalance",
                    json!(["synthetic-owner",{"commitment":"confirmed"}])
                )
                .await
                .unwrap()["value"],
            42
        );
    }
}

#[tokio::test]
async fn rate_limit_fails_over_and_cooldown_prevents_repeat_requests() {
    let limited = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.status = 429;
            reply.retry_after = Some("60");
        }
        reply
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(40);
        }
        reply
    })
    .await;
    let rpc = pool(&[&limited, &good]);
    for _ in 0..2 {
        assert_eq!(
            rpc.request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .unwrap()["value"],
            42
        );
    }
    assert_eq!(limited.count("getBalance"), 1);
    assert_eq!(good.count("getBalance"), 2);
    let diagnostics = rpc.diagnostics().to_string();
    assert!(!diagnostics.contains("private-rpc-path"));
    assert!(!diagnostics.contains("127.0.0.1"));
}

#[tokio::test]
async fn unsupported_and_history_unavailable_errors_do_not_mask_other_provider() {
    for code in [-32601, -32004, -32005, -32007, -32009, -32016] {
        let bad = Server::start(move |request| {
            if request["method"] == "getTransaction" {
                Reply::error(request, code)
            } else {
                ordinary(request)
            }
        })
        .await;
        let good = Server::start(|request| {
            let mut reply = ordinary(request);
            reply.delay = Duration::from_millis(20);
            reply
        })
        .await;
        let rpc = pool(&[&bad, &good]);
        let result = rpc
            .request_checked(
                "getTransaction",
                json!([wire_fixture().0,{"commitment":"finalized"}]),
                RpcReadRequirements {
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result["slot"], 1990);
        let diagnostics = rpc.diagnostics().to_string();
        assert!(!diagnostics.contains("private-provider-message"));
        assert!(!diagnostics.contains("private-provider-data"));
    }
}

#[tokio::test]
async fn absent_or_nonfinal_signature_status_does_not_mask_finalized_evidence() {
    for status in [
        Value::Null,
        json!({"slot":1990,"err":null,"confirmationStatus":"confirmed","confirmations":1}),
    ] {
        let insufficient = Server::start(move |request| {
            if request["method"] == "getSignatureStatuses" {
                Reply::result(
                    request,
                    json!({"context":{"slot":2000},"value":[status.clone()]}),
                )
            } else {
                ordinary(request)
            }
        })
        .await;
        let good = Server::start(|request| {
            let mut reply = ordinary(request);
            if request["method"] == "getSignatureStatuses" {
                reply.delay = Duration::from_millis(30);
            }
            reply
        })
        .await;
        let result = pool(&[&insufficient, &good])
            .request_checked(
                "getSignatureStatuses",
                json!([["synthetic-signature"],{"searchTransactionHistory":true}]),
                RpcReadRequirements {
                    finalized_signature: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result["value"][0]["confirmationStatus"], "finalized");
    }
}

#[tokio::test]
async fn null_transaction_and_lookup_table_allow_non_null_failover() {
    for method in ["getTransaction", "getAccountInfo"] {
        let absent = Server::start(move |request| {
            if request["method"] == method {
                Reply::result(
                    request,
                    if method == "getAccountInfo" {
                        json!({"context":{"slot":2000},"value":null})
                    } else {
                        Value::Null
                    },
                )
            } else {
                ordinary(request)
            }
        })
        .await;
        let good = Server::start(move |request| {
            let mut reply = if request["method"] == "getAccountInfo" {
                Reply::result(
                    request,
                    json!({"context":{"slot":2000},"value":{"data":["AA==","base64"]}}),
                )
            } else {
                ordinary(request)
            };
            if request["method"] == method {
                reply.delay = Duration::from_millis(30);
            }
            reply
        })
        .await;
        let result = pool(&[&absent, &good])
            .request_checked(
                method,
                json!([if method == "getTransaction" {wire_fixture().0.as_str()} else {"synthetic-identity"},{"commitment":"finalized"}]),
                RpcReadRequirements {
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!result.is_null());
        if method == "getAccountInfo" {
            assert!(!result["value"].is_null());
        }
    }
}

#[tokio::test]
async fn lagging_context_and_explicit_minimum_context_do_not_mask_fresh_reads() {
    let lagging = Server::start(|request| {
        if request["method"] == "getBalance" {
            Reply::result(request, json!({"context":{"slot":1000},"value":1}))
        } else {
            ordinary(request)
        }
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    let params = json!(["synthetic-owner",{"commitment":"confirmed","minContextSlot":1999}]);
    assert_eq!(
        pool(&[&lagging, &good])
            .request_checked(
                "getBalance",
                params.clone(),
                RpcReadRequirements {
                    minimum_context_slot: Some(1999),
                    ..Default::default()
                }
            )
            .await
            .unwrap()["value"],
        42
    );
    for request in good
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request["method"] == "getBalance")
    {
        assert_eq!(request["params"], params);
    }
}

#[tokio::test]
async fn stale_or_future_head_cannot_supply_contextual_evidence() {
    for offset in [-120_i64, 120] {
        let stale = Server::start(move |request| {
            if request["method"] == "getBlockTime" {
                Reply::result(
                    request,
                    json!(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as i64
                            + offset
                    ),
                )
            } else if request["method"] == "getBalance" {
                Reply::result(request, json!({"context":{"slot":2000},"value":1}))
            } else {
                ordinary(request)
            }
        })
        .await;
        let good = Server::start(|request| {
            let mut reply = ordinary(request);
            reply.delay = Duration::from_millis(20);
            reply
        })
        .await;
        assert_eq!(
            pool(&[&stale, &good])
                .request(
                    "getBalance",
                    json!(["synthetic-owner",{"commitment":"confirmed"}])
                )
                .await
                .unwrap()["value"],
            42
        );
    }
}

fn history(signature: &str, slot: u64) -> Value {
    json!({"signature":signature,"slot":slot,"err":null,"confirmationStatus":"finalized","memo":null,"blockTime":null})
}

#[tokio::test]
async fn sponsored_history_unions_and_deduplicates_all_eligible_providers() {
    let first = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            Reply::result(
                request,
                json!([history("first", 1990), history("shared", 1989)]),
            )
        } else {
            ordinary(request)
        }
    })
    .await;
    let second = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            let mut reply = Reply::result(
                request,
                json!([history("shared", 1989), history("second", 1988)]),
            );
            reply.delay = Duration::from_millis(30);
            reply
        } else {
            ordinary(request)
        }
    })
    .await;
    let result = pool(&[&first, &second])
        .request_checked(
            "getSignaturesForAddress",
            json!(["synthetic-owner",{"commitment":"finalized"}]),
            RpcReadRequirements {
                merge_signature_history: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let entries = result.as_array().unwrap();
    assert_eq!(entries.len(), 3);
    for signature in ["first", "shared", "second"] {
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry["signature"] == signature)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn conflicting_history_is_not_accepted_as_consistent_evidence() {
    let first = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            Reply::result(request, json!([history("same", 1990)]))
        } else {
            ordinary(request)
        }
    })
    .await;
    let second = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            Reply::result(request, json!([history("same", 1980)]))
        } else {
            ordinary(request)
        }
    })
    .await;
    assert!(
        pool(&[&first, &second])
            .request_checked(
                "getSignaturesForAddress",
                json!(["synthetic-owner",{"commitment":"finalized"}]),
                RpcReadRequirements {
                    merge_signature_history: true,
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn primary_response_loss_never_dispatches_to_a_second_provider() {
    let primary = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "sendTransaction" {
            reply.disconnect = true;
        }
        reply
    })
    .await;
    let backup = Server::start(ordinary).await;
    let rpc = pool(&[&primary, &backup]);
    let params = json!(["synthetic-frozen-wire",{"encoding":"base64","maxRetries":0}]);
    assert!(
        rpc.request("sendTransaction", params.clone())
            .await
            .is_err()
    );
    assert_eq!(primary.count("sendTransaction"), 1);
    assert_eq!(backup.count("sendTransaction"), 0);
    let sent = primary
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["method"] == "sendTransaction")
        .cloned()
        .unwrap();
    assert_eq!(sent["params"], params);
    assert_eq!(
        rpc.request_checked(
            "getSignatureStatuses",
            json!([["synthetic-signature"],{"searchTransactionHistory":true}]),
            RpcReadRequirements {
                finalized_signature: true,
                ..Default::default()
            }
        )
        .await
        .unwrap()["value"][0]["confirmationStatus"],
        "finalized"
    );
    assert_eq!(primary.count("sendTransaction"), 1);
    assert_eq!(backup.count("sendTransaction"), 0);
}

#[tokio::test]
async fn wrong_genesis_primary_prevents_write_even_when_backup_is_healthy() {
    let primary = Server::start(|request| {
        if request["method"] == "getGenesisHash" {
            Reply::result(request, json!("wrong-network"))
        } else {
            ordinary(request)
        }
    })
    .await;
    let backup = Server::start(ordinary).await;
    assert!(
        pool(&[&primary, &backup])
            .request(
                "sendTransaction",
                json!(["synthetic-wire",{"maxRetries":0}])
            )
            .await
            .is_err()
    );
    assert_eq!(primary.count("sendTransaction"), 0);
    assert_eq!(backup.count("sendTransaction"), 0);
}

#[tokio::test]
async fn stalled_provider_cannot_delay_a_usable_backup_or_outlive_cancellation() {
    let stalled = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let good = Server::start(ordinary).await;
    let started = tokio::time::Instant::now();
    assert_eq!(
        pool(&[&stalled, &good])
            .request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .unwrap()["value"],
        42
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let rpc = pool(&[&stalled]);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            rpc.request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
        )
        .await
        .is_err()
    );
    let count = stalled.count("getBalance");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        stalled.count("getBalance"),
        count,
        "cancelled caller must not schedule retries"
    );
}

#[tokio::test]
async fn all_stalled_providers_are_bounded_by_the_attempt_and_read_budget() {
    let first = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let second = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let started = tokio::time::Instant::now();
    assert!(
        pool(&[&first, &second])
            .request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "provider attempts must run concurrently with six-second limits"
    );
    assert_eq!(first.count("getBalance"), 1);
    assert_eq!(second.count("getBalance"), 1);
}

#[tokio::test]
async fn rate_limit_cooldown_expires_and_provider_becomes_eligible_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    let limited = Server::start(move |request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" && observed.fetch_add(1, Ordering::Relaxed) == 0 {
            reply.status = 429;
            reply.retry_after = Some("1");
        }
        reply
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    let rpc = pool(&[&limited, &good]);
    let params = json!(["synthetic-owner",{"commitment":"confirmed"}]);
    rpc.request("getBalance", params.clone()).await.unwrap();
    rpc.request("getBalance", params.clone()).await.unwrap();
    assert_eq!(limited.count("getBalance"), 1);
    tokio::time::sleep(Duration::from_millis(1050)).await;
    rpc.request("getBalance", params).await.unwrap();
    assert_eq!(limited.count("getBalance"), 2);
}

#[tokio::test]
async fn rate_limits_and_server_errors_never_replay_financial_submission() {
    for status in [429, 503] {
        let primary = Server::start(move |request| {
            let mut reply = ordinary(request);
            if request["method"] == "sendTransaction" {
                reply.status = status;
                reply.retry_after = Some("0");
            }
            reply
        })
        .await;
        let backup = Server::start(ordinary).await;
        let rpc = pool(&[&primary, &backup]);
        assert!(
            rpc.request(
                "sendTransaction",
                json!(["synthetic-wire",{"maxRetries":0}])
            )
            .await
            .is_err()
        );
        assert_eq!(primary.count("sendTransaction"), 1);
        assert_eq!(backup.count("sendTransaction"), 0);
    }
}

#[tokio::test]
async fn no_more_than_two_read_providers_are_started_at_once() {
    let first = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.status = 503;
            reply.delay = Duration::from_millis(250);
        }
        reply
    })
    .await;
    let second = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_secs(2);
        }
        reply
    })
    .await;
    let third = Server::start(ordinary).await;
    let rpc = pool(&[&first, &second, &third]);
    let read = tokio::spawn(async move {
        rpc.request(
            "getBalance",
            json!(["synthetic-owner",{"commitment":"confirmed"}]),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(first.count("getBalance"), 1);
    assert_eq!(second.count("getBalance"), 1);
    assert_eq!(third.count("getBalance"), 0);
    assert_eq!(read.await.unwrap().unwrap()["value"], 42);
    assert_eq!(third.count("getBalance"), 1);
}

#[tokio::test]
async fn simulation_rejection_is_returned_as_financial_evidence_without_retry() {
    let primary = Server::start(|request| if request["method"] == "simulateTransaction" {
        Reply::result(request,json!({"context":{"slot":2000},"value":{"err":{"InstructionError":[0,"InsufficientFunds"]},"unitsConsumed":0,"logs":[]}}))
    } else {ordinary(request)}).await;
    let rpc = pool(&[&primary]);
    let result = rpc
        .request(
            "simulateTransaction",
            json!(["synthetic-wire",{"commitment":"confirmed","sigVerify":true}]),
        )
        .await
        .unwrap();
    assert_eq!(
        result["value"]["err"]["InstructionError"][1],
        "InsufficientFunds"
    );
    assert_eq!(primary.count("simulateTransaction"), 1);
    assert_eq!(primary.count("sendTransaction"), 0);
}

#[tokio::test]
async fn retry_after_dates_invalid_headers_and_missing_headers_keep_provider_cooled_down() {
    for retry_after in [
        Some("Fri, 01 Jan 2050 00:00:00 GMT"),
        Some("invalid-private-header"),
        None,
    ] {
        let limited = Server::start(move |request| {
            let mut reply = ordinary(request);
            if request["method"] == "getBalance" {
                reply.status = 429;
                reply.retry_after = retry_after;
            }
            reply
        })
        .await;
        let good = Server::start(|request| {
            let mut reply = ordinary(request);
            if request["method"] == "getBalance" {
                reply.delay = Duration::from_millis(20);
            }
            reply
        })
        .await;
        let rpc = pool(&[&limited, &good]);
        let params = json!(["synthetic-owner",{"commitment":"confirmed"}]);
        rpc.request("getBalance", params.clone()).await.unwrap();
        rpc.request("getBalance", params).await.unwrap();
        assert_eq!(limited.count("getBalance"), 1);
        assert!(
            !rpc.diagnostics()
                .to_string()
                .contains("invalid-private-header")
        );
    }
}

#[tokio::test]
async fn cooldown_is_capped_at_twenty_four_hours() {
    let limited = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.status = 429;
            reply.retry_after = Some("999999999");
        }
        reply
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(20);
        }
        reply
    })
    .await;
    let rpc = pool(&[&limited, &good]);
    let params = json!(["synthetic-owner",{"commitment":"confirmed"}]);
    rpc.request("getBalance", params.clone()).await.unwrap();
    assert_eq!(limited.count("getBalance"), 1);
    // Advance only between network calls, and restore real scheduling before
    // performing IO, so virtual-time auto advancement cannot race socket readiness.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(24 * 60 * 60 + 1)).await;
    tokio::time::resume();
    rpc.request("getBalance", params).await.unwrap();
    assert_eq!(limited.count("getBalance"), 2);
}

#[tokio::test]
async fn confirmed_and_finalized_heads_have_independent_freshness_observations() {
    let server = Server::start(|request| {
        let method = request["method"].as_str().unwrap();
        let commitment = if method == "getSlot" {
            request["params"][0]["commitment"].as_str()
        } else {
            request["params"][1]["commitment"].as_str()
        };
        let slot = if commitment == Some("finalized") {
            1900
        } else {
            2000
        };
        if method == "getSlot" {
            Reply::result(request, json!(slot))
        } else if method == "getBalance" {
            Reply::result(request, json!({"context":{"slot":slot},"value":42}))
        } else {
            ordinary(request)
        }
    })
    .await;
    let rpc = pool(&[&server]);
    assert_eq!(
        rpc.request(
            "getBalance",
            json!(["synthetic-owner",{"commitment":"confirmed"}])
        )
        .await
        .unwrap()["context"]["slot"],
        2000
    );
    assert_eq!(
        rpc.request(
            "getBalance",
            json!(["synthetic-owner",{"commitment":"finalized"}])
        )
        .await
        .unwrap()["context"]["slot"],
        1900
    );
    assert_eq!(
        rpc.request(
            "getBalance",
            json!(["synthetic-owner",{"commitment":"confirmed"}])
        )
        .await
        .unwrap()["context"]["slot"],
        2000
    );
}

#[tokio::test]
async fn missing_results_and_wrong_protocol_versions_cannot_supply_evidence() {
    for invalid in ["missing_result", "version", "both_result_and_error"] {
        let bad=Server::start(move |request| {
            let mut reply=ordinary(request);
            if request["method"]=="getBalance" {
                reply.body=match invalid {
                    "missing_result"=>json!({"jsonrpc":"2.0","id":request["id"]}),
                    "version"=>json!({"jsonrpc":"1.0","id":request["id"],"result":{"context":{"slot":2000},"value":1}}),
                    _=>json!({"jsonrpc":"2.0","id":request["id"],"result":{"context":{"slot":2000},"value":1},"error":{"code":-32004,"message":"private-provider-message"}}),
                };
            }
            reply
        }).await;
        let good = Server::start(|request| {
            let mut reply = ordinary(request);
            if request["method"] == "getBalance" {
                reply.delay = Duration::from_millis(20);
            }
            reply
        })
        .await;
        assert_eq!(
            pool(&[&bad, &good])
                .request(
                    "getBalance",
                    json!(["synthetic-owner",{"commitment":"confirmed"}])
                )
                .await
                .unwrap()["value"],
            42
        );
    }
}

#[tokio::test]
async fn oversized_provider_response_is_rejected_before_failover() {
    let oversized = Server::start(|request| {
        if request["method"] == "getBalance" {
            Reply::result(request, json!("x".repeat(2 * 1024 * 1024)))
        } else {
            ordinary(request)
        }
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    assert_eq!(
        pool(&[&oversized, &good])
            .request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .unwrap()["value"],
        42
    );
}

#[test]
fn contradictory_finalized_statuses_in_a_completed_batch_fail_closed() {
    let finalized = json!({"context":{"slot":2000},"value":[{"slot":1990,"err":null,"confirmationStatus":"finalized","confirmations":null}]});
    for conflicting in [
        json!({"context":{"slot":2000},"value":[{"slot":1989,"err":null,"confirmationStatus":"finalized","confirmations":null}]}),
        json!({"context":{"slot":2000},"value":[{"slot":1990,"err":{"InstructionError":[0,"Custom"]},"confirmationStatus":"finalized","confirmations":null}]}),
    ] {
        assert!(super::responses_conflict(
            "getSignatureStatuses",
            &[&finalized, &conflicting]
        ));
    }
    let absent = json!({"context":{"slot":2000},"value":[null]});
    assert!(!super::responses_conflict(
        "getSignatureStatuses",
        &[&finalized, &absent]
    ));
}

#[tokio::test]
async fn same_provider_context_regression_uses_other_provider_evidence() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let reads = Arc::new(AtomicUsize::new(0));
    let seen = reads.clone();
    let regressing=Server::start(move |request| {
        if request["method"]=="getBalance" {
            let first=seen.fetch_add(1,Ordering::Relaxed)==0;
            Reply::result(request,json!({"context":{"slot":if first {2000} else {1999}},"value":if first {42} else {1}}))
        } else {ordinary(request)}
    }).await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(40);
        }
        reply
    })
    .await;
    let rpc = pool(&[&regressing, &good]);
    let params = json!(["synthetic-owner",{"commitment":"confirmed"}]);
    assert_eq!(
        rpc.request("getBalance", params.clone()).await.unwrap()["value"],
        42
    );
    assert_eq!(
        rpc.request("getBalance", params).await.unwrap()["value"],
        42
    );
}

#[tokio::test]
async fn context_far_ahead_of_same_provider_head_cannot_poison_other_providers() {
    let future = Server::start(|request| {
        if request["method"] == "getBalance" {
            Reply::result(request, json!({"context":{"slot":9000},"value":1}))
        } else {
            ordinary(request)
        }
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getBalance" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    let rpc = pool(&[&future, &good]);
    for _ in 0..2 {
        assert_eq!(
            rpc.request(
                "getBalance",
                json!(["synthetic-owner",{"commitment":"confirmed"}])
            )
            .await
            .unwrap()["value"],
            42
        );
    }
}

#[tokio::test]
async fn financial_transport_forces_zero_node_retries_and_preserves_other_options() {
    let primary = Server::start(|request| {
        if request["method"] == "sendTransaction" {
            Reply::result(request, json!(wire_fixture().0))
        } else {
            ordinary(request)
        }
    })
    .await;
    let backup = Server::start(ordinary).await;
    let rpc = pool(&[&primary, &backup]);
    rpc.request(
        "sendTransaction",
        json!(["synthetic-frozen-wire",{"encoding":"base64","maxRetries":5,"skipPreflight":false}]),
    )
    .await
    .unwrap();
    let sent = primary
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["method"] == "sendTransaction")
        .cloned()
        .unwrap();
    assert_eq!(
        sent["params"],
        json!(["synthetic-frozen-wire",{"encoding":"base64","maxRetries":0,"skipPreflight":false}])
    );
    assert_eq!(primary.count("sendTransaction"), 1);
    assert_eq!(backup.count("sendTransaction"), 0);
}

#[tokio::test]
async fn transaction_missing_metadata_does_not_mask_a_complete_receipt() {
    let incomplete = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getTransaction" {
            reply.body["result"]["meta"] = Value::Null;
        }
        reply
    })
    .await;
    let good = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getTransaction" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    let result=pool(&[&incomplete,&good]).request_checked("getTransaction",json!([wire_fixture().0,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]),RpcReadRequirements {require_non_null:true,..Default::default()}).await.unwrap();
    assert_eq!(result["meta"]["fee"], 5000);
}

#[tokio::test]
async fn history_provider_cannot_exceed_the_requested_search_bound() {
    let excessive = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            Reply::result(request, json!([history("one", 1990), history("two", 1989)]))
        } else {
            ordinary(request)
        }
    })
    .await;
    let good = Server::start(|request| {
        if request["method"] == "getSignaturesForAddress" {
            Reply::result(request, json!([history("bounded", 1990)]))
        } else {
            ordinary(request)
        }
    })
    .await;
    let result = pool(&[&excessive, &good])
        .request_checked(
            "getSignaturesForAddress",
            json!(["synthetic-owner",{"limit":1,"commitment":"finalized"}]),
            RpcReadRequirements {
                merge_signature_history: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result, json!([history("bounded", 1990)]));
}

fn current_blockhash(request: &Value) -> Reply {
    Reply::result(
        request,
        json!({"context":{"slot":2000},"value":{"blockhash":bs58::encode([0u8;32]).into_string(),"lastValidBlockHeight":1150}}),
    )
}

#[tokio::test]
async fn slot_reported_as_height_cannot_poison_valid_provider_height() {
    let primary = Server::start(|request| match request["method"].as_str().unwrap() {
        "getBlockHeight" => Reply::result(request, json!(2000)),
        "getLatestBlockhash" => current_blockhash(request),
        _ => ordinary(request),
    })
    .await;
    let backup = Server::start(|request| match request["method"].as_str().unwrap() {
        "getBlockHeight" => {
            let mut reply = Reply::result(request, json!(1000));
            reply.delay = Duration::from_millis(25);
            reply
        }
        "getLatestBlockhash" => current_blockhash(request),
        _ => ordinary(request),
    })
    .await;
    let rpc = pool(&[&primary, &backup]);
    let params = json!([{"commitment":"confirmed","minContextSlot":2000}]);
    for _ in 0..2 {
        assert_eq!(
            rpc.request("getBlockHeight", params.clone()).await.unwrap(),
            1000
        );
        let health = rpc.diagnostics();
        assert_eq!(health["last_read"]["selected_endpoint"], "rpc-2");
        assert_eq!(health["last_read"]["attempts"][0]["reason"], "incompatible");
        assert_eq!(
            health["endpoints"][0]["last_error_code"],
            "SOLANA_RPC_DATA_INVALID"
        );
        assert_eq!(rpc.selection.lock().unwrap().heights["confirmed"], 1000);
    }
    for server in [&primary, &backup] {
        for request in server.requests.lock().unwrap().iter().filter(|request| {
            matches!(
                request["method"].as_str(),
                Some("getBlockHeight" | "getLatestBlockhash")
            )
        }) {
            assert_eq!(request["params"], params);
        }
        assert_eq!(server.count("sendTransaction"), 0);
    }
}

#[tokio::test]
async fn stalled_height_consistency_check_does_not_delay_healthy_backup() {
    let primary = Server::start(|request| match request["method"].as_str().unwrap() {
        "getBlockHeight" => Reply::result(request, json!(1000)),
        "getLatestBlockhash" => {
            let mut reply = current_blockhash(request);
            reply.delay = Duration::from_secs(60);
            reply
        }
        _ => ordinary(request),
    })
    .await;
    let backup = Server::start(|request| match request["method"].as_str().unwrap() {
        "getBlockHeight" => Reply::result(request, json!(1000)),
        "getLatestBlockhash" => current_blockhash(request),
        _ => ordinary(request),
    })
    .await;
    let rpc = pool(&[&primary, &backup]);
    let started = tokio::time::Instant::now();
    assert_eq!(
        rpc.request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await
            .unwrap(),
        1000
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-2");
}

#[tokio::test]
async fn independent_read_scopes_can_observe_different_fresh_banks() {
    let first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = first.clone();
    let server = Server::start(move |request| {
        if request["method"] == "getBalance" {
            let slot = if request["params"][0] == "scope-a"
                && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                2001
            } else {
                2000
            };
            return Reply::result(request, json!({"context":{"slot":slot},"value":42}));
        }
        if request["method"] == "getAccountInfo" {
            return Reply::result(
                request,
                json!({"context":{"slot":2000},"value":{"owner":"public-owner"}}),
            );
        }
        ordinary(request)
    })
    .await;
    let rpc = pool(&[&server]);
    let a = json!(["scope-a",{"commitment":"finalized"}]);
    assert!(rpc.request("getBalance", a.clone()).await.is_ok());
    assert!(
        rpc.request(
            "getAccountInfo",
            json!(["scope-a",{"commitment":"finalized"}])
        )
        .await
        .is_ok()
    );
    assert!(
        rpc.request("getBalance", json!(["scope-b",{"commitment":"finalized"}]))
            .await
            .is_ok()
    );
    let error = rpc.request("getBalance", a).await.unwrap_err();
    assert_eq!(error.code, "SOLANA_RPC_STALE");
    assert_eq!(
        error.details.as_deref().unwrap()["stage"],
        "context_regression"
    );
}

#[tokio::test]
async fn response_scope_watermarks_are_bounded_and_do_not_store_request_data() {
    let server = Server::start(ordinary).await;
    let rpc = pool(&[&server]);
    for index in 0..130 {
        rpc.request(
            "getBalance",
            json!([format!("private-scope-{index}"),{"commitment":"finalized"}]),
        )
        .await
        .unwrap();
    }
    let state = rpc.selection.lock().unwrap();
    assert_eq!(state.response_slots.len(), super::MAX_RESPONSE_SCOPES);
    assert_eq!(state.response_order.len(), super::MAX_RESPONSE_SCOPES);
    assert!(
        state
            .response_slots
            .keys()
            .all(|(_, scope)| scope.len() == 64
                && scope.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );
}

#[tokio::test]
async fn four_provider_failover_reaches_last_after_two_stalls_and_missing_status() {
    let first = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getSignatureStatuses" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let second = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getSignatureStatuses" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let missing = Server::start(|request| {
        if request["method"] == "getSignatureStatuses" {
            Reply::result(request, json!({"context":{"slot":2000},"value":[null]}))
        } else {
            ordinary(request)
        }
    })
    .await;
    let last = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getSignatureStatuses" {
            reply.delay = Duration::from_millis(30);
        }
        reply
    })
    .await;
    let servers = [&first, &second, &missing, &last];
    let rpc = pool(&servers);
    let params = json!([["synthetic-signature"],{"searchTransactionHistory":true}]);
    let started = tokio::time::Instant::now();
    let result = rpc
        .request_checked(
            "getSignatureStatuses",
            params.clone(),
            RpcReadRequirements {
                finalized_signature: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result["value"][0]["confirmationStatus"], "finalized");
    assert!(started.elapsed() < Duration::from_secs(9));
    assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-4");
    for server in servers {
        assert_eq!(server.count("getSignatureStatuses"), 1);
        assert_eq!(server.count("sendTransaction"), 0);
        for request in server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "getSignatureStatuses")
        {
            assert_eq!(request["params"], params);
        }
    }
}

#[tokio::test]
async fn four_provider_failover_replaces_rate_limit_and_missing_evidence_while_primary_stalls() {
    let stalled = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getTransaction" {
            reply.delay = Duration::from_secs(60);
        }
        reply
    })
    .await;
    let limited = Server::start(|request| {
        let mut reply = ordinary(request);
        if request["method"] == "getTransaction" {
            reply.status = 429;
            reply.retry_after = Some("60");
        }
        reply
    })
    .await;
    let missing = Server::start(|request| {
        if request["method"] == "getTransaction" {
            Reply::result(request, Value::Null)
        } else {
            ordinary(request)
        }
    })
    .await;
    let last = Server::start(ordinary).await;
    let servers = [&stalled, &limited, &missing, &last];
    let rpc = pool(&servers);
    let params = json!([wire_fixture().0,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]);
    let started = tokio::time::Instant::now();
    let result = rpc
        .request_checked(
            "getTransaction",
            params.clone(),
            RpcReadRequirements {
                require_non_null: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result["slot"], 1990);
    assert_eq!(result["transaction"][0], wire_fixture().1);
    assert!(started.elapsed() < Duration::from_secs(2));
    let diagnostics = rpc.diagnostics();
    assert_eq!(diagnostics["last_read"]["selected_endpoint"], "rpc-4");
    assert_eq!(diagnostics["endpoints"][1]["status"], "cooling_down");
    for server in servers {
        assert_eq!(server.count("getTransaction"), 1);
        assert_eq!(server.count("sendTransaction"), 0);
        for request in server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "getTransaction")
        {
            assert_eq!(request["params"], params);
        }
    }
}

#[tokio::test]
async fn four_provider_financial_submission_uses_primary_once_despite_response_loss_or_failure() {
    for failure in ["response_loss", "rate_limit", "server_error"] {
        let primary = Server::start(move |request| {
            let mut reply = ordinary(request);
            if request["method"] == "sendTransaction" {
                match failure {
                    "response_loss" => reply.disconnect = true,
                    "rate_limit" => {
                        reply.status = 429;
                        reply.retry_after = Some("0");
                    }
                    _ => reply.status = 503,
                }
            }
            reply
        })
        .await;
        let second = Server::start(ordinary).await;
        let third = Server::start(ordinary).await;
        let fourth = Server::start(ordinary).await;
        let rpc = pool(&[&primary, &second, &third, &fourth]);
        assert!(
            rpc.request(
                "sendTransaction",
                json!(["synthetic-frozen-wire",{"encoding":"base64","maxRetries":5}]),
            )
            .await
            .is_err()
        );
        assert_eq!(primary.count("sendTransaction"), 1);
        assert_eq!(primary.count("getGenesisHash"), 1);
        let requests = primary.requests.lock().unwrap();
        let sent = requests
            .iter()
            .find(|request| request["method"] == "sendTransaction")
            .unwrap();
        assert_eq!(
            sent["params"],
            json!(["synthetic-frozen-wire",{"encoding":"base64","maxRetries":0}])
        );
        for backup in [&second, &third, &fourth] {
            assert_eq!(backup.count("sendTransaction"), 0);
            assert_eq!(backup.count("getGenesisHash"), 0);
        }
    }
}
