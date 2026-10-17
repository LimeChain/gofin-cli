use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

mod eligibility;
mod racing;
mod submission_recovery;

type ReplyHook = Box<dyn FnOnce(&Value) -> Value + Send>;

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    result: Value,
    error: Option<Value>,
    delay: Duration,
    disconnect: bool,
    on_request: Option<ReplyHook>,
}

impl Reply {
    fn result(result: &str) -> Self {
        Self {
            status: 200,
            headers: vec![],
            result: json!(result),
            error: None,
            delay: Duration::ZERO,
            disconnect: false,
            on_request: None,
        }
    }

    fn rate_limit(retry_after: Option<&str>) -> Self {
        Self {
            status: 429,
            headers: retry_after
                .into_iter()
                .map(|value| ("Retry-After".into(), value.into()))
                .collect(),
            result: Value::Null,
            error: None,
            delay: Duration::ZERO,
            disconnect: false,
            on_request: None,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn rpc_error(message: &str) -> Self {
        let mut reply = Self::result("0x");
        reply.error = Some(json!({"code":-32000,"message":message}));
        reply
    }

    fn json_rpc_rate_limit() -> Self {
        let mut reply = Self::result("0x");
        reply.error = Some(json!({"code":-32005,"message":"rate limited"}));
        reply
    }

    fn computed(on_request: impl FnOnce(&Value) -> Value + Send + 'static) -> Self {
        Self {
            status: 200,
            headers: vec![],
            result: Value::Null,
            error: None,
            delay: Duration::ZERO,
            disconnect: false,
            on_request: Some(Box::new(on_request)),
        }
    }

    fn lost_response(on_request: impl FnOnce(&Value) -> Value + Send + 'static) -> Self {
        let mut reply = Self::computed(on_request);
        reply.disconnect = true;
        reply
    }
}

// Keep the endpoint alive, so skipping it cannot be confused with a closed socket.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn new(replies: Vec<Reply>) -> Self {
        let mut replies = replies.into_iter();
        Self::handling(move |_| replies.next().expect("unexpected RPC fixture request"))
    }

    fn handling(mut response: impl FnMut(&Value) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let captured = requests.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        // Racing can cancel a losing request during connection
                        // setup, before its complete HTTP body reaches us.
                        let Some(bytes) = read_fixture_request(&mut stream, &stop) else {
                            continue;
                        };
                        let request: Value = serde_json::from_slice(&bytes).unwrap();
                        captured.lock().unwrap().push(request.clone());
                        let mut reply = response(&request);
                        if let Some(on_request) = reply.on_request.take() {
                            reply.result = on_request(&request);
                        }
                        if reply.disconnect {
                            // The request was accepted, but no HTTP response reaches the client.
                            continue;
                        }
                        let ready_at = Instant::now() + reply.delay;
                        while Instant::now() < ready_at && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let body = match reply.error {
                            Some(error) => {
                                json!({"jsonrpc":"2.0","id":request["id"],"error":error})
                            }
                            None => {
                                json!({"jsonrpc":"2.0","id":request["id"],"result":reply.result})
                            }
                        }
                        .to_string();
                        let sent = (|| -> std::io::Result<()> {
                            write!(
                                stream,
                                "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                                reply.status,
                                body.len()
                            )?;
                            for (name, value) in reply.headers {
                                write!(stream, "{name}: {value}\r\n")?;
                            }
                            write!(stream, "\r\n{body}")?;
                            stream.flush()
                        })();
                        if let Err(error) = sent {
                            // A losing or timed-out client may close before its reply.
                            assert!(matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("RPC fixture listener failed: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn rpc(&self) -> HttpRpc {
        HttpRpc::isolated_localhost(self.url.clone(), 1).unwrap()
    }
}

fn read_fixture_request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<Vec<u8>> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return None,
            Err(error) => panic!("RPC fixture request failed: {error}"),
        }
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            if bytes.len() >= body_start + content_length {
                return Some(bytes[body_start..body_start + content_length].to_vec());
            }
        }
    }
    None
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("RPC fixture worker failed");
            }
        }
    }
}

