//! Constrained native-ETH deposits through reviewed Arbitrum-compatible inboxes.
//!
//! The provider constructs only `Inbox.depositEth()` calls from the source
//! wallet to one pinned Inbox. Source event verification derives the canonical
//! type-0x64 child transaction identity using the maintained Arbitrum SDK
//! encoding. Finality and receipt inclusion are checked by the broker's EVM
//! runtime; neither provider status nor a caller-supplied hash is settlement
//! evidence.

use crate::{
    BridgeBounds, BridgeCapabilities, BridgeEvidenceCandidate, BridgeFee, BridgeIntent,
    BridgeProvider, BridgeQuote, BridgeQuoteBatch, BridgeQuoteRequest, BridgeReconciliation,
    BridgeStep, BridgeStepKind, BridgeTransferRef, EvmAsset, EvmCall, FeeKind, PreparedBridge,
    QuoteClass, RefundPath, bridge_error, ensure,
};
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use fin_domain::{FinError, Quantity, Result};
use fin_evm::EvmRuntime;
use num_traits::Zero;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};

const SOURCE_CHAIN_ID: u64 = 1;
const INBOX_DEPOSIT_ETH_SELECTOR: [u8; 4] = [0x43, 0x93, 0x70, 0xb1];
const PROVIDER_ID: &str = "arbitrum_canonical";
const QUOTE_TTL_SECONDS: i64 = 90;
const ARRIVAL_ESTIMATE_SECONDS: u64 = 600;
const ARBITRUM_CHAIN_ID: u64 = 42161;
const ROBINHOOD_CHAIN_ID: u64 = 4663;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalDeployment {
    pub child_chain_id: u64,
    pub inbox: String,
    pub bridge: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalConfig {
    pub deployments: BTreeMap<u64, CanonicalDeployment>,
}

impl CanonicalConfig {
    /// Publicly reviewed mainnet deployments. The addresses are independently
    /// checked against these constants during validation and cannot be chosen
    /// by quote responses or user input.
    pub fn reviewed() -> Self {
        Self {
            deployments: BTreeMap::from([
                (
                    ARBITRUM_CHAIN_ID,
                    CanonicalDeployment {
                        child_chain_id: ARBITRUM_CHAIN_ID,
                        inbox: "0x4Dbd4fc535Ac27206064B68FfCf827b0A60BAB3f".into(),
                        bridge: "0x8315177aB297bA92A06054cE80a67Ed4DBd7ed3a".into(),
                    },
                ),
                (
                    ROBINHOOD_CHAIN_ID,
                    CanonicalDeployment {
                        child_chain_id: ROBINHOOD_CHAIN_ID,
                        inbox: "0x1A07cc4BD17E0118BdB54D70990D2158AbAD7a2D".into(),
                        bridge: "0xDf8755334ce7A73cCF6b581C02eA649AE3E864b3".into(),
                    },
                ),
            ]),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let reviewed = Self::reviewed();
        ensure(
            self.deployments == reviewed.deployments,
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Canonical Inbox and Bridge deployments must match the reviewed Ethereum mainnet configuration.",
        )
    }

    fn deployment(&self, chain_id: u64) -> Result<&CanonicalDeployment> {
        self.deployments.get(&chain_id).ok_or_else(|| {
            bridge_error(
                "BRIDGE_ROUTE_UNAVAILABLE",
                "quote",
                "No reviewed canonical native-ETH route exists for this destination chain.",
                "Choose Arbitrum One or Robinhood Chain, or use another supported provider.",
            )
        })
    }
}

#[derive(Clone)]
pub struct CanonicalProvider {
    source_runtime: Arc<EvmRuntime>,
    config: CanonicalConfig,
}

impl CanonicalProvider {
    pub fn new(source_runtime: Arc<EvmRuntime>, config: CanonicalConfig) -> Result<Self> {
        config.validate()?;
        ensure(
            source_runtime.chain_id == SOURCE_CHAIN_ID,
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Canonical provider requires the configured Ethereum mainnet read runtime.",
        )?;
        ensure(
            !source_runtime.can_sign(),
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Canonical provider must receive a source runtime with no signing capability.",
        )?;
        Ok(Self {
            source_runtime,
            config,
        })
    }

    fn validate_intent(&self, intent: &BridgeIntent) -> Result<&CanonicalDeployment> {
        let deployment = self.config.deployment(intent.destination_chain_id)?;
        let source = parse_address(&intent.source_address)?;
        let recipient = parse_address(&intent.recipient)?;
        ensure(
            intent.source.chain_id == SOURCE_CHAIN_ID
                && intent.source.token.is_none()
                && intent.source.decimals == 18,
            "BRIDGE_ROUTE_UNAVAILABLE",
            "quote",
            "Canonical provider supports only native ETH from Ethereum mainnet.",
        )?;
        ensure(
            intent.destination_chain_id != SOURCE_CHAIN_ID
                && intent.destination_chain_id == deployment.child_chain_id,
            "BRIDGE_ROUTE_UNAVAILABLE",
            "quote",
            "Canonical provider supports Ethereum mainnet deposits to the reviewed child chain only.",
        )?;
        ensure(
            intent.destination_assets.len() == 1
                && intent.destination_assets[0].chain_id == intent.destination_chain_id
                && intent.destination_assets[0].token.is_none()
                && intent.destination_assets[0].decimals == 18,
            "BRIDGE_ASSET_UNSUPPORTED",
            "quote",
            "Canonical provider supports one exact native-ETH destination asset only.",
        )?;
        ensure(
            source == recipient,
            "BRIDGE_RECIPIENT_UNSUPPORTED",
            "quote",
            "Canonical native-ETH deposits require the source wallet to be the destination recipient.",
        )?;
        ensure(
            intent
                .refund_to
                .eq_ignore_ascii_case(&intent.source_address)
                && intent.allowed_refund_assets.len() <= 8,
            "BRIDGE_REFUND_UNSUPPORTED",
            "quote",
            "Canonical deposits do not provide a protocol refund path; refund target must remain the source wallet and at most eight permitted refund assets may be supplied.",
        )?;
        ensure(
            intent.allowed_refund_assets.iter().all(|asset| {
                (asset.chain_id == SOURCE_CHAIN_ID || asset.chain_id == intent.destination_chain_id)
                    && asset
                        .token
                        .as_deref()
                        .is_none_or(|token| Address::from_str(token).is_ok())
            }),
            "BRIDGE_REFUND_UNSUPPORTED",
            "quote",
            "Permitted refund assets must be valid EVM assets on the source or destination chain.",
        )?;
        ensure(
            !intent.amount.is_zero(),
            "BRIDGE_AMOUNT_INVALID",
            "quote",
            "Canonical deposit amount must be positive.",
        )?;
        Ok(deployment)
    }
}

#[async_trait]
impl BridgeProvider for CanonicalProvider {
    async fn discover_routes(
        &self,
        request: &crate::BridgeRouteRequest,
    ) -> Result<crate::BridgeRouteDiscovery> {
        let supported = request.source.chain_id == SOURCE_CHAIN_ID
            && request.source.token.is_none()
            && self
                .config
                .deployments
                .contains_key(&request.destination_chain_id)
            && request.destination_asset.as_ref().is_none_or(|asset| {
                asset.chain_id == request.destination_chain_id
                    && asset.token.is_none()
                    && asset.decimals == 18
            });
        Ok(crate::BridgeRouteDiscovery {
            provider:PROVIDER_ID.into(), availability:if supported {crate::RouteAvailability::Candidate} else {crate::RouteAvailability::Unavailable},
            candidates:if supported {vec![crate::BridgeRouteCandidate {destination:crate::BridgeAssetInput {chain_id:request.destination_chain_id,token:None},reported_decimals:Some(18)}]} else {vec![]},
            observed_at:fin_domain::now(), evidence_source:"Reviewed Ethereum canonical Inbox deployments".into(),complete:true,
            limitations:vec!["Native ETH Ethereum-to-Arbitrum/Robinhood only; quote requires the same source EOA as recipient, exact deployment verification and finite gas. No reverse route or ERC-20 integration.".into()],error:None,
        })
    }
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: PROVIDER_ID.into(),
            chains: BTreeSet::from([SOURCE_CHAIN_ID, ARBITRUM_CHAIN_ID, ROBINHOOD_CHAIN_ID]),
            native_eth: true,
            erc20: false,
            protocol_versions: BTreeSet::from(["arbitrum_eth_deposit_v1".into()]),
        }
    }

    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let intent = &request.intent;
        let deployment = self.validate_intent(intent)?;
        let sender = parse_address(&intent.source_address)?;
        let inbox = parse_address(&deployment.inbox)?;
        let value = quantity_u256(&intent.amount)?;
        let bytes = Bytes::from(INBOX_DEPOSIT_ETH_SELECTOR.to_vec());

        // This performs read-only eth_call/eth_estimateGas and fee reads. The
        // actual wallet write remains a separate Fin-authorized operation.
        let estimate = self
            .source_runtime
            .prepare(sender, inbox, value, bytes, U256::MAX)
            .await
            .map_err(|_| {
                bridge_error(
                    "BRIDGE_QUOTE_UNAVAILABLE",
                    "quote",
                    "Ethereum could not simulate and estimate the reviewed canonical deposit call.",
                    "Retry the read-only quote later; no wallet transaction was submitted.",
                )
            })?;
        let gas_fee = quantity(estimate.transaction.maximum_cost()?);
        let asset = EvmAsset {
            chain_id: SOURCE_CHAIN_ID,
            token: None,
            decimals: 18,
        };
        let amount = asset.amount(intent.amount.clone());
        let now = Utc::now();
        let valid_until = now + ChronoDuration::seconds(QUOTE_TTL_SECONDS);
        let call = EvmCall {
            chain_id: SOURCE_CHAIN_ID,
            to: format!("{inbox:#x}"),
            value: intent.amount.clone(),
            data: format!("0x{}", hex::encode(INBOX_DEPOSIT_ETH_SELECTOR)),
        };
        let step = BridgeStep {
            ordinal: 0,
            kind: BridgeStepKind::Deposit,
            call,
            expected_sender: format!("{sender:#x}"),
            maximum_debit: vec![amount.clone()],
            dependencies: Vec::new(),
        };
        let digest = quote_digest(intent, &now.to_rfc3339())?;
        let quote = BridgeQuote {
            cctp: None,
            quote_id: format!("canonical:{digest}"),
            provider: PROVIDER_ID.into(),
            route_id: format!("ethereum-native-eth-to-{}", intent.destination_chain_id),
            intent: intent.clone(),
            gross_debit: amount.clone(),
            expected_receipt: EvmAsset {
                chain_id: intent.destination_chain_id,
                token: None,
                decimals: 18,
            }
            .amount(intent.amount.clone()),
            minimum_receipt: EvmAsset {
                chain_id: intent.destination_chain_id,
                token: None,
                decimals: 18,
            }
            .amount(intent.amount.clone()),
            fees: vec![BridgeFee {
                amount: asset.amount(gas_fee),
                kind: FeeKind::Gas,
                payer: format!("{sender:#x}"),
            }],
            approval: None,
            protocol_steps: vec![step],
            estimated_arrival_seconds: Some(ARRIVAL_ESTIMATE_SECONDS),
            observed_at: now.to_rfc3339(),
            valid_until: valid_until.to_rfc3339(),
            class: QuoteClass::Executable,
            refund_paths: Vec::<RefundPath>::new(),
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
        };
        validate_quote_bounds(&quote, &request.bounds)?;
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
        let deployment = self.validate_intent(&quote.intent)?;
        ensure(
            quote.provider == PROVIDER_ID
                && quote.class == QuoteClass::Executable
                && quote.approval.is_none()
                && quote.refund_paths.is_empty()
                && quote.protocol_order_id.is_none()
                && quote.provider_request_id.is_none()
                && quote.order_solver.is_none(),
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Quote is not a valid canonical native-ETH deposit quote.",
        )?;
        ensure(
            bounds.latest_protocol_deadline.is_none(),
            "BRIDGE_BOUNDS_UNSUPPORTED",
            "prepare",
            "Canonical deposits do not expose a protocol deadline to constrain.",
        )?;
        let expiry = chrono::DateTime::parse_from_rfc3339(&quote.valid_until).map_err(|_| {
            bridge_error(
                "BRIDGE_QUOTE_INVALID",
                "prepare",
                "Canonical quote validity timestamp is malformed.",
                "Request a fresh canonical quote.",
            )
        })?;
        ensure(
            expiry > Utc::now(),
            "BRIDGE_QUOTE_EXPIRED",
            "prepare",
            "Canonical quote has expired.",
        )?;
        let sender = parse_address(wallet)?;
        ensure(
            sender == parse_address(&quote.intent.source_address)?
                && quote.protocol_steps.len() == 1,
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Canonical quote sender or reviewed single-deposit step differs from the selected wallet.",
        )?;
        let step = &quote.protocol_steps[0];
        ensure(
            step.kind == BridgeStepKind::Deposit
                && step.ordinal == 0
                && step.dependencies.is_empty()
                && parse_address(&step.expected_sender)? == sender
                && step.maximum_debit == vec![quote.gross_debit.clone()]
                && parse_address(&step.call.to)? == parse_address(&deployment.inbox)?
                && step.call.chain_id == SOURCE_CHAIN_ID
                && step.call.value == quote.intent.amount
                && step.call.data.eq_ignore_ascii_case(&format!(
                    "0x{}",
                    hex::encode(INBOX_DEPOSIT_ETH_SELECTOR)
                )),
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Canonical deposit step differs from the pinned Inbox, exact value, or depositEth selector.",
        )?;
        validate_quote_bounds(quote, bounds)?;
        let digest = scope_digest(quote, bounds, wallet)?;
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps: quote.protocol_steps.clone(),
            source_order_id: None,
            fixed_scope_digest: digest,
        })
    }

    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        let Some(locator) = transfer.destination_locator.as_ref() else {
            return Ok(BridgeReconciliation {
                status: "awaiting_source_deposit_proof".into(),
                failure: None,
                refund_fail_reason: None,
                candidates: Vec::new(),
            });
        };
        ensure(
            locator.scheme == "arbitrum_eth_deposit_v1"
                && !locator.source_evidence_id.is_empty()
                && B256::from_str(&locator.transaction_hash).is_ok()
                && transfer.source_chain_id == SOURCE_CHAIN_ID
                && locator.destination_chain_id == transfer.destination_chain_id
                && (locator.destination_chain_id == ARBITRUM_CHAIN_ID
                    || locator.destination_chain_id == ROBINHOOD_CHAIN_ID),
            "BRIDGE_LOCATOR_INVALID",
            "reconcile",
            "Saved canonical destination locator is not a supported derived ETH deposit identity.",
        )?;
        Ok(BridgeReconciliation {
            status: "destination_deposit_candidate_available".into(),
            failure: None,
            refund_fail_reason: None,
            candidates: vec![BridgeEvidenceCandidate {
                order_id: None,
                kind: crate::EvidenceKind::DestinationFill,
                chain_id: locator.destination_chain_id,
                transaction_hash: locator.transaction_hash.clone(),
                correlation: Some(locator.scheme.clone()),
            }],
        })
    }
}

