//! Opt-in public mainnet reads through GoFin's production RPC and wallet adapter.
//! No signer, credentials, signing, submission, or financial authorization exists.
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gofin_domain::{AssetId, GoFinError, Result};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{
    HttpRpc, MAINNET_GENESIS_HASH, NETWORK, ReceiptOutcome, RpcReadRequirements, SolanaRpc,
    SolanaRuntime, SolanaWalletAdapter,
    rpc::MAINNET_RPC_URLS,
    token::{TOKEN_PROGRAM_ID, decode_rpc_account_data, parse_mint},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL: &str = "So11111111111111111111111111111111111111112";
static LIVE_CASE_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn live_case() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LIVE_CASE_GATE.lock().await;
    // Public mainnet reports a ten-second cooldown under burst load. Space
    // opt-in scenarios, including across separate Tokio test runtimes.
    tokio::time::sleep(Duration::from_secs(12)).await;
    guard
}

struct NoPrices;
#[async_trait]
impl UsdPriceSource for NoPrices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Err(GoFinError::validation(
            "This read-only verification does not request financial valuation.",
        ))
    }
}

async fn provider_reads(index: usize) {
    let rpc = HttpRpc::new(vec![MAINNET_RPC_URLS[index].into()]).unwrap();
    assert_eq!(
        rpc.request("getGenesisHash", json!([])).await.unwrap(),
        MAINNET_GENESIS_HASH
    );
    let slot = rpc
        .request("getSlot", json!([{"commitment":"finalized"}]))
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    let account = rpc
        .request_checked(
            "getAccountInfo",
            json!([USDC,{"encoding":"base64","commitment":"finalized","minContextSlot":slot}]),
            RpcReadRequirements {
                minimum_context_slot: Some(slot),
                require_non_null: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(account["value"]["owner"], TOKEN_PROGRAM_ID);
    let raw = decode_rpc_account_data(&account["value"]["data"]).unwrap();
    let mint = parse_mint(TOKEN_PROGRAM_ID, &raw, 0).unwrap();
    assert_eq!(mint.decimals, 6);
    assert!(mint.initialized);
    let latest = rpc
        .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
        .await
        .unwrap();
    let hash = latest["value"]["blockhash"].as_str().unwrap();
    assert_eq!(
        rpc.request("isBlockhashValid", json!([hash,{"commitment":"confirmed"}]))
            .await
            .unwrap()["value"],
        true
    );
    let height = rpc
        .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
        .await;
    let health = rpc.diagnostics();
    assert_eq!(health["endpoints"][0]["identity_verified"], true);
    match height {
        Ok(height) => {
            let height = height.as_u64().unwrap();
            assert!(height <= latest["value"]["lastValidBlockHeight"].as_u64().unwrap());
            println!(
                "provider={} finalized_slot={slot} confirmed_height={height} USDC_mint_verified=true height_eligibility=valid health={health}",
                index + 1
            );
        }
        Err(error) if index == 1 && error.code == "SOLANA_RPC_DATA_INVALID" => {
            // This provider was observed returning a slot for getBlockHeight.
            // Its other verified read methods remain useful; height must fail closed.
            assert_eq!(health["last_read"]["outcome"], "error");
            assert_eq!(
                health["endpoints"][0]["last_error_code"],
                "SOLANA_RPC_DATA_INVALID"
            );
            println!(
                "provider={} finalized_slot={slot} USDC_mint_verified=true height_eligibility=rejected_incompatible_evidence health={health}",
                index + 1
            );
        }
        Err(error) => panic!("Provider read did not establish usable height evidence: {error:?}"),
    }
    if index >= 2 {
        verify_public_receipt(rpc).await;
    }
}

#[tokio::test]
#[ignore = "opt-in public mainnet reads; no signer or financial writes"]
async fn official_mainnet_reads() {
    let _case = live_case().await;
    provider_reads(0).await;
}

#[tokio::test]
#[ignore = "opt-in public mainnet reads; no signer or financial writes"]
async fn publicnode_mainnet_read_eligibility() {
    let _case = live_case().await;
    provider_reads(1).await;
}

#[tokio::test]
#[ignore = "opt-in public mainnet reads; no signer or financial writes"]
async fn onfinality_mainnet_reads() {
    let _case = live_case().await;
    provider_reads(2).await;
}

#[tokio::test]
#[ignore = "opt-in public mainnet reads; no signer or financial writes"]
async fn solanatracker_mainnet_reads() {
    let _case = live_case().await;
    provider_reads(3).await;
}

#[tokio::test]
#[ignore = "opt-in public mainnet adapter and receipt reads; no signer or financial writes"]
async fn default_pool_finalized_account_and_original_transaction() {
    let _case = live_case().await;
    let rpc = HttpRpc::mainnet_beta().unwrap();
    assert_eq!(rpc.diagnostics()["endpoints"].as_array().unwrap().len(), 4);
    // A rejected backup height must not poison later healthy observations.
    for _ in 0..2 {
        let latest = rpc
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await
            .unwrap();
        let height = rpc
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await
            .unwrap()
            .as_u64()
            .unwrap();
        assert!(height <= latest["value"]["lastValidBlockHeight"].as_u64().unwrap());
        println!(
            "default_pool_height=passed height={height} health={}",
            rpc.diagnostics()
        );
    }
    let accounts = rpc
        .request_checked(
            "getMultipleAccounts",
            json!([[USDC,WSOL],{"encoding":"base64","commitment":"finalized"}]),
            RpcReadRequirements {
                require_non_null: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for (index, decimals) in [6, 9].into_iter().enumerate() {
        let account = &accounts["value"][index];
        assert_eq!(account["owner"], TOKEN_PROGRAM_ID);
        let raw = decode_rpc_account_data(&account["data"]).unwrap();
        assert_eq!(
            parse_mint(TOKEN_PROGRAM_ID, &raw, 0).unwrap().decimals,
            decimals
        );
    }
    // Public mint address is used as a read-only owner; no private wallet data.
    let wallet = SolanaWalletAdapter::new(
        "public-read-probe",
        USDC,
        None,
        rpc.clone(),
        Arc::new(NoPrices),
    )
    .unwrap();
    assert!(wallet.signer().is_none());
    let balances = wallet.balances().await.unwrap();
    assert_eq!(balances["network"], NETWORK);
    assert_eq!(balances["observed_commitment"], "finalized");
    assert!(balances["native"]["amount_atoms"].as_str().is_some());
    println!(
        "default_pool_wallet_read=passed accounts_slot={} health={}",
        accounts["context"]["slot"],
        rpc.diagnostics()
    );

    verify_public_receipt(rpc).await;
}

async fn verify_public_receipt(rpc: Arc<HttpRpc>) {
    let history = rpc
        .request_checked(
            "getSignaturesForAddress",
            json!([USDC,{"commitment":"finalized","limit":3}]),
            RpcReadRequirements {
                merge_signature_history: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let runtime = SolanaRuntime::new(rpc.clone());
    for candidate in history.as_array().unwrap() {
        let signature = candidate["signature"].as_str().unwrap();
        match runtime
            .observe_finalized_transaction(signature)
            .await
            .unwrap()
        {
            ReceiptOutcome::Finalized { slot, proof, .. } => {
                assert_eq!(proof.signature, signature);
                assert!(!proof.required_signatures.is_empty());
                assert_eq!(proof.required_signatures[0], signature);
                assert_eq!(
                    proof.pre_balances_lamports.len(),
                    proof.post_balances_lamports.len()
                );
                println!(
                    "public_finalized_receipt=passed signature={signature} slot={slot} message_sha256={} required_signatures={} health={}",
                    proof.message_sha256,
                    proof.required_signatures.len(),
                    rpc.diagnostics()
                );
                return;
            }
            ReceiptOutcome::Pending { confirmed, reason } => {
                println!("public_receipt_candidate=pending confirmed={confirmed} reason={reason}")
            }
            ReceiptOutcome::NotDispatched { .. } => {
                panic!("A public chain observation cannot establish local non-dispatch")
            }
            ReceiptOutcome::ExpiredWithoutInclusion { .. } => {
                panic!(
                    "A public chain observation cannot establish a signed submission's full lifetime"
                )
            }
        }
    }
    panic!(
        "No finalized public transaction proof was available in the bounded recent history sample"
    );
}

// Controlled local outage; backup forwards allowlisted reads to a reviewed live provider.
// No financial RPC method can reach the upstream through this gateway.
struct Gateway {
    url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Gateway {
    async fn new(upstream: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if !matches!(reader.read_line(&mut line).await, Ok(n) if n>0) {
                    continue;
                }
                let mut length = None;
                loop {
                    line.clear();
                    if !matches!(reader.read_line(&mut line).await,Ok(n) if n>0) {
                        break;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = length.filter(|length| *length < 4096) else {
                    continue;
                };
                let mut bytes = vec![0; length];
                if reader.read_exact(&mut bytes).await.is_err() {
                    continue;
                }
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                assert!(matches!(
                    request["method"].as_str(),
                    Some("getGenesisHash" | "getSlot" | "getBlockTime" | "getBalance")
                ));
                count.fetch_add(1, Ordering::SeqCst);
                let (status, body, retry) = if let Some(index) = upstream {
                    match client
                        .post(MAINNET_RPC_URLS[index])
                        .json(&request)
                        .send()
                        .await
                    {
                        Ok(response) => {
                            let status = response.status().as_u16();
                            let body = response.bytes().await.unwrap_or_default().to_vec();
                            (status, body, "")
                        }
                        Err(_) => (503, Vec::new(), ""),
                    }
                } else {
                    (429, Vec::new(), "Retry-After: 120\r\n")
                };
                let header = format!(
                    "HTTP/1.1 {status} ReadOnlyProbe\r\nContent-Type: application/json\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if reader.get_mut().write_all(header.as_bytes()).await.is_ok() {
                    let _ = reader.get_mut().write_all(&body).await;
                }
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
#[ignore = "opt-in live backup reads behind a synthetic 429; gateway cannot forward writes"]
async fn injected_rate_limit_fails_over_to_live_publicnode() {
    let _case = live_case().await;
    let primary = Gateway::new(None).await;
    let backup = Gateway::new(Some(1)).await;
    let rpc =
        HttpRpc::isolated_localhost_pool(vec![primary.url.clone(), backup.url.clone()]).unwrap();
    for _ in 0..2 {
        let result = rpc
            .request("getBalance", json!([USDC,{"commitment":"finalized"}]))
            .await
            .unwrap();
        assert!(result["value"].is_u64());
        let health = rpc.diagnostics();
        assert_eq!(health["last_read"]["selected_endpoint"], "rpc-2");
        assert_eq!(health["last_read"]["attempts"][0]["reason"], "rate_limited");
        assert_eq!(health["endpoints"][1]["identity_verified"], true);
        println!(
            "injected_429_live_backup_read=passed slot={} health={health}",
            result["context"]["slot"]
        );
    }
    assert_eq!(primary.requests.load(Ordering::SeqCst), 1);
    assert!(backup.requests.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
#[ignore = "opt-in live fourth-provider reads behind three synthetic 429s; no writes"]
async fn injected_rate_limits_reach_live_fourth_provider() {
    let _case = live_case().await;
    let first = Gateway::new(None).await;
    let second = Gateway::new(None).await;
    let third = Gateway::new(None).await;
    let fourth = Gateway::new(Some(3)).await;
    let rpc = HttpRpc::isolated_localhost_pool(vec![
        first.url.clone(),
        second.url.clone(),
        third.url.clone(),
        fourth.url.clone(),
    ])
    .unwrap();
    for _ in 0..2 {
        let result = rpc
            .request("getBalance", json!([USDC,{"commitment":"finalized"}]))
            .await
            .unwrap();
        assert!(result["value"].is_u64());
        let health = rpc.diagnostics();
        assert_eq!(health["last_read"]["selected_endpoint"], "rpc-4");
        assert_eq!(health["endpoints"][3]["identity_verified"], true);
        println!(
            "three_injected_429s_live_fourth_read=passed slot={} health={health}",
            result["context"]["slot"]
        );
    }
    for unavailable in [&first, &second, &third] {
        assert_eq!(unavailable.requests.load(Ordering::SeqCst), 1);
    }
    assert!(fourth.requests.load(Ordering::SeqCst) >= 4);
}
