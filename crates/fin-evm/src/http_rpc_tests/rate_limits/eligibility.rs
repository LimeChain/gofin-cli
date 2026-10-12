use super::*;

fn header(number: u64, hash_byte: &str, age_seconds: i64) -> Value {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let timestamp = if age_seconds >= 0 {
        now.saturating_sub(age_seconds as u64)
    } else {
        now + age_seconds.unsigned_abs()
    };
    json!({
        "number":format!("0x{number:x}"),
        "hash":format!("0x{}", hash_byte.repeat(32)),
        "parentHash":format!("0x{}", "00".repeat(32)),
        "timestamp":format!("0x{timestamp:x}")
    })
}

fn pair(primary: &Server, backup: &Server) -> HttpRpc {
    HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1).unwrap()
}

fn constant_reply(value: Value) -> Reply {
    Reply::computed(move |_| value)
}

fn unsupported(message: &str) -> Reply {
    let mut reply = Reply::rpc_error(message);
    reply.error = Some(json!({"code":-32601,"message":message}));
    reply
}

#[tokio::test]
async fn stale_successful_latest_header_uses_fresh_backup() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(header(100, "11", 120)),
    ]);
    let fresh = header(101, "22", 0);
    let backup = Server::new(vec![Reply::result("0x1"), constant_reply(fresh.clone())]);
    let result = pair(&primary, &backup)
        .request("eth_getBlockByNumber", json!(["latest", false]))
        .await
        .unwrap();
    assert_eq!(result, fresh);
    for server in [&primary, &backup] {
        assert_eq!(server.requests()[1]["params"], json!(["latest", false]));
    }
}

#[tokio::test]
async fn implausibly_future_latest_header_cannot_supply_freshness() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(header(100, "11", -120)),
    ]);
    let fresh = header(101, "22", 0);
    let backup = Server::new(vec![Reply::result("0x1"), constant_reply(fresh.clone())]);
    assert_eq!(
        pair(&primary, &backup)
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await
            .unwrap(),
        fresh
    );
}

#[tokio::test]
async fn stale_successful_height_is_verified_on_its_own_endpoint() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x64"),
        constant_reply(header(100, "11", 120)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x65"),
        constant_reply(header(101, "22", 0)),
    ]);
    assert_eq!(
        pair(&primary, &backup)
            .request("eth_blockNumber", json!([]))
            .await
            .unwrap(),
        json!("0x65")
    );
    for server in [&primary, &backup] {
        let requests = server.requests();
        assert_eq!(requests[1]["method"], "eth_blockNumber");
        assert_eq!(requests[2]["method"], "eth_getBlockByNumber");
        assert_eq!(requests[2]["params"], json!(["latest", false]));
    }
}

#[tokio::test]
async fn latest_state_from_stale_primary_is_discarded() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0xdead"),
        constant_reply(header(100, "11", 120)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a"),
        constant_reply(header(101, "22", 0)),
    ]);
    let params = json!(["0x0000000000000000000000000000000000000002", "latest"]);
    assert_eq!(
        pair(&primary, &backup)
            .request("eth_getBalance", params.clone())
            .await
            .unwrap(),
        json!("0x2a")
    );
    assert_eq!(primary.requests()[1]["params"], params);
    assert_eq!(backup.requests()[1]["params"], params);
}

#[tokio::test]
async fn recent_head_is_reused_for_bounded_same_endpoint_freshness_checks() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a"),
        constant_reply(header(101, "22", 0)),
        Reply::result("0x2b"),
    ]);
    let rpc = primary.rpc();
    let params = json!(["0x0000000000000000000000000000000000000002", "latest"]);
    for expected in ["0x2a", "0x2b"] {
        assert_eq!(
            rpc.request("eth_getBalance", params.clone()).await.unwrap(),
            json!(expected)
        );
    }
    assert_eq!(
        primary
            .requests()
            .iter()
            .filter(|r| r["method"] == "eth_getBlockByNumber")
            .count(),
        1
    );
}

#[tokio::test]
async fn absent_receipt_on_primary_searches_original_hash_on_backup() {
    let transaction_hash = format!("0x{}", "ab".repeat(32));
    let primary = Server::new(vec![Reply::result("0x1"), constant_reply(Value::Null)]);
    let mined = json!({"transactionHash":transaction_hash,"blockNumber":"0x64",
        "blockHash":format!("0x{}", "11".repeat(32)),"status":"0x1"});
    let backup = Server::new(vec![Reply::result("0x1"), constant_reply(mined.clone())]);
    let params = json!([transaction_hash]);
    assert_eq!(
        pair(&primary, &backup)
            .request("eth_getTransactionReceipt", params.clone())
            .await
            .unwrap(),
        mined
    );
    for server in [&primary, &backup] {
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["method"], "eth_getTransactionReceipt");
        assert_eq!(requests[1]["params"], params);
        assert!(
            !requests
                .iter()
                .any(|r| r["method"] == "eth_sendRawTransaction")
        );
    }
}