fn validate_quote_bounds(quote: &BridgeQuote, bounds: &BridgeBounds) -> Result<()> {
    let amount = &quote.intent.amount;
    let (minimum, maximum) = bounds
        .input
        .as_ref()
        .map(|range| (&range.minimum, &range.maximum))
        .unwrap_or((amount, amount));
    ensure(
        amount >= minimum
            && amount <= maximum
            && bounds
                .maximum_source_debit
                .as_ref()
                .is_none_or(|max| max >= amount),
        "BRIDGE_BOUNDS_VIOLATED",
        "prepare",
        "Canonical source amount exceeds its selected input or maximum debit bounds.",
    )?;
    ensure(
        bounds.latest_protocol_deadline.is_none(),
        "BRIDGE_BOUNDS_UNSUPPORTED",
        "prepare",
        "Canonical deposits do not expose a protocol deadline to constrain.",
    )?;
    let expected_source = EvmAsset {
        chain_id: SOURCE_CHAIN_ID,
        token: None,
        decimals: 18,
    }
    .asset_id();
    let destination = &quote.expected_receipt;
    ensure(
        quote.gross_debit.asset == expected_source
            && quote.gross_debit.atoms == *amount
            && destination.asset
                == EvmAsset {
                    chain_id: quote.intent.destination_chain_id,
                    token: None,
                    decimals: 18,
                }
                .asset_id()
            && destination.atoms == *amount
            && quote.minimum_receipt == *destination
            && quote.fees.len() == 1
            && quote.fees[0].kind == FeeKind::Gas
            && quote.fees[0].amount.asset == expected_source
            && quote.fees[0]
                .payer
                .eq_ignore_ascii_case(&quote.intent.source_address),
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Canonical quote must conserve the exact native-ETH principal and report only source gas fees.",
    )?;
    if let Some(minimum) = &bounds.minimum_net_receipt {
        ensure(
            minimum.asset == destination.asset
                && !quantity_u256(&minimum.atoms)?.is_zero()
                && quantity_u256(&minimum.atoms)? <= quantity_u256(&destination.atoms)?,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Canonical ETH deposit must have a positive same-asset receipt floor no greater than its exact receipt.",
        )?;
    }
    if let Some(ratio) = &bounds.minimum_output_input_ratio {
        let lhs = ratio.numerator.integer();
        let rhs = ratio.denominator.integer();
        ensure(
            !rhs.is_zero() && lhs <= rhs,
            "BRIDGE_BOUNDS_UNSUPPORTED",
            "prepare",
            "Canonical ETH deposits cannot satisfy a minimum output-to-input ratio above one.",
        )?;
    }
    for cap in &bounds.fee_caps {
        let mut total = U256::ZERO;
        for fee in quote
            .fees
            .iter()
            .filter(|fee| fee.amount.asset == cap.asset && fee.kind == cap.kind)
        {
            total = total
                .checked_add(quantity_u256(&fee.amount.atoms)?)
                .ok_or_else(|| proof_error("Quote fee total overflows uint256."))?;
        }
        if total > quantity_u256(&cap.maximum)? {
            return Err(bridge_error(
                "BRIDGE_BOUNDS_VIOLATED",
                "prepare",
                "Estimated canonical source gas fee exceeds the selected fee cap.",
                "Refresh the gas estimate or request a broader fee cap with new authorization.",
            ));
        }
    }
    Ok(())
}

