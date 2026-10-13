//! Modeled two-chain protocol/financial lifecycle. The real Fin broker, engine,
//! signers and SQLite journal run against deterministic in-memory chain RPCs.
//! This is not a rollup, MPT proof, live-write, or Touch ID verification.
use async_trait::async_trait;
use fin_bridge::{
    BridgeAssetInput, BridgeBounds, BridgeQuoteInput, BridgeQuoteIntentInput, FeeCap, FeeKind,
    base_canonical::*,
};
use fin_broker::{BridgeAdapter, SharedUsdValuator};
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
    FinancialPlanStepSpec, FinancialStepCompletion,
};
use fin_domain::{
    Adapter, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, BridgeStepKind,
    BridgeStepLink, FinError, OperationRequest, Quantity, ResourceId, Result, Status, now,
};
use fin_engine::{Engine, EngineOptions};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use fin_evm::alloy::consensus::transaction::SignerRecoverable;
use fin_evm::alloy::{
    consensus::{Transaction, TxEnvelope},
    eips::eip2718::Decodable2718,
    primitives::{B256, Bytes, U256, keccak256},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use fin_evm::{
    EvmRpc, EvmRuntime, LocalEnvelopeSigner, address, hex_bytes, lower_address, quantity, rpc_u64,
    u256,
};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

sol! {
    function isFjord() external view returns (bool);
    function isIsthmus() external view returns (bool);
    function isJovian() external view returns (bool);
    function getL1FeeUpperBound(uint256 size) external view returns (uint256);
    function getOperatorFee(uint256 gas) external view returns (uint256);
    function bridgeETHTo(address to,uint32 minGasLimit,bytes extraData) external payable;
    function finalizeBridgeETH(address from,address to,uint256 amount,bytes extraData) external payable;
    function relayMessage(uint256 nonce,address sender,address target,uint256 value,uint256 minGasLimit,bytes message) external payable;
    struct Withdrawal {uint256 nonce;address sender;address target;uint256 value;uint256 gasLimit;bytes data;}
    struct OutputProof {bytes32 version;bytes32 stateRoot;bytes32 messagePasserStorageRoot;bytes32 latestBlockhash;}
    function proveWithdrawalTransaction(Withdrawal tx_,uint256 disputeGameIndex,OutputProof outputRootProof,bytes[] withdrawalProof) external;
    function finalizeWithdrawalTransactionExternalProof(Withdrawal tx_,address proofSubmitter) external;
    function anchorStateRegistry() external view returns (address);
    function disputeGameFactory() external view returns (address);
    function finalizedWithdrawals(bytes32 hash) external view returns (bool);
    function provenWithdrawals(bytes32 hash,address submitter) external view returns (address,uint64);
    function proofMaturityDelaySeconds() external view returns (uint256);
    function checkWithdrawal(bytes32 hash,address proofSubmitter) external view;
    function isGameProper(address game) external view returns (bool);
    function isGameRespected(address game) external view returns (bool);
    function isGameClaimValid(address game) external view returns (bool);
    function gameCount() external view returns (uint256);
    function gameAtIndex(uint256 index) external view returns (uint32,uint64,address);
    function l2SequenceNumber() external view returns (uint256);
    function rootClaim() external view returns (bytes32);
    function status() external view returns (uint8);
    function expectedResolution() external view returns (uint64);
    event ETHBridgeInitiated(address indexed from,address indexed to,uint256 amount,bytes extraData);
    event ETHBridgeFinalized(address indexed from,address indexed to,uint256 amount,bytes extraData);
    event MessagePassed(uint256 indexed nonce,address indexed sender,address indexed target,uint256 value,uint256 gasLimit,bytes data,bytes32 withdrawalHash);
    event WithdrawalProven(bytes32 indexed withdrawalHash,address indexed from,address indexed to);
    event WithdrawalProvenExtension1(bytes32 indexed withdrawalHash,address indexed proofSubmitter);
    event WithdrawalFinalized(bytes32 indexed withdrawalHash,bool success);
    event RelayedMessage(bytes32 indexed msgHash);
}
const GAME: &str = "0x9999999999999999999999999999999999999999";
const FRESH_GAME: &str = "0x9898989898989898989898989898989898989898";
fn block_hash(chain: u64) -> B256 {
    B256::repeat_byte(if chain == 1 { 0x44 } else { 0x22 })
}
fn block(chain: u64, number: u64) -> Value {
    json!({"hash":format!("{:#x}",block_hash(chain)),"number":format!("{number:#x}"),"timestamp":"0x2000","baseFeePerGas":"0x3b9aca00","stateRoot":format!("{:#x}",B256::repeat_byte(0x66)),"extraData":"0x000000000000000000"})
}
fn event<E: SolEvent>(chain: u64, address_: &str, index: u64, event: E) -> Value {
    let log = event.encode_log_data();
    json!({"address":address_,"topics":log.topics().iter().map(|h|format!("{h:#x}")).collect::<Vec<_>>(),"data":hex_bytes(&log.data),"logIndex":format!("{index:#x}"),"removed":false,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(chain))})
}
fn encoded<T: SolValue>(value: T) -> Value {
    json!(hex_bytes(&value.abi_encode()))
}
fn failure(code: &str) -> FinError {
    FinError::new(
        code,
        "fixture",
        "Modeled protocol rejected the request.",
        "Keep the original operation identity.",
    )
}
#[derive(Default)]
struct ChainState {
    transactions: BTreeMap<(u64, String), (Value, Value)>,
    broadcasts: BTreeMap<u64, usize>,
    withdrawal: Option<BaseWithdrawal>,
    message_hash: Option<String>,
    proven: bool,
    mature: bool,
    finalized: bool,
    invalidated: bool,
    new_game_published: bool,
    proven_game_index: u64,
    eth_base_fee: Option<u64>,
    revert_next: Option<u64>,
    lose_response: Option<u64>,
    omit_credit: bool,
    external_logs: Vec<Value>,
}
struct Rpc {
    chain: u64,
    state: Arc<Mutex<ChainState>>,
}
impl Rpc {
    fn output_root(&self) -> B256 {
        let proof = BaseWithdrawalWitness {
            dispute_game_index: 0,
            checkpoint_block: 110,
            version: format!("{:#x}", B256::ZERO),
            state_root: format!("{:#x}", B256::repeat_byte(0x66)),
            message_passer_storage_root: format!("{:#x}", B256::repeat_byte(0x77)),
            latest_block_hash: format!("{:#x}", block_hash(8453)),
            withdrawal_proof: vec!["0xc0".into()],
            output_root: String::new(),
        };
        proof.root().unwrap()
    }
    fn call(&self, params: &Value, state: &ChainState) -> Result<Value> {
        let raw = fin_evm::rpc_bytes(&params[0]["data"])?;
        let selector = raw.get(..4).unwrap_or_default();
        if selector == isFjordCall::SELECTOR {
            return Ok(encoded(true));
        }
        if selector == isIsthmusCall::SELECTOR || selector == isJovianCall::SELECTOR {
            return Ok(encoded(false));
        }
        if selector == getL1FeeUpperBoundCall::SELECTOR {
            return Ok(encoded(U256::from(100)));
        }
        if selector == getOperatorFeeCall::SELECTOR {
            return Ok(encoded(U256::ZERO));
        }
        if selector == anchorStateRegistryCall::SELECTOR {
            return Ok(encoded(address(ANCHOR_REGISTRY)?));
        }
        if selector == disputeGameFactoryCall::SELECTOR {
            return Ok(encoded(address(GAME_FACTORY)?));
        }
        if selector == finalizedWithdrawalsCall::SELECTOR {
            return Ok(encoded(state.finalized));
        }
        if selector == provenWithdrawalsCall::SELECTOR {
            return Ok(encoded((
                address(if state.proven_game_index == 0 {
                    GAME
                } else {
                    FRESH_GAME
                })?,
                if state.proven { 100u64 } else { 0u64 },
            )));
        }
        if selector == proofMaturityDelaySecondsCall::SELECTOR {
            return Ok(encoded(U256::from(3600)));
        }
        if selector == isGameProperCall::SELECTOR || selector == isGameRespectedCall::SELECTOR {
            let bytes = &raw[4..];
            let game = fin_evm::alloy::primitives::Address::from_slice(&bytes[12..32]);
            return Ok(encoded(!(state.invalidated && game == address(GAME)?)));
        }
        if selector == isGameClaimValidCall::SELECTOR {
            let decoded = isGameClaimValidCall::abi_decode(&raw).unwrap();
            return Ok(encoded(
                state.mature && !(state.invalidated && decoded.game == address(GAME)?),
            ));
        }
        if selector == statusCall::SELECTOR {
            let old = params[0]["to"]
                .as_str()
                .is_some_and(|to| to.eq_ignore_ascii_case(GAME));
            return Ok(encoded(U256::from(if state.invalidated && old {
                1
            } else if state.mature {
                2
            } else {
                0
            })));
        }
        if selector == expectedResolutionCall::SELECTOR {
            return Ok(encoded(5000u64));
        }
        if selector == gameCountCall::SELECTOR {
            return Ok(encoded(U256::from(if state.new_game_published {
                2
            } else {
                1
            })));
        }
        if selector == gameAtIndexCall::SELECTOR {
            let decoded = gameAtIndexCall::abi_decode(&raw).unwrap();
            return Ok(encoded((
                1u32,
                50u64,
                address(if decoded.index == U256::ZERO {
                    GAME
                } else {
                    FRESH_GAME
                })?,
            )));
        }
        if selector == l2SequenceNumberCall::SELECTOR {
            return Ok(encoded(U256::from(110)));
        }
        if selector == rootClaimCall::SELECTOR {
            return Ok(encoded(self.output_root()));
        }
        if selector == checkWithdrawalCall::SELECTOR {
            if !state.proven
                || !state.mature
                || (state.invalidated && state.proven_game_index == 0)
                || state.finalized
            {
                return Err(failure("RPC_REQUEST_FAILED"));
            }
            return Ok(json!("0x"));
        }
        if selector == proveWithdrawalTransactionCall::SELECTOR {
            let decoded = proveWithdrawalTransactionCall::abi_decode(&raw)
                .map_err(|_| failure("BAD_PROOF_CALL"))?;
            let saved = state
                .withdrawal
                .as_ref()
                .ok_or_else(|| failure("NO_WITHDRAWAL"))?;
            let actual = BaseWithdrawal {
                nonce: quantity(decoded.tx_.nonce),
                sender: lower_address(decoded.tx_.sender),
                target: lower_address(decoded.tx_.target),
                value: quantity(decoded.tx_.value),
                gas_limit: quantity(decoded.tx_.gasLimit),
                data: hex_bytes(&decoded.tx_.data),
            };
            if &actual != saved
                || decoded.disputeGameIndex
                    != U256::from(if state.new_game_published { 1 } else { 0 })
                || (state.invalidated && !state.new_game_published)
                || decoded.outputRootProof.latestBlockhash != block_hash(8453)
            {
                return Err(failure("BAD_PROOF_CALL"));
            }
            return Ok(json!("0x"));
        }
        if selector == finalizeWithdrawalTransactionExternalProofCall::SELECTOR {
            let decoded = finalizeWithdrawalTransactionExternalProofCall::abi_decode(&raw)
                .map_err(|_| failure("BAD_FINAL_CALL"))?;
            let saved = state
                .withdrawal
                .as_ref()
                .ok_or_else(|| failure("NO_WITHDRAWAL"))?;
            let actual = BaseWithdrawal {
                nonce: quantity(decoded.tx_.nonce),
                sender: lower_address(decoded.tx_.sender),
                target: lower_address(decoded.tx_.target),
                value: quantity(decoded.tx_.value),
                gas_limit: quantity(decoded.tx_.gasLimit),
                data: hex_bytes(&decoded.tx_.data),
            };
            if &actual != saved
                || !state.proven
                || !state.mature
                || (state.invalidated && state.proven_game_index == 0)
                || state.finalized
            {
                return Err(failure("RPC_REQUEST_FAILED"));
            }
            return Ok(json!("0x"));
        }
        if selector == bridgeETHToCall::SELECTOR {
            return Ok(json!("0x"));
        }
        Err(failure("UNEXPECTED_FIXTURE_CALL"))
    }
}
#[async_trait]
impl EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut state = self.state.lock().unwrap();
        Ok(match method {
            "eth_chainId" => json!(format!("{:#x}", self.chain)),
            "eth_getCode" => {
                let account = params[0].as_str().unwrap_or_default();
                if [
                    PORTAL,
                    L1_STANDARD_BRIDGE,
                    L2_STANDARD_BRIDGE,
                    GAME_FACTORY,
                    ANCHOR_REGISTRY,
                    GAME,
                    FRESH_GAME,
                    MESSAGE_PASSER,
                    "0x420000000000000000000000000000000000000f",
                ]
                .iter()
                .any(|a| account.eq_ignore_ascii_case(a))
                {
                    json!("0x6000")
                } else {
                    json!("0x")
                }
            }
            "eth_getBalance" => json!("0x56bc75e2d63100000"),
            "eth_blockNumber" => json!("0x6e"),
            "eth_getBlockByHash" => block(self.chain, 100),
            "eth_getBlockByNumber" => {
                let mut header = block(
                    self.chain,
                    if params[0] == json!("latest") || params[0] == json!("finalized") {
                        110
                    } else {
                        rpc_u64(&params[0])?
                    },
                );
                if self.chain == 1
                    && let Some(fee) = state.eth_base_fee
                {
                    header["baseFeePerGas"] = json!(format!("{fee:#x}"));
                }
                header
            }
            "eth_getTransactionCount" => json!(format!(
                "{:#x}",
                state.broadcasts.get(&self.chain).copied().unwrap_or(0)
            )),
            "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
            "eth_estimateGas" => json!("0x186a0"),
            "eth_call" => self.call(&params, &state)?,
            "eth_getProof" => {
                let saved = state
                    .withdrawal
                    .as_ref()
                    .ok_or_else(|| failure("NO_WITHDRAWAL"))?;
                let key = withdrawal_storage_key(saved.hash()?);
                if params[0] != json!(MESSAGE_PASSER) || params[1] != json!([format!("{key:#x}")]) {
                    return Err(failure("BAD_STORAGE_KEY"));
                }
                json!({"address":MESSAGE_PASSER,"storageHash":format!("{:#x}",B256::repeat_byte(0x77)),"storageProof":[{"key":format!("{key:#x}"),"value":"0x1","proof":["0xc0"]}]})
            }
            "eth_getLogs" => json!(state.external_logs),
            "eth_getTransactionByHash" | "eth_getTransactionReceipt" => state
                .transactions
                .get(&(self.chain, params[0].as_str().unwrap_or_default().into()))
                .map(|(tx, receipt)| {
                    if method == "eth_getTransactionByHash" {
                        tx.clone()
                    } else {
                        receipt.clone()
                    }
                })
                .unwrap_or(Value::Null),
            "eth_sendRawTransaction" => {
                let bytes = fin_evm::rpc_bytes(&params[0])?;
                let envelope = TxEnvelope::decode_2718(&mut bytes.as_ref())
                    .map_err(|_| failure("BAD_ENVELOPE"))?;
                let tx_hash = format!("{:#x}", keccak256(&bytes));
                let sender = envelope
                    .recover_signer()
                    .map_err(|_| failure("BAD_SIGNER"))?;
                assert_eq!(envelope.chain_id(), Some(self.chain));
                let selector = &envelope.input()[..4];
                let reverted = state.revert_next == Some(self.chain);
                if reverted {
                    state.revert_next = None;
                }
                let old_proven = state.proven;
                let old_game = state.proven_game_index;
                let old_finalized = state.finalized;
                let mut logs = Vec::new();
                if self.chain == 8453 && selector == bridgeETHToCall::SELECTOR {
                    let call = bridgeETHToCall::abi_decode(envelope.input()).unwrap();
                    assert_eq!(call.minGasLimit, 200_000);
                    assert!(call.extraData.is_empty());
                    let inner = finalizeBridgeETHCall {
                        from: sender,
                        to: call.to,
                        amount: envelope.value(),
                        extraData: Bytes::new(),
                    }
                    .abi_encode();
                    let relay = relayMessageCall {
                        nonce: (U256::from(1) << 240) + U256::from(42),
                        sender: address(L2_STANDARD_BRIDGE)?,
                        target: address(L1_STANDARD_BRIDGE)?,
                        value: envelope.value(),
                        minGasLimit: U256::from(200_000),
                        message: inner.into(),
                    }
                    .abi_encode();
                    let withdrawal = BaseWithdrawal {
                        nonce: Quantity::from_u64(77),
                        sender: L2_MESSENGER.into(),
                        target: L1_MESSENGER.into(),
                        value: quantity(envelope.value()),
                        gas_limit: Quantity::from_u64(350_000),
                        data: hex_bytes(&relay),
                    };
                    logs.push(event(
                        8453,
                        L2_STANDARD_BRIDGE,
                        1,
                        ETHBridgeInitiated {
                            from: sender,
                            to: call.to,
                            amount: envelope.value(),
                            extraData: Bytes::new(),
                        },
                    ));
                    logs.push(event(
                        8453,
                        MESSAGE_PASSER,
                        3,
                        MessagePassed {
                            nonce: U256::from(77),
                            sender: address(L2_MESSENGER)?,
                            target: address(L1_MESSENGER)?,
                            value: envelope.value(),
                            gasLimit: U256::from(350_000),
                            data: relay.clone().into(),
                            withdrawalHash: withdrawal.hash()?,
                        },
                    ));
                    state.message_hash = Some(format!("{:#x}", keccak256(&relay)));
                    state.withdrawal = Some(withdrawal);
                } else if self.chain == 1 && selector == proveWithdrawalTransactionCall::SELECTOR {
                    self.call(&json!([{"data":hex_bytes(envelope.input())}]), &state)?;
                    let saved = state.withdrawal.as_ref().unwrap();
                    let hash = saved.hash()?;
                    logs.push(event(
                        1,
                        PORTAL,
                        1,
                        WithdrawalProven {
                            withdrawalHash: hash,
                            from: address(&saved.sender)?,
                            to: address(&saved.target)?,
                        },
                    ));
                    logs.push(event(
                        1,
                        PORTAL,
                        2,
                        WithdrawalProvenExtension1 {
                            withdrawalHash: hash,
                            proofSubmitter: sender,
                        },
                    ));
                    state.proven = true;
                    state.proven_game_index = if state.new_game_published { 1 } else { 0 };
                } else if self.chain == 1
                    && selector == finalizeWithdrawalTransactionExternalProofCall::SELECTOR
                {
                    self.call(&json!([{"data":hex_bytes(envelope.input())}]), &state)?;
                    let saved = state.withdrawal.as_ref().unwrap();
                    let hash = saved.hash()?;
                    let amount = u256(&saved.value)?;
                    logs.push(event(
                        1,
                        PORTAL,
                        1,
                        WithdrawalFinalized {
                            withdrawalHash: hash,
                            success: true,
                        },
                    ));
                    if !state.omit_credit {
                        logs.push(event(
                            1,
                            L1_STANDARD_BRIDGE,
                            2,
                            ETHBridgeFinalized {
                                from: sender,
                                to: sender,
                                amount,
                                extraData: Bytes::new(),
                            },
                        ));
                        logs.push(event(
                            1,
                            L1_MESSENGER,
                            3,
                            RelayedMessage {
                                msgHash: state.message_hash.as_deref().unwrap().parse().unwrap(),
                            },
                        ));
                    }
                    state.finalized = true;
                } else {
                    return Err(failure("UNREVIEWED_FINANCIAL_CALL"));
                }
                if reverted {
                    logs.clear();
                    state.proven = old_proven;
                    state.proven_game_index = old_game;
                    state.finalized = old_finalized;
                }
                let tx = json!({"hash":tx_hash,"from":lower_address(sender),"to":lower_address(envelope.to().unwrap()),"value":format!("{:#x}",envelope.value()),"input":hex_bytes(envelope.input()),"nonce":format!("{:#x}",envelope.nonce()),"gas":format!("{:#x}",envelope.gas_limit()),"chainId":format!("{:#x}",self.chain),"type":"0x2","maxFeePerGas":format!("{:#x}",envelope.max_fee_per_gas()),"maxPriorityFeePerGas":format!("{:#x}",envelope.max_priority_fee_per_gas().unwrap()),"accessList":[],"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(self.chain))});
                let receipt = json!({"transactionHash":tx_hash,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(self.chain)),"status":if reverted {"0x0"}else{"0x1"},"gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","l1Fee":"0x64","logs":logs});
                state
                    .transactions
                    .insert((self.chain, tx_hash.clone()), (tx, receipt));
                *state.broadcasts.entry(self.chain).or_default() += 1;
                if state.lose_response == Some(self.chain) {
                    state.lose_response = None;
                    return Err(failure("RPC_UNAVAILABLE"));
                }
                json!(tx_hash)
            }
            other => {
                return Err(FinError::new(
                    "UNEXPECTED_FIXTURE_RPC",
                    "fixture",
                    format!("Unexpected {other}"),
                    "Extend only a justified synthetic read fixture.",
                ));
            }
        })
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
            source: "modeled Base lifecycle".into(),
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
            mechanism: "synthetic Base lifecycle approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Harness {
    temp: tempfile::TempDir,
    state: BridgeStateRepository,
    chain: Arc<Mutex<ChainState>>,
    approvals: Arc<Approvals>,
    wallet: String,
    bridge: Arc<BridgeAdapter>,
    engine: Arc<Engine>,
}
impl Harness {
    fn new() -> Self {
        Self::build(
            tempfile::tempdir().unwrap(),
            Arc::new(Mutex::new(ChainState::default())),
            Arc::new(Approvals::default()),
        )
    }
    fn build(
        temp: tempfile::TempDir,
        chain: Arc<Mutex<ChainState>>,
        approvals: Arc<Approvals>,
    ) -> Self {
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let wallet = lower_address(signer.address());
        let base_rpc = Arc::new(Rpc {
            chain: 8453,
            state: chain.clone(),
        });
        let eth_rpc = Arc::new(Rpc {
            chain: 1,
            state: chain.clone(),
        });
        let base = Arc::new(EvmRuntime::new(
            base_rpc.clone(),
            8453,
            Some(signer.clone()),
        ));
        let eth = Arc::new(EvmRuntime::new(eth_rpc.clone(), 1, Some(signer)));
        let valuator = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
        let make_wallet = |chain, runtime: Arc<EvmRuntime>| {
            let mut config = EthereumConfig::wallet_only(chain, 1, vec![]);
            config.require_finalized = false;
            Arc::new(
                EthereumAdapter::new(
                    config,
                    vec![WalletConnection {
                        name: "model-wallet".into(),
                        address: wallet.clone(),
                        signing_enabled: true,
                    }],
                    runtime,
                )
                .unwrap()
                .with_wallet_valuation(valuator.clone()),
            )
        };
        let base_wallet = make_wallet(8453, base.clone());
        let eth_wallet = make_wallet(1, eth.clone());
        let provider = Arc::new(
            BaseCanonicalProvider::new(
                Arc::new(EvmRuntime::new(eth_rpc, 1, None)),
                Arc::new(EvmRuntime::new(base_rpc, 8453, None)),
                BaseCanonicalConfig::reviewed(),
            )
            .unwrap(),
        );
        let bridge = Arc::new(
            BridgeAdapter::with_providers(
                vec![provider],
                BTreeMap::from([(8453, base_wallet.clone()), (1, eth_wallet)]),
                BTreeMap::from([(8453, base), (1, eth)]),
                valuator,
            )
            .unwrap(),
        );
        let shared = Store::open(temp.path().join("modeled-base.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(shared.clone());
        bridge.attach_state(state.clone()).unwrap();
        let engine = Arc::new(
            Engine::new_with_shared_store(
                shared.clone(),
                vec![base_wallet, bridge.clone()],
                approvals.clone(),
                EngineOptions {
                    max_movement_and_cost_usd_micros: "100000000".into(),
                    max_run_allocation_usd_micros: "100000000".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap(),
        );
        bridge.attach_engine(Arc::downgrade(&engine)).unwrap();
        Self {
            temp,
            state,
            chain,
            approvals,
            wallet,
            bridge,
            engine,
        }
    }
    fn restart(self) -> Self {
        let Self {
            temp,
            chain,
            approvals,
            ..
        } = self;
        Self::build(temp, chain, approvals)
    }
    fn publish_external_finalization(&self) {
        let mut state = self.chain.lock().unwrap();
        let saved = state.withdrawal.as_ref().unwrap();
        let withdrawal_hash = saved.hash().unwrap();
        let amount = u256(&saved.value).unwrap();
        let hash = format!("{:#x}", B256::repeat_byte(0xab));
        let wallet = address(&self.wallet).unwrap();
        let mut finalized = event(
            1,
            PORTAL,
            1,
            WithdrawalFinalized {
                withdrawalHash: withdrawal_hash,
                success: true,
            },
        );
        finalized["transactionHash"] = json!(hash);
        let logs = vec![
            finalized.clone(),
            event(
                1,
                L1_STANDARD_BRIDGE,
                2,
                ETHBridgeFinalized {
                    from: wallet,
                    to: wallet,
                    amount,
                    extraData: Bytes::new(),
                },
            ),
            event(
                1,
                L1_MESSENGER,
                3,
                RelayedMessage {
                    msgHash: state.message_hash.as_deref().unwrap().parse().unwrap(),
                },
            ),
        ];
        let tx = json!({"hash":hash,"from":"0x5555555555555555555555555555555555555555","to":PORTAL,"value":"0x0","input":"0x","nonce":"0x0","gas":"0x7a120","chainId":"0x1","type":"0x2","blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(1))});
        let receipt = json!({"transactionHash":hash,"blockNumber":"0x64","blockHash":format!("{:#x}",block_hash(1)),"status":"0x1","gasUsed":"0x7530","effectiveGasPrice":"0x3b9aca00","logs":logs});
        state.transactions.insert((1, hash), (tx, receipt));
        state.external_logs.push(finalized);
        state.finalized = true;
    }
    fn bounds(&self) -> BridgeBounds {
        BridgeBounds {
            fee_caps: [8453, 1]
                .into_iter()
                .map(|chain_id| FeeCap {
                    asset: fin_bridge::EvmAsset {
                        chain_id,
                        token: None,
                        decimals: 18,
                    }
                    .asset_id(),
                    kind: FeeKind::Gas,
                    maximum: Quantity::new("10000000000000000").unwrap(),
                    maximum_usd_micros: Some(20_000_000),
                })
                .collect(),
            ..Default::default()
        }
    }
    async fn quote(&self) -> (String, Vec<BridgeStepLink>, Vec<Value>) {
        let input = BridgeQuoteInput {
            provider: None,
            cctp: None,
            intent: BridgeQuoteIntentInput {
                wallet: "model-wallet".into(),
                source: BridgeAssetInput {
                    chain_id: 8453,
                    token: None,
                },
                destination_chain_id: 1,
                destination_assets: vec![BridgeAssetInput {
                    chain_id: 1,
                    token: None,
                }],
                recipient: self.wallet.clone(),
                refund_to: self.wallet.clone(),
                allowed_refund_assets: Vec::new(),
                amount: Quantity::from_u64(1_000_000_000_000),
            },
            bounds: self.bounds(),
            refresh_transfer_id: None,
        };
        let quoted = self
            .bridge
            .read("bridge.quotes", &serde_json::to_value(input).unwrap())
            .await
            .unwrap();
        let transfer = quoted["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap_or_else(|| panic!("Modeled source quote unavailable: {quoted}"))
            .to_owned();
        let mut links: Vec<BridgeStepLink> =
            serde_json::from_value(quoted["transfers"][0]["steps"].clone()).unwrap();
        links.sort_by_key(|s| match s.kind {
            BridgeStepKind::Deposit => 0,
            BridgeStepKind::Prove => 1,
            BridgeStepKind::Finalize => 2,
            _ => panic!("Native fixture has no approvals"),
        });
        assert_eq!(links.len(), 3);
        let inputs=links.iter().map(|s|json!({"transferId":transfer,"stepId":s.step_id,"stepKind":s.kind,"bounds":self.bounds()})).collect();
        (transfer, links, inputs)
    }
    fn plan(&self, inputs: &[Value]) -> FinancialPlanSpec {
        let amount = fin_bridge::EvmAsset {
            chain_id: 8453,
            token: None,
            decimals: 18,
        }
        .amount(Quantity::from_u64(1_000_000_000_000));
        FinancialPlanSpec {
            version: fin_domain::financial_plans::FINANCIAL_PLAN_VERSION,
            name: "Modeled Base canonical lifecycle".into(),
            summary: None,
            steps: inputs
                .iter()
                .enumerate()
                .map(|(i, input)| FinancialPlanStepSpec {
                    action_deadline_seconds: None,
                    step_id: format!("stage-{i}"),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    permitted_variations: json!({}),
                    completion: FinancialStepCompletion {
                        operation_succeeded: true,
                        principal_spend: None,
                        minimum_net_receipt: if i == 2 {
                            Some(fin_domain::ResourceAmount {
                                resource: ResourceId(format!("ethereum:1:{}", self.wallet)),
                                amount: fin_bridge::EvmAsset {
                                    chain_id: 1,
                                    token: None,
                                    decimals: 18,
                                }
                                .amount(Quantity::from_u64(1_000_000_000_000)),
                            })
                        } else {
                            None
                        },
                    },
                    funding_dependencies: Vec::new(),
                })
                .collect(),
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![fin_domain::ResourceAmount {
                    resource: ResourceId(format!("ethereum:8453:{}", self.wallet)),
                    amount,
                }],
                maximum_costs: self
                    .bounds()
                    .fee_caps
                    .into_iter()
                    .map(|cap| fin_domain::CostBound {
                        location: fin_domain::AccountingLocation::Cash {
                            resource: ResourceId(format!("{}:{}", cap.asset.network, self.wallet)),
                        },
                        amount: fin_domain::Amount {
                            asset: cap.asset,
                            atoms: cap.maximum,
                        },
                        treatment: fin_domain::CostTreatment::AdditionalDebit,
                    })
                    .collect(),
            },
        }
    }
    async fn authorize(
        &self,
        plan: FinancialPlanSpec,
        seconds: u64,
        replaces: Option<String>,
        request_id: &str,
    ) -> String {
        let request = FinancialPlanAuthorizationRequest {
            request_id: format!("review-{request_id}"),
            plan: plan.clone(),
            valid_for_seconds: Some(seconds),
            replaces_revision_id: replaces.clone(),
            expected_review_hash: None,
        };
        let review = self.engine.preview_financial_plan(request).await.unwrap();
        self.engine
            .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                request_id: request_id.into(),
                plan,
                valid_for_seconds: Some(seconds),
                replaces_revision_id: replaces,
                expected_review_hash: Some(review.review_hash),
            })
            .await
            .unwrap()
            .plan_id
    }
    async fn execute(
        &self,
        plan: &str,
        index: usize,
        input: &Value,
        request: &str,
    ) -> Result<fin_domain::OperationRecord> {
        self.engine
            .execute_financial_plan_step(
                plan,
                &format!("stage-{index}"),
                OperationRequest {
                    request_id: request.into(),
                    operation: "bridge.transfer".into(),
                    input: input.clone(),
                    run_id: None,
                },
            )
            .await
    }
    async fn settle(
        &self,
        mut operation: fin_domain::OperationRecord,
    ) -> fin_domain::OperationRecord {
        for _ in 0..3 {
            if matches!(operation.status, Status::Completed | Status::Failed) {
                break;
            }
            operation = self.engine.status(&operation.id, true).await.unwrap();
        }
        assert_eq!(
            operation.status,
            Status::Completed,
            "Modeled source/stage did not settle: {:?}",
            operation.error
        );
        operation
    }
}

#[tokio::test]
async fn modeled_three_stage_plan_rejects_early_finalize_then_verifies_destination_once() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let plan = h
        .authorize(h.plan(&inputs), 600, None, "authorize-model")
        .await;
    let early = h
        .execute(&plan, 1, &inputs[1], "early-prove")
        .await
        .unwrap_err();
    assert!(
        [
            "PLAN_STEP_NOT_ELIGIBLE",
            "BRIDGE_SOURCE_PROOF_REQUIRED",
            "FINANCIAL_PLAN_STEP_BLOCKED"
        ]
        .contains(&early.code.as_str()),
        "Unexpected early rejection: {early:?}"
    );
    let source = h
        .settle(h.execute(&plan, 0, &inputs[0], "source").await.unwrap())
        .await;
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().state,
        fin_domain::BridgeTransferState::InTransit
    );
    let duplicate = h.execute(&plan, 0, &inputs[0], "source").await.unwrap();
    assert_eq!(duplicate.id, source.id);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    let proof = h
        .settle(h.execute(&plan, 1, &inputs[1], "proof").await.unwrap())
        .await;
    assert_eq!(proof.operation, "bridge.transfer");
    let after_proof = h.engine.financial_plan_status(&plan).await.unwrap();
    assert_eq!(
        after_proof.steps[1].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::Completed,
        "Prove operation completed but plan progress is {:?}",
        after_proof
            .steps
            .iter()
            .map(|s| (&s.step_id, &s.state, &s.blockers))
            .collect::<Vec<_>>()
    );
    let immature = h
        .execute(&plan, 2, &inputs[2], "early-finalize")
        .await
        .unwrap_err();
    assert_eq!(
        immature.code,
        "BRIDGE_FINALIZATION_PENDING",
        "Model plan progress: {:?}",
        after_proof
            .steps
            .iter()
            .map(|s| (&s.step_id, &s.state, &s.blockers))
            .collect::<Vec<_>>()
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
    h.chain.lock().unwrap().mature = true;
    let finalization = h
        .settle(h.execute(&plan, 2, &inputs[2], "finalize").await.unwrap())
        .await;
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed", "{status}");
    let duplicate = h.execute(&plan, 2, &inputs[2], "finalize").await.unwrap();
    assert_eq!(duplicate.id, finalization.id);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 2);
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(record.transit.is_none());
    assert_eq!(record.settlement_proofs.len(), 1);
    let plan_status = h.engine.financial_plan_status(&plan).await.unwrap();
    assert!(
        plan_status
            .steps
            .iter()
            .all(|s| s.state
                == fin_domain::financial_plans::FinancialPlanStepProgressState::Completed)
    );
}

#[tokio::test]
async fn modeled_response_loss_each_stage_reconciles_original_hash_after_restart() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let mut spec = h.plan(&inputs);
    spec.steps[2].completion.minimum_net_receipt = None;
    let plan = h.authorize(spec, 600, None, "authorize-loss-model").await;
    h.chain.lock().unwrap().lose_response = Some(8453);
    let source = h
        .execute(&plan, 0, &inputs[0], "lost-source")
        .await
        .unwrap();
    assert!(matches!(
        source.status,
        Status::Unknown | Status::Submitted | Status::Pending
    ));
    assert!(source.submission.is_some());
    let h = h.restart();
    let source = h.settle(source).await;
    assert_eq!(
        h.state.transfer(&transfer).unwrap().unwrap().state,
        fin_domain::BridgeTransferState::InTransit
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    h.chain.lock().unwrap().lose_response = Some(1);
    let proof = h.execute(&plan, 1, &inputs[1], "lost-proof").await.unwrap();
    assert!(proof.submission.is_some());
    let h = h.restart();
    let proof = h.settle(proof).await;
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
    h.chain.lock().unwrap().mature = true;
    h.chain.lock().unwrap().lose_response = Some(1);
    let finalization = h
        .execute(&plan, 2, &inputs[2], "lost-finalize")
        .await
        .unwrap();
    assert!(finalization.submission.is_some());
    let h = h.restart();
    let finalization = h.settle(finalization).await;
    for (index, original, id) in [
        (0, source, "lost-source"),
        (1, proof, "lost-proof"),
        (2, finalization, "lost-finalize"),
    ] {
        let duplicate = h.execute(&plan, index, &inputs[index], id).await.unwrap();
        assert_eq!(duplicate.id, original.id);
        assert_eq!(
            duplicate.submission.as_ref().unwrap().reference,
            original.submission.as_ref().unwrap().reference
        );
    }
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 2);
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    assert_eq!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .settlement_proofs
            .len(),
        1
    );
}

