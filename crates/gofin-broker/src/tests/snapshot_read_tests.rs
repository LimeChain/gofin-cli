//! Snapshot aggregation over real synthetic HTTP reads, without financial writes.
use super::*;
use gofin_evm::EvmRpc;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

const WALLET: &str = "0x1111111111111111111111111111111111111111";

struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    balance_gate: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    async fn new(balance: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let balance_gate = Arc::new(Semaphore::new(0));
        let captured = requests.clone();
        let gate = balance_gate.clone();
        let balance = balance.to_owned();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let (stream, _) = incoming.unwrap();
                        let captured = captured.clone();
                        let gate = gate.clone();
                        let balance = balance.clone();
                        connections.spawn(async move {
                            serve_read(stream, captured, gate, balance).await;
                        });
                    }
                    Some(result) = connections.join_next() => { result.unwrap(); }
                }
            }
        });
        Self {
            url,
            requests,
            balance_gate,
            task,
        }
    }

    fn balance_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "eth_getBalance")
            .cloned()
            .collect()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Aborting the listener drops its JoinSet and all held fixture requests.
        self.task.abort();
    }
}

async fn serve_read(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Value>>>,
    balance_gate: Arc<Semaphore>,
    balance: String,
) {
    let Some(request) = read_http_request(&mut stream).await else {
        return; // A losing read may be cancelled before its HTTP body arrives.
    };
    requests.lock().unwrap().push(request.clone());
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!("0x1"),
        "eth_blockNumber" => json!("0x16"),
        "eth_getBlockByNumber" => json!({
            "number":"0x16", "hash":format!("0x{}", "11".repeat(32)),
            "timestamp":format!("0x{:x}", chrono::Utc::now().timestamp()),
        }),
        "eth_getTransactionCount" => json!("0x0"),
        "eth_getCode" => {
            assert_eq!(
                request["params"][0],
                "0xca11bde05977b3631167028862be2a173976ca11"
            );
            // This synthetic chain has no reviewed Multicall deployment. The
            // native balance still exercises the original gated read path.
            json!("0x")
        }
        "eth_getBalance" => {
            let mut closed = [0];
            tokio::select! {
                permit = balance_gate.acquire() => { permit.unwrap().forget(); }
                _ = stream.read(&mut closed) => { return; }
            }
            json!(balance)
        }
        method => panic!("Snapshot fixture received an unreviewed or write method: {method}"),
    };
    let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // A simultaneous peer can win while this response is being written.
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn read_http_request(stream: &mut TcpStream) -> Option<Value> {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let start = end + 4;
            if bytes.len() >= start + length {
                return Some(serde_json::from_slice(&bytes[start..start + length]).unwrap());
            }
        }
    }
}

async fn rpc_runtime() -> (
    Endpoint,
    Endpoint,
    Arc<gofin_evm::HttpRpc>,
    Arc<BrokerRuntime>,
) {
    let primary = Endpoint::new("0xaa").await;
    let backup = Endpoint::new("0x2a").await;
    let rpc = Arc::new(
        gofin_evm::HttpRpc::isolated_localhost_pool(
            vec![primary.url.clone(), backup.url.clone()],
            1,
        )
        .unwrap(),
    );
    let wallet = gofin_ethereum::EthereumAdapter::new(
        gofin_ethereum::EthereumConfig::wallet_only(1, 1, vec![]),
        vec![gofin_ethereum::WalletConnection {
            name: "synthetic".into(),
            address: WALLET.into(),
            signing_enabled: false,
        }],
        Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 1, None)),
    )
    .unwrap();
    let router = Arc::new(evm_router::EvmNetworkRouter::new(
        [(1, Arc::new(wallet))].into_iter().collect(),
    ));
    // Use the real broker composition so the SQLite journal is available;
    // the shared fixture authorizer rejects any attempt to request approval.
    let router = Arc::new(wallet_router::WalletFamilyRouter::new(
        Some(router),
        BTreeMap::new(),
    ));
    let runtime = delayed_runtime(vec![router]);
    (primary, backup, rpc, runtime)
}

async fn wait_for_both_balances(primary: &Endpoint, backup: &Endpoint) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while primary.balance_requests().is_empty() || backup.balance_requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("both providers must receive the balance read");
}

