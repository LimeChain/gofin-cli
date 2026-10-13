//! Opt-in read-only verification of Robinhood PublicNode through Fin's transport.
//! The controlled 429 gateway forwards allowlisted reads only; no signer exists.
use fin_evm::{EvmRpc, EvmRuntime, HttpRpc, rpc_u64};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const CHAIN: u64 = 4663;
const PRIMARY: &str = "https://rpc.mainnet.chain.robinhood.com";
const BACKUP: &str = "https://robinhood-rpc.publicnode.com";
const USDG: &str = "0x5fc5360d0400a0fd4f2af552add042d716f1d168";
// Public historical transaction, independently compared with the official RPC.
const ORIGINAL: &str = "0x44714b1e13c7e3bb8c5cce6b875b5445dd7b2e2ade3cfbaf765b3f2b7470420c";

fin_evm::alloy::sol! {
    function totalSupply() external view returns (uint256);
}

#[tokio::test]
#[ignore = "opt-in public mainnet reads; requires HTTPS and provider availability"]
async fn robinhood_publicnode_matches_pinned_state_and_original_transaction_evidence() {
    let network = fin_networks::by_chain_id(CHAIN).unwrap();
    assert_eq!(network.rpc_urls, [PRIMARY, BACKUP]);
    let official = Arc::new(HttpRpc::new(vec![PRIMARY.into()], CHAIN).unwrap());
    let publicnode = Arc::new(HttpRpc::new(vec![BACKUP.into()], CHAIN).unwrap());
    let defaults = Arc::new(
        HttpRpc::new(
            network.rpc_urls.iter().map(|url| (*url).into()).collect(),
            CHAIN,
        )
        .unwrap(),
    );
    let head = rpc_u64(
        &official
            .request("eth_blockNumber", json!([]))
            .await
            .unwrap(),
    )
    .unwrap();
    let block = head.checked_sub(16).unwrap();
    let params = json!([format!("0x{block:x}"), false]);
    let header = official
        .request("eth_getBlockByNumber", params.clone())
        .await
        .unwrap();
    let other = publicnode
        .request("eth_getBlockByNumber", params)
        .await
        .unwrap();
    assert_eq!(other["hash"], header["hash"]);
    assert_eq!(other["number"], header["number"]);
    let address = USDG.parse().unwrap();
    let a = EvmRuntime::new(official.clone(), CHAIN, None);
    let b = EvmRuntime::new(publicnode.clone(), CHAIN, None);
    assert!(!b.can_sign());
    a.code_at(address, Some(block)).await.unwrap();
    b.code_at(address, Some(block)).await.unwrap();
    assert_eq!(
        a.balance(address, Some(block)).await.unwrap(),
        b.balance(address, Some(block)).await.unwrap()
    );
    assert_eq!(
        a.call(address, &totalSupplyCall {}, Some(block))
            .await
            .unwrap(),
        b.call(address, &totalSupplyCall {}, Some(block))
            .await
            .unwrap()
    );
    // Latest reads also exercise Fin's endpoint-local freshness validation.
    b.code(address).await.unwrap();
    b.balance(address, None).await.unwrap();
    b.pending_nonce(address).await.unwrap();
    b.fees().await.unwrap();
    let receipt = official
        .request("eth_getTransactionReceipt", json!([ORIGINAL]))
        .await
        .unwrap();
    assert!(!receipt.is_null());
    assert_eq!(receipt["transactionHash"], ORIGINAL);
    assert_eq!(
        publicnode
            .request("eth_getTransactionReceipt", json!([ORIGINAL]))
            .await
            .unwrap(),
        receipt
    );
    let tx = official
        .request("eth_getTransactionByHash", json!([ORIGINAL]))
        .await
        .unwrap();
    let other = publicnode
        .request("eth_getTransactionByHash", json!([ORIGINAL]))
        .await
        .unwrap();
    // Providers may add harmless metadata such as blockTimestamp. Compare the
    // actual envelope and inclusion fields used by Fin, not those extra fields.
    for field in [
        "hash",
        "chainId",
        "from",
        "to",
        "nonce",
        "value",
        "input",
        "gas",
        "type",
        "maxFeePerGas",
        "maxPriorityFeePerGas",
        "accessList",
        "blockHash",
        "blockNumber",
    ] {
        assert_eq!(
            other[field], tx[field],
            "original transaction field: {field}"
        );
    }
    assert_eq!(tx["hash"], ORIGINAL);
    assert_eq!(rpc_u64(&tx["chainId"]).unwrap(), CHAIN);
    let receipt_block = official
        .request(
            "eth_getBlockByNumber",
            json!([receipt["blockNumber"], false]),
        )
        .await
        .unwrap();
    assert_eq!(receipt_block["hash"], receipt["blockHash"]);
    assert_eq!(
        publicnode
            .request(
                "eth_getBlockByNumber",
                json!([receipt["blockNumber"], false])
            )
            .await
            .unwrap()["hash"],
        receipt_block["hash"]
    );
    assert_eq!(publicnode.diagnostics()["read_timeout_ms"], 10000);
    // Default composition uses the same ordered pool, not a separate sidecar.
    let default_runtime = EvmRuntime::new(defaults, CHAIN, None);
    default_runtime.assert_chain().await.unwrap();
    default_runtime.code_at(address, Some(block)).await.unwrap();
    assert_eq!(
        default_runtime
            .call(address, &totalSupplyCall {}, Some(block))
            .await
            .unwrap(),
        a.call(address, &totalSupplyCall {}, Some(block))
            .await
            .unwrap()
    );
}