#[tokio::test]
async fn all_providers_missing_the_original_receipt_leave_it_unknown() {
    let primary = Server::new(vec![Reply::result("0x1"), constant_reply(Value::Null)]);
    let backup = Server::new(vec![Reply::result("0x1"), constant_reply(Value::Null)]);
    let result = pair(&primary, &backup)
        .request(
            "eth_getTransactionReceipt",
            json!([format!("0x{}", "ab".repeat(32))]),
        )
        .await
        .unwrap();
    assert!(
        result.is_null(),
        "missing observations are not evidence of failure or a reason to resubmit"
    );
    assert_eq!(primary.requests().len(), 2);
    assert_eq!(backup.requests().len(), 2);
}

#[tokio::test]
async fn missing_transaction_or_block_searches_another_provider() {
    for (method, params, found) in [
        (
            "eth_getTransactionByHash",
            json!([format!("0x{}", "ab".repeat(32))]),
            json!({"hash":format!("0x{}", "ab".repeat(32))}),
        ),
        (
            "eth_getBlockByNumber",
            json!(["0x10", false]),
            header(16, "11", 86_400),
        ),
    ] {
        let primary = Server::new(vec![Reply::result("0x1"), constant_reply(Value::Null)]);
        let backup = Server::new(vec![Reply::result("0x1"), constant_reply(found.clone())]);
        assert_eq!(
            pair(&primary, &backup)
                .request(method, params.clone())
                .await
                .unwrap(),
            found
        );
        assert_eq!(backup.requests()[1]["params"], params);
    }
}

#[tokio::test]
async fn unsupported_finality_is_cached_per_method_without_disabling_other_reads() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        unsupported("PRIVATE_RPC_ERROR finalized method unavailable"),
        constant_reply(header(101, "22", 0)),
    ]);
    let finalized = header(100, "11", 1_800);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(finalized.clone()),
        constant_reply(finalized.clone()),
    ]);
    let rpc = pair(&primary, &backup);
    for _ in 0..2 {
        assert_eq!(
            rpc.request_checked(
                "eth_getBlockByNumber",
                json!(["finalized", false]),
                RpcReadRequirements {
                    minimum_block: Some(100),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            finalized
        );
    }
    assert_eq!(
        primary.requests().len(),
        2,
        "unsupported finalized capability should not be retried immediately"
    );
    assert_eq!(
        rpc.request("eth_getBlockByNumber", json!(["latest", false]))
            .await
            .unwrap()["number"],
        json!("0x65")
    );
    assert_eq!(
        primary.requests().len(),
        3,
        "finalized unavailability must not disable latest on the same endpoint"
    );
}

#[tokio::test]
async fn finalized_header_below_original_receipt_uses_backup() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(header(99, "33", 1_800)),
    ]);
    let finalized = header(100, "11", 1_800);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(finalized.clone()),
    ]);
    assert_eq!(
        pair(&primary, &backup)
            .request_checked(
                "eth_getBlockByNumber",
                json!(["finalized", false]),
                RpcReadRequirements {
                    minimum_block: Some(100),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        finalized
    );
    assert_eq!(backup.requests()[1]["params"], json!(["finalized", false]));
}

#[tokio::test]
async fn minimum_confirmation_height_cannot_be_satisfied_by_older_primary() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x63"),
        constant_reply(header(99, "33", 0)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x65"),
        constant_reply(header(101, "22", 0)),
    ]);
    assert_eq!(
        pair(&primary, &backup)
            .request_checked(
                "eth_blockNumber",
                json!([]),
                RpcReadRequirements {
                    minimum_block: Some(101),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        json!("0x65")
    );
}

#[tokio::test]
async fn observed_historical_header_pins_state_by_canonical_hash_on_fallback() {
    let anchor = header(16, "11", 86_400);
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(anchor.clone()),
        Reply::rate_limit(Some("120")),
    ]);
    let backup = Server::new(vec![Reply::result("0x1"), Reply::result("0x2a")]);
    let rpc = pair(&primary, &backup);
    assert_eq!(
        rpc.request("eth_getBlockByNumber", json!(["0x10", false]))
            .await
            .unwrap(),
        anchor
    );
    assert_eq!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .unwrap(),
        json!("0x2a")
    );
    for server in [&primary, &backup] {
        let state = server
            .requests()
            .into_iter()
            .find(|r| r["method"] == "eth_getBalance")
            .unwrap();
        assert_eq!(state["params"][0], balance_params()[0]);
        assert_eq!(
            state["params"][1],
            json!({"blockHash":anchor["hash"],"requireCanonical":true})
        );
    }
}

