//! Synthetic outcome recovery through the real broker, Ethereum adapter and SQLite.
//! Only ephemeral loopback endpoints and a publicly known test key are used.
//! Public CLI rehearsal:
//! GOFIN_DEV_BUILD_ID=synthetic-rpc-recovery-build cargo build --locked -p gofin-cli --example gofin-dev
//! GOFIN_RPC_RECOVERY_CLI="$PWD/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --test rpc_restart_recovery
use async_trait::async_trait;
use gofin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, BrokerRuntime, ConnectionConfig, DeploymentConfig,
    NetworkConfig, RuntimeDependencies, compose_runtime_with_clock,
};
use gofin_domain::{
    Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, OperationRequest, Quantity,
    ResourceId, Result, now,
};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation};
use gofin_evm::{
    EnvelopeSigner, EvmRuntime, FrozenTransaction, HttpRpc, LocalEnvelopeSigner,
    alloy::primitives::{Bytes, U256},
    lower_address, quantity, u256, verify_signed_envelope,
};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

#[path = "support/outcome_evidence.rs"]
mod outcome_evidence;

const REQUEST_ID: &str = "synthetic-rpc-restart";
const WALLET: &str = "synthetic-wallet";
const RECIPIENT: &str = "0x2222222222222222222222222222222222222222";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
const TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const SECOND_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000002";

#[derive(Default)]
struct Counters {
    clock_offset_seconds: AtomicI64,
    approval_bindings: Mutex<Vec<String>>,
    broadcast_hashes: Mutex<Vec<String>>,
    approvals: AtomicUsize,
    signatures: AtomicUsize,
    broadcasts: AtomicUsize,
    reject_approval: AtomicBool,
    require_review: AtomicBool,
    reviews: Mutex<Vec<Value>>,
    pause_preparation: AtomicBool,
    preparation_started: AtomicBool,
    release_preparation: AtomicBool,
    pause_reporting: AtomicBool,
    reporting_started: AtomicBool,
    release_reporting: AtomicBool,
    fail_preparation_rpc: AtomicBool,
}

struct FixtureClock(Arc<Counters>);
impl gofin_domain::clock::Clock for FixtureClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
            + chrono::Duration::seconds(self.0.clock_offset_seconds.load(Ordering::SeqCst))
    }
}

impl Counters {
    fn snapshot(&self) -> (usize, usize) {
        (
            self.approvals.load(Ordering::SeqCst),
            self.broadcasts.load(Ordering::SeqCst),
        )
    }
    fn for_record(&self, record: &Value) -> (usize, usize) {
        let binding = record["approval"]["binding_hash"].as_str().unwrap();
        let hash = record["submission"]["reference"].as_str().unwrap();
        (
            self.approval_bindings
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v.as_str() == binding)
                .count(),
            self.broadcast_hashes
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v.as_str() == hash)
                .count(),
        )
    }
}

struct Approval(Arc<Counters>);
#[async_trait]
impl Authorizer for Approval {
    fn requires_financial_review(&self) -> bool {
        self.0.require_review.load(Ordering::SeqCst)
    }
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.approvals.fetch_add(1, Ordering::SeqCst);
        self.0
            .approval_bindings
            .lock()
            .unwrap()
            .push(challenge.binding_hash.clone());
        if let Some(review) = &challenge.review {
            self.0
                .reviews
                .lock()
                .unwrap()
                .push(serde_json::to_value(review).unwrap());
        }
        if self.0.reject_approval.load(Ordering::SeqCst) {
            return Err(gofin_domain::GoFinError::new(
                "AUTHORIZATION_CANCELLED",
                "approval",
                "Synthetic cancellation.",
                "Review before retrying.",
            ));
        }
        Ok(ApprovalAudit {
            mechanism: "synthetic-rpc-restart-only".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct Prices(bool);
fn usd(units: &Quantity) -> Quantity {
    quantity(
        u256(units).unwrap() * U256::from(2_000_000_000u64) / U256::from(10u64).pow(U256::from(18)),
    )
}
#[async_trait]
impl AssetValuator for Prices {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        if self.0 {
            return Err(gofin_domain::GoFinError::new(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Synthetic price source unavailable.",
                "Continue in native units.",
            ));
        }
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
        if self.0 {
            return Err(gofin_domain::GoFinError::new(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Synthetic price source unavailable.",
                "Continue in native units.",
            ));
        }
        assert_eq!(asset.reference, "native");
        Ok((usd(&units), "synthetic:ETH-USD-2000".into()))
    }
}

struct Signer {
    inner: LocalEnvelopeSigner,
    second: LocalEnvelopeSigner,
    chain: Arc<Chain>,
}
#[async_trait]
impl EnvelopeSigner for Signer {
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        self.chain
            .counters
            .signatures
            .fetch_add(1, Ordering::SeqCst);
        self.chain
            .prepared
            .lock()
            .unwrap()
            .push(transaction.clone());
        if lower_address(self.second.address()) == transaction.sender {
            self.second.sign_prepared(transaction).await
        } else {
            self.inner.sign_prepared(transaction).await
        }
    }
}

// This state represents the external chain. It survives the broker's restart;
// it is not passed to the broker as an operation cache or submission journal.
struct Chain {
    chain_id: u64,
    store_path: PathBuf,
    counters: Arc<Counters>,
    prepared: Mutex<Vec<FrozenTransaction>>,
    stall_first_response: AtomicBool,
    release_first_response: AtomicBool,
    transactions: Mutex<BTreeMap<String, FrozenTransaction>>,
    hash: Mutex<Option<String>>,
    multiple: bool,
    price_unavailable: bool,
    receipt_ready: AtomicBool,
    sender_balance: Mutex<U256>,
    second_balance: Mutex<U256>,
    recipient_balance: Mutex<U256>,
}
impl Chain {
    fn result(&self, request: &Value) -> Value {
        let method = request["method"].as_str().unwrap();
        let params = &request["params"];
        match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain_id)),
            "eth_blockNumber" => json!("0x65"),
            "eth_getCode" | "eth_call" => json!("0x"),
            "eth_getBalance" => {
                let balance = if params[0] == RECIPIENT {
                    *self.recipient_balance.lock().unwrap()
                } else if params[0]
                    == lower_address(
                        LocalEnvelopeSigner::from_private_key(SECOND_KEY)
                            .unwrap()
                            .address(),
                    )
                {
                    *self.second_balance.lock().unwrap()
                } else {
                    *self.sender_balance.lock().unwrap()
                };
                json!(format!("{balance:#x}"))
            }
            "eth_getTransactionCount" => {
                let address = params[0].as_str().unwrap_or_default();
                let count = self
                    .transactions
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|tx| tx.sender.eq_ignore_ascii_case(address))
                    .count();
                json!(format!("{count:#x}"))
            }
            "eth_estimateGas" => {
                if self.counters.pause_preparation.load(Ordering::SeqCst) {
                    self.counters
                        .preparation_started
                        .store(true, Ordering::SeqCst);
                    while !self.counters.release_preparation.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                json!("0x5208")
            }
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getBlockByNumber" => json!({
                "number": if params[0] == "latest" || params[0] == "finalized" { "0x65" } else { params[0].as_str().unwrap() },
                "hash": if params[0] == "0x64" { BLOCK_HASH.to_owned() } else { format!("0x{}", "55".repeat(32)) },
                "timestamp": format!("0x{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()),
                "baseFeePerGas": "0x3b9aca00"
            }),
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
                let original = params[0].as_str().unwrap().to_owned();
                assert!(
                    self.transactions.lock().unwrap().contains_key(&original),
                    "receipt must use a persisted original hash"
                );
                if !self.receipt_ready.load(Ordering::SeqCst) {
                    return Value::Null;
                }
                if method == "eth_getTransactionReceipt" {
                    return json!({"transactionHash":original,"blockHash":BLOCK_HASH,
                        "blockNumber":"0x64","status":"0x1","gasUsed":"0x5208",
                        "effectiveGasPrice":if std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref()==Ok("outcome.a7") {Value::Null} else {json!("0x77359400")},"logs":[]});
                }
                let transaction = self
                    .transactions
                    .lock()
                    .unwrap()
                    .get(&original)
                    .unwrap()
                    .clone();
                json!({"hash":original,"from":transaction.sender,"to":transaction.to,
                    "nonce":format!("{:#x}",transaction.nonce),"chainId":format!("{:#x}",self.chain_id),"type":"0x2",
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
        let prior = self.counters.broadcasts.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.multiple || prior == 0,
            "an accepted transaction must never be sent again"
        );
        let raw = hex::decode(
            request["params"][0]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        let transaction = self
            .prepared
            .lock()
            .unwrap()
            .iter()
            .find(|candidate| verify_signed_envelope(candidate, &raw).is_ok())
            .expect("broadcast matches a prepared envelope")
            .clone();
        let original = format!("{:#x}", verify_signed_envelope(&transaction, &raw).unwrap());
        self.counters
            .broadcast_hashes
            .lock()
            .unwrap()
            .push(original.clone());
        // Open a separate SQLite connection at the dispatch boundary. A memory
        // callback cannot satisfy this assertion: the original must be durable.
        let store = Store::open(&self.store_path).unwrap();
        let records = store.list_records().unwrap();
        let record = records
            .iter()
            .find(|record| record["submission"]["reference"] == original)
            .unwrap();
        let id = record["id"].as_str().unwrap();
        assert_eq!(store.submission(id).unwrap().unwrap().reference, original);
        assert_eq!(record["submission"]["reference"], original);
        assert!(
            store
                .latest_stage_attempt(id, "standalone")
                .unwrap()
                .is_some()
        );
        self.transactions
            .lock()
            .unwrap()
            .insert(original.clone(), transaction.clone());
        if prior == 0 {
            *self.hash.lock().unwrap() = Some(original);
        }
        let value = u256(&transaction.value).unwrap();
        let balance = if transaction.sender
            == lower_address(
                LocalEnvelopeSigner::from_private_key(SECOND_KEY)
                    .unwrap()
                    .address(),
            ) {
            &self.second_balance
        } else {
            &self.sender_balance
        };
        *balance.lock().unwrap() -= value + U256::from(42_000_000_000_000u64);
        *self.recipient_balance.lock().unwrap() += value;
    }
}

