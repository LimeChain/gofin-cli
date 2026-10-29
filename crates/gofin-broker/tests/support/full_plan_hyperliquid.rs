//! Controlled Arbitrum and Hyperliquid transport for one-plan rehearsal.
//! Real adapters retain preparation, signing, authority, and settlement checks.
use super::bridge_fixture;
use async_trait::async_trait;
use gofin_domain::{AssetPrice, GoFinError, Quantity, Result, SubmissionJournal, now};
use gofin_evm::{
    EvmRpc,
    alloy::{
        consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable},
        eips::eip2718::Decodable2718,
        primitives::{Address, U256, keccak256},
        sol_types::{SolCall, SolValue},
    },
    lower_address,
};
use gofin_exchange::ccxt::{HyperliquidAction, HyperliquidActionEnvelope, HyperliquidInfoRequest};
use gofin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidPrices, HyperliquidProfile,
    HyperliquidTransport,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const BRIDGE: &str = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
const FILL_HASH: &str = "0xabababababababababababababababababababababababababababababababab";

gofin_evm::alloy::sol! {
    function transfer(address to, uint256 value) external returns (bool);
    function usdcToken() external view returns (address);
    function paused() external view returns (bool);
    function symbol() external view returns (string);
}

#[derive(Default)]
struct State {
    deposit: Option<(String, U256)>,
    transactions: BTreeMap<String, (Value, Value)>,
    leverage: u32,
    cross: bool,
    orders: BTreeMap<String, Value>,
    fills: Vec<Value>,
}

pub struct Fixture {
    wallet: String,
    inner: Arc<bridge_fixture::Rpc>,
    state: Mutex<State>,
    pub deposits: AtomicUsize,
    pub native_dispatches: AtomicUsize,
}

impl Fixture {
    pub fn new(wallet: String, inner: Arc<bridge_fixture::Rpc>) -> Arc<Self> {
        Arc::new(Self {
            wallet,
            inner,
            state: Mutex::new(State {
                leverage: 1,
                cross: true,
                ..State::default()
            }),
            deposits: AtomicUsize::new(0),
            native_dispatches: AtomicUsize::new(0),
        })
    }

    pub fn profile(self: &Arc<Self>) -> HyperliquidProfile {
        HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "synthetic-hl".into(),
                owner: self.wallet.clone(),
                credential_ref: Some("synthetic-hl-key".into()),
                signing_enabled: true,
                execution_scope: Default::default(),
            }],
            self.clone(),
            Arc::new(Prices),
            Arc::new(DeferredExecutionState::default()),
        )
        .unwrap()
    }
}

pub fn deposit_input(amount: &str) -> Value {
    json!({"requestId":"full-plan-deposit","account":"synthetic-hl","wallet":"synthetic-wallet",
        "amount":amount,"maxAmountUsdc":amount,"maxFeeEth":"0.001"})
}

pub fn leverage_input() -> Value {
    json!({"request_id":"full-plan-leverage","account":"synthetic-hl","symbol":"SOL/USDC:USDC",
        "leverage":3,"cross":false})
}

pub fn order_input() -> Value {
    json!({"request_id":"full-plan-order","account":"synthetic-hl","symbol":"SOL/USDC:USDC",
        "market_type":"perpetual","side":"buy","order_type":"limit","size":"0.34",
        "limit_price":"100","min_price":"100","time_in_force":"ioc","max_notional_usdc":"34",
        "max_fee_usdc":"0.1","max_realized_loss_usdc":"0"})
}

fn encoded<T: SolValue>(value: T) -> Value {
    json!(format!("0x{}", hex::encode(value.abi_encode())))
}