#[tokio::test]
async fn numeric_fallback_cannot_use_same_height_on_another_fork() {
    let anchor = header(16, "11", 86_400);
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(anchor.clone()),
        Reply::rate_limit(Some("120")),
    ]);
    let mut hash_unsupported = Reply::rpc_error("invalid argument: blockHash is not supported");
    hash_unsupported.error =
        Some(json!({"code":-32602,"message":"invalid argument: blockHash is not supported"}));
    let backup = Server::new(vec![
        Reply::result("0x1"),
        hash_unsupported,
        constant_reply(header(16, "22", 86_400)),
    ]);
    let rpc = pair(&primary, &backup);
    rpc.request("eth_getBlockByNumber", json!(["0x10", false]))
        .await
        .unwrap();
    assert!(
        rpc.request("eth_getBalance", balance_params())
            .await
            .is_err()
    );
    assert!(
        !backup
            .requests()
            .iter()
            .any(|r| r["method"] == "eth_getBalance" && r["params"][1] == "0x10"),
        "do not even request numeric state after the endpoint reports another block hash"
    );
}

#[tokio::test]
async fn numeric_fallback_requires_matching_block_hash_before_and_after_state() {
    for changes_after_state in [false, true] {
        let anchor = header(16, "11", 86_400);
        let primary = Server::new(vec![
            Reply::result("0x1"),
            constant_reply(anchor.clone()),
            Reply::rate_limit(Some("120")),
        ]);
        let mut hash_unsupported = Reply::rpc_error("invalid argument: blockHash is not supported");
        hash_unsupported.error =
            Some(json!({"code":-32602,"message":"invalid argument: blockHash is not supported"}));
        let backup = Server::new(vec![
            Reply::result("0x1"),
            hash_unsupported,
            constant_reply(anchor.clone()),
            Reply::result("0x2a"),
            constant_reply(header(
                16,
                if changes_after_state { "22" } else { "11" },
                86_400,
            )),
        ]);
        let rpc = pair(&primary, &backup);
        rpc.request("eth_getBlockByNumber", json!(["0x10", false]))
            .await
            .unwrap();
        let result = rpc.request("eth_getBalance", balance_params()).await;
        if changes_after_state {
            assert_eq!(result.unwrap_err().code, "RPC_INCOMPATIBLE_BLOCK");
        } else {
            assert_eq!(result.unwrap(), json!("0x2a"));
        }
        let requests = backup.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[2]["method"], "eth_getBlockByNumber");
        assert_eq!(requests[2]["params"], json!(["0x10", false]));
        assert_eq!(requests[3]["method"], "eth_getBalance");
        assert_eq!(requests[3]["params"], balance_params());
        assert_eq!(requests[4]["method"], "eth_getBlockByNumber");
        assert_eq!(requests[4]["params"], json!(["0x10", false]));
    }
}

#[tokio::test]
async fn semantic_revert_does_not_search_for_a_more_favorable_answer() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::rpc_error("execution reverted: PRIVATE_UPSTREAM_TEXT"),
    ]);
    let backup = Server::new(vec![]);
    assert!(pair(&primary, &backup).request("eth_call", json!([{"to":"0x0000000000000000000000000000000000000002","data":"0x12345678"}, "0x10"])).await.is_err());
    assert!(backup.requests().is_empty());
}

