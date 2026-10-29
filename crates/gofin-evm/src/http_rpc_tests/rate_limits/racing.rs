use super::*;

#[tokio::test]
async fn bytecode_server_rejection_waits_for_validated_fallback_without_retry() {
    for (backup_reply, expected) in [
        (Reply::result("0x6000"), Ok(json!("0x6000"))),
        (Reply::result("not bytecode"), Err("RPC_DATA_INVALID")),
        (
            Reply::rpc_error("private secondary provider payload"),
            Err("RPC_REQUEST_FAILED"),
        ),
    ] {
        let release = Arc::new(AtomicBool::new(false));
        let mut backup_reply = backup_reply;
        let backup_result = backup_reply.result.clone();
        let backup_release = release.clone();
        backup_reply.on_request = Some(Box::new(move |_| {
            let until = Instant::now() + Duration::from_secs(5);
            while !backup_release.load(Ordering::SeqCst) {
                assert!(Instant::now() < until, "test must release the fallback");
                std::thread::sleep(Duration::from_millis(1));
            }
            backup_result
        }));
        let primary = Server::new(vec![
            Reply::result("0x1"),
            Reply::rpc_error("private primary provider payload"),
        ]);
        let backup = Server::new(vec![Reply::result("0x1"), backup_reply]);
        let rpc = Arc::new(
            HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1)
                .unwrap(),
        );
        let params = json!(["0x1111111111111111111111111111111111111111", "0x10"]);
        let read = tokio::spawn({
            let rpc = rpc.clone();
            let params = params.clone();
            async move { rpc.request("eth_getCode", params).await }
        });
        // Observe the primary rejection before releasing the fallback. This
        // proves the losing error cannot cancel a still-pending valid read.
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            while rpc.diagnostics()["endpoints"][0]["last_error_code"] != "RPC_REQUEST_FAILED" {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        let prematurely_finished = read.is_finished();
        release.store(true, Ordering::SeqCst);
        observed.expect("primary rejection must be observed");
        let result = read.await.unwrap();
        assert!(!prematurely_finished, "fallback must remain eligible");
        match expected {
            Ok(value) => {
                assert_eq!(result.unwrap(), value);
                assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-2");
            }
            Err(code) => {
                let error = result.unwrap_err();
                assert_eq!(error.code, code);
                assert!(!serde_json::to_string(&error).unwrap().contains("private"));
                assert_eq!(rpc.diagnostics()["last_read"]["outcome"], "error");
            }
        }
        for server in [&primary, &backup] {
            let requests = server.requests();
            assert_eq!(requests.len(), 2, "only identity and one bytecode read");
            assert_eq!(requests[1]["method"], "eth_getCode");
            assert_eq!(requests[1]["params"], params);
        }
    }
}

#[test]
fn bytecode_server_fallback_does_not_reclassify_other_rejections() {
    let generic = rpc_rejection(&json!({"code":-32000,"message":"server error"}), false);
    assert!(!generic.retryable, "no same-endpoint or write retry");
    assert!(rpc_race::can_try_another("eth_getCode", &generic));
    for method in [
        "eth_call",
        "eth_estimateGas",
        "eth_sendRawTransaction",
        "eth_getBalance",
    ] {
        assert!(!rpc_race::can_try_another(method, &generic), "{method}");
    }
    for code in [json!(-32602), json!(3), json!("-32000"), Value::Null] {
        let failure = rpc_rejection(&json!({"code":code,"message":"server error"}), false);
        assert!(!rpc_race::can_try_another("eth_getCode", &failure));
    }
}

#[tokio::test]
async fn generic_broadcast_rejection_does_not_use_read_fallback() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rpc_error("private broadcast error"),
    ]);
    let backup = Server::new(vec![]);
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap();
    let error = rpc
        .request("eth_sendRawTransaction", json!(["0x1234"]))
        .await
        .unwrap_err();
    assert_eq!(error.code, "RPC_REQUEST_FAILED");
    assert_eq!(primary.requests().len(), 2);
    assert!(backup.requests().is_empty());
}