// Both endpoints observe the same external chain. A accepts the complete POST
// before dropping its response, then rate-limits reads. B serves only reads.
// Reads race, so B's small fixture delay makes A's injected 429 or missing
// receipt observable before B wins, rather than depending on thread scheduling.
struct Endpoint {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    drain: std::sync::mpsc::Sender<std::sync::mpsc::Sender<()>>,
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
        let (drain, drained) = std::sync::mpsc::channel::<std::sync::mpsc::Sender<()>>();
        let captured = requests.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        // Acknowledge only after every queued connection and
                        // delayed response callback has been handled.
                        while let Ok(complete) = drained.try_recv() {
                            let _ = complete.send(());
                        }
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("synthetic RPC accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Some(request) = read_request(&mut stream) else {
                    continue; // GoFin may cancel a losing read before its POST is complete.
                };
                captured.lock().unwrap().push(request.clone());
                if request["method"] == "eth_getBalance"
                    && chain.counters.fail_preparation_rpc.load(Ordering::SeqCst)
                {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    continue;
                }
                if request["method"] == "eth_sendRawTransaction" {
                    assert!(primary, "RPC B must never receive a broadcast");
                    let first = chain.counters.broadcasts.load(Ordering::SeqCst) == 0;
                    chain.accept(&request);
                    if first && chain.stall_first_response.load(Ordering::SeqCst) {
                        let chain = chain.clone();
                        std::thread::spawn(move || {
                            let started = std::time::Instant::now();
                            while !chain.release_first_response.load(Ordering::SeqCst)
                                && started.elapsed() < Duration::from_secs(20)
                            {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            drop(stream);
                        });
                    }
                    continue; // Fully accepted request, no HTTP response.
                }
                if !primary {
                    std::thread::sleep(Duration::from_millis(25));
                }
                if primary && !missing_receipts && chain.hash.lock().unwrap().is_some() {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
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
                    // A losing read can be cancelled after the server receives
                    // its request; a closed response socket is expected then.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            }
        });
        Self {
            url,
            requests,
            drain,
            stopped,
            worker: Some(worker),
        }
    }

    async fn drain(&self) {
        let (complete, completed) = std::sync::mpsc::channel();
        self.drain.send(complete).unwrap();
        tokio::task::spawn_blocking(move || {
            completed
                .recv_timeout(Duration::from_secs(5))
                .expect("synthetic RPC fixture did not drain")
        })
        .await
        .unwrap();
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
fn read_request(stream: &mut TcpStream) -> Option<Value> {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut buffer = [0; 4096];
        let read = read_fixture_bytes(stream, &mut buffer)?;
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
        let read = read_fixture_bytes(stream, &mut buffer)?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    Some(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap())
}

fn read_fixture_bytes(stream: &mut TcpStream, buffer: &mut [u8]) -> Option<usize> {
    match stream.read(buffer) {
        Ok(0) => None,
        Ok(read) => Some(read),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ) =>
        {
            None
        }
        Err(error) => panic!("synthetic RPC request read failed: {error}"),
    }
}

