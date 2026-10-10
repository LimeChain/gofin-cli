//! Synthetic-only public lifecycle fixtures for the ERC-20 bridge path.
use async_trait::async_trait;
use fin_bridge::*;
use fin_domain::{AssetId, Quantity, Result, now};
use fin_evm::{
    EvmRpc,
    alloy::{
        consensus::{Transaction, TxEnvelope},
        eips::eip2718::Decodable2718,
        primitives::{Address, B256, U256, keccak256},
        sol_types::SolCall,
    },
    lower_address,
};
use fin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

pub const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
pub const ROUTER: &str = "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f";
pub const SOLVER: &str = "0xf70da97812cb96acdf810712aa562db8dfa3dbef";
pub const ETH_USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub const ARB_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const PROVIDER_REQUEST_ID: &str =
    "0x5555555555555555555555555555555555555555555555555555555555555555";
const DESTINATION_TX: &str = "0x7777777777777777777777777777777777777777777777777777777777777777";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
pub const ALT_PAYOUT_SENDER: &str = "0xca7ded7e4f4ba8ab3b10009236ae6d1b95094589";

fin_evm::alloy::sol! {
    interface FixtureErc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function decimals() external view returns (uint8);
        event Approval(address indexed owner, address indexed spender, uint256 value);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
    interface FixtureDepository {
        function depositErc20(address depositor, address token, uint256 amount, bytes32 id) external;
        event RelayErc20Deposit(address depositor, address token, uint256 amount, bytes32 id);
    }
}

fn q(value: &str) -> Quantity {
    Quantity::new(value).unwrap()
}
fn word_address(value: Address) -> String {
    hex::encode(value.into_word())
}
fn word(value: U256) -> String {
    format!("{value:064x}")
}
fn addr(value: &str) -> Address {
    value.parse().unwrap()
}
fn hash(value: &str) -> String {
    format!("0x{}", hex::encode(keccak256(value.as_bytes())))
}
fn pad_topic(address: Address) -> String {
    format!("0x{}", word_address(address))
}