#[async_trait]
impl EvmRpc for Fixture {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call" {
            let raw = gofin_evm::rpc_bytes(&params[0]["data"])?;
            let selector = raw.get(..4).unwrap_or_default();
            if selector == usdcTokenCall::SELECTOR {
                assert_eq!(params[0]["to"], BRIDGE);
                return Ok(encoded(
                    bridge_fixture::ARB_USDC.parse::<Address>().unwrap(),
                ));
            }
            if selector == pausedCall::SELECTOR {
                assert_eq!(params[0]["to"], BRIDGE);
                return Ok(encoded(false));
            }
            if selector == symbolCall::SELECTOR {
                return Ok(encoded("USDC".to_owned()));
            }
            if selector == transferCall::SELECTOR {
                let call = transferCall::abi_decode(&raw).unwrap();
                assert_eq!(params[0]["to"], bridge_fixture::ARB_USDC);
                assert_eq!(lower_address(call.to), BRIDGE);
                if *self.inner.token_balance.lock().unwrap() < call.value {
                    return Err(GoFinError::validation(
                        "Synthetic deposit has no funded USDC.",
                    ));
                }
                return Ok(encoded(true));
            }
        }
        if method == "eth_sendRawTransaction" {
            let raw = gofin_evm::rpc_bytes(&params[0])?;
            let tx = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
            let sender = tx.recover_signer().unwrap();
            assert_eq!(lower_address(sender), self.wallet);
            assert_eq!(tx.chain_id(), Some(42161));
            assert_eq!(lower_address(tx.to().unwrap()), bridge_fixture::ARB_USDC);
            assert_eq!(tx.value(), U256::ZERO);
            let call = transferCall::abi_decode(tx.input()).unwrap();
            assert_eq!(lower_address(call.to), BRIDGE);
            let hash = format!("{:#x}", tx.tx_hash());
            let mut state = self.state.lock().unwrap();
            if state.transactions.contains_key(&hash) {
                return Ok(json!(hash));
            }
            let mut balance = self.inner.token_balance.lock().unwrap();
            assert!(
                *balance >= call.value,
                "deposit must spend actual bridge credit"
            );
            *balance -= call.value;
            let log = json!({"address":bridge_fixture::ARB_USDC,
                "topics":[format!("{:#x}",keccak256("Transfer(address,address,uint256)")),
                    format!("{:#x}",sender.into_word()),format!("{:#x}",call.to.into_word())],
                "data":format!("0x{:064x}",call.value),"transactionHash":hash,
                "blockNumber":"0x64","blockHash":BLOCK_HASH,"logIndex":"0x0","removed":false});
            let transaction = json!({"hash":hash,"from":self.wallet,"to":bridge_fixture::ARB_USDC,
                "value":"0x0","input":format!("0x{}",hex::encode(tx.input())),
                "nonce":format!("{:#x}",tx.nonce()),"gas":format!("{:#x}",tx.gas_limit()),
                "chainId":"0xa4b1","type":"0x2","maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),
                "maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),
                "accessList":[],"blockNumber":"0x64","blockHash":BLOCK_HASH});
            let receipt = json!({"transactionHash":hash,"blockNumber":"0x64","blockHash":BLOCK_HASH,
                "status":"0x1","gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","l1Fee":"0x0","logs":[log]});
            state
                .transactions
                .insert(hash.clone(), (transaction, receipt));
            state.deposit = Some((hash.clone(), call.value));
            self.deposits.fetch_add(1, Ordering::SeqCst);
            return Ok(json!(hash));
        }
        if method == "eth_getTransactionCount" {
            return Ok(json!(format!(
                "0x{:x}",
                self.deposits.load(Ordering::SeqCst)
            )));
        }
        if matches!(
            method,
            "eth_getTransactionByHash" | "eth_getTransactionReceipt"
        ) {
            let state = self.state.lock().unwrap();
            if let Some((tx, receipt)) = state
                .transactions
                .get(params[0].as_str().unwrap_or_default())
            {
                return Ok(if method == "eth_getTransactionReceipt" {
                    receipt
                } else {
                    tx
                }
                .clone());
            }
        }
        self.inner.request(method, params).await
    }
}

