use super::*;

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
