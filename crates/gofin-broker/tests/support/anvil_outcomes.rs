// Generic outcome cases against an owned chain, with faults only at RPC boundaries.
struct OutcomeClock(std::sync::atomic::AtomicI64);
impl clock::Clock for OutcomeClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(self.0.load(Ordering::SeqCst))
    }
}
#[derive(Default)]
struct OutcomeFaults {
    hide_receipt: AtomicBool,
    hide_transaction: AtomicBool,
    lose_response: AtomicBool,
    missing_fee: AtomicBool,
    fail_preparation: AtomicBool,
    pause_preparation: AtomicBool,
    preparation_entered: AtomicBool,
    release_preparation: AtomicBool,
    pause_dispatch: AtomicBool,
    dispatch_entered: AtomicBool,
    release_dispatch: AtomicBool,
}
struct OutcomeRpc {
    inner: Arc<Rpc>,
    faults: Arc<OutcomeFaults>,
}
#[async_trait]
impl EvmRpc for OutcomeRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_getTransactionByHash"
            && self.faults.hide_transaction.load(Ordering::SeqCst)
        {
            return Ok(Value::Null);
        }
        if method == "eth_getBalance" && self.faults.pause_preparation.load(Ordering::SeqCst) {
            self.faults
                .preparation_entered
                .store(true, Ordering::SeqCst);
            while !self.faults.release_preparation.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        if method == "eth_getBalance" && self.faults.fail_preparation.load(Ordering::SeqCst) {
            return Err(fail("Required balance read unavailable"));
        }
        if method == "eth_getTransactionReceipt" && self.faults.hide_receipt.load(Ordering::SeqCst)
        {
            return Ok(Value::Null);
        }
        let mut result = self.inner.request(method, params).await?;
        if method == "eth_sendRawTransaction" {
            self.faults.dispatch_entered.store(true, Ordering::SeqCst);
            if self.faults.pause_dispatch.load(Ordering::SeqCst) {
                while !self.faults.release_dispatch.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            if self.faults.lose_response.swap(false, Ordering::SeqCst) {
                return Err(GoFinError::new(
                    "RPC_TIMEOUT",
                    "submission",
                    "Accepted response lost at fixture transport.",
                    "Observe the original identity.",
                ));
            }
        }
        if method == "eth_getTransactionReceipt"
            && result.is_object()
            && self.faults.missing_fee.load(Ordering::SeqCst)
        {
            result["effectiveGasPrice"] = Value::Null;
        }
        Ok(result)
    }
}
struct OutcomePrices(bool);
#[async_trait]
impl UsdPriceSource for OutcomePrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        if self.0 {
            Err(fail("Optional price source unavailable"))
        } else {
            Prices.quote(asset).await
        }
    }
}
#[async_trait]
impl WalletValuation for OutcomePrices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        if self.0 {
            Err(fail("Optional price source unavailable"))
        } else {
            Prices.quote_usd(asset, units).await
        }
    }
}
#[derive(Default)]
struct OutcomeApproval {
    count: AtomicUsize,
    reviews: Mutex<Vec<Value>>,
    pause: AtomicBool,
    entered: AtomicBool,
    release: AtomicBool,
}
#[async_trait]
impl Authorizer for OutcomeApproval {
    fn requires_financial_review(&self) -> bool {
        true
    }
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.reviews
            .lock()
            .unwrap()
            .push(serde_json::to_value(&c.review).unwrap());
        self.entered.store(true, Ordering::SeqCst);
        if self.pause.load(Ordering::SeqCst) {
            while !self.release.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Ok(ApprovalAudit {
            mechanism: "owned-Anvil-fixture".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn outcome_runtime(
    path: &Path,
    rpc: Arc<OutcomeRpc>,
    approval: Arc<OutcomeApproval>,
    clock: Arc<OutcomeClock>,
    unpriced: bool,
) -> Arc<gofin_broker::BrokerRuntime> {
    let signer = Arc::new(
        gofin_broker::MappedEnvelopeSigner::new(BTreeMap::from([
            (
                owner(),
                Arc::new(LocalEnvelopeSigner::from_private_key(KEY).unwrap())
                    as Arc<dyn gofin_evm::EnvelopeSigner>,
            ),
            (
                other_owner(),
                Arc::new(LocalEnvelopeSigner::from_private_key(KEY2).unwrap())
                    as Arc<dyn gofin_evm::EnvelopeSigner>,
            ),
        ]))
        .unwrap(),
    );
    let adapter = EthereumAdapter::new(
        EthereumConfig::wallet_only(1, 1, vec![]),
        vec![
            WalletConnection {
                name: "synthetic".into(),
                address: owner(),
                signing_enabled: true,
            },
            WalletConnection {
                name: "synthetic-other".into(),
                address: other_owner(),
                signing_enabled: true,
            },
        ],
        Arc::new(EvmRuntime::new(rpc, 1, Some(signer))),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(OutcomePrices(unpriced)));
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(adapter)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(OutcomePrices(
                unpriced,
            )))),
            bridge: None,
        },
        clock,
    )
    .unwrap()
}
fn outcome_request(id: &str, wallet: &str) -> (Vec<String>, BrokerRequest) {
    let input = json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":"0x2222222222222222222222222222222222222222","max_fee_eth":"0.001"});
    (
        args(&[
            "execute",
            "wallet.transfer",
            "--id",
            id,
            "--input",
            &input.to_string(),
            "--json",
        ]),
        BrokerRequest::Execute {
            request: OperationRequest {
                request_id: id.into(),
                operation: "wallet.transfer".into(),
                input,
                run_id: None,
            },
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public CLI and owned Anvil"]
async fn generic_outcomes_on_owned_anvil() {
    assert!(std::env::var_os("GOFIN_UNISWAP_CLI").is_some());
    let scenario = std::env::var("GOFIN_OUTCOME_SCENARIO").expect("explicit scenario");
    assert!(
        ["a1", "a2", "a3", "a4", "a5", "a7", "a22", "a23"]
            .iter()
            .any(|case| scenario == format!("outcome.{case}.anvil"))
    );
    let (_node, rpc, _) = node(1).await;
    rpc.request(
        "anvil_setBalance",
        json!([other_owner(), "0x8ac7230489e80000"]),
    )
    .await
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("outcome.sqlite");
    let faults = Arc::new(OutcomeFaults::default());
    faults.hide_receipt.store(true, Ordering::SeqCst);
    faults.lose_response.store(
        matches!(scenario.as_str(), "outcome.a2.anvil" | "outcome.a22.anvil"),
        Ordering::SeqCst,
    );
    faults.hide_transaction.store(
        matches!(scenario.as_str(), "outcome.a2.anvil" | "outcome.a22.anvil"),
        Ordering::SeqCst,
    );
    faults
        .missing_fee
        .store(scenario == "outcome.a7.anvil", Ordering::SeqCst);
    let wrapped = Arc::new(OutcomeRpc {
        inner: rpc.clone(),
        faults: faults.clone(),
    });
    let approval = Arc::new(OutcomeApproval::default());
    let clock = Arc::new(OutcomeClock(std::sync::atomic::AtomicI64::new(0)));
    let unpriced = scenario == "outcome.a1.anvil";
    let client = Client::new(
        outcome_runtime(
            &path,
            wrapped.clone(),
            approval.clone(),
            clock.clone(),
            unpriced,
        ),
        temp.path(),
    )
    .await;
    if scenario == "outcome.a5.anvil" {
        let mut store = Store::open(&path).unwrap();
        store
            .create_request(
                "corrupt",
                "corrupt",
                "corrupt",
                "wallet.transfer",
                None,
                "unknown",
                &json!({"id":"corrupt"}),
                &now(),
            )
            .unwrap();
        drop(store);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE operations SET record_json='{' WHERE id='corrupt'",
                [],
            )
            .unwrap();
    }
    let (argv, request) = outcome_request("original", "synthetic");
    let original = client.call(argv.clone(), request.clone(), true).await;
    let id = original["id"].as_str().unwrap().to_owned();
    assert_eq!(original["outcome"]["phase"], "in_progress", "{original}");
    assert_eq!(original["outcome"]["replay_safe"], false);
    if matches!(scenario.as_str(), "outcome.a2.anvil" | "outcome.a22.anvil") {
        assert_eq!(original["outcome"]["dispatch"], "uncertain", "{original}");
    } else {
        assert_eq!(original["outcome"]["dispatch"], "dispatched", "{original}");
    }
    assert!(original["outcome"]["next_check_after"].is_string());
    assert_eq!(rpc.broadcasts.load(Ordering::SeqCst), 1);
    if unpriced {
        assert!(original["preview"]["valuation"].is_null());
        assert!(approval.reviews.lock().unwrap()[0]["terms"].as_array().unwrap().iter().any(|term|term["label"]=="Expected USD value"&&term["value"]=="Unavailable"));
    }
    if scenario == "outcome.a4.anvil" || scenario == "outcome.a23.anvil" {
        clock.0.store(7 * 24 * 60 * 60, Ordering::SeqCst);
        let open = client
            .call(
                args(&["operations", "--open", "--json"]),
                BrokerRequest::OpenOperations,
                true,
            )
            .await;
        assert!(
            open.as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == id && r["outcome"]["phase"] == "unconfirmed")
        );
        if scenario == "outcome.a23.anvil" {
            let dismissed = client.runtime.engine.dismiss_operation(&id).unwrap();
            assert_eq!(dismissed["outcome"]["replay_safe"], false);
            let open = client
                .call(
                    args(&["operations", "--open", "--json"]),
                    BrokerRequest::OpenOperations,
                    true,
                )
                .await;
            assert!(!open.as_array().unwrap().iter().any(|r| r["id"] == id));
        }
        clock.0.store(0, Ordering::SeqCst);
    }
    if scenario == "outcome.a5.anvil" {
        let bad = client
            .call(
                args(&["status", "corrupt", "--json"]),
                BrokerRequest::Status {
                    id: "corrupt".into(),
                },
                true,
            )
            .await;
        assert_eq!(bad["outcome"]["replay_safe"], false);
        assert_eq!(bad["outcome"]["reporting"], "unavailable");
    }
    // Destroy the old broker handle; rebuild against the same durable journal.
    drop(client);
    let client = Client::new(
        outcome_runtime(&path, wrapped, approval.clone(), clock, unpriced),
        temp.path(),
    )
    .await;
    let repeated = client.call(argv, request, true).await;
    assert_eq!(repeated["id"], id);
    assert_eq!(repeated["submission"], original["submission"]);
    assert_eq!(approval.count.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.broadcasts.load(Ordering::SeqCst), 1);
    let snapshot = || {
        (
            approval.count.load(Ordering::SeqCst),
            rpc.broadcasts.load(Ordering::SeqCst),
        )
    };
    let main_counts = snapshot();
    let mut probes = Vec::new();
    for (probe_id, wallet, scope) in [
        ("probe-same", "synthetic", "same_account"),
        ("probe-other", "synthetic-other", "different_account"),
    ] {
        let before = snapshot();
        let (argv, req) = outcome_request(probe_id, wallet);
        let result = client.call(argv, req, true).await;
        probes.push(outcome_evidence::admission_probe(
            scope,
            before,
            snapshot(),
            &result,
        ));
    }
    if scenario == "outcome.a22.anvil" {
        assert!(
            approval.reviews.lock().unwrap()[1]["advisories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["operation_id"] == id)
        );
    }
    faults.hide_transaction.store(false, Ordering::SeqCst);
    faults.hide_receipt.store(false, Ordering::SeqCst);
    rpc.request("anvil_mine", json!(["0x40"])).await.unwrap();
    let observed = client
        .call(
            args(&["status", &id, "--json"]),
            BrokerRequest::Status { id: id.clone() },
            true,
        )
        .await;
    assert_eq!(observed["outcome"]["result"], "executed", "{observed}");
    assert_eq!(observed["outcome"]["phase"], "done");
    assert_eq!(observed["outcome"]["replay_safe"], false);
    assert_eq!(observed["submission"], original["submission"]);
    if scenario == "outcome.a7.anvil" {
        assert_eq!(observed["outcome"]["reporting"], "partial");
    }
    assert_eq!(snapshot(), (3, 3));
    let mut evidence = json!({"scenario":scenario,"approval_count":main_counts.0,"external_submission_count":main_counts.1,"intervention_count":0,"remaining_external_uncertainty":true,"environment":"owned Anvil; receipt/transport faults are injected at RPC boundary"});
    outcome_evidence::attach_probes(&mut evidence, &probes);
    std::fs::write(
        std::env::var("GOFIN_OUTCOME_EVIDENCE").unwrap(),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}

async fn outcome_wait(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn outcome_child(socket: &Path, argv: Vec<String>) -> tokio::process::Child {
    tokio::process::Command::new(std::env::var("GOFIN_UNISWAP_CLI").unwrap())
        .args([
            "--identity",
            CLIENT_IDENTITY,
            "--build-id",
            "synthetic-uniswap-build",
        ])
        .args(argv)
        .env("GOFIN_DEV_IDENTITY", CLIENT_IDENTITY)
        .env("GOFIN_DEV_BUILD_ID", "synthetic-uniswap-build")
        .env("GOFIN_DEV_SOCKET", socket)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public CLI and owned Anvil"]
async fn disconnect_outcomes_on_owned_anvil() {
    let mut main_counts = (0, 0);
    let mut probes = Vec::new();
    for phase in ["preparation", "approval", "dispatch", "reporting"] {
        let (_node, rpc, _) = node(1).await;
        rpc.request(
            "anvil_setBalance",
            json!([other_owner(), "0x8ac7230489e80000"]),
        )
        .await
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outcome.sqlite");
        let faults = Arc::new(OutcomeFaults::default());
        faults.hide_receipt.store(true, Ordering::SeqCst);
        faults
            .pause_preparation
            .store(phase == "preparation", Ordering::SeqCst);
        faults
            .pause_dispatch
            .store(phase == "dispatch", Ordering::SeqCst);
        let wrapped = Arc::new(OutcomeRpc {
            inner: rpc.clone(),
            faults: faults.clone(),
        });
        let approval = Arc::new(OutcomeApproval::default());
        approval.pause.store(phase == "approval", Ordering::SeqCst);
        let clock = Arc::new(OutcomeClock(std::sync::atomic::AtomicI64::new(0)));
        let barrier = Arc::new((AtomicBool::new(false), AtomicBool::new(false)));
        let client = Client::with_reporting_barrier(
            outcome_runtime(
                &path,
                wrapped.clone(),
                approval.clone(),
                clock.clone(),
                false,
            ),
            temp.path(),
            Some(barrier.clone()),
        )
        .await;
        let (argv, request) = outcome_request("disconnected", "synthetic");
        let mut child = if phase == "reporting" {
            let original = client.call(argv.clone(), request.clone(), true).await;
            faults.hide_receipt.store(false, Ordering::SeqCst);
            rpc.request("anvil_mine", json!(["0x40"])).await.unwrap();
            outcome_child(
                &client.socket,
                args(&["status", original["id"].as_str().unwrap(), "--json"]),
            )
        } else {
            outcome_child(&client.socket, argv)
        };
        outcome_wait(if phase == "preparation" {
            &faults.preparation_entered
        } else if phase == "approval" {
            &approval.entered
        } else if phase == "dispatch" {
            &faults.dispatch_entered
        } else {
            &barrier.0
        })
        .await;
        child.kill().await.unwrap();
        let _ = child.wait().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        faults.release_preparation.store(true, Ordering::SeqCst);
        approval.release.store(true, Ordering::SeqCst);
        faults.release_dispatch.store(true, Ordering::SeqCst);
        barrier.1.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let saved = Store::open(&path)
            .unwrap()
            .by_request("disconnected")
            .unwrap()
            .unwrap();
        let original_submission = Store::open(&path)
            .unwrap()
            .submission(&saved.operation_id)
            .unwrap();
        assert_eq!(
            rpc.broadcasts.load(Ordering::SeqCst),
            usize::from(matches!(phase, "dispatch" | "reporting"))
        );
        assert_eq!(
            original_submission.is_some(),
            matches!(phase, "dispatch" | "reporting")
        );
        drop(client);
        let client = Client::new(
            outcome_runtime(&path, wrapped, approval.clone(), clock, false),
            temp.path(),
        )
        .await;
        let observed = client
            .call(
                args(&["status", &saved.operation_id, "--json"]),
                BrokerRequest::Status {
                    id: saved.operation_id.clone(),
                },
                true,
            )
            .await;
        assert_eq!(observed["id"], saved.operation_id);
        if let Some(submission) = original_submission {
            assert_eq!(observed["submission"]["reference"], submission.reference);
            assert_eq!(observed["outcome"]["replay_safe"], false);
        } else {
            assert!(observed["submission"].is_null());
            assert_eq!(observed["outcome"]["dispatch"], "not_dispatched");
        }
        if phase == "reporting" {
            assert_eq!(observed["outcome"]["phase"], "done");
        }
        let snapshot = || {
            (
                approval.count.load(Ordering::SeqCst),
                rpc.broadcasts.load(Ordering::SeqCst),
            )
        };
        let main = snapshot();
        main_counts.0 += main.0;
        main_counts.1 += main.1;
        for (id, wallet, scope) in [
            ("same", "synthetic", "same_account"),
            ("other", "synthetic-other", "different_account"),
        ] {
            let before = snapshot();
            let (argv, request) = outcome_request(id, wallet);
            let result = client.call(argv, request, true).await;
            probes.push(outcome_evidence::admission_probe(
                scope,
                before,
                snapshot(),
                &result,
            ));
        }
        assert_eq!(snapshot(), (main.0 + 2, main.1 + 2));
    }
    let mut evidence = json!({"scenario":"outcome.a20.anvil","approval_count":main_counts.0,"external_submission_count":main_counts.1,"intervention_count":0,"remaining_external_uncertainty":true,"restart_barriers":["preparation","approval before admission","accepted submission before response","confirmed before response"]});
    outcome_evidence::attach_probes(&mut evidence, &probes);
    std::fs::write(
        std::env::var("GOFIN_OUTCOME_EVIDENCE").unwrap(),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}