fn broker(path: &Path, urls: Vec<String>, chain: Arc<Chain>) -> Arc<BrokerRuntime> {
    let authorizer = Arc::new(Approval(chain.counters.clone()));
    broker_with_authorizer(path, urls, chain, authorizer)
}
fn broker_with_authorizer(
    path: &Path,
    urls: Vec<String>,
    chain: Arc<Chain>,
    authorizer: Arc<dyn Authorizer>,
) -> Arc<BrokerRuntime> {
    let signer = LocalEnvelopeSigner::from_private_key(TEST_KEY).unwrap();
    let address = lower_address(signer.address());
    let second = LocalEnvelopeSigner::from_private_key(SECOND_KEY).unwrap();
    let second_address = lower_address(second.address());
    let chain_id = chain.chain_id;
    let network = NetworkConfig::reviewed_evm_networks()
        .into_iter()
        .find(|network| network.chain_id == chain_id)
        .unwrap();
    let confirmations = network.confirmations;
    let asset = AssetId {
        network: format!("ethereum:{chain_id}"),
        reference: "native".into(),
        decimals: 18,
    };
    let config = BrokerConfigV2 {
        version: 2,
        networks: vec![network],
        deployments: if chain_id == 1 {
            vec![DeploymentConfig::ethereum_mainnet()]
        } else {
            vec![]
        },
        connections: vec![
            ConnectionConfig::Ethereum {
                name: WALLET.into(),
                resource: ResourceId(format!("ethereum:{chain_id}:{address}")),
                credential_ref: Some("00000000-0000-4000-8000-000000000001".into()),
                network: format!("ethereum:{chain_id}"),
                address: address.clone(),
                signing_enabled: true,
                assets: vec![asset.clone()],
            },
            ConnectionConfig::Ethereum {
                name: "second-wallet".into(),
                resource: ResourceId(format!("ethereum:{chain_id}:{second_address}")),
                credential_ref: Some("00000000-0000-4000-8000-000000000002".into()),
                network: format!("ethereum:{chain_id}"),
                address: second_address.clone(),
                signing_enabled: true,
                assets: vec![asset],
            },
        ],
        policy: BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() },
        extensions: BTreeMap::new(),
    };
    let rpc = HttpRpc::isolated_localhost_pool(urls, chain_id).unwrap();
    let adapter = EthereumAdapter::new(
        EthereumConfig::wallet_only(chain_id, confirmations, vec![]),
        vec![
            WalletConnection {
                name: WALLET.into(),
                address,
                signing_enabled: true,
            },
            WalletConnection {
                name: "second-wallet".into(),
                address: second_address,
                signing_enabled: true,
            },
        ],
        Arc::new(EvmRuntime::new(
            Arc::new(rpc),
            chain_id,
            Some(Arc::new(Signer {
                inner: signer,
                second,
                chain: chain.clone(),
            })),
        )),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Prices(chain.price_unavailable)));
    let clock = Arc::new(FixtureClock(chain.counters.clone()));
    compose_runtime_with_clock(
        config,
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(adapter)],
            authorizer,
            valuator: Arc::new(Prices(chain.price_unavailable)),
            bridge: None,
        },
        clock,
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
        Self::new_with_reporting_barrier(runtime, directory, None).await
    }
    async fn new_with_reporting_barrier(
        runtime: Arc<BrokerRuntime>,
        directory: &Path,
        barrier: Option<Arc<Counters>>,
    ) -> Self {
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
                let broker = broker.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
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
                    let cancellable = input["cancel_on_disconnect"].as_bool().unwrap_or(false)
                        && request.may_use_rpc_reads()
                        && broker.request_uses_rpc_reads(&request);
                    let financial = input["cancel_on_disconnect"].as_bool().unwrap_or(false)
                        && matches!(
                            request,
                            BrokerRequest::Execute { .. }
                                | BrokerRequest::ContinueOperation { .. }
                                | BrokerRequest::ReauthorizeOperation { .. }
                        );
                    let response = if financial {
                        gofin_protocol::progress::observe_financial_request(
                            reader.get_mut(),
                            input["progress"].as_bool().unwrap_or(false),
                            false,
                            Box::pin(broker.handle_client_request(request, false)),
                        )
                        .await
                    } else {
                        let Some(response) = gofin_protocol::progress::observe_request(
                            reader.get_mut(),
                            input["progress"].as_bool().unwrap_or(false),
                            cancellable,
                            Box::pin(broker.handle_client_request(request, cancellable)),
                        )
                        .await
                        else {
                            return;
                        };
                        response
                    };
                    if let Some(barrier) = barrier.as_ref()
                        && barrier.pause_reporting.load(Ordering::SeqCst)
                        && response
                            .result
                            .as_ref()
                            .is_some_and(|r| r["status"] == "completed")
                    {
                        barrier.reporting_started.store(true, Ordering::SeqCst);
                        while !barrier.release_reporting.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    let _ = reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await;
                });
            }
        });
        Self {
            runtime,
            socket,
            server: Some(server),
        }
    }
    async fn call(&self, request: BrokerRequest) -> Value {
        let Ok(binary) = std::env::var("GOFIN_RPC_RECOVERY_CLI") else {
            // Latest runtime composition has large financial futures. Keep
            // the fixture's direct-call path on the heap, as the IPC path does.
            let invocation =
                gofin_domain::execution_progress::Invocation::new(Duration::from_secs(300));
            let response = invocation
                .scope(Box::pin(self.runtime.handle(request)))
                .await;
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
            BrokerRequest::ContinueOperation { id, token } => vec![
                "operations".into(),
                "continue".into(),
                id,
                "--token".into(),
                token,
                "--json".into(),
            ],
            BrokerRequest::Doctor => vec!["doctor".into(), "--json".into()],
            BrokerRequest::OpenOperations => {
                vec!["operations".into(), "--open".into(), "--json".into()]
            }
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
                .env("GOFIN_DEV_IDENTITY", "synthetic-rpc-recovery")
                .env("GOFIN_DEV_BUILD_ID", "synthetic-rpc-recovery-build")
                .env("GOFIN_DEV_SOCKET", socket)
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

async fn restart_recovery(chain_id: u64, missing_receipts: bool, corrupt_probe: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.sqlite");
    if corrupt_probe {
        let mut store = Store::open(&path).unwrap();
        store
            .create_request(
                "corrupt-history",
                "corrupt-request",
                "corrupt-fingerprint",
                "wallet.transfer",
                None,
                "unknown",
                &json!({"id":"corrupt-history"}),
                &now(),
            )
            .unwrap();
        store
            .create_request(
                "invalid-shape",
                "invalid-shape-request",
                "invalid-shape-fingerprint",
                "wallet.transfer",
                None,
                "unknown",
                &json!({"id":"invalid-shape"}),
                &now(),
            )
            .unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE operations SET record_json='{' WHERE id='corrupt-history'",
            [],
        )
        .unwrap();
    }
    let initial_balance = U256::from(100_000_000_000_000_000u64);
    let counters = Arc::new(Counters::default());
    let chain = Arc::new(Chain {
        chain_id,
        store_path: path.clone(),
        counters: counters.clone(),
        prepared: Mutex::new(Vec::new()),
        stall_first_response: AtomicBool::new(false),
        release_first_response: AtomicBool::new(false),
        transactions: Mutex::new(BTreeMap::new()),
        hash: Mutex::new(None),
        multiple: corrupt_probe,
        price_unavailable: corrupt_probe
            && std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref() == Ok("outcome.a1"),
        receipt_ready: AtomicBool::new(false),
        sender_balance: Mutex::new(initial_balance),
        second_balance: Mutex::new(initial_balance),
        recipient_balance: Mutex::new(U256::ZERO),
    });
    let primary = Endpoint::with_visibility(chain.clone(), true, missing_receipts);
    let backup = Endpoint::new(chain.clone(), false);
    let urls = vec![primary.url.clone(), backup.url.clone()];
    let client = Client::new(broker(&path, urls.clone(), chain.clone()), directory.path()).await;
    if corrupt_probe {
        let unreadable = client
            .call(BrokerRequest::Status {
                id: "corrupt-history".into(),
            })
            .await;
        assert_eq!(unreadable["error"]["code"], "history_unreadable");
        assert_eq!(unreadable["outcome"]["dispatch"], "uncertain");
        assert_eq!(unreadable["outcome"]["replay_safe"], false);
        let invalid = client
            .call(BrokerRequest::Status {
                id: "invalid-shape".into(),
            })
            .await;
        assert_eq!(invalid["error"]["code"], "history_unreadable");
        assert_eq!(invalid["outcome"]["replay_safe"], false);
        let conn = rusqlite::Connection::open(&path).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT record_json FROM operations WHERE id='corrupt-history'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, "{", "observation must not overwrite corrupt history");
    }
    let request = OperationRequest {
        request_id: REQUEST_ID.into(),
        operation: "wallet.transfer".into(),
        input: json!({"request_id":REQUEST_ID,"wallet":WALLET,"network":"ethereum","chain_id":chain_id,"token":"native",
            "amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),
        run_id: None,
    };
    let initial_pending = client
        .call(BrokerRequest::Execute {
            request: request.clone(),
        })
        .await;
    // The bounded immediate reconciliation has already queried the accepted
    // original hash. Missing receipts mean Pending, not terminal settlement;
    // keep that absence and every financial hold intact across reconstruction.
    assert_eq!(initial_pending["status"], "pending", "{initial_pending}");
    assert_eq!(initial_pending["outcome"]["phase"], "in_progress");
    if std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref() == Ok("outcome.a1") {
        assert!(
            initial_pending["preview"]["valuation"].is_null(),
            "missing price must stay unavailable"
        );
    }
    if std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref() == Ok("outcome.a3") {
        assert!(
            initial_pending["outcome"]["next_check_after"].is_string(),
            "pending outcome needs observation cadence"
        );
    }
    assert_eq!(initial_pending["outcome"]["replay_safe"], false);
    let main_counts = counters.snapshot();
    let mut admission_probes = Vec::new();
    if corrupt_probe {
        for (probe_id, wallet) in [
            ("same-account-probe", WALLET),
            ("different-account-probe", "second-wallet"),
        ] {
            let probe = OperationRequest {
                request_id: probe_id.into(),
                operation: "wallet.transfer".into(),
                input: json!({"request_id":probe_id,"wallet":wallet,"network":"ethereum","chain_id":chain_id,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),
                run_id: None,
            };
            let before = counters.snapshot();
            let observed = client.call(BrokerRequest::Execute { request: probe }).await;
            admission_probes.push(outcome_evidence::admission_probe(
                if wallet == WALLET {
                    "same_account"
                } else {
                    "different_account"
                },
                before,
                counters.snapshot(),
                &observed,
            ));
            assert!(
                !observed["submission"].is_null(),
                "unrelated action must be admitted while the original is pending: {observed}"
            );
            assert_ne!(observed["id"], initial_pending["id"]);
        }
    }
    assert!(initial_pending["effects"].is_null());
    assert!(!chain.receipt_ready.load(Ordering::SeqCst));
    let id = initial_pending["id"].as_str().unwrap().to_owned();
    let hash = chain.hash.lock().unwrap().clone().unwrap();
    assert_eq!(initial_pending["submission"]["reference"], hash);
    assert_eq!(
        initial_pending["last_reconciliation_evidence"]["original_hash"],
        hash
    );
    assert_eq!(
        initial_pending["last_reconciliation_evidence"]["canonical_confirmed"],
        false
    );
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.get_record(&id).unwrap().unwrap()["status"], "pending");
    let submission = saved.submission(&id).unwrap().unwrap();
    drop(saved);
    let old_runtime = Arc::downgrade(&client.runtime);
    let primary_reads = primary.requests.lock().unwrap().len();
    client.stop().await; // Drop broker, engine, adapter, signer and HTTP caches.
    assert!(old_runtime.upgrade().is_none());
    backup.drain().await;
    let backup_reads_before_restart = backup.requests.lock().unwrap().len();

    let restart_directory = directory.path().join("restart");
    std::fs::create_dir(&restart_directory).unwrap();
    let restarted = Client::new(broker(&path, urls, chain.clone()), &restart_directory).await;
    restarted.runtime.engine.recover_interrupted().unwrap();
    if corrupt_probe {
        let unreadable = restarted
            .call(BrokerRequest::Status {
                id: "corrupt-history".into(),
            })
            .await;
        assert_eq!(unreadable["outcome"]["replay_safe"], false);
        let conn = rusqlite::Connection::open(&path).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT record_json FROM operations WHERE id='corrupt-history'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            raw, "{",
            "restart and status must preserve unreadable bytes"
        );
    }
    let saved = Store::open(&path).unwrap();
    assert_eq!(saved.get_record(&id).unwrap().unwrap()["status"], "pending");
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
        reads[backup_reads_before_restart..]
            .iter()
            .any(|request| request["method"] == "eth_getTransactionReceipt"
                && request["params"] == json!([hash])),
        "restarted broker must read the persisted original receipt through RPC B"
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
        counters.approvals.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    assert_eq!(
        counters.signatures.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    assert_eq!(
        counters.broadcasts.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    chain.receipt_ready.store(true, Ordering::SeqCst);
    let completed = restarted
        .call(BrokerRequest::Status { id: id.clone() })
        .await;
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(completed["outcome"]["result"], "executed");
    assert_eq!(completed["id"], id);
    assert_eq!(completed["submission"]["reference"], hash);
    // Cancellation can leave a fully sent losing POST queued in the fixture.
    // Drain callbacks before sampling, so a prior read cannot look like a new
    // doctor probe; drain again afterwards to catch any actual probe.
    primary.drain().await;
    backup.drain().await;
    let before_doctor = (
        primary.requests.lock().unwrap().len(),
        backup.requests.lock().unwrap().len(),
    );
    let doctor = restarted.call(BrokerRequest::Doctor).await;
    primary.drain().await;
    backup.drain().await;
    assert_eq!(
        before_doctor,
        (
            primary.requests.lock().unwrap().len(),
            backup.requests.lock().unwrap().len()
        )
    );
    let health = &doctor["rpc_health"][0];
    assert_eq!(health["chain_id"], chain_id);
    assert!(health["read_timeout_ms"].is_null());
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
    let missing_fee = std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref() == Ok("outcome.a7");
    assert_eq!(effects.len(), if missing_fee { 1 } else { 2 });
    let transfer = effects
        .iter()
        .find(|effect| effect["kind"] == "transfer")
        .unwrap();
    assert_eq!(transfer["to"], format!("ethereum:{chain_id}:{RECIPIENT}"));
    assert_eq!(transfer["debit"]["atoms"], "1000000000000000");
    assert_eq!(transfer["credit"]["atoms"], "1000000000000000");
    let cost = effects.iter().find(|effect| effect["kind"] == "cost");
    if missing_fee {
        assert!(cost.is_none());
        assert_eq!(completed["outcome"]["reporting"], "partial");
        assert_eq!(completed["outcome"]["phase"], "done");
    } else {
        let cost = cost.unwrap();
        assert_eq!(cost["category"], "ethereum_gas");
        assert_eq!(cost["amount"]["atoms"], "42000000000000");
    }
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
    let duplicate = restarted.call(BrokerRequest::Execute { request }).await;
    assert_eq!(duplicate["id"], completed["id"]);
    let repeated = restarted.call(BrokerRequest::Status { id }).await;
    assert_eq!(repeated["outcome"]["result"], "executed");
    assert_eq!(
        counters.approvals.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    assert_eq!(
        counters.signatures.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    assert_eq!(
        counters.broadcasts.load(Ordering::SeqCst),
        if corrupt_probe { 3 } else { 1 }
    );
    assert_eq!(
        *chain.recipient_balance.lock().unwrap(),
        U256::from(if corrupt_probe {
            3_000_000_000_000_000u64
        } else {
            1_000_000_000_000_000u64
        })
    );
    assert_eq!(
        *chain.sender_balance.lock().unwrap(),
        initial_balance
            - U256::from(if corrupt_probe {
                2_084_000_000_000_000u64
            } else {
                1_042_000_000_000_000u64
            })
    );
    assert_eq!(
        Store::open(&path).unwrap().list_records().unwrap().len(),
        if corrupt_probe { 5 } else { 1 }
    );
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let requested = std::env::var("GOFIN_OUTCOME_SCENARIO").unwrap_or_default();
        let scenario = if corrupt_probe
            && matches!(
                requested.as_str(),
                "outcome.a1" | "outcome.a2" | "outcome.a3" | "outcome.a7"
            ) {
            requested.as_str()
        } else if corrupt_probe {
            "outcome.a5"
        } else {
            "outcome.identity-recovery"
        };
        let mut evidence = json!({"scenario":scenario,
            "approval_count":main_counts.0, "external_submission_count":main_counts.1,
            "intervention_count":0,"remaining_external_uncertainty":corrupt_probe,
            "blocked_unrelated_activity":Value::Null});
        if corrupt_probe {
            outcome_evidence::attach_probes(&mut evidence, &admission_probes);
        }
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
    restarted.stop().await;
    for (endpoint, expected) in [(&primary, if corrupt_probe { 3 } else { 1 }), (&backup, 0)] {
        if chain_id == 4663 {
            assert!(
                endpoint.requests.lock().unwrap().iter().all(|request| {
                    request["method"] != "eth_getBlockByNumber"
                        || request["params"][0] != "finalized"
                }),
                "Robinhood recovery retains sequencer-inclusion settlement"
            );
        }
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
    Box::pin(restart_recovery(1, false, false)).await;
}

#[tokio::test]
async fn corrupt_history_remains_incomplete_while_unrelated_transfer_executes() {
    Box::pin(restart_recovery(1, true, true)).await;
}

#[tokio::test]
async fn preapproval_rpc_failure_continues_original_identity_once() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap(); let path=directory.path().join("journal.sqlite");
        let counters=Arc::new(Counters::default()); counters.fail_preparation_rpc.store(true,Ordering::SeqCst);
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,false);
        let backup=Endpoint::with_visibility(chain.clone(),false,false);
        let client=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),directory.path()).await;
        let request=OperationRequest {request_id:"predispatch-retry".into(),operation:"wallet.transfer".into(),input:json!({"request_id":"predispatch-retry","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let original=client.call(BrokerRequest::Execute{request:request.clone()}).await;
        assert_eq!(original["outcome"]["dispatch"],"not_dispatched","{original}");
        let id=original["id"].as_str().unwrap().to_owned();
        let token=original.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap().to_owned();
        assert_eq!(counters.approvals.load(Ordering::SeqCst),0);
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        counters.fail_preparation_rpc.store(false,Ordering::SeqCst);
        let repeated=client.call(BrokerRequest::Execute{request:request.clone()}).await;
        assert_eq!(repeated["id"],id); assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        let continued=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token:token.clone()}).await;
        assert_eq!(continued["id"],id);assert!(continued["submission"].is_object(),"{continued}");
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        let duplicate=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token}).await;
        assert_eq!(duplicate["id"],id);assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        chain.receipt_ready.store(true,Ordering::SeqCst);
        let settled=client.call(BrokerRequest::Status{id}).await;assert_eq!(settled["outcome"]["result"],"executed","{settled}");
        client.stop().await;
    }).await
}

