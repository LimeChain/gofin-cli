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
use gofin_exchange::ccxt::{
    HyperliquidAction, HyperliquidActionEnvelope, HyperliquidExplorerRequest,
    HyperliquidInfoRequest, HyperliquidOrderType, HyperliquidTimeInForce,
};
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
    perp_usdc: i128,
    spot_usdc: i128,
    spot_eth: i128,
    position: i128,
    ledger: Vec<Value>,
    explorer: Vec<Value>,
    pub receipt: Option<Value>,
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
        .with_withdrawal_proof(self.clone())
    }
    pub fn set_receipt(&self, receipt: Value) {
        self.state.lock().unwrap().receipt = Some(receipt);
    }
    pub fn cash(&self) -> (i128, i128, i128, i128) {
        let s = self.state.lock().unwrap();
        (s.perp_usdc, s.spot_usdc, s.spot_eth, s.position)
    }
    pub fn open_oid(&self) -> u64 {
        self.state
            .lock()
            .unwrap()
            .orders
            .values()
            .find(|v| v["order"]["status"] == "open")
            .unwrap()["order"]["order"]["oid"]
            .as_u64()
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
            state.perp_usdc += gofin_evm::quantity(call.value)
                .as_str()
                .parse::<i128>()
                .unwrap();
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
        let s = self.state.lock().unwrap();
        let spot = decimal(s.spot_usdc);
        let eth = decimal(s.spot_eth);
        let margin = (s.position.abs() * 100 + i128::from(s.leverage) - 1) / i128::from(s.leverage);
        let available = decimal(s.perp_usdc - margin);
        let open_orders: Vec<Value> = s
            .orders
            .values()
            .filter(|row| row["order"]["status"] == "open")
            .map(|row| row["order"]["order"].clone())
            .collect();
        let held_eth: i128 = open_orders
            .iter()
            .filter(|order| order["coin"] == "@0" && order["side"] == "A")
            .map(|order| units(order["sz"].as_str().unwrap()))
            .sum();
        Ok(match request {
            HyperliquidInfoRequest::UserRole { .. } => json!({"role":"user"}),
            HyperliquidInfoRequest::UserAbstractionState { .. } => {
                json!({"abstraction":"disabled"})
            }
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => json!(false),
            HyperliquidInfoRequest::PerpDexs => json!([null]),
            HyperliquidInfoRequest::MetaAndAssetCtxs => {
                json!([{"universe":[{"name":"SOL","szDecimals":2,"maxLeverage":20}]},[{"markPx":"100","midPx":"100","oraclePx":"100"}]])
            }
            HyperliquidInfoRequest::SpotMetaAndAssetCtxs => {
                json!([{"tokens":[{"name":"ETH","index":1,"weiDecimals":18,"szDecimals":4,"tokenId":"0xeeee"},{"name":"USDC","index":0,"weiDecimals":8,"szDecimals":8,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054"}],"universe":[{"name":"ETH/USDC","index":0,"tokens":[1,0]}]},[{"midPx":"10"}]])
            }
            HyperliquidInfoRequest::SpotClearinghouseState { .. } => {
                json!({"balances":[{"coin":"USDC","token":0,"total":spot,"hold":"0"},{"coin":"ETH","token":1,"total":eth,"hold":decimal(held_eth)}]})
            }
            HyperliquidInfoRequest::ClearinghouseState { .. } => {
                json!({"withdrawable":available,"assetPositions":if s.position==0 {json!([])}else{json!([{"position":{"coin":"SOL","szi":decimal(s.position),"entryPx":"100","leverage":{"type":if s.cross {"cross"}else{"isolated"},"value":s.leverage},"marginUsed":decimal(margin)}}])}})
            }
            HyperliquidInfoRequest::FrontendOpenOrders { .. } => json!(open_orders),
            HyperliquidInfoRequest::ActiveAssetData { owner, coin } => {
                json!({"user":owner,"coin":coin,"leverage":{"type":if s.cross {"cross"}else{"isolated"},"value":s.leverage},"availableToTrade":[available,available],"maxTradeSzs":["100","100"],"markPx":"100"})
            }
            HyperliquidInfoRequest::UserNonFundingLedgerUpdates { end_time_ms, .. } => {
                let mut rows = s.ledger.clone();
                if let Some((hash, amount)) = &s.deposit {
                    rows.push(json!({"time":end_time_ms.unwrap_or(1),"hash":hash,"delta":{"type":"deposit","usdc":gofin_evm::quantity(*amount).to_decimal(6)}}));
                }
                json!(rows)
            }
            HyperliquidInfoRequest::OrderStatus { order_id, .. } => s
                .orders
                .values()
                .find(|v| v["order"]["order"]["oid"] == order_id)
                .cloned()
                .unwrap_or_else(|| json!({"status":"unknownOid"})),
            HyperliquidInfoRequest::OrderStatusByClientId {
                client_order_id, ..
            } => s
                .orders
                .get(&client_order_id)
                .cloned()
                .unwrap_or_else(|| json!({"status":"unknownOid"})),
            HyperliquidInfoRequest::UserFillsByTime { .. } => json!(s.fills),
            other => {
                return Err(GoFinError::validation(format!(
                    "unexpected HL positive info: {other:?}"
                )));
            }
        })
    }
    async fn explorer(&self, request: HyperliquidExplorerRequest) -> Result<Value> {
        let s = self.state.lock().unwrap();
        Ok(match request {
            HyperliquidExplorerRequest::UserDetails { .. } => json!({"txs":s.explorer}),
            HyperliquidExplorerRequest::TxDetails { hash } => {
                json!({"tx":s.explorer.iter().find(|t|t["hash"]==hash).cloned().unwrap_or(Value::Null)})
            }
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
        let mut s = self.state.lock().unwrap();
        match &envelope.action {
            HyperliquidAction::UpdateLeverage {
                leverage, is_cross, ..
            } => {
                s.leverage = *leverage;
                s.cross = *is_cross;
                Ok(json!({"status":"ok"}))
            }
            HyperliquidAction::Order { orders, .. } => {
                assert_eq!(orders.len(), 1);
                let order = &orders[0];
                let client = order.client_order_id.clone().unwrap();
                let oid = 100 + self.native_dispatches.load(Ordering::SeqCst) as u64;
                let spot = order.asset != 0;
                let coin = if spot { "@0" } else { "SOL" };
                let side = if order.is_buy { "B" } else { "A" };
                let resting = matches!(
                    order.order_type,
                    HyperliquidOrderType::Limit {
                        time_in_force: HyperliquidTimeInForce::Gtc
                    }
                );
                let size = units(&order.size);
                let price = units(&order.price);
                let value = size * price / 1_000_000;
                let fee = 10_000;
                if !resting {
                    if spot {
                        if order.is_buy {
                            s.spot_usdc -= value + fee;
                            s.spot_eth += size;
                        } else if size == units("1.01") {
                            s.spot_usdc += value;
                            s.spot_eth -= size + fee;
                        } else {
                            s.spot_usdc += value - fee;
                            s.spot_eth -= size;
                        }
                        assert!(s.spot_usdc >= 0 && s.spot_eth >= 0);
                    } else {
                        let change = if order.is_buy { size } else { -size };
                        if order.reduce_only {
                            assert!(
                                s.position != 0
                                    && s.position.signum() != change.signum()
                                    && s.position.abs() >= size
                            );
                        } else {
                            assert_eq!(s.position, 0);
                        }
                        s.position += change;
                        s.perp_usdc -= fee;
                        assert!(s.perp_usdc >= 0);
                    }
                    s.fills.push(json!({"oid":oid,"tid":oid+1000,"hash":format!("0x{:064x}",oid),"coin":coin,"side":side,"time":envelope.nonce_ms,"sz":order.size,"px":order.price,"feeToken":if spot && !order.is_buy && size==units("1.01") {"ETH"}else{"USDC"},"fee":"0.01","closedPnl":"0"}));
                }
                s.orders.insert(client,json!({"status":"order","order":{"status":if resting {"open"}else{"filled"},"order":{"coin":coin,"side":side,"cloid":order.client_order_id,"origSz":order.size,"sz":if resting {order.size.as_str()}else{"0"},"oid":oid}}}));
                Ok(
                    json!({"status":"ok","response":{"type":"order","data":{"statuses":[if resting {json!({"resting":{"oid":oid}})}else{json!({"filled":{"oid":oid,"totalSz":order.size,"avgPx":order.price}})}]}}}),
                )
            }
            HyperliquidAction::Cancel { cancels } => {
                for cancel in cancels {
                    let row = s
                        .orders
                        .values_mut()
                        .find(|r| r["order"]["order"]["oid"] == cancel.order_id)
                        .unwrap();
                    assert_eq!(row["order"]["status"], "open");
                    row["order"]["status"] = json!("canceled");
                }
                Ok(
                    json!({"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}),
                )
            }
            HyperliquidAction::UsdClassTransfer {
                amount_usdc,
                to_perp,
                ..
            } => {
                let amount = units(amount_usdc);
                if *to_perp {
                    assert!(s.spot_usdc >= amount);
                    s.spot_usdc -= amount;
                    s.perp_usdc += amount;
                } else {
                    assert!(s.perp_usdc >= amount);
                    s.perp_usdc -= amount;
                    s.spot_usdc += amount;
                }
                let hash = format!("0x{:064x}", envelope.nonce_ms);
                let action = envelope.action_wire_json().unwrap()["action"].clone();
                s.explorer.push(json!({"hash":hash,"user":account.owner,"action":action,"error":null,"block":123,"time":envelope.nonce_ms}));
                s.ledger.push(json!({"hash":hash,"time":envelope.nonce_ms,"delta":{"type":"accountClassTransfer","toPerp":to_perp,"usdc":amount_usdc}}));
                Ok(json!({"status":"ok"}))
            }
            HyperliquidAction::Withdraw {
                destination,
                amount_usdc,
                time_ms,
            } => {
                let gross = units(amount_usdc);
                assert!(s.perp_usdc >= gross);
                s.perp_usdc -= gross;
                let fee = 10_000;
                let net = gross - fee;
                let action_hash = format!("0x{:064x}", envelope.nonce_ms);
                let ledger_hash = format!("0x{:064x}", envelope.nonce_ms + 1);
                let action = envelope.action_wire_json().unwrap()["action"].clone();
                s.explorer.push(json!({"hash":action_hash,"user":account.owner,"action":action,"error":null,"block":123,"time":time_ms}));
                s.ledger.push(json!({"time":time_ms+1,"hash":ledger_hash,"delta":{"type":"withdraw","usdc":decimal(net),"nonce":time_ms*1000,"fee":"0.01"}}));
                assert_eq!(destination, &self.wallet);
                Ok(json!({"status":"ok"}))
            }
            other => Err(GoFinError::validation(format!(
                "unexpected HL positive action {other:?}"
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

fn units(value: &str) -> i128 {
    Quantity::from_decimal(value, 6)
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
}
fn decimal(value: i128) -> String {
    let n = Quantity::new(value.unsigned_abs().to_string())
        .unwrap()
        .to_decimal(6);
    if value < 0 { format!("-{n}") } else { n }
}

#[async_trait]
impl gofin_hyperliquid::account_actions::HyperliquidWithdrawalProof for Fixture {
    async fn prove_sequenced_arbitrum_withdrawal(
        &self,
        request: &gofin_hyperliquid::account_actions::WithdrawalProofRequest,
    ) -> Result<Option<gofin_hyperliquid::account_actions::WithdrawalReceiptProof>> {
        let s = self.state.lock().unwrap();
        let Some(r) = s.receipt.as_ref() else {
            return Ok(None);
        };
        if r["status"] != "0x1"
            || r["chainId"] != 42161
            || r["sender"] != BRIDGE
            || r["hash"] != request.ledger_hash
            || r["recipient"] != request.destination
            || r["token"] != request.token
            || r["amount"] != request.net_amount.to_decimal(36)
        {
            return Ok(None);
        };
        Ok(Some(
            gofin_hyperliquid::account_actions::WithdrawalReceiptProof {
                transaction_hash: request.ledger_hash.clone(),
                sender: BRIDGE.into(),
                recipient: request.destination.clone(),
                token: request.token.clone(),
                net_amount: Quantity::from_decimal(&request.net_amount.to_decimal(36), 6)?,
                sequenced: true,
                block_number: 100,
                event_digest: "synthetic-exact-arbitrum-log".into(),
            },
        ))
    }
}
