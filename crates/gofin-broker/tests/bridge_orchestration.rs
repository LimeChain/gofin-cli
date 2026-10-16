//! Public broker/engine composition with synthetic, in-memory protocol and RPC.
//! No network, funded keys, or real authorization; this is lifecycle evidence only.
use async_trait::async_trait;
use gofin_bridge::*;
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer,
    BridgeStepKind as DomainStepKind, OperationRequest, Quantity, Result, Status,
    boundaries::BoundaryAuthorizationRequest, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner,
    alloy::{
        consensus::{Transaction, TxEnvelope},
        eips::eip2718::Decodable2718,
        primitives::{U256, keccak256},
    },
    lower_address,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "support/bridge_provider.rs"]
mod bridge_provider;
use bridge_provider::SyntheticProvider;
const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const ROUTER: &str = "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f";
const SOLVER: &str = "0xf70da97812cb96acdf810712aa562db8dfa3dbef";
fn q(s: &str) -> Quantity {
    Quantity::new(s).unwrap()
}
fn word_address(a: &str) -> String {
    format!("{:0>64}", &a[2..])
}
fn word(n: U256) -> String {
    format!("{n:064x}")
}
fn block_hash() -> String {
    format!("0x{}", "44".repeat(32))
}
fn omit_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for value in map.values_mut() {
                omit_nulls(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                omit_nulls(value);
            }
        }
        _ => {}
    }
}
fn tx_hash() -> String {
    format!("0x{}", "77".repeat(32))
}
fn request_id() -> String {
    format!("0x{}", "55".repeat(32))
}