#[tokio::test]
async fn interrupted_preapproval_token_yields_one_new_token_after_restart() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap(); let path=directory.path().join("journal.sqlite");
        let counters=Arc::new(Counters::default()); counters.fail_preparation_rpc.store(true,Ordering::SeqCst);
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,false);let backup=Endpoint::with_visibility(chain.clone(),false,false);
        let client=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),directory.path()).await;
        let request=OperationRequest{request_id:"interrupted-preapproval".into(),operation:"wallet.transfer".into(),input:json!({"request_id":"interrupted-preapproval","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let original=client.call(BrokerRequest::Execute{request:request.clone()}).await;
        let id=original["id"].as_str().unwrap().to_owned();let token=original.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap().to_owned();
        client.stop().await;
        std::fs::remove_file(directory.path().join("cli.sock")).unwrap();
        {let mut store=Store::open(&path).unwrap(); let record=store.get_typed_record(&id).unwrap().unwrap(); let lease=store.try_acquire_action_lease(&id).unwrap();assert!(store.claim_preapproval_continuation(&lease,&token,record["fingerprint"].as_str().unwrap()).unwrap().1);}
        counters.fail_preparation_rpc.store(false,Ordering::SeqCst);
        let client=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),directory.path()).await;
        let interrupted=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token:token.clone()}).await;
        let next=interrupted.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap().to_owned();assert_ne!(next,token);
        let repeated=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token:token.clone()}).await;
        assert_eq!(repeated.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str),Some(next.as_str()));
        assert_eq!(counters.approvals.load(Ordering::SeqCst),0);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        let result=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token:next}).await;
        assert_eq!(result["id"],id);assert!(result["submission"].is_object(),"{result}");
        let replay=client.call(BrokerRequest::ContinueOperation{id:id.clone(),token}).await;
        assert_eq!(replay,interrupted);
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        client.stop().await;
    }).await
}