pub struct Erc20Provider {
    revisions: AtomicUsize,
    pub omit_approval: AtomicBool,
    destination_rpc: Option<Arc<Rpc>>,
    direct_payout: DestinationPayout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationPayout {
    Router,
    Direct,
    DirectTransferFrom,
    DirectWrongOrderTag,
    DirectWrongToken,
    DirectNoTransferLog,
}
impl Erc20Provider {
    pub fn with_destination(destination_rpc: Arc<Rpc>, direct_payout: DestinationPayout) -> Self {
        Self {
            revisions: AtomicUsize::new(0),
            omit_approval: AtomicBool::new(false),
            destination_rpc: Some(destination_rpc),
            direct_payout,
        }
    }
}
#[async_trait]
impl BridgeProvider for Erc20Provider {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: BTreeSet::from([1, 42161]),
            native_eth: true,
            erc20: true,
            protocol_versions: BTreeSet::from(["protocol.v2".into()]),
        }
    }

    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let revision = self.revisions.fetch_add(1, Ordering::SeqCst) + 1;
        let order = format!("0x{:064x}", revision + 0x80);
        let source = &request.intent.source;
        let destination = &request.intent.destination_assets[0];
        let expected_receipt = if destination.token.is_some() {
            q("9700000")
        } else {
            q("5000000000000000")
        };
        let minimum_receipt = if destination.token.is_some() {
            q("9500000")
        } else {
            q("4800000000000000")
        };
        if let Some(destination_rpc) = &self.destination_rpc
            && self.direct_payout != DestinationPayout::Router
        {
            let tag = if self.direct_payout == DestinationPayout::DirectWrongOrderTag {
                PROVIDER_REQUEST_ID
            } else {
                &order
            };
            destination_rpc.set_direct_destination(
                destination,
                &request.intent.recipient,
                tag,
                &expected_receipt,
                self.direct_payout,
            );
        }
        let token = addr(source.token.as_deref().unwrap());
        let depositor = addr(&request.intent.source_address);
        let approval = ApprovalRequirement {
            token: source.token.clone().unwrap(),
            spender: DEPOSITORY.into(),
            amount: request.intent.amount.clone(),
        };
        let approve = BridgeStep {
            ordinal: 0,
            kind: BridgeStepKind::Approval,
            call: EvmCall {
                chain_id: source.chain_id,
                to: approval.token.clone(),
                value: q("0"),
                data: format!(
                    "0x{}",
                    hex::encode(
                        FixtureErc20::approveCall {
                            spender: addr(DEPOSITORY),
                            amount: U256::from_str_radix(request.intent.amount.as_str(), 10)
                                .unwrap(),
                        }
                        .abi_encode()
                    )
                ),
            },
            expected_sender: request.intent.source_address.clone(),
            maximum_debit: vec![],
            dependencies: vec![],
        };
        let deposit = BridgeStep {
            ordinal: 1,
            kind: BridgeStepKind::Deposit,
            call: EvmCall {
                chain_id: source.chain_id,
                to: DEPOSITORY.into(),
                value: q("0"),
                data: format!(
                    "0x{}",
                    hex::encode(
                        FixtureDepository::depositErc20Call {
                            depositor,
                            token,
                            amount: U256::from_str_radix(request.intent.amount.as_str(), 10)
                                .unwrap(),
                            id: order.parse::<B256>().unwrap(),
                        }
                        .abi_encode()
                    )
                ),
            },
            expected_sender: request.intent.source_address.clone(),
            maximum_debit: vec![source.amount(request.intent.amount.clone())],
            dependencies: vec![0],
        };
        let omit_approval = self.omit_approval.load(Ordering::SeqCst);
        let mut deposit = deposit;
        if omit_approval {
            deposit.ordinal = 0;
            deposit.dependencies.clear();
        }
        Ok(BridgeQuoteBatch {
            quotes: vec![BridgeQuote {
                quote_id: format!("synthetic-erc20-{revision}"),
                provider: "relay".into(),
                route_id: "synthetic-usdc-fixed-route".into(),
                intent: request.intent.clone(),
                gross_debit: source.amount(request.intent.amount.clone()),
                expected_receipt: destination.amount(expected_receipt),
                minimum_receipt: destination.amount(minimum_receipt),
                fees: vec![],
                approval: (!omit_approval).then_some(approval),
                protocol_steps: if omit_approval {
                    vec![deposit]
                } else {
                    vec![approve, deposit]
                },
                estimated_arrival_seconds: Some(120),
                observed_at: now(),
                valid_until: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
                class: QuoteClass::Executable,
                refund_paths: vec![RefundPath {
                    chain_id: source.chain_id,
                    asset: source.asset_id(),
                    recipient: request.intent.refund_to.clone(),
                    minimum: q("0"),
                    maximum: request.intent.amount.clone(),
                }],
                order_solver: Some(SOLVER.into()),
                protocol_order_id: Some(order),
                provider_request_id: Some(PROVIDER_REQUEST_ID.into()),
            }],
            failures: vec![],
        })
    }

    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        _: &str,
    ) -> Result<PreparedBridge> {
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: quote.protocol_order_id.clone(),
            fixed_scope_digest: "synthetic-erc20-fixed-scope".into(),
        })
    }

    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        Ok(BridgeReconciliation {
            status: "success".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: vec![BridgeEvidenceCandidate {
                order_id: transfer.order_id.clone(),
                kind: EvidenceKind::DestinationFill,
                chain_id: transfer.destination_chain_id,
                transaction_hash: DESTINATION_TX.into(),
                correlation: transfer.provider_request_id.clone(),
            }],
        })
    }
}

