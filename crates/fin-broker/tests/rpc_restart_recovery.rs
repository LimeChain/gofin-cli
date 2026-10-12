//! Synthetic RPC recovery through the real broker, Ethereum adapter and SQLite.
//! Only ephemeral loopback endpoints and a publicly known test key are used.
//! Public CLI rehearsal:
//! FIN_DEV_BUILD_ID=synthetic-rpc-recovery-build cargo build --locked -p fin-cli --example fin-dev
//! FIN_RPC_RECOVERY_CLI="$PWD/target/debug/examples/fin-dev" cargo test --locked -p fin-broker --test rpc_restart_recovery
use async_trait::async_trait;
use fin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, BrokerRuntime, ConnectionConfig, DeploymentConfig,
    NetworkConfig, RuntimeDependencies, compose_runtime,
};
use fin_domain::{
    Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, OperationRequest, Quantity,
    ResourceId, Result, now,
};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation};
use fin_evm::{
    EnvelopeSigner, EvmRuntime, FrozenTransaction, HttpRpc, LocalEnvelopeSigner,
    alloy::primitives::{Bytes, U256},
    lower_address, quantity, u256, verify_signed_envelope,
};
use fin_protocol::BrokerRequest;
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

const REQUEST_ID: &str = "synthetic-rpc-restart";
const WALLET: &str = "synthetic-wallet";
const RECIPIENT: &str = "0x2222222222222222222222222222222222222222";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
const TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

#[derive(Default)]
struct Counters {
    approvals: AtomicUsize,
    signatures: AtomicUsize,
    broadcasts: AtomicUsize,
}

struct Approval(Arc<Counters>);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.approvals.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic-rpc-restart-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct Prices;
fn usd(units: &Quantity) -> Quantity {
    quantity(
        u256(units).unwrap() * U256::from(2_000_000_000u64) / U256::from(10u64).pow(U256::from(18)),
    )
}
#[async_trait]
impl AssetValuator for Prices {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        Ok((
            usd(&amount.atoms).to_string(),
            now(),
            "synthetic:ETH-USD-2000".into(),
        ))
    }
}
#[async_trait]
impl WalletValuation for Prices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        assert_eq!(asset.reference, "native");
        Ok((usd(&units), "synthetic:ETH-USD-2000".into()))
    }
}

struct Signer {
    inner: LocalEnvelopeSigner,
    chain: Arc<Chain>,
}
#[async_trait]
impl EnvelopeSigner for Signer {
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        self.chain
            .counters
            .signatures
            .fetch_add(1, Ordering::SeqCst);
        *self.chain.transaction.lock().unwrap() = Some(transaction.clone());
        self.inner.sign_prepared(transaction).await
    }
}