#[tokio::test]
async fn modeled_expired_plan_keeps_source_identity_and_renews_same_parent_without_replay() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let mut spec = h.plan(&inputs);
    spec.steps[2].completion.minimum_net_receipt = None;
    let plan = h
        .authorize(spec.clone(), 1, None, "authorize-short-model")
        .await;
    let source = h
        .settle(
            h.execute(&plan, 0, &inputs[0], "short-source")
                .await
                .unwrap(),
        )
        .await;
    let original_locator = h
        .state
        .transfer(&transfer)
        .unwrap()
        .unwrap()
        .transit
        .unwrap()
        .destination_locator
        .unwrap();
    // This test observes actual authorization expiry; RPC clock controls never
    // change the real Fin authority clock or its durable consumed accounting.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let h = h.restart();
    let expired = h.engine.financial_plan_status(&plan).await.unwrap();
    assert_eq!(
        expired.revision.status,
        fin_domain::financial_plans::FinancialPlanAuthorizationStatus::Expired
    );
    assert!(
        h.execute(&plan, 1, &inputs[1], "expired-proof")
            .await
            .is_err()
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.chain
            .lock()
            .unwrap()
            .broadcasts
            .get(&1)
            .copied()
            .unwrap_or(0),
        0
    );
    let renewed = h
        .authorize(spec, 600, Some(expired.revision.revision_id), "renew-model")
        .await;
    assert_eq!(renewed, plan);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 2);
    let current = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        current
            .transit
            .as_ref()
            .unwrap()
            .destination_locator
            .as_ref()
            .unwrap(),
        &original_locator
    );
    assert_eq!(
        current.steps[0].operation_id.as_deref(),
        Some(source.id.as_str())
    );
    h.settle(
        h.execute(&plan, 1, &inputs[1], "renewed-proof")
            .await
            .unwrap(),
    )
    .await;
    h.chain.lock().unwrap().mature = true;
    h.settle(
        h.execute(&plan, 2, &inputs[2], "renewed-finalize")
            .await
            .unwrap(),
    )
    .await;
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 2);
    let status = h.engine.financial_plan_status(&plan).await.unwrap();
    assert_eq!(status.usage.consumed_debits.len(), 1);
    assert_eq!(
        status.usage.consumed_debits[0].amount.atoms,
        Quantity::from_u64(1_000_000_000_000)
    );
}

