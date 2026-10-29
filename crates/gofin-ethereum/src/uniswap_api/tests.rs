//! Synthetic public adapter lifecycles. No provider network calls or live funds.
use super::*;
#[cfg(target_os = "macos")]
use crate::amm_conformance_tests::uniswap_native_approval as native_approval;
use api::{UniswapApiClient, UniswapApiRequest, UniswapApiTransport};
use gofin_evm::alloy::{
    primitives::{B256, keccak256},
    sol_types::{SolCall, SolValue},
};
use gofin_evm::{EvmRpc, LocalEnvelopeSigner, hex_bytes, hex_quantity};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const INPUT: &str = "0x0000000000000000000000000000000000000065";
const OUTPUT: &str = "0x0000000000000000000000000000000000000066";
const REACTOR: &str = "0x00000000000000000000000000000000000001d6";
fn owner() -> String {
    lower_address(
        LocalEnvelopeSigner::from_private_key(KEY)
            .unwrap()
            .address(),
    )
}
fn order_hash() -> String {
    format!("0x{}", "ab".repeat(32))
}
fn filler_hash() -> String {
    format!("0x{}", "cd".repeat(32))
}
fn block_hash(n: u64) -> String {
    format!("{:#x}", B256::from(U256::from(n).to_be_bytes::<32>()))
}