#[tokio::test]
async fn cancelled_approval_dispatches_nothing_and_status_never_prompts() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");
        let counters=Arc::new(Counters::default());counters.reject_approval.store(true,Ordering::SeqCst);
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);
        let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain),directory.path()).await;
        let input=json!({"request_id":"cancelled-review","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
        let parsed=client.call(BrokerRequest::Execute { request: OperationRequest { request_id:"cancelled-review".into(),operation:"wallet.transfer".into(),input,run_id:None } }).await;
        assert_eq!(parsed["status"],"cancelled");
        assert_eq!(parsed["error"]["code"],"AUTHORIZATION_CANCELLED");
        assert!(parsed["submission"].is_null());
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        let store=Store::open(&path).unwrap();let row=store.by_request("cancelled-review").unwrap().unwrap();
        assert!(store.submission(&row.operation_id).unwrap().is_none());
        let before=store.get_record(&row.operation_id).unwrap().unwrap();drop(store);
        let status=client.call(BrokerRequest::Status{id:row.operation_id.clone()}).await;
        assert!(status["submission"].is_null());
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        assert_eq!(Store::open(&path).unwrap().get_record(&row.operation_id).unwrap().unwrap(),before);
        counters.reject_approval.store(false,Ordering::SeqCst);
        let main_counts=counters.snapshot();
        let mut admission_probes=Vec::new();
        for (id,wallet) in [("a26-same-account",WALLET),("a26-different-account","second-wallet")] {
            let probe=OperationRequest {request_id:id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
            let before=counters.snapshot();
            let admitted=client.call(BrokerRequest::Execute{request:probe}).await;
            admission_probes.push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"}, before, counters.snapshot(), &admitted));
        }
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),2);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a26","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        client.stop().await;
    }).await
}

#[tokio::test]
async fn concurrent_requests_preserve_one_identity_and_distinct_evm_nonces() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");
        let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(true),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let primary=Endpoint::with_visibility(chain.clone(),true,true);let backup=Endpoint::new(chain.clone(),false);
        let client=Arc::new(Client::new(broker(&path,vec![primary.url.clone(),backup.url.clone()],chain.clone()),directory.path()).await);
        let request=|id:&str,wallet:&str| OperationRequest {request_id:id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let first_request=request("race-original",WALLET);
        let first={let client=client.clone();let request=first_request.clone();tokio::spawn(async move {client.call(BrokerRequest::Execute{request}).await})};
        let accepted=tokio::time::timeout(Duration::from_secs(10),async {while counters.broadcasts.load(Ordering::SeqCst)<1 {tokio::time::sleep(Duration::from_millis(10)).await}}).await;
        assert!(accepted.is_ok(),"first submission was not accepted");
        let duplicate={let client=client.clone();let request=first_request;tokio::spawn(async move {client.call(BrokerRequest::Execute{request}).await})};
        let same={let client=client.clone();let request=request("race-same",WALLET);tokio::spawn(async move {client.call(BrokerRequest::Execute{request}).await})};
        let different={let client=client.clone();let request=request("race-other","second-wallet");tokio::spawn(async move {client.call(BrokerRequest::Execute{request}).await})};
        let independent=tokio::time::timeout(Duration::from_secs(10),async {while counters.broadcasts.load(Ordering::SeqCst)<3 {tokio::time::sleep(Duration::from_millis(10)).await}}).await;
        chain.release_first_response.store(true,Ordering::SeqCst);
        assert!(independent.is_ok(),"unrelated submissions were blocked while the first accepted response was stalled");
        let (first,duplicate,same,different)=tokio::join!(first,duplicate,same,different);
        let (first,duplicate,same,different)=(first.unwrap(),duplicate.unwrap(),same.unwrap(),different.unwrap());
        assert_eq!(first["id"],duplicate["id"]);assert_ne!(first["id"],same["id"]);assert_ne!(first["id"],different["id"]);
        {let txs=chain.transactions.lock().unwrap();let mut first_wallet_nonces=txs.values().filter(|tx|tx.sender==lower_address(LocalEnvelopeSigner::from_private_key(TEST_KEY).unwrap().address())).map(|tx|tx.nonce).collect::<Vec<_>>();first_wallet_nonces.sort();assert_eq!(first_wallet_nonces,vec![0,1]);}
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);
        let main_counts=counters.for_record(&first);
        let admission_probes=vec![
            outcome_evidence::admission_probe("same_account",(0,0),counters.for_record(&same),&same),
            outcome_evidence::admission_probe("different_account",(0,0),counters.for_record(&different),&different)];
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a6","approval_count":main_counts.0,"external_submission_count":main_counts.1,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        Arc::try_unwrap(client).ok().unwrap().stop().await;
    }).await
}