#[tokio::test]
async fn modeled_missing_destination_credit_never_completes_transfer_or_plan_receipt() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let plan = h
        .authorize(h.plan(&inputs), 600, None, "authorize-missing-credit")
        .await;
    h.settle(
        h.execute(&plan, 0, &inputs[0], "missing-source")
            .await
            .unwrap(),
    )
    .await;
    h.settle(
        h.execute(&plan, 1, &inputs[1], "missing-proof")
            .await
            .unwrap(),
    )
    .await;
    {
        let mut state = h.chain.lock().unwrap();
        state.mature = true;
        state.omit_credit = true;
    }
    let finalization = h
        .settle(
            h.execute(&plan, 2, &inputs[2], "missing-finalize")
                .await
                .unwrap(),
        )
        .await;
    assert_eq!(
        finalization.status,
        Status::Completed,
        "The concrete portal stage succeeds, but this does not establish recipient credit."
    );
    assert_eq!(
        h.bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap_err()
            .code,
        "BRIDGE_PROOF_MISMATCH"
    );
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(record.transit.is_some());
    assert!(record.settlement_proofs.is_empty());
    assert_ne!(record.state, fin_domain::BridgeTransferState::Completed);
    let status = h.engine.financial_plan_status(&plan).await.unwrap();
    assert_ne!(
        status.steps[2].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::Completed
    );
    assert!(
        h.execute(&plan, 2, &inputs[2], "second-financial-finalize")
            .await
            .is_err()
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 2);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn modeled_deferred_receipt_cannot_widen_quantity_or_financial_identity() {
    for altered in [0u8, 1, 2] {
        let h = Harness::new();
        let (_, _, inputs) = h.quote().await;
        let mut spec = h.plan(&inputs);
        let target = spec.steps[2]
            .completion
            .minimum_net_receipt
            .as_mut()
            .unwrap();
        match altered {
            0 => target.amount.atoms = Quantity::from_u64(1_000_000_000_001),
            1 => {
                target.resource =
                    ResourceId("ethereum:1:0x9999999999999999999999999999999999999999".into())
            }
            _ => target.amount.asset.network = "ethereum:8453".into(),
        }
        let request = FinancialPlanAuthorizationRequest {
            request_id: format!("invalid-target-review-{altered}"),
            plan: spec.clone(),
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = h.engine.preview_financial_plan(request).await;
        match review {
            Err(_) => {
                assert!(h.chain.lock().unwrap().broadcasts.is_empty());
            }
            Ok(review) => {
                let plan = h
                    .engine
                    .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                        request_id: format!("invalid-target-authorize-{altered}"),
                        plan: spec,
                        valid_for_seconds: Some(600),
                        replaces_revision_id: None,
                        expected_review_hash: Some(review.review_hash),
                    })
                    .await
                    .unwrap()
                    .plan_id;
                h.settle(
                    h.execute(
                        &plan,
                        0,
                        &inputs[0],
                        &format!("invalid-target-source-{altered}"),
                    )
                    .await
                    .unwrap(),
                )
                .await;
                h.settle(
                    h.execute(
                        &plan,
                        1,
                        &inputs[1],
                        &format!("invalid-target-proof-{altered}"),
                    )
                    .await
                    .unwrap(),
                )
                .await;
                h.chain.lock().unwrap().mature = true;
                let status = h.engine.financial_plan_status(&plan).await.unwrap();
                assert_eq!(
                    status.steps[2].state,
                    fin_domain::financial_plans::FinancialPlanStepProgressState::Blocked
                );
                assert!(
                    h.execute(
                        &plan,
                        2,
                        &inputs[2],
                        &format!("invalid-target-final-{altered}")
                    )
                    .await
                    .is_err()
                );
                assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
            }
        }
    }
}