#[tokio::test]
async fn cancelling_bytecode_fallback_cannot_continue_identity_or_retry() {
    let release = Arc::new(AtomicBool::new(false));
    let primary = Server::new(vec![Reply::result("0x1"), Reply::rpc_error("server error")]);
    let mut pending = Reply::rpc_error("server error");
    let backup_release = release.clone();
    pending.on_request = Some(Box::new(move |_| {
        let until = Instant::now() + Duration::from_secs(5);
        while !backup_release.load(Ordering::SeqCst) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        json!("0x")
    }));
    // Both slots must remain occupied until cancellation; the primary error
    // can start the third endpoint, so keep its identity read pending too.
    let third_release = release.clone();
    let third = Server::new(vec![Reply::computed(move |_| {
        let until = Instant::now() + Duration::from_secs(5);
        while !third_release.load(Ordering::SeqCst) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        json!("0x1")
    })]);
    let backup = Server::new(vec![Reply::result("0x1"), pending]);
    let rpc = Arc::new(
        HttpRpc::isolated_localhost_pool(
            vec![primary.url.clone(), backup.url.clone(), third.url.clone()],
            1,
        )
        .unwrap(),
    );
    let read = tokio::spawn({
        let rpc = rpc.clone();
        async move {
            rpc.request(
                "eth_getCode",
                json!(["0x1111111111111111111111111111111111111111", "0x10"]),
            )
            .await
        }
    });
    let pending = tokio::time::timeout(Duration::from_secs(2), async {
        while backup.requests().len() != 2 || third.requests().len() != 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    read.abort();
    let cancelled = read.await;
    release.store(true, Ordering::SeqCst);
    pending.expect("both remaining read attempts must be pending");
    assert!(cancelled.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(primary.requests().len(), 2, "no primary retry");
    assert_eq!(backup.requests().len(), 2, "no backup retry");
    assert_eq!(third.requests().len(), 1, "no code read after cancellation");
    assert_eq!(rpc.diagnostics()["last_read"]["outcome"], "cancelled");
}

#[tokio::test]
async fn concurrent_read_can_finish_after_former_rpc_and_http_deadlines() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xaa").delayed(Duration::from_secs(1)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(250)),
    ]);
    let rpc = Arc::new(
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap(),
    );
    let read = tokio::spawn({
        let rpc = rpc.clone();
        async move { rpc.request("eth_getBalance", balance_params()).await }
    });
    // Wait for both real HTTP requests before advancing only Tokio's clock.
    // The fixture's wall-clock responses remain pending, without a 31s test.
    tokio::time::timeout(Duration::from_secs(2), async {
        while primary.requests().len() < 2 || backup.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("both providers must start the same read");
    let started = tokio::time::Instant::now();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    assert!(started.elapsed() >= Duration::from_secs(31));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), read)
            .await
            .expect("the backup response should complete the read")
            .unwrap()
            .expect("a valid read must survive the former 10s and 30s deadlines"),
        json!("0x2a")
    );
    for server in [&primary, &backup] {
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            2,
            "neither provider should retry a slow read"
        );
        assert_eq!(requests[1]["params"], balance_params());
    }
    assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-2");
    assert!(rpc.diagnostics()["read_timeout_ms"].is_null());
}