#[derive(Default)]
struct State {
    submitted: Mutex<Option<Submission>>,
    receipt: Mutex<Value>,
    events: Mutex<Vec<String>>,
    order_posts: AtomicUsize,
    swap_posts: AtomicUsize,
    broadcasts: AtomicUsize,
    lose_response: AtomicBool,
    authorized: AtomicBool,
    drift_nonce: AtomicBool,
    nonce_reads: AtomicUsize,
    head_height: AtomicU64,
    block_epoch: u64,
    log_ranges: Mutex<Vec<(u64, u64)>>,
    malformed_header: AtomicBool,
    finalized_recheck_changed: AtomicBool,
}
impl State {
    fn advance_to(&self, timestamp: u64) {
        self.head_height
            .store((timestamp - self.block_epoch) * 10 + 2, Ordering::SeqCst);
    }
}
struct Journal(Arc<State>);
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        require(
            self.0.authorized.load(Ordering::SeqCst),
            "SYNTHETIC_AUTHORIZATION_REQUIRED",
            "The isolated operation needs synthetic authorization.",
        )
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.assert_authorized()?;
        let mut durable = self.0.submitted.lock().unwrap();
        require(
            durable.is_none(),
            "SYNTHETIC_REPLAY",
            "An immutable submission already exists.",
        )?;
        *durable = Some(submission.clone());
        self.0.events.lock().unwrap().push("persist".into());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, submission: &Submission, _: &Value) -> Result<()> {
        require(
            self.0.submitted.lock().unwrap().as_ref() == Some(submission),
            "SYNTHETIC_IDENTITY",
            "Dispatch evidence needs its durable identity.",
        )
    }
}
struct Rpc {
    state: Arc<State>,
    chain: u64,
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.state.events.lock().unwrap().push(method.into());
        Ok(match method {
            "eth_chainId" => json!(format!("0x{:x}", self.chain)),
            "eth_blockNumber" => json!(format!(
                "{:#x}",
                self.state.head_height.load(Ordering::SeqCst)
            )),
            "eth_getBlockByNumber" => {
                let n = match params[0].as_str().unwrap() {
                    "latest" | "finalized" => self.state.head_height.load(Ordering::SeqCst),
                    s => u64::from_str_radix(&s[2..], 16).unwrap(),
                };
                let mut header = json!({"number":format!("0x{n:x}"),"hash":block_hash(n),"timestamp":format!("0x{:x}",self.state.block_epoch + n / 10),"baseFeePerGas":"0x1"});
                if self.state.malformed_header.load(Ordering::SeqCst) {
                    header.as_object_mut().unwrap().remove("hash");
                }
                if self.state.finalized_recheck_changed.load(Ordering::SeqCst)
                    && n == self.state.head_height.load(Ordering::SeqCst)
                    && params[0].as_str().unwrap().starts_with("0x")
                {
                    header["hash"] = json!(block_hash(n + 1));
                }
                header
            }
            "eth_getCode" => json!("0x6000"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_getTransactionCount" => {
                let count = self.state.nonce_reads.fetch_add(1, Ordering::SeqCst);
                json!(
                    if self.state.drift_nonce.load(Ordering::SeqCst) && count > 0 {
                        "0x8"
                    } else {
                        "0x7"
                    }
                )
            }
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_call" => {
                assert!(
                    params[0].get("from").is_none(),
                    "provider transaction must not be locally simulated"
                );
                let bytes = rpc_bytes(&params[0]["data"])?;
                let selector: &[u8] = &bytes[..4];
                if selector == abi::Erc20::decimalsCall::SELECTOR {
                    json!(hex_bytes(&U256::from(6).abi_encode()))
                } else if selector == abi::Erc20::symbolCall::SELECTOR {
                    json!(hex_bytes(&"SYNTH".to_owned().abi_encode()))
                } else if selector == abi::Erc20::balanceOfCall::SELECTOR
                    || selector == abi::Erc20::allowanceCall::SELECTOR
                {
                    json!(hex_bytes(&U256::from(1_000_000_000u64).abi_encode()))
                } else {
                    panic!("unexpected synthetic call selector {}", hex_bytes(&bytes))
                }
            }
            "eth_sendRawTransaction" => {
                let durable = self
                    .state
                    .submitted
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("identity must be durable before broadcast");
                assert_eq!(
                    durable.reference,
                    format!("{:#x}", keccak256(rpc_bytes(&params[0])?))
                );
                self.state.broadcasts.fetch_add(1, Ordering::SeqCst);
                if self.state.lose_response.load(Ordering::SeqCst) {
                    return Err(GoFinError::validation(
                        "Synthetic response lost after accepting transaction.",
                    ));
                }
                json!(durable.reference)
            }
            "eth_getTransactionReceipt" => self.state.receipt.lock().unwrap().clone(),
            "eth_getTransactionByHash" => {
                let submission = self.state.submitted.lock().unwrap().clone().unwrap();
                let receipt = self.state.receipt.lock().unwrap().clone();
                if submission.kind == "ethereum" {
                    let tx: FrozenTransaction =
                        serde_json::from_value(submission.data["transaction"].clone()).unwrap();
                    json!({"hash":submission.reference,"from":tx.sender,"to":tx.to,"nonce":format!("0x{:x}",tx.nonce),"chainId":format!("0x{:x}",tx.chain_id),"type":"0x2","gas":format!("0x{:x}",tx.gas),"value":hex_quantity(u256(&tx.value)?),"input":tx.calldata,"maxFeePerGas":hex_quantity(u256(&tx.max_fee_per_gas)?),"maxPriorityFeePerGas":hex_quantity(u256(&tx.max_priority_fee_per_gas)?),"accessList":[],"blockHash":receipt["blockHash"],"blockNumber":receipt["blockNumber"]})
                } else {
                    json!({"hash":filler_hash(),"from":"0x0000000000000000000000000000000000000009","to":REACTOR,"blockHash":receipt["blockHash"],"blockNumber":receipt["blockNumber"]})
                }
            }
            "eth_getLogs" => {
                let from = rpc_u64(&params[0]["fromBlock"])?;
                let to = rpc_u64(&params[0]["toBlock"])?;
                assert!(
                    from <= to && to - from < 1000,
                    "hosted RPC block range limit"
                );
                self.state.log_ranges.lock().unwrap().push((from, to));
                json!([])
            }
            _ => panic!("unexpected synthetic RPC {method}"),
        })
    }
}
struct Prices;
#[async_trait]
impl WalletValuation for Prices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        Ok((
            units.mul_div_ceil(
                &Quantity::from_u64(if asset.reference == "native" {
                    3_000_000_000
                } else {
                    1_000_000
                }),
                &quantity(pow10(asset.decimals)?),
            )?,
            "synthetic unit prices".into(),
        ))
    }
}
struct Provider {
    state: Arc<State>,
    chain: u64,
    routing: &'static str,
    deadline: u64,
    quote_mutation: Mutex<Option<(&'static str, Value)>>,
}
impl Provider {
    fn permit(&self) -> Value {
        let domain = json!({"name":"Permit2","chainId":self.chain,"verifyingContract":PERMIT2});
        if self.routing == "CLASSIC" {
            json!({"domain":domain,"types":{
                "PermitSingle":[{"name":"details","type":"PermitDetails"},{"name":"spender","type":"address"},{"name":"sigDeadline","type":"uint256"}],
                "PermitDetails":[{"name":"token","type":"address"},{"name":"amount","type":"uint160"},{"name":"expiration","type":"uint48"},{"name":"nonce","type":"uint48"}]},
                "values":{"details":{"token":INPUT,"amount":"1000000","expiration":self.deadline,"nonce":7},"spender":REACTOR,"sigDeadline":self.deadline.to_string()}})
        } else {
            // Provider type shapes observed from live read-only V2 and V3
            // quotes; all identities, amounts and signatures remain synthetic.
            let fields = |pairs: &[(&str, &str)]| -> Value {
                pairs
                    .iter()
                    .map(|(name, ty)| json!({"name":name,"type":ty}))
                    .collect()
            };
            let witness_type = if self.routing == "DUTCH_V2" {
                "V2DutchOrder"
            } else {
                "V3DutchOrder"
            };
            let mut types = json!({
                "PermitWitnessTransferFrom":fields(&[("permitted","TokenPermissions"),("spender","address"),("nonce","uint256"),("deadline","uint256"),("witness",witness_type)]),
                "TokenPermissions":fields(&[("token","address"),("amount","uint256")]),
                "OrderInfo":fields(&[("reactor","address"),("swapper","address"),("nonce","uint256"),("deadline","uint256"),("additionalValidationContract","address"),("additionalValidationData","bytes")])
            });
            let info = json!({"reactor":REACTOR,"swapper":owner(),"nonce":"7","deadline":self.deadline,"additionalValidationContract":lower_address(Address::ZERO),"additionalValidationData":"0x"});
            let witness = if self.routing == "DUTCH_V2" {
                types["V2DutchOrder"] = fields(&[
                    ("info", "OrderInfo"),
                    ("cosigner", "address"),
                    ("baseInputToken", "address"),
                    ("baseInputStartAmount", "uint256"),
                    ("baseInputEndAmount", "uint256"),
                    ("baseOutputs", "DutchOutput[]"),
                ]);
                types["DutchOutput"] = fields(&[
                    ("token", "address"),
                    ("startAmount", "uint256"),
                    ("endAmount", "uint256"),
                    ("recipient", "address"),
                ]);
                json!({"info":info,"cosigner":REACTOR,"baseInputToken":INPUT,"baseInputStartAmount":"1000000","baseInputEndAmount":"1000000","baseOutputs":[{"token":OUTPUT,"startAmount":"1000000","endAmount":"990000","recipient":owner()}]})
            } else {
                types["V3DutchOrder"] = fields(&[
                    ("info", "OrderInfo"),
                    ("cosigner", "address"),
                    ("startingBaseFee", "uint256"),
                    ("baseInput", "V3DutchInput"),
                    ("baseOutputs", "V3DutchOutput[]"),
                ]);
                types["V3DutchInput"] = fields(&[
                    ("token", "address"),
                    ("startAmount", "uint256"),
                    ("curve", "NonlinearDutchDecay"),
                    ("maxAmount", "uint256"),
                    ("adjustmentPerGweiBaseFee", "uint256"),
                ]);
                types["V3DutchOutput"] = fields(&[
                    ("token", "address"),
                    ("startAmount", "uint256"),
                    ("curve", "NonlinearDutchDecay"),
                    ("recipient", "address"),
                    ("minAmount", "uint256"),
                    ("adjustmentPerGweiBaseFee", "uint256"),
                ]);
                types["NonlinearDutchDecay"] = fields(&[
                    ("relativeBlocks", "uint256"),
                    ("relativeAmounts", "int256[]"),
                ]);
                json!({"info":info,"cosigner":REACTOR,"startingBaseFee":"0","baseInput":{"token":INPUT,"startAmount":"1000000","curve":{"relativeBlocks":"32","relativeAmounts":["0"]},"maxAmount":"1000000","adjustmentPerGweiBaseFee":"0"},"baseOutputs":[{"token":OUTPUT,"startAmount":"1000000","curve":{"relativeBlocks":"32","relativeAmounts":["10000"]},"recipient":owner(),"minAmount":"990000","adjustmentPerGweiBaseFee":"0"}]})
            };
            json!({"domain":domain,"types":types,"values":{"permitted":{"token":INPUT,"amount":"1000000"},"spender":REACTOR,"nonce":"7","deadline":self.deadline,"witness":witness}})
        }
    }
    fn quote(&self) -> Value {
        let mut quote = json!({"routing":self.routing,"quote":{"quoteId":"synthetic-quote","chainId":self.chain,"swapper":owner(),"tradeType":"EXACT_INPUT","input":{"token":INPUT,"amount":"1000000","maximumAmount":"1000000"},"output":{"token":OUTPUT,"recipient":owner(),"amount":"1000000","minimumAmount":"990000"},"slippage":1,"slippageTolerance":1,"priceImpact":0,"gasFee":"450000000000"},"permitData":self.permit()});
        if self.routing != "CLASSIC" {
            quote["quote"]["orderId"] = json!(order_hash());
            quote["quote"]["orderInfo"] = json!({"chainId":self.chain,"swapper":owner(),"reactor":REACTOR,"nonce":"7","deadline":self.deadline,"input":{"token":INPUT,"startAmount":"1000000","endAmount":"1000000","maxAmount":"1000000"},"outputs":[{"token":OUTPUT,"recipient":owner(),"startAmount":"1000000","endAmount":"990000","minAmount":"990000"}]});
        }
        if let Some((pointer, value)) = self.quote_mutation.lock().unwrap().as_ref() {
            *quote.pointer_mut(pointer).unwrap() = value.clone();
        }
        quote
    }
}
#[async_trait]
impl UniswapApiTransport for Provider {
    async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
        match request {
            UniswapApiRequest::Post { path, body } => match path.as_str() {
                "/quote" => {
                    assert_eq!(body["permitAmount"], "EXACT");
                    assert_eq!(body["routingPreference"], "BEST_PRICE");
                    Ok(self.quote())
                }
                "/swap" => {
                    self.state.swap_posts.fetch_add(1, Ordering::SeqCst);
                    assert!(self.state.authorized.load(Ordering::SeqCst));
                    assert_eq!(body["quote"], self.quote()["quote"]);
                    assert_eq!(body["signature"].as_str().unwrap().len(), 132);
                    Ok(
                        json!({"swap":{"chainId":self.chain,"from":owner(),"to":REACTOR,"value":"0","data":"0xdeadbeef010203","gasLimit":"30000"}}),
                    )
                }
                "/order" => {
                    assert!(
                        self.state.submitted.lock().unwrap().is_some(),
                        "order identity must be durable before submission"
                    );
                    self.state.order_posts.fetch_add(1, Ordering::SeqCst);
                    self.state.events.lock().unwrap().push("order".into());
                    assert_eq!(body["signature"].as_str().unwrap().len(), 132);
                    if self.state.lose_response.load(Ordering::SeqCst) {
                        Err(GoFinError::validation(
                            "Synthetic response lost after accepting order.",
                        ))
                    } else {
                        Ok(json!({"orderId":order_hash(),"orderStatus":"open"}))
                    }
                }
                _ => panic!("unexpected synthetic provider POST {path}"),
            },
            UniswapApiRequest::Get { path, query } => {
                assert_eq!(path, "/orders");
                assert_eq!(query, vec![("orderId".into(), order_hash())]);
                let receipt = self.state.receipt.lock().unwrap();
                Ok(
                    json!({"orders":[{"orderId":order_hash(),"orderStatus":if receipt.is_null(){"open"}else{"filled"},"txHash":if receipt.is_null(){Value::Null}else{json!(filler_hash())}}]}),
                )
            }
        }
    }
}
fn fixture(routing: &'static str) -> (EthereumAdapter, Arc<State>, Arc<Provider>, Value) {
    let chain = if routing == "DUTCH_V3" { 42161 } else { 1 };
    let timestamp = chrono::Utc::now().timestamp() as u64;
    let state = Arc::new(State {
        head_height: AtomicU64::new(22),
        block_epoch: timestamp - 2,
        ..Default::default()
    });
    let provider = Arc::new(Provider {
        state: state.clone(),
        chain,
        routing,
        deadline: timestamp + 300,
        quote_mutation: Mutex::new(None),
    });
    let runtime = Arc::new(EvmRuntime::new(
        Arc::new(Rpc {
            state: state.clone(),
            chain,
        }),
        chain,
        Some(Arc::new(
            LocalEnvelopeSigner::from_private_key(KEY).unwrap(),
        )),
    ));
    let mut adapter = EthereumAdapter::new(
        EthereumConfig::wallet_only(chain, 2, vec![INPUT.into(), OUTPUT.into()]),
        vec![WalletConnection {
            name: "synthetic".into(),
            address: owner(),
            signing_enabled: true,
        }],
        runtime,
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Prices));
    adapter.uniswap_api = UniswapApiClient::with_transport(provider.clone());
    let input = json!({"wallet":"synthetic","network":"ethereum","chain_id":chain,"request_id":"synthetic-uniswap","token_in":INPUT,"token_out":OUTPUT,"recipient":owner(),"amount":"1","min_output":"0.99","max_fee_eth":"0.0001","slippage_bps":100,"deadline":timestamp+300});
    (adapter, state, provider, input)
}
fn record(preview: PreparedOperation, submission: Submission) -> OperationRecord {
    OperationRecord {
        id: "synthetic-uniswap".into(),
        request_id: "synthetic-uniswap".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: preview.operation.clone(),
        input: preview.input.clone(),
        status: Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(preview),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
fn topic_address(a: &str) -> String {
    format!("{:#x}", B256::from(address(a).unwrap().into_word()))
}
fn transfer(token: &str, from: &str, to: &str, atoms: u64, index: u64) -> Value {
    json!({"address":token,"topics":[format!("{:#x}",keccak256("Transfer(address,address,uint256)")),topic_address(from),topic_address(to)],"data":hex_bytes(&U256::from(atoms).to_be_bytes::<32>()),"logIndex":format!("0x{index:x}"),"removed":false})
}
fn settle(state: &State, routing: &str) {
    let submission = state.submitted.lock().unwrap().clone().unwrap();
    let mut logs = vec![
        transfer(INPUT, &owner(), REACTOR, 1_000_000, 0),
        transfer(OUTPUT, REACTOR, &owner(), 995_000, 1),
    ];
    if routing != "CLASSIC" {
        logs.push(json!({"address":REACTOR,"topics":[format!("{:#x}",keccak256("Fill(bytes32,address,address,uint256)")),order_hash(),topic_address("0x0000000000000000000000000000000000000009"),topic_address(&owner())],"data":hex_bytes(&U256::from(7).to_be_bytes::<32>()),"logIndex":"0x2","removed":false}));
    }
    let transaction_hash = if routing == "CLASSIC" {
        submission.reference.clone()
    } else {
        filler_hash()
    };
    for log in &mut logs {
        log["transactionHash"] = json!(transaction_hash);
        log["blockHash"] = json!(block_hash(20));
        log["blockNumber"] = json!("0x14");
    }
    *state.receipt.lock().unwrap() = json!({"transactionHash":if routing=="CLASSIC"{submission.reference}else{filler_hash()},"blockHash":block_hash(20),"blockNumber":"0x14","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x989680","logs":logs});
}

#[tokio::test]
async fn api_amm_and_uniswapx_v2_v3_complete_authorized_adapter_lifecycles() {
    for routing in ["CLASSIC", "DUTCH_V2", "DUTCH_V3"] {
        let (adapter, state, _, input) = fixture(routing);
        let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
        assert_eq!(prepared.binding["quote"]["routing"], routing);
        #[cfg(target_os = "macos")]
        if routing != "CLASSIC" {
            let reason = native_approval::assert_prepared_renders(&prepared);
            for term in [
                "UniswapX",
                "swap 1",
                "for at least 0.99",
                "fees ≤0 ETH",
                "permit limited to this swap",
            ] {
                assert!(reason.contains(term), "missing {term}: {reason}");
            }
        }
        assert_eq!(
            state.order_posts.load(Ordering::SeqCst) + state.broadcasts.load(Ordering::SeqCst),
            0
        );
        let journal = Arc::new(Journal(state.clone()));
        assert!(adapter.submit(&prepared, journal.clone()).await.is_err());
        state.authorized.store(true, Ordering::SeqCst);
        let submission = adapter.submit(&prepared, journal).await.unwrap();
        let record = record(prepared, submission);
        assert_ne!(
            adapter.reconcile(&record).await.unwrap().status,
            Status::Completed
        );
        settle(&state, routing);
        let result = adapter.reconcile(&record).await.unwrap();
        assert_eq!(
            result.status,
            Status::Completed,
            "{routing}: {}",
            result.evidence
        );
        let verified = result
            .verified
            .expect("terminal settlement must include financial proof");
        let conversion = verified
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::Conversion {
                    spent, received, ..
                } => Some((spent, received)),
                _ => None,
            })
            .expect("canonical token logs must prove the conversion");
        assert_eq!(conversion.0.asset.reference, INPUT);
        assert_eq!(conversion.0.atoms, Quantity::from_u64(1_000_000));
        assert_eq!(conversion.1.asset.reference, OUTPUT);
        assert_eq!(conversion.1.atoms, Quantity::from_u64(995_000));
        let costs: Vec<_> = verified
            .effects
            .iter()
            .filter_map(|effect| match effect {
                FinancialEffect::Cost { amount, .. } => Some(amount),
                _ => None,
            })
            .collect();
        if routing == "CLASSIC" {
            assert_eq!(costs.len(), 1);
            assert_eq!(costs[0].asset.reference, "native");
            assert_eq!(costs[0].atoms, Quantity::from_u64(210_000_000_000));
        } else {
            assert!(costs.is_empty(), "fillers pay UniswapX execution fees");
        }
        let events = state.events.lock().unwrap();
        let persist = events.iter().position(|event| event == "persist").unwrap();
        let dispatch = events
            .iter()
            .position(|event| {
                event
                    == if routing == "CLASSIC" {
                        "eth_sendRawTransaction"
                    } else {
                        "order"
                    }
            })
            .unwrap();
        assert!(persist < dispatch);
        drop(events);
        assert!(
            !state
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "eth_estimateGas")
        );
        assert_eq!(
            state.broadcasts.load(Ordering::SeqCst),
            usize::from(routing == "CLASSIC")
        );
        assert_eq!(
            state.order_posts.load(Ordering::SeqCst),
            usize::from(routing != "CLASSIC")
        );
    }
}