#[tokio::test]
async fn composed_rpc_snapshot_keeps_simultaneous_reads_past_command_cutoffs() {
    let (primary, backup, rpc, runtime) = rpc_runtime().await;
    let mut snapshot = tokio::spawn(async move {
        runtime
            .handle(BrokerRequest::Read {
                operation: "gofin.snapshot".into(),
                input: json!({"networks":["ethereum:1"]}),
            })
            .await
    });
    wait_for_both_balances(&primary, &backup).await;

    // Advance the timeout clock while both actual HTTP responses are held.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(181)).await;
    tokio::time::resume();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut snapshot)
            .await
            .is_err(),
        "a slow read must remain pending instead of becoming a timed partial snapshot"
    );
    backup.balance_gate.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(2), snapshot)
        .await
        .expect("releasing the valid backup must finish the snapshot")
        .unwrap();
    assert!(response.ok, "{:?}", response.error);
    let snapshot: StateSnapshot = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(snapshot.complete, "{:?}", snapshot.pending);
    assert!(snapshot.pending.is_empty());
    assert_eq!(snapshot.state.holdings.len(), 1);
    assert_eq!(snapshot.state.holdings[0].amount.atoms.to_string(), "42");
    assert_eq!(
        snapshot.state.holdings[0].amount.asset.network,
        "ethereum:1"
    );
    assert!(snapshot.coverage.contains(&format!("evm:{WALLET}")));
    let a = primary.balance_requests();
    let b = backup.balance_requests();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0]["params"], b[0]["params"]);
    assert_eq!(
        a[0]["params"][1],
        json!({"blockHash":format!("0x{}", "11".repeat(32)),"requireCanonical":true})
    );
    for endpoint in [&primary, &backup] {
        assert!(
            endpoint
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| { request["method"] != "eth_sendRawTransaction" })
        );
    }
    assert!(rpc.diagnostics()["read_timeout_ms"].is_null());
}

fn balances_request() -> BrokerRequest {
    BrokerRequest::Read {
        operation: "wallet.balances".into(),
        input: json!({"wallet":"synthetic","network":"evm","chain_id":1,"tokens":[]}),
    }
}

struct HeldMeteoraRpc {
    started: tokio::sync::Notify,
    release: Semaphore,
    active: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
}

struct ActiveMeteoraRead<'a>(&'a AtomicUsize);

impl Drop for ActiveMeteoraRead<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl gofin_solana::rpc::SolanaRpc for HeldMeteoraRpc {
    async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
        self.requests.lock().unwrap().push((method.into(), params));
        match method {
            "getProgramAccounts" => {
                self.active.fetch_add(1, Ordering::SeqCst);
                let _active = ActiveMeteoraRead(&self.active);
                self.started.notify_one();
                self.release.acquire().await.unwrap().forget();
                Ok(json!([]))
            }
            "getSlot" => Ok(json!(42)),
            _ => panic!("Meteora snapshot attempted an unreviewed RPC method: {method}"),
        }
    }
}

struct NoMeteoraSigners;

impl gofin_meteora::SecondarySignerVault for NoMeteoraSigners {
    fn create_or_load(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> gofin_domain::Result<(String, Arc<gofin_solana::SolanaSigner>)> {
        panic!("A snapshot must not create a signer")
    }

    fn load(&self, _: &str) -> gofin_domain::Result<Arc<gofin_solana::SolanaSigner>> {
        panic!("A snapshot must not load a signer")
    }
}

const METEORA_WALLET: &str = "11111111111111111111111111111111";

fn meteora_snapshot_request() -> BrokerRequest {
    BrokerRequest::Read {
        operation: "gofin.snapshot".into(),
        input: json!({"resources":[format!("solana:mainnet-beta:{METEORA_WALLET}")]}),
    }
}

fn meteora_rpc_runtime() -> (Arc<HeldMeteoraRpc>, Arc<BrokerRuntime>) {
    let rpc = Arc::new(HeldMeteoraRpc {
        started: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let prices = Arc::new(FixedSolanaPrices);
    let wallet = Arc::new(
        gofin_solana::SolanaWalletAdapter::new(
            "synthetic-meteora",
            METEORA_WALLET,
            None,
            rpc.clone(),
            prices.clone(),
        )
        .unwrap(),
    );
    let adapter = gofin_meteora::MeteoraAdapter::new(
        BTreeMap::from([("synthetic-meteora".into(), wallet)]),
        vec![
            Arc::new(gofin_meteora::dlmm::DlmmProtocol::new()),
            Arc::new(gofin_meteora::damm_v2::DammV2Protocol::new()),
        ],
        prices,
        Arc::new(NoMeteoraSigners),
    )
    .unwrap();
    (rpc, delayed_runtime(vec![Arc::new(adapter)]))
}

#[tokio::test]
async fn meteora_snapshot_waits_for_rpc_past_observer_deadline() {
    let (rpc, runtime) = meteora_rpc_runtime();
    let request = meteora_snapshot_request();
    let classified_rpc = runtime.request_uses_rpc_reads(&request);
    let mut observation = tokio::spawn(async move { runtime.handle(request).await });
    rpc.started.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(181)).await;
    tokio::time::resume();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut observation)
            .await
            .is_err(),
        "Meteora RPC inventory must wait instead of returning a timed partial snapshot"
    );
    assert!(classified_rpc);
    assert_eq!(rpc.active.load(Ordering::SeqCst), 1);
    rpc.release.add_permits(2);
    let response = tokio::time::timeout(Duration::from_secs(2), observation)
        .await
        .unwrap()
        .unwrap();
    assert!(response.ok, "{:?}", response.error);
    let snapshot: StateSnapshot = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(snapshot.complete, "{:?}", snapshot.pending);
    assert!(snapshot.pending.is_empty());
    assert!(
        snapshot
            .coverage
            .contains(&format!("solana:mainnet-beta:{METEORA_WALLET}"))
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|entry| entry.contains("PositionV2"))
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|entry| entry.contains("DAMM position PDAs"))
    );
    assert_eq!(rpc.active.load(Ordering::SeqCst), 0);
    let requests = rpc.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(method, _)| method == "getProgramAccounts")
            .count(),
        2
    );
}