fn quote_digest(intent: &BridgeIntent, observed_at: &str) -> Result<String> {
    let encoded = serde_json::to_vec(&(intent, observed_at)).map_err(|_| {
        bridge_error(
            "BRIDGE_QUOTE_INVALID",
            "quote",
            "Canonical quote identity could not be encoded.",
            "Retry the read-only quote; no wallet transaction was submitted.",
        )
    })?;
    Ok(format!("{:#x}", keccak256(encoded)))
}

fn scope_digest(quote: &BridgeQuote, bounds: &BridgeBounds, wallet: &str) -> Result<String> {
    // A new quote changes its ID, timestamps and estimated fee on every refresh.
    // Bind the authorization to the immutable route and normalized bounds so
    // those read-only refreshes do not change the operation-boundary scope.
    let config = CanonicalConfig::reviewed();
    let deployment = config.deployment(quote.intent.destination_chain_id)?;
    let mut intent = quote.intent.clone();
    let exact_amount = intent.amount.clone();
    intent.amount = Quantity::from_u64(0);
    let mut normalized_bounds = bounds.clone();
    normalized_bounds
        .input
        .get_or_insert(crate::AmountInterval {
            minimum: exact_amount.clone(),
            maximum: exact_amount,
        });
    let steps: Vec<_> = quote
        .protocol_steps
        .iter()
        .map(|step| {
            (
                step.ordinal,
                &step.kind,
                step.call.chain_id,
                step.call.to.to_ascii_lowercase(),
                step.call.data.to_ascii_lowercase(),
                step.expected_sender.to_ascii_lowercase(),
                &step.dependencies,
            )
        })
        .collect();
    let encoded = serde_json::to_vec(&(
        PROVIDER_ID,
        &quote.route_id,
        intent,
        steps,
        deployment,
        normalized_bounds,
        wallet.to_ascii_lowercase(),
    ))
    .map_err(|_| {
        bridge_error(
            "BRIDGE_SCOPE_INVALID",
            "prepare",
            "Canonical bridge authorization scope could not be encoded.",
            "Request a fresh quote.",
        )
    })?;
    Ok(format!("{:#x}", keccak256(encoded)))
}

