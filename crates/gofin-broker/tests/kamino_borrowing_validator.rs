//! Public CLI -> real broker/engine/adapter -> owned cloned validator.
//! Only synthetic credentials/funding. Run the dedicated Python runner.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::*;
use gofin_kamino::klend::{KlendAdapter, NativeKlendProtocol};
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    hide_receipts: AtomicBool,
    lose_response: AtomicBool,
}
#[async_trait]
impl SolanaRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "getGenesisHash" {
            return Ok(json!(gofin_solana::MAINNET_GENESIS_HASH));
        }
        if self.hide_receipts.load(Ordering::SeqCst) {
            if method == "getTransaction" {
                return Ok(Value::Null);
            }
            if method == "getSignatureStatuses" {
                return Ok(json!({"context":{"slot":0},"value":[null]}));
            }
        }
        if method == "sendTransaction" {
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
        if method == "simulateTransaction" && !response["result"]["value"]["err"].is_null() {
            eprintln!("Local simulation failure: {}", response["result"]["value"]);
        }
        if method == "sendTransaction" && self.lose_response.swap(false, Ordering::SeqCst) {
            return Err(error(
                "Controlled loss of response after validator accepted submission",
            ));
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
        KlendAdapter::new(
            BTreeMap::from([("wallet".into(), wallet.clone())]),
            Arc::new(NativeKlendProtocol::new()),
            prices,
        )
        .unwrap(),
    );
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
            adapters: vec![wallet, adapter],
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
    client
        .call(
            args(&["preview", op, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Preview {
                operation: op.into(),
                input: input.clone(),
            },
            ok,
        )
        .await
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
fn position<'a>(obligation: &'a Value, side: &str) -> Option<&'a Value> {
    obligation["positions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["side"] == side)
}

#[tokio::test]
#[ignore = "owned cloned validator + synthetic credentials only; use scripts/verify-kamino-borrowing-validator.py"]
async fn cli_collateral_borrow_partial_repay_restart_full_repay_and_withdraw() {
    assert!(
        std::env::var("GOFIN_KAMINO_CLI").is_ok(),
        "CLI binary required; no direct-request fallback"
    );
    let url = std::env::var("GOFIN_KLEND_VALIDATOR_RPC").unwrap();
    let endpoint = reqwest::Url::parse(&url).unwrap();
    assert_eq!(endpoint.scheme(), "http");
    assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
    assert!(endpoint.username().is_empty() && endpoint.password().is_none());
    assert!(endpoint.port().is_some());
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(std::env::var("GOFIN_KLEND_VALIDATOR_MANIFEST").unwrap()).unwrap(),
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
        hide_receipts: AtomicBool::new(false),
        lose_response: AtomicBool::new(false),
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let approval = Arc::new(Approval::default());
    let mut client = Client::new(
        runtime(&path, rpc.clone(), signer.clone(), approval.clone()),
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
    let obligation = initial["address"].as_str().unwrap().to_owned();
    let selected = json!({"wallet":"wallet","market":market,"obligation":obligation});
    let risk = json!({"minimum_health_factor_bps":20000,"maximum_debt_usd_micros":"10000000","maximum_debt_to_collateral_bps":3000});
    let input = |index: usize| json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","market":market,"obligation":obligation,"reserve":manifest["reserves"][index]["address"],"token_mint":manifest["reserves"][index]["mint"],"maximum_protocol_fee_atoms":"100000","maximum_network_fee_lamports":50000,"maximum_account_funding_lamports":0});
    let market_view = read(
        &client,
        "kamino.lending.market",
        json!({"wallet":"wallet","market":market}),
    )
    .await;
    assert_eq!(market_view["address"], market);
    for reserve in manifest["reserves"].as_array().unwrap() {
        let view = read(
            &client,
            "kamino.lending.reserve",
            json!({"wallet":"wallet","market":market,"reserve":reserve["address"]}),
        )
        .await;
        assert_eq!(view["token"]["mint"], reserve["mint"]);
    }
    let mut supply = input(0);
    supply["amount_atoms"] = json!("20000000");
    supply["maximum_account_funding_lamports"] = json!(100000000);
    preview(&client, "kamino.lending.supply", &supply, true).await;
    let supply_record = settle(
        &client,
        execute(&client, "kamino.lending.supply", supply, "klend-supply").await,
    )
    .await;
    let supplied = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(
        position(&supplied, "supply").unwrap()["native_units"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    let mut borrow = input(1);
    borrow["amount_atoms"] = json!("5000000");
    borrow["maximum_debt_increase_atoms"] = json!("5100000");
    borrow["risk_limits"] = risk.clone();
    let mut unsafe_borrow = borrow.clone();
    unsafe_borrow["risk_limits"]["maximum_debt_usd_micros"] = json!("1");
    preview(&client, "kamino.lending.borrow", &unsafe_borrow, false).await;
    let mut wrong_mint = borrow.clone();
    wrong_mint["token_mint"] = manifest["reserves"][0]["mint"].clone();
    preview(&client, "kamino.lending.borrow", &wrong_mint, false).await;
    let mut insufficient_debt_authority = borrow.clone();
    insufficient_debt_authority["maximum_debt_increase_atoms"] = json!("4999999");
    preview(
        &client,
        "kamino.lending.borrow",
        &insufficient_debt_authority,
        false,
    )
    .await;
    assert_eq!(
        rpc.sends.load(Ordering::SeqCst),
        1,
        "rejected risk preview must never send"
    );
    preview(&client, "kamino.lending.borrow", &borrow, true).await;
    // Submit exactly once, lose the response and hide settlement, then restart
    // the broker over the same journal before recovering the original identity.
    rpc.hide_receipts.store(true, Ordering::SeqCst);
    rpc.lose_response.store(true, Ordering::SeqCst);
    let pending = execute(&client, "kamino.lending.borrow", borrow, "klend-borrow").await;
    assert!(!pending["submission"].is_null(), "{pending}");
    assert_ne!(pending["status"], "completed");
    let identity = pending["submission"].clone();
    let sends = rpc.sends.load(Ordering::SeqCst);
    drop(client);
    client = Client::new(runtime(&path, rpc.clone(), signer, approval), temp.path()).await;
    rpc.hide_receipts.store(false, Ordering::SeqCst);
    let borrow_record = settle(&client, pending).await;
    assert_eq!(borrow_record["submission"], identity);
    assert_eq!(
        rpc.sends.load(Ordering::SeqCst),
        sends,
        "recovery must never replay the borrow"
    );
    let borrowed = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(
        position(&borrowed, "debt").unwrap()["underlying_atoms"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 5_000_000
    );
    let mut unsafe_withdrawal = input(0);
    unsafe_withdrawal["collateral_atoms"] = json!("all");
    unsafe_withdrawal["minimum_receive_atoms"] = json!("1");
    unsafe_withdrawal["risk_limits"] = risk.clone();
    preview(
        &client,
        "kamino.lending.withdraw",
        &unsafe_withdrawal,
        false,
    )
    .await;
    assert_eq!(rpc.sends.load(Ordering::SeqCst), sends);
    let portfolio = client
        .call(args(&["snapshot", "--json"]), BrokerRequest::Snapshot, true)
        .await;
    let mut repay = input(1);
    repay["amount_atoms"] = json!("2000000");
    preview(&client, "kamino.lending.repay", &repay, true).await;
    let partial_record = settle(
        &client,
        execute(
            &client,
            "kamino.lending.repay",
            repay,
            "klend-partial-repay",
        )
        .await,
    )
    .await;
    let partial = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(
        position(&partial, "debt").unwrap()["underlying_atoms"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            < 4_000_000
    );
    let mut repay_all = input(1);
    repay_all["amount_atoms"] = json!("all");
    repay_all["maximum_repay_atoms"] = json!("3200000");
    preview(&client, "kamino.lending.repay", &repay_all, true).await;
    let repay_record = settle(
        &client,
        execute(
            &client,
            "kamino.lending.repay",
            repay_all,
            "klend-full-repay",
        )
        .await,
    )
    .await;
    let debt_free = read(&client, "kamino.lending.obligation", selected.clone()).await;
    assert!(position(&debt_free, "debt").is_none());
    let mut withdraw = input(0);
    withdraw["collateral_atoms"] = json!("all");
    withdraw["minimum_receive_atoms"] = json!("19000000");
    withdraw["risk_limits"] = risk;
    preview(&client, "kamino.lending.withdraw", &withdraw, true).await;
    let withdraw_record = settle(
        &client,
        execute(
            &client,
            "kamino.lending.withdraw",
            withdraw,
            "klend-withdraw",
        )
        .await,
    )
    .await;
    let final_obligation = read(&client, "kamino.lending.obligation", selected).await;
    assert!(final_obligation["positions"].as_array().unwrap().is_empty());
    assert_eq!(rpc.sends.load(Ordering::SeqCst), 5);
    let evidence = json!({"kind":"owned_cloned_validator","automatic_development_approval":true,"public_writes":false,"oracle_age_fixture_seconds":manifest["synthetic_oracle_max_age_seconds"],"initial":initial,"supplied":supplied,"borrowed":borrowed,"portfolio_with_debt":portfolio,"partially_repaid":partial,"debt_free":debt_free,"final_obligation":final_obligation,"operations":[supply_record,borrow_record,partial_record,repay_record,withdraw_record],"send_count":5});
    std::fs::write(
        std::env::var("GOFIN_KLEND_VALIDATOR_EVIDENCE").unwrap(),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}