#[tokio::test]
async fn human_dismissal_hides_unconfirmed_without_changing_financial_facts() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain),directory.path()).await;
        let request=|id:&str,wallet:&str| OperationRequest {request_id:id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let original=client.call(BrokerRequest::Execute{request:request("dismiss-original",WALLET)}).await;
        let id=original["id"].as_str().unwrap().to_owned();let submission=original["submission"].clone();
        counters.clock_offset_seconds.store(7 * 24 * 60 * 60, Ordering::SeqCst);
        let open=client.call(BrokerRequest::OpenOperations).await;
        assert!(open.as_array().unwrap().iter().any(|r|r["id"]==id && r["outcome"]["phase"]=="unconfirmed"));
        let replay=client.call(BrokerRequest::Execute{request:request("dismiss-original",WALLET)}).await;
        assert_eq!(replay["id"],id);assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);

        // This direct call stands in for the already-authorized human-only
        // enrollment boundary; production CLI still requires fresh human sudo.
        let dismissed=client.runtime.engine.dismiss_operation(&id).unwrap();
        assert_eq!(dismissed["human_dismissed"],true);assert_eq!(dismissed["outcome"]["replay_safe"],false);
        let after=client.call(BrokerRequest::OpenOperations).await;
        assert!(!after.as_array().unwrap().iter().any(|r|r["id"]==id));
        let observed=client.call(BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(observed["human_dismissed"],true);assert_eq!(observed["outcome"]["replay_safe"],false);assert_eq!(observed["submission"],submission);
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        counters.clock_offset_seconds.store(0, Ordering::SeqCst);
        let main_counts = counters.snapshot();
        let mut admission_probes = Vec::new();
        for (probe_id,wallet) in [("dismiss-same",WALLET),("dismiss-other","second-wallet")] {let before=counters.snapshot();let probe=client.call(BrokerRequest::Execute{request:request(probe_id,wallet)}).await;assert!(!probe["submission"].is_null());admission_probes.push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"}, before, counters.snapshot(), &probe));}
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let scenario=if std::env::var("GOFIN_OUTCOME_SCENARIO").as_deref()==Ok("outcome.a4") {"outcome.a4"} else {"outcome.a23"};let mut evidence=json!({"scenario":scenario,"approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"dismissal_evidence":"synthetic authorized engine boundary; physical human path not observed"});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        client.stop().await;
    }).await
}

#[tokio::test]
async fn similar_unconfirmed_operation_is_shown_to_human_without_blocking_approval() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain),directory.path()).await;
        let request=|id:&str,wallet:&str| OperationRequest {request_id:id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let original=client.call(BrokerRequest::Execute{request:request("advisory-original",WALLET)}).await;
        let id=original["id"].as_str().unwrap();
        assert_eq!(original["outcome"]["phase"], "in_progress");
        assert_eq!(original["outcome"]["replay_safe"], false);
        counters.require_review.store(true,Ordering::SeqCst);
        let main_counts=counters.snapshot();
        let mut admission_probes=Vec::new();
        let before=counters.snapshot();
        let same=client.call(BrokerRequest::Execute{request:request("advisory-same",WALLET)}).await;
        assert!(!same["submission"].is_null());
        admission_probes.push(outcome_evidence::admission_probe("same_account", before, counters.snapshot(), &same));
        {let reviews=counters.reviews.lock().unwrap();let advisory=reviews[0]["advisories"].as_array().unwrap();let warning=advisory.iter().find(|a|a["operation_id"]==id).unwrap();assert_eq!(warning["operation"],"wallet.transfer");assert_eq!(warning["amounts"][0]["amount"]["atoms"],"1000000000000000");}
        let before=counters.snapshot();
        let other=client.call(BrokerRequest::Execute{request:request("advisory-other","second-wallet")}).await;assert!(!other["submission"].is_null());
        admission_probes.push(outcome_evidence::admission_probe("different_account", before, counters.snapshot(), &other));
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a22","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":1,"probe_submission_count":1,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"rendering_evidence":"structured synthetic approval review; native rendering verified separately"});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        client.stop().await;
    }).await
}

#[tokio::test]
async fn legacy_hyperliquid_favorable_fill_is_observed_without_new_trade() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"),"/../gofin-store/tests/fixtures/schema25-hyperliquid-sell.sqlite"),&path).unwrap();
        let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain),directory.path()).await;
        let saved=Store::open(&path).unwrap().list_typed_records().unwrap().into_iter().find(|r|r["request_id"]=="directional-close").unwrap();
        let id=saved["id"].as_str().unwrap().to_owned();
        let observed=client.call(BrokerRequest::Status{id:id.clone()}).await;
        assert_eq!(observed["status"],"completed");assert_eq!(observed["outcome"]["result"],"executed");
        assert_eq!(observed["submission"],saved["submission"]);assert_eq!(observed["approval"],saved["approval"]);
        assert_eq!(observed["preview"]["binding"]["orders"][0]["input"]["size"],"0.95");
        assert_eq!(observed["preview"]["binding"]["orders"][0]["min_price"],"90.95");
        assert!(observed["effects"].to_string().contains("95.75"));
        assert_eq!(counters.approvals.load(Ordering::SeqCst),0);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        let main_counts = counters.snapshot();
        let mut admission_probes = Vec::new();
        for (probe_id,wallet) in [("legacy-same",WALLET),("legacy-other","second-wallet")] {let before=counters.snapshot();let probe=client.call(BrokerRequest::Execute{request:OperationRequest {request_id:probe_id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":probe_id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None}}).await;assert!(!probe["submission"].is_null());admission_probes.push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"}, before, counters.snapshot(), &probe));}
        assert_eq!(counters.approvals.load(Ordering::SeqCst),2);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),2);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a25","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":false,"blocked_unrelated_activity":false});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        client.stop().await;
    }).await
}

#[tokio::test]
async fn legacy_journals_remain_observable_and_do_not_block_new_writes() {
    Box::pin(async {
        let fixtures=["schema25.sqlite","schema25-attestations.sqlite","schema25-grants.sqlite","schema25-partial-bridge.sqlite","schema25-bridge-stages.sqlite","schema25-over-limit.sqlite","schema25-cost-attention.sqlite","schema25-reservation-released.sqlite","schema25-hyperliquid-sell.sqlite"];
        let mut admission_probes=Vec::new();
        let mut main_counts=(0,0);
        for name in fixtures {
            let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");
            std::fs::copy(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gofin-store/tests/fixtures").join(name),&path).unwrap();
            let counters=Arc::new(Counters::default());
            let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
            let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain),directory.path()).await;
            let records=Store::open(&path).unwrap().list_typed_records().unwrap();
            for saved in &records {let id=saved["id"].as_str().unwrap();let observed=client.call(BrokerRequest::Status{id:id.into()}).await;assert_eq!(observed["id"],id,"{name}");assert_eq!(observed["submission"],saved["submission"],"{name}");assert_eq!(observed["approval"],saved["approval"],"{name}");assert_eq!(observed["outcome"]["version"],1);assert_eq!(observed["outcome"]["replay_safe"],false,"legacy history is not nonexecution proof: {name}: {observed}");assert!(observed["outcome"]["phase"].is_string());}
            // Retirement is an interface-level rejection; this does not prove that any specific stored grant was loaded.
            let retired=OperationRequest {request_id:"retired-attempt".into(),operation:"wallet.transfer".into(),input:json!({}),run_id:None};
            let response=Box::pin(client.runtime.handle(BrokerRequest::ExecutePlanStep{plan_id:"synthetic-plan".into(),step_id:"step".into(),request:retired.clone()})).await;
            assert!(!response.ok);assert_eq!(response.error.unwrap().code,"EXECUTION_INTERFACE_RETIRED");
            let response=Box::pin(client.runtime.handle(BrokerRequest::ExecuteBoundary{authorization_id:"synthetic-boundary".into(),request:retired})).await;
            assert!(!response.ok);assert_eq!(response.error.unwrap().code,"EXECUTION_INTERFACE_RETIRED");
            assert_eq!(counters.approvals.load(Ordering::SeqCst),0);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
            let measured=counters.snapshot();main_counts.0+=measured.0;main_counts.1+=measured.1;
            for (suffix,wallet) in [("same",WALLET),("other","second-wallet")] {let id=format!("legacy-{suffix}");let before=counters.snapshot();let result=client.call(BrokerRequest::Execute{request:OperationRequest {request_id:id.clone(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None}}).await;admission_probes.push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"},before,counters.snapshot(),&result));}
            assert_eq!(counters.approvals.load(Ordering::SeqCst),2);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),2);
            client.stop().await;
        }
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a24","approval_count":main_counts.0,"external_submission_count":main_counts.1,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
    }).await
}