pub struct Rpc {
    pub chain_id: u64,
    pub wallet: String,
    pub allowance: Mutex<U256>,
    pub token_balance: Mutex<U256>,
    allowance_at_block: Mutex<BTreeMap<u64, U256>>,
    block_number: AtomicU64,
    sent: Mutex<BTreeMap<String, (Value, Value)>>,
    broadcasts: AtomicUsize,
    token_balance_reads: AtomicUsize,
    estimate_gas_calls: AtomicUsize,
    transaction_mutation: Mutex<Option<&'static str>>,
    destination_override: Mutex<Option<(Value, Value)>>,
    empty_code: Mutex<BTreeSet<String>>,
    historical_code_unavailable: Mutex<bool>,
}
impl Rpc {
    pub fn source_with_allowance(wallet: String, allowance: U256) -> Self {
        Self::new(1, wallet, allowance)
    }
    pub fn destination(wallet: String) -> Self {
        Self::new(42161, wallet, U256::ZERO)
    }
    fn new(chain_id: u64, wallet: String, allowance: U256) -> Self {
        Self {
            chain_id,
            wallet,
            allowance: Mutex::new(allowance),
            token_balance: Mutex::new(U256::from(20_000_000u64)),
            allowance_at_block: Mutex::new(BTreeMap::from([(100, allowance)])),
            // Destination candidates are included in block 100; expose one
            // additional finalized head so the configured two-confirmation
            // observer can settle them honestly.
            block_number: AtomicU64::new(if chain_id == 1 { 100 } else { 101 }),
            sent: Mutex::new(BTreeMap::new()),
            broadcasts: AtomicUsize::new(0),
            token_balance_reads: AtomicUsize::new(0),
            estimate_gas_calls: AtomicUsize::new(0),
            transaction_mutation: Mutex::new(None),
            destination_override: Mutex::new(None),
            empty_code: Mutex::new(BTreeSet::new()),
            historical_code_unavailable: Mutex::new(false),
        }
    }
    fn set_direct_destination(
        &self,
        asset: &EvmAsset,
        recipient: &str,
        tag: &str,
        amount: &Quantity,
        payout: DestinationPayout,
    ) {
        let recipient_address = addr(recipient);
        let sender = addr(ALT_PAYOUT_SENDER);
        let (to, value, input, logs) = if let Some(token) = &asset.token {
            *self.historical_code_unavailable.lock().unwrap() = true;
            let amount = U256::from_str_radix(amount.as_str(), 10).unwrap();
            let token_sender = if payout == DestinationPayout::DirectTransferFrom {
                Address::repeat_byte(0x61)
            } else {
                sender
            };
            let input = if payout == DestinationPayout::DirectTransferFrom {
                format!(
                    "0x23b872dd{}{}{}{}",
                    word_address(token_sender),
                    word_address(recipient_address),
                    word(amount),
                    tag.trim_start_matches("0x")
                )
            } else {
                format!(
                    "0xa9059cbb{}{}{}",
                    word_address(recipient_address),
                    word(amount),
                    tag.trim_start_matches("0x")
                )
            };
            let transfer = self.event_log(
                token,
                vec![
                    hash("Transfer(address,address,uint256)"),
                    pad_topic(token_sender),
                    pad_topic(recipient_address),
                ],
                word(amount),
                DESTINATION_TX,
                100,
                0,
            );
            let transaction_token = if payout == DestinationPayout::DirectWrongToken {
                if token.eq_ignore_ascii_case(ARB_USDC) {
                    ETH_USDC
                } else {
                    ARB_USDC
                }
            } else {
                token
            };
            let logs = if payout == DestinationPayout::DirectNoTransferLog {
                vec![]
            } else {
                vec![transfer]
            };
            (transaction_token.to_owned(), "0x0".to_owned(), input, logs)
        } else {
            self.empty_code
                .lock()
                .unwrap()
                .insert(lower_address(recipient_address));
            (
                lower_address(recipient_address),
                format!("{:#x}", U256::from_str_radix(amount.as_str(), 10).unwrap()),
                format!("0x{}", tag.trim_start_matches("0x")),
                vec![],
            )
        };
        let transaction = json!({
            "hash":DESTINATION_TX,"from":ALT_PAYOUT_SENDER,"to":to,"value":value,"input":input,
            "nonce":"0x1","gas":"0x186a0","chainId":format!("0x{:x}",self.chain_id),"type":"0x2",
            "maxFeePerGas":"0x10","maxPriorityFeePerGas":"0x1","accessList":[],
            "blockNumber":"0x64","blockHash":BLOCK_HASH
        });
        let receipt = json!({
            "transactionHash":DESTINATION_TX,"blockNumber":"0x64","blockHash":BLOCK_HASH,"status":"0x1",
            "gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":logs
        });
        *self.destination_override.lock().unwrap() = Some((transaction, receipt));
    }
    pub fn broadcasts(&self) -> usize {
        self.broadcasts.load(Ordering::SeqCst)
    }
    pub fn token_balance_reads(&self) -> usize {
        self.token_balance_reads.load(Ordering::SeqCst)
    }
    pub fn estimate_gas_calls(&self) -> usize {
        self.estimate_gas_calls.load(Ordering::SeqCst)
    }
    pub fn mutate_reported_transaction(&self, mutation: &'static str) {
        *self.transaction_mutation.lock().unwrap() = Some(mutation);
    }
    fn event_log(
        &self,
        emitter: &str,
        topics: Vec<String>,
        data: String,
        tx: &str,
        block: u64,
        index: u64,
    ) -> Value {
        json!({
            "address": emitter,
            "topics": topics,
            "data": format!("0x{data}"),
            "logIndex": format!("0x{index:x}"),
            "transactionHash": tx,
            "blockNumber": format!("0x{block:x}"),
            "blockHash": BLOCK_HASH,
            "removed": false
        })
    }
    fn destination_receipt(&self) -> (Value, Value) {
        let tx = DESTINATION_TX;
        let router = addr(ROUTER);
        let recipient = addr(&self.wallet);
        let token = addr(ARB_USDC);
        let amount = U256::from(9_700_000u64);
        let mut event_data = String::new();
        for word_data in [
            word_address(router),
            word_address(recipient),
            word_address(token),
            word(amount),
            word(U256::from(160)),
            word(U256::from(32)),
            "55".repeat(32),
        ] {
            event_data.push_str(&word_data);
        }
        let movement = self.event_log(
            ROUTER,
            vec![hash("FundsMovement(address,address,address,uint256,bytes)")],
            event_data,
            tx,
            100,
            0,
        );
        let transfer = self.event_log(
            ARB_USDC,
            vec![
                hash("Transfer(address,address,uint256)"),
                pad_topic(router),
                pad_topic(recipient),
            ],
            word(amount),
            tx,
            100,
            1,
        );
        let transaction = json!({
            "hash":tx,"from":SOLVER,"to":ROUTER,"value":"0x0","input":"0x",
            "nonce":"0x1","gas":"0x186a0","chainId":"0xa4b1","type":"0x2",
            "maxFeePerGas":"0x10","maxPriorityFeePerGas":"0x1","accessList":[],
            "blockNumber":"0x64","blockHash":BLOCK_HASH
        });
        let receipt = json!({
            "transactionHash":tx,"blockNumber":"0x64","blockHash":BLOCK_HASH,"status":"0x1",
            "gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":[movement,transfer]
        });
        (transaction, receipt)
    }
    fn block_value(&self, tag: &Value) -> Value {
        let latest = self.block_number.load(Ordering::SeqCst);
        let block = match tag.as_str().unwrap_or("latest") {
            "latest" | "finalized" | "safe" => latest,
            value => u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap_or(latest),
        };
        json!({"hash":BLOCK_HASH,"number":format!("0x{block:x}"),"baseFeePerGas":"0x3b9aca00"})
    }
    fn store_transaction(&self, raw: &[u8]) -> (String, Value, Value) {
        let tx = TxEnvelope::decode_2718(&mut raw.to_vec().as_slice()).unwrap();
        let hash_value = format!("{:#x}", keccak256(raw));
        let block = self.block_number.fetch_add(1, Ordering::SeqCst) + 1;
        let to = lower_address(tx.to().unwrap());
        let input = tx.input().as_ref();
        let selector = &input[..4];
        let mut logs = Vec::new();
        if to.eq_ignore_ascii_case(ETH_USDC) {
            assert_eq!(selector, &FixtureErc20::approveCall::SELECTOR);
            let spender = Address::from_slice(&input[16..36]);
            assert_eq!(spender, addr(DEPOSITORY));
            let amount = U256::from_be_slice(&input[36..68]);
            let before = *self.allowance.lock().unwrap();
            *self.allowance.lock().unwrap() = amount;
            let mut history = self.allowance_at_block.lock().unwrap();
            history.insert(block - 1, before);
            history.insert(block, amount);
            logs.push(self.event_log(
                ETH_USDC,
                vec![
                    hash("Approval(address,address,uint256)"),
                    pad_topic(addr(&self.wallet)),
                    pad_topic(spender),
                ],
                word(amount),
                &hash_value,
                block,
                0,
            ));
        } else {
            assert_eq!(to, DEPOSITORY);
            assert_eq!(selector, &FixtureDepository::depositErc20Call::SELECTOR);
            let depositor = Address::from_slice(&input[16..36]);
            let token = Address::from_slice(&input[48..68]);
            let amount = U256::from_be_slice(&input[68..100]);
            let order = B256::from_slice(&input[100..132]);
            assert_eq!(depositor, addr(&self.wallet));
            assert_eq!(token, addr(ETH_USDC));
            let balance_before = *self.token_balance.lock().unwrap();
            assert!(
                balance_before >= amount,
                "deposit cannot exceed source USDC balance"
            );
            *self.token_balance.lock().unwrap() = balance_before - amount;
            // Model USDC transferFrom semantics: a finite allowance is reduced
            // by the amount actually transferred (the approved cap is not left
            // untouched after the deposit).
            let allowance_before = *self.allowance.lock().unwrap();
            assert!(
                allowance_before >= amount,
                "deposit cannot exceed finite token allowance"
            );
            let allowance_after = allowance_before - amount;
            *self.allowance.lock().unwrap() = allowance_after;
            let mut history = self.allowance_at_block.lock().unwrap();
            history.insert(block - 1, allowance_before);
            history.insert(block, allowance_after);
            let token_transfer = self.event_log(
                ETH_USDC,
                vec![
                    hash("Transfer(address,address,uint256)"),
                    pad_topic(depositor),
                    pad_topic(addr(DEPOSITORY)),
                ],
                word(amount),
                &hash_value,
                block,
                0,
            );
            let mut event_data = String::new();
            event_data.push_str(&word_address(depositor));
            event_data.push_str(&word_address(token));
            event_data.push_str(&word(amount));
            event_data.push_str(&hex::encode(order));
            let deposit_event = self.event_log(
                DEPOSITORY,
                vec![hash("RelayErc20Deposit(address,address,uint256,bytes32)")],
                event_data,
                &hash_value,
                block,
                1,
            );
            logs.extend([token_transfer, deposit_event]);
        }
        let receipt = json!({
            "transactionHash":hash_value,"blockNumber":format!("0x{block:x}"),"blockHash":BLOCK_HASH,"status":"0x1",
            "gasUsed":if to.eq_ignore_ascii_case(ETH_USDC) {"0x8ca0"} else {"0x11940"},
            "effectiveGasPrice":"0x3b9aca00","logs":logs
        });
        let transaction = json!({
            "hash":hash_value,"from":self.wallet,"to":to,"value":format!("{:#x}",tx.value()),
            "input":format!("0x{}",hex::encode(input)),"nonce":format!("{:#x}",tx.nonce()),
            "gas":format!("{:#x}",tx.gas_limit()),"chainId":format!("0x{:x}",self.chain_id),
            "type":"0x2","maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),
            "maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),"accessList":[],
            "blockNumber":format!("0x{block:x}"),"blockHash":BLOCK_HASH
        });
        (hash_value, transaction, receipt)
    }
}