fn parse_address(value: &str) -> Result<Address> {
    Address::from_str(value).map_err(|_| {
        bridge_error(
            "BRIDGE_PROOF_INVALID",
            "validation",
            "Expected an exact 20-byte EVM address.",
            "Keep the transfer unresolved and inspect its saved canonical evidence.",
        )
    })
}

fn quantity_u256(value: &Quantity) -> Result<U256> {
    U256::from_str(value.as_str()).map_err(|_| {
        bridge_error(
            "BRIDGE_AMOUNT_INVALID",
            "validation",
            "Atomic amount exceeds uint256.",
            "Use a positive uint256-bounded ETH amount.",
        )
    })
}

fn quantity(value: U256) -> Quantity {
    Quantity::new(value.to_string()).expect("U256 is a canonical unsigned quantity")
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalRawLog {
    pub log_index: u32,
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalDepositReceipt {
    pub inbox_log: CanonicalRawLog,
    pub bridge_log: CanonicalRawLog,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedEthDeposit {
    pub source_address: String,
    pub recipient: String,
    pub amount: Quantity,
    pub destination_chain_id: u64,
    pub inbox: String,
    pub bridge: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedEthDeposit {
    pub message_index: Quantity,
    pub source_address: String,
    pub aliased_sender: String,
    pub recipient: String,
    pub amount: Quantity,
    pub message_data: String,
    pub message_data_hash: String,
    pub inbox: String,
    pub bridge: String,
    pub inbox_log_index: u32,
    pub bridge_log_index: u32,
    pub destination_chain_id: u64,
    pub destination_transaction_hash: String,
}

/// Checks the paired Inbox and Bridge logs from one already-canonicalized L1
/// receipt. The broker must independently bind that receipt to the submitted
/// `depositEth()` transaction, its wallet/value/calldata and finality.
pub fn verify_eth_deposit_source(
    receipt: &CanonicalDepositReceipt,
    expected: &ExpectedEthDeposit,
) -> Result<VerifiedEthDeposit> {
    ensure(
        expected.destination_chain_id == ARBITRUM_CHAIN_ID
            || expected.destination_chain_id == ROBINHOOD_CHAIN_ID,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical source event names an unsupported child chain.",
    )?;
    let reviewed = CanonicalConfig::reviewed();
    let deployment = reviewed.deployment(expected.destination_chain_id)?;
    let source = parse_address(&expected.source_address)?;
    let recipient = parse_address(&expected.recipient)?;
    let inbox = parse_address(&expected.inbox)?;
    let bridge = parse_address(&expected.bridge)?;
    ensure(
        inbox == parse_address(&deployment.inbox)?
            && bridge == parse_address(&deployment.bridge)?
            && source == recipient
            && !quantity_u256(&expected.amount)?.is_zero(),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical proof expected terms differ from the reviewed same-wallet native deposit scope.",
    )?;
    let inbox_emitter = parse_address(&receipt.inbox_log.address)?;
    let bridge_emitter = parse_address(&receipt.bridge_log.address)?;
    ensure(
        inbox_emitter == inbox && bridge_emitter == bridge,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical source proof logs were not emitted by the reviewed Inbox and Bridge.",
    )?;
    ensure(
        receipt.inbox_log.log_index != receipt.bridge_log.log_index,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Inbox and Bridge proof logs cannot share one receipt log index.",
    )?;

    let inbox_topics = parse_topics(&receipt.inbox_log.topics, 2)?;
    ensure(
        inbox_topics[0] == keccak256("InboxMessageDelivered(uint256,bytes)")
            && receipt.inbox_log.topics.len() == 2,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical Inbox log is not InboxMessageDelivered(uint256,bytes).",
    )?;
    let message_index = U256::from_be_slice(inbox_topics[1].as_slice());
    let packed = decode_inbox_data(&receipt.inbox_log.data)?;
    ensure(
        packed.len() == 52
            && Address::from_slice(&packed[..20]) == recipient
            && U256::from_be_slice(&packed[20..]) == quantity_u256(&expected.amount)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Inbox deposit payload does not contain the approved recipient and exact ETH amount.",
    )?;
    let expected_hash = keccak256(&packed);

    // IBridge.MessageDelivered indexes both messageIndex and beforeInboxAcc.
    // The latter therefore belongs in topic[2], not the ABI data section.
    let bridge_topics = parse_topics(&receipt.bridge_log.topics, 3)?;
    ensure(
        bridge_topics[0]
            == keccak256(
                "MessageDelivered(uint256,bytes32,address,uint8,address,bytes32,uint256,uint64)",
            )
            && receipt.bridge_log.topics.len() == 3,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical Bridge log is not MessageDelivered with the reviewed event layout.",
    )?;
    let bridge_index = U256::from_be_slice(bridge_topics[1].as_slice());
    let bridge_data = decode_hex(&receipt.bridge_log.data)?;
    ensure(
        bridge_data.len() == 192,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Bridge MessageDelivered data must contain its six canonical ABI words.",
    )?;
    let event_inbox = word_address(&bridge_data[0..32])?;
    let kind = word_u256(&bridge_data[32..64])?;
    let aliased_sender = word_address(&bridge_data[64..96])?;
    let data_hash = B256::from_slice(&bridge_data[96..128]);
    let expected_alias = apply_l1_to_l2_alias(source);
    ensure(
        bridge_index == message_index
            && event_inbox == inbox
            && kind == U256::from(12u8)
            && aliased_sender == expected_alias
            && data_hash == expected_hash,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Inbox and Bridge logs do not prove the same aliased native-ETH deposit message.",
    )?;
    let amount = quantity_u256(&expected.amount)?;
    let child_hash = derive_eth_deposit_tx_hash(
        expected.destination_chain_id,
        message_index,
        aliased_sender,
        recipient,
        amount,
    );
    Ok(VerifiedEthDeposit {
        message_index: quantity(message_index),
        source_address: format!("{source:#x}"),
        aliased_sender: format!("{aliased_sender:#x}"),
        recipient: format!("{recipient:#x}"),
        amount: quantity(amount),
        message_data: format!("0x{}", hex::encode(&packed)),
        message_data_hash: format!("{expected_hash:#x}"),
        inbox: format!("{inbox:#x}"),
        bridge: format!("{bridge:#x}"),
        inbox_log_index: receipt.inbox_log.log_index,
        bridge_log_index: receipt.bridge_log.log_index,
        destination_chain_id: expected.destination_chain_id,
        destination_transaction_hash: format!("{child_hash:#x}"),
    })
}

/// Verifies the system-generated child transaction corresponding to one
/// finalized parent deposit. Type 0x64 credits native balance without executing
/// code at the recipient, so no recipient-code execution assumption applies.
pub fn verify_eth_deposit_child(
    transaction: &CanonicalChildTransaction,
    receipt: &CanonicalChildReceipt,
    expected: &VerifiedEthDeposit,
) -> Result<VerifiedEthDepositReceipt> {
    let sender = parse_address(&transaction.from)?;
    let to = parse_address(&transaction.to)?;
    let expected_source = parse_address(&expected.source_address)?;
    let expected_sender = apply_l1_to_l2_alias(expected_source);
    let expected_recipient = parse_address(&expected.recipient)?;
    let expected_amount = quantity_u256(&expected.amount)?;
    let message_index = quantity_u256(&expected.message_index)?;
    let expected_hash = B256::from_str(&expected.destination_transaction_hash)
        .map_err(|_| proof_error("Saved canonical child transaction hash is malformed."))?;
    let derived_hash = derive_eth_deposit_tx_hash(
        expected.destination_chain_id,
        message_index,
        expected_sender,
        expected_recipient,
        expected_amount,
    );
    let mut packed_data = Vec::with_capacity(52);
    packed_data.extend_from_slice(expected_recipient.as_slice());
    packed_data.extend_from_slice(expected_amount.to_be_bytes::<32>().as_slice());
    let saved_message_data = decode_hex(&expected.message_data).ok();
    ensure(
        (expected.destination_chain_id == ARBITRUM_CHAIN_ID
            || expected.destination_chain_id == ROBINHOOD_CHAIN_ID)
            && expected_source == expected_recipient
            && expected
                .aliased_sender
                .eq_ignore_ascii_case(&format!("{expected_sender:#x}"))
            && saved_message_data.as_deref() == Some(packed_data.as_slice())
            && B256::from_str(&expected.message_data_hash).ok() == Some(keccak256(&packed_data))
            && derived_hash == expected_hash
            && B256::from_str(&transaction.hash).ok() == Some(expected_hash)
            && B256::from_str(&receipt.transaction_hash).ok() == Some(expected_hash)
            && transaction.chain_id == expected.destination_chain_id
            && transaction.transaction_type == 0x64
            && sender == expected_sender
            && to == expected_recipient
            && quantity_u256(&transaction.value)? == expected_amount,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Child transaction fields do not match the derived canonical ETH deposit identity.",
    )?;
    ensure(
        receipt.success,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Canonical child deposit receipt did not succeed.",
    )?;
    Ok(VerifiedEthDepositReceipt {
        transaction_hash: format!("{expected_hash:#x}"),
        chain_id: expected.destination_chain_id,
        sender: format!("{sender:#x}"),
        recipient: format!("{to:#x}"),
        amount: expected.amount.clone(),
        message_index: expected.message_index.clone(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalChildTransaction {
    pub hash: String,
    pub chain_id: u64,
    pub transaction_type: u8,
    pub from: String,
    pub to: String,
    pub value: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalChildReceipt {
    pub transaction_hash: String,
    pub success: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedEthDepositReceipt {
    pub transaction_hash: String,
    pub chain_id: u64,
    pub sender: String,
    pub recipient: String,
    pub amount: Quantity,
    pub message_index: Quantity,
}

/// SDK-compatible Arbitrum deposit transaction id:
/// `keccak256(0x64 || RLP([chainId,minimal-u, messageNum[32], from[20],
/// to[20], value[minimal-u]]))`.
pub fn derive_eth_deposit_tx_hash(
    child_chain_id: u64,
    message_number: U256,
    from: Address,
    to: Address,
    value: U256,
) -> B256 {
    let fields = [
        rlp_bytes(&minimal_integer(U256::from(child_chain_id))),
        rlp_bytes(&message_number.to_be_bytes::<32>()),
        rlp_bytes(from.as_slice()),
        rlp_bytes(to.as_slice()),
        rlp_bytes(&minimal_integer(value)),
    ];
    let payload: Vec<u8> = fields.into_iter().flatten().collect();
    let mut encoded = rlp_list(&payload);
    encoded.insert(0, 0x64);
    keccak256(encoded)
}

fn apply_l1_to_l2_alias(address: Address) -> Address {
    const ALIAS_OFFSET: [u8; 20] = [
        0x11, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x11, 0x11,
    ];
    let value = U256::from_be_slice(address.as_slice());
    let offset = U256::from_be_slice(&ALIAS_OFFSET);
    Address::from_slice((value.overflowing_add(offset).0).to_be_bytes::<32>()[12..].as_ref())
}

fn minimal_integer(value: U256) -> Vec<u8> {
    let bytes = value.to_be_bytes::<32>();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    bytes[first..].to_vec()
}

fn rlp_bytes(value: &[u8]) -> Vec<u8> {
    if value.len() == 1 && value[0] < 0x80 {
        return value.to_vec();
    }
    if value.len() <= 55 {
        let mut encoded = Vec::with_capacity(value.len() + 1);
        encoded.push(0x80 + value.len() as u8);
        encoded.extend_from_slice(value);
        return encoded;
    }
    let length = minimal_integer(U256::from(value.len()));
    let mut encoded = Vec::with_capacity(value.len() + length.len() + 1);
    encoded.push(0xb7 + length.len() as u8);
    encoded.extend_from_slice(&length);
    encoded.extend_from_slice(value);
    encoded
}

fn rlp_list(payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(payload.len() + 3);
    if payload.len() <= 55 {
        encoded.push(0xc0 + payload.len() as u8);
    } else {
        let length = minimal_integer(U256::from(payload.len()));
        encoded.push(0xf7 + length.len() as u8);
        encoded.extend_from_slice(&length);
    }
    encoded.extend_from_slice(payload);
    encoded
}

fn parse_topics(topics: &[String], expected: usize) -> Result<Vec<B256>> {
    ensure(
        topics.len() == expected,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Canonical event has an unexpected indexed topic count.",
    )?;
    topics.iter().map(|topic| parse_b256(topic)).collect()
}

fn parse_b256(value: &str) -> Result<B256> {
    B256::from_str(value).map_err(|_| proof_error("Expected an exact 32-byte event topic."))
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    hex::decode(
        value
            .strip_prefix("0x")
            .ok_or_else(|| proof_error("Canonical event data is missing its 0x prefix."))?,
    )
    .map_err(|_| proof_error("Canonical event data is not valid hex."))
}

fn decode_inbox_data(value: &str) -> Result<Vec<u8>> {
    let data = decode_hex(value)?;
    ensure(
        data.len() == 128
            && U256::from_be_slice(&data[..32]) == U256::from(32u8)
            && U256::from_be_slice(&data[32..64]) == U256::from(52u8)
            && data[116..].iter().all(|byte| *byte == 0),
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "InboxMessageDelivered data must be canonical ABI encoding of exactly 52 packed ETH-deposit bytes.",
    )?;
    Ok(data[64..116].to_vec())
}

fn word_address(word: &[u8]) -> Result<Address> {
    ensure(
        word.len() == 32 && word[..12].iter().all(|byte| *byte == 0),
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Canonical event address word is not ABI-canonical.",
    )?;
    Ok(Address::from_slice(&word[12..]))
}

fn word_u256(word: &[u8]) -> Result<U256> {
    ensure(
        word.len() == 32,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Canonical event uint word is malformed.",
    )?;
    Ok(U256::from_be_slice(word))
}

fn proof_error(message: &str) -> FinError {
    bridge_error(
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        message,
        "Keep the transfer unresolved and inspect the original canonical source receipt.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BridgeProvider, BridgeQuoteRequest};
    use async_trait::async_trait;
    use fin_domain::ResourceId;
    use fin_evm::EvmRpc;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    struct SyntheticRpc {
        requests: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl EvmRpc for SyntheticRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests.lock().unwrap().push((method.into(), params));
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_getBalance" => json!("0x3635c9adc5dea00000"),
                "eth_call" => json!("0x"),
                "eth_estimateGas" => json!("0x5208"),
                "eth_getBlockByNumber" => json!({"baseFeePerGas":"0x3b9aca00"}),
                "eth_maxPriorityFeePerGas" => json!("0x3b9aca00"),
                "eth_getTransactionCount" => json!("0x0"),
                other => panic!("unexpected synthetic RPC method {other}"),
            })
        }
    }

    fn canonical_provider() -> (CanonicalProvider, Arc<SyntheticRpc>) {
        let rpc = Arc::new(SyntheticRpc {
            requests: Mutex::new(Vec::new()),
        });
        let runtime = Arc::new(EvmRuntime::new(rpc.clone(), SOURCE_CHAIN_ID, None));
        (
            CanonicalProvider::new(runtime, CanonicalConfig::reviewed()).unwrap(),
            rpc,
        )
    }

    #[tokio::test]
    async fn routes_disclose_existing_native_inbox_scope_without_quote_or_rpc() {
        let (provider, rpc) = canonical_provider();
        for destination in [42161, 4663] {
            let request = crate::BridgeRouteRequest {
                source: EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                },
                destination_chain_id: destination,
                destination_asset: None,
            };
            let routes = provider.discover_routes(&request).await.unwrap();
            assert_eq!(routes.availability, crate::RouteAvailability::Candidate);
            assert_eq!(routes.candidates.len(), 1);
            assert_eq!(routes.candidates[0].destination.chain_id, destination);
            let mut token = request.clone();
            token.source.token = Some("0x1111111111111111111111111111111111111111".into());
            assert_eq!(
                provider.discover_routes(&token).await.unwrap().availability,
                crate::RouteAvailability::Unavailable
            );
        }
        let reverse = crate::BridgeRouteRequest {
            source: EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            },
            destination_chain_id: 1,
            destination_asset: None,
        };
        assert_eq!(
            provider
                .discover_routes(&reverse)
                .await
                .unwrap()
                .availability,
            crate::RouteAvailability::Unavailable
        );
        assert!(rpc.requests.lock().unwrap().is_empty());
    }

    fn native_request(destination: u64) -> BridgeQuoteRequest {
        let wallet = "0x1111111111111111111111111111111111111111";
        BridgeQuoteRequest {
            cctp: None,
            intent: BridgeIntent {
                wallet: ResourceId("ethereum-wallet".into()),
                source_address: wallet.into(),
                source: EvmAsset {
                    chain_id: SOURCE_CHAIN_ID,
                    token: None,
                    decimals: 18,
                },
                destination_chain_id: destination,
                destination_assets: vec![EvmAsset {
                    chain_id: destination,
                    token: None,
                    decimals: 18,
                }],
                recipient: wallet.into(),
                refund_to: wallet.into(),
                allowed_refund_assets: vec![],
                amount: Quantity::new("1000000000000000").unwrap(),
            },
            bounds: BridgeBounds::default(),
        }
    }

    // Independent vectors generated with @arbitrum/sdk@4.1.0 from
    // EthDepositMessage.calculateDepositTxId (npm integrity is pinned in
    // .context/canonical-sdk-reference/provenance.json).
    #[test]
    fn child_hash_matches_independent_arbitrum_sdk_vectors() {
        let sender = parse_address("0x1111111111111111111111111111111111111111").unwrap();
        let recipient = parse_address("0x2222222222222222222222222222222222222222").unwrap();
        let value = U256::from(1_000_000_000_000_000u64);
        let vectors = [
            (
                42161,
                U256::ZERO,
                "0x564e5b91b2130768281c4d4133bff8fb7cc89f9d93350a15be80b06fa4221074",
            ),
            (
                42161,
                U256::from(257u64),
                "0xa4d8731607fca64f4c4be39ff09af49e7a388315dd9f36aea9a86e3c22ad839f",
            ),
            (
                4663,
                U256::ZERO,
                "0x606524b43e8d30c62f2ed653cabde42e1893b4cfe661ce3d2193c85c58ccb5b7",
            ),
            (
                4663,
                U256::from(257u64),
                "0x903abf737e00e6b8cdeb779420326cd9e04c76a4c3d90aa77b84926983b87ab3",
            ),
        ];
        for (chain, number, expected) in vectors {
            assert_eq!(
                format!(
                    "{:#x}",
                    derive_eth_deposit_tx_hash(chain, number, sender, recipient, value)
                ),
                expected
            );
        }
    }

    #[tokio::test]
    async fn quote_and_prepare_are_one_reviewed_native_inbox_deposit() {
        let (provider, rpc) = canonical_provider();
        for destination in [ARBITRUM_CHAIN_ID, ROBINHOOD_CHAIN_ID] {
            let request = native_request(destination);
            let batch = provider.quote(&request).await.unwrap();
            assert!(batch.failures.is_empty());
            let quote = &batch.quotes[0];
            assert_eq!(quote.provider, PROVIDER_ID);
            assert!(quote.protocol_order_id.is_none());
            assert!(quote.provider_request_id.is_none());
            assert!(quote.order_solver.is_none());
            assert_eq!(quote.expected_receipt.atoms, request.intent.amount);
            assert_eq!(quote.minimum_receipt.atoms, request.intent.amount);
            assert_eq!(quote.estimated_arrival_seconds, Some(600));
            assert_eq!(quote.protocol_steps.len(), 1);
            assert_eq!(quote.protocol_steps[0].kind, BridgeStepKind::Deposit);
            assert_eq!(quote.protocol_steps[0].call.value, request.intent.amount);
            assert_eq!(quote.protocol_steps[0].call.data, "0x439370b1");
            assert_eq!(quote.protocol_steps[0].call.chain_id, SOURCE_CHAIN_ID);
            assert_eq!(
                quote.protocol_steps[0].call.to.to_ascii_lowercase(),
                CanonicalConfig::reviewed().deployments[&destination]
                    .inbox
                    .to_ascii_lowercase()
            );
            let prepared = provider
                .prepare(
                    quote,
                    &BridgeBounds::default(),
                    &request.intent.source_address,
                )
                .unwrap();
            assert!(prepared.source_order_id.is_none());
            assert_eq!(prepared.steps.len(), 1);
            assert_eq!(prepared.steps[0].call, quote.protocol_steps[0].call);
            let mut expired = quote.clone();
            expired.valid_until = (Utc::now() - ChronoDuration::seconds(1)).to_rfc3339();
            assert!(
                provider
                    .prepare(
                        &expired,
                        &BridgeBounds::default(),
                        &request.intent.source_address
                    )
                    .is_err()
            );
            let mut tampered = quote.clone();
            tampered.protocol_steps[0].call.data = "0xdeadbeef".into();
            assert!(
                provider
                    .prepare(
                        &tampered,
                        &BridgeBounds::default(),
                        &request.intent.source_address
                    )
                    .is_err()
            );
        }
        let calls = rpc.requests.lock().unwrap();
        assert!(
            calls
                .iter()
                .all(|(method, _)| method != "eth_sendRawTransaction")
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "eth_estimateGas")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn exact_deposit_accepts_lower_receipt_floor_and_refreshed_amount_in_range() {
        let (provider, _) = canonical_provider();
        let request = native_request(ARBITRUM_CHAIN_ID);
        let mut quote = provider.quote(&request).await.unwrap().quotes.remove(0);
        let original = quote.intent.amount.clone();
        let original_atoms = quantity_u256(&original).unwrap();
        let floor_atoms = original_atoms * U256::from(90u8) / U256::from(100u8);
        let refreshed_atoms = original_atoms * U256::from(98u8) / U256::from(100u8);
        let minimum_input = refreshed_atoms;
        let mut bounds = BridgeBounds {
            input: Some(crate::AmountInterval {
                minimum: quantity(minimum_input),
                maximum: original.clone(),
            }),
            maximum_source_debit: Some(original.clone()),
            minimum_net_receipt: Some(quote.expected_receipt.clone()),
            ..BridgeBounds::default()
        };
        bounds.minimum_net_receipt.as_mut().unwrap().atoms = quantity(floor_atoms);

        // Exact delivery exceeds this weaker user-selected floor.
        validate_quote_bounds(&quote, &bounds).unwrap();
        let fixed_scope = scope_digest(&quote, &bounds, &request.intent.source_address).unwrap();

        // Refresh at 98% while preserving the exact principal and receipt; the
        // new amount remains inside the already-authorized input interval.
        quote.intent.amount = quantity(refreshed_atoms);
        quote.gross_debit.atoms = quantity(refreshed_atoms);
        quote.expected_receipt.atoms = quantity(refreshed_atoms);
        quote.minimum_receipt.atoms = quantity(refreshed_atoms);
        quote.protocol_steps[0].call.value = quantity(refreshed_atoms);
        quote.protocol_steps[0].maximum_debit[0].atoms = quantity(refreshed_atoms);
        validate_quote_bounds(&quote, &bounds).unwrap();
        assert_eq!(
            scope_digest(&quote, &bounds, &request.intent.source_address).unwrap(),
            fixed_scope
        );

        bounds.minimum_net_receipt.as_mut().unwrap().atoms =
            quantity(refreshed_atoms + U256::from(1u8));
        assert!(validate_quote_bounds(&quote, &bounds).is_err());
        bounds.minimum_net_receipt.as_mut().unwrap().atoms = Quantity::from_u64(0);
        assert!(validate_quote_bounds(&quote, &bounds).is_err());
    }

    #[tokio::test]
    async fn provider_rejects_asset_scope_recipient_and_configuration_drift() {
        let (provider, _) = canonical_provider();
        let mut request = native_request(ARBITRUM_CHAIN_ID);
        request.intent.recipient = "0x2222222222222222222222222222222222222222".into();
        assert!(provider.quote(&request).await.is_err());
        let mut request = native_request(ARBITRUM_CHAIN_ID);
        request.intent.destination_assets[0].token =
            Some("0x3333333333333333333333333333333333333333".into());
        assert!(provider.quote(&request).await.is_err());
        let mut config = CanonicalConfig::reviewed();
        config
            .deployments
            .get_mut(&ARBITRUM_CHAIN_ID)
            .unwrap()
            .inbox = "0x2222222222222222222222222222222222222222".into();
        assert!(
            CanonicalProvider::new(
                Arc::new(EvmRuntime::new(
                    Arc::new(SyntheticRpc {
                        requests: Mutex::new(Vec::new()),
                    }),
                    SOURCE_CHAIN_ID,
                    None
                )),
                config
            )
            .is_err()
        );
        let signer =
            Arc::new(fin_evm::LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let signing_runtime = Arc::new(EvmRuntime::new(
            Arc::new(SyntheticRpc {
                requests: Mutex::new(Vec::new()),
            }),
            SOURCE_CHAIN_ID,
            Some(signer),
        ));
        assert!(CanonicalProvider::new(signing_runtime, CanonicalConfig::reviewed()).is_err());
    }

    #[tokio::test]
    async fn canonical_accepts_permitted_refund_assets_without_promising_a_refund_path() {
        let (provider, _) = canonical_provider();
        let mut request = native_request(ARBITRUM_CHAIN_ID);
        request.intent.allowed_refund_assets = vec![
            EvmAsset {
                chain_id: SOURCE_CHAIN_ID,
                token: None,
                decimals: 18,
            },
            EvmAsset {
                chain_id: ARBITRUM_CHAIN_ID,
                token: None,
                decimals: 18,
            },
        ];
        let batch = provider.quote(&request).await.unwrap();
        assert_eq!(batch.quotes.len(), 1);
        assert!(batch.quotes[0].refund_paths.is_empty());
        assert_eq!(batch.quotes[0].intent.allowed_refund_assets.len(), 2);
        assert_eq!(
            batch.quotes[0].intent.refund_to,
            batch.quotes[0].intent.source_address
        );
    }

    fn address_word(address: Address) -> [u8; 32] {
        address.into_word().into()
    }

    fn uint_word(value: U256) -> [u8; 32] {
        value.to_be_bytes::<32>()
    }

    fn sample_receipt(
        inbox: Address,
        bridge: Address,
        source: Address,
        recipient: Address,
        amount: U256,
        message_index: U256,
    ) -> CanonicalDepositReceipt {
        let mut packed = Vec::new();
        packed.extend_from_slice(recipient.as_slice());
        packed.extend_from_slice(amount.to_be_bytes::<32>().as_slice());
        let mut inbox_data = Vec::new();
        inbox_data.extend_from_slice(&uint_word(U256::from(32u8)));
        inbox_data.extend_from_slice(&uint_word(U256::from(52u8)));
        inbox_data.extend_from_slice(&packed);
        inbox_data.extend_from_slice(&[0u8; 12]);
        let inbox_log = CanonicalRawLog {
            log_index: 4,
            address: format!("{inbox:#x}"),
            topics: vec![
                format!("{:#x}", keccak256("InboxMessageDelivered(uint256,bytes)")),
                format!("{:#x}", B256::from(message_index.to_be_bytes::<32>())),
            ],
            data: format!("0x{}", hex::encode(inbox_data)),
        };
        let alias = apply_l1_to_l2_alias(source);
        let mut bridge_data = Vec::new();
        bridge_data.extend_from_slice(&address_word(inbox));
        bridge_data.extend_from_slice(&uint_word(U256::from(12u8)));
        bridge_data.extend_from_slice(&address_word(alias));
        bridge_data.extend_from_slice(keccak256(&packed).as_slice());
        bridge_data.extend_from_slice(&uint_word(U256::from(1u8))); // baseFeeL1
        bridge_data.extend_from_slice(&uint_word(U256::from(2u8))); // timestamp
        let bridge_log = CanonicalRawLog {
            log_index: 5,
            address: format!("{bridge:#x}"),
            topics: vec![
                format!(
                    "{:#x}",
                    keccak256(
                        "MessageDelivered(uint256,bytes32,address,uint8,address,bytes32,uint256,uint64)"
                    )
                ),
                format!("{:#x}", B256::from(message_index.to_be_bytes::<32>())),
                format!("{:#x}", B256::repeat_byte(0x55)), // beforeInboxAcc
            ],
            data: format!("0x{}", hex::encode(bridge_data)),
        };
        CanonicalDepositReceipt {
            inbox_log,
            bridge_log,
        }
    }

    fn expected(
        source: Address,
        recipient: Address,
        inbox: Address,
        bridge: Address,
    ) -> ExpectedEthDeposit {
        ExpectedEthDeposit {
            source_address: format!("{source:#x}"),
            recipient: format!("{recipient:#x}"),
            amount: Quantity::new("1000000000000000").unwrap(),
            destination_chain_id: ARBITRUM_CHAIN_ID,
            inbox: format!("{inbox:#x}"),
            bridge: format!("{bridge:#x}"),
        }
    }

    #[test]
    fn paired_source_events_prove_exact_deposit_and_child_identity() {
        let source = parse_address("0x1111111111111111111111111111111111111111").unwrap();
        let recipient = source;
        let inbox = parse_address(&CanonicalConfig::reviewed().deployments[&42161].inbox).unwrap();
        let bridge =
            parse_address(&CanonicalConfig::reviewed().deployments[&42161].bridge).unwrap();
        let expected = expected(source, recipient, inbox, bridge);
        let receipt = sample_receipt(
            inbox,
            bridge,
            source,
            recipient,
            U256::from(1_000_000_000_000_000u64),
            U256::from(257u64),
        );
        let verified = verify_eth_deposit_source(&receipt, &expected).unwrap();
        assert_eq!(verified.message_index, Quantity::from_u64(257));
        assert_eq!(verified.inbox_log_index, 4);
        assert_eq!(verified.bridge_log_index, 5);
        assert_eq!(
            verified.aliased_sender,
            format!("{:#x}", apply_l1_to_l2_alias(source))
        );
        assert_eq!(verified.amount, expected.amount);
        assert_eq!(
            verified.message_data_hash,
            format!(
                "{:#x}",
                keccak256(hex::decode(verified.message_data.trim_start_matches("0x")).unwrap())
            )
        );

        let child_tx = CanonicalChildTransaction {
            hash: verified.destination_transaction_hash.clone(),
            chain_id: ARBITRUM_CHAIN_ID,
            transaction_type: 0x64,
            from: verified.aliased_sender.clone(),
            to: verified.recipient.clone(),
            value: verified.amount.clone(),
        };
        let child_receipt = CanonicalChildReceipt {
            transaction_hash: verified.destination_transaction_hash.clone(),
            success: true,
        };
        assert!(verify_eth_deposit_child(&child_tx, &child_receipt, &verified).is_ok());
    }

    #[test]
    fn paired_source_and_child_proofs_reject_tampered_fields() {
        let source = parse_address("0x1111111111111111111111111111111111111111").unwrap();
        let recipient = source;
        let config = CanonicalConfig::reviewed();
        let inbox = parse_address(&config.deployments[&42161].inbox).unwrap();
        let bridge = parse_address(&config.deployments[&42161].bridge).unwrap();
        let expected = expected(source, recipient, inbox, bridge);
        let receipt = sample_receipt(
            inbox,
            bridge,
            source,
            recipient,
            U256::from(1_000_000_000_000_000u64),
            U256::from(257u64),
        );
        let verified = verify_eth_deposit_source(&receipt, &expected).unwrap();

        let mut bad = receipt.clone();
        bad.bridge_log.topics[1] = format!("{:#x}", B256::repeat_byte(1));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        bad.bridge_log.address = format!("{:#x}", Address::from_slice(&[0x99; 20]));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        let mut bytes = hex::decode(bad.bridge_log.data.trim_start_matches("0x")).unwrap();
        bytes[63] = 11; // MessageDelivered kind must be ETH deposit kind 12.
        bad.bridge_log.data = format!("0x{}", hex::encode(bytes));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        let mut bytes = hex::decode(bad.bridge_log.data.trim_start_matches("0x")).unwrap();
        bytes[95] ^= 1; // Bridge sender must be the source wallet's L1→L2 alias.
        bad.bridge_log.data = format!("0x{}", hex::encode(bytes));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        bad.bridge_log.topics.pop(); // beforeInboxAcc is an indexed argument.
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        let mut bytes = hex::decode(bad.inbox_log.data.trim_start_matches("0x")).unwrap();
        bytes[64] ^= 1; // Recipient bytes are inside the hashed packed payload.
        bad.inbox_log.data = format!("0x{}", hex::encode(bytes));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());
        let mut bad = receipt.clone();
        let mut bytes = hex::decode(bad.inbox_log.data.trim_start_matches("0x")).unwrap();
        bytes[31] = 33; // Dynamic offset must be the canonical 32.
        bad.inbox_log.data = format!("0x{}", hex::encode(bytes));
        assert!(verify_eth_deposit_source(&bad, &expected).is_err());

        let tx = CanonicalChildTransaction {
            hash: verified.destination_transaction_hash.clone(),
            chain_id: ARBITRUM_CHAIN_ID,
            transaction_type: 0x64,
            from: verified.aliased_sender.clone(),
            to: verified.recipient.clone(),
            value: verified.amount.clone(),
        };
        let rcpt = CanonicalChildReceipt {
            transaction_hash: verified.destination_transaction_hash.clone(),
            success: true,
        };
        let mut bad = tx.clone();
        bad.transaction_type = 2;
        assert!(verify_eth_deposit_child(&bad, &rcpt, &verified).is_err());
        let mut bad = tx.clone();
        bad.chain_id = ROBINHOOD_CHAIN_ID;
        assert!(verify_eth_deposit_child(&bad, &rcpt, &verified).is_err());
        let mut bad = tx.clone();
        bad.value = Quantity::from_u64(1);
        assert!(verify_eth_deposit_child(&bad, &rcpt, &verified).is_err());
        let mut bad = rcpt;
        bad.success = false;
        assert!(verify_eth_deposit_child(&tx, &bad, &verified).is_err());
    }
}
