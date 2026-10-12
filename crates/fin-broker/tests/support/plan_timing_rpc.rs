//! In-memory transport for a withdrawal-funded real AMM adapter followed by Relay.
//! Public synthetic signing key only; no network fallback exists.
use super::fixture::{ETH_USDC, Rpc};
use async_trait::async_trait;
use fin_domain::{FinError, Result, clock::Clock};
use fin_evm::{
    EvmRpc,
    alloy::{
        consensus::{Transaction, TxEnvelope},
        eips::eip2718::Decodable2718,
        primitives::{
            Address, U256,
            aliases::{I24, U160},
            keccak256,
        },
        sol_types::{SolCall, SolEvent, SolValue},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
pub const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub const FACTORY: &str = "0x0000000000000000000000000000000000000450";
pub const QUOTER: &str = "0x0000000000000000000000000000000000000460";
pub const SWAP_ROUTER: &str = "0x0000000000000000000000000000000000000470";
pub const POOL: &str = "0x0000000000000000000000000000000000000500";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
pub fn withdrawal_hash() -> String {
    format!("0x{}", "99".repeat(32))
}
fin_evm::alloy::sol! {
    interface Metadata { function symbol() external view returns(string); function decimals() external view returns(uint8); }
    interface Factory { function getPool(address,address,uint24) external view returns(address); }
    interface RouterInfo { function WETH9() external view returns(address); }
    interface PoolInfo { function token0() external view returns(address); function token1() external view returns(address); function slot0() external view returns(uint160,int24,uint16,uint16,uint16,uint8,bool); event Swap(address indexed sender,address indexed recipient,int256 amount0,int256 amount1,uint160 sqrtPriceX96,uint128 liquidity,int24 tick); }
    interface Quoter { struct Params{address tokenIn;address tokenOut;uint256 amountIn;uint24 fee;uint160 sqrtPriceLimitX96;} function quoteExactInputSingle(Params p) external returns(uint256,uint160,uint32,uint256); }
    interface Weth { event Deposit(address indexed dst,uint256 wad); }
    interface Token { event Transfer(address indexed from,address indexed to,uint256 value); }
}
fn addr(value: &str) -> Address {
    value.parse().unwrap()
}
fn encoded<T: SolValue>(value: T) -> Value {
    json!(format!("0x{}", hex::encode(value.abi_encode())))
}
fn event<T: SolEvent>(address: &str, event: T, hash: &str, block: u64, index: usize) -> Value {
    let log = event.encode_log_data();
    json!({"address":address,"topics":log.topics().iter().map(|v|format!("{v:#x}")).collect::<Vec<_>>(),"data":format!("0x{}",hex::encode(&log.data)),"transactionHash":hash,"blockHash":BLOCK_HASH,"blockNumber":format!("{block:#x}"),"logIndex":format!("{index:#x}"),"removed":false})
}
pub struct WorkflowRpc {
    pub inner: Arc<Rpc>,
    pub clock: Arc<dyn Clock>,
    pub swaps: AtomicUsize,
    pub withdrawal_ready: std::sync::atomic::AtomicBool,
    pub swap_receipt_ready: std::sync::atomic::AtomicBool,
    swap_included_at: Mutex<Option<i64>>,
    transactions: Mutex<BTreeMap<String, (Value, Value)>>,
}
impl WorkflowRpc {
    pub fn new(inner: Arc<Rpc>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner,
            clock,
            swaps: AtomicUsize::new(0),
            withdrawal_ready: std::sync::atomic::AtomicBool::new(false),
            swap_receipt_ready: std::sync::atomic::AtomicBool::new(true),
            swap_included_at: Mutex::new(None),
            transactions: Mutex::new(BTreeMap::new()),
        }
    }
    pub async fn runtime_balance(&self) -> U256 {
        fin_evm::rpc_u256(
            &self
                .request("eth_getBalance", json!([self.inner.wallet, "latest"]))
                .await
                .unwrap(),
        )
        .unwrap()
    }
    fn withdrawal(&self) -> (Value, Value) {
        let hash = withdrawal_hash();
        (
            json!({"hash":hash,"from":"0x0000000000000000000000000000000000000003","to":self.inner.wallet,"nonce":"0x0","chainId":"0x1","type":"0x2","gas":"0x5208","value":"0x1550f7dca70000","input":"0x","maxFeePerGas":"0x3b9aca00","maxPriorityFeePerGas":"0x3b9aca00","accessList":[],"blockNumber":"0x63","blockHash":BLOCK_HASH}),
            json!({"transactionHash":hash,"blockHash":BLOCK_HASH,"blockNumber":"0x63","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x1","logs":[]}),
        )
    }
    fn native_balance(&self, block: Option<u64>) -> U256 {
        let baseline = U256::from(100_000_000_000_000_000u64); // existing gas, not plan proceeds
        let credit =
            if self.withdrawal_ready.load(Ordering::SeqCst) && block.is_none_or(|b| b >= 99) {
                U256::from(6_000_000_000_000_000u64)
            } else {
                U256::ZERO
            };
        let spent = if self.swaps.load(Ordering::SeqCst) > 0 && block.is_none_or(|b| b >= 101) {
            U256::from(4_000_000_000_000_000u64) + U256::from(100_000u64)
        } else {
            U256::ZERO
        };
        // This scenario has one bridge deposit: the independent fixture receipt
        // reports 72,000 gas at 1 gwei in block 102.
        let bridge_fee = if self.inner.broadcasts() > 0 && block.is_none_or(|b| b >= 102) {
            U256::from(72_000_000_000_000u64)
        } else {
            U256::ZERO
        };
        baseline + credit - spent - bridge_fee
    }
}
#[async_trait]
impl EvmRpc for WorkflowRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "eth_maxPriorityFeePerGas" => return Ok(json!("0x1")),
            "eth_estimateGas"
                if params[0]["to"]
                    .as_str()
                    .is_some_and(|to| to.eq_ignore_ascii_case(SWAP_ROUTER)) =>
            {
                return Ok(json!("0x186a0"));
            }
            "eth_getTransactionCount" => {
                return Ok(json!(format!(
                    "{:#x}",
                    self.swaps.load(Ordering::SeqCst) + self.inner.broadcasts()
                )));
            }
            "eth_getBalance" => {
                let block = params
                    .get(1)
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("0x"))
                    .and_then(|s| u64::from_str_radix(&s[2..], 16).ok());
                return Ok(json!(format!("{:#x}", self.native_balance(block))));
            }
            "eth_getCode"
                if params[0]
                    .as_str()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&self.inner.wallet)) =>
            {
                return Ok(json!("0x"));
            }
            "eth_getBlockByNumber" => {
                let mut value = self.inner.request(method, params.clone()).await?;
                value["timestamp"] = json!(format!("{:#x}", self.clock.now().timestamp()));
                let block = params[0]
                    .as_str()
                    .filter(|s| s.starts_with("0x"))
                    .and_then(|s| u64::from_str_radix(&s[2..], 16).ok());
                if block == Some(101) {
                    if let Some(at) = *self.swap_included_at.lock().unwrap() {
                        value["timestamp"] = json!(format!("{at:#x}"));
                    }
                    value["transactions"] = json!(
                        self.transactions
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|(tx, _)| tx["blockNumber"] == "0x65")
                            .map(|(tx, _)| tx.clone())
                            .collect::<Vec<_>>()
                    );
                }
                return Ok(value);
            }
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
                let hash = params[0].as_str().unwrap();
                let saved =
                    if hash == withdrawal_hash() && self.withdrawal_ready.load(Ordering::SeqCst) {
                        Some(self.withdrawal())
                    } else {
                        self.transactions.lock().unwrap().get(hash).cloned()
                    };
                if let Some((tx, receipt)) = saved {
                    if hash != withdrawal_hash()
                        && method == "eth_getTransactionReceipt"
                        && !self.swap_receipt_ready.load(Ordering::SeqCst)
                    {
                        return Ok(Value::Null);
                    }
                    return Ok(if method == "eth_getTransactionReceipt" {
                        receipt
                    } else {
                        tx
                    });
                }
            }
            "eth_call" => {
                let data = fin_evm::rpc_bytes(&params[0]["data"])?;
                let selector = data.get(..4).unwrap_or(&[]);
                let to = params[0]["to"].as_str().unwrap_or("");
                if selector == Metadata::symbolCall::SELECTOR {
                    return Ok(encoded(if to.eq_ignore_ascii_case(WETH) {
                        "WETH".to_owned()
                    } else {
                        "USDC".to_owned()
                    }));
                }
                if selector == Metadata::decimalsCall::SELECTOR && to.eq_ignore_ascii_case(WETH) {
                    return Ok(encoded(U256::from(18)));
                }
                if selector == Factory::getPoolCall::SELECTOR {
                    return Ok(encoded(addr(POOL)));
                }
                if selector == RouterInfo::WETH9Call::SELECTOR {
                    return Ok(encoded(addr(WETH)));
                }
                if selector == PoolInfo::token0Call::SELECTOR {
                    return Ok(encoded(addr(ETH_USDC)));
                }
                if selector == PoolInfo::token1Call::SELECTOR {
                    return Ok(encoded(addr(WETH)));
                }
                if selector == PoolInfo::slot0Call::SELECTOR {
                    return Ok(encoded((
                        U160::from(1u64) << 96,
                        I24::ZERO,
                        0u16,
                        0u16,
                        0u16,
                        0u32,
                        true,
                    )));
                }
                if selector == Quoter::quoteExactInputSingleCall::SELECTOR {
                    return Ok(encoded((
                        U256::from(10_000_000u64),
                        U160::ZERO,
                        0u32,
                        U256::from(100_000u64),
                    )));
                }
                if to.eq_ignore_ascii_case(SWAP_ROUTER) && params[0].get("from").is_some() {
                    return Ok(encoded(U256::from(10_000_000u64)));
                }
            }
            "eth_sendRawTransaction" => {
                let raw = fin_evm::rpc_bytes(&params[0])?;
                let tx = TxEnvelope::decode_2718(&mut raw.as_ref())
                    .map_err(|_| FinError::validation("Invalid isolated transaction."))?;
                if tx.to() == Some(addr(SWAP_ROUTER)) {
                    assert_eq!(
                        self.swaps.load(Ordering::SeqCst),
                        0,
                        "one immutable swap submission"
                    );
                    let hash = format!("{:#x}", keccak256(&raw));
                    let owner = addr(&self.inner.wallet);
                    let amount = U256::from(4_000_000_000_000_000u64);
                    assert_eq!(tx.value(), amount);
                    let logs = vec![
                        event(
                            POOL,
                            PoolInfo::Swap {
                                sender: addr(SWAP_ROUTER),
                                recipient: owner,
                                amount0: -fin_evm::alloy::primitives::I256::from_raw(U256::from(
                                    10_000_000u64,
                                )),
                                amount1: fin_evm::alloy::primitives::I256::from_raw(amount),
                                sqrtPriceX96: U160::ZERO,
                                liquidity: 1,
                                tick: I24::ZERO,
                            },
                            &hash,
                            101,
                            0,
                        ),
                        event(
                            WETH,
                            Weth::Deposit {
                                dst: addr(SWAP_ROUTER),
                                wad: amount,
                            },
                            &hash,
                            101,
                            1,
                        ),
                        event(
                            WETH,
                            Token::Transfer {
                                from: addr(SWAP_ROUTER),
                                to: addr(POOL),
                                value: amount,
                            },
                            &hash,
                            101,
                            2,
                        ),
                        event(
                            ETH_USDC,
                            Token::Transfer {
                                from: addr(POOL),
                                to: owner,
                                value: U256::from(10_000_000u64),
                            },
                            &hash,
                            101,
                            3,
                        ),
                    ];
                    let transaction = json!({"hash":hash,"from":self.inner.wallet,"to":SWAP_ROUTER,"nonce":format!("{:#x}",tx.nonce()),"chainId":"0x1","type":"0x2","gas":format!("{:#x}",tx.gas_limit()),"value":format!("{:#x}",tx.value()),"input":format!("0x{}",hex::encode(tx.input())),"maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),"maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":"0x65","blockHash":BLOCK_HASH});
                    let receipt = json!({"transactionHash":hash,"blockHash":BLOCK_HASH,"blockNumber":"0x65","status":"0x1","gasUsed":"0x186a0","effectiveGasPrice":"0x1","logs":logs});
                    self.transactions
                        .lock()
                        .unwrap()
                        .insert(hash.clone(), (transaction, receipt));
                    self.swaps.fetch_add(1, Ordering::SeqCst);
                    *self.swap_included_at.lock().unwrap() = Some(self.clock.now().timestamp());
                    *self.inner.token_balance.lock().unwrap() = U256::from(10_000_000u64);
                    self.inner.advance_head();
                    return Ok(json!(hash));
                }
            }
            _ => {}
        }
        self.inner.request(method, params).await
    }
}
