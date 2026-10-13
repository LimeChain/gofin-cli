//! Base's reviewed OP Stack StandardBridge. RPC receipts must already be checked
//! for canonical inclusion/finality by the broker. These helpers bind financial
//! events to the exact messenger payload, never an unrelated same-value log.
//! Contract ABI reference: base/contracts revision
//! 5f915b1e471457fa924b8daabdc5289eca361f24 (Azul/Beryl compatible).
use crate::canonical::CanonicalRawLog;
use crate::{
    ApprovalRequirement, BridgeBounds, BridgeCapabilities, BridgeEvidenceCandidate, BridgeFee,
    BridgeIntent, BridgeProvider, BridgeQuote, BridgeQuoteBatch, BridgeQuoteRequest,
    BridgeReconciliation, BridgeStep, BridgeStepKind, BridgeTransferRef, EvmAsset, EvmCall,
    FeeKind, PreparedBridge, QuoteClass, bridge_error, ensure,
};
use alloy::{
    primitives::{Address, B256, Bytes, U256, keccak256},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use fin_domain::{Quantity, Result};
use fin_evm::{EvmRuntime, address, hash, hex_bytes, quantity, rpc_bytes, rpc_u64, rpc_u256, u256};
use num_traits::Zero;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub const PROVIDER_ID: &str = "base_canonical";
pub const BASE_CHAIN_ID: u64 = 8453;
pub const L1_STANDARD_BRIDGE: &str = "0x3154cf16ccdb4c6d922629664174b904d80f2c35";
pub const L2_STANDARD_BRIDGE: &str = "0x4200000000000000000000000000000000000010";
pub const L1_MESSENGER: &str = "0x866e82a600a1414e583f7f13623f1ac5d58b0afa";
pub const L2_MESSENGER: &str = "0x4200000000000000000000000000000000000007";
pub const PORTAL: &str = "0x49048044d57e1c92a77f79988d21fa8faf74e97e";
pub const MESSAGE_PASSER: &str = "0x4200000000000000000000000000000000000016";
pub const GAME_FACTORY: &str = "0x43edb88c4b80fdd2adff2412a7bebf9df42cb40e";
pub const ANCHOR_REGISTRY: &str = "0x909f6cf47ed12f010a796527f562bfc26c7f4e72";
const MIN_GAS: u32 = 200_000;

sol! {
    interface StandardBridge {
        function bridgeETHTo(address to, uint32 minGasLimit, bytes extraData) external payable;
        function bridgeERC20To(address localToken, address remoteToken, address to, uint256 amount, uint32 minGasLimit, bytes extraData) external;
        function finalizeBridgeETH(address from, address to, uint256 amount, bytes extraData) external payable;
        function finalizeBridgeERC20(address localToken, address remoteToken, address from, address to, uint256 amount, bytes extraData) external;
        event ETHBridgeInitiated(address indexed from, address indexed to, uint256 amount, bytes extraData);
        event ETHBridgeFinalized(address indexed from, address indexed to, uint256 amount, bytes extraData);
        event ERC20BridgeInitiated(address indexed localToken, address indexed remoteToken, address indexed from, address to, uint256 amount, bytes extraData);
        event ERC20BridgeFinalized(address indexed localToken, address indexed remoteToken, address indexed from, address to, uint256 amount, bytes extraData);
    }
    interface Token {
        event Transfer(address indexed from, address indexed to, uint256 value);
        function remoteToken() external view returns (address);
        function bridge() external view returns (address);
        function decimals() external view returns (uint8);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function supportsInterface(bytes4 interfaceId) external view returns (bool);
    }
    interface Messenger {
        function relayMessage(uint256 nonce, address sender, address target, uint256 value, uint256 minGasLimit, bytes message) external payable;
        event RelayedMessage(bytes32 indexed msgHash);
        event FailedRelayedMessage(bytes32 indexed msgHash);
    }
    struct WithdrawalTransaction {
        uint256 nonce; address sender; address target; uint256 value; uint256 gasLimit; bytes data;
    }
    struct OutputRootProof {
        bytes32 version; bytes32 stateRoot; bytes32 messagePasserStorageRoot; bytes32 latestBlockhash;
    }
    interface OptimismPortal {
        function proveWithdrawalTransaction(WithdrawalTransaction tx_, uint256 disputeGameIndex, OutputRootProof outputRootProof, bytes[] withdrawalProof) external;
        function finalizeWithdrawalTransactionExternalProof(WithdrawalTransaction tx_, address proofSubmitter) external;
        function checkWithdrawal(bytes32 withdrawalHash, address proofSubmitter) external view;
        function finalizedWithdrawals(bytes32 withdrawalHash) external view returns (bool);
        function provenWithdrawals(bytes32 withdrawalHash, address submitter) external view returns (address disputeGameProxy, uint64 timestamp);
        function proofMaturityDelaySeconds() external view returns (uint256);
        function anchorStateRegistry() external view returns (address);
        function disputeGameFactory() external view returns (address);
        event TransactionDeposited(address indexed from, address indexed to, uint256 indexed version, bytes opaqueData);
        event WithdrawalProven(bytes32 indexed withdrawalHash, address indexed from, address indexed to);
        event WithdrawalProvenExtension1(bytes32 indexed withdrawalHash, address indexed proofSubmitter);
        event WithdrawalFinalized(bytes32 indexed withdrawalHash, bool success);
    }
    interface MessagePasser {
        event MessagePassed(uint256 indexed nonce, address indexed sender, address indexed target, uint256 value, uint256 gasLimit, bytes data, bytes32 withdrawalHash);
    }
    interface GameFactory {
        function gameCount() external view returns (uint256);
        function gameAtIndex(uint256 index) external view returns (uint32 gameType, uint64 timestamp, address proxy);
    }
    interface Game {
        function rootClaim() external view returns (bytes32);
        function l2SequenceNumber() external view returns (uint256);
        function status() external view returns (uint8);
        function expectedResolution() external view returns (uint64);
    }
    interface AnchorRegistry {
        function isGameProper(address game) external view returns (bool);
        function isGameRespected(address game) external view returns (bool);
        function isGameClaimValid(address game) external view returns (bool);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseCanonicalConfig {
    pub l1_standard_bridge: String,
    pub l2_standard_bridge: String,
    pub l1_messenger: String,
    pub l2_messenger: String,
    pub portal: String,
    pub message_passer: String,
    pub game_factory: String,
    pub anchor_registry: String,
}
impl BaseCanonicalConfig {
    pub fn reviewed() -> Self {
        Self {
            l1_standard_bridge: L1_STANDARD_BRIDGE.into(),
            l2_standard_bridge: L2_STANDARD_BRIDGE.into(),
            l1_messenger: L1_MESSENGER.into(),
            l2_messenger: L2_MESSENGER.into(),
            portal: PORTAL.into(),
            message_passer: MESSAGE_PASSER.into(),
            game_factory: GAME_FACTORY.into(),
            anchor_registry: ANCHOR_REGISTRY.into(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure(
            self == &Self::reviewed(),
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Base canonical contracts must equal the reviewed deployments.",
        )
    }
}

#[derive(Clone)]
pub struct BaseCanonicalProvider {
    pub ethereum: Arc<EvmRuntime>,
    pub base: Arc<EvmRuntime>,
    config: BaseCanonicalConfig,
}
impl BaseCanonicalProvider {
    pub fn new(
        ethereum: Arc<EvmRuntime>,
        base: Arc<EvmRuntime>,
        config: BaseCanonicalConfig,
    ) -> Result<Self> {
        config.validate()?;
        ensure(
            ethereum.chain_id == 1
                && base.chain_id == BASE_CHAIN_ID
                && !ethereum.can_sign()
                && !base.can_sign(),
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Base canonical provider requires Ethereum and Base read-only runtimes.",
        )?;
        Ok(Self {
            ethereum,
            base,
            config,
        })
    }
    fn runtime(&self, chain: u64) -> &Arc<EvmRuntime> {
        if chain == 1 {
            &self.ethereum
        } else {
            &self.base
        }
    }
    /// Both quantities and reviewed bridge binding are pinned to the same block.
    /// Token lists are candidates; only this exact deployed pair enables a quote.
    pub async fn verify_pair(&self, l1_token: &str, l2_token: &str) -> Result<u8> {
        self.base.assert_chain().await?;
        self.ethereum.assert_chain().await?;
        let block = rpc_u64(&self.base.rpc.request("eth_blockNumber", json!([])).await?)?;
        let l1_block = rpc_u64(
            &self
                .ethereum
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let l1 = address(l1_token)?;
        let l2 = address(l2_token)?;
        self.ethereum.code_at(l1, Some(l1_block)).await?;
        self.base.code_at(l2, Some(block)).await?;
        let remote = self
            .base
            .call(l2, &Token::remoteTokenCall {}, Some(block))
            .await?;
        let bridge = self
            .base
            .call(l2, &Token::bridgeCall {}, Some(block))
            .await?;
        let l2_decimals = self
            .base
            .call(l2, &Token::decimalsCall {}, Some(block))
            .await?;
        let l1_decimals = self
            .ethereum
            .call(l1, &Token::decimalsCall {}, Some(l1_block))
            .await?;
        // IOptimismMintableERC20 ERC165 identity (remoteToken,bridge,mint,burn).
        let mintable = self
            .base
            .call(
                l2,
                &Token::supportsInterfaceCall {
                    interfaceId: mintable_interface_id().into(),
                },
                Some(block),
            )
            .await?;
        ensure(
            remote == l1
                && bridge == address(L2_STANDARD_BRIDGE)?
                && mintable
                && l1_decimals == l2_decimals,
            "BRIDGE_TOKEN_PAIR_UNSUPPORTED",
            "quote",
            "Deployed Base token does not verify the exact StandardBridge mintable pair and matching decimals.",
        )?;
        Ok(l1_decimals)
    }
    fn validate_intent(&self, intent: &BridgeIntent) -> Result<()> {
        validate_intent(intent)
    }
    /// Canonical stage feasibility includes protocol assertions, not a calendar constant.
    pub async fn withdrawal_readiness(
        &self,
        withdrawal: &BaseWithdrawal,
        submitter: &str,
    ) -> Result<BaseWithdrawalReadiness> {
        let hash_ = withdrawal.hash()?;
        let submitter = address(submitter)?;
        self.verify_proof_contracts().await?;
        let portal = address(PORTAL)?;
        let block = rpc_u64(
            &self
                .ethereum
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let finalized = self
            .ethereum
            .call(
                portal,
                &OptimismPortal::finalizedWithdrawalsCall {
                    withdrawalHash: hash_,
                },
                Some(block),
            )
            .await?;
        let proven = self
            .ethereum
            .call(
                portal,
                &OptimismPortal::provenWithdrawalsCall {
                    withdrawalHash: hash_,
                    submitter,
                },
                Some(block),
            )
            .await?;
        let maturity = self
            .ethereum
            .call(
                portal,
                &OptimismPortal::proofMaturityDelaySecondsCall {},
                Some(block),
            )
            .await?;
        let (claim_valid, proof_invalidated, expected_resolution_at, ready) =
            if proven.timestamp == 0 || finalized {
                (false, false, None, false)
            } else {
                let game = proven.disputeGameProxy;
                let proper = self
                    .ethereum
                    .call(
                        address(ANCHOR_REGISTRY)?,
                        &AnchorRegistry::isGameProperCall { game },
                        Some(block),
                    )
                    .await?;
                let respected = self
                    .ethereum
                    .call(
                        address(ANCHOR_REGISTRY)?,
                        &AnchorRegistry::isGameRespectedCall { game },
                        Some(block),
                    )
                    .await?;
                let status = self
                    .ethereum
                    .call(game, &Game::statusCall {}, Some(block))
                    .await?;
                ensure(
                    status <= 2,
                    "BRIDGE_PROOF_SYSTEM_UNSUPPORTED",
                    "read",
                    "Checkpoint game status is outside the reviewed enum.",
                )?;
                let valid = self
                    .ethereum
                    .call(
                        address(ANCHOR_REGISTRY)?,
                        &AnchorRegistry::isGameClaimValidCall { game },
                        Some(block),
                    )
                    .await?;
                let invalidated = !proper || !respected || status == 1;
                // Azul exposes a proof-count-dependent resolution timestamp. It is a
                // useful estimate only; actual checkWithdrawal remains authoritative.
                let estimate = self
                    .ethereum
                    .call(game, &Game::expectedResolutionCall {}, Some(block))
                    .await
                    .ok();
                let ready = if valid && !invalidated {
                    match self
                        .ethereum
                        .call(
                            portal,
                            &OptimismPortal::checkWithdrawalCall {
                                withdrawalHash: hash_,
                                proofSubmitter: submitter,
                            },
                            Some(block),
                        )
                        .await
                    {
                        Ok(_) => true,
                        Err(error) if error.code == "RPC_REQUEST_FAILED" => false,
                        Err(error) => return Err(error),
                    }
                } else {
                    false
                };
                (valid, invalidated, estimate, ready)
            };
        Ok(BaseWithdrawalReadiness {
            observed_block: block,
            finalized,
            proven_at: proven.timestamp,
            proof_maturity_seconds: quantity(maturity),
            game: format!("{:#x}", proven.disputeGameProxy),
            claim_valid,
            proof_invalidated,
            expected_resolution_at,
            ready,
        })
    }
    async fn verify_proof_contracts(&self) -> Result<()> {
        self.ethereum.assert_chain().await?;
        let portal = address(PORTAL)?;
        self.ethereum.code(portal).await?;
        let registry = self
            .ethereum
            .call(portal, &OptimismPortal::anchorStateRegistryCall {}, None)
            .await?;
        let factory = self
            .ethereum
            .call(portal, &OptimismPortal::disputeGameFactoryCall {}, None)
            .await?;
        ensure(
            registry == address(ANCHOR_REGISTRY)? && factory == address(GAME_FACTORY)?,
            "BRIDGE_PROOF_SYSTEM_UNSUPPORTED",
            "read",
            "Base portal proof dependencies differ from the reviewed deployments.",
        )
    }
    /// Select an already-published proper/respected checkpoint and ask the configured
    /// Base RPC for its storage proof. The portal's eth_call independently checks
    /// every proof node before this call can become a financial candidate.
    pub async fn withdrawal_witness(
        &self,
        withdrawal: &BaseWithdrawal,
        source_block: u64,
        submitter: &str,
    ) -> Result<BaseWithdrawalWitness> {
        self.verify_proof_contracts().await?;
        self.base.assert_chain().await?;
        let l1_block = rpc_u64(
            &self
                .ethereum
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let count = self
            .ethereum
            .call(
                address(GAME_FACTORY)?,
                &GameFactory::gameCountCall {},
                Some(l1_block),
            )
            .await?;
        let count: u64 = count
            .try_into()
            .map_err(|_| proof_error("Game count exceeds supported indexing."))?;
        // Bounded look-back: missing checkpoint is actionable pending, never a
        // manufactured output or a provider-supplied game contract.
        for index in (count.saturating_sub(256)..count).rev() {
            let record = self
                .ethereum
                .call(
                    address(GAME_FACTORY)?,
                    &GameFactory::gameAtIndexCall {
                        index: U256::from(index),
                    },
                    Some(l1_block),
                )
                .await?;
            let game = record.proxy;
            if !self
                .ethereum
                .call(
                    address(ANCHOR_REGISTRY)?,
                    &AnchorRegistry::isGameProperCall { game },
                    Some(l1_block),
                )
                .await?
                || !self
                    .ethereum
                    .call(
                        address(ANCHOR_REGISTRY)?,
                        &AnchorRegistry::isGameRespectedCall { game },
                        Some(l1_block),
                    )
                    .await?
            {
                continue;
            }
            let checkpoint = self
                .ethereum
                .call(game, &Game::l2SequenceNumberCall {}, Some(l1_block))
                .await?;
            let checkpoint: u64 = checkpoint
                .try_into()
                .map_err(|_| proof_error("Checkpoint exceeds uint64 block number."))?;
            if checkpoint < source_block {
                continue;
            }
            if self
                .ethereum
                .call(game, &Game::statusCall {}, Some(l1_block))
                .await?
                == 1
            {
                continue;
            }
            let expected_root = self
                .ethereum
                .call(game, &Game::rootClaimCall {}, Some(l1_block))
                .await?;
            let block = self
                .base
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("{checkpoint:#x}"), false]),
                )
                .await?;
            if block.is_null() {
                continue;
            }
            ensure(
                rpc_u64(&block["number"])? == checkpoint,
                "BRIDGE_PROOF_MISMATCH",
                "read",
                "Checkpoint block number differs from its RPC request.",
            )?;
            let key = withdrawal_storage_key(withdrawal.hash()?);
            let proof = self
                .base
                .rpc
                .request(
                    "eth_getProof",
                    json!([
                        MESSAGE_PASSER,
                        [format!("{key:#x}")],
                        format!("{checkpoint:#x}")
                    ]),
                )
                .await?;
            let witness = BaseWithdrawalWitness::from_rpc(
                index,
                checkpoint,
                expected_root,
                &block,
                &proof,
                key,
            )?;
            let call = encode_prove_call(withdrawal, &witness)?;
            // Contract verifies MPT against the checkpoint root and game state.
            self.ethereum.rpc.request("eth_call", json!([{"from":submitter,"to":PORTAL,"data":call.data},format!("{l1_block:#x}")])).await?;
            return Ok(witness);
        }
        Err(bridge_error(
            "BRIDGE_WITHDRAWAL_PROOF_PENDING",
            "read",
            "No usable reviewed checkpoint proof currently covers this withdrawal.",
            "Retry status later or use archive-capable Base RPC; do not initiate another withdrawal.",
        ))
    }
}

#[async_trait]
impl BridgeProvider for BaseCanonicalProvider {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: PROVIDER_ID.into(),
            chains: BTreeSet::from([1, BASE_CHAIN_ID]),
            native_eth: true,
            erc20: true,
            protocol_versions: BTreeSet::from(["base_standard_bridge_v1".into()]),
        }
    }
    async fn discover_routes(
        &self,
        request: &crate::BridgeRouteRequest,
    ) -> Result<crate::BridgeRouteDiscovery> {
        use crate::{
            BridgeAssetInput, BridgeRouteCandidate, BridgeRouteDiscovery, RouteAvailability,
        };
        let mut result = BridgeRouteDiscovery { provider:PROVIDER_ID.into(),availability:RouteAvailability::Unavailable,candidates:Vec::new(),observed_at:Utc::now().to_rfc3339(),evidence_source:"Reviewed StandardBridge deployments and exact Base mintable-token RPC metadata".into(),complete:false,limitations:vec!["Canonical candidates require executable amount-specific quoting and current deployed-pair verification.".into()],error:None };
        if !((request.source.chain_id == 1 && request.destination_chain_id == BASE_CHAIN_ID)
            || (request.source.chain_id == BASE_CHAIN_ID && request.destination_chain_id == 1))
        {
            result
                .limitations
                .push("Only Ethereum ↔ Base canonical routes are reviewed.".into());
            return Ok(result);
        }
        if request.source.token.is_none() {
            if request.source.decimals == 18
                && request.destination_asset.as_ref().is_none_or(|a| {
                    a.chain_id == request.destination_chain_id
                        && a.token.is_none()
                        && a.decimals == 18
                })
            {
                result.candidates.push(BridgeRouteCandidate {
                    destination: BridgeAssetInput {
                        chain_id: request.destination_chain_id,
                        token: None,
                    },
                    reported_decimals: Some(18),
                });
                result.availability = RouteAvailability::Candidate;
                result.complete = true;
            }
            return Ok(result);
        }
        let source = request.source.token.as_deref().expect("native handled");
        let mut targets = Vec::new();
        if let Some(exact) = &request.destination_asset {
            if exact.chain_id != request.destination_chain_id || exact.token.is_none() {
                return Ok(result);
            }
            targets.push(exact.token.clone().expect("checked"));
        } else if request.source.chain_id == BASE_CHAIN_ID {
            self.base.assert_chain().await?;
            let block = rpc_u64(&self.base.rpc.request("eth_blockNumber", json!([])).await?)?;
            let remote = self
                .base
                .call(address(source)?, &Token::remoteTokenCall {}, Some(block))
                .await?;
            targets.push(format!("{remote:#x}"));
        } else {
            // Recent factory events are candidates, not an exhaustive token list.
            // Exact destination lookup also works for older/custom mintable tokens.
            self.base.assert_chain().await?;
            let block = rpc_u64(&self.base.rpc.request("eth_blockNumber", json!([])).await?)?;
            let logs=self.base.rpc.request("eth_getLogs",json!([{"address":"0xf10122d428b4bc8a9d050d06a2037259b4c4b83b","fromBlock":format!("{:#x}",block.saturating_sub(10_000)),"toBlock":format!("{block:#x}"),"topics":[format!("{:#x}",keccak256("OptimismMintableERC20Created(address,address,address)")),null,format!("{:#x}",address(source)?.into_word())]}])).await?;
            let logs = logs
                .as_array()
                .ok_or_else(|| proof_error("Factory candidate logs are not an array."))?;
            ensure(
                logs.len() <= 128,
                "BRIDGE_DISCOVERY_UNAVAILABLE",
                "read",
                "Factory returned too many exact-token candidates.",
            )?;
            for log in logs {
                if let Some(local) = log["topics"][1].as_str() {
                    let word = hash(local)?;
                    ensure(
                        word.as_slice()[..12].iter().all(|b| *b == 0),
                        "BRIDGE_PROOF_INVALID",
                        "read",
                        "Factory token candidate has noncanonical address padding.",
                    )?;
                    targets.push(format!(
                        "{:#x}",
                        Address::from_slice(&word.as_slice()[12..])
                    ));
                }
            }
            result.limitations.push("Factory scan covers only the latest 10,000 canonical Base blocks; older/custom representations require an exact destination address.".into());
        }
        targets.sort();
        targets.dedup();
        for target in targets {
            let pair = if request.source.chain_id == 1 {
                self.verify_pair(source, &target).await
            } else {
                self.verify_pair(&target, source).await
            };
            match pair {
                Ok(decimals)
                    if decimals == request.source.decimals
                        && request
                            .destination_asset
                            .as_ref()
                            .is_none_or(|a| a.decimals == decimals) =>
                {
                    result.candidates.push(BridgeRouteCandidate {
                        destination: BridgeAssetInput {
                            chain_id: request.destination_chain_id,
                            token: Some(target),
                        },
                        reported_decimals: Some(decimals),
                    })
                }
                Ok(_) => {}
                Err(error) => {
                    result.error = Some(error);
                }
            }
        }
        result.availability = if !result.candidates.is_empty() {
            RouteAvailability::Candidate
        } else if request.destination_asset.is_none() && request.source.chain_id == 1 {
            RouteAvailability::Unknown
        } else {
            RouteAvailability::Unavailable
        };
        Ok(result)
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let intent = &request.intent;
        self.validate_intent(intent)?;
        if let Some(source_token) = &intent.source.token {
            let destination_token = intent.destination_assets[0]
                .token
                .as_deref()
                .expect("validated pair");
            let (l1, l2) = if intent.source.chain_id == 1 {
                (source_token.as_str(), destination_token)
            } else {
                (destination_token, source_token.as_str())
            };
            let decimals = self.verify_pair(l1, l2).await?;
            ensure(
                decimals == intent.source.decimals
                    && decimals == intent.destination_assets[0].decimals,
                "BRIDGE_TOKEN_PAIR_UNSUPPORTED",
                "quote",
                "Exact token decimals differ from deployed canonical pair.",
            )?;
        }
        let runtime = self.runtime(intent.source.chain_id);
        runtime.assert_chain().await?;
        let call = initiation_call(intent)?;
        let sender = address(&intent.source_address)?;
        let spender = address(&call.to)?;
        runtime.code(spender).await?;
        let mut approval = None;
        let mut steps = Vec::new();
        let mut gas_fee = U256::ZERO;
        if let Some(token) = &intent.source.token {
            let allowance = runtime
                .call(
                    address(token)?,
                    &Token::allowanceCall {
                        owner: sender,
                        spender,
                    },
                    None,
                )
                .await?;
            if allowance < u256(&intent.amount)? {
                approval = Some(ApprovalRequirement {
                    token: token.clone(),
                    spender: call.to.clone(),
                    amount: intent.amount.clone(),
                });
                if allowance != U256::ZERO {
                    let reset = EvmCall {
                        chain_id: intent.source.chain_id,
                        to: token.clone(),
                        value: Quantity::from_u64(0),
                        data: hex_bytes(
                            &Token::approveCall {
                                spender,
                                amount: U256::ZERO,
                            }
                            .abi_encode(),
                        ),
                    };
                    steps.push(step(
                        steps.len() as u8,
                        BridgeStepKind::ResetAllowance,
                        reset,
                        intent,
                        Vec::new(),
                    ));
                }
                let approve = EvmCall {
                    chain_id: intent.source.chain_id,
                    to: token.clone(),
                    value: Quantity::from_u64(0),
                    data: hex_bytes(
                        &Token::approveCall {
                            spender,
                            amount: u256(&intent.amount)?,
                        }
                        .abi_encode(),
                    ),
                };
                steps.push(step(
                    steps.len() as u8,
                    BridgeStepKind::Approval,
                    approve,
                    intent,
                    Vec::new(),
                ));
            }
        }
        steps.push(step(
            steps.len() as u8,
            BridgeStepKind::Deposit,
            call,
            intent,
            vec![intent.source.amount(intent.amount.clone())],
        ));
        for planned in &steps {
            let call = planned.evm_call()?;
            let estimate = runtime
                .prepare(
                    sender,
                    address(&call.to)?,
                    u256(&call.value)?,
                    rpc_bytes(&json!(call.data))?,
                    U256::MAX,
                )
                .await;
            let fee = match estimate {
                Ok(prepared) => prepared.transaction.maximum_cost()?,
                // ERC20 initiation cannot execute until its finite allowance step
                // settles. This conservative quote estimate grants no authority;
                // dispatch still requires a successful fresh exact simulation.
                Err(_) if approval.is_some() && !planned.dependencies.is_empty() => {
                    let (max_fee, priority_fee) = runtime.fees().await?;
                    let mut transaction = fin_evm::FrozenTransaction {
                        chain_id: call.chain_id,
                        sender: intent.source_address.clone(),
                        to: call.to.clone(),
                        value: call.value.clone(),
                        calldata: call.data.clone(),
                        nonce: 0,
                        gas: 500_000,
                        max_fee_per_gas: quantity(max_fee),
                        max_priority_fee_per_gas: quantity(priority_fee),
                        rollup_fee: None,
                    };
                    transaction.rollup_fee = runtime.rollup_fee_bound(&transaction).await?;
                    transaction.maximum_cost()?
                }
                Err(error) => return Err(error),
            };
            gas_fee = gas_fee
                .checked_add(fee)
                .ok_or_else(|| proof_error("Quoted fee sum overflows uint256."))?;
        }
        let mut fees = vec![BridgeFee {
            amount: EvmAsset {
                chain_id: intent.source.chain_id,
                token: None,
                decimals: 18,
            }
            .amount(quantity(gas_fee)),
            kind: FeeKind::Gas,
            payer: intent.source_address.clone(),
        }];
        if intent.source.chain_id == BASE_CHAIN_ID {
            let (max_fee, _) = self.ethereum.fees().await?;
            let cost = max_fee
                .checked_mul(U256::from(2_500_000))
                .ok_or_else(|| proof_error("Withdrawal stage fee budget overflow."))?;
            fees.push(BridgeFee {
                amount: EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .amount(quantity(cost)),
                kind: FeeKind::Gas,
                payer: intent.source_address.clone(),
            });
            for kind in [BridgeStepKind::Prove, BridgeStepKind::Finalize] {
                steps.push(step(
                    steps.len() as u8,
                    kind,
                    EvmCall {
                        chain_id: 1,
                        to: PORTAL.into(),
                        value: Quantity::from_u64(0),
                        data: "0x".into(),
                    },
                    intent,
                    Vec::new(),
                ));
            }
        }
        let now = Utc::now();
        let until = now + Duration::seconds(90);
        let amount = intent.source.amount(intent.amount.clone());
        let destination = intent.destination_assets[0].amount(intent.amount.clone());
        let identity = serde_json::to_vec(&(intent, &steps, now.to_rfc3339()))
            .map_err(|_| proof_error("Quote encoding failed."))?;
        let quote = BridgeQuote {
            cctp: None,
            quote_id: format!("base-canonical:{:#x}", keccak256(identity)),
            provider: PROVIDER_ID.into(),
            route_id: route_id(intent)?,
            intent: intent.clone(),
            gross_debit: amount,
            expected_receipt: destination.clone(),
            minimum_receipt: destination,
            fees,
            approval,
            protocol_steps: steps,
            estimated_arrival_seconds: if intent.source.chain_id == 1 {
                Some(600)
            } else {
                None
            },
            observed_at: now.to_rfc3339(),
            valid_until: until.to_rfc3339(),
            class: QuoteClass::Executable,
            refund_paths: Vec::new(),
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
        };
        validate_bounds(&quote, &request.bounds)?;
        Ok(BridgeQuoteBatch {
            quotes: vec![quote],
            failures: Vec::new(),
        })
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge> {
        let expiry = chrono::DateTime::parse_from_rfc3339(&quote.valid_until)
            .map_err(|_| proof_error("Malformed canonical quote expiry."))?;
        ensure(
            expiry > Utc::now(),
            "BRIDGE_QUOTE_EXPIRED",
            "prepare",
            "Base canonical quote expired; request a fresh quote.",
        )?;
        self.config.validate()?;
        let fixed_scope_digest = scope_digest(quote, bounds, wallet)?;
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: None,
            fixed_scope_digest,
        })
    }
    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        let Some(locator) = &transfer.destination_locator else {
            return Ok(BridgeReconciliation {
                status: "awaiting_source_proof".into(),
                failure: None,
                refund_fail_reason: None,
                candidates: Vec::new(),
            });
        };
        ensure(
            locator.scheme == "base_deposit_v1"
                && transfer.source_chain_id == 1
                && transfer.evm_destination_chain_id()? == BASE_CHAIN_ID
                && locator.destination_chain_id == Some(BASE_CHAIN_ID)
                && !locator.source_evidence_id.is_empty()
                && hash(&locator.transaction_hash).is_ok(),
            "BRIDGE_LOCATOR_INVALID",
            "reconcile",
            "Saved Base deposit locator differs from its source-derived identity.",
        )?;
        Ok(BridgeReconciliation {
            status: "destination_deposit_candidate_available".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: vec![BridgeEvidenceCandidate {
                order_id: None,
                kind: crate::EvidenceKind::DestinationFill,
                chain_id: BASE_CHAIN_ID,
                transaction_hash: locator.transaction_hash.clone(),
                correlation: Some(locator.scheme.clone()),
            }],
        })
    }
}
/// Validate the saved finite topology and deterministic financial scope without
/// treating quote expiry as authority. This helper grants no dispatch permission;
/// the broker may use it only for stages of an independently proven withdrawal.
pub fn scope_digest(quote: &BridgeQuote, bounds: &BridgeBounds, wallet: &str) -> Result<String> {
    validate_intent(&quote.intent)?;
    ensure(
        quote.provider == PROVIDER_ID
            && quote.class == QuoteClass::Executable
            && quote.route_id == route_id(&quote.intent)?
            && quote.refund_paths.is_empty()
            && quote.order_solver.is_none()
            && quote.protocol_order_id.is_none()
            && quote.provider_request_id.is_none(),
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Base canonical quote contains unreviewed provider or route data.",
    )?;
    ensure(
        address(wallet)? == address(&quote.intent.source_address)?,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Base canonical quote signer differs from the selected wallet.",
    )?;
    validate_bounds(quote, bounds)?;
    let intent = &quote.intent;
    let expected = initiation_call(intent)?;
    let deferred = if intent.source.chain_id == BASE_CHAIN_ID {
        2
    } else {
        0
    };
    ensure(
        quote.protocol_steps.len() > deferred && quote.protocol_steps.len() <= 3 + deferred,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical quote has an invalid finite step count.",
    )?;
    let source_len = quote.protocol_steps.len() - deferred;
    let deposit = &quote.protocol_steps[source_len - 1];
    ensure(
        deposit.kind == BridgeStepKind::Deposit
            && deposit.evm_call()? == &expected
            && deposit.maximum_debit == vec![quote.gross_debit.clone()],
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical initiation differs from its exact reviewed bridge call.",
    )?;
    for (index, item) in quote.protocol_steps.iter().enumerate() {
        let dependencies = if index == 0 {
            Vec::new()
        } else {
            vec![(index - 1) as u8]
        };
        ensure(
            item.ordinal as usize == index
                && item.dependencies == dependencies
                && address(&item.expected_sender)? == address(wallet)?,
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Canonical step identities or dependencies changed.",
        )?;
        if index >= source_len {
            let kind = if index == source_len {
                BridgeStepKind::Prove
            } else {
                BridgeStepKind::Finalize
            };
            ensure(
                item.kind == kind
                    && item.evm_call()?
                        == &EvmCall {
                            chain_id: 1,
                            to: PORTAL.into(),
                            value: Quantity::from_u64(0),
                            data: "0x".into(),
                        }
                    && item.maximum_debit.is_empty(),
                "BRIDGE_QUOTE_INVALID",
                "prepare",
                "Deferred canonical stage differs from its reviewed portal and immutable-message scope.",
            )?;
        } else {
            ensure(
                item.evm_call()?.chain_id == intent.source.chain_id,
                "BRIDGE_QUOTE_INVALID",
                "prepare",
                "Canonical source step changed network.",
            )?;
            if index + 1 < source_len {
                let token = intent
                    .source
                    .token
                    .as_deref()
                    .ok_or_else(|| proof_error("Native ETH route cannot approve tokens."))?;
                let allowed_kind = if source_len == 3 && index == 0 {
                    BridgeStepKind::ResetAllowance
                } else {
                    BridgeStepKind::Approval
                };
                let amount = if allowed_kind == BridgeStepKind::ResetAllowance {
                    U256::ZERO
                } else {
                    u256(&intent.amount)?
                };
                ensure(
                    item.kind == allowed_kind
                        && item.evm_call()?.to == token
                        && item.evm_call()?.value == Quantity::from_u64(0)
                        && item.maximum_debit.is_empty()
                        && item.evm_call()?.data
                            == hex_bytes(
                                &Token::approveCall {
                                    spender: address(&expected.to)?,
                                    amount,
                                }
                                .abi_encode(),
                            ),
                    "BRIDGE_QUOTE_INVALID",
                    "prepare",
                    "Canonical allowance differs from its finite reviewed token, spender or amount.",
                )?;
            }
        }
    }
    let expected_approval = if source_len > 1 {
        Some(ApprovalRequirement {
            token: intent.source.token.clone().expect("validated approvals"),
            spender: expected.to.clone(),
            amount: intent.amount.clone(),
        })
    } else {
        None
    };
    ensure(
        quote.approval == expected_approval,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical approval metadata differs from its reviewed steps.",
    )?;
    let mut normalized_intent = intent.clone();
    normalized_intent.amount = Quantity::from_u64(0);
    let mut normalized_bounds = bounds.clone();
    normalized_bounds
        .input
        .get_or_insert(crate::AmountInterval {
            minimum: intent.amount.clone(),
            maximum: intent.amount.clone(),
        });
    let identity = serde_json::to_vec(&(
        PROVIDER_ID,
        &quote.route_id,
        normalized_intent,
        normalized_bounds,
        &BaseCanonicalConfig::reviewed(),
        wallet.to_ascii_lowercase(),
    ))
    .map_err(|_| proof_error("Scope encoding failed."))?;
    Ok(format!("{:#x}", keccak256(identity)))
}

fn validate_intent(intent: &BridgeIntent) -> Result<()> {
    ensure(
        (intent.source.chain_id == 1 && intent.evm_destination_chain_id()? == BASE_CHAIN_ID)
            || (intent.source.chain_id == BASE_CHAIN_ID && intent.evm_destination_chain_id()? == 1),
        "BRIDGE_ROUTE_UNAVAILABLE",
        "quote",
        "Base canonical bridging supports Ethereum to Base and Base to Ethereum only.",
    )?;
    ensure(
        intent.destination_assets.len() == 1
            && intent.destination_assets[0].chain_id == intent.evm_destination_chain_id()?
            && intent.source.token.is_none() == intent.destination_assets[0].token.is_none()
            && intent.source.decimals == intent.destination_assets[0].decimals
            && !u256(&intent.amount)?.is_zero(),
        "BRIDGE_ASSET_UNSUPPORTED",
        "quote",
        "Canonical route requires one exact matching native or StandardBridge token representation and a positive amount.",
    )?;
    address(&intent.source_address)?;
    address(&intent.recipient)?;
    ensure(
        address(&intent.refund_to)? == address(&intent.source_address)?,
        "BRIDGE_REFUND_UNSUPPORTED",
        "quote",
        "Canonical bridge has no refund path; refund target must remain the source wallet.",
    )?;
    if let Some(token) = &intent.source.token {
        address(token)?;
        address(
            intent.destination_assets[0]
                .token
                .as_deref()
                .expect("validated token"),
        )?;
    } else {
        ensure(
            intent.source.decimals == 18,
            "BRIDGE_ASSET_UNSUPPORTED",
            "quote",
            "Native ETH uses exactly 18 decimals.",
        )?;
    }
    Ok(())
}
fn route_id(intent: &BridgeIntent) -> Result<String> {
    Ok(format!(
        "base-standard:{}:{}:{}:{}",
        intent.source.chain_id,
        intent.source.token.as_deref().unwrap_or("native"),
        intent.evm_destination_chain_id()?,
        intent.destination_assets[0]
            .token
            .as_deref()
            .unwrap_or("native")
    ))
}
fn step(
    ordinal: u8,
    kind: BridgeStepKind,
    call: EvmCall,
    intent: &BridgeIntent,
    debit: Vec<fin_domain::Amount>,
) -> BridgeStep {
    BridgeStep {
        ordinal,
        kind,
        call: Some(call),
        solana_mint: None,
        expected_sender: intent.source_address.clone(),
        maximum_debit: debit,
        dependencies: if ordinal == 0 {
            Vec::new()
        } else {
            vec![ordinal - 1]
        },
    }
}
pub fn initiation_call(intent: &BridgeIntent) -> Result<EvmCall> {
    validate_intent(intent)?;
    let to = if intent.source.chain_id == 1 {
        L1_STANDARD_BRIDGE
    } else {
        L2_STANDARD_BRIDGE
    };
    let data = if let Some(token) = &intent.source.token {
        StandardBridge::bridgeERC20ToCall {
            localToken: address(token)?,
            remoteToken: address(
                intent.destination_assets[0]
                    .token
                    .as_deref()
                    .expect("validated"),
            )?,
            to: address(&intent.recipient)?,
            amount: u256(&intent.amount)?,
            minGasLimit: MIN_GAS,
            extraData: Bytes::new(),
        }
        .abi_encode()
    } else {
        StandardBridge::bridgeETHToCall {
            to: address(&intent.recipient)?,
            minGasLimit: MIN_GAS,
            extraData: Bytes::new(),
        }
        .abi_encode()
    };
    Ok(EvmCall {
        chain_id: intent.source.chain_id,
        to: to.into(),
        value: if intent.source.token.is_none() {
            intent.amount.clone()
        } else {
            Quantity::from_u64(0)
        },
        data: hex_bytes(&data),
    })
}
fn validate_bounds(quote: &BridgeQuote, bounds: &BridgeBounds) -> Result<()> {
    let intent = &quote.intent;
    let receipt = intent.destination_assets[0].amount(intent.amount.clone());
    ensure(
        quote.gross_debit == intent.source.amount(intent.amount.clone())
            && quote.expected_receipt == receipt
            && quote.minimum_receipt == receipt,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical quote must conserve the exact principal in its selected representation.",
    )?;
    ensure(
        bounds.latest_protocol_deadline.is_none(),
        "BRIDGE_BOUNDS_UNSUPPORTED",
        "prepare",
        "Canonical bridge has no protocol execution deadline.",
    )?;
    if let Some(input) = &bounds.input {
        ensure(
            input.minimum <= intent.amount && intent.amount <= input.maximum,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical amount falls outside the approved input interval.",
        )?;
    }
    if let Some(max) = &bounds.maximum_source_debit {
        ensure(
            max >= &intent.amount,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical amount exceeds the source cap.",
        )?;
    }
    if let Some(min) = &bounds.minimum_net_receipt {
        ensure(
            min.asset == receipt.asset
                && !min.atoms.integer().is_zero()
                && min.atoms <= receipt.atoms,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical receipt floor differs from its exact asset or exceeds principal.",
        )?;
    }
    if let Some(ratio) = &bounds.minimum_output_input_ratio {
        ensure(
            !ratio.denominator.integer().is_zero() && ratio.numerator <= ratio.denominator,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical route cannot return a principal ratio above one.",
        )?;
    }
    let expected_chains = if intent.source.chain_id == BASE_CHAIN_ID {
        vec![BASE_CHAIN_ID, 1]
    } else {
        vec![1]
    };
    ensure(
        quote.fees.len() == expected_chains.len(),
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical fee entries must cover source and deferred-stage networks exactly.",
    )?;
    for (fee, chain_id) in quote.fees.iter().zip(expected_chains) {
        ensure(
            fee.kind == FeeKind::Gas
                && fee.amount.asset
                    == EvmAsset {
                        chain_id,
                        token: None,
                        decimals: 18,
                    }
                    .asset_id()
                && address(&fee.payer)? == address(&intent.source_address)?,
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Canonical fee identity differs from its exact native gas asset.",
        )?;
    }
    for cap in &bounds.fee_caps {
        let sum = quote
            .fees
            .iter()
            .filter(|f| f.kind == cap.kind && f.amount.asset == cap.asset)
            .fold(U256::ZERO, |total, f| {
                total.saturating_add(u256(&f.amount.atoms).unwrap_or(U256::MAX))
            });
        ensure(
            sum <= u256(&cap.maximum)?,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical fees exceed the selected cap.",
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseDeposit {
    pub source_block_hash: String,
    pub portal_log_index: u32,
    pub source_hash: String,
    pub transaction_hash: String,
    pub from: String,
    pub to: String,
    pub mint: Quantity,
    pub value: Quantity,
    pub gas: u64,
    pub data: String,
    pub message_hash: String,
    pub bridge_log_index: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseWithdrawal {
    pub nonce: Quantity,
    pub sender: String,
    pub target: String,
    pub value: Quantity,
    pub gas_limit: Quantity,
    pub data: String,
}
impl BaseWithdrawal {
    fn typed(&self) -> Result<WithdrawalTransaction> {
        Ok(WithdrawalTransaction {
            nonce: u256(&self.nonce)?,
            sender: address(&self.sender)?,
            target: address(&self.target)?,
            value: u256(&self.value)?,
            gasLimit: u256(&self.gas_limit)?,
            data: rpc_bytes(&json!(self.data))?,
        })
    }
    /// Solidity hashes the six fields, not an extra dynamic-struct tuple offset.
    pub fn hash(&self) -> Result<B256> {
        let tx = self.typed()?;
        Ok(keccak256(
            (
                tx.nonce,
                tx.sender,
                tx.target,
                tx.value,
                tx.gasLimit,
                tx.data,
            )
                .abi_encode_params(),
        ))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedBaseWithdrawal {
    pub withdrawal: BaseWithdrawal,
    pub withdrawal_hash: String,
    pub message_hash: String,
    pub message_log_index: u32,
    pub bridge_log_index: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseWithdrawalReadiness {
    pub observed_block: u64,
    pub finalized: bool,
    pub proven_at: u64,
    pub proof_maturity_seconds: Quantity,
    pub game: String,
    pub claim_valid: bool,
    pub proof_invalidated: bool,
    pub expected_resolution_at: Option<u64>,
    pub ready: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BaseWithdrawalWitness {
    pub dispute_game_index: u64,
    pub checkpoint_block: u64,
    pub version: String,
    pub state_root: String,
    pub message_passer_storage_root: String,
    pub latest_block_hash: String,
    pub withdrawal_proof: Vec<String>,
    pub output_root: String,
}
impl BaseWithdrawalWitness {
    fn output_proof(&self) -> Result<OutputRootProof> {
        Ok(OutputRootProof {
            version: hash(&self.version)?,
            stateRoot: hash(&self.state_root)?,
            messagePasserStorageRoot: hash(&self.message_passer_storage_root)?,
            latestBlockhash: hash(&self.latest_block_hash)?,
        })
    }
    pub fn root(&self) -> Result<B256> {
        let proof = self.output_proof()?;
        Ok(keccak256(
            (
                proof.version,
                proof.stateRoot,
                proof.messagePasserStorageRoot,
                proof.latestBlockhash,
            )
                .abi_encode(),
        ))
    }
    fn from_rpc(
        index: u64,
        checkpoint: u64,
        expected: B256,
        block: &Value,
        proof: &Value,
        key: B256,
    ) -> Result<Self> {
        ensure(
            address(
                proof["address"]
                    .as_str()
                    .ok_or_else(|| proof_error("Storage proof missing address."))?,
            )? == address(MESSAGE_PASSER)?,
            "BRIDGE_PROOF_MISMATCH",
            "read",
            "Storage proof belongs to another contract.",
        )?;
        let storage = proof["storageProof"]
            .as_array()
            .ok_or_else(|| proof_error("Storage proof missing nodes."))?;
        ensure(
            storage.len() == 1
                && rpc_u256(&storage[0]["key"])? == U256::from_be_slice(key.as_slice())
                && rpc_u256(&storage[0]["value"])? == U256::from(1),
            "BRIDGE_PROOF_MISMATCH",
            "read",
            "Checkpoint does not prove the exact withdrawal's sentMessages slot is true.",
        )?;
        let nodes = storage[0]["proof"]
            .as_array()
            .ok_or_else(|| proof_error("Missing MPT nodes."))?;
        ensure(
            !nodes.is_empty() && nodes.len() <= 128,
            "BRIDGE_PROOF_INVALID",
            "read",
            "Storage proof node count is unsupported.",
        )?;
        let mut withdrawal_proof = Vec::new();
        for node in nodes {
            let bytes = rpc_bytes(node)?;
            ensure(
                !bytes.is_empty() && bytes.len() <= 32768,
                "BRIDGE_PROOF_INVALID",
                "read",
                "Storage proof node is empty or too large.",
            )?;
            withdrawal_proof.push(hex_bytes(&bytes));
        }
        let witness = Self {
            dispute_game_index: index,
            checkpoint_block: checkpoint,
            version: format!("{:#x}", B256::ZERO),
            state_root: block["stateRoot"]
                .as_str()
                .ok_or_else(|| proof_error("Checkpoint state root missing."))?
                .into(),
            message_passer_storage_root: proof["storageHash"]
                .as_str()
                .ok_or_else(|| proof_error("Message passer storage root missing."))?
                .into(),
            latest_block_hash: block["hash"]
                .as_str()
                .ok_or_else(|| proof_error("Checkpoint block hash missing."))?
                .into(),
            withdrawal_proof,
            output_root: format!("{expected:#x}"),
        };
        ensure(
            witness.root()? == expected,
            "BRIDGE_PROOF_MISMATCH",
            "read",
            "Configured Base checkpoint and message-passer storage root differ from the reviewed game's output claim.",
        )?;
        Ok(witness)
    }
}
pub fn withdrawal_storage_key(withdrawal_hash: B256) -> B256 {
    keccak256((withdrawal_hash, U256::ZERO).abi_encode())
}
pub fn encode_prove_call(
    withdrawal: &BaseWithdrawal,
    witness: &BaseWithdrawalWitness,
) -> Result<EvmCall> {
    ensure(
        hash(&witness.version)? == B256::ZERO
            && witness.root()? == hash(&witness.output_root)?
            && !witness.withdrawal_proof.is_empty()
            && witness.withdrawal_proof.len() <= 128,
        "BRIDGE_PROOF_INVALID",
        "prepare",
        "Canonical output proof is malformed or has an unsupported version.",
    )?;
    let nodes = witness
        .withdrawal_proof
        .iter()
        .map(|n| rpc_bytes(&json!(n)))
        .collect::<Result<Vec<_>>>()?;
    ensure(
        nodes.iter().map(|node| node.len()).sum::<usize>() <= 65_536,
        "BRIDGE_PROOF_INVALID",
        "prepare",
        "Withdrawal witness exceeds the finite reviewed proof size.",
    )?;
    let data = OptimismPortal::proveWithdrawalTransactionCall {
        tx_: withdrawal.typed()?,
        disputeGameIndex: U256::from(witness.dispute_game_index),
        outputRootProof: witness.output_proof()?,
        withdrawalProof: nodes,
    }
    .abi_encode();
    Ok(EvmCall {
        chain_id: 1,
        to: PORTAL.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(&data),
    })
}
pub fn encode_finalize_call(withdrawal: &BaseWithdrawal, submitter: &str) -> Result<EvmCall> {
    let data = OptimismPortal::finalizeWithdrawalTransactionExternalProofCall {
        tx_: withdrawal.typed()?,
        proofSubmitter: address(submitter)?,
    }
    .abi_encode();
    Ok(EvmCall {
        chain_id: 1,
        to: PORTAL.into(),
        value: Quantity::from_u64(0),
        data: hex_bytes(&data),
    })
}

pub fn deposit_source_hash(block_hash: B256, log_index: u64) -> B256 {
    let deposit_id = keccak256((block_hash, U256::from(log_index)).abi_encode());
    keccak256((U256::ZERO, deposit_id).abi_encode())
}
/// Consensus EIP-2718 deposit payload: sourceHash, from, to, mint, value,
/// gasLimit, false, data. OP receipt depositNonce is deliberately excluded.
pub fn deposit_transaction_hash(
    source_hash: B256,
    from: Address,
    to: Address,
    mint: U256,
    value: U256,
    gas: u64,
    data: &[u8],
) -> B256 {
    let mut payload = Vec::new();
    for item in [
        rlp_bytes(source_hash.as_slice()),
        rlp_bytes(from.as_slice()),
        rlp_bytes(to.as_slice()),
        rlp_uint(mint),
        rlp_uint(value),
        rlp_uint(U256::from(gas)),
        rlp_bytes(&[]),
        rlp_bytes(data),
    ] {
        payload.extend(item);
    }
    let mut raw = vec![0x7e];
    raw.extend(rlp_list(&payload));
    keccak256(raw)
}
fn alias(from: Address) -> Address {
    let offset =
        U256::from_str_radix("1111000000000000000000000000000000001111", 16).expect("constant");
    let sum = U256::from_be_slice(from.as_slice()).wrapping_add(offset);
    Address::from_slice(&sum.to_be_bytes::<32>()[12..])
}
fn relay_payload(intent: &BridgeIntent, nonce: U256, min_gas: U256) -> Result<Vec<u8>> {
    let from = address(&intent.source_address)?;
    let to = address(&intent.recipient)?;
    let amount = u256(&intent.amount)?;
    let (source_bridge, destination_bridge) = if intent.source.chain_id == 1 {
        (L1_STANDARD_BRIDGE, L2_STANDARD_BRIDGE)
    } else {
        (L2_STANDARD_BRIDGE, L1_STANDARD_BRIDGE)
    };
    let message = if intent.source.token.is_none() {
        StandardBridge::finalizeBridgeETHCall {
            from,
            to,
            amount,
            extraData: Bytes::new(),
        }
        .abi_encode()
    } else {
        StandardBridge::finalizeBridgeERC20Call {
            localToken: address(
                intent.destination_assets[0]
                    .token
                    .as_deref()
                    .expect("validated"),
            )?,
            remoteToken: address(intent.source.token.as_deref().expect("validated"))?,
            from,
            to,
            amount,
            extraData: Bytes::new(),
        }
        .abi_encode()
    };
    Ok(Messenger::relayMessageCall {
        nonce,
        sender: address(source_bridge)?,
        target: address(destination_bridge)?,
        value: if intent.source.token.is_none() {
            amount
        } else {
            U256::ZERO
        },
        minGasLimit: min_gas,
        message: message.into(),
    }
    .abi_encode())
}
fn verify_relay_payload(data: &[u8], intent: &BridgeIntent) -> Result<B256> {
    let relay = Messenger::relayMessageCall::abi_decode(data)
        .map_err(|_| proof_error("Message payload is not the reviewed relayMessage ABI."))?;
    ensure(
        relay.nonce >> 240 == U256::from(1)
            && relay.minGasLimit == U256::from(MIN_GAS)
            && data == relay_payload(intent, relay.nonce, relay.minGasLimit)?.as_slice(),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Cross-domain message differs from the exact bridge assets, recipient, quantity, sender, target or version.",
    )?;
    Ok(keccak256(data))
}
fn decode_event<E: SolEvent>(log: &CanonicalRawLog, emitter: &str) -> Result<E> {
    ensure(
        address(&log.address)? == address(emitter)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Bridge event emitter differs from the reviewed contract.",
    )?;
    let topics = log
        .topics
        .iter()
        .map(|t| hash(t))
        .collect::<Result<Vec<_>>>()?;
    let data = rpc_bytes(&json!(log.data))?;
    E::decode_raw_log_validate(topics, &data)
        .map_err(|_| proof_error("Event differs from its exact reviewed ABI."))
}
fn find_log<'a>(
    logs: &'a [CanonicalRawLog],
    emitter: &str,
    signature: B256,
) -> Result<&'a CanonicalRawLog> {
    let matching: Vec<_> = logs
        .iter()
        .filter(|l| {
            l.address.eq_ignore_ascii_case(emitter)
                && l.topics
                    .first()
                    .is_some_and(|t| t.eq_ignore_ascii_case(&format!("{signature:#x}")))
        })
        .collect();
    ensure(
        matching.len() == 1,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Expected exactly one correlated reviewed bridge event.",
    )?;
    Ok(matching[0])
}
fn verify_bridge_log(
    logs: &[CanonicalRawLog],
    intent: &BridgeIntent,
    finalized: bool,
) -> Result<u32> {
    let bridge = if (intent.source.chain_id == 1) == !finalized {
        L1_STANDARD_BRIDGE
    } else {
        L2_STANDARD_BRIDGE
    };
    let from = address(&intent.source_address)?;
    let to = address(&intent.recipient)?;
    let amount = u256(&intent.amount)?;
    if let Some(source_token) = &intent.source.token {
        let (token, transfer_from, transfer_to) = if finalized {
            (
                intent.destination_assets[0]
                    .token
                    .as_deref()
                    .expect("validated"),
                if intent.evm_destination_chain_id()? == BASE_CHAIN_ID {
                    Address::ZERO
                } else {
                    address(bridge)?
                },
                to,
            )
        } else {
            (
                source_token.as_str(),
                from,
                if intent.source.chain_id == BASE_CHAIN_ID {
                    Address::ZERO
                } else {
                    address(bridge)?
                },
            )
        };
        let transfer_log = find_log(logs, token, Token::Transfer::SIGNATURE_HASH)?;
        let transfer = decode_event::<Token::Transfer>(transfer_log, token)?;
        ensure(
            transfer.from == transfer_from
                && transfer.to == transfer_to
                && transfer.value == amount,
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Canonical ERC20 proof lacks exact escrow/burn or release/mint effects.",
        )?;
    }
    if intent.source.token.is_none() {
        if finalized {
            let log = find_log(
                logs,
                bridge,
                StandardBridge::ETHBridgeFinalized::SIGNATURE_HASH,
            )?;
            let event = decode_event::<StandardBridge::ETHBridgeFinalized>(log, bridge)?;
            ensure(
                event.from == from
                    && event.to == to
                    && event.amount == amount
                    && event.extraData.is_empty(),
                "BRIDGE_PROOF_MISMATCH",
                "reconcile",
                "Finalized native bridge event differs from exact approved principal.",
            )?;
            Ok(log.log_index)
        } else {
            let log = find_log(
                logs,
                bridge,
                StandardBridge::ETHBridgeInitiated::SIGNATURE_HASH,
            )?;
            let event = decode_event::<StandardBridge::ETHBridgeInitiated>(log, bridge)?;
            ensure(
                event.from == from
                    && event.to == to
                    && event.amount == amount
                    && event.extraData.is_empty(),
                "BRIDGE_PROOF_MISMATCH",
                "reconcile",
                "Initiated native bridge event differs from exact approved principal.",
            )?;
            Ok(log.log_index)
        }
    } else {
        let (local, remote) = if finalized {
            (
                intent.destination_assets[0]
                    .token
                    .as_deref()
                    .expect("validated"),
                intent.source.token.as_deref().expect("validated"),
            )
        } else {
            (
                intent.source.token.as_deref().expect("validated"),
                intent.destination_assets[0]
                    .token
                    .as_deref()
                    .expect("validated"),
            )
        };
        if finalized {
            let log = find_log(
                logs,
                bridge,
                StandardBridge::ERC20BridgeFinalized::SIGNATURE_HASH,
            )?;
            let event = decode_event::<StandardBridge::ERC20BridgeFinalized>(log, bridge)?;
            ensure(
                event.localToken == address(local)?
                    && event.remoteToken == address(remote)?
                    && event.from == from
                    && event.to == to
                    && event.amount == amount
                    && event.extraData.is_empty(),
                "BRIDGE_PROOF_MISMATCH",
                "reconcile",
                "Finalized ERC20 bridge event differs from exact approved pair and principal.",
            )?;
            Ok(log.log_index)
        } else {
            let log = find_log(
                logs,
                bridge,
                StandardBridge::ERC20BridgeInitiated::SIGNATURE_HASH,
            )?;
            let event = decode_event::<StandardBridge::ERC20BridgeInitiated>(log, bridge)?;
            ensure(
                event.localToken == address(local)?
                    && event.remoteToken == address(remote)?
                    && event.from == from
                    && event.to == to
                    && event.amount == amount
                    && event.extraData.is_empty(),
                "BRIDGE_PROOF_MISMATCH",
                "reconcile",
                "Initiated ERC20 bridge event differs from exact approved pair and principal.",
            )?;
            Ok(log.log_index)
        }
    }
}
pub fn verify_deposit_source(
    logs: &[CanonicalRawLog],
    intent: &BridgeIntent,
    source_block_hash: &str,
) -> Result<BaseDeposit> {
    validate_intent(intent)?;
    ensure(
        intent.source.chain_id == 1,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Expected Ethereum to Base source deposit.",
    )?;
    let bridge_log_index = verify_bridge_log(logs, intent, false)?;
    let log = find_log(
        logs,
        PORTAL,
        OptimismPortal::TransactionDeposited::SIGNATURE_HASH,
    )?;
    let event = decode_event::<OptimismPortal::TransactionDeposited>(log, PORTAL)?;
    let opaque = &event.opaqueData;
    ensure(
        log.log_index != bridge_log_index
            && event.version == U256::ZERO
            && event.from == alias(address(L1_MESSENGER)?)
            && event.to == address(L2_MESSENGER)?
            && opaque.len() >= 73,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Portal deposit version, messenger identity or payload differs from the reviewed bridge flow.",
    )?;
    let mint = U256::from_be_slice(&opaque[..32]);
    let value = U256::from_be_slice(&opaque[32..64]);
    let gas = u64::from_be_bytes(opaque[64..72].try_into().expect("length checked"));
    let expected = if intent.source.token.is_none() {
        u256(&intent.amount)?
    } else {
        U256::ZERO
    };
    ensure(
        mint == expected && value == expected && opaque[72] == 0 && gas >= u64::from(MIN_GAS),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Portal deposit value, mint, creation flag or gas does not match the financial message.",
    )?;
    let message_hash = verify_relay_payload(&opaque[73..], intent)?;
    let block_hash = hash(source_block_hash)?;
    let source_hash = deposit_source_hash(block_hash, u64::from(log.log_index));
    let transaction_hash = deposit_transaction_hash(
        source_hash,
        event.from,
        event.to,
        mint,
        value,
        gas,
        &opaque[73..],
    );
    Ok(BaseDeposit {
        source_block_hash: format!("{block_hash:#x}"),
        portal_log_index: log.log_index,
        source_hash: format!("{source_hash:#x}"),
        transaction_hash: format!("{transaction_hash:#x}"),
        from: format!("{:#x}", event.from),
        to: format!("{:#x}", event.to),
        mint: quantity(mint),
        value: quantity(value),
        gas,
        data: hex_bytes(&opaque[73..]),
        message_hash: format!("{message_hash:#x}"),
        bridge_log_index,
    })
}
pub fn verify_withdrawal_source(
    logs: &[CanonicalRawLog],
    intent: &BridgeIntent,
) -> Result<VerifiedBaseWithdrawal> {
    validate_intent(intent)?;
    ensure(
        intent.source.chain_id == BASE_CHAIN_ID,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Expected Base to Ethereum withdrawal initiation.",
    )?;
    let bridge_log_index = verify_bridge_log(logs, intent, false)?;
    let log = find_log(
        logs,
        MESSAGE_PASSER,
        MessagePasser::MessagePassed::SIGNATURE_HASH,
    )?;
    let event = decode_event::<MessagePasser::MessagePassed>(log, MESSAGE_PASSER)?;
    let expected = if intent.source.token.is_none() {
        u256(&intent.amount)?
    } else {
        U256::ZERO
    };
    ensure(
        log.log_index != bridge_log_index
            && event.sender == address(L2_MESSENGER)?
            && event.target == address(L1_MESSENGER)?
            && event.value == expected
            && event.gasLimit >= U256::from(MIN_GAS),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Withdrawal messenger, target, amount or gas differs from the reviewed financial message.",
    )?;
    let message_hash = verify_relay_payload(&event.data, intent)?;
    let withdrawal = BaseWithdrawal {
        nonce: quantity(event.nonce),
        sender: format!("{:#x}", event.sender),
        target: format!("{:#x}", event.target),
        value: quantity(event.value),
        gas_limit: quantity(event.gasLimit),
        data: hex_bytes(&event.data),
    };
    let calculated = withdrawal.hash()?;
    ensure(
        calculated == event.withdrawalHash,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "MessagePassed withdrawal hash differs from its exact immutable payload.",
    )?;
    Ok(VerifiedBaseWithdrawal {
        withdrawal,
        withdrawal_hash: format!("{calculated:#x}"),
        message_hash: format!("{message_hash:#x}"),
        message_log_index: log.log_index,
        bridge_log_index,
    })
}
/// The caller canonicalizes the exact source-derived transaction receipt first.
/// A successful messenger relay and bridge credit must coexist in that receipt.
pub fn verify_destination_logs(
    logs: &[CanonicalRawLog],
    intent: &BridgeIntent,
    expected_message_hash: &str,
) -> Result<u32> {
    validate_intent(intent)?;
    let index = verify_bridge_log(logs, intent, true)?;
    let messenger = if intent.evm_destination_chain_id()? == 1 {
        L1_MESSENGER
    } else {
        L2_MESSENGER
    };
    let log = find_log(logs, messenger, Messenger::RelayedMessage::SIGNATURE_HASH)?;
    let relay = decode_event::<Messenger::RelayedMessage>(log, messenger)?;
    ensure(
        index != log.log_index && relay.msgHash == hash(expected_message_hash)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination bridge credit lacks the exact source message relay.",
    )?;
    ensure(
        !logs.iter().any(|l| {
            l.address.eq_ignore_ascii_case(messenger)
                && l.topics.first().is_some_and(|t| {
                    t.eq_ignore_ascii_case(&format!(
                        "{:#x}",
                        Messenger::FailedRelayedMessage::SIGNATURE_HASH
                    ))
                })
        }),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination receipt contains a failed relay.",
    )?;
    Ok(index)
}
pub fn verify_deposit_transaction(tx: &Value, expected: &BaseDeposit) -> Result<()> {
    let source_hash = deposit_source_hash(
        hash(&expected.source_block_hash)?,
        u64::from(expected.portal_log_index),
    );
    let calculated = deposit_transaction_hash(
        source_hash,
        address(&expected.from)?,
        address(&expected.to)?,
        u256(&expected.mint)?,
        u256(&expected.value)?,
        expected.gas,
        &rpc_bytes(&json!(expected.data))?,
    );
    ensure(
        rpc_u64(&tx["type"])? == 0x7e
            && tx["hash"]
                .as_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&expected.transaction_hash))
            && calculated == hash(&expected.transaction_hash)?
            && source_hash == hash(&expected.source_hash)?
            && hash(
                tx["sourceHash"]
                    .as_str()
                    .ok_or_else(|| proof_error("Deposit transaction lacks sourceHash."))?,
            )? == source_hash
            && address(
                tx["from"]
                    .as_str()
                    .ok_or_else(|| proof_error("Deposit sender missing."))?,
            )? == address(&expected.from)?
            && address(
                tx["to"]
                    .as_str()
                    .ok_or_else(|| proof_error("Deposit target missing."))?,
            )? == address(&expected.to)?
            && rpc_u256(&tx["mint"])? == u256(&expected.mint)?
            && rpc_u256(&tx["value"])? == u256(&expected.value)?
            && rpc_u64(&tx["gas"])? == expected.gas
            // OP RPC implementations may omit a false system flag. The
            // source-derived transaction hash already commits to false.
            && tx.get("isSystemTx").is_none_or(|flag| flag == &json!(false))
            && rpc_bytes(&tx["input"])? == rpc_bytes(&json!(expected.data))?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Base destination deposit transaction differs from its source-derived OP Stack identity.",
    )
}
pub fn verify_withdrawal_finalized(logs: &[CanonicalRawLog], expected_hash: &str) -> Result<u32> {
    let log = find_log(
        logs,
        PORTAL,
        OptimismPortal::WithdrawalFinalized::SIGNATURE_HASH,
    )?;
    let event = decode_event::<OptimismPortal::WithdrawalFinalized>(log, PORTAL)?;
    ensure(
        event.withdrawalHash == hash(expected_hash)? && event.success,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Portal withdrawal was not finalized successfully for this immutable message.",
    )?;
    Ok(log.log_index)
}
pub fn verify_withdrawal_proven(
    logs: &[CanonicalRawLog],
    withdrawal: &BaseWithdrawal,
    submitter: &str,
) -> Result<u32> {
    let log = find_log(
        logs,
        PORTAL,
        OptimismPortal::WithdrawalProven::SIGNATURE_HASH,
    )?;
    let event = decode_event::<OptimismPortal::WithdrawalProven>(log, PORTAL)?;
    let extension = find_log(
        logs,
        PORTAL,
        OptimismPortal::WithdrawalProvenExtension1::SIGNATURE_HASH,
    )?;
    let event_extension =
        decode_event::<OptimismPortal::WithdrawalProvenExtension1>(extension, PORTAL)?;
    ensure(
        log.log_index != extension.log_index
            && event.withdrawalHash == withdrawal.hash()?
            && event.from == address(&withdrawal.sender)?
            && event.to == address(&withdrawal.target)?
            && event_extension.withdrawalHash == event.withdrawalHash
            && event_extension.proofSubmitter == address(submitter)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Portal proof event differs from the withdrawal and exact proof submitter.",
    )?;
    Ok(log.log_index)
}
fn proof_error(message: &str) -> fin_domain::FinError {
    bridge_error(
        "BRIDGE_PROOF_INVALID",
        "validation",
        message,
        "Keep the transfer unresolved and inspect its saved canonical evidence.",
    )
}
fn rlp_uint(value: U256) -> Vec<u8> {
    let bytes = value.to_be_bytes::<32>();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(32);
    rlp_bytes(&bytes[start..])
}
fn rlp_bytes(value: &[u8]) -> Vec<u8> {
    if value.len() == 1 && value[0] < 0x80 {
        return value.to_vec();
    }
    let mut out = rlp_prefix(0x80, 0xb7, value.len());
    out.extend(value);
    out
}
fn rlp_list(payload: &[u8]) -> Vec<u8> {
    let mut out = rlp_prefix(0xc0, 0xf7, payload.len());
    out.extend(payload);
    out
}
fn rlp_prefix(short: u8, long: u8, len: usize) -> Vec<u8> {
    if len <= 55 {
        return vec![short + len as u8];
    }
    let bytes = len.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).expect("nonzero length");
    let mut out = vec![long + (bytes.len() - start) as u8];
    out.extend(&bytes[start..]);
    out
}

fn mintable_interface_id() -> [u8; 4] {
    let mut id = [0u8; 4];
    for signature in [
        "remoteToken()",
        "bridge()",
        "mint(address,uint256)",
        "burn(address,uint256)",
    ] {
        let digest = keccak256(signature);
        for i in 0..4 {
            id[i] ^= digest[i];
        }
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_evm::EvmRpc;
    use std::sync::atomic::{AtomicU8, Ordering};
    struct FixtureRpc {
        chain: u64,
        mode: AtomicU8,
    }
    #[async_trait]
    impl EvmRpc for FixtureRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let mode = self.mode.load(Ordering::SeqCst);
            Ok(match method {
                "eth_chainId" => json!(format!("{:#x}", self.chain)),
                "eth_blockNumber" => json!("0x100"),
                "eth_getCode" => json!("0x6000"),
                "eth_estimateGas" => json!("0x186a0"),
                "eth_getTransactionCount" => json!("0x0"),
                "eth_getBalance" => json!("0xffffffffffffffffffff"),
                "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
                "eth_getBlockByNumber" => {
                    json!({"baseFeePerGas":"0x3b9aca00","number":"0x100","timestamp":"0x2000"})
                }
                "eth_call" => {
                    let data = params[0]["data"].as_str().expect("calldata");
                    if data.starts_with(&hex_bytes(
                        &OptimismPortal::anchorStateRegistryCall::SELECTOR,
                    )) {
                        json!(hex_bytes(&address(ANCHOR_REGISTRY)?.abi_encode()))
                    } else if data.starts_with(&hex_bytes(
                        &OptimismPortal::disputeGameFactoryCall::SELECTOR,
                    )) {
                        json!(hex_bytes(&address(GAME_FACTORY)?.abi_encode()))
                    } else if data.starts_with(&hex_bytes(
                        &OptimismPortal::finalizedWithdrawalsCall::SELECTOR,
                    )) {
                        json!(hex_bytes(&(mode == 4).abi_encode()))
                    } else if data
                        .starts_with(&hex_bytes(&OptimismPortal::provenWithdrawalsCall::SELECTOR))
                    {
                        json!(hex_bytes(
                            &(
                                address("0x9999999999999999999999999999999999999999")?,
                                if mode == 5 { 0u64 } else { 100u64 }
                            )
                                .abi_encode()
                        ))
                    } else if data.starts_with(&hex_bytes(
                        &OptimismPortal::proofMaturityDelaySecondsCall::SELECTOR,
                    )) {
                        json!(hex_bytes(&U256::from(3600).abi_encode()))
                    } else if data
                        .starts_with(&hex_bytes(&AnchorRegistry::isGameProperCall::SELECTOR))
                        || data
                            .starts_with(&hex_bytes(&AnchorRegistry::isGameRespectedCall::SELECTOR))
                    {
                        json!(hex_bytes(&true.abi_encode()))
                    } else if data
                        .starts_with(&hex_bytes(&AnchorRegistry::isGameClaimValidCall::SELECTOR))
                    {
                        json!(hex_bytes(&(mode != 2 && mode != 3).abi_encode()))
                    } else if data.starts_with(&hex_bytes(&Game::statusCall::SELECTOR)) {
                        json!(hex_bytes(
                            &U256::from(if mode == 2 {
                                1u8
                            } else if mode == 3 {
                                0u8
                            } else {
                                2u8
                            })
                            .abi_encode()
                        ))
                    } else if data.starts_with(&hex_bytes(&Game::expectedResolutionCall::SELECTOR))
                    {
                        json!(hex_bytes(&4321u64.abi_encode()))
                    } else if data
                        .starts_with(&hex_bytes(&OptimismPortal::checkWithdrawalCall::SELECTOR))
                    {
                        if mode == 1 {
                            return Err(fin_domain::FinError::new(
                                "RPC_REQUEST_FAILED",
                                "rpc",
                                "Synthetic immature proof.",
                                "Wait.",
                            ));
                        }
                        if mode == 6 {
                            return Err(fin_domain::FinError::new(
                                "RPC_UNAVAILABLE",
                                "rpc",
                                "Synthetic outage.",
                                "Retry.",
                            ));
                        }
                        json!("0x")
                    } else {
                        json!("0x")
                    }
                }
                other => panic!("Unexpected synthetic RPC {other}"),
            })
        }
    }
    fn provider() -> (BaseCanonicalProvider, Arc<FixtureRpc>) {
        let eth = Arc::new(FixtureRpc {
            chain: 1,
            mode: AtomicU8::new(0),
        });
        let base = Arc::new(FixtureRpc {
            chain: BASE_CHAIN_ID,
            mode: AtomicU8::new(0),
        });
        (
            BaseCanonicalProvider::new(
                Arc::new(EvmRuntime::new(eth.clone(), 1, None)),
                Arc::new(EvmRuntime::new(base, BASE_CHAIN_ID, None)),
                BaseCanonicalConfig::reviewed(),
            )
            .unwrap(),
            eth,
        )
    }
    fn native() -> BridgeIntent {
        BridgeIntent {
            wallet: fin_domain::ResourceId("synthetic-wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            },
            solana_destination: None,
            destination_chain_id: Some(BASE_CHAIN_ID),
            destination_assets: vec![EvmAsset {
                chain_id: BASE_CHAIN_ID,
                token: None,
                decimals: 18,
            }],
            recipient: "0x2222222222222222222222222222222222222222".into(),
            refund_to: "0x1111111111111111111111111111111111111111".into(),
            allowed_refund_assets: Vec::new(),
            amount: Quantity::from_u64(1_000_000),
        }
    }
    #[tokio::test]
    async fn executable_native_quote_is_exact_and_tampered_calls_fail() {
        let (provider, _) = provider();
        let intent = native();
        let batch = provider
            .quote(&BridgeQuoteRequest {
                cctp: None,
                intent: intent.clone(),
                bounds: BridgeBounds::default(),
            })
            .await
            .unwrap();
        let quote = &batch.quotes[0];
        provider
            .prepare(quote, &BridgeBounds::default(), &intent.source_address)
            .unwrap();
        for (field, value) in [
            ("to", json!(PORTAL)),
            ("data", json!("0x")),
            ("chain_id", json!(BASE_CHAIN_ID)),
            ("value", json!("2")),
        ] {
            let mut serialized = serde_json::to_value(quote).unwrap();
            serialized["protocol_steps"][0]["call"][field] = value;
            let altered: BridgeQuote = serde_json::from_value(serialized).unwrap();
            assert!(
                provider
                    .prepare(&altered, &BridgeBounds::default(), &intent.source_address)
                    .is_err()
            );
        }
        let saved_digest =
            scope_digest(quote, &BridgeBounds::default(), &intent.source_address).unwrap();
        let mut expired = quote.clone();
        expired.valid_until = "2020-01-01T00:00:00Z".into();
        assert_eq!(
            scope_digest(&expired, &BridgeBounds::default(), &intent.source_address).unwrap(),
            saved_digest
        );
        assert_eq!(
            provider
                .prepare(&expired, &BridgeBounds::default(), &intent.source_address)
                .unwrap_err()
                .code,
            "BRIDGE_QUOTE_EXPIRED"
        );
        let bounds = BridgeBounds {
            maximum_source_debit: Some(Quantity::from_u64(1)),
            ..BridgeBounds::default()
        };
        assert!(
            provider
                .prepare(quote, &bounds, &intent.source_address)
                .is_err()
        );
    }
    #[tokio::test]
    async fn readiness_uses_live_proof_state_invalidation_and_contract_checks() {
        let (provider, rpc) = provider();
        let withdrawal = BaseWithdrawal {
            nonce: Quantity::from_u64(1),
            sender: L2_MESSENGER.into(),
            target: L1_MESSENGER.into(),
            value: Quantity::from_u64(1),
            gas_limit: Quantity::from_u64(300_000),
            data: "0x".into(),
        };
        let submitter = "0x1111111111111111111111111111111111111111";
        let ready = provider
            .withdrawal_readiness(&withdrawal, submitter)
            .await
            .unwrap();
        assert!(ready.ready);
        assert_eq!(ready.proof_maturity_seconds, Quantity::from_u64(3600));
        assert_eq!(ready.expected_resolution_at, Some(4321));
        rpc.mode.store(1, Ordering::SeqCst);
        let immature = provider
            .withdrawal_readiness(&withdrawal, submitter)
            .await
            .unwrap();
        assert!(!immature.ready);
        assert!(!immature.proof_invalidated);
        rpc.mode.store(2, Ordering::SeqCst);
        let invalid = provider
            .withdrawal_readiness(&withdrawal, submitter)
            .await
            .unwrap();
        assert!(invalid.proof_invalidated);
        assert!(!invalid.ready);
        rpc.mode.store(3, Ordering::SeqCst);
        let pending = provider
            .withdrawal_readiness(&withdrawal, submitter)
            .await
            .unwrap();
        assert!(!pending.proof_invalidated);
        assert!(!pending.ready);
        rpc.mode.store(4, Ordering::SeqCst);
        let finalized = provider
            .withdrawal_readiness(&withdrawal, submitter)
            .await
            .unwrap();
        assert!(finalized.finalized);
        assert!(!finalized.ready);
        rpc.mode.store(5, Ordering::SeqCst);
        assert_eq!(
            provider
                .withdrawal_readiness(&withdrawal, submitter)
                .await
                .unwrap()
                .proven_at,
            0
        );
        rpc.mode.store(6, Ordering::SeqCst);
        assert_eq!(
            provider
                .withdrawal_readiness(&withdrawal, submitter)
                .await
                .unwrap_err()
                .code,
            "RPC_UNAVAILABLE"
        );
    }
    #[tokio::test]
    async fn route_discovery_native_is_separate_from_write_authority() {
        let (provider, _) = provider();
        let request = crate::BridgeRouteRequest {
            source: native().source,
            destination_chain_id: BASE_CHAIN_ID,
            destination_asset: None,
        };
        let result = provider.discover_routes(&request).await.unwrap();
        assert_eq!(result.availability, crate::RouteAvailability::Candidate);
        assert_eq!(result.candidates.len(), 1);
        let value = serde_json::to_value(result).unwrap();
        assert!(value.get("steps").is_none());
        assert!(value.get("transfer_id").is_none());
    }
}
