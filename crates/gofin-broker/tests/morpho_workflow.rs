//! Synthetic public CLI → broker → real Morpho adapter → durable receipt proof.
//! The RPC transport is local and cannot send a transaction to a real network.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::Clock, *};
use gofin_ethereum::{
    EthereumAdapter, EthereumConfig, WalletConnection, WalletValuation,
    morpho::api::{MorphoApiRequest, MorphoApiTransport, MorphoClient},
};
use gofin_evm::alloy::{
    primitives::{Address, B256, Bytes, U256, keccak256},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use gofin_evm::{
    EnvelopeSigner, EvmRpc, EvmRuntime, FrozenTransaction, LocalEnvelopeSigner, hex_bytes,
    hex_quantity, quantity, u256,
};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

const OWNER: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const ASSET: &str = "0x0000000000000000000000000000000000000101";
const VAULT: &str = "0x0000000000000000000000000000000000000201";
const BUNDLE: &str = "0x02912516d49de997db75b9d7858faae59209650b";
const FACTORY: &str = "0xa1d94f746defa1928926b84fb2596c06926c0405";
const BUILD: &str = "synthetic-morpho-workflow-build";
const IDENTITY: &str = "synthetic-morpho-workflow";
sol! {
 struct TokenPermit { uint8 kind; bytes data; }
 struct SharePermit { uint256 value; uint256 nonce; uint256 deadline; uint8 v; bytes32 r; bytes32 s; }
 interface Bundles {
  function vaultBundlesV1Deposit(address vault,uint256 assets,uint256 maxSharePriceE27,TokenPermit assetPermit,uint256 referralFeePct,address referralFeeRecipient,uint256 deadline) external payable;
  function vaultBundlesV1Withdraw(address vault,uint256 assets,uint256 shares,SharePermit sharesPermit,uint256 referralFeePct,address referralFeeRecipient,uint256 deadline) external;
 }
 interface Token {
  function approve(address spender,uint256 value) external returns (bool);
  function allowance(address owner,address spender) external view returns(uint256);
  function balanceOf(address owner) external view returns(uint256);
  function decimals() external view returns(uint8);
  function symbol() external view returns(string);
  function name() external view returns(string);
  event Transfer(address indexed from,address indexed to,uint256 value);
  event Approval(address indexed owner,address indexed spender,uint256 value);
 }
 interface Vault {
  function previewDeposit(uint256 assets) external view returns(uint256);
  function previewWithdraw(uint256 assets) external view returns(uint256);
  function previewRedeem(uint256 shares) external view returns(uint256);
  event Deposit(address indexed sender,address indexed onBehalf,uint256 assets,uint256 shares);
  event Withdraw(address indexed sender,address indexed receiver,address indexed onBehalf,uint256 assets,uint256 shares);
 }
}
fn address(s: &str) -> Address {
    s.parse().unwrap()
}
fn abi<T: SolValue>(value: T) -> Value {
    json!(hex_bytes(&value.abi_encode()))
}
fn selector(signature: &str) -> [u8; 4] {
    keccak256(signature).as_slice()[..4].try_into().unwrap()
}
fn block_hash(block: u64) -> String {
    format!("{:#x}", B256::from(U256::from(block).to_be_bytes::<32>()))
}
fn fault(message: impl Into<String>) -> GoFinError {
    GoFinError::new(
        "SYNTHETIC_RPC",
        "transport",
        message,
        "Inspect the isolated fixture.",
    )
}
struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
#[derive(Default)]
struct AdjustableClock(AtomicI64);
impl Clock for AdjustableClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::seconds(self.0.load(Ordering::SeqCst))
    }
}
#[derive(Default)]
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic Morpho named plan".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: TestClock.now().to_rfc3339(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl WalletValuation for Prices {
    async fn quote_usd(&self, a: &AssetId, n: Quantity) -> Result<(Quantity, String)> {
        let p = if a.reference == "native" {
            2_000_000_000u64
        } else {
            1_000_000
        };
        Ok((
            n.mul_div_ceil(
                &Quantity::from_u64(p),
                &quantity(U256::from(10).pow(U256::from(a.decimals))),
            )?,
            "synthetic verified USD fixture".into(),
        ))
    }
}
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Err(fault("Unexpected external quote"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    cash: U256,
    shares: U256,
    total_assets: U256,
    total_shares: U256,
    asset_allowance: U256,
    share_allowance: U256,
    native: U256,
}
impl State {
    fn new() -> Self {
        Self {
            cash: U256::from(10_000_000),
            shares: U256::ZERO,
            total_assets: U256::from(100_000_000),
            total_shares: U256::from(100) * U256::from(10).pow(U256::from(18)),
            asset_allowance: U256::ZERO,
            share_allowance: U256::ZERO,
            native: U256::from(10).pow(U256::from(18)),
        }
    }
    fn preview_deposit(&self, a: U256) -> U256 {
        a * (self.total_shares + U256::from(1_000_000_000_000u64))
            / (self.total_assets + U256::from(1))
    }
    fn preview_withdraw(&self, a: U256) -> U256 {
        let d = self.total_assets + U256::from(1);
        (a * (self.total_shares + U256::from(1_000_000_000_000u64)) + d - U256::from(1)) / d
    }
    fn preview_redeem(&self, s: U256) -> U256 {
        s * (self.total_assets + U256::from(1))
            / (self.total_shares + U256::from(1_000_000_000_000u64))
    }
}
struct Chain {
    block: u64,
    nonce: u64,
    states: BTreeMap<u64, State>,
    signed: BTreeMap<String, FrozenTransaction>,
    receipts: BTreeMap<String, Value>,
}
struct Rpc {
    chain: Mutex<Chain>,
    dispatches: AtomicUsize,
    hide_receipts: AtomicBool,
    lose_reply: AtomicBool,
    fail_simulation: AtomicBool,
    revert_next_action: AtomicBool,
    conflicting_receipts: AtomicBool,
    credit_dust_after_next_action: AtomicBool,
    transfer_shares_after_next_action: AtomicI64,
    unexpected_receipt_debit: AtomicBool,
    unexpected_receipt_share_transfer: AtomicBool,
}
impl Rpc {
    fn new() -> Self {
        Self {
            chain: Mutex::new(Chain {
                block: 100,
                nonce: 0,
                states: BTreeMap::from([(100, State::new())]),
                signed: BTreeMap::new(),
                receipts: BTreeMap::new(),
            }),
            dispatches: AtomicUsize::new(0),
            hide_receipts: AtomicBool::new(false),
            lose_reply: AtomicBool::new(false),
            fail_simulation: AtomicBool::new(false),
            revert_next_action: AtomicBool::new(false),
            conflicting_receipts: AtomicBool::new(false),
            credit_dust_after_next_action: AtomicBool::new(false),
            transfer_shares_after_next_action: AtomicI64::new(0),
            unexpected_receipt_debit: AtomicBool::new(false),
            unexpected_receipt_share_transfer: AtomicBool::new(false),
        }
    }
    fn loss(&self) {
        let mut c = self.chain.lock().unwrap();
        let mut s = c.states[&c.block].clone();
        s.total_assets = s.total_assets * U256::from(9) / U256::from(10);
        c.block += 1;
        let b = c.block;
        c.states.insert(b, s);
    }
    fn state(&self) -> State {
        let c = self.chain.lock().unwrap();
        c.states[&c.block].clone()
    }
    fn change_state(&self, change: impl FnOnce(&mut State)) {
        let mut c = self.chain.lock().unwrap();
        let mut state = c.states[&c.block].clone();
        change(&mut state);
        c.block += 1;
        let block = c.block;
        c.states.insert(block, state);
    }
    fn event<E: SolEvent>(emitter: &str, event: E, hash: &str, b: u64, index: usize) -> Value {
        let e = event.encode_log_data();
        json!({"address":emitter,"topics":e.topics().iter().map(|v|v.to_string()).collect::<Vec<_>>(),"data":hex_bytes(&e.data),"removed":false,"transactionHash":hash,"transactionIndex":"0x0","logIndex":format!("{index:#x}"),"blockHash":block_hash(b),"blockNumber":format!("{b:#x}")})
    }
    fn dispatch(&self, raw: &Value) -> Result<Value> {
        let hash = keccak256(gofin_evm::rpc_bytes(raw)?).to_string();
        let mut c = self.chain.lock().unwrap();
        let tx = c
            .signed
            .get(&hash)
            .cloned()
            .ok_or_else(|| fault("Unregistered signed envelope"))?;
        assert_eq!(tx.nonce, c.nonce, "No replayed nonce");
        assert_eq!(tx.sender, OWNER);
        let mut s = c.states[&c.block].clone();
        let data = gofin_evm::rpc_bytes(&json!(tx.calldata))?;
        let b = c.block + 1;
        let mut logs = vec![];
        let reverted = tx.to == BUNDLE && self.revert_next_action.swap(false, Ordering::SeqCst);
        if reverted {
            assert!(
                data.starts_with(&Bundles::vaultBundlesV1DepositCall::SELECTOR)
                    || data.starts_with(&Bundles::vaultBundlesV1WithdrawCall::SELECTOR)
            );
        } else if data.starts_with(&Token::approveCall::SELECTOR) {
            let call = Token::approveCall::abi_decode_validate(&data).unwrap();
            assert_eq!(call.spender, address(BUNDLE));
            if tx.to == ASSET {
                s.asset_allowance = call.value
            } else {
                assert_eq!(tx.to, VAULT);
                s.share_allowance = call.value
            };
            logs.push(Self::event(
                &tx.to,
                Token::Approval {
                    owner: address(OWNER),
                    spender: call.spender,
                    value: call.value,
                },
                &hash,
                b,
                0,
            ));
        } else if data.starts_with(&Bundles::vaultBundlesV1DepositCall::SELECTOR) {
            let call = Bundles::vaultBundlesV1DepositCall::abi_decode_validate(&data).unwrap();
            assert_eq!(tx.to, BUNDLE);
            assert_eq!(call.vault, address(VAULT));
            let a = call.assets;
            let sh = s.preview_deposit(a);
            s.cash -= a;
            s.shares += sh;
            s.total_assets += a;
            s.total_shares += sh;
            s.asset_allowance -= a;
            logs.push(Self::event(
                VAULT,
                Vault::Deposit {
                    sender: address(BUNDLE),
                    onBehalf: address(OWNER),
                    assets: a,
                    shares: sh,
                },
                &hash,
                b,
                0,
            ));
            logs.push(Self::event(
                ASSET,
                Token::Transfer {
                    from: address(OWNER),
                    to: address(BUNDLE),
                    value: a,
                },
                &hash,
                b,
                1,
            ));
            logs.push(Self::event(
                ASSET,
                Token::Transfer {
                    from: address(BUNDLE),
                    to: address(VAULT),
                    value: a,
                },
                &hash,
                b,
                2,
            ));
            logs.push(Self::event(
                VAULT,
                Token::Transfer {
                    from: Address::ZERO,
                    to: address(OWNER),
                    value: sh,
                },
                &hash,
                b,
                3,
            ));
        } else if data.starts_with(&Bundles::vaultBundlesV1WithdrawCall::SELECTOR) {
            let call = Bundles::vaultBundlesV1WithdrawCall::abi_decode_validate(&data).unwrap();
            assert_eq!(tx.to, BUNDLE);
            assert_eq!(call.vault, address(VAULT));
            let (a, sh) = if call.assets > U256::ZERO {
                (call.assets, s.preview_withdraw(call.assets))
            } else {
                (s.preview_redeem(call.shares), call.shares)
            };
            s.cash += a;
            s.shares -= sh;
            s.total_assets -= a;
            s.total_shares -= sh;
            s.share_allowance -= sh;
            logs.push(Self::event(
                VAULT,
                Vault::Withdraw {
                    sender: address(BUNDLE),
                    receiver: address(BUNDLE),
                    onBehalf: address(OWNER),
                    assets: a,
                    shares: sh,
                },
                &hash,
                b,
                0,
            ));
            logs.push(Self::event(
                ASSET,
                Token::Transfer {
                    from: address(VAULT),
                    to: address(BUNDLE),
                    value: a,
                },
                &hash,
                b,
                1,
            ));
            logs.push(Self::event(
                ASSET,
                Token::Transfer {
                    from: address(BUNDLE),
                    to: address(OWNER),
                    value: a,
                },
                &hash,
                b,
                2,
            ));
            logs.push(Self::event(
                VAULT,
                Token::Transfer {
                    from: address(OWNER),
                    to: Address::ZERO,
                    value: sh,
                },
                &hash,
                b,
                3,
            ));
        } else {
            return Err(fault("Unexpected write selector"));
        }
        if tx.to == BUNDLE
            && self
                .credit_dust_after_next_action
                .swap(false, Ordering::SeqCst)
        {
            // A separate transaction sends one underlying atom after our action
            // in the same block. Its transfer is absent from our receipt.
            s.cash += U256::from(1);
        }
        if tx.to == BUNDLE {
            // Separate share transfers change ownership, not vault supply or
            // backing. They are deliberately absent from our action receipt.
            let share_units = self
                .transfer_shares_after_next_action
                .swap(0, Ordering::SeqCst);
            let shares =
                U256::from(share_units.unsigned_abs()) * U256::from(10).pow(U256::from(18));
            if share_units < 0 {
                s.shares -= shares;
            } else {
                s.shares += shares;
            }
        }
        s.native -= U256::from(21_000_000u64);
        c.states.insert(b, s);
        c.block = b;
        c.nonce += 1;
        c.receipts.insert(hash.clone(),json!({"transactionHash":hash,"blockHash":block_hash(b),"blockNumber":format!("{b:#x}"),"from":OWNER,"to":tx.to,"status":if reverted {"0x0"} else {"0x1"},"gasUsed":"0x5208","effectiveGasPrice":"0x3e8","transactionIndex":"0x0","logs":logs}));
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        if self.lose_reply.swap(false, Ordering::SeqCst) {
            return Err(fault(
                "Transport lost reply after accepting the immutable transaction",
            ));
        }
        Ok(json!(hash))
    }
}
struct Signer {
    inner: LocalEnvelopeSigner,
    rpc: Arc<Rpc>,
}
#[async_trait]
impl EnvelopeSigner for Signer {
    async fn sign_prepared(&self, t: &FrozenTransaction) -> Result<Bytes> {
        let bytes = self.inner.sign_prepared(t).await?;
        self.rpc
            .chain
            .lock()
            .unwrap()
            .signed
            .insert(keccak256(&bytes).to_string(), t.clone());
        Ok(bytes)
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_sendRawTransaction" {
            return self.dispatch(&params[0]);
        }
        let c = self.chain.lock().unwrap();
        let latest = c.block;
        let selected = params[1]
            .as_str()
            .filter(|v| v.starts_with("0x"))
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(latest);
        let s = c.states.range(..=selected).next_back().unwrap().1;
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!(format!("{latest:#x}")),
            "eth_getBlockByNumber" => {
                let n = params[0]
                    .as_str()
                    .filter(|v| v.starts_with("0x"))
                    .map(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap())
                    .unwrap_or(latest);
                json!({"number":format!("{n:#x}"),"hash":block_hash(n),"timestamp":format!("{:#x}",chrono::Utc::now().timestamp()),"baseFeePerGas":"0x1"})
            }
            "eth_getCode" => match params[0].as_str().unwrap() {
                FACTORY => json!(include_str!("fixtures/morpho/factory.hex").trim()),
                BUNDLE => json!(include_str!("fixtures/morpho/bundles.hex").trim()),
                OWNER => json!("0x"),
                _ => json!("0x6000"),
            },
            "eth_getBalance" => json!(hex_quantity(s.native)),
            "eth_getTransactionCount" => json!(format!("{:#x}", c.nonce)),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_maxPriorityFeePerGas" => json!("0x3e8"),
            "eth_getTransactionReceipt" => {
                if self.hide_receipts.load(Ordering::SeqCst) {
                    Value::Null
                } else {
                    let mut receipt = c
                        .receipts
                        .get(params[0].as_str().unwrap())
                        .cloned()
                        .unwrap_or(Value::Null);
                    if self.conflicting_receipts.load(Ordering::SeqCst)
                        && receipt["to"] == BUNDLE
                        && receipt["logs"]
                            .as_array()
                            .is_some_and(|logs| logs.len() > 1)
                    {
                        // The receipt and transaction envelope are canonical, but its
                        // transfer amount contradicts the vault event and wallet state.
                        receipt["logs"][1]["data"] = abi(U256::from(1));
                    }
                    if (self.unexpected_receipt_debit.load(Ordering::SeqCst)
                        || self
                            .unexpected_receipt_share_transfer
                            .load(Ordering::SeqCst))
                        && receipt["to"] == BUNDLE
                    {
                        let hash = receipt["transactionHash"].as_str().unwrap().to_owned();
                        let block = gofin_evm::rpc_u64(&receipt["blockNumber"])?;
                        let logs = receipt["logs"].as_array_mut().unwrap();
                        logs.push(Self::event(
                            if self
                                .unexpected_receipt_share_transfer
                                .load(Ordering::SeqCst)
                            {
                                VAULT
                            } else {
                                ASSET
                            },
                            Token::Transfer {
                                from: address(OWNER),
                                to: Address::repeat_byte(7),
                                value: U256::from(1),
                            },
                            &hash,
                            block,
                            logs.len(),
                        ));
                    }
                    receipt
                }
            }
            "eth_getTransactionByHash" => {
                let h = params[0].as_str().unwrap();
                if let (Some(t), Some(r)) = (c.signed.get(h), c.receipts.get(h)) {
                    json!({"hash":h,"from":t.sender,"to":t.to,"nonce":format!("{:#x}",t.nonce),"chainId":"0x1","type":"0x2","gas":format!("{:#x}",t.gas),"value":hex_quantity(u256(&t.value)?),"input":t.calldata,"maxFeePerGas":hex_quantity(u256(&t.max_fee_per_gas)?),"maxPriorityFeePerGas":hex_quantity(u256(&t.max_priority_fee_per_gas)?),"accessList":[],"blockHash":r["blockHash"],"blockNumber":r["blockNumber"]})
                } else {
                    Value::Null
                }
            }
            "eth_call" => {
                let data = gofin_evm::rpc_bytes(&params[0]["data"])?;
                let sig: [u8; 4] = data[..4].try_into().unwrap();
                let to = params[0]["to"].as_str().unwrap();
                if sig == Token::approveCall::SELECTOR {
                    abi(true)
                } else if sig == Bundles::vaultBundlesV1DepositCall::SELECTOR
                    || sig == Bundles::vaultBundlesV1WithdrawCall::SELECTOR
                {
                    if self.fail_simulation.load(Ordering::SeqCst) {
                        return Err(GoFinError::new(
                            "SIMULATION_FAILED",
                            "preview",
                            "Exact bundle simulation deliberately reverted.",
                            "Refresh the candidate after resolving the simulated revert.",
                        ));
                    }
                    json!("0x")
                } else if sig == Token::decimalsCall::SELECTOR {
                    abi(U256::from(if to == VAULT { 18u8 } else { 6u8 }))
                } else if sig == Token::symbolCall::SELECTOR {
                    abi(if to == VAULT { "vUSDC" } else { "USDC" }.to_owned())
                } else if sig == Token::nameCall::SELECTOR {
                    abi("Synthetic token".to_owned())
                } else if sig == Token::balanceOfCall::SELECTOR {
                    let q = Token::balanceOfCall::abi_decode_validate(&data).unwrap();
                    abi(if to == VAULT {
                        if q.owner == address(OWNER) {
                            s.shares
                        } else {
                            U256::ZERO
                        }
                    } else if q.owner == address(VAULT) {
                        s.total_assets
                    } else if q.owner == address(OWNER) {
                        s.cash
                    } else {
                        U256::ZERO
                    })
                } else if sig == Token::allowanceCall::SELECTOR {
                    abi(if to == VAULT {
                        s.share_allowance
                    } else {
                        s.asset_allowance
                    })
                } else if sig == Vault::previewDepositCall::SELECTOR {
                    abi(s.preview_deposit(
                        Vault::previewDepositCall::abi_decode_validate(&data)
                            .unwrap()
                            .assets,
                    ))
                } else if sig == Vault::previewWithdrawCall::SELECTOR {
                    abi(s.preview_withdraw(
                        Vault::previewWithdrawCall::abi_decode_validate(&data)
                            .unwrap()
                            .assets,
                    ))
                } else if sig == Vault::previewRedeemCall::SELECTOR {
                    abi(s.preview_redeem(
                        Vault::previewRedeemCall::abi_decode_validate(&data)
                            .unwrap()
                            .shares,
                    ))
                } else if sig == selector("asset()") {
                    abi(address(ASSET))
                } else if sig == selector("totalAssets()") {
                    abi(s.total_assets)
                } else if sig == selector("totalSupply()") {
                    abi(s.total_shares)
                } else if sig == selector("virtualShares()") {
                    abi(U256::from(1_000_000_000_000u64))
                } else if sig == selector("liquidityData()") {
                    abi(Bytes::new())
                } else if [
                    "isVaultV2(address)",
                    "canReceiveShares(address)",
                    "canSendShares(address)",
                    "canReceiveAssets(address)",
                    "canSendAssets(address)",
                ]
                .iter()
                .any(|v| sig == selector(v))
                {
                    abi(true)
                } else if [
                    "sendAssetsGate()",
                    "receiveAssetsGate()",
                    "sendSharesGate()",
                    "receiveSharesGate()",
                    "adapterRegistry()",
                    "liquidityAdapter()",
                ]
                .iter()
                .any(|v| sig == selector(v))
                {
                    abi(Address::ZERO)
                } else if ["adaptersLength()", "performanceFee()", "managementFee()"]
                    .iter()
                    .any(|v| sig == selector(v))
                {
                    abi(U256::ZERO)
                } else {
                    return Err(fault(format!("Unhandled selector {}", hex::encode(sig))));
                }
            }
            _ => return Err(fault(format!("Unhandled RPC {method}"))),
        })
    }
}
struct Api;
#[async_trait]
impl MorphoApiTransport for Api {
    async fn request(&self, r: MorphoApiRequest) -> Result<Value> {
        match r {
            MorphoApiRequest::Rest { path, .. } if path == "/v0/vaults-v2" => Ok(
                json!({"data":[{"chain_id":1,"vault_address":VAULT,"version":"2.0","name":"Synthetic V2","symbol":"vUSDC","asset_address":ASSET,"factory_address":FACTORY}],"cursor":null}),
            ),
            MorphoApiRequest::Graphql { .. } => Ok(
                json!({"data":{"vaultV2ByAddress":{"address":VAULT,"apy":0.03,"netApy":0.03,"rewards":[]}}}),
            ),
            _ => Err(fault("Deliberately unavailable optional indexer metadata")),
        }
    }
}
fn adapter(rpc: Arc<Rpc>) -> Arc<EthereumAdapter> {
    let signer = Arc::new(Signer {
        inner: LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap(),
        rpc: rpc.clone(),
    });
    Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(1, 1, vec![ASSET.into()]),
            vec![WalletConnection {
                name: "synthetic".into(),
                address: OWNER.into(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc, 1, Some(signer))),
        )
        .unwrap()
        .with_wallet_valuation(Arc::new(Prices))
        .with_morpho_api(MorphoClient::with_transport(Arc::new(Api))),
    )
}
fn runtime(
    path: &Path,
    rpc: Arc<Rpc>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    runtime_with_clock(path, rpc, approval, Arc::new(TestClock))
}
fn runtime_with_clock(
    path: &Path,
    rpc: Arc<Rpc>,
    approval: Arc<Approval>,
    clock: Arc<dyn Clock>,
) -> Arc<gofin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy {
                require_run_for_writes: false,
                max_movement_usd_micros: "20000000".into(),
                max_run_allocation_usd_micros: "200000000".into(),
                preview_ttl_seconds: 90,
                allow_unpriced_exchange_assets: false,
            },
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![adapter(rpc)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        clock,
    )
    .unwrap()
}
struct Client {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}
impl Client {
    async fn new(runtime: Arc<gofin_broker::BrokerRuntime>, directory: &Path) -> Self {
        let path = directory.join("morpho.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], IDENTITY);
                    let reply = json!({"kind":"hello","identity":IDENTITY,"build_id":BUILD,"mode":"synthetic"});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let envelope: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(envelope["request"].clone()).unwrap();
                    let response = gofin_protocol::progress::observe(
                        reader.get_mut(),
                        envelope["progress"].as_bool().unwrap_or(false),
                        runtime.handle(request),
                    )
                    .await;
                    let reply = json!({"identity":IDENTITY,"build_id":BUILD,"mode":"synthetic","response":response});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        Self { path, task }
    }
    async fn call(&self, args: Vec<String>) -> Value {
        let binary = PathBuf::from(
            std::env::var_os("GOFIN_MORPHO_CLI").expect("Run make test-morpho-rehearsal"),
        )
        .canonicalize()
        .unwrap();
        let path = self.path.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(binary)
                .args(["--identity", IDENTITY, "--build-id", BUILD, "--json"])
                .args(args)
                .env("GOFIN_DEV_IDENTITY", IDENTITY)
                .env("GOFIN_DEV_BUILD_ID", BUILD)
                .env("GOFIN_DEV_SOCKET", path)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stdout)))
    }
    async fn read(&self, name: &str, input: Value) -> Value {
        self.call(vec![
            "read".into(),
            name.into(),
            "--input".into(),
            input.to_string(),
        ])
        .await
    }
    async fn authorize(&self, plan: &Value, id: &str) -> String {
        let review = self
            .call(vec![
                "plans".into(),
                "preview".into(),
                "--input".into(),
                plan.to_string(),
                "--valid-for".into(),
                "10m".into(),
            ])
            .await;
        let hash = review["review_hash"]
            .as_str()
            .unwrap_or_else(|| panic!("{review}"));
        let authorized = self
            .call(vec![
                "plans".into(),
                "authorize".into(),
                "--input".into(),
                plan.to_string(),
                "--id".into(),
                id.into(),
                "--review-hash".into(),
                hash.into(),
                "--valid-for".into(),
                "10m".into(),
            ])
            .await;
        authorized["plan_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{authorized}"))
            .to_owned()
    }
    async fn plan_status(&self, plan: &str) -> Value {
        self.call(vec!["plans".into(), "status".into(), plan.into()])
            .await
    }
    async fn approve_deposit(&self, plan: &str) {
        let approved = self
            .execute(
                "wallet.approve",
                approval_input(ASSET, "2"),
                "approve-assets",
                plan,
                "approve-assets",
                false,
            )
            .await;
        let settled = self
            .call(vec!["status".into(), operation_id(&approved)])
            .await;
        assert_eq!(settled["status"], "completed", "{settled}");
    }
    async fn execute(
        &self,
        name: &str,
        input: Value,
        id: &str,
        plan: &str,
        step: &str,
        dry: bool,
    ) -> Value {
        let mut args = vec![
            "execute".into(),
            name.into(),
            "--input".into(),
            input.to_string(),
            "--id".into(),
            id.into(),
            "--plan".into(),
            plan.into(),
            "--step".into(),
            step.into(),
        ];
        if dry {
            args.push("--dry-run".into())
        };
        self.call(args).await
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
fn approval_input(token: &str, amount: &str) -> Value {
    json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"token":token,"spender":BUNDLE,"amount":amount,"max_fee_eth":"0.001","request_id":"named-scope"})
}
fn vault_input(amount: &str) -> Value {
    json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"vault":VAULT,"amount":amount,"max_fee_eth":"0.001","request_id":"named-scope"})
}
fn plan() -> Value {
    let mut deposit = vault_input("2");
    deposit["min_shares"] = json!("1.9999");
    let mut withdraw = vault_input("0.5");
    withdraw["max_shares"] = json!("2");
    let mut redeem = vault_input("all");
    redeem["max_shares"] = json!("2");
    let mut redeposit = vault_input("0.5");
    redeposit["min_shares"] = json!("0.4999");
    redeem["accept_prevailing_value"] = json!(true);
    let mut steps = vec![];
    for (id, operation, input, maximum) in [
        (
            "approve-assets",
            "wallet.approve",
            approval_input(ASSET, "2"),
            "2",
        ),
        ("deposit", "morpho.deposit", deposit, "2"),
        (
            "approve-shares",
            "wallet.approve",
            approval_input(VAULT, "2"),
            "2",
        ),
        ("partial-withdraw", "morpho.withdraw", withdraw, "0.5"),
        (
            "approve-redeposit",
            "wallet.approve",
            approval_input(ASSET, "0.5"),
            "0.5",
        ),
        ("redeposit-proceeds", "morpho.deposit", redeposit, "0.5"),
        (
            "refresh-share-allowance",
            "wallet.approve",
            approval_input(VAULT, "2"),
            "2",
        ),
        ("redeem-all", "morpho.redeem", redeem, "2"),
        (
            "revoke-shares",
            "wallet.approve",
            approval_input(VAULT, "0"),
            "0",
        ),
    ] {
        let mut variation = json!({"min_amount":"0","max_amount":maximum,"max_fee_eth":"0.001"});
        if operation == "wallet.approve" {
            variation["vault"] = json!(VAULT);
        }
        if operation == "morpho.redeem" {
            variation["max_receipt_assets"] = json!("2");
        }
        let dependencies = if id == "redeposit-proceeds" {
            json!([{"producing_step_id":"partial-withdraw","resource":format!("ethereum:1:{OWNER}"),"asset":{"network":"ethereum:1","reference":ASSET,"decimals":6}}])
        } else {
            json!([])
        };
        steps.push(json!({"step_id":id,"operation":operation,"input":input,"permitted_variations":variation,"completion":{"operation_succeeded":true},"funding_dependencies":dependencies}));
    }
    let resource = format!("ethereum:1:{OWNER}");
    json!({"version":1,"name":"Morpho V2 deposit and standard exits","steps":steps,"aggregate_bounds":{"maximum_debits":[{"resource":resource,"amount":{"asset":{"network":"ethereum:1","reference":ASSET,"decimals":6},"atoms":"2500000"}}],"maximum_costs":[{"location":{"kind":"cash","resource":resource},"amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"9000000000000000"},"treatment":"additional_debit"}]}})
}
fn deposit_plan() -> Value {
    let mut plan = plan();
    plan["steps"].as_array_mut().unwrap().truncate(2);
    plan["aggregate_bounds"]["maximum_debits"][0]["amount"]["atoms"] = json!("2000000");
    plan["aggregate_bounds"]["maximum_costs"][0]["amount"]["atoms"] = json!("2000000000000000");
    plan
}
fn operation_id(result: &Value) -> String {
    result["operation_id"]
        .as_str()
        .or_else(|| result["id"].as_str())
        .unwrap_or_else(|| panic!("{result}"))
        .to_owned()
}
fn journal_record(path: &Path, id: &str) -> OperationRecord {
    serde_json::from_value(Store::open(path).unwrap().get_record(id).unwrap().unwrap()).unwrap()
}
fn assert_reservation_count(status: &Value, count: usize) {
    assert_eq!(
        status["usage"]["reservations"].as_array().unwrap().len(),
        count,
        "{status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn settled_approval_survives_balance_gas_and_simulation_preflight_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let plan = deposit_plan();
    let plan_id = client.authorize(&plan, "preflight-failures").await;
    client.approve_deposit(&plan_id).await;
    let funded = rpc.state();
    assert_eq!(funded.asset_allowance, U256::from(2_000_000));
    let input = plan["steps"][1]["input"].clone();

    for (id, expected_code) in [
        ("insufficient-assets", "BALANCE_INSUFFICIENT"),
        ("insufficient-gas", "INSUFFICIENT_GAS_BALANCE"),
        ("denied-simulation", "SIMULATION_FAILED"),
    ] {
        rpc.change_state(|state| *state = funded.clone());
        match id {
            "insufficient-assets" => rpc.change_state(|state| state.cash = U256::ZERO),
            "insufficient-gas" => rpc.change_state(|state| state.native = U256::ZERO),
            _ => rpc.fail_simulation.store(true, Ordering::SeqCst),
        }
        let before = rpc.state();
        let rejected = client
            .execute(
                "morpho.deposit",
                input.clone(),
                id,
                &plan_id,
                "deposit",
                false,
            )
            .await;
        assert_eq!(rejected["error"]["code"], expected_code, "{rejected}");
        assert_eq!(
            rpc.state(),
            before,
            "Rejected {id} must not change chain state"
        );
        assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(rpc.chain.lock().unwrap().signed.len(), 1);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_reservation_count(&client.plan_status(&plan_id).await, 0);
        rpc.fail_simulation.store(false, Ordering::SeqCst);
    }
    rpc.change_state(|state| *state = funded.clone());
    let completed = client
        .execute(
            "morpho.deposit",
            input,
            "retry-ready",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    let settled = client
        .call(vec!["status".into(), operation_id(&completed)])
        .await;
    assert_eq!(settled["status"], "completed", "{settled}");
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.state().cash, U256::from(8_000_000));
    assert_eq!(rpc.state().asset_allowance, U256::ZERO);
    assert!(rpc.state().shares > U256::ZERO);
    assert_reservation_count(&client.plan_status(&plan_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn independent_underlying_credit_does_not_block_settlement_or_become_plan_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let plan = deposit_plan();
    let plan_id = client
        .authorize(&plan, "independent-underlying-credit")
        .await;
    client.approve_deposit(&plan_id).await;
    rpc.hide_receipts.store(true, Ordering::SeqCst);
    rpc.credit_dust_after_next_action
        .store(true, Ordering::SeqCst);
    let input = plan["steps"][1]["input"].clone();
    let result = client
        .execute(
            "morpho.deposit",
            input.clone(),
            "deposit-with-dust",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    let id = operation_id(&result);
    let original = journal_record(&path, &id);
    assert!(!original.status.terminal());
    assert!(original.effects.is_none());
    assert_reservation_count(&client.plan_status(&plan_id).await, 1);
    assert_eq!(rpc.state().cash, U256::from(8_000_001));

    // Removing the global cash equality must not hide an unexpected debit
    // inside our own receipt, even when the approved route is also present.
    rpc.hide_receipts.store(false, Ordering::SeqCst);
    rpc.unexpected_receipt_debit.store(true, Ordering::SeqCst);
    let rejected = client.call(vec!["status".into(), id.clone()]).await;
    assert_eq!(
        rejected["error"]["code"], "MORPHO_RECEIPT_MISMATCH",
        "{rejected}"
    );
    assert!(journal_record(&path, &id).effects.is_none());
    assert_reservation_count(&client.plan_status(&plan_id).await, 1);
    rpc.unexpected_receipt_debit.store(false, Ordering::SeqCst);

    let settled = client.call(vec!["status".into(), id.clone()]).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    let record = journal_record(&path, &id);
    assert_eq!(record.submission, original.submission);
    let proof = record.effects.as_ref().unwrap();
    assert_eq!(
        proof.evidence["underlying_balance_observation"]["projected_from_this_transaction"],
        "8000000"
    );
    assert_eq!(
        proof.evidence["underlying_balance_observation"]["observed_at_inclusion_block"],
        "8000001"
    );
    assert_eq!(
        proof.evidence["underlying_balance_observation"]["difference_not_attributed_to_this_transaction"],
        "1"
    );
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::CashDebit { amount, .. } if amount.atoms == Quantity::from_u64(2_000_000)
    )));
    assert!(
        !proof
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
    );
    let plan_status = client.plan_status(&plan_id).await;
    assert_reservation_count(&plan_status, 0);
    assert!(
        plan_status["usage"]["verified_proceeds"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Store::open(&path).unwrap().reservation_count(&id).unwrap(),
        0
    );

    drop(client);
    let restarted = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let repeated = restarted
        .execute(
            "morpho.deposit",
            input,
            "deposit-with-dust",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    assert_eq!(operation_id(&repeated), id);
    assert_eq!(journal_record(&path, &id).effects, record.effects);
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(rpc.chain.lock().unwrap().signed.len(), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.state().cash, U256::from(8_000_001));
    assert_reservation_count(&restarted.plan_status(&plan_id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn independent_share_transfers_settle_attributed_backing_without_importing_earnings() {
    for external_units in [-1, 1] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let rpc = Arc::new(Rpc::new());
        let approval = Arc::new(Approval::default());
        let client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
        let plan = plan();
        let plan_id = client.authorize(&plan, "independent-share-transfer").await;
        client.approve_deposit(&plan_id).await;
        let expected_shares = rpc.state().preview_deposit(U256::from(2_000_000));
        rpc.hide_receipts.store(true, Ordering::SeqCst);
        rpc.transfer_shares_after_next_action
            .store(external_units, Ordering::SeqCst);
        let input = plan["steps"][1]["input"].clone();
        let result = client
            .execute(
                "morpho.deposit",
                input.clone(),
                "deposit-with-share-transfer",
                &plan_id,
                "deposit",
                false,
            )
            .await;
        let id = operation_id(&result);
        let original = journal_record(&path, &id);
        assert!(!original.status.terminal());
        assert_reservation_count(&client.plan_status(&plan_id).await, 1);

        rpc.hide_receipts.store(false, Ordering::SeqCst);
        rpc.unexpected_receipt_share_transfer
            .store(true, Ordering::SeqCst);
        let rejected = client.call(vec!["status".into(), id.clone()]).await;
        assert_eq!(
            rejected["error"]["code"], "MORPHO_RECEIPT_MISMATCH",
            "{rejected}"
        );
        assert!(journal_record(&path, &id).effects.is_none());
        assert_reservation_count(&client.plan_status(&plan_id).await, 1);
        rpc.unexpected_receipt_share_transfer
            .store(false, Ordering::SeqCst);

        let settled = client.call(vec!["status".into(), id.clone()]).await;
        assert_eq!(settled["status"], "completed", "{settled}");
        let record = journal_record(&path, &id);
        assert_eq!(record.submission, original.submission);
        let proof = record.effects.as_ref().unwrap();
        let current = rpc.state();
        let anchor = proof
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::PositionChange { anchor, .. } => anchor.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(anchor.evidence_version, 2);
        assert_eq!(anchor.evidence["share_scope"], "transaction_attributed");
        assert_eq!(
            anchor.evidence["transaction_hash"],
            record.submission.as_ref().unwrap().reference
        );
        assert_eq!(
            anchor.evidence["share_balance"],
            json!(quantity(expected_shares))
        );
        assert_eq!(
            anchor.evidence["observed_wallet_share_balance"],
            json!(quantity(current.shares))
        );
        assert_eq!(
            anchor.evidence["observed_wallet_underlying_value"],
            json!(quantity(current.preview_redeem(current.shares)))
        );
        assert_eq!(
            anchor.principal.atoms,
            quantity(current.preview_redeem(expected_shares))
        );
        assert_ne!(
            anchor.principal.atoms,
            quantity(current.preview_redeem(current.shares))
        );
        // Only possible deposit conversion rounding remains, not the one-USDC
        // value of the externally transferred share in either direction.
        assert!(proof.effects.iter().any(|effect| matches!(effect,
            FinancialEffect::Accrual { delta, .. } if matches!(delta.as_str(), "0" | "-1")
        )));
        assert!(
            !proof
                .effects
                .iter()
                .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
        );
        let plan_status = client.plan_status(&plan_id).await;
        assert_reservation_count(&plan_status, 0);
        assert!(
            plan_status["usage"]["verified_proceeds"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            Store::open(&path).unwrap().reservation_count(&id).unwrap(),
            0
        );

        drop(client);
        let restarted =
            Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
        let repeated = restarted
            .execute(
                "morpho.deposit",
                input,
                "deposit-with-share-transfer",
                &plan_id,
                "deposit",
                false,
            )
            .await;
        assert_eq!(operation_id(&repeated), id);
        assert_eq!(journal_record(&path, &id).effects, record.effects);
        assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
        assert_eq!(rpc.chain.lock().unwrap().signed.len(), 2);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_reservation_count(&restarted.plan_status(&plan_id).await, 0);

        // A settled transaction does not silently authorize adopting external
        // share changes into the next financial action or its cost basis.
        let approve = restarted
            .execute(
                "wallet.approve",
                plan["steps"][2]["input"].clone(),
                "approve-shares",
                &plan_id,
                "approve-shares",
                false,
            )
            .await;
        let approval_status = restarted
            .call(vec!["status".into(), operation_id(&approve)])
            .await;
        assert_eq!(approval_status["status"], "completed", "{approval_status}");
        let blocked = restarted
            .execute(
                "morpho.withdraw",
                plan["steps"][3]["input"].clone(),
                "external-shares-not-adopted",
                &plan_id,
                "partial-withdraw",
                false,
            )
            .await;
        assert_eq!(
            blocked["error"]["code"], "MORPHO_POSITION_CHANGED",
            "{blocked}"
        );
        assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 3);
        assert_eq!(rpc.state().shares, current.shares);
        assert_reservation_count(&restarted.plan_status(&plan_id).await, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn reverted_morpho_action_settles_only_gas_and_never_replays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let plan = deposit_plan();
    let plan_id = client.authorize(&plan, "reverted-action").await;
    client.approve_deposit(&plan_id).await;
    let before = rpc.state();
    rpc.revert_next_action.store(true, Ordering::SeqCst);
    let input = plan["steps"][1]["input"].clone();
    let result = client
        .execute(
            "morpho.deposit",
            input.clone(),
            "reverted-deposit",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    let id = operation_id(&result);
    let settled = client.call(vec!["status".into(), id.clone()]).await;
    assert_eq!(settled["status"], "failed", "{settled}");
    let record = journal_record(&path, &id);
    assert_eq!(record.status, Status::Failed);
    let proof = record
        .effects
        .as_ref()
        .expect("A mined revert must prove actual gas");
    assert_eq!(proof.effects.len(), 1, "{proof:?}");
    match &proof.effects[0] {
        FinancialEffect::Cost {
            location,
            amount,
            category,
            treatment,
        } => {
            assert_eq!(
                location,
                &AccountingLocation::Cash {
                    resource: ResourceId(format!("ethereum:1:{OWNER}"))
                }
            );
            assert_eq!(amount.asset.network, "ethereum:1");
            assert_eq!(amount.asset.reference, "native");
            assert_eq!(amount.atoms, Quantity::from_u64(21_000_000));
            assert_eq!(category, "ethereum_gas");
            assert_eq!(treatment, &CostTreatment::AdditionalDebit);
        }
        other => panic!("A revert must have no financial effect except gas: {other:?}"),
    }
    let mut expected = before;
    expected.native -= U256::from(21_000_000);
    assert_eq!(rpc.state(), expected);
    assert_eq!(rpc.state().asset_allowance, U256::from(2_000_000));
    let plan_status = client.plan_status(&plan_id).await;
    assert_reservation_count(&plan_status, 0);
    assert!(
        plan_status["usage"]["consumed_debits"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{plan_status}"
    );
    assert_eq!(
        plan_status["usage"]["consumed_costs"][0]["amount"]["atoms"], "42000000",
        "{plan_status}"
    );
    assert!(
        plan_status["usage"]["verified_proceeds"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{plan_status}"
    );
    let repeated = client
        .execute(
            "morpho.deposit",
            input,
            "reverted-deposit",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    assert_eq!(operation_id(&repeated), id, "{repeated}");
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(journal_record(&path, &id).effects, record.effects);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn missing_then_conflicting_receipt_preserves_identity_reservation_and_blocks_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let plan = deposit_plan();
    let plan_id = client.authorize(&plan, "unresolved-proof").await;
    client.approve_deposit(&plan_id).await;
    rpc.hide_receipts.store(true, Ordering::SeqCst);
    let input = plan["steps"][1]["input"].clone();
    let pending = client
        .execute(
            "morpho.deposit",
            input.clone(),
            "uncertain-deposit",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    let id = operation_id(&pending);
    let initial = journal_record(&path, &id);
    let submission = initial
        .submission
        .as_ref()
        .expect("Dispatch identity must be durable");
    assert!(!initial.status.terminal());
    assert!(initial.effects.is_none());
    assert!(Store::open(&path).unwrap().reservation_count(&id).unwrap() > 0);
    assert_reservation_count(&client.plan_status(&plan_id).await, 1);

    for conflicting in [false, true] {
        rpc.hide_receipts.store(!conflicting, Ordering::SeqCst);
        rpc.conflicting_receipts
            .store(conflicting, Ordering::SeqCst);
        let status = client.call(vec!["status".into(), id.clone()]).await;
        if conflicting {
            assert_eq!(
                status["error"]["code"], "MORPHO_RECEIPT_MISMATCH",
                "{status}"
            );
        } else {
            assert_ne!(status["status"], "completed", "{status}");
        }
        let repeated = client
            .execute(
                "morpho.deposit",
                input.clone(),
                "uncertain-deposit",
                &plan_id,
                "deposit",
                false,
            )
            .await;
        assert_ne!(repeated["status"], "completed", "{repeated}");
        let record = journal_record(&path, &id);
        assert!(!record.status.terminal(), "{record:?}");
        assert_eq!(record.submission.as_ref(), Some(submission));
        assert!(record.effects.is_none());
        assert!(Store::open(&path).unwrap().reservation_count(&id).unwrap() > 0);
        assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
        assert_eq!(rpc.chain.lock().unwrap().signed.len(), 2);
    }
    rpc.conflicting_receipts.store(false, Ordering::SeqCst);
    let settled = client.call(vec!["status".into(), id.clone()]).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    let record = journal_record(&path, &id);
    assert_eq!(record.submission.as_ref(), Some(submission));
    assert!(
        record
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(
                effect,
                FinancialEffect::OriginMovement { debit, credit, .. }
                    if debit.atoms == Quantity::from_u64(2_000_000)
                        && credit.atoms == Quantity::from_u64(2_000_000)
            ))
    );
    assert_eq!(
        Store::open(&path).unwrap().reservation_count(&id).unwrap(),
        0
    );
    assert_reservation_count(&client.plan_status(&plan_id).await, 0);
    assert_eq!(rpc.state().cash, U256::from(8_000_000));
    assert!(rpc.state().shares > U256::ZERO);
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn expired_plan_cannot_spend_the_settled_approval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let clock = Arc::new(AdjustableClock::default());
    let client = Client::new(
        runtime_with_clock(&path, rpc.clone(), approval.clone(), clock.clone()),
        dir.path(),
    )
    .await;
    let plan = deposit_plan();
    let plan_id = client.authorize(&plan, "expiring-deposit").await;
    client.approve_deposit(&plan_id).await;
    let before = rpc.state();
    clock.0.store(3_600, Ordering::SeqCst);
    let rejected = client
        .execute(
            "morpho.deposit",
            plan["steps"][1]["input"].clone(),
            "expired-deposit",
            &plan_id,
            "deposit",
            false,
        )
        .await;
    assert_eq!(
        rejected["error"]["code"], "PLAN_AUTHORIZATION_INACTIVE",
        "{rejected}"
    );
    assert_eq!(rpc.state(), before);
    assert_eq!(rpc.state().asset_allowance, U256::from(2_000_000));
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.chain.lock().unwrap().signed.len(), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_reservation_count(&client.plan_status(&plan_id).await, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires built public CLI: make test-morpho-rehearsal"]
async fn named_morpho_lifecycle_reconciles_lost_reply_after_restart_and_realizes_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let rpc = Arc::new(Rpc::new());
    let approval = Arc::new(Approval::default());
    let mut client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
    let vaults = client.read("morpho.vaults", json!({"chain_id":1})).await;
    assert!(vaults.to_string().contains(VAULT), "{vaults}");
    let inspection = client
        .read(
            "morpho.vault",
            json!({"chain_id":1,"vault":VAULT,"owner":OWNER}),
        )
        .await;
    assert!(
        inspection
            .to_string()
            .contains("canonical_rpc_factory_membership"),
        "{inspection}"
    );
    let plan = plan();
    let review = client
        .call(vec![
            "plans".into(),
            "preview".into(),
            "--input".into(),
            plan.to_string(),
        ])
        .await;
    let hash = review["review_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("{review}"));
    let authorization = client
        .call(vec![
            "plans".into(),
            "authorize".into(),
            "--input".into(),
            plan.to_string(),
            "--id".into(),
            "morpho-roundtrip".into(),
            "--review-hash".into(),
            hash.into(),
        ])
        .await;
    let plan_id = authorization["plan_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{authorization}"))
        .to_owned();
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let steps = plan["steps"].as_array().unwrap();
    for (index, step) in steps.iter().enumerate() {
        let op = step["operation"].as_str().unwrap();
        let id = step["step_id"].as_str().unwrap();
        let request = format!("morpho-{id}");
        let input = step["input"].clone();
        if index == 1 {
            let mut weakened = input.clone();
            weakened["min_shares"] = json!("1");
            let rejected = client
                .execute(op, weakened, "morpho-weakened-floor", &plan_id, id, true)
                .await;
            assert_eq!(
                rejected["error"]["code"], "BOUNDARY_VIOLATION",
                "{rejected}"
            );
            rpc.fail_simulation.store(true, Ordering::SeqCst);
            let failed = client
                .execute(
                    op,
                    input.clone(),
                    "morpho-failed-preview",
                    &plan_id,
                    id,
                    true,
                )
                .await;
            assert!(failed.get("error").is_some(), "{failed}");
            assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 1);
            rpc.fail_simulation.store(false, Ordering::SeqCst);
            rpc.hide_receipts.store(true, Ordering::SeqCst);
            rpc.lose_reply.store(true, Ordering::SeqCst);
        }
        if id == "redeem-all" {
            rpc.loss();
        }
        let result = client
            .execute(op, input.clone(), &request, &plan_id, id, false)
            .await;
        let operation_id = result["operation_id"]
            .as_str()
            .or_else(|| result["id"].as_str())
            .unwrap_or_else(|| panic!("{result}"))
            .to_owned();
        if index == 1 {
            assert_ne!(result["status"], "completed", "{result}");
            let sends = rpc.dispatches.load(Ordering::SeqCst);
            drop(client);
            client = Client::new(runtime(&path, rpc.clone(), approval.clone()), dir.path()).await;
            let pending = client
                .call(vec!["status".into(), operation_id.clone()])
                .await;
            assert_ne!(pending["status"], "completed", "{pending}");
            assert_eq!(rpc.dispatches.load(Ordering::SeqCst), sends);
            let reserved = client
                .call(vec!["plans".into(), "status".into(), plan_id.clone()])
                .await;
            assert_eq!(
                reserved["usage"]["reservations"].as_array().unwrap().len(),
                1,
                "{reserved}"
            );
            assert_eq!(reserved["usage"]["reservations"][0]["step_id"], "deposit");
            rpc.hide_receipts.store(false, Ordering::SeqCst);
        }
        let settled = client
            .call(vec!["status".into(), operation_id.clone()])
            .await;
        assert_eq!(settled["status"], "completed", "{settled}");
        let repeated = client
            .execute(op, input, &request, &plan_id, id, false)
            .await;
        assert_eq!(repeated["id"], result["id"], "{repeated}");
        assert_eq!(rpc.dispatches.load(Ordering::SeqCst), index + 1);
    }
    let state = rpc.state();
    assert_eq!(state.shares, U256::ZERO);
    assert_eq!(state.share_allowance, U256::ZERO);
    assert_eq!(state.asset_allowance, U256::ZERO);
    assert!(state.cash < U256::from(10_000_000));
    assert!(state.cash > U256::from(9_700_000));
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let status = client
        .call(vec!["plans".into(), "status".into(), plan_id])
        .await;
    assert!(status.to_string().contains("completed"), "{status}");
    assert!(
        status["usage"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{status}"
    );
    assert!(
        status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|step| step["state"] == "completed"),
        "{status}"
    );
    assert!(
        status["usage"]["verified_proceeds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|proceeds| proceeds["producing_step_id"] == "partial-withdraw"),
        "{status}"
    );
    let positions = client
        .read(
            "morpho.positions",
            json!({"chain_id":1,"vault":VAULT,"owner":OWNER}),
        )
        .await;
    assert!(
        positions.to_string().contains("\"atoms\":\"0\""),
        "{positions}"
    );
}

#[tokio::test]
async fn routed_share_scope_counts_verified_underlying_once_and_preserves_decimal_identity() {
    let rpc = Arc::new(Rpc::new());
    rpc.change_state(|state| state.shares = U256::from(10).pow(U256::from(18)));
    let router = gofin_broker::EvmNetworkRouter::new(BTreeMap::from([(1, adapter(rpc.clone()))]));
    router
        .read(
            "morpho.vault",
            &json!({"chain_id":1,"vault":VAULT,"owner":OWNER}),
        )
        .await
        .unwrap();
    let share = AssetId {
        network: "ethereum:1".into(),
        reference: VAULT.into(),
        decimals: 18,
    };
    let mut scope = SnapshotScope {
        resources: vec![ResourceId(format!("evm:{OWNER}"))],
        networks: vec!["ethereum:1".into()],
        assets: vec![share.clone()],
    };
    let snapshot = router.snapshot_scoped(&scope).await.unwrap();
    assert!(snapshot.complete, "{:?}", snapshot.pending);
    assert!(
        snapshot
            .state
            .holdings
            .iter()
            .all(|holding| holding.amount.asset.reference != VAULT)
    );
    let positions = &snapshot.state.positions;
    assert_eq!(positions.len(), 1, "{positions:?}");
    let position = &positions[0];
    assert_eq!(position.kind, PositionKind::Investment);
    assert_eq!(position.amount.asset.reference, ASSET);
    assert_eq!(position.amount.asset.decimals, 6);
    let state = rpc.state();
    assert_eq!(
        position.amount.atoms,
        quantity(state.preview_redeem(state.shares))
    );
    assert!(gofin_ethereum::morpho::snapshot::represented_share(
        position, &share
    ));
    assert!(
        snapshot
            .pending
            .iter()
            .all(|reason| !reason.contains("scope-asset-not-covered"))
    );
    scope.assets[0].decimals = 6;
    let wrong_decimals = router.snapshot_scoped(&scope).await.unwrap();
    assert!(!wrong_decimals.complete);
    assert!(
        wrong_decimals
            .pending
            .iter()
            .any(|reason| reason.contains("scope-asset-not-covered")),
        "{:?}",
        wrong_decimals.pending
    );
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_morpho_discovery_and_verified_capacity_require_no_signer() {
    let rpc = Arc::new(Rpc::new());
    let adapter = EthereumAdapter::public_morpho(
        EthereumConfig::wallet_only(1, 1, vec![]),
        Arc::new(EvmRuntime::new(rpc.clone(), 1, None)),
    )
    .unwrap()
    .with_morpho_api(MorphoClient::with_transport(Arc::new(Api)));
    assert!(adapter.wallets.is_empty());
    let directory = adapter
        .read("morpho.vaults", &json!({"network":"evm","chain_id":1}))
        .await
        .unwrap();
    assert_eq!(directory["vaults"][0]["vault_address"], VAULT);
    let inspected = adapter
        .read(
            "morpho.vault",
            &json!({"chain_id":1,"vault":VAULT,"owner":OWNER}),
        )
        .await
        .unwrap();
    let state = &inspected["state"];
    assert_eq!(
        state["write_compatibility"]["supported"], true,
        "{inspected}"
    );
    assert_eq!(state["capacity"]["owner_withdrawable_assets"], "0");
    assert_eq!(state["capacity"]["vault_withdrawable_assets"], "100000000");
    assert_eq!(state["capacity"]["complete"], true);
    assert_eq!(rpc.dispatches.load(Ordering::SeqCst), 0);
}

struct ChangedCode(Arc<Rpc>);
#[async_trait]
impl EvmRpc for ChangedCode {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_getCode" && params[0] == FACTORY {
            return Ok(json!("0x6000"));
        }
        self.0.request(method, params).await
    }
}
#[tokio::test]
async fn explicit_unsupported_deployment_returns_a_reason_without_trusting_the_indexer() {
    let adapter = EthereumAdapter::public_morpho(
        EthereumConfig::wallet_only(1, 1, vec![]),
        Arc::new(EvmRuntime::new(
            Arc::new(ChangedCode(Arc::new(Rpc::new()))),
            1,
            None,
        )),
    )
    .unwrap()
    .with_morpho_api(MorphoClient::with_transport(Arc::new(Api)));
    let result = adapter
        .read("morpho.vault", &json!({"chain_id":1,"vault":VAULT}))
        .await
        .unwrap();
    assert_eq!(result["write_availability"]["supported"], false);
    assert_eq!(
        result["write_availability"]["reason"]["code"],
        "MORPHO_DEPLOYMENT_CHANGED"
    );
    assert_eq!(result["state"], Value::Null);
}

struct ContextGate(Arc<Rpc>);
#[async_trait]
impl EvmRpc for ContextGate {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_call" {
            let calldata = gofin_evm::rpc_bytes(&params[0]["data"])?;
            if calldata.starts_with(&selector("sendAssetsGate()")) {
                return Ok(abi(address("0x0000000000000000000000000000000000000301")));
            }
            if calldata.starts_with(&selector("canSendAssets(address)")) {
                return Err(fault("Gate requires the bundle's transient initiator"));
            }
        }
        self.0.request(method, params).await
    }
}
#[tokio::test]
async fn transient_gate_context_does_not_turn_unknown_capacity_into_zero_or_reject_the_vault() {
    let adapter = EthereumAdapter::public_morpho(
        EthereumConfig::wallet_only(1, 1, vec![]),
        Arc::new(EvmRuntime::new(
            Arc::new(ContextGate(Arc::new(Rpc::new()))),
            1,
            None,
        )),
    )
    .unwrap()
    .with_morpho_api(MorphoClient::with_transport(Arc::new(Api)));
    let result = adapter
        .read(
            "morpho.vault",
            &json!({"chain_id":1,"vault":VAULT,"owner":OWNER}),
        )
        .await
        .unwrap();
    assert_eq!(
        result["state"]["write_compatibility"]["supported"], true,
        "{result}"
    );
    assert_eq!(result["capacity"]["complete"], false);
    assert_eq!(result["capacity"]["deposit_assets"], Value::Null);
    assert_eq!(
        result["capacity"]["advisory_gate_observation"]["send_assets"],
        Value::Null
    );
    assert_eq!(result["capacity"]["exact_bundle_simulation_required"], true);
}