#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(match method {
            "eth_chainId" => json!(format!("0x{:x}", self.chain_id)),
            "eth_getCode" => {
                let address = params[0].as_str().unwrap_or_default().to_ascii_lowercase();
                let block_tag = params.get(1).and_then(Value::as_str).unwrap_or_default();
                // Dynamic asset checks read code at the current pinned head.
                // This fixture denies older payout-state reads, not that exact
                // current-head contract metadata needed before source dispatch.
                let current_token_metadata = self.chain_id == 42161
                    && address.eq_ignore_ascii_case(ARB_USDC)
                    && u64::from_str_radix(block_tag.trim_start_matches("0x"), 16).ok()
                        == Some(self.block_number.load(Ordering::SeqCst));
                if block_tag.starts_with("0x")
                    && *self.historical_code_unavailable.lock().unwrap()
                    && !current_token_metadata
                {
                    return Err(fin_domain::FinError::new(
                        "RPC_HISTORICAL_STATE_UNAVAILABLE",
                        "rpc",
                        "synthetic historical code read is unavailable",
                        "retry the same read",
                    )
                    .with_details(json!({"http_status":400,"json_rpc_code":-32001})));
                }
                let wrong_token_network = (address.eq_ignore_ascii_case(ETH_USDC)
                    && self.chain_id != 1)
                    || (address.eq_ignore_ascii_case(ARB_USDC) && self.chain_id != 42161);
                if wrong_token_network || self.empty_code.lock().unwrap().contains(&address) {
                    json!("0x")
                } else {
                    json!("0x60006000")
                }
            }
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_call" => {
                let call = &params[0];
                let data = call["data"].as_str().unwrap_or("").trim_start_matches("0x");
                if data == hex::encode(FixtureErc20::decimalsCall {}.abi_encode()) {
                    // Precision belongs to the exact fixture contract on its
                    // network, independently of Relay candidate metadata.
                    let expected_token = match self.chain_id {
                        1 => ETH_USDC,
                        42161 => ARB_USDC,
                        _ => panic!("unexpected synthetic ERC-20 metadata network"),
                    };
                    if call["to"]
                        .as_str()
                        .is_some_and(|to| to.eq_ignore_ascii_case(expected_token))
                    {
                        json!(format!("0x{:064x}", U256::from(6)))
                    } else {
                        json!("0x")
                    }
                } else if data.starts_with("dd62ed3e") {
                    let fallback_tag = Value::String("latest".into());
                    let tag = params.get(1).unwrap_or(&fallback_tag);
                    let block = match tag.as_str().unwrap_or("latest") {
                        "latest" => self.block_number.load(Ordering::SeqCst),
                        value => {
                            u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap_or(0)
                        }
                    };
                    let allowance = self
                        .allowance_at_block
                        .lock()
                        .unwrap()
                        .range(..=block)
                        .next_back()
                        .map(|(_, v)| *v)
                        .unwrap_or(U256::ZERO);
                    json!(format!("0x{:064x}", allowance))
                } else if data.starts_with("095ea7b3") {
                    if !call["from"]
                        .as_str()
                        .is_some_and(|from| from.eq_ignore_ascii_case(&self.wallet))
                    {
                        return Err(fin_domain::FinError::new(
                            "SYNTHETIC_ZERO_OWNER_APPROVE_REVERT",
                            "simulation",
                            "USDC approve simulation requires the actual wallet caller.",
                            "Use a sender-aware eth_call.",
                        ));
                    }
                    json!(format!("0x{:064x}", U256::from(1)))
                } else if data.starts_with("70a08231") {
                    self.token_balance_reads.fetch_add(1, Ordering::SeqCst);
                    json!(format!("0x{:064x}", *self.token_balance.lock().unwrap()))
                } else {
                    json!("0x")
                }
            }
            "eth_estimateGas" => {
                self.estimate_gas_calls.fetch_add(1, Ordering::SeqCst);
                let to = params[0]["to"].as_str().unwrap_or("");
                json!(if to.eq_ignore_ascii_case(ETH_USDC) {
                    "0x9c40"
                } else {
                    "0x15f90"
                })
            }
            "eth_getTransactionCount" => {
                json!(format!("0x{:x}", self.broadcasts.load(Ordering::SeqCst)))
            }
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_blockNumber" => json!(format!("0x{:x}", self.block_number.load(Ordering::SeqCst))),
            "eth_getBlockByNumber" => self.block_value(&params[0]),
            "eth_sendRawTransaction" => {
                assert_eq!(self.chain_id, 1, "destination is observe-only");
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let (tx_hash, transaction, receipt) = self.store_transaction(&raw);
                self.sent
                    .lock()
                    .unwrap()
                    .insert(tx_hash.clone(), (transaction, receipt));
                self.broadcasts.fetch_add(1, Ordering::SeqCst);
                json!(tx_hash)
            }
            "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                let hash = params[0].as_str().unwrap_or_default();
                let stored = if self.chain_id == 42161 && hash == DESTINATION_TX {
                    self.destination_override
                        .lock()
                        .unwrap()
                        .clone()
                        .or_else(|| Some(self.destination_receipt()))
                } else {
                    self.sent.lock().unwrap().get(hash).cloned()
                };
                stored
                    .map(|(mut tx, mut receipt)| {
                        if self.chain_id == 1 {
                            match *self.transaction_mutation.lock().unwrap() {
                                Some("to") => {
                                    tx["to"] = json!("0x0000000000000000000000000000000000000004");
                                }
                                Some("value") => tx["value"] = json!("0x1"),
                                Some("calldata") => tx["input"] = json!("0x1234"),
                                Some("gas") => tx["gas"] = json!("0x1"),
                                Some("max_fee") => tx["maxFeePerGas"] = json!("0x1"),
                                Some("receipt_gas") => receipt["gasUsed"] = json!("0x100000"),
                                Some("receipt_price") => {
                                    receipt["effectiveGasPrice"] = json!("0xffffffffffffffff")
                                }
                                _ => {}
                            }
                        }
                        if method == "eth_getTransactionReceipt" {
                            receipt
                        } else {
                            tx
                        }
                    })
                    .unwrap_or(Value::Null)
            }
            other => panic!("unsupported synthetic RPC method {other}: {params}"),
        })
    }
}

