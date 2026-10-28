//! Public CLI synthetic Hyperliquid spot and perpetual round trips.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::SystemClock, *};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{EvmRuntime, LocalEnvelopeSigner, alloy::primitives::U256};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[allow(dead_code)]
#[path = "support/bridge_erc20_fixture.rs"]
mod bridge_fixture;
#[path = "support/hyperliquid_positive.rs"]
mod fixture;
#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;
include!("support/kraken_plan_client.rs");
struct Prices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if a.reference == "native" { "2000" } else { "1" }.into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic HL prices".into(),
        })
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.purpose, "financial_write");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic HL approval".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
async fn setup() -> (tempfile::TempDir, Client, Arc<fixture::Fixture>, String) {
    let temp = tempfile::tempdir().unwrap();
    let signer = Arc::new(
        LocalEnvelopeSigner::from_private_key(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
    );
    let wallet = gofin_evm::lower_address(signer.address());
    let rpc = Arc::new(bridge_fixture::Rpc::destination_on(wallet.clone(), 42161));
    *rpc.token_balance.lock().unwrap() = U256::from(100_000_000u64);
    let venue = fixture::Fixture::new(wallet.clone(), rpc);
    let valuator = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let mut config = EthereumConfig::wallet_only(42161, 1, vec![bridge_fixture::ARB_USDC.into()]);
    config.require_finalized = false;
    let eth = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(venue.clone(), 42161, Some(signer))),
        )
        .unwrap()
        .with_wallet_valuation(valuator.clone()),
    );
    let profile = Arc::new(venue.profile());
    let exchange = Arc::new(gofin_exchange::ExchangeAdapter::new(
        profile.without_operation("hyperliquid.account.deposit"),
    ));
    let deposit = Arc::new(gofin_broker::HyperliquidEthereumDepositAdapter::new(
        profile,
        eth,
        venue.clone(),
    ));
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = "300000000".into();
    policy.max_run_allocation_usd_micros = "500000000".into();
    let runtime = compose_runtime_with_clock(
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
            store: Store::open(temp.path().join("hl.sqlite")).unwrap(),
            adapters: vec![deposit, exchange],
            authorizer: Arc::new(Approval(AtomicUsize::new(0))),
            valuator,
            bridge: None,
        },
        Arc::new(SystemClock),
    )
    .unwrap();
    let client = Client::new(runtime, temp.path()).await;
    (temp, client, venue, wallet)
}
async fn execute(client: &Client, op: &str, input: Value) -> Value {
    let id = input
        .get("request_id")
        .or_else(|| input.get("requestId"))
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let preview = client
        .call(
            args(&["preview", op, "--input", &input.to_string(), "--json"]),
            BrokerRequest::Preview {
                operation: op.into(),
                input: input.clone(),
            },
            true,
        )
        .await;
    #[cfg(target_os = "macos")]
    native_approval::assert_prepared_renders(&serde_json::from_value(preview).unwrap());
    client
        .call(
            args(&[
                "execute",
                op,
                "--input",
                &input.to_string(),
                "--id",
                &id,
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id,
                    operation: op.into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI synthetic-kraken-build"]
async fn positive_hyperliquid_perpetual_via_public_cli() {
    let cli = std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").expect("public CLI required");
    assert!(std::path::Path::new(&cli).is_file());
    let (_temp, client, venue, _wallet) = setup().await;
    let deposit = execute(
        &client,
        "hyperliquid.account.deposit",
        fixture::deposit_input("50"),
    )
    .await;
    assert_eq!(deposit["status"], "completed", "{deposit}");
    let leverage = execute(
        &client,
        "hyperliquid.account.set_leverage",
        fixture::leverage_input(),
    )
    .await;
    assert_eq!(leverage["status"], "completed", "{leverage}");
    for (id, side, reduce) in [
        ("long-open", "buy", false),
        ("long-close", "sell", true),
        ("short-open", "sell", false),
        ("short-close", "buy", true),
    ] {
        let order = json!({"request_id":id,"account":"synthetic-hl","symbol":"SOL/USDC:USDC","market_type":"perpetual","side":side,"order_type":"limit","size":"0.1","limit_price":"100","min_price":"100","time_in_force":"ioc","reduce_only":reduce,"max_notional_usdc":"10","max_fee_usdc":"0.1","max_realized_loss_usdc":"0"});
        let result = execute(&client, "hyperliquid.orders.place", order).await;
        assert_eq!(result["status"], "completed", "{result}");
        if id == "long-close" {
            let resting = json!({"request_id":"rest","account":"synthetic-hl","symbol":"SOL/USDC:USDC","market_type":"perpetual","side":"sell","order_type":"limit","size":"0.1","limit_price":"110","min_price":"110","time_in_force":"gtc","max_notional_usdc":"11","max_fee_usdc":"0.1","max_realized_loss_usdc":"0"});
            let open = execute(&client, "hyperliquid.orders.place", resting).await;
            assert_eq!(open["status"], "pending", "{open}");
            let oid = venue.open_oid().to_string();
            let cancel = json!({"request_id":"cancel","account":"synthetic-hl","symbol":"SOL/USDC:USDC","order_id":oid});
            let cancelled = execute(&client, "hyperliquid.orders.cancel", cancel).await;
            assert_eq!(cancelled["status"], "completed", "{cancelled}");
            let parent_id = open["id"].as_str().unwrap();
            let parent = client
                .call(
                    args(&["status", parent_id, "--json"]),
                    BrokerRequest::Status {
                        id: parent_id.into(),
                    },
                    true,
                )
                .await;
            assert!(
                parent["status"] == "failed" || parent["status"] == "completed",
                "{parent}"
            );
        }
    }
    assert_eq!(venue.cash().3, 0);
    let withdrawal = json!({"request_id":"withdraw","account":"synthetic-hl","asset":"USDC","amount":"49.96","address":_wallet,"max_amount_usdc":"49.96","max_fee_usdc":"0.01"});
    let pending = execute(&client, "hyperliquid.account.withdraw", withdrawal).await;
    assert!(
        pending["status"] == "unknown" || pending["status"] == "pending",
        "{pending}"
    );
    let nonce = pending["submission"]["data"]["envelope"]["nonce_ms"]
        .as_u64()
        .unwrap();
    venue.set_receipt(json!({"hash":format!("0x{:064x}",nonce+1),"recipient":_wallet,"token":bridge_fixture::ARB_USDC,"amount":"49.95","status":"0x1","chainId":42161,"sender":"0x2df1c51e09aecf9cacb7bc98cb1742757f163df7"}));
    let id = pending["id"].as_str().unwrap();
    let done = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(venue.cash().0, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI synthetic-kraken-build"]
async fn positive_hyperliquid_spot_via_public_cli() {
    let cli = std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").expect("public CLI required");
    assert!(std::path::Path::new(&cli).is_file());
    let (_temp, client, venue, wallet) = setup().await;
    let deposit = execute(
        &client,
        "hyperliquid.account.deposit",
        fixture::deposit_input("50"),
    )
    .await;
    assert_eq!(deposit["status"], "completed", "{deposit}");
    let transfer = json!({"request_id":"to-spot","account":"synthetic-hl","asset":"USDC","amount":"30","destination":"spot","max_amount_usdc":"30","max_fee_usdc":"0.01"});
    let moved = execute(&client, "hyperliquid.account.transfer", transfer).await;
    assert_eq!(moved["status"], "completed", "{moved}");
    for (id, side, size, price, tif, status) in [
        ("buy", "buy", "2.02", "10", "ioc", "completed"),
        ("part", "sell", "1", "10", "ioc", "completed"),
        ("rest", "sell", "1.01", "10", "gtc", "pending"),
        ("final", "sell", "1.01", "10", "ioc", "completed"),
    ] {
        let order = json!({"request_id":id,"account":"synthetic-hl","symbol":"ETH/USDC","market_type":"spot","side":side,"order_type":"limit","size":size,"limit_price":price,"min_price":price,"time_in_force":tif,"max_notional_usdc":if id=="buy" {"20.2"} else if id=="final" || id=="rest" {"10.1"} else {"10"},"max_fee_usdc":"0.1","max_realized_loss_usdc":"0"});
        let result = execute(&client, "hyperliquid.orders.place", order).await;
        assert_eq!(result["status"], status, "{result}");
        if id == "final" {
            assert!(
                result["effects"]["effects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "cost"
                        && e["amount"]["asset"]["reference"] == "0xeeee"
                        && e["amount"]["atoms"]
                            == Quantity::from_decimal("0.01", 36).unwrap().as_str()),
                "{result}"
            );
        }
        if id == "rest" {
            let oid = venue.open_oid().to_string();
            let cancel = json!({"request_id":"cancel","account":"synthetic-hl","symbol":"ETH/USDC","order_id":oid});
            let cancelled = execute(&client, "hyperliquid.orders.cancel", cancel).await;
            assert_eq!(cancelled["status"], "completed", "{cancelled}");
            let parent_id = result["id"].as_str().unwrap();
            let parent = client
                .call(
                    args(&["status", parent_id, "--json"]),
                    BrokerRequest::Status {
                        id: parent_id.into(),
                    },
                    true,
                )
                .await;
            assert!(
                parent["status"] == "failed" || parent["status"] == "completed",
                "{parent}"
            );
        }
    }
    assert_eq!(venue.cash().2, 0);
    assert_eq!(venue.cash().1, 29_880_000);
    let transfer = json!({"request_id":"to-perp","account":"synthetic-hl","asset":"USDC","amount":"29.88","destination":"perpetual","max_amount_usdc":"29.88","max_fee_usdc":"0.01"});
    let moved = execute(&client, "hyperliquid.account.transfer", transfer).await;
    assert_eq!(moved["status"], "completed", "{moved}");
    let withdrawal = json!({"request_id":"withdraw","account":"synthetic-hl","asset":"USDC","amount":"49.88","address":wallet,"max_amount_usdc":"49.88","max_fee_usdc":"0.01"});
    let pending = execute(&client, "hyperliquid.account.withdraw", withdrawal).await;
    assert!(
        pending["status"] == "unknown" || pending["status"] == "pending",
        "{pending}"
    );
    let nonce = pending["submission"]["data"]["envelope"]["nonce_ms"]
        .as_u64()
        .unwrap();
    venue.set_receipt(json!({"hash":format!("0x{:064x}",nonce+1),"recipient":wallet,"token":bridge_fixture::ARB_USDC,"amount":"49.87","status":"0x1","chainId":42161,"sender":"0x2df1c51e09aecf9cacb7bc98cb1742757f163df7"}));
    let id = pending["id"].as_str().unwrap();
    let done = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(venue.cash().0, 0);
}