#[tokio::test]
async fn modeled_cumulative_eth_stage_fees_block_an_unaffordable_frozen_final_before_signing() {
    let h = Harness::new();
    let (_, _, inputs) = h.quote().await;
    let plan = h
        .authorize(h.plan(&inputs), 600, None, "authorize-cumulative")
        .await;
    h.settle(h.execute(&plan, 0, &inputs[0], "cap-source").await.unwrap())
        .await;
    h.settle(h.execute(&plan, 1, &inputs[1], "cap-proof").await.unwrap())
        .await;
    let usage = h.engine.financial_plan_status(&plan).await.unwrap().usage;
    assert_eq!(usage.consumed_costs.len(), 2);
    let eth_cost = usage
        .consumed_costs
        .iter()
        .find(|c| c.amount.asset.network == "ethereum:1")
        .unwrap();
    assert_eq!(
        eth_cost.amount.atoms,
        Quantity::from_u64(30_000_000_000_000)
    );
    {
        let mut state = h.chain.lock().unwrap();
        state.mature = true;
        state.eth_base_fee = Some(35_000_000_000);
    }
    let blocked = h
        .execute(&plan, 2, &inputs[2], "cap-final")
        .await
        .unwrap_err();
    assert_eq!(blocked.code, "BRIDGE_CUMULATIVE_FEE_LIMIT");
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    let mut widened = inputs[2].clone();
    widened["bounds"]["fee_caps"][1]["maximum"] = json!("20000000000000000");
    assert!(h.execute(&plan, 2, &widened, "widen-final").await.is_err());
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
}