#[tokio::test]
async fn quote_bounds_reject_bad_minimum_before_signing_or_dispatch() {
    let (adapter, state, provider, input) = fixture("CLASSIC");
    *provider.quote_mutation.lock().unwrap() =
        Some(("/quote/output/minimumAmount", json!("980000")));
    assert_eq!(
        adapter
            .prepare("uniswap.swap", &input)
            .await
            .unwrap_err()
            .code,
        "UNISWAP_API_INVALID"
    );
    assert!(state.submitted.lock().unwrap().is_none());
    assert_eq!(state.broadcasts.load(Ordering::SeqCst), 0);
    assert_eq!(state.swap_posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn accepted_transaction_and_order_timeouts_reconcile_original_identity_without_replay() {
    for routing in ["CLASSIC", "DUTCH_V2", "DUTCH_V3"] {
        let (adapter, state, _, input) = fixture(routing);
        let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
        state.authorized.store(true, Ordering::SeqCst);
        state.lose_response.store(true, Ordering::SeqCst);
        assert!(
            adapter
                .submit(&prepared, Arc::new(Journal(state.clone())))
                .await
                .is_err()
        );
        let submission = state
            .submitted
            .lock()
            .unwrap()
            .clone()
            .expect("identity survives lost response");
        let record = record(prepared, submission);
        settle(&state, routing);
        for _ in 0..2 {
            assert_eq!(
                adapter.reconcile(&record).await.unwrap().status,
                Status::Completed
            );
        }
        assert_eq!(
            state.broadcasts.load(Ordering::SeqCst) + state.order_posts.load(Ordering::SeqCst),
            1
        );
    }
}

#[tokio::test]
async fn nonce_change_rejects_before_signing_and_broadcast() {
    let (adapter, state, _, input) = fixture("CLASSIC");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    state.drift_nonce.store(true, Ordering::SeqCst);
    let error = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap_err();
    assert_eq!(error.code, "NONCE_CHANGED");
    assert!(state.submitted.lock().unwrap().is_none());
    assert_eq!(state.broadcasts.load(Ordering::SeqCst), 0);
}

/// Native-input fixture shared by permission planning tests. It exercises the
/// same hosted quote contract without depending on legacy pool RPC selectors.
pub(crate) fn classic_fixture_client() -> UniswapApiClient {
    struct NativeQuote;
    #[async_trait]
    impl UniswapApiTransport for NativeQuote {
        async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
            let UniswapApiRequest::Post { path, body } = request else {
                panic!("quote-only fixture")
            };
            assert_eq!(path, "/quote");
            Ok(json!({"routing":"CLASSIC","quote":{
                "quoteId":"synthetic-native-quote","chainId":body["tokenInChainId"],"swapper":body["swapper"],"tradeType":"EXACT_INPUT",
                "input":{"token":body["tokenIn"],"amount":body["amount"],"maximumAmount":body["amount"]},
                "output":{"token":body["tokenOut"],"recipient":body["recipient"],"amount":"3000000","minimumAmount":"2970000"},
                "slippage":1,"priceImpact":0,"gasFee":"450000000000"
            }}))
        }
    }
    UniswapApiClient::with_transport(Arc::new(NativeQuote))
}

#[tokio::test]
async fn uniswapx_expiry_requires_finalized_empty_fill_search() {
    let (adapter, state, provider, input) = fixture("DUTCH_V2");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    let submission = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap();
    let record = record(prepared, submission);
    assert_eq!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Pending
    );
    state.advance_to(provider.deadline + 1);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Expired);
    assert!(result.verified.unwrap().effects.is_empty());
    assert!(
        state
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_getLogs")
    );
    assert_eq!(state.order_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delayed_uniswapx_recovery_scans_only_order_window_in_bounded_ranges() {
    let (adapter, state, provider, input) = fixture("DUTCH_V3");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    let submission = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap();
    let record = record(prepared, submission);
    state.head_height.store(1_000_000, Ordering::SeqCst);
    state.events.lock().unwrap().clear();
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Expired);
    assert!(result.verified.unwrap().effects.is_empty());
    let first_expired_block = (provider.deadline + 1 - state.block_epoch) * 10;
    let ranges = state.log_ranges.lock().unwrap();
    assert_eq!(ranges.len(), 4);
    assert_eq!(ranges.first().unwrap().0, 21);
    assert_eq!(ranges.last().unwrap().1, first_expired_block);
    assert!(ranges.windows(2).all(|w| w[0].1 + 1 == w[1].0));
    assert!(ranges.iter().all(|(from, to)| to - from < 1000));
    let events = state.events.lock().unwrap();
    assert!(
        events
            .iter()
            .filter(|m| *m == "eth_getBlockByNumber")
            .count()
            <= 23
    );
    assert_eq!(events.last().unwrap(), "eth_getBlockByNumber");
    assert_eq!(state.order_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delayed_uniswapx_recovery_rejects_changed_finalized_view() {
    let (adapter, state, _, input) = fixture("DUTCH_V3");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    let submission = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap();
    let record = record(prepared, submission);
    state.head_height.store(1_000_000, Ordering::SeqCst);
    state
        .finalized_recheck_changed
        .store(true, Ordering::SeqCst);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    assert!(result.verified.is_none());
    assert_eq!(
        result.evidence["details"]["reason"],
        "finalized-view-changed"
    );
    assert_eq!(state.log_ranges.lock().unwrap().len(), 4);
    assert_eq!(state.order_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uniswapx_provider_filled_status_does_not_replace_order_and_transfer_proof() {
    let (adapter, state, _, input) = fixture("DUTCH_V2");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    let submission = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap();
    let record = record(prepared, submission);
    settle(&state, "DUTCH_V2");
    state.receipt.lock().unwrap()["logs"][2]["data"] =
        json!(hex_bytes(&U256::from(8).to_be_bytes::<32>()));
    assert_eq!(
        adapter.reconcile(&record).await.unwrap_err().code,
        "UNISWAPX_FILL_IDENTITY"
    );
    settle(&state, "DUTCH_V2");
    state.receipt.lock().unwrap()["logs"][1]["data"] =
        json!(hex_bytes(&U256::from(980_000).to_be_bytes::<32>()));
    assert_eq!(
        adapter.reconcile(&record).await.unwrap_err().code,
        "SWAP_SETTLEMENT_MISMATCH"
    );
    assert_eq!(state.order_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uniswapx_malformed_finalized_header_cannot_prove_unfilled_expiry() {
    let (adapter, state, provider, input) = fixture("DUTCH_V2");
    let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
    state.authorized.store(true, Ordering::SeqCst);
    let submission = adapter
        .submit(&prepared, Arc::new(Journal(state.clone())))
        .await
        .unwrap();
    let record = record(prepared, submission);
    state.advance_to(provider.deadline + 1);
    state.malformed_header.store(true, Ordering::SeqCst);
    if let Ok(result) = adapter.reconcile(&record).await {
        assert_eq!(result.status, Status::Pending);
        assert!(result.verified.is_none());
    }
    assert_eq!(state.order_posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_provider_permit_fails_without_signing_or_submission() {
    for (pointer, value) in [
        ("/permitData", json!("malformed")),
        ("/permitData/types", json!([])),
        ("/permitData/values", json!("malformed")),
        ("/permitData/domain", Value::Null),
    ] {
        let (adapter, state, provider, input) = fixture("CLASSIC");
        *provider.quote_mutation.lock().unwrap() = Some((pointer, value));
        let prepared = adapter.prepare("uniswap.swap", &input).await.unwrap();
        state.authorized.store(true, Ordering::SeqCst);
        let failure = adapter
            .submit(&prepared, Arc::new(Journal(state.clone())))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "UNISWAP_PERMIT_INVALID");
        assert!(state.submitted.lock().unwrap().is_none());
        assert_eq!(state.swap_posts.load(Ordering::SeqCst), 0);
        assert_eq!(state.broadcasts.load(Ordering::SeqCst), 0);
    }
}