// Loopback gateways are test-only. A always returns 429; B transparently
// forwards these reads to the real PublicNode HTTPS endpoint. This injects an
// outage without requiring a public provider to fail or accepting any writes.
struct Gateway {
    url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Gateway {
    async fn new(forward: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let mut length = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let length = length.unwrap();
                assert!(length < 4096);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                assert!(matches!(
                    request["method"].as_str(),
                    Some("eth_chainId" | "eth_getBlockByNumber" | "eth_getTransactionReceipt")
                ));
                captured.fetch_add(1, Ordering::SeqCst);
                let (status, body, retry) = if forward {
                    let response = client.post(BACKUP).json(&request).send().await.unwrap();
                    let status = response.status().as_u16();
                    (status, response.bytes().await.unwrap().to_vec(), "")
                } else {
                    (429, vec![], "Retry-After: 120\r\n")
                };
                reader.get_mut().write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                reader.get_mut().write_all(&body).await.unwrap();
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
#[ignore = "opt-in real PublicNode reads behind a synthetic 429; never forwards writes"]
async fn robinhood_429_fails_over_to_live_publicnode_and_retains_original_receipt_hash() {
    let primary = Gateway::new(false).await;
    let backup = Gateway::new(true).await;
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()], CHAIN)
            .unwrap();
    let receipt = rpc
        .request("eth_getTransactionReceipt", json!([ORIGINAL]))
        .await
        .unwrap();
    assert_eq!(receipt["transactionHash"], ORIGINAL);
    let health = rpc.diagnostics();
    assert_eq!(health["last_read"]["selected_endpoint"], "rpc-2");
    assert_eq!(health["last_read"]["attempts"][0]["reason"], "rate_limited");
    assert_eq!(health["read_timeout_ms"], 10000);
    assert_eq!(primary.requests.load(Ordering::SeqCst), 1);
    // The next read skips the cooling primary and queries that same hash.
    assert_eq!(
        rpc.request("eth_getTransactionReceipt", json!([ORIGINAL]))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(primary.requests.load(Ordering::SeqCst), 1);
    assert!(backup.requests.load(Ordering::SeqCst) >= 3);
}
