//! Controlled public-CLI reverse workflow using real adapters and synthetic providers.
use async_trait::async_trait;
use gofin_broker::{
    BridgeAdapter, BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies,
    SharedUsdValuator, compose_runtime_with_clock,
};
use gofin_domain::{clock::SystemClock, *};
use gofin_ethereum::uniswap_api::api::{UniswapApiClient, UniswapApiRequest, UniswapApiTransport};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner,
    alloy::{
        consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable},
        eips::eip2718::Decodable2718,
        primitives::{U256, keccak256},
    },
};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::sync::Mutex;
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
#[allow(dead_code)]
#[path = "support/hyperliquid_positive.rs"]
mod fixture;
#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;
include!("support/kraken_plan_client.rs");
const OUTPUT_TOKEN: &str = "0x0000000000000000000000000000000000000066";
const SWAP_ROUTER: &str = "0x00000000000000000000000000000000000001d6";
struct SwapProvider {
    wallet: String,
    swaps: AtomicUsize,
}
#[async_trait]
impl UniswapApiTransport for SwapProvider {
    async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
        match request {
            UniswapApiRequest::Post { path, body } if path == "/quote" => {
                let amount = body["amount"].as_str().unwrap();
                Ok(
                    json!({"routing":"CLASSIC","permitData":null,"quote":{"chainId":1,"swapper":self.wallet,"tradeType":"EXACT_INPUT","quoteId":"reverse-quote","input":{"token":body["tokenIn"],"amount":amount,"maximumAmount":amount},"output":{"token":body["tokenOut"],"recipient":self.wallet,"amount":amount,"minimumAmount":"990000"},"slippage":1,"priceImpact":0,"gasFee":"100000000000000","txFailureReasons":[]}}),
                )
            }
            UniswapApiRequest::Post { path, .. } if path == "/swap" => {
                self.swaps.fetch_add(1, Ordering::SeqCst);
                Ok(
                    json!({"swap":{"chainId":1,"from":self.wallet,"to":SWAP_ROUTER,"value":"0","data":"0x12345678","gasLimit":"200000"}}),
                )
            }
            _ => Err(GoFinError::validation(
                "Unexpected synthetic swap API request.",
            )),
        }
    }
}
struct SwapRpc {
    inner: Arc<bridge_fixture::Rpc>,
    wallet: String,
    transaction: Mutex<Option<(Value, Value)>>,
    sends: AtomicUsize,
}
#[async_trait]
impl EvmRpc for SwapRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call" {
            let call = &params[0];
            let to = call["to"].as_str().unwrap_or("");
            let data = call["data"].as_str().unwrap_or("");
            if data.starts_with("0x95d89b41") {
                let name = if to.eq_ignore_ascii_case(OUTPUT_TOKEN) {
                    "OUT"
                } else {
                    "USDC"
                };
                use gofin_evm::alloy::sol_types::SolValue;
                return Ok(json!(format!("0x{}", hex::encode(name.abi_encode()))));
            }
            if to.eq_ignore_ascii_case(OUTPUT_TOKEN) && data.starts_with("0x313ce567") {
                return Ok(json!(format!("0x{:064x}", 6)));
            }
        }
        if method == "eth_sendRawTransaction" {
            let raw = gofin_evm::rpc_bytes(&params[0])?;
            let tx = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
            if tx
                .to()
                .is_some_and(|to| gofin_evm::lower_address(to).eq_ignore_ascii_case(SWAP_ROUTER))
            {
                let sender = tx.recover_signer().unwrap();
                assert_eq!(gofin_evm::lower_address(sender), self.wallet);
                let hash = format!("{:#x}", tx.tx_hash());
                let block_hash = format!("0x{}", "44".repeat(32));
                let topic = format!("{:#x}", keccak256("Transfer(address,address,uint256)"));
                let topic_address =
                    |address: &str| format!("0x{:0>64}", address.trim_start_matches("0x"));
                let transfer = |token: &str, from: &str, to: &str, index: u64| json!({"address":token,"topics":[topic,topic_address(from),topic_address(to)],"data":format!("0x{:064x}",1_000_000u64),"transactionHash":hash,"blockNumber":"0x64","blockHash":block_hash,"logIndex":format!("0x{index:x}"),"removed":false});
                let receipt = json!({"transactionHash":hash,"blockNumber":"0x64","blockHash":block_hash,"status":"0x1","gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":[transfer(bridge_fixture::ETH_USDC,&self.wallet,SWAP_ROUTER,0),transfer(OUTPUT_TOKEN,SWAP_ROUTER,&self.wallet,1)]});
                let transaction = json!({"hash":hash,"from":self.wallet,"to":SWAP_ROUTER,"value":"0x0","input":"0x12345678","nonce":format!("0x{:x}",tx.nonce()),"gas":format!("0x{:x}",tx.gas_limit()),"chainId":"0x1","type":"0x2","maxFeePerGas":format!("0x{:x}",tx.max_fee_per_gas()),"maxPriorityFeePerGas":format!("0x{:x}",tx.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":"0x64","blockHash":block_hash});
                *self.transaction.lock().unwrap() = Some((transaction, receipt));
                self.sends.fetch_add(1, Ordering::SeqCst);
                return Ok(json!(hash));
            }
        }
        if matches!(
            method,
            "eth_getTransactionReceipt" | "eth_getTransactionByHash"
        ) && let Some((tx, receipt)) = self.transaction.lock().unwrap().as_ref()
            && params[0] == tx["hash"]
        {
            return Ok(if method == "eth_getTransactionReceipt" {
                receipt.clone()
            } else {
                tx.clone()
            });
        }
        let mut response = self.inner.request(method, params).await?;
        if method == "eth_getBlockByNumber" && response.is_object() {
            response["timestamp"] = json!(format!("0x{:x}", chrono::Utc::now().timestamp()));
        }
        Ok(response)
    }
}
struct Prices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "2000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic reverse fixture".into(),
        })
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic reverse approval".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
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
async fn reverse_close_withdraw_bridge_then_swap_via_public_cli() {
    let temp = tempfile::tempdir().unwrap();
    let signer = Arc::new(
        LocalEnvelopeSigner::from_private_key(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
    );
    let wallet = gofin_evm::lower_address(signer.address());
    let arb = Arc::new(bridge_fixture::Rpc::destination_on(wallet.clone(), 42161));
    *arb.token_balance.lock().unwrap() = U256::ZERO;
    let venue = fixture::Fixture::new(wallet.clone(), arb.clone());
    venue.seed_reverse_position(50_000_000, 100_000);
    let eth = Arc::new(bridge_fixture::Rpc::source_with_allowance(
        wallet.clone(),
        U256::from(10_000_000u64),
    ));
    *eth.token_balance.lock().unwrap() = U256::ZERO;
    let arb_runtime = Arc::new(EvmRuntime::new(venue.clone(), 42161, Some(signer.clone())));
    let swap_rpc = Arc::new(SwapRpc {
        inner: eth.clone(),
        wallet: wallet.clone(),
        transaction: Mutex::new(None),
        sends: AtomicUsize::new(0),
    });
    let eth_runtime = Arc::new(EvmRuntime::new(swap_rpc.clone(), 1, Some(signer)));
    let valuator = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let connection = vec![WalletConnection {
        name: "synthetic-wallet".into(),
        address: wallet.clone(),
        signing_enabled: true,
    }];
    let mut arb_config =
        EthereumConfig::wallet_only(42161, 1, vec![bridge_fixture::ARB_USDC.into()]);
    arb_config.require_finalized = false;
    let mut eth_config = EthereumConfig::wallet_only(1, 1, vec![bridge_fixture::ETH_USDC.into()]);
    eth_config.require_finalized = false;
    let arb_wallet = Arc::new(
        EthereumAdapter::new(arb_config, connection.clone(), arb_runtime.clone())
            .unwrap()
            .with_wallet_valuation(valuator.clone()),
    );
    let swap_provider = Arc::new(SwapProvider {
        wallet: wallet.clone(),
        swaps: AtomicUsize::new(0),
    });
    let eth_wallet = Arc::new(
        EthereumAdapter::new(eth_config, connection, eth_runtime.clone())
            .unwrap()
            .with_wallet_valuation(valuator.clone())
            .with_uniswap_api(UniswapApiClient::with_transport(swap_provider.clone())),
    );
    let wallets = BTreeMap::from([(1, eth_wallet), (42161, arb_wallet)]);
    let provider = Arc::new(bridge_fixture::Erc20Provider::with_destination(
        eth.clone(),
        bridge_fixture::DestinationPayout::DirectWithHistoricalReads,
    ));
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            provider,
            wallets.clone(),
            BTreeMap::from([(1, eth_runtime), (42161, arb_runtime)]),
            valuator.clone(),
        )
        .unwrap(),
    );
    let exchange = Arc::new(gofin_exchange::ExchangeAdapter::new(venue.profile()));
    let approvals = Arc::new(Approval(AtomicUsize::new(0)));
    let runtime = compose_runtime_with_clock(
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
            store: Store::open(temp.path().join("reverse.sqlite")).unwrap(),
            adapters: vec![
                Arc::new(gofin_broker::EvmNetworkRouter::new(wallets)),
                bridge.clone(),
                exchange,
            ],
            authorizer: approvals.clone(),
            valuator,
            bridge: Some(bridge),
        },
        Arc::new(SystemClock),
    )
    .unwrap();
    let client = Client::new(runtime, temp.path()).await;
    let close = json!({"request_id":"reverse-close","account":"synthetic-hl","symbol":"SOL/USDC:USDC","market_type":"perpetual","side":"sell","order_type":"limit","size":"0.1","limit_price":"100","time_in_force":"ioc","reduce_only":true,"max_notional_usdc":"10","max_fee_usdc":"0.1","max_realized_loss_usdc":"0"});
    let close_result = execute(&client, "hyperliquid.orders.place", close.clone()).await;
    assert_eq!(close_result["status"], "completed", "{close_result}");
    assert_eq!(venue.cash().3, 0);
    let withdrawal = json!({"request_id":"reverse-withdraw","account":"synthetic-hl","asset":"USDC","amount":"20","address":wallet,"max_amount_usdc":"20","max_fee_usdc":"0.01"});
    let pending = execute(&client, "hyperliquid.account.withdraw", withdrawal.clone()).await;
    let nonce = pending["submission"]["data"]["envelope"]["nonce_ms"]
        .as_u64()
        .unwrap();
    venue.set_receipt(json!({"hash":format!("0x{:064x}",nonce+1),"recipient":wallet,"token":bridge_fixture::ARB_USDC,"amount":"19.99","status":"0x1","chainId":42161,"sender":"0x2df1c51e09aecf9cacb7bc98cb1742757f163df7"}));
    *arb.token_balance.lock().unwrap() = U256::from(19_990_000u64);
    let id = pending["id"].as_str().unwrap();
    let done = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(done["status"], "completed", "{done}");
    let bridge_input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":42161,"token":bridge_fixture::ARB_USDC},"destinationChainId":1,"destinationAssets":[{"chainId":1,"token":bridge_fixture::ETH_USDC}],"recipient":wallet,"refundTo":wallet,"allowedRefundAssets":[],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64+600,"bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:1","reference":bridge_fixture::ETH_USDC,"decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:42161","reference":"native","decimals":18},"kind":"gas","maximum":"10000000000000000"}]}});
    let request = OperationRequest {
        request_id: "reverse-bridge".into(),
        operation: "bridge.execute".into(),
        input: bridge_input.clone(),
        run_id: None,
    };
    let result = client
        .call(
            args(&[
                "execute",
                "bridge.execute",
                "--input",
                &bridge_input.to_string(),
                "--id",
                "reverse-bridge",
                "--json",
            ]),
            BrokerRequest::Execute {
                request: request.clone(),
            },
            true,
        )
        .await;
    assert!(result["preview"].is_object(), "{result}");
    let bridge_id = result["id"].as_str().unwrap().to_owned();
    let id = bridge_id.as_str();
    let done = client
        .call(
            args(&["status", id, "--side", "destination", "--json"]),
            BrokerRequest::StatusSide {
                id: id.into(),
                side: "destination".into(),
            },
            true,
        )
        .await;
    assert_eq!(done["outcome"]["phase"], "done", "{done}");
    *eth.token_balance.lock().unwrap() = U256::from(9_700_000u64);
    let swap = json!({"wallet":"synthetic-wallet","network":"ethereum","chain_id":1,"request_id":"reverse-swap","token_in":bridge_fixture::ETH_USDC,"token_out":OUTPUT_TOKEN,"amount":"1","recipient":wallet,"min_output":"0.99","slippage_bps":100,"deadline":chrono::Utc::now().timestamp() as u64+300,"max_fee_eth":"0.01","dex":"uniswap","allowed_dexes":["uniswap"]});
    let result = execute(&client, "swap.execute", swap.clone()).await;
    assert_eq!(result["status"], "completed", "{result}");
    assert!(
        result["effects"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| effect["kind"] == "conversion"
                && effect["spent"]["atoms"] == "1000000"
                && effect["received"]["atoms"] == "1000000")
    );
    assert_eq!(swap_rpc.sends.load(Ordering::SeqCst), 1);
    assert_eq!(swap_provider.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 4);
    assert_eq!(venue.native_dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(arb.broadcasts(), 2);
    for (operation, id, input, original) in [
        (
            "hyperliquid.orders.place",
            "reverse-close",
            close,
            close_result["id"].as_str().unwrap(),
        ),
        (
            "hyperliquid.account.withdraw",
            "reverse-withdraw",
            withdrawal,
            pending["id"].as_str().unwrap(),
        ),
        (
            "bridge.execute",
            "reverse-bridge",
            bridge_input,
            bridge_id.as_str(),
        ),
        (
            "swap.execute",
            "reverse-swap",
            swap,
            result["id"].as_str().unwrap(),
        ),
    ] {
        let duplicate = client
            .call(
                args(&[
                    "execute",
                    operation,
                    "--input",
                    &input.to_string(),
                    "--id",
                    id,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: id.into(),
                        operation: operation.into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(duplicate["id"], original);
    }
    assert_eq!(approvals.0.load(Ordering::SeqCst), 4);
    assert_eq!(venue.native_dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(arb.broadcasts(), 2);
    assert_eq!(swap_rpc.sends.load(Ordering::SeqCst), 1);
}