struct FixedPrices {
    current: Mutex<String>,
    calls: AtomicUsize,
    switch_at: AtomicUsize,
}
#[async_trait]
impl UsdPriceSource for FixedPrices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: {
                let count = self.calls.fetch_add(1, Ordering::SeqCst);
                if count >= self.switch_at.load(Ordering::SeqCst) {
                    "2100".into()
                } else {
                    self.current.lock().unwrap().clone()
                }
            },
            updated_at: now(),
            observed_at: now(),
            source: "synthetic bridge fixture".into(),
        })
    }
}
#[derive(Default)]
struct CountingApproval(AtomicUsize);
#[async_trait]
impl Authorizer for CountingApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic fixture only".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Rpc {
    chain: u64,
    sender: String,
    sent: Mutex<BTreeMap<String, (Value, Value)>>,
    broadcasts: AtomicUsize,
    destination_sender: Mutex<String>,
}
impl Rpc {
    fn new(chain: u64, sender: String) -> Self {
        Self {
            chain,
            sender,
            sent: Mutex::new(BTreeMap::new()),
            broadcasts: AtomicUsize::new(0),
            destination_sender: Mutex::new(SOLVER.into()),
        }
    }
    fn log(&self, address: &str, signature: &str, data: String, hash: &str) -> Value {
        json!({"address":address,"topics":[format!("{:#x}",keccak256(signature))],"data":format!("0x{data}"),"logIndex":"0x0","transactionHash":hash,"blockNumber":"0x64","blockHash":block_hash(),"removed":false})
    }
    fn destination(&self) -> (Value, Value) {
        let data = format!(
            "{}{}{}{}{}{}{}",
            word_address(ROUTER),
            word_address(&self.sender),
            word(U256::ZERO),
            word(U256::from(4_490_000_000_000_000u64)),
            word(U256::from(160)),
            word(U256::from(32)),
            &request_id()[2..]
        );
        let log = self.log(
            ROUTER,
            "FundsMovement(address,address,address,uint256,bytes)",
            data,
            &tx_hash(),
        );
        (
            json!({"hash":tx_hash(),"from":*self.destination_sender.lock().unwrap(),"to":ROUTER,"blockNumber":"0x64","blockHash":block_hash()}),
            json!({"transactionHash":tx_hash(),"blockNumber":"0x64","blockHash":block_hash(),"status":"0x1","gasUsed":"0x7d00","effectiveGasPrice":"0x3b9aca00","logs":[log]}),
        )
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, p: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain)),
            "eth_getCode" => json!("0x60006000"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_call" => json!("0x"),
            "eth_estimateGas" => json!("0x7530"),
            "eth_getTransactionCount" => {
                json!(format!("{:#x}", self.broadcasts.load(Ordering::SeqCst)))
            }
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_blockNumber" => json!("0x64"),
            "eth_getBlockByNumber" => {
                if self.chain == 42161 && p[0] == "finalized" {
                    // Destination settlement must work at its sequenced head.
                    Value::Null
                } else {
                    json!({"hash":block_hash(),"number":"0x64","baseFeePerGas":"0x3b9aca00"})
                }
            }
            "eth_sendRawTransaction" => {
                assert_eq!(self.chain, 1, "destination observer cannot sign");
                let raw = hex::decode(p[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                let hash = format!("{:#x}", keccak256(&raw));
                let data = format!(
                    "{}{}{}",
                    word_address(&self.sender),
                    word(tx.value()),
                    hex::encode(&tx.input()[tx.input().len() - 32..])
                );
                let log = self.log(
                    DEPOSITORY,
                    "RelayNativeDeposit(address,uint256,bytes32)",
                    data,
                    &hash,
                );
                let reported = json!({"hash":hash,"from":self.sender,"to":lower_address(tx.to().unwrap()),"value":format!("{:#x}",tx.value()),"input":format!("0x{}",hex::encode(tx.input())),"nonce":format!("{:#x}",tx.nonce()),"gas":format!("{:#x}",tx.gas_limit()),"chainId":"0x1","type":"0x2","maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),"maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":"0x64","blockHash":block_hash()});
                let receipt = json!({"transactionHash":hash,"blockNumber":"0x64","blockHash":block_hash(),"status":"0x1","gasUsed":"0x7d00","effectiveGasPrice":"0x3b9aca00","logs":[log]});
                self.sent
                    .lock()
                    .unwrap()
                    .insert(hash.clone(), (reported, receipt));
                self.broadcasts.fetch_add(1, Ordering::SeqCst);
                json!(hash)
            }
            "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                let pair = if self.chain == 42161 && p[0] == json!(tx_hash()) {
                    Some(self.destination())
                } else {
                    self.sent
                        .lock()
                        .unwrap()
                        .get(p[0].as_str().unwrap())
                        .cloned()
                };
                pair.map(|(tx, r)| {
                    if method == "eth_getTransactionReceipt" {
                        r
                    } else {
                        tx
                    }
                })
                .unwrap_or(Value::Null)
            }
            other => panic!("unimplemented synthetic RPC {other}: {p}"),
        })
    }
}
struct Harness {
    _tmp: tempfile::TempDir,
    adapter: Arc<BridgeAdapter>,
    engine: Arc<Engine>,
    state: BridgeStateRepository,
    source: Arc<Rpc>,
    destination: Arc<Rpc>,
    approval: Arc<CountingApproval>,
    address: String,
    prices: Arc<FixedPrices>,
}
impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("journal.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(store.clone());
        // Public deterministic fixture. No transport in this test can contact a network.
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let address = lower_address(signer.address());
        let source = Arc::new(Rpc::new(1, address.clone()));
        let destination = Arc::new(Rpc::new(42161, address.clone()));
        let source_runtime = Arc::new(EvmRuntime::new(source.clone(), 1, Some(signer)));
        let dest_runtime = Arc::new(EvmRuntime::new(destination.clone(), 42161, None));
        let source_prices = Arc::new(FixedPrices {
            current: Mutex::new("2000".into()),
            calls: AtomicUsize::new(0),
            switch_at: AtomicUsize::new(usize::MAX),
        });
        let prices = Arc::new(SharedUsdValuator::with_source(source_prices.clone()));
        let wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: address.clone(),
                    signing_enabled: true,
                }],
                source_runtime.clone(),
            )
            .unwrap()
            .with_wallet_valuation(prices.clone()),
        );
        let dest_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(42161, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: address.clone(),
                    signing_enabled: false,
                }],
                dest_runtime.clone(),
            )
            .unwrap(),
        );
        let adapter = Arc::new(
            BridgeAdapter::with_provider_and_observers(
                Arc::new(SyntheticProvider {
                    revisions: AtomicUsize::new(0),
                }),
                BTreeMap::from([(1, wallet.clone()), (42161, dest_wallet)]),
                BTreeMap::from([(1, source_runtime), (42161, dest_runtime)]),
                prices,
            )
            .unwrap(),
        );
        adapter.attach_state(state.clone()).unwrap();
        let approval = Arc::new(CountingApproval::default());
        let engine = Arc::new(
            Engine::new_with_shared_store(
                store,
                vec![wallet, adapter.clone()],
                approval.clone(),
                EngineOptions {
                    max_movement_and_cost_usd_micros: "100000000".into(),
                    max_run_allocation_usd_micros: "200000000".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap(),
        );
        adapter.attach_engine(Arc::downgrade(&engine)).unwrap();
        Self {
            _tmp: tmp,
            adapter,
            engine,
            state,
            source,
            destination,
            approval,
            address,
            prices: source_prices,
        }
    }
    fn quote_input(&self, amount: &str, refresh: Option<String>) -> BridgeQuoteInput {
        BridgeQuoteInput {
            provider: None,
            cctp: None,
            intent: BridgeQuoteIntentInput {
                wallet: "synthetic-wallet".into(),
                source: BridgeAssetInput {
                    chain_id: 1,
                    token: None,
                },
                solana_destination: None,
                destination_chain_id: Some(42161),
                destination_assets: vec![BridgeAssetInput {
                    chain_id: 42161,
                    token: None,
                }],
                recipient: self.address.clone(),
                refund_to: self.address.clone(),
                allowed_refund_assets: vec![BridgeAssetInput {
                    chain_id: 1,
                    token: None,
                }],
                amount: q(amount),
            },
            bounds: BridgeBounds::default(),
            refresh_transfer_id: refresh,
        }
    }
    fn bounds(&self) -> BridgeBounds {
        BridgeBounds {
            input: Some(AmountInterval {
                minimum: q("4000000000000000"),
                maximum: q("6000000000000000"),
            }),
            maximum_source_debit: Some(q("6000000000000000")),
            input_usd_micros: Some(AmountInterval {
                minimum: q("7500000"),
                maximum: q("13500000"),
            }),
            source_usd_per_unit_micros: Some(AmountInterval {
                minimum: q("1800000000"),
                maximum: q("2200000000"),
            }),
            maximum_total_usd_micros: Some(14_000_000),
            minimum_net_receipt: Some(
                EvmAsset {
                    chain_id: 42161,
                    token: None,
                    decimals: 18,
                }
                .amount(q("3900000000000000")),
            ),
            fee_caps: vec![FeeCap {
                asset: EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                kind: FeeKind::Gas,
                maximum: q("200000000000000"),
                maximum_usd_micros: None,
            }],
            ..Default::default()
        }
    }
    async fn quote(&self) -> (String, String) {
        let out = self
            .adapter
            .read(
                "bridge.quotes",
                &serde_json::to_value(self.quote_input("5000000000000000", None)).unwrap(),
            )
            .await
            .unwrap();
        (
            out["transfers"][0]["transfer_id"].as_str().unwrap().into(),
            out["transfers"][0]["steps"][0]["step_id"]
                .as_str()
                .unwrap()
                .into(),
        )
    }
    fn step_input(&self, t: &str, s: &str) -> Value {
        let mut value = serde_json::to_value(BridgeTransferInput {
            transfer_id: t.into(),
            step_id: s.into(),
            step_kind: DomainStepKind::Deposit,
            bounds: Some(self.bounds()),
        })
        .unwrap();
        omit_nulls(&mut value);
        value
    }
    async fn grant(&self, t: &str, s: &str) -> String {
        let mut bounds = serde_json::to_value(self.bounds()).unwrap();
        omit_nulls(&mut bounds);
        self.engine
            .authorize_boundary(BoundaryAuthorizationRequest {
                request_id: format!("approve-{t}"),
                operation: "bridge.transfer".into(),
                input: self.step_input(t, s),
                bounds,
                valid_for_seconds: 600,
                replaces: None,
            })
            .await
            .unwrap()
            .id
    }
}
#[tokio::test]
async fn bounded_refresh_source_settlement_and_duplicate_dispatch() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let h = Harness::new();
        let (transfer, step) = h.quote().await;
        assert!(
            h.adapter.snapshot().await.unwrap().complete,
            "read-only options are not pending funds"
        );
        let grant = h.grant(&transfer, &step).await;
        let refreshed = h
            .adapter
            .read(
                "bridge.quotes",
                &serde_json::to_value(h.quote_input("4500000000000000", Some(transfer.clone())))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refreshed["transfers"][0]["steps"][0]["step_id"], step);
        let outside = h
            .adapter
            .read(
                "bridge.quotes",
                &serde_json::to_value(h.quote_input("7000000000000000", Some(transfer.clone())))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(outside.code, "BRIDGE_REFRESH_AMOUNT_OUT_OF_BOUNDS");
        // Change the fresh quote during execution, after its initial preview.
        h.prices
            .switch_at
            .store(h.prices.calls.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
        let request = OperationRequest {
            request_id: "source-attempt".into(),
            operation: "bridge.transfer".into(),
            input: h.step_input(&transfer, &step),
            run_id: None,
        };
        let op = h
            .engine
            .execute_boundary(&grant, request.clone())
            .await
            .unwrap();
        assert_eq!(op.status, Status::Completed, "error={:?}", op.error);
        assert_eq!(
            h.engine.execute_boundary(&grant, request).await.unwrap().id,
            op.id
        );
        assert_eq!(h.source.broadcasts.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            1,
            "quote refresh needs no repeated approval"
        );
        let pending = h.state.transfer(&transfer).unwrap().unwrap();
        assert!(pending.transit.is_some());
        assert_ne!(pending.state, gofin_domain::BridgeTransferState::Completed);
        *h.destination.destination_sender.lock().unwrap() = h.address.clone();
        let pending = h
            .adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_ne!(
            pending["state"], "completed",
            "unrelated user payment cannot settle a Relay transfer"
        );
        *h.destination.destination_sender.lock().unwrap() = SOLVER.into();
        let settled = h
            .adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(settled["state"], "completed", "{settled}");
        let repeated = h
            .adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(repeated["state"], "completed");
        assert_eq!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .applied_proofs
                .len(),
            2
        );
    })
    .await
    .expect("broker lifecycle must not deadlock its shared store");
}