// This state represents the external chain. It survives the broker's restart;
// it is not passed to the broker as an operation cache or submission journal.
struct Chain {
    store_path: PathBuf,
    counters: Arc<Counters>,
    transaction: Mutex<Option<FrozenTransaction>>,
    hash: Mutex<Option<String>>,
    receipt_ready: AtomicBool,
    sender_balance: Mutex<U256>,
    recipient_balance: Mutex<U256>,
}
impl Chain {
    fn result(&self, request: &Value) -> Value {
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        match method {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!("0x65"),
            "eth_getCode" | "eth_call" => json!("0x"),
            "eth_getBalance" => {
                let balance = if params[0] == RECIPIENT {
                    *self.recipient_balance.lock().unwrap()
                } else {
                    *self.sender_balance.lock().unwrap()
                };
                json!(format!("{balance:#x}"))
            }
            "eth_getTransactionCount" => json!(format!(
                "{:#x}",
                self.counters.broadcasts.load(Ordering::SeqCst)
            )),
            "eth_estimateGas" => json!("0x5208"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getBlockByNumber" => json!({
                "number": if params[0] == "latest" || params[0] == "finalized" { "0x65" } else { params[0].as_str().unwrap() },
                "hash": if params[0] == "0x64" { BLOCK_HASH.to_owned() } else { format!("0x{}", "55".repeat(32)) },
                "timestamp": format!("0x{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()),
                "baseFeePerGas": "0x3b9aca00"
            }),
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
                let original = self.hash.lock().unwrap().clone().unwrap();
                assert_eq!(
                    params,
                    &json!([original]),
                    "recovery must query the original hash"
                );
                if !self.receipt_ready.load(Ordering::SeqCst) {
                    return Value::Null;
                }
                if method == "eth_getTransactionReceipt" {
                    return json!({"transactionHash":original,"blockHash":BLOCK_HASH,
                        "blockNumber":"0x64","status":"0x1","gasUsed":"0x5208",
                        "effectiveGasPrice":"0x77359400","logs":[]});
                }
                let transaction = self.transaction.lock().unwrap().clone().unwrap();
                json!({"hash":original,"from":transaction.sender,"to":transaction.to,
                    "nonce":format!("{:#x}",transaction.nonce),"chainId":"0x1","type":"0x2",
                    "gas":format!("{:#x}",transaction.gas),
                    "value":format!("{:#x}",u256(&transaction.value).unwrap()),"input":transaction.calldata,
                    "maxFeePerGas":format!("{:#x}",u256(&transaction.max_fee_per_gas).unwrap()),
                    "maxPriorityFeePerGas":format!("{:#x}",u256(&transaction.max_priority_fee_per_gas).unwrap()),
                    "accessList":[],"blockHash":BLOCK_HASH,"blockNumber":"0x64"})
            }
            _ => panic!("unexpected synthetic RPC method {method}"),
        }
    }

    fn accept(&self, request: &Value) {
        assert_eq!(
            self.counters.broadcasts.fetch_add(1, Ordering::SeqCst),
            0,
            "an accepted transaction must never be sent again"
        );
        let transaction = self.transaction.lock().unwrap().clone().unwrap();
        let raw = hex::decode(
            request["params"][0]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        let original = format!("{:#x}", verify_signed_envelope(&transaction, &raw).unwrap());
        // Open a separate SQLite connection at the dispatch boundary. A memory
        // callback cannot satisfy this assertion: the original must be durable.
        let store = Store::open(&self.store_path).unwrap();
        let records = store.list_records().unwrap();
        let record = records
            .iter()
            .find(|record| record["request_id"] == REQUEST_ID)
            .unwrap();
        let id = record["id"].as_str().unwrap();
        assert_eq!(store.submission(id).unwrap().unwrap().reference, original);
        assert_eq!(record["submission"]["reference"], original);
        assert!(store.reservation_count(id).unwrap() > 0);
        *self.hash.lock().unwrap() = Some(original);
        let value = u256(&transaction.value).unwrap();
        *self.sender_balance.lock().unwrap() -= value + U256::from(42_000_000_000_000u64);
        *self.recipient_balance.lock().unwrap() += value;
    }
}

// Both endpoints observe the same external chain. A accepts the complete POST
// before dropping its response, then rate-limits reads. B serves only reads.
struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Endpoint {
    fn new(chain: Arc<Chain>, primary: bool) -> Self {
        Self::with_visibility(chain, primary, false)
    }
    fn with_visibility(chain: Arc<Chain>, primary: bool, missing_receipts: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let captured = requests.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("synthetic RPC accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                captured.lock().unwrap().push(request.clone());
                if request["method"] == "eth_sendRawTransaction" {
                    assert!(primary, "RPC B must never receive a broadcast");
                    chain.accept(&request);
                    continue; // Fully accepted request, no HTTP response.
                }
                if primary && !missing_receipts && chain.hash.lock().unwrap().is_some() {
                    write!(stream, "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let result = if primary
                        && missing_receipts
                        && matches!(
                            request["method"].as_str(),
                            Some("eth_getTransactionReceipt")
                        ) {
                        Value::Null
                    } else {
                        chain.result(&request)
                    };
                    let body =
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
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
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "synthetic RPC received an incomplete request");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0);
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn broker(path: &Path, urls: Vec<String>, chain: Arc<Chain>) -> Arc<BrokerRuntime> {
    let signer = LocalEnvelopeSigner::from_private_key(TEST_KEY).unwrap();
    let address = lower_address(signer.address());
    let asset = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let config = BrokerConfigV2 {
        version: 2,
        networks: vec![NetworkConfig::ethereum_mainnet()],
        deployments: vec![DeploymentConfig::ethereum_mainnet()],
        connections: vec![ConnectionConfig::Ethereum {
            name: WALLET.into(),
            resource: ResourceId(format!("ethereum:1:{address}")),
            credential_ref: Some("00000000-0000-4000-8000-000000000001".into()),
            network: "ethereum:1".into(),
            address: address.clone(),
            signing_enabled: true,
            assets: vec![asset],
        }],
        policy: BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() },
        extensions: BTreeMap::new(),
    };
    let rpc = HttpRpc::isolated_localhost_pool(urls, 1).unwrap();
    let adapter = EthereumAdapter::new(
        EthereumConfig::wallet_only(1, 2, vec![]),
        vec![WalletConnection {
            name: WALLET.into(),
            address,
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(
            Arc::new(rpc),
            1,
            Some(Arc::new(Signer {
                inner: signer,
                chain: chain.clone(),
            })),
        )),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Prices));
    compose_runtime(
        config,
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(adapter)],
            authorizer: Arc::new(Approval(chain.counters.clone())),
            valuator: Arc::new(Prices),
            bridge: None,
        },
    )
    .unwrap()
}