#[tokio::test]
async fn modeled_invalidated_proof_explicit_recovery_renews_same_plan_and_preserves_spent_fees() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let original_spec = h.plan(&inputs);
    let plan = h
        .authorize(original_spec.clone(), 600, None, "authorize-recovery")
        .await;
    let source = h
        .settle(
            h.execute(&plan, 0, &inputs[0], "recovery-source")
                .await
                .unwrap(),
        )
        .await;
    let old_proof = h
        .settle(
            h.execute(&plan, 1, &inputs[1], "recovery-old-proof")
                .await
                .unwrap(),
        )
        .await;
    let source_locator = h
        .state
        .transfer(&transfer)
        .unwrap()
        .unwrap()
        .transit
        .unwrap()
        .destination_locator
        .unwrap();
    h.chain.lock().unwrap().invalidated = true;
    let revoked = h.engine.revoke_financial_plan(&plan).await.unwrap();
    assert_eq!(
        revoked.revision.status,
        fin_domain::financial_plans::FinancialPlanAuthorizationStatus::Revoked
    );
    let response = h
        .bridge
        .read("bridge.recovery", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(response["created"], true);
    let repeated = h
        .bridge
        .read("bridge.recovery", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(repeated["created"], false);
    assert_eq!(repeated["steps"], response["steps"]);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
    let links: Vec<BridgeStepLink> = serde_json::from_value(response["steps"].clone()).unwrap();
    assert_eq!(links.len(), 2);
    let new_inputs=links.iter().map(|link|json!({"transferId":transfer,"stepId":link.step_id,"stepKind":link.kind,"bounds":h.bounds()})).collect::<Vec<_>>();
    assert!(
        h.execute(&plan, 3, &new_inputs[0], "unapproved-recovery")
            .await
            .is_err()
    );
    let exact = h
        .engine
        .execute(OperationRequest {
            request_id: "exact-cannot-steal-recovery".into(),
            operation: "bridge.transfer".into(),
            input: new_inputs[0].clone(),
            run_id: None,
        })
        .await;
    if let Ok(operation) = exact {
        assert!(operation.submission.is_none() && operation.status != Status::Completed);
    }
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 1);
    let mut revised = original_spec;
    revised.steps.pop();
    for (i, input) in new_inputs.iter().enumerate() {
        let mut step = h.plan(std::slice::from_ref(input)).steps.remove(0);
        step.step_id = format!("stage-{}", i + 3);
        step.completion.minimum_net_receipt = if i == 1 {
            Some(fin_domain::ResourceAmount {
                resource: ResourceId(format!("ethereum:1:{}", h.wallet)),
                amount: fin_bridge::EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .amount(Quantity::from_u64(1_000_000_000_000)),
            })
        } else {
            None
        };
        revised.steps.push(step);
    }
    let renewed = h
        .authorize(
            revised,
            600,
            Some(revoked.revision.revision_id),
            "authorize-recovery-revision",
        )
        .await;
    assert_eq!(renewed, plan);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 2);
    h.chain.lock().unwrap().new_game_published = true;
    let proof = h
        .settle(
            h.execute(&plan, 3, &new_inputs[0], "recovery-new-proof")
                .await
                .unwrap(),
        )
        .await;
    assert_ne!(proof.id, old_proof.id);
    assert_ne!(
        proof.submission.as_ref().unwrap().reference,
        old_proof.submission.as_ref().unwrap().reference
    );
    h.chain.lock().unwrap().mature = true;
    h.settle(
        h.execute(&plan, 4, &new_inputs[1], "recovery-new-final")
            .await
            .unwrap(),
    )
    .await;
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(
        record.steps[0].operation_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(
        record.steps[1].operation_id.as_deref(),
        Some(old_proof.id.as_str())
    );
    assert_eq!(
        record.steps[2].state,
        fin_domain::BridgeStepState::Superseded
    );
    assert_eq!(record.steps.len(), 5);
    let persisted = h
        .state
        .transfer(&transfer)
        .unwrap()
        .unwrap()
        .quote_revisions[0]
        .payload["quote"]
        .clone();
    assert_eq!(persisted["intent"]["amount"], "1000000000000");
    assert_eq!(
        source_locator.proof["withdrawal"]["withdrawal_hash"],
        response["withdrawal_hash"]
    );
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 3);
    let usage = h.engine.financial_plan_status(&plan).await.unwrap().usage;
    assert_eq!(usage.consumed_debits.len(), 1);
    let eth_cost = usage
        .consumed_costs
        .iter()
        .find(|c| c.amount.asset.network == "ethereum:1")
        .unwrap();
    assert_eq!(
        eth_cost.amount.atoms,
        Quantity::from_u64(90_000_000_000_000)
    );
}