#[tokio::test]
async fn broadcast_keeps_http_timeout_without_retry_or_backup() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x1234").delayed(Duration::from_secs(1)),
    ]);
    let backup = Server::new(vec![]);
    let rpc = Arc::new(
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap(),
    );
    let broadcast = tokio::spawn({
        let rpc = rpc.clone();
        async move {
            rpc.request("eth_sendRawTransaction", json!(["0xdeadbeef"]))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while primary.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the primary must receive the single broadcast");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let failure = tokio::time::timeout(Duration::from_millis(500), broadcast)
        .await
        .expect("the existing write HTTP timeout must still apply")
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.code, "RPC_UNAVAILABLE");
    let requests = primary.requests();
    assert_eq!(
        requests.len(),
        2,
        "a timed-out broadcast must not be retried"
    );
    assert_eq!(requests[1]["method"], "eth_sendRawTransaction");
    assert_eq!(requests[1]["params"], json!(["0xdeadbeef"]));
    assert!(backup.requests().is_empty());
}

#[tokio::test]
async fn primary_write_identity_timeout_prevents_any_broadcast() {
    let primary = Server::new(vec![Reply::result("0x1").delayed(Duration::from_secs(1))]);
    let backup = Server::new(vec![]);
    let rpc = Arc::new(
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap(),
    );
    let broadcast = tokio::spawn({
        let rpc = rpc.clone();
        async move {
            rpc.request("eth_sendRawTransaction", json!(["0xdeadbeef"]))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while primary.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the primary identity check must start before dispatch");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    let failure = tokio::time::timeout(Duration::from_millis(500), broadcast)
        .await
        .expect("the existing primary identity bound must still apply")
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.code, "RPC_READ_TIMEOUT");
    assert_eq!(failure.details.as_deref().unwrap()["method"], "eth_chainId");
    let requests = primary.requests();
    assert_eq!(requests.len(), 1, "identity timeout must prevent dispatch");
    assert_eq!(requests[0]["method"], "eth_chainId");
    assert!(backup.requests().is_empty());
}

#[tokio::test]
async fn concurrent_reads_start_both_providers_and_accept_faster_valid_backup() {
    // A waits two seconds; B answers in 50 ms. The old sequential selection
    // cannot finish inside the observation window, despite a healthy backup.
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xaa").delayed(Duration::from_secs(2)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
    ]);
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap();

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        rpc.request("eth_getBalance", balance_params()),
    )
    .await
    .expect("B should answer while A's read is still running")
    .unwrap();
    assert_eq!(result, json!("0x2a"));
    for server in [&primary, &backup] {
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "both providers must receive the read");
        assert_eq!(requests[1]["params"], balance_params());
    }
    assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-2");
}

#[tokio::test]
async fn faster_malformed_quantity_cannot_win_the_read_race() {
    let primary = Server::new(vec![Reply::result("0x1"), Reply::result("not-a-quantity")]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
    ]);
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap();
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0x2a")
    );
    let health = rpc.diagnostics();
    assert_eq!(health["last_read"]["attempts"][0]["reason"], "incompatible");
    assert_eq!(health["last_read"]["selected_endpoint"], "rpc-2");
}

#[tokio::test]
async fn faster_malformed_receipt_cannot_win_over_original_hash_evidence() {
    let original = format!("0x{}", "33".repeat(32));
    for malformed in [
        json!("0x1"),
        json!({"transactionHash":format!("0x{}", "44".repeat(32))}),
    ] {
        let primary = Server::new(vec![
            Reply::result("0x1"),
            Reply::computed(move |_| malformed),
        ]);
        let receipt = json!({"transactionHash":original});
        let backup = Server::new(vec![
            Reply::result("0x1"),
            Reply::computed({
                let receipt = receipt.clone();
                move |_| receipt
            })
            .delayed(Duration::from_millis(50)),
        ]);
        let rpc =
            HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1)
                .unwrap();
        assert_eq!(
            rpc.request("eth_getTransactionReceipt", json!([original]))
                .await
                .unwrap(),
            receipt
        );
        assert_eq!(
            rpc.diagnostics()["last_read"]["attempts"][0]["reason"],
            "incompatible"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn already_observed_semantic_revert_is_not_hidden_by_same_batch_success() {
    for primary_reverts in [false, true] {
        let release = Arc::new(AtomicBool::new(false));
        let replies = |reverts| {
            let mut reply = if reverts {
                Reply::rpc_error("execution reverted")
            } else {
                Reply::result("0x2a")
            };
            let release = release.clone();
            reply.on_request = Some(Box::new(move |_| {
                let until = Instant::now() + Duration::from_secs(2);
                while !release.load(Ordering::SeqCst) {
                    assert!(Instant::now() < until);
                    std::thread::sleep(Duration::from_millis(1));
                }
                json!("0x2a")
            }));
            vec![reply]
        };
        let primary = Server::new(replies(primary_reverts));
        let backup = Server::new(replies(!primary_reverts));
        let rpc =
            HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1)
                .unwrap();
        // Identity was verified by prior reads. Pause this read after dispatch
        // so both network responses are ready when the parent next polls it.
        rpc.verified
            .lock()
            .unwrap()
            .extend([primary.url.clone(), backup.url.clone()]);
        let mut read = Box::pin(rpc.request("eth_call", json!([{}, "0x10"])));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                assert!(
                    std::future::poll_fn(|context| std::task::Poll::Ready(
                        read.as_mut().poll(context)
                    ))
                    .await
                    .is_pending()
                );
                if primary.requests().len() == 1 && backup.requests().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        release.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(primary.requests().len(), 1);
        assert_eq!(backup.requests().len(), 1);
        assert_eq!(read.await.unwrap_err().code, "RPC_REQUEST_FAILED");
        assert_eq!(rpc.diagnostics()["last_read"]["outcome"], "error");
    }
}

#[tokio::test]
async fn faster_wrong_chain_cannot_win_over_a_slower_valid_primary() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
    ]);
    let backup = Server::new(vec![Reply::result("0x2")]);
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap();
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0x2a")
    );
    assert_eq!(
        backup.requests().len(),
        1,
        "wrong-chain endpoint receives no balance request"
    );
    assert_eq!(rpc.diagnostics()["last_read"]["selected_endpoint"], "rpc-1");
}

