//! End-to-end Canonical Inbox lifecycle using only deterministic in-memory RPC.
//! It exercises quote aggregation, source envelope+event proof, durable transit
//! locator, exact type-0x64 child transaction, finality, idempotency and restart.
use async_trait::async_trait;
use fin_bridge::{
    BridgeAssetInput, BridgeQuoteInput, BridgeQuoteIntentInput, BridgeTransferInput,
    canonical::{CanonicalConfig, derive_eth_deposit_tx_hash},
};
use fin_broker::{BridgeAdapter, SharedUsdValuator};
use fin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, FinError,
    OperationRequest, Quantity, Result, Status, now,
};
use fin_engine::{Engine, EngineOptions};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use fin_evm::alloy::consensus::transaction::SignerRecoverable;
use fin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner,
    alloy::{
        consensus::{Transaction, TxEnvelope},
        eips::eip2718::Decodable2718,
        primitives::{Address, U256, keccak256},
    },
    lower_address,
};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_store::{BridgeStateRepository, SharedStore, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

fn q(value: &str) -> Quantity {
    Quantity::new(value).unwrap()
}

fn safe_fork_operation_evidence(operation: &fin_domain::OperationRecord) -> Value {
    const SAFE_KEYS: &[&str] = &[
        "chain_id",
        "transaction_hash",
        "block_number",
        "block_hash",
        "receipt_status",
        "receipt_scope",
        "evidence_id",
        "log_index",
        "correlation",
        "source_address",
        "recipient",
        "amount_atoms",
        "message_index",
        "destination_transaction_hash",
    ];
    let mut output = serde_json::Map::new();
    for (name, source) in [
        ("effects", operation.effects.as_ref().map(|v| &v.evidence)),
        (
            "reconciliation",
            operation.last_reconciliation_evidence.as_ref(),
        ),
    ] {
        if let Some(Value::Object(fields)) = source {
            let mut safe = serde_json::Map::new();
            for key in SAFE_KEYS {
                if let Some(value) = fields.get(*key)
                    && (value.is_string() || value.is_number() || value.is_boolean())
                {
                    safe.insert((*key).to_owned(), value.clone());
                }
            }
            if !safe.is_empty() {
                output.insert(name.to_owned(), Value::Object(safe));
            }
        }
    }
    Value::Object(output)
}

fn safe_fork_quote_diagnostic(quote_result: &Value) -> Value {
    let failures = quote_result["failures"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|failure| {
            json!({
                "provider": failure["provider"],
            "destination_chain_id": failure["destination_asset"]["chain_id"],
            "error_code": failure["error"]["code"],
            "error_stage": failure["error"]["stage"],
            "safe_reason": match (
                failure["error"]["code"].as_str(),
                failure["error"]["stage"].as_str(),
            ) {
                (Some("BRIDGE_BOUNDS_VIOLATED"), Some("prepare")) => {
                    "The Canonical source gas estimate exceeded the selected fee cap."
                }
                _ => "The provider rejected this quote; inspect the code and stage.",
            },
            })
        })
        .collect::<Vec<_>>();
    json!({
        "coverage": {
            "providers": quote_result["coverage"]["providers"],
            "compared_providers": quote_result["coverage"]["compared_providers"],
            "complete": quote_result["coverage"]["complete"],
        },
        "transfer_count": quote_result["transfers"].as_array().map_or(0, Vec::len),
        "failures": failures,
    })
}
fn word(value: U256) -> String {
    format!("{value:064x}")
}
fn address_word(value: &str) -> String {
    format!("{:0>64}", &value[2..])
}
fn block_hash() -> String {
    format!("0x{}", "44".repeat(32))
}
#[derive(Default)]
struct SharedChain {
    source: BTreeMap<String, (Value, Value)>,
    child: BTreeMap<String, (Value, Value)>,
    broadcasts: usize,
    child_sender: Option<String>,
}

struct Rpc {
    chain_id: u64,
    destination_chain: u64,
    state: Arc<Mutex<SharedChain>>,
}
impl Rpc {
    fn block(&self, number: &str) -> Value {
        json!({"hash":block_hash(),"number":number,"baseFeePerGas":"0x3b9aca00"})
    }
    fn source_logs(
        &self,
        inbox: &str,
        bridge: &str,
        sender: &str,
        recipient: &str,
        amount: U256,
    ) -> Vec<Value> {
        let mut packed = Vec::new();
        packed.extend_from_slice(
            Address::from_slice(&hex::decode(&recipient[2..]).unwrap()).as_slice(),
        );
        packed.extend_from_slice(&amount.to_be_bytes::<32>());
        let mut inbox_data = Vec::new();
        inbox_data.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
        inbox_data.extend_from_slice(&U256::from(52).to_be_bytes::<32>());
        inbox_data.extend_from_slice(&packed);
        inbox_data.extend_from_slice(&[0; 12]);
        let index = U256::from(1u8);
        let inbox_log = json!({
            "address":inbox,
            "topics":[format!("{:#x}",keccak256("InboxMessageDelivered(uint256,bytes)")),format!("0x{}",word(index))],
            "data":format!("0x{}",hex::encode(inbox_data)),"logIndex":"0x0","removed":false
        });
        // Canonical L1-to-L2 alias offset, modulo 2^160.
        let source = U256::from_be_slice(&hex::decode(&sender[2..]).unwrap());
        let offset =
            U256::from_be_slice(&hex::decode("1111000000000000000000000000000000001111").unwrap());
        let alias = format!(
            "0x{:040x}",
            source.overflowing_add(offset).0
                & U256::from_limbs([u64::MAX, u64::MAX, u64::MAX, 0xffff])
        );
        let message_hash = keccak256(&packed);
        let bridge_data = format!(
            "{}{}{}{}{}{}",
            address_word(inbox),
            word(U256::from(12)),
            address_word(&alias),
            hex::encode(message_hash),
            word(U256::from(1)),
            word(U256::from(2))
        );
        let bridge_log = json!({
            "address":bridge,
            "topics":[format!("{:#x}",keccak256("MessageDelivered(uint256,bytes32,address,uint8,address,bytes32,uint256,uint64)")),format!("0x{}",word(index)),format!("0x{}","55".repeat(32))],
            "data":format!("0x{bridge_data}"),"logIndex":"0x1","removed":false
        });
        let logs = vec![inbox_log, bridge_log];
        let pair = fin_bridge::canonical::CanonicalDepositReceipt {
            inbox_log: fin_bridge::canonical::CanonicalRawLog {
                log_index: 0,
                address: logs[0]["address"].as_str().unwrap().into(),
                topics: logs[0]["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().into())
                    .collect(),
                data: logs[0]["data"].as_str().unwrap().into(),
            },
            bridge_log: fin_bridge::canonical::CanonicalRawLog {
                log_index: 1,
                address: logs[1]["address"].as_str().unwrap().into(),
                topics: logs[1]["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().into())
                    .collect(),
                data: logs[1]["data"].as_str().unwrap().into(),
            },
        };
        let deployment = &CanonicalConfig::reviewed().deployments[&self.destination_chain];
        fin_bridge::canonical::verify_eth_deposit_source(
            &pair,
            &fin_bridge::canonical::ExpectedEthDeposit {
                source_address: sender.into(),
                recipient: recipient.into(),
                amount: q(&amount.to_string()),
                destination_chain_id: self.destination_chain,
                inbox: deployment.inbox.clone(),
                bridge: deployment.bridge.clone(),
            },
        )
        .unwrap_or_else(|e| panic!("synthetic paired source logs are invalid: {e:?}"));
        logs
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut state = self.state.lock().unwrap();
        let answer = match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain_id)),
            "eth_call" => json!("0x"),
            "eth_estimateGas" => json!("0x7530"),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_getTransactionCount" => json!(format!("{:#x}", state.broadcasts)),
            "eth_getBalance" => json!(format!("{:#x}", U256::from(100_000_000_000_000_000u128))),
            "eth_getCode" => {
                let inbox = CanonicalConfig::reviewed().deployments[&self.destination_chain]
                    .inbox
                    .clone();
                if params[0]
                    .as_str()
                    .is_some_and(|address| address.eq_ignore_ascii_case(&inbox))
                {
                    json!("0x60006000")
                } else {
                    json!("0x")
                }
            }
            "eth_blockNumber" => json!("0x66"),
            "eth_getBlockByNumber" => {
                if self.chain_id != 1 && params[0] == json!("finalized") {
                    // Both reviewed child chains settle from sequencer inclusion.
                    Value::Null
                } else if params[0] == json!("finalized") {
                    self.block("0x66")
                } else {
                    self.block(params[0].as_str().unwrap_or("0x64"))
                }
            }
            "eth_sendRawTransaction" => {
                assert_eq!(
                    self.chain_id, 1,
                    "only the Ethereum wallet submits a transaction"
                );
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let envelope = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
                let tx_hash = format!("{:#x}", keccak256(&raw));
                let to = lower_address(envelope.to().unwrap());
                let sender = lower_address(envelope.recover_signer().unwrap());
                let amount = envelope.value();
                let deployment =
                    CanonicalConfig::reviewed().deployments[&self.destination_chain].clone();
                let input = format!("0x{}", hex::encode(envelope.input()));
                let reported = json!({
                    "hash":tx_hash,"from":sender,"to":to,"value":format!("{:#x}",amount),"input":input,
                    "nonce":format!("{:#x}",envelope.nonce()),"gas":format!("{:#x}",envelope.gas_limit()),
                    "chainId":"0x1","type":"0x2","maxFeePerGas":format!("{:#x}",envelope.max_fee_per_gas()),
                    "maxPriorityFeePerGas":format!("{:#x}",envelope.max_priority_fee_per_gas().unwrap()),"accessList":[],
                    "blockNumber":"0x64","blockHash":block_hash()
                });
                let logs = self.source_logs(
                    &deployment.inbox,
                    &deployment.bridge,
                    &sender,
                    &sender,
                    amount,
                );
                let receipt = json!({"transactionHash":tx_hash,"blockNumber":"0x64","blockHash":block_hash(),"status":"0x1","gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":logs});
                state.source.insert(tx_hash.clone(), (reported, receipt));
                state.broadcasts += 1;
                // Derive and stage the corresponding system transaction exactly as the protocol does.
                let from = Address::from_slice(&hex::decode(&sender[2..]).unwrap());
                let alias_offset = U256::from_be_slice(
                    &hex::decode("1111000000000000000000000000000000001111").unwrap(),
                );
                let alias = Address::from_slice(
                    (U256::from_be_slice(from.as_slice())
                        .overflowing_add(alias_offset)
                        .0
                        & U256::from_limbs([u64::MAX, u64::MAX, u64::MAX, 0xffff]))
                    .to_be_bytes::<32>()[12..]
                        .as_ref(),
                );
                let child_hash = format!(
                    "{:#x}",
                    derive_eth_deposit_tx_hash(
                        self.destination_chain,
                        U256::from(1),
                        alias,
                        from,
                        amount
                    )
                );
                let child_sender = state
                    .child_sender
                    .clone()
                    .unwrap_or_else(|| format!("{alias:#x}"));
                let child_tx = json!({"hash":child_hash,"chainId":format!("0x{:x}",self.destination_chain),"type":"0x64","from":child_sender,"to":sender,"value":format!("{:#x}",amount),"blockNumber":"0x64","blockHash":block_hash()});
                let child_receipt = json!({"transactionHash":child_hash,"blockNumber":"0x64","blockHash":block_hash(),"status":"0x1","gasUsed":"0x0","effectiveGasPrice":"0x0","logs":[]});
                state.child.insert(child_hash, (child_tx, child_receipt));
                json!(tx_hash)
            }
            "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                let hash = params[0].as_str().unwrap_or("");
                if let Some((tx, receipt)) = state.source.get(hash) {
                    if method == "eth_getTransactionReceipt" {
                        receipt.clone()
                    } else {
                        tx.clone()
                    }
                } else if let Some((tx, receipt)) = state.child.get(hash) {
                    if method == "eth_getTransactionReceipt" {
                        receipt.clone()
                    } else {
                        let mut tx = tx.clone();
                        if let Some(sender) = &state.child_sender {
                            tx["from"] = json!(sender);
                        }
                        tx
                    }
                } else {
                    Value::Null
                }
            }
            other => panic!("unexpected synthetic RPC {other}: {params}"),
        };
        Ok(answer)
    }
}

struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: "2000".into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic canonical fixture".into(),
        })
    }
}
#[derive(Default)]
struct Approvals(AtomicUsize);
#[async_trait]
impl Authorizer for Approvals {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic canonical test".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[test]
fn fork_quote_failure_diagnostic_is_code_only() {
    let source = json!({
        "coverage":{"providers":["arbitrum_canonical"],"compared_providers":1,"complete":false},
        "transfers":[],
        "failures":[{
            "provider":"arbitrum_canonical",
            "destination_asset":{"chain_id":42161},
            "error":{"code":"BRIDGE_FEE_CAP_REQUIRED","stage":"validation","message":"private fixture detail"}
        }]
    });
    let diagnostic = safe_fork_quote_diagnostic(&source);
    assert_eq!(
        diagnostic["failures"][0]["error_code"],
        "BRIDGE_FEE_CAP_REQUIRED"
    );
    assert_eq!(diagnostic["transfer_count"], 0);
    assert!(
        !serde_json::to_string(&diagnostic)
            .unwrap()
            .contains("private fixture detail")
    );
}

struct Harness {
    _tmp: tempfile::TempDir,
    adapter: Arc<BridgeAdapter>,
    engine: Arc<Engine>,
    state: BridgeStateRepository,
    chain: Arc<Mutex<SharedChain>>,
    approvals: Arc<Approvals>,
    address: String,
    destination: u64,
    shared: SharedStore,
    valuator: Arc<SharedUsdValuator>,
}
impl Harness {
    fn new(destination: u64) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("canonical.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(store.clone());
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let address = lower_address(signer.address());
        let chain = Arc::new(Mutex::new(SharedChain::default()));
        let source_rpc = Arc::new(Rpc {
            chain_id: 1,
            destination_chain: destination,
            state: chain.clone(),
        });
        let child_rpc = Arc::new(Rpc {
            chain_id: destination,
            destination_chain: destination,
            state: chain.clone(),
        });
        let source_runtime = Arc::new(EvmRuntime::new(source_rpc, 1, Some(signer)));
        let child_runtime = Arc::new(EvmRuntime::new(child_rpc, destination, None));
        let mut wallet_config = EthereumConfig::wallet_only(1, 1, vec![]);
        wallet_config.require_finalized = false;
        let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
        let wallet = Arc::new(
            EthereumAdapter::new(
                wallet_config,
                vec![WalletConnection {
                    name: "wallet".into(),
                    address: address.clone(),
                    signing_enabled: true,
                }],
                source_runtime.clone(),
            )
            .unwrap()
            .with_wallet_valuation(prices.clone()),
        );
        let canonical = Arc::new(
            fin_bridge::canonical::CanonicalProvider::new(
                Arc::new(EvmRuntime::new(source_runtime.rpc.clone(), 1, None)),
                CanonicalConfig::reviewed(),
            )
            .unwrap(),
        );
        let adapter = Arc::new(
            BridgeAdapter::with_providers(
                vec![canonical],
                BTreeMap::from([(1, wallet.clone())]),
                BTreeMap::from([(1, source_runtime), (destination, child_runtime)]),
                prices.clone(),
            )
            .unwrap(),
        );
        adapter.attach_state(state.clone()).unwrap();
        let approvals = Arc::new(Approvals::default());
        let engine = Arc::new(
            Engine::new_with_shared_store(
                store.clone(),
                vec![wallet, adapter.clone()],
                approvals.clone(),
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
            chain,
            approvals,
            address,
            destination,
            shared: store,
            valuator: prices,
        }
    }
    fn quote_input(&self, amount: &str, refresh: Option<String>) -> BridgeQuoteInput {
        BridgeQuoteInput {
            intent: BridgeQuoteIntentInput {
                wallet: "wallet".into(),
                source: BridgeAssetInput {
                    chain_id: 1,
                    token: None,
                },
                destination_chain_id: self.destination,
                destination_assets: vec![BridgeAssetInput {
                    chain_id: self.destination,
                    token: None,
                }],
                recipient: self.address.clone(),
                refund_to: self.address.clone(),
                allowed_refund_assets: vec![],
                amount: q(amount),
            },
            bounds: self.bounds(),
            refresh_transfer_id: refresh,
        }
    }
    async fn quote(&self, amount: &str, refresh: Option<String>) -> (String, String) {
        self.quote_with_bounds(amount, refresh, self.bounds()).await
    }
    async fn quote_with_bounds(
        &self,
        amount: &str,
        refresh: Option<String>,
        bounds: fin_bridge::BridgeBounds,
    ) -> (String, String) {
        let mut input = self.quote_input(amount, refresh);
        input.bounds = bounds;
        let value = self
            .adapter
            .read("bridge.quotes", &serde_json::to_value(input).unwrap())
            .await
            .unwrap();
        let priced_fees = value["transfers"][0]["priced_fees"]
            .as_array()
            .unwrap_or_else(|| panic!("canonical quote omitted Fin-priced fees: {value}"));
        assert!(!priced_fees.is_empty());
        assert!(
            priced_fees
                .iter()
                .all(|fee| fee["usd_micros"].as_u64().is_some())
        );
        (
            value["transfers"][0]["transfer_id"]
                .as_str()
                .unwrap_or_else(|| panic!("canonical quote failed: {value}"))
                .into(),
            value["transfers"][0]["steps"][0]["step_id"]
                .as_str()
                .unwrap()
                .into(),
        )
    }
    fn transfer_input(&self, t: &str, s: &str) -> Value {
        serde_json::to_value(BridgeTransferInput {
            transfer_id: t.into(),
            step_id: s.into(),
            step_kind: fin_domain::BridgeStepKind::Deposit,
            bounds: Some(self.bounds()),
        })
        .unwrap()
    }
    fn bounds(&self) -> fin_bridge::BridgeBounds {
        fin_bridge::BridgeBounds {
            input: Some(fin_bridge::AmountInterval {
                minimum: q("4000000000000000"),
                maximum: q("6000000000000000"),
            }),
            maximum_source_debit: Some(q("6000000000000000")),
            input_usd_micros: Some(fin_bridge::AmountInterval {
                minimum: q("8000000"),
                maximum: q("12000000"),
            }),
            source_usd_per_unit_micros: Some(fin_bridge::AmountInterval {
                minimum: q("1800000000"),
                maximum: q("2200000000"),
            }),
            maximum_total_usd_micros: Some(14_000_000),
            minimum_net_receipt: Some(
                fin_bridge::EvmAsset {
                    chain_id: self.destination,
                    token: None,
                    decimals: 18,
                }
                .amount(q("4000000000000000")),
            ),
            fee_caps: vec![fin_bridge::FeeCap {
                asset: fin_bridge::EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                kind: fin_bridge::FeeKind::Gas,
                maximum: q("200000000000000"),
                maximum_usd_micros: None,
            }],
            ..Default::default()
        }
    }
    async fn grant(&self, t: &str, s: &str) -> String {
        let mut bounds = serde_json::to_value(self.bounds()).unwrap();
        fn strip(v: &mut Value) {
            match v {
                Value::Object(m) => {
                    m.retain(|_, x| !x.is_null());
                    for x in m.values_mut() {
                        strip(x)
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        strip(x)
                    }
                }
                _ => {}
            }
        }
        strip(&mut bounds);
        self.engine
            .authorize_boundary(fin_domain::boundaries::BoundaryAuthorizationRequest {
                request_id: format!("authorize-{t}"),
                operation: "bridge.transfer".into(),
                input: self.transfer_input(t, s),
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
async fn canonical_source_locator_child_settlement_and_recovery_work_on_both_children() {
    for destination in [42161, 4663] {
        let h = Harness::new(destination);
        // The quote is only a read hint; the later grant selects explicit
        // bounds and must freeze those independently for refresh/execute.
        let (transfer, step) = h
            .quote_with_bounds(
                "5000000000000000",
                None,
                fin_bridge::BridgeBounds::default(),
            )
            .await;
        let grant = h.grant(&transfer, &step).await;
        let outside = h
            .adapter
            .read(
                "bridge.quotes",
                &serde_json::to_value(h.quote_input("7000000000000000", Some(transfer.clone())))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(outside.code, "BRIDGE_BOUNDS_VIOLATED");
        let refreshed = h.quote("4500000000000000", Some(transfer.clone())).await;
        assert_eq!(refreshed.0, transfer);
        assert_eq!(
            refreshed.1, step,
            "in-bounds refresh preserves the declared step id"
        );
        let input = h.transfer_input(&transfer, &step);
        let request = OperationRequest {
            request_id: format!("canonical-source-{destination}"),
            operation: "bridge.transfer".into(),
            input: input.clone(),
            run_id: None,
        };
        let first = h
            .engine
            .execute_boundary(&grant, request.clone())
            .await
            .unwrap();
        assert_eq!(
            first.status,
            Status::Completed,
            "source proof must settle before child transit: {:?}",
            first.error
        );
        assert_eq!(
            h.engine.execute_boundary(&grant, request).await.unwrap().id,
            first.id
        );
        assert_eq!(
            h.chain.lock().unwrap().broadcasts,
            1,
            "duplicate never broadcasts twice"
        );
        assert_eq!(
            h.approvals.0.load(Ordering::SeqCst),
            1,
            "refresh did not request new authorization"
        );
        let record = h.state.transfer(&transfer).unwrap().unwrap();
        let transit = record
            .transit
            .as_ref()
            .expect("source handoff created transit");
        let locator = transit
            .destination_locator
            .as_ref()
            .expect("source proof persisted the child identity");
        assert_eq!(locator.scheme, "arbitrum_eth_deposit_v1");
        assert_eq!(locator.destination_chain_id, destination);
        assert!(record.approved_bounds.is_some());
        let unrelated = format!("0x{}", "99".repeat(32));
        let child = record
            .transit
            .as_ref()
            .unwrap()
            .destination_locator
            .as_ref()
            .unwrap()
            .transaction_hash
            .clone();
        // Rebuild the observer and broker objects over the same durable store.
        let restarted_source = Arc::new(Rpc {
            chain_id: 1,
            destination_chain: destination,
            state: h.chain.clone(),
        });
        let restarted_child = Arc::new(Rpc {
            chain_id: destination,
            destination_chain: destination,
            state: h.chain.clone(),
        });
        let restarted_source_runtime = Arc::new(EvmRuntime::new(restarted_source, 1, None));
        let restarted_child_runtime = Arc::new(EvmRuntime::new(restarted_child, destination, None));
        let restarted_provider = Arc::new(
            fin_bridge::canonical::CanonicalProvider::new(
                restarted_source_runtime.clone(),
                CanonicalConfig::reviewed(),
            )
            .unwrap(),
        );
        let restarted_adapter = Arc::new(
            BridgeAdapter::with_providers(
                vec![restarted_provider],
                BTreeMap::new(),
                BTreeMap::from([
                    (1, restarted_source_runtime),
                    (destination, restarted_child_runtime),
                ]),
                h.valuator.clone(),
            )
            .unwrap(),
        );
        restarted_adapter
            .attach_state(BridgeStateRepository::new(h.shared.clone()))
            .unwrap();
        let restarted_engine = Arc::new(
            Engine::new_with_shared_store(
                h.shared.clone(),
                vec![restarted_adapter.clone()],
                Arc::new(Approvals::default()),
                EngineOptions {
                    max_movement_and_cost_usd_micros: "100000000".into(),
                    max_run_allocation_usd_micros: "200000000".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap(),
        );
        restarted_adapter
            .attach_engine(Arc::downgrade(&restarted_engine))
            .unwrap();
        // A different child sender cannot settle even when the candidate hash is correct.
        h.chain.lock().unwrap().child_sender =
            Some("0x2222222222222222222222222222222222222222".into());
        let pending = restarted_adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(pending["state"], "in_transit");
        assert!(pending["settlement"].is_null());
        assert_eq!(
            h.state
                .transfer(&transfer)
                .unwrap()
                .unwrap()
                .transit
                .unwrap()
                .destination_locator
                .unwrap()
                .transaction_hash,
            child
        );
        h.chain.lock().unwrap().child_sender = None;
        let settled = restarted_adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(
            settled["state"], "completed",
            "destination type-0x64 proof should settle: {settled}"
        );
        assert_eq!(settled["settlement"]["outcome"], "destination");
        assert_eq!(
            settled["settlement"]["cash_credits"][0]["amount_atoms"],
            "4500000000000000"
        );
        assert_eq!(
            settled["settlement"]["cash_credits"][0]["asset"]["network"],
            format!("ethereum:{destination}")
        );
        assert_eq!(settled["settlement"]["proof"]["chain_id"], destination);
        assert_eq!(settled["settlement"]["proof"]["transaction_hash"], child);
        assert_eq!(
            settled["settlement"]["proof"]["proof_method"],
            "canonical_eth_deposit_child_transaction"
        );
        let again = restarted_adapter
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(again["state"], "completed");
        assert_eq!(again["settlement"], settled["settlement"]);
        assert!(unrelated != child);
        assert_eq!(h.chain.lock().unwrap().broadcasts, 1);
        assert_eq!(h.state.transfer(&transfer).unwrap().unwrap().transit, None);
    }
}

#[path = "support/canonical_fork.rs"]
mod canonical_fork;

#[tokio::test]
#[ignore = "Requires a fresh guarded loopback Anvil fork of Ethereum block 26,099,326; never uses a live write"]
async fn deployed_canonical_native_source_proof_on_both_children() -> Result<()> {
    use canonical_fork::CanonicalForkRpc;
    let rpc = CanonicalForkRpc::new().await?;
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    )?);
    let address = lower_address(signer.address());
    let runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, Some(signer)));
    let mut config = EthereumConfig::wallet_only(1, 1, vec![]);
    config.require_finalized = false;
    let valuator = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let wallet = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "fork-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            runtime.clone(),
        )?
        .with_wallet_valuation(valuator.clone()),
    );
    let tmp = tempfile::tempdir()
        .map_err(|_| FinError::validation("Could not create the fork test journal."))?;
    let shared = Store::open(tmp.path().join("canonical-fork.sqlite"))
        .map_err(|_| FinError::validation("Could not initialize the guarded fork test journal."))?
        .share();
    let state = BridgeStateRepository::new(shared.clone());
    let mut deposits = Vec::new();
    for destination in [42161, 4663] {
        let provider = Arc::new(fin_bridge::canonical::CanonicalProvider::new(
            Arc::new(EvmRuntime::new(rpc.clone(), 1, None)),
            CanonicalConfig::reviewed(),
        )?);
        let bridge = Arc::new(BridgeAdapter::with_providers(
            vec![provider],
            BTreeMap::from([(1, wallet.clone())]),
            BTreeMap::from([(1, runtime.clone())]),
            valuator.clone(),
        )?);
        bridge.attach_state(state.clone())?;
        let engine = Arc::new(Engine::new_with_shared_store(
            shared.clone(),
            vec![wallet.clone(), bridge.clone()],
            Arc::new(Approvals::default()),
            EngineOptions {
                max_movement_and_cost_usd_micros: "100000000".into(),
                max_run_allocation_usd_micros: "200000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )?);
        bridge.attach_engine(Arc::downgrade(&engine))?;
        let fork_bounds = fin_bridge::BridgeBounds {
            fee_caps: vec![fin_bridge::FeeCap {
                asset: fin_bridge::EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                kind: fin_bridge::FeeKind::Gas,
                maximum: q("5000000000000000"),
                maximum_usd_micros: Some(20_000_000),
            }],
            ..Default::default()
        };
        let quoted = bridge
            .read(
                "bridge.quotes",
                &serde_json::to_value(BridgeQuoteInput {
                    intent: BridgeQuoteIntentInput {
                        wallet: "fork-wallet".into(),
                        source: BridgeAssetInput {
                            chain_id: 1,
                            token: None,
                        },
                        destination_chain_id: destination,
                        destination_assets: vec![BridgeAssetInput {
                            chain_id: destination,
                            token: None,
                        }],
                        recipient: address.clone(),
                        refund_to: address.clone(),
                        allowed_refund_assets: vec![],
                        amount: q("1000000000000000"),
                    },
                    bounds: fork_bounds.clone(),
                    refresh_transfer_id: None,
                })
                .map_err(|_| FinError::validation("Could not encode the fork quote input."))?,
            )
            .await?;
        let transfer = quoted["transfers"][0]["transfer_id"].as_str().ok_or_else(|| {
            FinError::new(
                "CANONICAL_FORK_QUOTE_UNAVAILABLE",
                "fork",
                "The guarded fork did not produce a Canonical quote; inspect sanitized provider failure codes.",
                "Correct fixture quote bounds before any submission; no write was attempted.",
            )
            .with_details(safe_fork_quote_diagnostic(&quoted))
        })?.to_owned();
        let step = quoted["transfers"][0]["steps"][0]["step_id"]
            .as_str()
            .ok_or_else(|| FinError::validation("Fork quote did not return a step identity."))?
            .to_owned();
        let input = serde_json::to_value(BridgeTransferInput {
            transfer_id: transfer.clone(),
            step_id: step.clone(),
            step_kind: fin_domain::BridgeStepKind::Deposit,
            bounds: Some(fork_bounds),
        })
        .map_err(|_| FinError::validation("Could not encode the fork transfer input."))?;
        // Verify the user's selected fee scope before asking the engine to execute.
        engine.preview("bridge.transfer", &input).await?;
        let mut operation = engine
            .execute(OperationRequest {
                request_id: format!("canonical-fork-{destination}"),
                operation: "bridge.transfer".into(),
                input,
                run_id: None,
            })
            .await?;
        for _ in 0..12 {
            if !matches!(
                operation.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                break;
            }
            rpc.mine_finality_block().await?;
            operation = engine.status(&operation.id, true).await?;
        }
        if operation.status != Status::Completed {
            let diagnostic = json!({
                "transfer_id": transfer,
                "operation_id": operation.id,
                "status": format!("{:?}", operation.status),
                "error": operation.error.as_ref().map(|error| json!({
                    "code": error.code,
                    "stage": error.stage,
                    "message": error.message,
                })),
                "submission_reference": operation.submission.as_ref().map(|submission| {
                    submission.reference.as_str()
                }),
                "evidence": safe_fork_operation_evidence(&operation),
            });
            let diagnostic_path = tmp
                .path()
                .join(format!("canonical-fork-{destination}-failure.json"));
            let _ = std::fs::write(
                &diagnostic_path,
                serde_json::to_vec_pretty(&diagnostic).unwrap_or_default(),
            );
            return Err(FinError::new(
                "CANONICAL_FORK_SOURCE_UNVERIFIED",
                "fork",
                format!("Canonical source operation did not complete: {diagnostic}"),
                "Inspect this same local fork operation; never rebroadcast it.",
            ));
        }
        let record = state
            .transfer(&transfer)
            .map_err(|_| FinError::validation("Could not reload the fork transfer record."))?
            .ok_or_else(|| FinError::validation("Canonical fork transfer was not persisted."))?;
        let locator = record
            .transit
            .as_ref()
            .and_then(|v| v.destination_locator.as_ref())
            .ok_or_else(|| {
                FinError::validation("Verified source proof did not persist its child locator.")
            })?;
        if locator.destination_chain_id != destination
            || locator.scheme != "arbitrum_eth_deposit_v1"
        {
            return Err(FinError::validation(
                "Canonical source proof persisted the wrong child route.",
            ));
        }
        deposits.push((destination, record));
    }
    rpc.guard().await?;
    if deposits.len() != 2 {
        return Err(FinError::validation(
            "Canonical fork did not verify both child destinations.",
        ));
    }
    Ok(())
}