// Optional public CLI rehearsal uses the development binary's existing socket
// protocol. The installed service and native approval are never invoked.
struct Client {
    runtime: Arc<BrokerRuntime>,
    socket: PathBuf,
    server: Option<tokio::task::JoinHandle<()>>,
}
impl Client {
    async fn new(runtime: Arc<BrokerRuntime>, directory: &Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = directory.join("cli.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let broker = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let hello: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(hello["identity"], "synthetic-rpc-recovery");
                assert_eq!(hello["build_id"], "synthetic-rpc-recovery-build");
                assert_eq!(hello["mode"], "synthetic");
                let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
                reader
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                let input: Value = serde_json::from_str(&line).unwrap();
                let request: BrokerRequest =
                    serde_json::from_value(input["request"].clone()).unwrap();
                let response = broker.handle(request).await;
                let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                reader
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        Self {
            runtime,
            socket,
            server: Some(server),
        }
    }
    async fn call(&self, request: BrokerRequest) -> Value {
        let Ok(binary) = std::env::var("FIN_RPC_RECOVERY_CLI") else {
            let response = self.runtime.handle(request).await;
            assert!(response.ok, "{:?}", response.error);
            return response.result.unwrap();
        };
        let args = match request {
            BrokerRequest::Execute { request } => vec![
                "execute".into(),
                request.operation,
                "--input".into(),
                request.input.to_string(),
                "--id".into(),
                request.request_id,
                "--json".into(),
            ],
            BrokerRequest::Status { id } => vec!["status".into(), id, "--json".into()],
            BrokerRequest::Doctor => vec!["doctor".into(), "--json".into()],
            _ => panic!("unsupported RPC recovery fixture command"),
        };
        let socket = self.socket.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(binary)
                .args([
                    "--identity",
                    "synthetic-rpc-recovery",
                    "--build-id",
                    "synthetic-rpc-recovery-build",
                ])
                .args(args)
                .env("FIN_DEV_IDENTITY", "synthetic-rpc-recovery")
                .env("FIN_DEV_BUILD_ID", "synthetic-rpc-recovery-build")
                .env("FIN_DEV_SOCKET", socket)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.stderr.is_empty(),
            "JSON stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "CLI failure: {result}");
        result
    }
    async fn stop(mut self) {
        let server = self.server.take().unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        // Await cancellation so even the socket server's runtime handle is gone.
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

async fn restart_recovery(missing_receipts: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.sqlite");
    let initial_balance = U256::from(100_000_000_000_000_000u64);
    let counters = Arc::new(Counters::default());
    let chain = Arc::new(Chain {
        store_path: path.clone(),
        counters: counters.clone(),
        transaction: Mutex::new(None),
        hash: Mutex::new(None),
        receipt_ready: AtomicBool::new(false),
        sender_balance: Mutex::new(initial_balance),
        recipient_balance: Mutex::new(U256::ZERO),
    });
    let primary = Endpoint::with_visibility(chain.clone(), true, missing_receipts);
    let backup = Endpoint::new(chain.clone(), false);
    let urls = vec![primary.url.clone(), backup.url.clone()];
    let client = Client::new(broker(&path, urls.clone(), chain.clone()), directory.path()).await;
    let request = OperationRequest {
        request_id: REQUEST_ID.into(),
        operation: "wallet.transfer".into(),
        input: json!({"request_id":REQUEST_ID,"wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native",
            "amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),
        run_id: None,
    };
    let unknown = client
        .call(BrokerRequest::Execute {
            request: request.clone(),
        })
        .await;
    assert_eq!(unknown["status"], "unknown", "{unknown}");
    let id = unknown["id"].as_str().unwrap().to_owned();
    let hash = chain.hash.lock().unwrap().clone().unwrap();
    assert_eq!(unknown["submission"]["reference"], hash);
    let allocated = client.runtime.engine.allocation_used_usd_micros().unwrap();
    assert_ne!(allocated, "0");
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.get_record(&id).unwrap().unwrap()["status"], "unknown");
    let submission = saved.submission(&id).unwrap().unwrap();
    let reservations = saved.reservations(&id).unwrap();
    assert!(!reservations.is_empty());
    drop(saved);
    let old_runtime = Arc::downgrade(&client.runtime);
    let primary_reads = primary.requests.lock().unwrap().len();
    client.stop().await; // Drop broker, engine, adapter, signer and HTTP caches.
    assert!(old_runtime.upgrade().is_none());

    let restart_directory = directory.path().join("restart");
    std::fs::create_dir(&restart_directory).unwrap();
    let restarted = Client::new(broker(&path, urls, chain.clone()), &restart_directory).await;
    restarted.runtime.engine.recover_interrupted().unwrap();
    assert_eq!(
        restarted
            .runtime
            .engine
            .allocation_used_usd_micros()
            .unwrap(),
        allocated
    );
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.get_record(&id).unwrap().unwrap()["status"], "unknown");
    assert_eq!(saved.reservations(&id).unwrap(), reservations);
    assert_eq!(saved.submission(&id).unwrap().unwrap(), submission);
    drop(saved);
    let pending = restarted
        .call(BrokerRequest::Status { id: id.clone() })
        .await;
    if missing_receipts {
        assert!(primary.requests.lock().unwrap().len() > primary_reads);
        assert!(
            primary
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "eth_getTransactionReceipt")
        );
    } else {
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            primary_reads + 1,
            "fresh client verifies A once then respects its cooldown"
        );
    }
    assert_eq!(pending["id"], id);
    assert_eq!(pending["submission"]["reference"], hash);
    assert_eq!(pending["status"], "pending");
    assert!(pending["effects"].is_null());
    let reads = backup.requests.lock().unwrap().clone();
    assert!(
        reads
            .iter()
            .any(|request| request["method"] == "eth_getTransactionReceipt"),
        "restarted broker must read the persisted original receipt through RPC B"
    );
    assert_eq!(
        Store::open(&path).unwrap().reservations(&id).unwrap(),
        reservations
    );
    assert_eq!(
        restarted
            .runtime
            .engine
            .allocation_used_usd_micros()
            .unwrap(),
        allocated
    );

    // Even an execute retry with the same ID must reconcile, never dispatch.
    let duplicate_pending = restarted
        .call(BrokerRequest::Execute {
            request: request.clone(),
        })
        .await;
    assert_eq!(duplicate_pending["id"], id);
    assert_eq!(duplicate_pending["status"], "pending");
    assert_eq!(duplicate_pending["submission"]["reference"], hash);
    assert!(duplicate_pending["effects"].is_null());
    assert_eq!(
        Store::open(&path).unwrap().reservations(&id).unwrap(),
        reservations
    );
    assert_eq!(counters.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(counters.signatures.load(Ordering::SeqCst), 1);
    assert_eq!(counters.broadcasts.load(Ordering::SeqCst), 1);
    chain.receipt_ready.store(true, Ordering::SeqCst);
    let completed = restarted
        .call(BrokerRequest::Status { id: id.clone() })
        .await;
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(completed["id"], id);
    assert_eq!(completed["submission"]["reference"], hash);
    let before_doctor = (
        primary.requests.lock().unwrap().len(),
        backup.requests.lock().unwrap().len(),
    );
    let doctor = restarted.call(BrokerRequest::Doctor).await;
    assert_eq!(
        before_doctor,
        (
            primary.requests.lock().unwrap().len(),
            backup.requests.lock().unwrap().len()
        )
    );
    let health = &doctor["rpc_health"][0];
    assert_eq!(health["chain_id"], 1);
    assert_eq!(health["last_failover"]["selected_endpoint"], "rpc-2");
    assert_eq!(health["last_failover"]["outcome"], "ok");
    if missing_receipts {
        assert_eq!(
            health["last_failover"]["method"],
            "eth_getTransactionReceipt"
        );
        assert_eq!(health["last_failover"]["attempts"][0]["reason"], "missing");
    }
    let encoded = doctor.to_string();
    for private in [&primary.url, &backup.url, TEST_KEY] {
        assert!(!encoded.contains(private));
    }
    let effects = completed["effects"]["effects"].as_array().unwrap();
    assert_eq!(effects.len(), 2, "one gas charge and one exact transfer");
    let transfer = effects
        .iter()
        .find(|effect| effect["kind"] == "transfer")
        .unwrap();
    assert_eq!(transfer["to"], format!("ethereum:1:{RECIPIENT}"));
    assert_eq!(transfer["debit"]["atoms"], "1000000000000000");
    assert_eq!(transfer["credit"]["atoms"], "1000000000000000");
    let cost = effects
        .iter()
        .find(|effect| effect["kind"] == "cost")
        .unwrap();
    assert_eq!(cost["category"], "ethereum_gas");
    assert_eq!(cost["amount"]["atoms"], "42000000000000");
    assert_eq!(completed["effects"]["submission"]["reference"], hash);
    assert_eq!(
        completed["effects"]["evidence"]["original_envelope_verified"],
        true
    );
    assert!(
        !completed["effects"]["proof_digest"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let saved = Store::open(&path).unwrap();
    assert_eq!(
        saved.get_record(&id).unwrap().unwrap()["status"],
        "completed"
    );
    assert_eq!(saved.submission(&id).unwrap().unwrap(), submission);
    let settled = saved.get_record(&id).unwrap().unwrap();
    assert_eq!(
        settled["effects"]["effects"],
        completed["effects"]["effects"]
    );
    assert_eq!(
        settled["effects"]["proof_digest"],
        completed["effects"]["proof_digest"]
    );
    assert_eq!(saved.reservation_count(&id).unwrap(), 0);
    drop(saved);
    // Settlement releases resource locks, but not the rolling 24-hour usage.
    assert_eq!(
        restarted
            .runtime
            .engine
            .allocation_used_usd_micros()
            .unwrap(),
        allocated
    );
    let duplicate = restarted.call(BrokerRequest::Execute { request }).await;
    assert_eq!(duplicate, completed);
    let repeated = restarted.call(BrokerRequest::Status { id }).await;
    assert_eq!(repeated, completed);
    assert_eq!(counters.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(counters.signatures.load(Ordering::SeqCst), 1);
    assert_eq!(counters.broadcasts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *chain.recipient_balance.lock().unwrap(),
        U256::from(1_000_000_000_000_000u64)
    );
    assert_eq!(
        *chain.sender_balance.lock().unwrap(),
        initial_balance - U256::from(1_042_000_000_000_000u64)
    );
    assert_eq!(Store::open(&path).unwrap().list_records().unwrap().len(), 1);
    restarted.stop().await;
    for (endpoint, expected) in [(&primary, 1), (&backup, 0)] {
        assert_eq!(
            endpoint
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "eth_sendRawTransaction")
                .count(),
            expected
        );
    }
}

#[tokio::test]
async fn lost_broadcast_recovers_after_broker_restart_from_sqlite_through_backup_without_resending()
{
    restart_recovery(false).await;
}

#[tokio::test]
async fn responsive_primary_missing_receipt_recovers_through_backup_after_restart() {
    restart_recovery(true).await;
}