pub struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if asset.reference == "native" {
                "2000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic ERC-20 bridge fixture".into(),
        })
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[tokio::test]
    async fn synthetic_token_metadata_binds_contract_and_network() {
        for (chain, token, wrong_network_token) in
            [(1, ETH_USDC, ARB_USDC), (42161, ARB_USDC, ETH_USDC)]
        {
            let rpc = Rpc::new(
                chain,
                "0x1111111111111111111111111111111111111111".into(),
                U256::ZERO,
            );
            let selector = format!(
                "0x{}",
                hex::encode(FixtureErc20::decimalsCall {}.abi_encode())
            );
            assert_eq!(
                rpc.request("eth_call", json!([{"to":token,"data":selector},"latest"]))
                    .await
                    .unwrap(),
                json!(format!("0x{:064x}", U256::from(6)))
            );
            assert_eq!(
                rpc.request(
                    "eth_call",
                    json!([{"to":wrong_network_token,"data":selector},"latest"])
                )
                .await
                .unwrap(),
                json!("0x")
            );
            assert_eq!(
                rpc.request("eth_getCode", json!([token, "latest"]))
                    .await
                    .unwrap(),
                json!("0x60006000")
            );
            assert_eq!(
                rpc.request("eth_getCode", json!([wrong_network_token, "latest"]))
                    .await
                    .unwrap(),
                json!("0x")
            );
        }
    }
}