#[tokio::test]
async fn disconnect_cancels_meteora_snapshot_rpc_inventory() {
    let (rpc, runtime) = meteora_rpc_runtime();
    let shared = Arc::new(RwLock::new(runtime));
    let serving_runtime = shared.clone();
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &serving_runtime,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    let mut bytes = serde_json::to_vec(&gofin_protocol::progress::RequestEnvelope {
        request: meteora_snapshot_request(),
        progress: false,
        progress_details: false,
        cancel_on_disconnect: true,
    })
    .unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
    let mut client = BufReader::new(client);
    read_policy(&mut client).await;
    rpc.started.notified().await;
    assert_eq!(rpc.active.load(Ordering::SeqCst), 1);
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap();
    let _runtime = tokio::time::timeout(Duration::from_millis(20), shared.write())
        .await
        .expect("cancelled Meteora snapshot must release its runtime revision");
    tokio::time::timeout(Duration::from_secs(2), async {
        while rpc.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect must drop the held Meteora RPC read");
    assert_eq!(rpc.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rpc_deadline_exception_uses_resolved_operation_and_snapshot_scope() {
    let (_, _, _, runtime) = rpc_runtime().await;
    assert!(runtime.request_uses_rpc_reads(&balances_request()));
    assert!(runtime.request_uses_rpc_reads(&BrokerRequest::Snapshot));
    assert!(runtime.request_uses_rpc_reads(&BrokerRequest::Read {
        operation: "gofin.snapshot".into(),
        input: json!({"resources":[format!("evm:{WALLET}")]}),
    }));
    for input in [
        json!({"networks":["binance"]}),
        json!({"resources":["binance:synthetic"]}),
        json!({"assets":[{"network":"binance","reference":"USDC","decimals":6}]}),
    ] {
        assert!(!runtime.request_uses_rpc_reads(&BrokerRequest::Read {
            operation: "gofin.snapshot".into(),
            input,
        }));
    }
    assert!(
        !runtime.request_uses_rpc_reads(&BrokerRequest::Read {
            operation: "wallet.transfer".into(),
            input: json!({"wallet":"synthetic","network":"evm","chain_id":1}),
        }),
        "RPC metadata must not allow a financial write through the read route"
    );
    assert!(!runtime.request_uses_rpc_reads(&BrokerRequest::Capabilities));
    assert!(!runtime.request_uses_rpc_reads(&BrokerRequest::Status {
        id: "missing".into()
    }));
}

async fn send_rpc_read(client: &mut UnixStream) {
    let mut bytes = serde_json::to_vec(&gofin_protocol::progress::RequestEnvelope {
        request: balances_request(),
        progress: false,
        progress_details: false,
        cancel_on_disconnect: true,
    })
    .unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
}

async fn read_policy(client: &mut BufReader<UnixStream>) {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    let frame: gofin_protocol::progress::RpcReadFrame = serde_json::from_str(&line).unwrap();
    assert!(frame.rpc_read);
}

#[tokio::test]
async fn negotiated_wallet_read_waits_past_command_budgets_and_keeps_final_json() {
    let (primary, backup, _, runtime) = rpc_runtime().await;
    let shared = Arc::new(RwLock::new(runtime));
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &shared,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    send_rpc_read(&mut client).await;
    let mut client = BufReader::new(client);
    read_policy(&mut client).await;
    wait_for_both_balances(&primary, &backup).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(181)).await;
    tokio::time::resume();
    assert!(
        !serving.is_finished(),
        "broker and engine must keep the RPC observation alive"
    );
    backup.balance_gate.add_permits(1);
    let mut bytes = vec![];
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.unwrap()["data"][0]["amount"]["atoms"], "42");
    serving.await.unwrap();
}

#[tokio::test]
async fn disconnecting_negotiated_read_cancels_both_rpcs_and_releases_runtime() {
    let (primary, backup, rpc, runtime) = rpc_runtime().await;
    let shared = Arc::new(RwLock::new(runtime));
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving_runtime = shared.clone();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &serving_runtime,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    send_rpc_read(&mut client).await;
    let mut client = BufReader::new(client);
    read_policy(&mut client).await;
    wait_for_both_balances(&primary, &backup).await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap();
    let _runtime = tokio::time::timeout(Duration::from_millis(20), shared.write())
        .await
        .expect("cancelled read must release the pinned runtime revision");
    let trace = &rpc.diagnostics()["last_read"];
    assert_eq!(trace["outcome"], "cancelled");
    assert_eq!(trace["attempts"].as_array().unwrap().len(), 2);
    assert!(
        trace["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| attempt["reason"] == "cancelled")
    );
    assert_eq!(primary.balance_requests().len(), 1);
    assert_eq!(backup.balance_requests().len(), 1);
}

#[tokio::test]
async fn non_rpc_snapshot_keeps_deadline_even_with_disconnect_opt_in() {
    let started = Arc::new(tokio::sync::Notify::new());
    let runtime = delayed_runtime(vec![Arc::new(DelayedObserver {
        name: "binance:synthetic",
        delay: Duration::from_secs(300),
        fail: false,
        started: started.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
    })]);
    let request = BrokerRequest::Read {
        operation: "gofin.snapshot".into(),
        input: json!({"networks":["binance"]}),
    };
    assert!(!runtime.request_uses_rpc_reads(&request));
    let shared = RwLock::new(runtime);
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &shared,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    let mut bytes = serde_json::to_vec(&gofin_protocol::progress::RequestEnvelope {
        request,
        progress: false,
        progress_details: false,
        cancel_on_disconnect: true,
    })
    .unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
    started.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    bytes.clear();
    tokio::time::timeout(Duration::from_millis(20), client.read_to_end(&mut bytes))
        .await
        .expect("non-RPC snapshots must retain their original 10-second observer cutoff")
        .unwrap();
    // A non-RPC request emits only its original final response, no policy frame.
    let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["complete"], false);
    assert!(
        result["pending"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str().unwrap().contains("OBSERVER_READ_TIMEOUT"))
    );
    serving.await.unwrap();
}

#[tokio::test]
async fn legacy_half_closed_client_can_finish_an_rpc_read() {
    let (primary, backup, _, runtime) = rpc_runtime().await;
    let shared = RwLock::new(runtime);
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &shared,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    let mut bytes = serde_json::to_vec(&balances_request()).unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
    client.shutdown().await.unwrap();
    wait_for_both_balances(&primary, &backup).await;
    backup.balance_gate.add_permits(1);
    bytes.clear();
    client.read_to_end(&mut bytes).await.unwrap();
    let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(response.ok, "{:?}", response.error);
    serving.await.unwrap();
}

#[tokio::test]
async fn legacy_rpc_client_keeps_bounded_wait_and_releases_stalled_requests() {
    let (primary, backup, rpc, runtime) = rpc_runtime().await;
    let shared = Arc::new(RwLock::new(runtime));
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving_runtime = shared.clone();
    let serving = tokio::spawn(async move {
        serve_connection_with_timeouts(
            server,
            &serving_runtime,
            unsafe { libc::geteuid() },
            IpcTimeouts::PRODUCTION,
        )
        .await
        .unwrap();
    });
    let mut bytes = serde_json::to_vec(&balances_request()).unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).await.unwrap();
    client.shutdown().await.unwrap();
    wait_for_both_balances(&primary, &backup).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(121)).await;
    tokio::time::resume();
    bytes.clear();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response.error.unwrap().code, "BROKER_READ_TIMEOUT");
    serving.await.unwrap();
    let _runtime = tokio::time::timeout(Duration::from_millis(20), shared.write())
        .await
        .expect("legacy timeout must release the runtime revision");
    assert_eq!(rpc.diagnostics()["last_read"]["outcome"], "cancelled");
}