#[tokio::test]
async fn exact_step_approval_leaves_source_wallet_available_during_transit() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let h = Harness::new();
        let (transfer, step) = h.quote().await;
        let request = OperationRequest {
            request_id: "exact-source-attempt".into(),
            operation: "bridge.transfer".into(),
            input: h.step_input(&transfer, &step),
            run_id: None,
        };
        let first = h.engine.execute(request.clone()).await.unwrap();
        assert_eq!(first.status, Status::Completed, "error={:?}", first.error);
        assert_eq!(h.engine.execute(request).await.unwrap().id, first.id);
        assert!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .transit
                .is_some()
        );
        let (second_transfer, second_step) = h.quote().await;
        let second = h
            .engine
            .execute(OperationRequest {
                request_id: "second-exact-source-attempt".into(),
                operation: "bridge.transfer".into(),
                input: h.step_input(&second_transfer, &second_step),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(second.status, Status::Completed, "error={:?}", second.error);
        assert_eq!(h.source.broadcasts.load(Ordering::SeqCst), 2);
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 2);
        assert!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .transit
                .is_some()
        );
        assert!(
            h.state
                .transfer(&second_transfer)
                .unwrap()
                .unwrap()
                .transit
                .is_some()
        );
        // Ordinary exact operations retain the approved conservative fee charge.
        let expected: u64 = [&first, &second]
            .iter()
            .map(|record| {
                let valuation = record.preview.as_ref().unwrap().valuation.as_ref().unwrap();
                valuation
                    .movement_usd_micros
                    .as_str()
                    .parse::<u64>()
                    .unwrap()
                    + valuation
                        .maximum_fee_usd_micros
                        .as_str()
                        .parse::<u64>()
                        .unwrap()
            })
            .sum();
        assert_eq!(
            h.engine.allocation_used_usd_micros().unwrap(),
            expected.to_string(),
            "source principal and gas count exactly once per transfer"
        );
    })
    .await
    .expect("multiple transfer steps must not deadlock or retain a settled wallet lock");
}