#[async_trait]
impl HyperliquidTransport for Fixture {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value> {
        let state = self.state.lock().unwrap();
        let deposited = state
            .deposit
            .as_ref()
            .map_or(U256::ZERO, |(_, value)| *value);
        let collateral = gofin_evm::quantity(deposited).to_decimal(6);
        let free = if state.fills.is_empty() {
            deposited
        } else {
            // $34 / 3 rounded up at USDC precision, plus the observed $.01 fee.
            deposited - U256::from(11_343_334u64)
        };
        Ok(match request {
            HyperliquidInfoRequest::UserRole { .. } => json!({"role":"user"}),
            HyperliquidInfoRequest::UserAbstractionState { .. } => json!({"abstraction":"disabled"}),
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => json!(false),
            HyperliquidInfoRequest::PerpDexs => json!([null]),
            HyperliquidInfoRequest::MetaAndAssetCtxs => json!([
                {"universe":[{"name":"SOL","szDecimals":2,"maxLeverage":20}]},
                [{"markPx":"100","midPx":"100","oraclePx":"100"}]
            ]),
            HyperliquidInfoRequest::ClearinghouseState { .. } => json!({
                "withdrawable":gofin_evm::quantity(free).to_decimal(6),"assetPositions":if state.fills.is_empty(){json!([])}else{
                    json!([{"position":{"coin":"SOL","szi":"0.34","entryPx":"100",
                        "leverage":{"type":"isolated","value":3},"marginUsed":"11.333334"}}])}
            }),
            HyperliquidInfoRequest::FrontendOpenOrders { .. } => json!([]),
            HyperliquidInfoRequest::ActiveAssetData { owner, coin } => {
                let capacity = gofin_evm::quantity(free).to_decimal(6);
                let max_size = gofin_evm::quantity(free * U256::from(state.leverage) / U256::from(100)).to_decimal(6);
                json!({"user":owner,"coin":coin,
                    "leverage":{"type":if state.cross{"cross"}else{"isolated"},"value":state.leverage},
                    "availableToTrade":[capacity,capacity],"maxTradeSzs":[max_size,max_size],"markPx":"100"})
            }
            HyperliquidInfoRequest::UserNonFundingLedgerUpdates { end_time_ms:Some(time), .. } => {
                state.deposit.as_ref().map_or_else(||json!([]), |(hash,_)|
                    json!([{"time":time,"hash":hash,"delta":{"type":"deposit","usdc":collateral}}]))
            }
            HyperliquidInfoRequest::OrderStatusByClientId { client_order_id, .. } => state.orders
                .get(&client_order_id).cloned().unwrap_or_else(||json!({"status":"unknownOid"})),
            HyperliquidInfoRequest::UserFillsByTime { .. } => json!(state.fills),
            other => return Err(GoFinError::validation(format!("Unexpected full-plan HL info: {other:?}"))),
        })
    }

    async fn dispatch(
        &self,
        account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value> {
        journal.assert_authorized()?;
        assert_eq!(account.owner, self.wallet);
        self.native_dispatches.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        match &envelope.action {
            HyperliquidAction::UpdateLeverage {
                leverage, is_cross, ..
            } => {
                state.leverage = *leverage;
                state.cross = *is_cross;
                Ok(json!({"status":"ok"}))
            }
            HyperliquidAction::Order { orders, .. } => {
                assert_eq!(orders.len(), 1);
                let order = &orders[0];
                assert_eq!(order.asset, 0);
                assert!(order.is_buy);
                assert_eq!(order.size, "0.34");
                assert_eq!(order.price, "100");
                state.orders.insert(
                    order.client_order_id.clone().unwrap(),
                    json!({"status":"order",
                    "order":{"status":"filled","order":{"coin":"SOL","side":"B",
                        "cloid":order.client_order_id,"origSz":order.size,"sz":"0","oid":100}}}),
                );
                state.fills.push(json!({"oid":100,"tid":101,"hash":FILL_HASH,"coin":"SOL","side":"B",
                    "time":envelope.nonce_ms,"sz":"0.34","px":"100","feeToken":"USDC","fee":"0.01","closedPnl":"0"}));
                Ok(
                    json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":100,"totalSz":"0.34","avgPx":"100"}}]}}}),
                )
            }
            other => Err(GoFinError::validation(format!(
                "Unexpected full-plan HL action: {other:?}"
            ))),
        }
    }
}

struct Prices;
#[async_trait]
impl HyperliquidPrices for Prices {
    async fn usdc_price(&self) -> Result<AssetPrice> {
        Ok(AssetPrice {
            asset: gofin_hyperliquid::usdc(),
            unit_atoms: Quantity::from_decimal("1", 36).unwrap(),
            usd_micros_per_unit: Quantity::from_u64(1_000_000),
            observed_at: now(),
            source: "synthetic full-plan USDC quote".into(),
        })
    }
}
