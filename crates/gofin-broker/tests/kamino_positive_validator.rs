//! Public CLI -> real broker/engine/adapter -> owned cloned validator.
//! Only synthetic credentials/funding. Run the dedicated Python runner.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::*;
use gofin_kamino::{
    KaminoAdapter,
    common::KaminoApi,
    klend::{KlendAdapter, NativeKlendProtocol},
    lending::LendingProtocol,
    liquidity::LiquidityProtocol,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_protocol::BrokerRequest;
use gofin_solana::{SolanaRpc, SolanaSigner, SolanaWalletAdapter};
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

const CLIENT_IDENTITY: &str = "synthetic-kamino";
include!("support/kamino_client.rs");

#[test]
fn public_cli_fixture_does_not_embed_the_unused_broker_router_future() {
    fn future_size<F: std::future::Future>(_: impl FnOnce() -> F) -> usize {
        std::mem::size_of::<F>()
    }
    let client: Option<Client> = None;
    let size = future_size(|| {
        client
            .as_ref()
            .unwrap()
            .call(vec![], BrokerRequest::Snapshot, true)
    });
    assert!(
        size < 64 * 1024,
        "public CLI call future occupies {size} bytes"
    );
}

struct Rpc {
    url: String,
    client: reqwest::Client,
    sends: AtomicUsize,
    observed_slot: AtomicU64,
}
#[async_trait]
impl SolanaRpc for Rpc {
    async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        if method == "getGenesisHash" {
            return Ok(json!(gofin_solana::MAINNET_GENESIS_HASH));
        }
        if method == "getSignaturesForAddress"
            && let Some(minimum) = params[1]["minContextSlot"].as_u64()
        {
            // Surfpool incorrectly filters historical signatures by minContextSlot.
            // Check its actual finalized context independently, then omit only
            // that option; the returned history is unchanged and the adapter
            // still validates its exact signature, slot, and provenance.
            // Finalized slots can trail processed account contexts by 32 slots.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let context: Value = self.client.post(&self.url).timeout(Duration::from_secs(1))
                    .json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"finalized"}]}))
                    .send().await.map_err(|_| error("Surfpool finalized context unavailable"))?
                    .json().await.map_err(|_| error("Surfpool finalized context invalid"))?;
                if let Some(slot) = context["result"].as_u64().filter(|slot| *slot >= minimum) {
                    eprintln!("Surfpool verified finalized context: {slot}, minimum: {minimum}");
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(error("Surfpool finalized context did not reach minimum"));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            eprintln!("Surfpool history original parameters: {params}");
            params[1].as_object_mut().unwrap().remove("minContextSlot");
            eprintln!("Surfpool history forwarded parameters: {params}");
        }
        if method == "sendTransaction" {
            let current: Value = self.client.post(&self.url).timeout(Duration::from_secs(1)).json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"processed"}]})).send().await.map_err(|_|error("Surfpool slot unavailable"))?.json().await.map_err(|_|error("Surfpool slot invalid"))?;
            let target = current["result"]
                .as_u64()
                .unwrap()
                .max(self.observed_slot.load(Ordering::Relaxed));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let next: Value = self.client.post(&self.url).timeout(Duration::from_secs(1)).json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"processed"}]})).send().await.map_err(|_|error("Surfpool slot unavailable"))?.json().await.map_err(|_|error("Surfpool slot invalid"))?;
                if next["result"].as_u64().unwrap() > target {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(error("Surfpool slot did not advance"));
                }
            }
            self.sends.fetch_add(1, Ordering::SeqCst);
        }
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| error("loopback transport failed"))?
            .json()
            .await
            .map_err(|_| error("loopback JSON invalid"))?;
        if !response["error"].is_null() {
            return Err(error(&format!("{method}: {}", response["error"])));
        }
        if method == "getSignaturesForAddress" {
            eprintln!("Surfpool history raw response: {}", response["result"]);
        }
        if let Some(slot) = response
            .pointer("/result/context/slot")
            .and_then(Value::as_u64)
        {
            self.observed_slot.fetch_max(slot, Ordering::Relaxed);
        }
        if method == "simulateTransaction" && !response["result"]["value"]["err"].is_null() {
            eprintln!("Local simulation failure: {}", response["result"]["value"]);
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| error("loopback result missing"))
    }
}
fn error(message: &str) -> GoFinError {
    GoFinError::new(
        "KLEND_VALIDATOR",
        "test",
        message,
        "Inspect ignored owned-validator evidence.",
    )
}
struct OfflineApi;
#[async_trait]
impl KaminoApi for OfflineApi {
    async fn get(&self, _: &str) -> Result<Value> {
        Err(error("public API disabled"))
    }
}
#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "100"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic_validator_prices".into(),
        })
    }
}
#[derive(Default)]
struct Approval(Mutex<Vec<AuthorizationChallenge>>);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.lock().unwrap().push(c.clone());
        Ok(ApprovalAudit {
            mechanism: "automatic development approval (owned validator fixture)".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn runtime(
    path: &Path,
    rpc: Arc<Rpc>,
    signer: Arc<SolanaSigner>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let prices = Arc::new(Prices);
    let wallet = Arc::new(
        SolanaWalletAdapter::new(
            "wallet",
            signer.address(),
            Some(signer.clone()),
            rpc,
            prices.clone(),
        )
        .unwrap(),
    );
    let adapter = Arc::new(
        KaminoAdapter::new(
            BTreeMap::from([("wallet".into(), wallet.clone())]),
            vec![
                Arc::new(LendingProtocol::default()),
                Arc::new(LiquidityProtocol::with_api(Arc::new(OfflineApi))),
            ],
            prices,
        )
        .unwrap(),
    );
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = "500000000".into();
    policy.max_run_allocation_usd_micros = "2000000000".into();
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![wallet, adapter],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap()
}

fn borrowing_runtime(
    path: &Path,
    rpc: Arc<Rpc>,
    primary: Arc<SolanaSigner>,
    spend: Arc<SolanaSigner>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let prices = Arc::new(Prices);
    let primary_wallet = Arc::new(
        SolanaWalletAdapter::new(
            "wallet",
            primary.address(),
            Some(primary.clone()),
            rpc.clone(),
            prices.clone(),
        )
        .unwrap(),
    );
    let spend_wallet = if spend.address() == primary.address() {
        primary_wallet.clone()
    } else {
        Arc::new(
            SolanaWalletAdapter::new(
                "second",
                spend.address(),
                Some(spend.clone()),
                rpc,
                prices.clone(),
            )
            .unwrap(),
        )
    };
    let klend = Arc::new(
        KlendAdapter::new(
            BTreeMap::from([("wallet".into(), primary_wallet)]),
            Arc::new(NativeKlendProtocol::new()),
            prices,
        )
        .unwrap(),
    );
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = "500000000".into();
    policy.max_run_allocation_usd_micros = "2000000000".into();
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![spend_wallet, klend],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap()
}
async fn read(client: &Client, op: &str, input: Value) -> Value {
    eprintln!("Owned-validator CLI read: {op}");
    client
        .call(
            args(&["read", op, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Read {
                operation: op.into(),
                input,
            },
            true,
        )
        .await
}
async fn preview(client: &Client, op: &str, input: &Value, ok: bool) -> Value {
    eprintln!("Owned-validator CLI preview: {op}; expected success={ok}");
    let value = client
        .call(
            args(&["preview", op, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Preview {
                operation: op.into(),
                input: input.clone(),
            },
            ok,
        )
        .await;
    #[cfg(target_os = "macos")]
    if ok {
        native_approval::assert_prepared_renders(&serde_json::from_value(value.clone()).unwrap());
    }
    value
}
async fn execute(client: &Client, op: &str, input: Value, id: &str) -> Value {
    eprintln!("Owned-validator CLI execute: {op}; id={id}");
    client
        .call(
            args(&[
                "execute",
                op,
                "--input",
                &input.to_string(),
                "--id",
                id,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id.into(),
                    operation: op.into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await
}
async fn settle(client: &Client, mut result: Value) -> Value {
    let id = result["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if result["status"] == "completed" {
            return result;
        }
        assert_ne!(result["status"], "failed", "{result}");
        tokio::time::sleep(Duration::from_millis(600)).await;
        result = client
            .call(
                args(&["status", &id, "--json"]),
                BrokerRequest::Status { id: id.clone() },
                true,
            )
            .await;
    }
    panic!("Local settlement did not complete: {result}");
}

fn receipt_token_balance(receipt: &Value, field: &str, owner: &str, mint: &str) -> u128 {
    receipt["meta"][field]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["owner"] == owner && row["mint"] == mint)
        .map(|row| {
            row["uiTokenAmount"]["amount"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .unwrap()
        })
        .sum()
}

async fn token_balance(rpc: &Rpc, account: &str) -> u64 {
    rpc.request(
        "getTokenAccountBalance",
        json!([account,{"commitment":"finalized"}]),
    )
    .await
    .unwrap()["value"]["amount"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn positive_vault(family: &str) {
    let cli = std::env::var("GOFIN_KAMINO_CLI").expect("public CLI required");
    assert!(Path::new(&cli).is_file());
    let url = std::env::var("GOFIN_KAMINO_POSITIVE_RPC").unwrap();
    let endpoint = reqwest::Url::parse(&url).unwrap();
    assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(std::env::var("GOFIN_KAMINO_POSITIVE_MANIFEST").unwrap()).unwrap(),
    )
    .unwrap();
    let signer = Arc::new(
        SolanaSigner::parse(
            &std::fs::read_to_string(manifest["wallet_key"].as_str().unwrap()).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(signer.address(), manifest["wallet"].as_str().unwrap());
    let rpc = Arc::new(Rpc {
        url,
        client: reqwest::Client::new(),
        sends: AtomicUsize::new(0),
        observed_slot: AtomicU64::new(0),
    });
    let temp = tempfile::tempdir().unwrap();
    let owner = signer.address().to_owned();
    let client = Client::new(
        runtime(
            &temp.path().join("store.sqlite"),
            rpc.clone(),
            signer,
            Arc::new(Approval::default()),
        ),
        temp.path(),
    )
    .await;
    let address = if family == "lending" {
        manifest["vault"].as_str().unwrap()
    } else {
        manifest["strategies"][0]["address"].as_str().unwrap()
    };
    let vault = read(
        &client,
        "kamino.vault",
        json!({"wallet":"wallet","family":family,"vault":address}),
    )
    .await;
    let info = &vault["vault"];
    let share = info["shares"]["mint"].as_str().unwrap();
    let amounts: Vec<Value> = info["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let decimals = a["decimals"].as_u64().unwrap() as u32;
            let units = if family == "lending" { 50 } else { 1 };
            json!({"mint":a["mint"],"atoms":(units*10u64.pow(decimals)).to_string()})
        })
        .collect();
    let mut input = json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","family":family,"vault":address,"share_mint":share,"amounts":amounts,"maximum_network_fee_lamports":1000000,"maximum_account_funding_lamports":100000000});
    preview(&client, "kamino.deposit", &input, true).await;
    let deposited = settle(
        &client,
        execute(&client, "kamino.deposit", input.clone(), "deposit").await,
    )
    .await;
    assert_eq!(deposited["status"], "completed");
    let position = read(
        &client,
        "kamino.position",
        json!({"wallet":"wallet","family":family,"vault":address}),
    )
    .await;
    let minted = position["position"]["wallet_shares_atoms"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(minted > 0);
    input.as_object_mut().unwrap().remove("amounts");
    input["shares_atoms"] = json!("all");
    preview(&client, "kamino.withdraw", &input, true).await;
    let withdrawn = settle(
        &client,
        execute(&client, "kamino.withdraw", input, "withdraw").await,
    )
    .await;
    assert_eq!(withdrawn["status"], "completed");
    assert_ne!(
        deposited["submission"]["reference"],
        withdrawn["submission"]["reference"]
    );
    for (record, deposit) in [(&deposited, true), (&withdrawn, false)] {
        let sig = record["submission"]["data"]["chain_signature"]
            .as_str()
            .unwrap();
        let receipt=rpc.request("getTransaction",json!([sig,{"encoding":"json","commitment":"finalized","maxSupportedTransactionVersion":0}])).await.unwrap();
        assert!(receipt["slot"].as_u64().unwrap() > 0);
        assert!(receipt["meta"]["err"].is_null());
        let fee = receipt["meta"]["fee"].as_u64().unwrap();
        assert!(fee > 0 && fee <= 1000000);
        assert!(
            !receipt["meta"]["preTokenBalances"].is_null()
                && !receipt["meta"]["postTokenBalances"].is_null()
        );
        let before = receipt_token_balance(&receipt, "preTokenBalances", &owner, share);
        let after = receipt_token_balance(&receipt, "postTokenBalances", &owner, share);
        if deposit {
            assert_eq!(after - before, u128::from(minted));
        } else {
            assert_eq!(before - after, u128::from(minted));
        }
        for amount in &amounts {
            let mint = amount["mint"].as_str().unwrap();
            let before = receipt_token_balance(&receipt, "preTokenBalances", &owner, mint);
            let after = receipt_token_balance(&receipt, "postTokenBalances", &owner, mint);
            if deposit {
                assert!(
                    before >= after
                        && before - after
                            <= amount["atoms"].as_str().unwrap().parse::<u128>().unwrap()
                );
            } else {
                assert!(after >= before);
            }
        }
    }
    let final_position = read(
        &client,
        "kamino.position",
        json!({"wallet":"wallet","family":family,"vault":address}),
    )
    .await;
    assert_eq!(final_position["position"]["wallet_shares_atoms"], "0");
}
#[tokio::test]
#[ignore = "requires isolated Surfpool and actual CLI"]
async fn positive_kamino_lending_via_public_cli() {
    positive_vault("lending").await;
}
#[tokio::test]
#[ignore = "requires isolated Surfpool and actual CLI"]
async fn positive_kamino_liquidity_via_public_cli() {
    positive_vault("liquidity").await;
}

#[tokio::test]
#[ignore = "requires isolated Surfpool and actual CLI"]
async fn positive_kamino_borrowing_via_public_cli() {
    let cli = std::env::var("GOFIN_KAMINO_CLI").expect("public CLI required");
    assert!(Path::new(&cli).is_file());
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(std::env::var("GOFIN_KAMINO_POSITIVE_MANIFEST").unwrap()).unwrap(),
    )
    .unwrap();
    let primary = Arc::new(
        SolanaSigner::parse(
            &std::fs::read_to_string(manifest["wallet_key"].as_str().unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let second = Arc::new(
        SolanaSigner::parse(
            &std::fs::read_to_string(std::env::var("GOFIN_KAMINO_SECOND_KEY").unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let url = std::env::var("GOFIN_KAMINO_POSITIVE_RPC").unwrap();
    assert_eq!(
        reqwest::Url::parse(&url).unwrap().host_str(),
        Some("127.0.0.1")
    );
    let rpc = Arc::new(Rpc {
        url,
        client: reqwest::Client::new(),
        sends: AtomicUsize::new(0),
        observed_slot: AtomicU64::new(0),
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store.sqlite");
    let approval = Arc::new(Approval::default());
    let mut client = Client::new(
        borrowing_runtime(
            &path,
            rpc.clone(),
            primary.clone(),
            primary.clone(),
            approval.clone(),
        ),
        temp.path(),
    )
    .await;
    let market = manifest["market"].as_str().unwrap();
    let initial = read(
        &client,
        "kamino.lending.obligation",
        json!({"wallet":"wallet","market":market}),
    )
    .await;
    assert_eq!(initial["exists"], false);
    let obligation = initial["address"].as_str().unwrap();
    let selected = json!({"wallet":"wallet","market":market,"obligation":obligation});
    let risk = json!({"minimum_health_factor_bps":20000,"maximum_debt_usd_micros":"10000000","maximum_debt_to_collateral_bps":3000});
    let input = |i: usize| json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","market":market,"obligation":obligation,"reserve":manifest["reserves"][i]["address"],"token_mint":manifest["reserves"][i]["mint"],"maximum_protocol_fee_atoms":"100000","maximum_network_fee_lamports":50000,"maximum_account_funding_lamports":0});
    let mut receipts = Vec::new();
    let mut supply = input(1);
    supply["amount_atoms"] = json!("20000000");
    supply["maximum_account_funding_lamports"] = json!(100000000);
    preview(&client, "kamino.lending.supply", &supply, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "kamino.lending.supply", supply, "supply").await,
        )
        .await,
    );
    let supplied = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(
        supplied["positions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["side"] == "supply"
                && p["native_units"].as_str().unwrap().parse::<u64>().unwrap() > 0)
    );
    let mut borrow = input(0);
    borrow["amount_atoms"] = json!("5000000");
    borrow["maximum_debt_increase_atoms"] = json!("5100000");
    borrow["risk_limits"] = risk.clone();
    // The positive fixture enables borrowing only in its owned clone. Prove
    // that the same real adapter still refuses a paused market before using it.
    {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let params = json!([market, {"encoding":"base64", "commitment":"processed"}]);
        let account = rpc.request("getAccountInfo", params.clone()).await.unwrap();
        let enabled = STANDARD
            .decode(account["value"]["data"][0].as_str().unwrap())
            .unwrap();
        let offset = manifest["market_policy_layout"]["borrow_disabled_offset"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(enabled[offset], 0);
        let mut paused = enabled.clone();
        paused[offset] = 1;
        rpc.request(
            "surfnet_setAccount",
            json!([market, {"data":hex::encode(&paused)}]),
        )
        .await
        .unwrap();
        let sends = rpc.sends.load(Ordering::SeqCst);
        let denied = preview(&client, "kamino.lending.borrow", &borrow, false).await;
        assert_eq!(denied["code"], "KAMINO_LENDING_UNAVAILABLE");
        assert_eq!(rpc.sends.load(Ordering::SeqCst), sends);
        rpc.request(
            "surfnet_setAccount",
            json!([market, {"data":hex::encode(&enabled)}]),
        )
        .await
        .unwrap();
        let restored = rpc.request("getAccountInfo", params).await.unwrap();
        assert_eq!(restored["value"]["data"], account["value"]["data"]);
    }
    preview(&client, "kamino.lending.borrow", &borrow, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "kamino.lending.borrow", borrow, "borrow").await,
        )
        .await,
    );
    let borrowed = read(&client, "kamino.lending.obligation", selected.clone()).await;
    let debt = borrowed["positions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["side"] == "debt")
        .unwrap()["underlying_atoms"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((5_000_000..=5_100_000).contains(&debt));
    let mint = manifest["reserves"][0]["mint"].as_str().unwrap();
    let token_program = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    let primary_ata =
        gofin_solana::associated_token_address(primary.address(), mint, token_program).unwrap();
    let second_ata =
        gofin_solana::associated_token_address(second.address(), mint, token_program).unwrap();
    let before_transfer = token_balance(&rpc, &primary_ata).await;
    let transfer = |wallet: &str, sender: &str, recipient: &str, id: &str| json!({"wallet":wallet,"network":"solana","cluster":"mainnet-beta","recipient":recipient,"mint":mint,"source_token_account":gofin_solana::associated_token_address(sender,mint,"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap(),"amount":"1","max_fee_sol":"0.001","max_account_funding_sol":"0.01","requestId":id});
    let out = transfer(
        "wallet",
        primary.address(),
        second.address(),
        "transfer-out",
    );
    preview(&client, "wallet.transfer", &out, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "wallet.transfer", out, "transfer-out").await,
        )
        .await,
    );
    assert_eq!(
        token_balance(&rpc, &primary_ata).await,
        before_transfer - 1_000_000
    );
    assert_eq!(token_balance(&rpc, &second_ata).await, 1_000_000);
    drop(client);
    client = Client::new(
        borrowing_runtime(
            &path,
            rpc.clone(),
            primary.clone(),
            second.clone(),
            approval.clone(),
        ),
        temp.path(),
    )
    .await;
    let back = transfer(
        "second",
        second.address(),
        primary.address(),
        "transfer-back",
    );
    preview(&client, "wallet.transfer", &back, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "wallet.transfer", back, "transfer-back").await,
        )
        .await,
    );
    assert_eq!(token_balance(&rpc, &primary_ata).await, before_transfer);
    assert_eq!(token_balance(&rpc, &second_ata).await, 0);
    drop(client);
    client = Client::new(
        borrowing_runtime(&path, rpc.clone(), primary.clone(), primary, approval),
        temp.path(),
    )
    .await;
    let mut repay = input(0);
    repay["amount_atoms"] = json!("all");
    repay["maximum_repay_atoms"] = json!("5200000");
    preview(&client, "kamino.lending.repay", &repay, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "kamino.lending.repay", repay, "repay").await,
        )
        .await,
    );
    let debt_free = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(
        !debt_free["positions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["side"] == "debt")
    );
    let mut withdrawal = input(1);
    withdrawal["collateral_atoms"] = json!("all");
    withdrawal["minimum_receive_atoms"] = json!("19000000");
    withdrawal["risk_limits"] = risk;
    preview(&client, "kamino.lending.withdraw", &withdrawal, true).await;
    receipts.push(
        settle(
            &client,
            execute(&client, "kamino.lending.withdraw", withdrawal, "withdraw").await,
        )
        .await,
    );
    let done = read(&client, "kamino.lending.obligation", selected).await;
    assert!(done["positions"].as_array().unwrap().is_empty());
    assert_eq!(rpc.sends.load(Ordering::SeqCst), 6);
    let mut signatures = std::collections::BTreeSet::new();
    for record in &receipts {
        let sig = record["submission"]["data"]["chain_signature"]
            .as_str()
            .unwrap();
        assert!(signatures.insert(sig.to_owned()));
        let receipt=rpc.request("getTransaction",json!([sig,{"encoding":"json","commitment":"finalized","maxSupportedTransactionVersion":0}])).await.unwrap();
        assert!(receipt["slot"].as_u64().unwrap() > 0 && receipt["meta"]["err"].is_null());
        let fee = receipt["meta"]["fee"].as_u64().unwrap();
        assert!(fee > 0 && fee <= 1_000_000);
    }
}