struct PausedYolo {
    inner: gofin_broker::yolo_runtime::YoloAuthorizer,
    calls: AtomicUsize,
    second_ready: AtomicBool,
    release_second: AtomicBool,
}
#[async_trait]
impl Authorizer for PausedYolo {
    fn requires_financial_review(&self) -> bool {
        self.inner.requires_financial_review()
    }
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let audit = self.inner.authorize(challenge).await?;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            self.second_ready.store(true, Ordering::SeqCst);
            while !self.release_second.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Ok(audit)
    }
    fn acquire_submission_authority(
        &self,
        audit: &ApprovalAudit,
    ) -> Result<Box<dyn gofin_domain::SubmissionAuthorityLease>> {
        self.inner.acquire_submission_authority(audit)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_yolo_preserves_admitted_submission_and_rejects_unadmitted_authority() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let state=directory.path().join("yolo.json");let gate=directory.path().join("yolo.lock");
        let enabled=gofin_broker::yolo_runtime::set_enabled(&state,&gate,true).unwrap();
        let counters=Arc::new(Counters::default());let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(true),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let wrapped=Arc::new(PausedYolo {inner:gofin_broker::yolo_runtime::YoloAuthorizer::development_fixture(Arc::new(Approval(counters.clone())),state.clone(),gate.clone()),calls:AtomicUsize::new(0),second_ready:AtomicBool::new(false),release_second:AtomicBool::new(false)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Arc::new(Client::new(broker_with_authorizer(&path,vec![endpoint.url.clone()],chain.clone(),wrapped.clone()),directory.path()).await);
        let request=|id:&str,wallet:&str| OperationRequest {request_id:id.into(),operation:"wallet.transfer".into(),input:json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let first={let c=client.clone();tokio::spawn(async move {c.call(BrokerRequest::Execute{request:request("yolo-admitted",WALLET)}).await})};
        tokio::time::timeout(Duration::from_secs(10),async {while counters.broadcasts.load(Ordering::SeqCst)<1 {tokio::time::sleep(Duration::from_millis(5)).await}}).await.unwrap();
        let second={let c=client.clone();tokio::spawn(async move {c.call(BrokerRequest::Execute{request:request("yolo-unadmitted","second-wallet")}).await})};
        tokio::time::timeout(Duration::from_secs(10),async {while !wrapped.second_ready.load(Ordering::SeqCst) {tokio::time::sleep(Duration::from_millis(5)).await}}).await.unwrap();
        let disabled=gofin_broker::yolo_runtime::set_enabled(&state,&gate,false).unwrap();assert!(!disabled.enabled);assert_ne!(disabled.revision,enabled.revision);
        wrapped.release_second.store(true,Ordering::SeqCst);chain.release_first_response.store(true,Ordering::SeqCst);
        let admitted=first.await.unwrap();let refused=second.await.unwrap();
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);assert!(!admitted["submission"].is_null());assert!(refused["submission"].is_null(),"{refused}");
        assert_eq!(admitted["approval"]["source"],"yolo");assert_eq!(admitted["approval"]["yolo_provenance"]["generation"],enabled.generation);
        let observed=client.call(BrokerRequest::Status{id:admitted["id"].as_str().unwrap().into()}).await;assert_eq!(observed["submission"],admitted["submission"]);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        let main_counts=counters.snapshot();
        let mut admission_probes=Vec::new();
        for (probe_id,wallet) in [("yolo-probe-same",WALLET),("yolo-probe-other","second-wallet")] {let before=counters.snapshot();let probe=client.call(BrokerRequest::Execute{request:request(probe_id,wallet)}).await;admission_probes.push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"},before,counters.snapshot(),&probe));}
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);assert_eq!(counters.approvals.load(Ordering::SeqCst),2);
        if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a21","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"expected_scenario_action":"disable Yolo while two actions are in flight","remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"yolo_evidence":"development-only protected-gate fixture; physical human activation not observed"});outcome_evidence::attach_probes(&mut evidence,&admission_probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        Arc::try_unwrap(client).ok().unwrap().stop().await;
    }).await
}