#[tokio::test]
async fn modeled_reverted_exact_proof_charges_gas_then_recovers_with_fresh_stage_ids() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let execute = |input: Value, id: String| {
        let engine = h.engine.clone();
        async move {
            engine
                .execute(OperationRequest {
                    request_id: id,
                    operation: "bridge.transfer".into(),
                    input,
                    run_id: None,
                })
                .await
                .unwrap()
        }
    };
    let source = h
        .settle(execute(inputs[0].clone(), "exact-source".into()).await)
        .await;
    h.chain.lock().unwrap().revert_next = Some(1);
    let proof = execute(inputs[1].clone(), "exact-reverting-proof".into()).await;
    let failed = h.engine.status(&proof.id, true).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.submission.is_some());
    let effects = failed.effects.as_ref().unwrap();
    assert!(effects.effects.iter().any(|effect|matches!(effect,fin_domain::FinancialEffect::Cost{amount,..} if amount.atoms==Quantity::from_u64(30_000_000_000_000))));
    let parent = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(parent.transit.is_some());
    assert_eq!(
        parent.steps[1].state,
        fin_domain::BridgeStepState::NeedsAttention
    );
    assert_eq!(
        parent.steps[0].operation_id.as_deref(),
        Some(source.id.as_str())
    );
    let recovery = h
        .bridge
        .read("bridge.recovery", &json!({"transferId":transfer}))
        .await
        .unwrap();
    let links: Vec<BridgeStepLink> = serde_json::from_value(recovery["steps"].clone()).unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 2);
    let recovery_input = |link: &BridgeStepLink| json!({"transferId":transfer,"stepId":link.step_id,"stepKind":link.kind,"bounds":h.bounds()});
    let new_proof = h
        .settle(execute(recovery_input(&links[0]), "exact-new-proof".into()).await)
        .await;
    assert_ne!(new_proof.id, failed.id);
    h.chain.lock().unwrap().mature = true;
    h.settle(execute(recovery_input(&links[1]), "exact-new-final".into()).await)
        .await;
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 4);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&8453], 1);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 3);
    let operations = h.engine.operations().unwrap();
    let eth_gas = operations
        .iter()
        .filter_map(|o| o.effects.as_ref())
        .flat_map(|b| &b.effects)
        .filter_map(|effect| match effect {
            fin_domain::FinancialEffect::Cost { amount, .. }
                if amount.asset.network == "ethereum:1" =>
            {
                Some(amount.atoms.integer())
            }
            _ => None,
        })
        .sum::<num_bigint::BigUint>();
    assert_eq!(eth_gas, Quantity::from_u64(90_000_000_000_000).integer());
}