fn balance_params() -> Value {
    json!(["0x0000000000000000000000000000000000000002", "0x10"])
}

#[tokio::test]
async fn two_pruned_endpoints_reach_archive_with_same_canonical_block() {
    let unavailable = |message: &str| {
        vec![
            Reply::result("0x1"),
            Reply::rpc_error(message),
            Reply::rpc_error(message),
            Reply::rpc_error(message),
        ]
    };
    let primary = Server::new(unavailable("missing trie node SYNTHETIC_ROOT"));
    let backup = Server::new(unavailable(
        "historical state 61a3d7d087734a9401c2fdfdb914fb3085bce806bb4c7f432be010eefea0e042 is not available",
    ));
    let archive = Server::new(vec![
        Reply::result("0x1"),
        // Both pruned endpoints must finish their bounded retries before
        // archive evidence wins and cancels any remaining read.
        Reply::result("0x2a").delayed(Duration::from_millis(100)),
    ]);
    let rpc = HttpRpc::isolated_localhost_pool(
        vec![primary.url.clone(), backup.url.clone(), archive.url.clone()],
        1,
    )
    .unwrap();
    let params = json!([
        {"to":"0x82af49447d8a07e3bd95bd0d56f35241523fbab1", "data":"0x18160ddd"},
        {"blockHash":format!("0x{}", "11".repeat(32)), "requireCanonical":true}
    ]);
    assert_eq!(
        rpc.request("eth_call", params.clone()).await.unwrap(),
        json!("0x2a")
    );
    for (server, expected_calls) in [(&primary, 3), (&backup, 3), (&archive, 1)] {
        let requests = server.requests();
        assert_eq!(requests.len(), expected_calls + 1);
        assert_eq!(requests[0]["method"], "eth_chainId");
        for request in &requests[1..] {
            assert_eq!(request["method"], "eth_call");
            assert_eq!(request["params"], params);
        }
    }
}

#[tokio::test]
async fn slow_primary_read_loses_to_faster_backup() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xaa").delayed(Duration::from_secs(2)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(30)),
    ]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(600),
            rpc.request("eth_getBalance", balance_params()),
        )
        .await
        .expect("a faster valid backup should finish the read")
        .unwrap(),
        json!("0x2a")
    );
    for server in [&primary, &backup] {
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["method"], "eth_chainId");
        assert_eq!(requests[1]["method"], "eth_getBalance");
        assert_eq!(requests[1]["params"], balance_params());
    }
}

#[tokio::test]
async fn slow_primary_identity_check_loses_to_verified_backup() {
    let primary = Server::new(vec![Reply::result("0x1").delayed(Duration::from_secs(2))]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(30)),
    ]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(600),
            rpc.request("eth_getBalance", balance_params()),
        )
        .await
        .expect("a verified backup should beat slow primary identity")
        .unwrap(),
        json!("0x2a")
    );
    let requests = primary.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "eth_chainId");
    let requests = backup.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["method"], "eth_chainId");
    assert_eq!(requests[1]["params"], balance_params());
}

#[tokio::test]
async fn caller_can_cancel_when_all_providers_remain_pending() {
    let replies = || {
        vec![
            Reply::result("0x1"),
            Reply::result("0xaa").delayed(Duration::from_secs(2)),
        ]
    };
    let primary = Server::new(replies());
    let backup = Server::new(replies());
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    // This timeout belongs to the test's caller. GoFin has no read deadline.
    let cancellation = tokio::time::timeout(
        Duration::from_millis(100),
        rpc.request("eth_getBalance", balance_params()),
    )
    .await;
    assert!(cancellation.is_err());
    let health = rpc.diagnostics();
    let trace = &health["last_read"];
    assert_eq!(trace["outcome"], "cancelled");
    assert_eq!(trace["selected_endpoint"], Value::Null);
    assert_eq!(
        trace["attempts"],
        json!([{"endpoint_id":"rpc-1","reason":"cancelled"},{"endpoint_id":"rpc-2","reason":"cancelled"}])
    );
    // Cancelled scoped futures must not schedule another HTTP request or retry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let encoded = health.to_string();
    for server in [&primary, &backup] {
        assert_eq!(server.requests().len(), 2, "both providers must be tried");
        assert!(!encoded.contains(&server.url));
    }
}

