//! Stateful Ethereum transport for the cross-venue plan rehearsal. All keys,
//! balances, protocol contracts and receipts are synthetic; no network fallback.
use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_ethereum::{AaveConfig, EthereumConfig};
use gofin_evm::{
    EvmRpc,
    alloy::{
        consensus::{Transaction, TxEnvelope, transaction::SignerRecoverable},
        eips::eip2718::Decodable2718,
        primitives::{Address, B256, U256, aliases::U40, keccak256},
        sol_types::{SolCall, SolEvent, SolValue},
    },
    lower_address,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
#[path = "../../../gofin-ethereum/src/abi.rs"]
mod abi;
use abi::*;
pub const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub const USDC: &str = super::bridge_fixture::ETH_USDC;
pub const POOL: &str = "0x0000000000000000000000000000000000000300";
pub const ORACLE: &str = "0x0000000000000000000000000000000000000400";
const BLOCK_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";
fn addr(s: &str) -> Address {
    s.parse().unwrap()
}
fn internal(n: u64) -> Address {
    Address::from_word(B256::from(U256::from(n).to_be_bytes::<32>()))
}
fn ray() -> U256 {
    U256::from(10).pow(U256::from(27))
}
fn encoded<T: SolValue>(v: T) -> Value {
    json!(format!("0x{}", hex::encode(v.abi_encode())))
}
fn event<T: SolEvent>(address: Address, e: T, hash: &str, block: u64, index: usize) -> Value {
    let l = e.encode_log_data();
    json!({"address":lower_address(address),"topics":l.topics().iter().map(|t|format!("{t:#x}")).collect::<Vec<_>>(),"data":format!("0x{}",hex::encode(&l.data)),"transactionHash":hash,"blockHash":BLOCK_HASH,"blockNumber":format!("{block:#x}"),"logIndex":format!("{index:#x}"),"removed":false})
}
pub fn withdrawal_hash() -> String {
    format!("0x{}", "99".repeat(32))
}
pub fn config() -> EthereumConfig {
    EthereumConfig {
        weth: Some(WETH.into()),
        snapshot_tokens: vec![WETH.into(), USDC.into()],
        aave: Some(AaveConfig {
            pool: POOL.into(),
            oracle: ORACLE.into(),
        }),
        ..Default::default()
    }
}
#[derive(Clone)]
struct State {
    native: U256,
    weth: U256,
    usdc: U256,
    allowance: U256,
    supply: U256,
    debt: U256,
}
pub struct WorkflowEvm {
    pub wallet: String,
    pub bridge: Arc<super::bridge_fixture::Rpc>,
    height: AtomicU64,
    nonce: AtomicU64,
    pub writes: AtomicUsize,
    states: Mutex<BTreeMap<u64, State>>,
    transactions: Mutex<BTreeMap<String, (Value, Value)>>,
}
impl WorkflowEvm {
    pub fn new(wallet: String, bridge: Arc<super::bridge_fixture::Rpc>) -> Self {
        *bridge.token_balance.lock().unwrap() = U256::ZERO;
        Self {
            wallet,
            bridge,
            height: AtomicU64::new(100),
            nonce: AtomicU64::new(0),
            writes: AtomicUsize::new(0),
            states: Mutex::new(BTreeMap::from([(
                100,
                State {
                    native: U256::from(10_000_000_000_000_000u64),
                    weth: U256::ZERO,
                    usdc: U256::ZERO,
                    allowance: U256::ZERO,
                    supply: U256::ZERO,
                    debt: U256::ZERO,
                },
            )])),
            transactions: Mutex::new(BTreeMap::new()),
        }
    }
    fn state(&self, tag: &Value) -> State {
        let block = tag
            .as_str()
            .and_then(|v| v.strip_prefix("0x"))
            .and_then(|v| u64::from_str_radix(v, 16).ok())
            .unwrap_or(self.height.load(Ordering::SeqCst));
        self.states
            .lock()
            .unwrap()
            .range(..=block)
            .next_back()
            .unwrap()
            .1
            .clone()
    }
    pub fn native(&self) -> U256 {
        self.state(&Value::Null).native
    }
    pub fn weth(&self) -> U256 {
        self.state(&Value::Null).weth
    }
    pub fn usdc(&self) -> U256 {
        *self.bridge.token_balance.lock().unwrap()
    }
    pub fn supply(&self) -> U256 {
        self.state(&Value::Null).supply
    }
    pub fn debt(&self) -> U256 {
        self.state(&Value::Null).debt
    }
    pub fn allowance(&self) -> U256 {
        self.state(&Value::Null).allowance
    }
    pub fn credit_withdrawal(&self, amount: U256) {
        assert!(
            !self
                .transactions
                .lock()
                .unwrap()
                .contains_key(&withdrawal_hash())
        );
        let mut state = self.state(&Value::Null);
        state.native += amount;
        let block = self.height.fetch_add(2, Ordering::SeqCst) + 1;
        self.states.lock().unwrap().insert(block, state);
        let hash = withdrawal_hash();
        let tx = json!({"hash":hash,"from":lower_address(internal(900)),"to":self.wallet,"value":format!("{amount:#x}"),"input":"0x","nonce":"0x0","gas":"0x5208","chainId":"0x1","type":"0x2","maxFeePerGas":"0x3b9aca00","maxPriorityFeePerGas":"0x1","accessList":[],"blockNumber":format!("{block:#x}"),"blockHash":BLOCK_HASH});
        let receipt = json!({"transactionHash":hash,"blockNumber":format!("{block:#x}"),"blockHash":BLOCK_HASH,"status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x3b9aca00","logs":[]});
        self.transactions
            .lock()
            .unwrap()
            .insert(hash, (tx, receipt));
    }
    fn submit(&self, tx: &TxEnvelope, raw: &[u8]) -> Result<Value> {
        assert_eq!(tx.chain_id(), Some(1));
        assert_eq!(tx.recover_signer().unwrap(), addr(&self.wallet));
        assert_eq!(tx.nonce(), self.nonce.load(Ordering::SeqCst));
        let hash = format!("{:#x}", keccak256(raw));
        assert!(!self.transactions.lock().unwrap().contains_key(&hash));
        let block = self.height.fetch_add(2, Ordering::SeqCst) + 1;
        let mut state = self.state(&Value::Null);
        let owner = addr(&self.wallet);
        let to = tx.to().unwrap();
        let mut logs = Vec::new();
        if to == addr(WETH) && tx.input().starts_with(&Weth::depositCall::SELECTOR) {
            assert!(state.native >= tx.value());
            state.native -= tx.value();
            state.weth += tx.value();
            logs.push(event(
                to,
                Weth::Deposit {
                    dst: owner,
                    wad: tx.value(),
                },
                &hash,
                block,
                0,
            ));
        } else if to == addr(WETH) && tx.input().starts_with(&Erc20::approveCall::SELECTOR) {
            let c = Erc20::approveCall::abi_decode(tx.input()).unwrap();
            assert_eq!(c.spender, addr(POOL));
            assert!(c.amount < U256::MAX);
            state.allowance = c.amount;
            logs.push(event(
                to,
                Erc20::Approval {
                    owner,
                    spender: c.spender,
                    value: c.amount,
                },
                &hash,
                block,
                0,
            ));
        } else if to == addr(POOL) && tx.input().starts_with(&Pool::supplyCall::SELECTOR) {
            let c = Pool::supplyCall::abi_decode(tx.input()).unwrap();
            assert_eq!(c.asset, addr(WETH));
            assert_eq!(c.onBehalfOf, owner);
            assert!(state.weth >= c.amount && state.allowance >= c.amount);
            state.weth -= c.amount;
            state.allowance -= c.amount;
            state.supply += c.amount;
            logs.push(event(
                addr(WETH),
                Erc20::Transfer {
                    from: owner,
                    to: internal(110),
                    value: c.amount,
                },
                &hash,
                block,
                0,
            ));
            logs.push(event(
                to,
                Pool::Supply {
                    reserve: c.asset,
                    user: owner,
                    onBehalfOf: owner,
                    amount: c.amount,
                    referralCode: 0,
                },
                &hash,
                block,
                1,
            ));
            logs.push(event(
                internal(110),
                AToken::Mint {
                    caller: owner,
                    onBehalfOf: owner,
                    value: c.amount,
                    balanceIncrease: U256::ZERO,
                    index: ray(),
                },
                &hash,
                block,
                2,
            ));
            logs.push(event(
                internal(110),
                Erc20::Transfer {
                    from: Address::ZERO,
                    to: owner,
                    value: c.amount,
                },
                &hash,
                block,
                3,
            ));
            logs.push(event(
                to,
                Pool::ReserveUsedAsCollateralEnabled {
                    reserve: c.asset,
                    user: owner,
                },
                &hash,
                block,
                4,
            ));
        } else if to == addr(POOL) && tx.input().starts_with(&Pool::borrowCall::SELECTOR) {
            let c = Pool::borrowCall::abi_decode(tx.input()).unwrap();
            assert_eq!(c.asset, addr(USDC));
            assert_eq!(c.onBehalfOf, owner);
            assert_eq!(c.interestRateMode, U256::from(2));
            assert!(
                state.supply * U256::from(2000) * U256::from(8500)
                    / U256::from(10).pow(U256::from(12))
                    / U256::from(10000)
                    >= (state.debt + c.amount) * U256::from(2)
            );
            state.debt += c.amount;
            state.usdc += c.amount;
            *self.bridge.token_balance.lock().unwrap() += c.amount;
            logs.push(event(
                to,
                Pool::Borrow {
                    reserve: c.asset,
                    user: owner,
                    onBehalfOf: owner,
                    amount: c.amount,
                    interestRateMode: 2,
                    borrowRate: U256::ZERO,
                    referralCode: 0,
                },
                &hash,
                block,
                0,
            ));
            logs.push(event(
                addr(USDC),
                Erc20::Transfer {
                    from: internal(111),
                    to: owner,
                    value: c.amount,
                },
                &hash,
                block,
                1,
            ));
            logs.push(event(
                internal(121),
                AToken::Mint {
                    caller: owner,
                    onBehalfOf: owner,
                    value: c.amount,
                    balanceIncrease: U256::ZERO,
                    index: ray(),
                },
                &hash,
                block,
                2,
            ));
            logs.push(event(
                internal(121),
                Erc20::Transfer {
                    from: Address::ZERO,
                    to: owner,
                    value: c.amount,
                },
                &hash,
                block,
                3,
            ));
        } else {
            return Err(GoFinError::validation(
                "Unsupported synthetic Aave workflow transaction",
            ));
        }
        let gas = 65_000u64;
        let gas_price = 1_000_000_000u64;
        state.native -= U256::from(gas) * U256::from(gas_price);
        self.states.lock().unwrap().insert(block, state);
        self.nonce.fetch_add(1, Ordering::SeqCst);
        self.writes.fetch_add(1, Ordering::SeqCst);
        let transaction = json!({"hash":hash,"from":self.wallet,"to":lower_address(to),"value":format!("{:#x}",tx.value()),"input":format!("0x{}",hex::encode(tx.input())),"nonce":format!("{:#x}",tx.nonce()),"gas":format!("{:#x}",tx.gas_limit()),"chainId":"0x1","type":"0x2","maxFeePerGas":format!("{:#x}",tx.max_fee_per_gas()),"maxPriorityFeePerGas":format!("{:#x}",tx.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":format!("{block:#x}"),"blockHash":BLOCK_HASH});
        let receipt = json!({"transactionHash":hash,"blockNumber":format!("{block:#x}"),"blockHash":BLOCK_HASH,"status":"0x1","gasUsed":format!("{gas:#x}"),"effectiveGasPrice":format!("{gas_price:#x}"),"logs":logs});
        self.transactions
            .lock()
            .unwrap()
            .insert(hash.clone(), (transaction, receipt));
        Ok(json!(hash))
    }
}
#[async_trait]
impl EvmRpc for WorkflowEvm {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_sendRawTransaction" {
            let raw = hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
            let tx = TxEnvelope::decode_2718(&mut raw.as_slice()).unwrap();
            if tx.to() == Some(addr(WETH)) || tx.to() == Some(addr(POOL)) {
                return self.submit(&tx, &raw);
            }
            assert_eq!(tx.chain_id(), Some(1));
            assert_eq!(tx.recover_signer().unwrap(), addr(&self.wallet));
            assert_eq!(tx.nonce(), self.nonce.load(Ordering::SeqCst));
            self.bridge
                .advance_block_number(self.height.load(Ordering::SeqCst));
            let result = self.bridge.request(method, params).await?;
            self.nonce.fetch_add(1, Ordering::SeqCst);
            let receipt = self
                .bridge
                .request("eth_getTransactionReceipt", json!([result.clone()]))
                .await?;
            let block = gofin_evm::rpc_u64(&receipt["blockNumber"])?;
            let mut state = self.state(&Value::Null);
            state.usdc = *self.bridge.token_balance.lock().unwrap();
            state.native -= gofin_evm::rpc_u256(&receipt["gasUsed"])?
                * gofin_evm::rpc_u256(&receipt["effectiveGasPrice"])?;
            self.states.lock().unwrap().insert(block, state);
            self.height.store(block + 1, Ordering::SeqCst);
            return Ok(result);
        }
        if matches!(
            method,
            "eth_getTransactionReceipt" | "eth_getTransactionByHash"
        ) {
            if let Some((tx, receipt)) = self
                .transactions
                .lock()
                .unwrap()
                .get(params[0].as_str().unwrap())
            {
                return Ok(if method == "eth_getTransactionReceipt" {
                    receipt.clone()
                } else {
                    tx.clone()
                });
            }
            return self.bridge.request(method, params).await;
        }
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!(format!("{:#x}", self.height.load(Ordering::SeqCst))),
            "eth_getBlockByNumber" => {
                let n = params[0]
                    .as_str()
                    .and_then(|v| v.strip_prefix("0x"))
                    .and_then(|v| u64::from_str_radix(v, 16).ok())
                    .unwrap_or(self.height.load(Ordering::SeqCst));
                json!({"hash":BLOCK_HASH,"number":format!("{n:#x}"),"timestamp":format!("{:#x}",chrono::Utc::now().timestamp()),"baseFeePerGas":"0x3b9aca00"})
            }
            "eth_getBalance" => json!(format!("{:#x}", self.state(&params[1]).native)),
            "eth_getTransactionCount" => json!(format!("{:#x}", self.nonce.load(Ordering::SeqCst))),
            "eth_getCode" => json!(if params[0]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&self.wallet))
            {
                "0x"
            } else {
                "0x6000"
            }),
            "eth_getStorageAt" => json!(format!(
                "{:#x}",
                B256::from(U256::from(301).to_be_bytes::<32>())
            )),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_call" => {
                let data = gofin_evm::rpc_bytes(&params[0]["data"])?;
                if data.len() < 4 {
                    return self.bridge.request(method, params).await;
                }
                let selector: [u8; 4] = data[..4].try_into().unwrap();
                let to = addr(params[0]["to"].as_str().unwrap());
                let s = self.state(&params[1]);
                let weth = to == addr(WETH)
                    || to == internal(110)
                    || to == internal(120)
                    || to == internal(115);
                let underlying = if weth { addr(WETH) } else { addr(USDC) };
                let token_from_arg = || Address::from_slice(&data[16..36]);
                let reserve_weth = || token_from_arg() == addr(WETH);
                match selector {
                    Erc20::decimalsCall::SELECTOR => encoded(U256::from(if weth { 18 } else { 6 })),
                    Erc20::symbolCall::SELECTOR => {
                        encoded(if weth { "WETH" } else { "USDC" }.to_string())
                    }
                    Erc20::balanceOfCall::SELECTOR => {
                        let owner = Erc20::balanceOfCall::abi_decode(&data).unwrap().owner;
                        encoded(if to == internal(110) {
                            s.supply
                        } else if to == internal(121) {
                            s.debt
                        } else if [internal(111), internal(120), internal(115), internal(116)]
                            .contains(&to)
                        {
                            U256::ZERO
                        } else if owner != addr(&self.wallet) {
                            U256::from(10).pow(U256::from(24))
                        } else if to == addr(WETH) {
                            s.weth
                        } else {
                            s.usdc
                        })
                    }
                    Erc20::totalSupplyCall::SELECTOR => encoded(U256::from(10).pow(U256::from(24))),
                    Erc20::allowanceCall::SELECTOR => {
                        if to == addr(WETH) {
                            encoded(s.allowance)
                        } else {
                            return self.bridge.request(method, params).await;
                        }
                    }
                    Erc20::approveCall::SELECTOR => encoded(true),
                    Weth::depositCall::SELECTOR
                    | Pool::supplyCall::SELECTOR
                    | Pool::borrowCall::SELECTOR => json!("0x"),
                    AToken::POOLCall::SELECTOR => encoded(addr(POOL)),
                    AToken::UNDERLYING_ASSET_ADDRESSCall::SELECTOR => encoded(underlying),
                    AToken::ATOKEN_REVISIONCall::SELECTOR
                    | AToken::DEBT_TOKEN_REVISIONCall::SELECTOR => encoded(U256::from(5)),
                    AToken::scaledBalanceOfCall::SELECTOR => encoded(if to == internal(110) {
                        s.supply
                    } else if to == internal(121) {
                        s.debt
                    } else {
                        U256::ZERO
                    }),
                    AToken::getPreviousIndexCall::SELECTOR => encoded(ray()),
                    Pool::POOL_REVISIONCall::SELECTOR => encoded(U256::from(10)),
                    Pool::ADDRESSES_PROVIDERCall::SELECTOR => encoded(internal(200)),
                    AddressesProvider::getPoolCall::SELECTOR => encoded(addr(POOL)),
                    AddressesProvider::getPriceOracleCall::SELECTOR => encoded(addr(ORACLE)),
                    AddressesProvider::getPoolDataProviderCall::SELECTOR => encoded(internal(210)),
                    Pool::getReservesListCall::SELECTOR => encoded(vec![addr(WETH), addr(USDC)]),
                    Pool::getConfigurationCall::SELECTOR => encoded(
                        U256::from(8000)
                            | (U256::from(8500) << 16)
                            | (U256::from(1) << 56)
                            | (U256::from(1) << 58),
                    ),
                    Pool::getUserEModeCall::SELECTOR => encoded(U256::ZERO),
                    Pool::getReserveDataCall::SELECTOR => {
                        let w = reserve_weth();
                        encoded(PoolReserveData {
                            configuration: U256::from(8000) | (U256::from(8500) << 16),
                            liquidityIndex: ray().to::<u128>(),
                            currentLiquidityRate: 0,
                            variableBorrowIndex: ray().to::<u128>(),
                            currentVariableBorrowRate: 0,
                            currentStableBorrowRate: 0,
                            lastUpdateTimestamp: U40::from(1),
                            id: if w { 0 } else { 1 },
                            aTokenAddress: internal(if w { 110 } else { 111 }),
                            stableDebtTokenAddress: internal(if w { 115 } else { 116 }),
                            variableDebtTokenAddress: internal(if w { 120 } else { 121 }),
                            interestRateStrategyAddress: internal(130),
                            accruedToTreasury: 0,
                            unbacked: 0,
                            isolationModeTotalDebt: 0,
                        })
                    }
                    DataProvider::getReserveConfigurationDataCall::SELECTOR => encoded((
                        U256::from(if reserve_weth() { 18 } else { 6 }),
                        U256::from(8000),
                        U256::from(8500),
                        U256::from(10500),
                        U256::from(1000),
                        true,
                        true,
                        false,
                        true,
                        false,
                    )),
                    DataProvider::getReserveTokensAddressesCall::SELECTOR => {
                        encoded(if reserve_weth() {
                            (internal(110), internal(115), internal(120))
                        } else {
                            (internal(111), internal(116), internal(121))
                        })
                    }
                    DataProvider::getReserveCapsCall::SELECTOR => encoded((U256::ZERO, U256::ZERO)),
                    DataProvider::getPausedCall::SELECTOR => encoded(false),
                    DataProvider::getUserReserveDataCall::SELECTOR => {
                        let w = reserve_weth();
                        encoded((
                            if w { s.supply } else { U256::ZERO },
                            U256::ZERO,
                            if w { U256::ZERO } else { s.debt },
                            U256::ZERO,
                            if w { U256::ZERO } else { s.debt },
                            U256::ZERO,
                            U256::ZERO,
                            U40::ZERO,
                            w && s.supply > U256::ZERO,
                        ))
                    }
                    Pool::getUserAccountDataCall::SELECTOR => {
                        let collateral = s.supply * U256::from(200_000_000_000u64)
                            / U256::from(10).pow(U256::from(18));
                        let debt = s.debt * U256::from(100);
                        encoded((
                            collateral,
                            debt,
                            collateral * U256::from(8000) / U256::from(10000) - debt,
                            U256::from(8500),
                            U256::from(8000),
                            if debt == U256::ZERO {
                                U256::MAX
                            } else {
                                collateral * U256::from(8500) * U256::from(10).pow(U256::from(18))
                                    / U256::from(10000)
                                    / debt
                            },
                        ))
                    }
                    Pool::getReserveNormalizedIncomeCall::SELECTOR
                    | Pool::getReserveNormalizedVariableDebtCall::SELECTOR => encoded(ray()),
                    Oracle::BASE_CURRENCYCall::SELECTOR => encoded(Address::ZERO),
                    Oracle::BASE_CURRENCY_UNITCall::SELECTOR => encoded(U256::from(100_000_000)),
                    Oracle::getAssetPriceCall::SELECTOR => encoded(U256::from(if reserve_weth() {
                        200_000_000_000u64
                    } else {
                        100_000_000u64
                    })),
                    _ => {
                        return self.bridge.request(method, params).await;
                    }
                }
            }
            _ => return self.bridge.request(method, params).await,
        })
    }
}