#[tokio::test]
async fn third_provider_replaces_failed_slot_without_waiting_for_slow_peer() {
    let primary = Server::new(vec![Reply::result("0x1"), Reply::rate_limit(Some("120"))]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xaa").delayed(Duration::from_secs(2)),
    ]);
    let third = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(50)),
    ]);
    let rpc = HttpRpc::isolated_localhost_pool(
        vec![primary.url.clone(), backup.url.clone(), third.url.clone()],
        1,
    )
    .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(500),
            rpc.request("eth_getBalance", balance_params())
        )
        .await
        .unwrap()
        .unwrap(),
        json!("0x2a")
    );
    let health = rpc.diagnostics();
    assert_eq!(health["last_read"]["selected_endpoint"], "rpc-3");
    assert_eq!(
        health["last_read"]["attempts"],
        json!([
            {"endpoint_id":"rpc-1","reason":"rate_limited"},
            {"endpoint_id":"rpc-2","reason":"cancelled"},
            {"endpoint_id":"rpc-3","reason":"ok"}
        ])
    );
}

#[tokio::test]
async fn winner_cancels_losing_retry_sleep_without_sending_another_request() {
    let mut unavailable = Reply::result("0x0");
    unavailable.status = 503;
    let primary = Server::new(vec![Reply::result("0x1"), unavailable]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_millis(30)),
    ]);
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap();
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0x2a")
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        primary.requests().len(),
        2,
        "cancelled retry must never resume"
    );
    assert_eq!(
        rpc.diagnostics()["last_read"]["attempts"][0]["reason"],
        "cancelled"
    );
}

#[tokio::test]
async fn concurrent_pin_is_immutable_even_when_global_cache_is_evicted() {
    let state_read_started = Arc::new(AtomicBool::new(false));
    let published = Arc::new(Mutex::new(None::<Arc<HttpRpc>>));
    let first_hash = format!("0x{}", "11".repeat(32));
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::computed({
            let first_hash = first_hash.clone();
            move |_| json!({"number":"0x10","hash":first_hash})
        }),
        Reply::computed({
            let started = state_read_started.clone();
            let first_hash = first_hash.clone();
            move |request| {
                assert_eq!(
                    request["params"][1],
                    json!({"blockHash":first_hash,"requireCanonical":true})
                );
                started.store(true, Ordering::SeqCst);
                json!("0x2a")
            }
        })
        .delayed(Duration::from_millis(100)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::computed({
            let published = published.clone();
            let started = state_read_started.clone();
            move |_| {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !started.load(Ordering::SeqCst) {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                published
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .selection
                    .lock()
                    .unwrap()
                    .evict_anchors();
                json!({"number":"0x10","hash":format!("0x{}", "22".repeat(32))})
            }
        }),
    ]);
    let rpc = Arc::new(
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap(),
    );
    *published.lock().unwrap() = Some(rpc.clone());
    assert_eq!(
        rpc.request_checked(
            "eth_getBalance",
            balance_params(),
            RpcReadRequirements {
                pin_block: Some(16),
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        json!("0x2a")
    );
    assert_eq!(
        backup.requests().len(),
        2,
        "different-fork backup must never read state"
    );
    assert_eq!(
        rpc.diagnostics()["last_read"]["attempts"][1]["reason"],
        "incompatible"
    );
    assert!(
        rpc.cached_anchor("eth_getBalance", &balance_params())
            .is_none(),
        "rejected racing fork must not poison the cache"
    );
}