#[tokio::test]
async fn slow_backup_can_finish_after_fast_primary_failure() {
    let primary = Server::new(vec![Reply::result("0x1"), Reply::rate_limit(Some("120"))]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(400)),
    ]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0x2a")
    );
    assert_eq!(primary.requests().len(), 2);
    assert_eq!(backup.requests().len(), 2);
}

#[tokio::test]
async fn slow_primary_does_not_allow_wrong_chain_backup() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xaa").delayed(Duration::from_millis(200)),
    ]);
    let backup = Server::new(vec![Reply::result("0x2")]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0xaa")
    );
    assert!(rpc.diagnostics()["last_read"]["attempts"]
        .as_array().unwrap().iter().any(|attempt| attempt["endpoint_id"] == "rpc-2"
            && attempt["reason"] == "incompatible"));
    assert_eq!(primary.requests().len(), 2);
    assert_eq!(
        backup.requests().len(),
        1,
        "do not read balances on the wrong chain"
    );
}

#[tokio::test]
async fn slow_broadcast_is_not_cancelled_or_redirected_after_identity_is_checked() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x1234").delayed(Duration::from_millis(300)),
    ]);
    let backup = Server::new(vec![]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    rpc.write_identity_timeout = Duration::from_millis(100);
    let input = json!(["0xdeadbeef"]);

    assert_eq!(
        rpc.request("eth_sendRawTransaction", input.clone())
            .await
            .unwrap(),
        json!("0x1234")
    );
    let requests = primary.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["method"], "eth_sendRawTransaction");
    assert_eq!(requests[1]["params"], input);
    assert!(backup.requests().is_empty());
}

#[tokio::test]
async fn retry_after_seconds_controls_cooldown() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rate_limit(Some("120")),
        Reply::rate_limit(Some("120")),
        Reply::rate_limit(Some("120")),
        Reply::result("0xaa"),
    ]);
    let rpc = primary.rpc();
    let error = rpc
        .request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    assert_eq!(error.details.as_deref().unwrap()["http_status"], 429);
    let error = rpc
        .request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    assert_eq!(error.code, "RPC_RATE_LIMITED");
    let remaining = error.details.as_deref().unwrap()["retry_after_ms"]
        .as_u64()
        .unwrap();
    assert!(
        (119_000..=120_000).contains(&remaining),
        "expected 120 seconds, got {remaining} ms"
    );
    assert_eq!(
        primary.requests().len(),
        2,
        "do not retry before Retry-After"
    );
}

fn headers(value: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
    headers
}

#[test]
fn retry_after_parses_seconds_and_all_http_date_formats() {
    let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
    for (value, seconds) in [("120", 120), ("5", 5), ("0", 0), ("  005\t", 5)] {
        assert_eq!(
            retry_after_delay(&headers(value), now),
            Some(Duration::from_secs(seconds))
        );
    }
    for value in [
        "Wed, 21 Oct 2015 07:30:00 GMT",
        "Wednesday, 21-Oct-15 07:30:00 GMT",
        "Wed Oct 21 07:30:00 2015",
    ] {
        assert_eq!(
            retry_after_delay(&headers(value), now),
            Some(Duration::from_secs(120)),
            "{value}"
        );
    }
    assert_eq!(
        retry_after_delay(&headers("Wed, 21 Oct 2015 07:27:00 GMT"), now),
        Some(Duration::ZERO)
    );
    assert_eq!(
        retry_after_delay(&headers("Wed, 21 Oct 2015 07:28:00 GMT"), now),
        Some(Duration::ZERO)
    );
}