static A20_PROBES: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static A20_MAIN_APPROVALS: AtomicUsize = AtomicUsize::new(0);
static A20_MAIN_SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
static A20_PROBE_APPROVALS: AtomicUsize = AtomicUsize::new(0);
static A20_PROBE_SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
struct PausedApproval {
    inner: Approval,
    entered: AtomicBool,
    release: AtomicBool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_preapproval_continue_cancels_shared_invocation_before_dispatch() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");
        let counters=Arc::new(Counters::default());counters.fail_preparation_rpc.store(true,Ordering::SeqCst);
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let paused=Arc::new(PausedApproval{inner:Approval(counters.clone()),entered:AtomicBool::new(false),release:AtomicBool::new(false)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,false);let backup=Endpoint::with_visibility(chain.clone(),false,false);
        let runtime=broker_with_authorizer(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone(),paused.clone());
        let client=Client::new(runtime.clone(),directory.path()).await;
        let request=OperationRequest{request_id:"drop-preapproval".into(),operation:"wallet.transfer".into(),input:json!({"request_id":"drop-preapproval","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"}),run_id:None};
        let original=client.call(BrokerRequest::Execute{request}).await;
        let id=original["id"].as_str().unwrap().to_owned();let token=original.pointer("/outcome/next_action/continuation_token").and_then(Value::as_str).unwrap().to_owned();
        counters.fail_preparation_rpc.store(false,Ordering::SeqCst);
        let invocation=gofin_domain::execution_progress::Invocation::new(Duration::from_secs(30));
        let work={let runtime=runtime.clone();tokio::spawn(invocation.scope(async move {runtime.handle(BrokerRequest::ContinueOperation{id,token}).await}))};
        wait_flag(&paused.entered).await;work.abort();let _=work.await;paused.release.store(true,Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        client.stop().await;
    }).await
}
#[async_trait]
impl Authorizer for PausedApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let audit = self.inner.authorize(challenge).await?;
        self.entered.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(audit)
    }
}
fn disconnect_cli(socket: &Path, id: &str, input: &Value) -> tokio::process::Child {
    tokio::process::Command::new(
        std::env::var("GOFIN_RPC_RECOVERY_CLI").expect("CLI fixture required"),
    )
    .args([
        "--identity",
        "synthetic-rpc-recovery",
        "--build-id",
        "synthetic-rpc-recovery-build",
        "execute",
        "wallet.transfer",
        "--input",
        &input.to_string(),
        "--id",
        id,
        "--json",
    ])
    .env("GOFIN_DEV_IDENTITY", "synthetic-rpc-recovery")
    .env("GOFIN_DEV_BUILD_ID", "synthetic-rpc-recovery-build")
    .env("GOFIN_DEV_SOCKET", socket)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .unwrap()
}
async fn wait_flag(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the GOFIN_RPC_RECOVERY_CLI fixture binary"]
async fn cli_disconnect_before_admission_never_dispatches() {
    Box::pin(async {
      for phase in ["preparation","admission"] {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let counters=Arc::new(Counters::default());
        counters.pause_preparation.store(phase=="preparation",Ordering::SeqCst);
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let paused=Arc::new(PausedApproval {inner:Approval(counters.clone()),entered:AtomicBool::new(false),release:AtomicBool::new(phase!="admission")});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let client=Client::new(broker_with_authorizer(&path,vec![endpoint.url.clone()],chain.clone(),paused.clone()),directory.path()).await;
        let input=json!({"request_id":phase,"wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
        let mut child=disconnect_cli(&client.socket,phase,&input);
        if phase=="preparation" {wait_flag(&counters.preparation_started).await} else {wait_flag(&paused.entered).await}
        child.kill().await.unwrap();let _=child.wait().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        counters.release_preparation.store(true,Ordering::SeqCst);paused.release.store(true,Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0,"{phase}");
        let store=Store::open(&path).unwrap();let saved=store.by_request(phase).unwrap();if let Some(row)=&saved {assert!(store.submission(&row.operation_id).unwrap().is_none())}drop(store);
        client.stop().await;
        let restart_dir=directory.path().join("restart");std::fs::create_dir(&restart_dir).unwrap();
        let client=Client::new(broker(&path,vec![endpoint.url.clone()],chain.clone()),&restart_dir).await;
        let approvals_before=counters.approvals.load(Ordering::SeqCst);if phase=="admission" {assert_eq!(approvals_before,1)}
        A20_MAIN_APPROVALS.fetch_add(approvals_before,Ordering::SeqCst);
        if let Some(row)=saved {let recovered=client.call(BrokerRequest::Status{id:row.operation_id.clone()}).await;assert_eq!(recovered["id"],row.operation_id);assert!(recovered["submission"].is_null());}
        assert_eq!(counters.approvals.load(Ordering::SeqCst),approvals_before);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),0);
        let main_counts=counters.snapshot();
        for (probe_id,wallet) in [("disconnect-probe-same",WALLET),("disconnect-probe-other","second-wallet")] {
            let input=json!({"request_id":probe_id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
            let result=client.call(BrokerRequest::Execute{request:OperationRequest{request_id:probe_id.into(),operation:"wallet.transfer".into(),input,run_id:None}}).await;assert!(!result["submission"].is_null());A20_PROBES.lock().unwrap().push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"},(0,0),counters.for_record(&result),&result));
        }
        assert_eq!(counters.approvals.load(Ordering::SeqCst),approvals_before+2);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),2);
        A20_PROBE_APPROVALS.fetch_add(counters.snapshot().0-main_counts.0,Ordering::SeqCst);A20_PROBE_SUBMISSIONS.fetch_add(counters.snapshot().1-main_counts.1,Ordering::SeqCst);
        client.stop().await;
      }
      if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a20","approval_count":A20_MAIN_APPROVALS.load(Ordering::SeqCst),"external_submission_count":A20_MAIN_SUBMISSIONS.load(Ordering::SeqCst),"probe_approval_count":A20_PROBE_APPROVALS.load(Ordering::SeqCst),"probe_submission_count":A20_PROBE_SUBMISSIONS.load(Ordering::SeqCst),"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"restart_evidence":"preparation, pre-admission, accepted response loss, and confirmed-before-report barriers"});outcome_evidence::attach_probes(&mut evidence,&A20_PROBES.lock().unwrap());std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the GOFIN_RPC_RECOVERY_CLI fixture binary"]
async fn cli_disconnect_after_confirmation_recovers_completed_original() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(false),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let backup=Endpoint::with_visibility(chain.clone(),false,true);let client=Client::new_with_reporting_barrier(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),directory.path(),Some(counters.clone())).await;
        let input=json!({"request_id":"confirmed-before-report","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
        let initial=client.call(BrokerRequest::Execute{request:OperationRequest{request_id:"confirmed-before-report".into(),operation:"wallet.transfer".into(),input,run_id:None}}).await;
        let id=initial["id"].as_str().unwrap().to_owned();let submission=initial["submission"].clone();
        chain.receipt_ready.store(true,Ordering::SeqCst);counters.pause_reporting.store(true,Ordering::SeqCst);
        let mut child=tokio::process::Command::new(std::env::var("GOFIN_RPC_RECOVERY_CLI").expect("CLI fixture required"))
            .args(["--identity","synthetic-rpc-recovery","--build-id","synthetic-rpc-recovery-build","status",&id,"--json"])
            .env("GOFIN_DEV_IDENTITY","synthetic-rpc-recovery").env("GOFIN_DEV_BUILD_ID","synthetic-rpc-recovery-build").env("GOFIN_DEV_SOCKET",&client.socket)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        wait_flag(&counters.reporting_started).await;child.kill().await.unwrap();let _=child.wait().await;counters.release_reporting.store(true,Ordering::SeqCst);
        client.stop().await;
        let restart_dir=directory.path().join("restart");std::fs::create_dir(&restart_dir).unwrap();
        let restarted=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),&restart_dir).await;
        let observed=restarted.call(BrokerRequest::Status{id:id.clone()}).await;assert_eq!(observed["status"],"completed");assert_eq!(observed["submission"],submission);
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        chain.receipt_ready.store(false,Ordering::SeqCst);
        let main_counts=counters.snapshot();
        for (probe_id,wallet) in [("report-probe-same",WALLET),("report-probe-other","second-wallet")] {
            let input=json!({"request_id":probe_id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
            let result=restarted.call(BrokerRequest::Execute{request:OperationRequest{request_id:probe_id.into(),operation:"wallet.transfer".into(),input,run_id:None}}).await;assert!(!result["submission"].is_null());A20_PROBES.lock().unwrap().push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"},(0,0),counters.for_record(&result),&result));
        }
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);
        A20_MAIN_APPROVALS.fetch_add(main_counts.0,Ordering::SeqCst);A20_MAIN_SUBMISSIONS.fetch_add(main_counts.1,Ordering::SeqCst);A20_PROBE_APPROVALS.fetch_add(counters.snapshot().0-main_counts.0,Ordering::SeqCst);A20_PROBE_SUBMISSIONS.fetch_add(counters.snapshot().1-main_counts.1,Ordering::SeqCst);
        restarted.stop().await;
    }).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the GOFIN_RPC_RECOVERY_CLI fixture binary"]
async fn cli_disconnect_after_admission_recovers_original_submission() {
    Box::pin(async {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("journal.sqlite");let counters=Arc::new(Counters::default());
        let chain=Arc::new(Chain {chain_id:1,store_path:path.clone(),counters:counters.clone(),prepared:Mutex::new(Vec::new()),stall_first_response:AtomicBool::new(true),release_first_response:AtomicBool::new(false),transactions:Mutex::new(BTreeMap::new()),hash:Mutex::new(None),multiple:true,price_unavailable:false,receipt_ready:AtomicBool::new(false),sender_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),second_balance:Mutex::new(U256::from(100_000_000_000_000_000u64)),recipient_balance:Mutex::new(U256::ZERO)});
        let endpoint=Endpoint::with_visibility(chain.clone(),true,true);let backup=Endpoint::with_visibility(chain.clone(),false,true);let client=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),directory.path()).await;
        let input=json!({"request_id":"disconnect-admitted","wallet":WALLET,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});
        let mut child=disconnect_cli(&client.socket,"disconnect-admitted",&input);
        tokio::time::timeout(Duration::from_secs(10),async {while counters.broadcasts.load(Ordering::SeqCst)<1 {tokio::time::sleep(Duration::from_millis(5)).await}}).await.unwrap();
        let store=Store::open(&path).unwrap();let row=store.by_request("disconnect-admitted").unwrap().unwrap();let original=store.submission(&row.operation_id).unwrap().unwrap();drop(store);
        child.kill().await.unwrap();let _=child.wait().await;chain.release_first_response.store(true,Ordering::SeqCst);tokio::time::sleep(Duration::from_millis(300)).await;client.stop().await;
        let restart_dir=directory.path().join("restart");std::fs::create_dir(&restart_dir).unwrap();let restarted=Client::new(broker(&path,vec![endpoint.url.clone(),backup.url.clone()],chain.clone()),&restart_dir).await;
        let observed=restarted.call(BrokerRequest::Status{id:row.operation_id.clone()}).await;assert_eq!(observed["id"],row.operation_id);assert_eq!(observed["submission"]["reference"],original.reference);
        assert_eq!(counters.approvals.load(Ordering::SeqCst),1);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),1);
        let main_counts=counters.snapshot();
        for (probe_id,wallet) in [("admitted-probe-same",WALLET),("admitted-probe-other","second-wallet")] {let input=json!({"request_id":probe_id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":RECIPIENT,"max_fee_eth":"0.001"});let result=restarted.call(BrokerRequest::Execute{request:OperationRequest{request_id:probe_id.into(),operation:"wallet.transfer".into(),input,run_id:None}}).await;assert!(!result["submission"].is_null());A20_PROBES.lock().unwrap().push(outcome_evidence::admission_probe(if wallet==WALLET {"same_account"} else {"different_account"},(0,0),counters.for_record(&result),&result));}
        assert_eq!(counters.approvals.load(Ordering::SeqCst),3);assert_eq!(counters.broadcasts.load(Ordering::SeqCst),3);A20_MAIN_APPROVALS.fetch_add(main_counts.0,Ordering::SeqCst);A20_MAIN_SUBMISSIONS.fetch_add(main_counts.1,Ordering::SeqCst);A20_PROBE_APPROVALS.fetch_add(counters.snapshot().0-main_counts.0,Ordering::SeqCst);A20_PROBE_SUBMISSIONS.fetch_add(counters.snapshot().1-main_counts.1,Ordering::SeqCst);restarted.stop().await;
    }).await
}