#[tokio::test]
async fn modeled_external_finalization_settles_after_failed_fin_candidate_without_another_write() {
    let h = Harness::new();
    let (transfer, _, inputs) = h.quote().await;
    let execute = |input: Value, id: &str| {
        let engine = h.engine.clone();
        let id = id.to_owned();
        async move {
            engine
                .execute(OperationRequest {
                    request_id: id,
                    operation: "bridge.transfer".into(),
                    input,
                    run_id: None,
                })
                .await
                .unwrap()
        }
    };
    h.settle(execute(inputs[0].clone(), "external-source").await)
        .await;
    h.settle(execute(inputs[1].clone(), "external-proof").await)
        .await;
    {
        let mut state = h.chain.lock().unwrap();
        state.mature = true;
        state.revert_next = Some(1);
    }
    let candidate = execute(inputs[2].clone(), "failed-own-final").await;
    let failed = h.engine.status(&candidate.id, true).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert!(
        h.state
            .transfer(&transfer)
            .unwrap()
            .unwrap()
            .transit
            .is_some()
    );
    h.publish_external_finalization();
    let status = h
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    assert!(record.transit.is_none());
    assert_eq!(record.settlement_proofs.len(), 1);
    assert_eq!(
        record.settlement_proofs[0].evidence["transaction_hash"],
        format!("{:#x}", B256::repeat_byte(0xab))
    );
    assert_eq!(h.approvals.0.load(Ordering::SeqCst), 3);
    assert_eq!(h.chain.lock().unwrap().broadcasts[&1], 2);
    assert_eq!(
        h.engine.status(&failed.id, true).await.unwrap().status,
        Status::Failed
    );
    assert!(
        h.bridge
            .read("bridge.recovery", &json!({"transferId":transfer}))
            .await
            .is_err()
    );
}
