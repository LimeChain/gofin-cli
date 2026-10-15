//! Fail-closed loopback guard for the Canonical source fork acceptance fixture.
use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_evm::EvmRpc;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

const FORK_BLOCK: u64 = 26_099_326;
const FORK_HASH: &str = "0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf";
const MAX_LOCAL_SUBMISSIONS: usize = 2;
const MAX_FINALITY_MINE_BLOCKS: usize = 12;

fn blocked() -> GoFinError {
    GoFinError::new(
        "ISOLATED_FORK_REQUIRED",
        "fork",
        "The Canonical test requires the exact owned loopback Ethereum fork.",
        "Run the documented test-fork runner; no live or remote fallback is permitted.",
    )
}

pub struct CanonicalForkRpc {
    endpoint: String,
    instance_id: String,
    client: reqwest::Client,
    ids: AtomicU64,
    submissions: AtomicUsize,
    finality_mines: AtomicUsize,
}
impl CanonicalForkRpc {
    pub async fn new() -> Result<Arc<Self>> {
        let endpoint = std::env::var("GOFIN_FORK_RPC_URL").map_err(|_| blocked())?;
        let instance_id = std::env::var("GOFIN_FORK_INSTANCE_ID").map_err(|_| blocked())?;
        let port = endpoint
            .strip_prefix("http://127.0.0.1:")
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(blocked)?;
        if endpoint != format!("http://127.0.0.1:{port}")
            || !instance_id.starts_with("0x")
            || instance_id.len() != 66
            || !instance_id[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(blocked());
        }
        let rpc = Arc::new(Self {
            endpoint,
            instance_id,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|_| blocked())?,
            ids: AtomicU64::new(1),
            submissions: AtomicUsize::new(0),
            finality_mines: AtomicUsize::new(0),
        });
        rpc.guard().await?;
        Ok(rpc)
    }
    async fn raw(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| blocked())?;
        if !response.status().is_success() {
            return Err(blocked());
        }
        let body: Value = response.json().await.map_err(|_| blocked())?;
        if body["id"].as_u64() != Some(id) || body["jsonrpc"] != "2.0" || !body["error"].is_null() {
            return Err(blocked());
        }
        body.get("result").cloned().ok_or_else(blocked)
    }
    pub async fn guard(&self) -> Result<()> {
        let metadata = self.raw("anvil_metadata", json!([])).await?;
        if metadata["instanceId"].as_str() != Some(&self.instance_id)
            || !metadata["clientVersion"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .starts_with("anvil/")
            || metadata["chainId"].as_u64() != Some(1)
            || metadata["forkedNetwork"]["forkBlockNumber"].as_u64() != Some(FORK_BLOCK)
            || metadata["forkedNetwork"]["forkBlockHash"].as_str() != Some(FORK_HASH)
        {
            return Err(blocked());
        }
        Ok(())
    }

    /// Disposable-chain fixture controls, never financial restoration evidence.
    pub async fn snapshot_fixture(&self) -> Result<String> {
        self.guard().await?;
        self.raw("evm_snapshot", json!([]))
            .await?
            .as_str()
            .map(str::to_owned)
            .ok_or_else(blocked)
    }
    pub async fn restore_fixture(&self, snapshot: &str) -> Result<()> {
        self.guard().await?;
        if !snapshot.starts_with("0x")
            || snapshot.len() > 66
            || !snapshot[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(blocked());
        }
        if self.raw("evm_revert", json!([snapshot])).await? != json!(true) {
            return Err(blocked());
        }
        self.guard().await
    }

    /// Mine only on the exact guarded local fork, and only a small bounded
    /// number of blocks while reconciling an already-submitted operation.
    pub async fn mine_finality_block(&self) -> Result<()> {
        self.guard().await?;
        let mined = self.finality_mines.fetch_add(1, Ordering::SeqCst);
        if mined >= MAX_FINALITY_MINE_BLOCKS {
            return Err(blocked());
        }
        self.raw("evm_mine", json!([])).await?;
        Ok(())
    }
}
#[async_trait]
impl EvmRpc for CanonicalForkRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "eth_chainId"
                | "eth_blockNumber"
                | "eth_getCode"
                | "eth_getBalance"
                | "eth_getTransactionCount"
                | "eth_call"
                | "eth_estimateGas"
                | "eth_maxPriorityFeePerGas"
                | "eth_getBlockByNumber"
                | "eth_getBlockByHash"
                | "eth_getTransactionByHash"
                | "eth_getTransactionReceipt"
                | "eth_sendRawTransaction"
                | "evm_mine"
        ) {
            return Err(blocked());
        }
        if method == "eth_sendRawTransaction" {
            self.guard().await?;
            let previous = self.submissions.fetch_add(1, Ordering::SeqCst);
            if previous >= MAX_LOCAL_SUBMISSIONS {
                return Err(blocked());
            }
        }
        self.raw(method, params).await
    }
}