#[test]
fn invalid_retry_after_is_ignored_and_excessive_delays_are_bounded() {
    let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
    for value in [
        "",
        "-1",
        "+1",
        "1.5",
        "1e3",
        "\"120\"",
        "120, 240",
        "PRIVATE_INVALID_VALUE",
    ] {
        assert_eq!(retry_after_delay(&headers(value), now), None, "{value}");
    }
    assert_eq!(
        retry_after_delay(&reqwest::header::HeaderMap::new(), now),
        None
    );
    let mut repeated = headers("120");
    repeated.append(reqwest::header::RETRY_AFTER, "240".parse().unwrap());
    assert_eq!(retry_after_delay(&repeated, now), None);
    let mut non_text = reqwest::header::HeaderMap::new();
    non_text.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    assert_eq!(retry_after_delay(&non_text, now), None);
    for value in [
        "86401",
        "18446744073709551615",
        "18446744073709551615000",
        "Fri, 31 Dec 9999 23:59:59 GMT",
    ] {
        assert_eq!(
            retry_after_delay(&headers(value), now),
            Some(MAX_RPC_RATE_LIMIT_COOLDOWN),
            "{value}"
        );
    }
}

#[tokio::test]
async fn retry_after_http_date_controls_cooldown() {
    let target = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
    let primary = Server::new(vec![Reply::result("0x1"), Reply::rate_limit(Some(&target))]);
    let rpc = primary.rpc();
    let error = rpc
        .request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    let remaining = error.details.as_deref().unwrap()["retry_after_ms"]
        .as_u64()
        .unwrap();
    assert!((118_000..=120_000).contains(&remaining));
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap_err()
            .code,
        "RPC_RATE_LIMITED"
    );
    assert_eq!(primary.requests().len(), 2);
}

#[tokio::test]
async fn retry_after_fails_over_skips_primary_and_allows_return_after_expiry() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rate_limit(Some("120")),
        Reply::result("0xaa").delayed(Duration::from_millis(50)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        // Let A's 429 arrive before B wins, so the cooldown is observable.
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
        Reply::result("0x2b"),
        Reply::result("0x2c").delayed(Duration::from_secs(1)),
    ]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    for expected in ["0x2a", "0x2b"] {
        assert_eq!(
            rpc.request("eth_getBalance", balance_params())
                .await
                .unwrap(),
            json!(expected)
        );
    }
    assert_eq!(primary.requests().len(), 2);
    let requests = backup.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["method"], "eth_chainId");
    for request in &requests[1..] {
        assert_eq!(request["method"], "eth_getBalance");
        assert_eq!(request["params"], balance_params());
    }
    // Simulate expiry without sleeping for two minutes.
    rpc.read_cooldowns
        .lock()
        .unwrap()
        .insert(primary.url.clone(), Instant::now() - Duration::from_secs(1));
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0xaa")
    );
    assert_eq!(primary.requests().len(), 3);
    assert_eq!(backup.requests().len(), 4);
}

#[tokio::test]
async fn missing_or_invalid_retry_after_retains_default_cooldown_and_retries() {
    for value in [None, Some("PRIVATE_INVALID_VALUE")] {
        let primary = Server::new(vec![
            Reply::result("0x1"),
            Reply::rate_limit(value),
            Reply::rate_limit(value),
            Reply::rate_limit(value),
        ]);
        let rpc = primary.rpc();
        let error = rpc
            .request("eth_getBalance", balance_params())
            .await
            .unwrap_err();
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("PRIVATE_INVALID_VALUE")
        );
        assert_eq!(error.details.as_deref().unwrap()["http_status"], 429);
        let error = rpc
            .request("eth_getBalance", balance_params())
            .await
            .unwrap_err();
        assert_eq!(error.code, "RPC_RATE_LIMITED");
        let remaining = error.details.as_deref().unwrap()["retry_after_ms"]
            .as_u64()
            .unwrap();
        assert!((29_000..=30_000).contains(&remaining));
        assert_eq!(primary.requests().len(), 4);
    }
}