#[tokio::test]
async fn cancelling_an_inflight_read_does_not_start_backup_requests() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2a").delayed(Duration::from_secs(2)),
    ]);
    let backup = Server::new(vec![]);
    let rpc = Arc::new(pair(&primary, &backup));
    let running = rpc.clone();
    let task =
        tokio::spawn(async move { running.request("eth_getBalance", balance_params()).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while primary.requests().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(backup.requests().is_empty());
    assert_eq!(rpc.diagnostics()["last_read"]["outcome"], "cancelled");
}

#[tokio::test]
async fn eligibility_failure_diagnostics_do_not_echo_provider_errors_or_requests() {
    let secret = "PRIVATE_QUERY_AND_UPSTREAM_MARKER";
    let primary = Server::new(vec![Reply::result("0x1"), unsupported(secret)]);
    let backup = Server::new(vec![Reply::result("0x1"), unsupported(secret)]);
    let error = pair(&primary, &backup)
        .request(
            "eth_call",
            json!([{"to":"0x0000000000000000000000000000000000000002","data":secret}, "0x10"]),
        )
        .await
        .unwrap_err();
    let encoded = serde_json::to_string(&error).unwrap();
    assert!(!encoded.contains(secret));
    assert!(!encoded.contains(&primary.url));
    assert!(!encoded.contains(&backup.url));
}

#[tokio::test]
async fn advancing_head_between_number_and_header_reads_is_not_staleness() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x64"),
        constant_reply(header(101, "22", 0)),
        constant_reply(header(100, "11", 1)),
    ]);
    assert_eq!(
        primary
            .rpc()
            .request("eth_blockNumber", json!([]))
            .await
            .unwrap(),
        json!("0x64")
    );
    assert_eq!(primary.requests()[3]["params"], json!(["0x64", false]));
}

#[tokio::test]
async fn explicit_typed_state_pin_resolves_hash_before_first_state_read() {
    let anchor = header(16, "11", 86_400);
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(anchor.clone()),
        Reply::rate_limit(Some("120")),
    ]);
    let backup = Server::new(vec![Reply::result("0x1"), Reply::result("0x2a")]);
    let rpc = pair(&primary, &backup);
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
    assert_eq!(primary.requests()[1]["method"], "eth_getBlockByNumber");
    for server in [&primary, &backup] {
        let reads = server.requests();
        let state = reads
            .iter()
            .find(|r| r["method"] == "eth_getBalance")
            .unwrap();
        assert_eq!(
            state["params"][1],
            json!({"blockHash":anchor["hash"],"requireCanonical":true})
        );
    }
}

#[tokio::test]
async fn cache_eviction_during_failover_cannot_replace_request_block_identity() {
    let anchor = header(16, "11", 86_400);
    let holder: Arc<Mutex<Option<std::sync::Weak<HttpRpc>>>> = Arc::new(Mutex::new(None));
    let callback = holder.clone();
    let mut limited = Reply::rate_limit(Some("120"));
    limited.on_request = Some(Box::new(move |_| {
        let rpc = callback
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        // Model eviction by concurrent unrelated block observations while A
        // handles this request. The per-request anchor must survive it.
        let mut state = rpc.selection.lock().unwrap();
        state.evict_anchors();
        Value::Null
    }));
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(anchor.clone()),
        limited,
    ]);
    let backup = Server::new(vec![Reply::result("0x1"), Reply::result("0x2a")]);
    let rpc = Arc::new(pair(&primary, &backup));
    *holder.lock().unwrap() = Some(Arc::downgrade(&rpc));
    rpc.request("eth_getBlockByNumber", json!(["0x10", false]))
        .await
        .unwrap();
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
        "B must not resolve another hash at this height"
    );
    assert_eq!(
        backup.requests()[1]["params"][1],
        json!({"blockHash":anchor["hash"],"requireCanonical":true})
    );
}

#[tokio::test]
async fn pending_nonce_from_stale_provider_cannot_reach_execution_checks() {
    let primary = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x2"),
        constant_reply(header(100, "11", 120)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        Reply::result("0x3"),
        constant_reply(header(101, "22", 0)),
    ]);
    assert_eq!(
        pair(&primary, &backup)
            .request(
                "eth_getTransactionCount",
                json!(["0x0000000000000000000000000000000000000002", "pending"])
            )
            .await
            .unwrap(),
        json!("0x3")
    );
}

#[tokio::test]
async fn healthy_primary_read_does_not_erase_last_successful_failover() {
    let hash = format!("0x{}", "ab".repeat(32));
    let primary = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(Value::Null),
        constant_reply(header(100, "11", 0)),
    ]);
    let backup = Server::new(vec![
        Reply::result("0x1"),
        constant_reply(json!({"transactionHash":hash})),
    ]);
    let rpc = pair(&primary, &backup);
    rpc.request("eth_getTransactionReceipt", json!([hash]))
        .await
        .unwrap();
    rpc.request("eth_getBlockByNumber", json!(["latest", false]))
        .await
        .unwrap();
    let health = rpc.diagnostics();
    assert_eq!(health["last_read"]["selected_endpoint"], "rpc-1");
    assert_eq!(health["last_failover"]["selected_endpoint"], "rpc-2");
    assert_eq!(
        health["last_failover"]["method"],
        "eth_getTransactionReceipt"
    );
}
