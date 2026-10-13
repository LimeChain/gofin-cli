//! Snapshot aggregation over real synthetic HTTP reads, without financial writes.
use super::*;
use fin_evm::EvmRpc;
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

#[tokio::test]
async fn composed_snapshot_keeps_simultaneous_reads_past_both_former_cutoffs() {
    let primary = Endpoint::new("0xaa").await;
    let backup = Endpoint::new("0x2a").await;
    let rpc = Arc::new(
        fin_evm::HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], 1)
            .unwrap(),
    );
    let wallet = fin_ethereum::EthereumAdapter::new(
        fin_ethereum::EthereumConfig::wallet_only(1, 1, vec![]),
        vec![fin_ethereum::WalletConnection {
            name: "synthetic".into(),
            address: WALLET.into(),
            signing_enabled: false,
        }],
        Arc::new(fin_evm::EvmRuntime::new(rpc.clone(), 1, None)),
    )
    .unwrap();
    let router = Arc::new(evm_router::EvmNetworkRouter::new(
        [(1, Arc::new(wallet))].into_iter().collect(),
    ));
    // Use the real broker composition so the SQLite journal is available;
    // the shared fixture authorizer rejects any attempt to request approval.
    let runtime = delayed_runtime(vec![router]);
    let mut snapshot = tokio::spawn(async move {
        runtime
            .handle(BrokerRequest::Read {
                operation: "fin.snapshot".into(),
                input: json!({"networks":["ethereum:1"]}),
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while primary.balance_requests().is_empty() || backup.balance_requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("both providers must receive the snapshot's balance read");

    // Advance the timeout clock while both actual HTTP responses are held.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
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