#[tokio::test]
async fn json_rpc_rate_limit_uses_thirty_second_cooldown_even_with_valid_id() {
    let primary = Server::new(vec![Reply::result("0x1"), Reply::json_rpc_rate_limit()]);
    let rpc = primary.rpc();
    let error = rpc
        .request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    let details = error.details.as_deref().unwrap();
    assert_eq!(details["json_rpc_code"], -32005);
    assert!((29_000..=30_000).contains(&details["retry_after_ms"].as_u64().unwrap()));
    assert!(
        rpc.read_cooldown_remaining(&primary.url)
            .unwrap()
            .is_some_and(|remaining| remaining > Duration::from_secs(29))
    );

    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap_err()
            .code,
        "RPC_RATE_LIMITED"
    );
    assert_eq!(primary.requests().len(), 2);
}

#[tokio::test]
async fn zero_retry_after_allows_bounded_retry_without_default_cooldown() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rate_limit(Some("0")),
        Reply::result("0xaa"),
        Reply::result("0xbb"),
    ]);
    let rpc = primary.rpc();
    for expected in ["0xaa", "0xbb"] {
        assert_eq!(
            rpc.request("eth_getBalance", balance_params())
                .await
                .unwrap(),
            json!(expected)
        );
    }
    assert!(rpc.read_cooldown_remaining(&primary.url).unwrap().is_none());
    assert_eq!(primary.requests().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_rate_limits_cannot_shorten_an_existing_deadline() {
    let server = Server::new(vec![]);
    let rpc = Arc::new(server.rpc());
    let tasks: Vec<_> = [120, 5]
        .into_iter()
        .map(|seconds| {
            let rpc = rpc.clone();
            let url = server.url.clone();
            tokio::spawn(async move {
                rpc.record_read_rate_limit(&url, Duration::from_secs(seconds))
                    .unwrap();
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    let remaining = rpc.read_cooldown_remaining(&server.url).unwrap().unwrap();
    assert!(remaining > Duration::from_secs(119));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn all_providers_in_cooldown_report_earliest_retry_without_requests() {
    let primary = Server::new(vec![Reply::result("0x1"), Reply::rate_limit(Some("120"))]);
    let backup = Server::new(vec![Reply::result("0x1"), Reply::rate_limit(Some("60"))]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    rpc.request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    let error = rpc
        .request("eth_getBalance", balance_params())
        .await
        .unwrap_err();
    assert_eq!(error.code, "RPC_RATE_LIMITED");
    assert_eq!(error.details.as_deref().unwrap()["chain_id"], 1);
    let remaining = error.details.as_deref().unwrap()["retry_after_ms"]
        .as_u64()
        .unwrap();
    assert!((59_000..=60_000).contains(&remaining));
    let encoded = serde_json::to_string(&error).unwrap();
    for server in [&primary, &backup] {
        assert_eq!(server.requests().len(), 2);
        assert!(!encoded.contains(&server.url));
    }
}

#[tokio::test]
async fn retry_after_does_not_redirect_or_retry_a_broadcast() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rate_limit(Some("120")),
        Reply::rate_limit(Some("120")),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
    ]);
    let mut rpc = primary.rpc();
    rpc.urls.push(backup.url.clone());
    rpc.request("eth_getBalance", balance_params())
        .await
        .unwrap();
    let input = json!(["0xdeadbeef"]);
    let error = rpc
        .request("eth_sendRawTransaction", input.clone())
        .await
        .unwrap_err();
    assert_eq!(error.details.as_deref().unwrap()["http_status"], 429);
    assert!(
        error
            .details
            .as_deref()
            .unwrap()
            .get("retry_after_ms")
            .is_none()
    );
    let requests = primary.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2]["method"], "eth_sendRawTransaction");
    assert_eq!(requests[2]["params"], input);
    assert_eq!(backup.requests().len(), 2);
}
