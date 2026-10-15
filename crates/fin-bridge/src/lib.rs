//! Typed bridge adapter contracts and provider implementations.
//! This crate prepares constrained candidates; broker policy and Fin's EVM
//! runtime remain responsible for authorization, signing, submission and proof.
use async_trait::async_trait;
use fin_domain::{Amount, AssetId, FinError, Quantity, ResourceId, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub mod base_canonical;
pub mod canonical;
pub mod cctp;
pub mod relay;
pub mod routes;
pub use routes::*;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvmAsset {
    pub chain_id: u64,
    /// `None` means native ETH. ERC-20 identities are exact lowercase addresses.
    pub token: Option<String>,
    pub decimals: u8,
}
/// Exact Solana mainnet asset identity. `None` is native SOL; an SPL or
/// Token-2022 mint is case-sensitive and its decimals come from RPC evidence.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaAsset {
    pub network: String,
    pub mint: Option<String>,
    pub decimals: u8,
}
impl SolanaAsset {
    pub fn asset_id(&self) -> AssetId {
        AssetId {
            network: self.network.clone(),
            reference: self.mint.clone().unwrap_or_else(|| "native".into()),
            decimals: self.decimals,
        }
    }
    pub fn amount(&self, atoms: Quantity) -> Amount {
        Amount {
            asset: self.asset_id(),
            atoms,
        }
    }
}
impl EvmAsset {
    pub fn asset_id(&self) -> AssetId {
        AssetId {
            network: format!("ethereum:{}", self.chain_id),
            reference: self.token.clone().unwrap_or_else(|| "native".into()),
            decimals: self.decimals,
        }
    }
    pub fn amount(&self, atoms: Quantity) -> Amount {
        Amount {
            asset: self.asset_id(),
            atoms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeIntent {
    pub wallet: ResourceId,
    pub source_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EvmAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_source: Option<SolanaAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_chain_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_destination: Option<SolanaDestination>,
    #[serde(default)]
    pub destination_assets: Vec<EvmAsset>,
    pub recipient: String,
    pub refund_to: String,
    pub allowed_refund_assets: Vec<EvmAsset>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_solana_refund_assets: Vec<SolanaAsset>,
    pub amount: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmountInterval {
    pub minimum: Quantity,
    pub maximum: Quantity,
}

/// All caps are explicit and asset-denominated. USD caps use integer microdollars;
/// a caller must supply independent Fin pricing evidence before policy can enforce them.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct BridgeBounds {
    pub input: Option<AmountInterval>,
    pub maximum_source_debit: Option<Quantity>,
    pub input_usd_micros: Option<AmountInterval>,
    pub source_usd_per_unit_micros: Option<AmountInterval>,
    pub minimum_net_receipt: Option<Amount>,
    pub minimum_output_input_ratio: Option<RatioBound>,
    pub fee_caps: Vec<FeeCap>,
    pub maximum_total_usd_micros: Option<u64>,
    pub authorization_seconds: Option<u32>,
    pub latest_protocol_deadline: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RatioBound {
    pub numerator: Quantity,
    pub denominator: Quantity,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(PartialOrd, Ord, Hash)]
pub enum FeeKind {
    Gas,
    AdditionalDebit,
    WithheldFromOutput,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FeeCap {
    pub asset: AssetId,
    pub kind: FeeKind,
    pub maximum: Quantity,
    pub maximum_usd_micros: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuoteClass {
    Executable,
    Indicative,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeFee {
    pub amount: Amount,
    pub kind: FeeKind,
    pub payer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PricedBridgeFee {
    /// Exact quote fee identity; USD value must come from fresh Fin pricing.
    pub fee: BridgeFee,
    pub usd_micros: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefundPath {
    pub chain_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    pub asset: AssetId,
    pub recipient: String,
    pub minimum: Quantity,
    pub maximum: Quantity,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeQuote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cctp: Option<cctp::CctpQuoteMetadata>,
    pub quote_id: String,
    pub provider: String,
    pub route_id: String,
    pub intent: BridgeIntent,
    pub gross_debit: Amount,
    pub expected_receipt: Amount,
    pub minimum_receipt: Amount,
    pub fees: Vec<BridgeFee>,
    pub approval: Option<ApprovalRequirement>,
    pub protocol_steps: Vec<BridgeStep>,
    /// Provider estimate in integer seconds; it is not a finality promise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_arrival_seconds: Option<u64>,
    pub observed_at: String,
    pub valid_until: String,
    pub class: QuoteClass,
    pub refund_paths: Vec<RefundPath>,
    /// Relay order solver, when the selected provider has this concept. It is
    /// not a universal destination-transaction sender identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_solver: Option<String>,
    pub protocol_order_id: Option<String>,
    pub provider_request_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequirement {
    pub token: String,
    pub spender: String,
    pub amount: Quantity,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStepKind {
    Approval,
    #[serde(rename = "reset_allowance")]
    ResetAllowance,
    Deposit,
    Prove,
    Finalize,
    Mint,
}
impl BridgeStepKind {
    /// Stable engine input spelling. Every concrete attempt continues to use
    /// `bridge.transfer`; this value only narrows which reviewed step is run.
    pub const fn operation_step(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::ResetAllowance => "reset_allowance",
            Self::Deposit => "deposit",
            Self::Prove => "prove",
            Self::Finalize => "finalize",
            Self::Mint => "mint",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvmCall {
    pub chain_id: u64,
    pub to: String,
    pub value: Quantity,
    pub data: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeStep {
    pub ordinal: u8,
    pub kind: BridgeStepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<EvmCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_mint: Option<SolanaMintSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_transaction: Option<SolanaSourceTransaction>,
    pub expected_sender: String,
    pub maximum_debit: Vec<Amount>,
    pub dependencies: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaSourceTransaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_base64: Option<String>,
    #[serde(default)]
    #[schemars(with = "Vec<SolanaInstructionSchema>")]
    pub instructions: Vec<fin_solana::NormalizedInstruction>,
    #[serde(default)]
    pub lookup_tables: Vec<String>,
    pub source_address: String,
    pub order_id: String,
    pub request_id: String,
    pub mint: Option<String>,
    pub amount_atoms: Quantity,
    pub max_network_fee_lamports: u64,
    pub max_rent_lamports: u64,
    pub last_valid_block_height: Option<u64>,
    pub expire_at: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaInstructionSchema {
    pub program_id: String,
    pub accounts: Vec<SolanaAccountMetaSchema>,
    pub data_base64: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaAccountMetaSchema {
    pub address: String,
    pub signer: bool,
    pub writable: bool,
}
/// Exact parent/step binding supplied with every engine-facing bridge attempt.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeStepBinding {
    pub transfer_id: String,
    pub step_id: String,
    pub step_kind: String,
}
impl BridgeStepBinding {
    pub fn new(
        transfer_id: impl Into<String>,
        step_id: impl Into<String>,
        kind: BridgeStepKind,
    ) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            step_id: step_id.into(),
            step_kind: kind.operation_step().into(),
        }
    }

    pub fn validate(
        &self,
        expected_transfer_id: &str,
        expected_step_id: &str,
        expected_kind: BridgeStepKind,
    ) -> Result<()> {
        ensure(
            self.transfer_id == expected_transfer_id
                && self.step_id == expected_step_id
                && self.step_kind == expected_kind.operation_step(),
            "BRIDGE_STEP_BINDING_INVALID",
            "prepare",
            "Bridge step input does not match its approved transfer, step identity, and kind.",
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparedBridge {
    pub quote: BridgeQuote,
    pub bounds: BridgeBounds,
    pub steps: Vec<BridgeStep>,
    /// Provider order identity when the route requires one. Canonical
    /// message-based routes can derive their destination locator from source
    /// proof instead of inventing an order ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_order_id: Option<String>,
    pub fixed_scope_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeCapabilities {
    pub provider: String,
    pub chains: BTreeSet<u64>,
    pub native_eth: bool,
    pub erc20: bool,
    pub protocol_versions: BTreeSet<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeQuoteRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cctp: Option<cctp::CctpOptions>,
    pub intent: BridgeIntent,
    pub bounds: BridgeBounds,
}

/// Public CLI input. `wallet` is a configured broker alias; the broker resolves
/// it to the physical `BridgeIntent.wallet` resource before provider access.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeQuoteInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cctp: Option<cctp::CctpOptions>,
    pub intent: BridgeQuoteIntentInput,
    #[serde(default)]
    pub bounds: BridgeBounds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_transfer_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeQuoteIntentInput {
    pub wallet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BridgeAssetInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_source: Option<SolanaAssetInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_chain_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_destination: Option<SolanaDestinationInput>,
    #[serde(default)]
    pub destination_assets: Vec<BridgeAssetInput>,
    pub recipient: String,
    pub refund_to: String,
    pub allowed_refund_assets: Vec<BridgeAssetInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_solana_refund_assets: Vec<SolanaAssetInput>,
    pub amount: Quantity,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeAssetInput {
    pub chain_id: u64,
    #[serde(default)]
    pub token: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SolanaAssetInput {
    pub network: String,
    #[serde(default)]
    pub mint: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeTransferInput {
    pub transfer_id: String,
    pub step_id: String,
    pub step_kind: fin_domain::BridgeStepKind,
    /// Human-selected execution bounds; quote-time hints alone grant no write scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<BridgeBounds>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeStatusInput {
    pub transfer_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeQuoteBatch {
    pub quotes: Vec<BridgeQuote>,
    pub failures: Vec<BridgeQuoteFailure>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeQuoteFailure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_asset: Option<EvmAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_destination_asset: Option<SolanaAsset>,
    pub error: FinError,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeTransferRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    pub source_chain_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_chain_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_network: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_locator: Option<fin_domain::BridgeDestinationLocator>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeEvidenceCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    pub kind: EvidenceKind,
    pub chain_id: u64,
    pub transaction_hash: String,
    pub correlation: Option<String>,
}
/// Relay status text and errors are diagnostics only. Candidate transactions
/// are locators; only Fin's independently verified finalized chain proof settles.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeReconciliation {
    pub status: String,
    pub failure: Option<Value>,
    pub refund_fail_reason: Option<Value>,
    pub candidates: Vec<BridgeEvidenceCandidate>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    SourceDeposit,
    DestinationFill,
    Refund,
    SettlementHub,
}

/// Checks USD bounds against fresh independent Fin prices supplied by the broker.
/// Every amount is microdollars or an atomic quantity; provider USD fields are ignored.
/// `fee_usd_micros` contains only incremental costs not already in source gross
/// debit (source gas and additional debits); exclude withheld-from-output fees.
pub fn validate_usd_bounds(
    bounds: &BridgeBounds,
    source: &Amount,
    source_price_micros_per_unit: u64,
    fee_usd_micros: &[u64],
) -> Result<()> {
    use num_bigint::BigUint;
    let scale = BigUint::from(10u8).pow(source.asset.decimals as u32);
    if let Some(interval) = &bounds.source_usd_per_unit_micros {
        let price = Quantity::from_u64(source_price_micros_per_unit);
        if price < interval.minimum || price > interval.maximum {
            return Err(FinError::new(
                "BRIDGE_BOUNDS_VIOLATED",
                "pricing",
                "Fresh independent source price falls outside the approved price interval.",
                "Refresh pricing or request broader bounds with new human authorization.",
            ));
        }
    }
    if let Some(interval) = &bounds.input_usd_micros {
        let value = source.atoms.integer() * BigUint::from(source_price_micros_per_unit);
        if value < interval.minimum.integer() * &scale
            || value > interval.maximum.integer() * &scale
        {
            return Err(FinError::new(
                "BRIDGE_BOUNDS_VIOLATED",
                "pricing",
                "Exact source USD value falls outside the approved USD input interval.",
                "Refresh pricing or request broader bounds with new human authorization.",
            ));
        }
    }
    if let Some(maximum) = bounds.maximum_total_usd_micros {
        let source_cost = (source.atoms.integer() * BigUint::from(source_price_micros_per_unit)
            + &scale
            - BigUint::from(1u8))
            / &scale;
        let fees = fee_usd_micros
            .iter()
            .fold(BigUint::from(0u8), |sum, fee| sum + BigUint::from(*fee));
        if source_cost + fees > BigUint::from(maximum) {
            return Err(FinError::new(
                "BRIDGE_BOUNDS_VIOLATED",
                "pricing",
                "Source value plus independently priced fees exceeds the approved total USD cap.",
                "Refresh pricing or request broader bounds with new human authorization.",
            ));
        }
    }
    Ok(())
}

/// Enforces per-fee USD caps using independently priced, exact quote fee entries.
/// Duplicate entries of one asset/kind are aggregated before comparison.
pub fn validate_fee_usd_caps(bounds: &BridgeBounds, fees: &[PricedBridgeFee]) -> Result<()> {
    use num_bigint::BigUint;
    for cap in bounds
        .fee_caps
        .iter()
        .filter(|cap| cap.maximum_usd_micros.is_some())
    {
        let total = fees
            .iter()
            .filter(|priced| priced.fee.amount.asset == cap.asset && priced.fee.kind == cap.kind)
            .fold(BigUint::from(0u8), |sum, priced| {
                sum + BigUint::from(priced.usd_micros)
            });
        if total > BigUint::from(cap.maximum_usd_micros.unwrap_or_default()) {
            return Err(FinError::new(
                "BRIDGE_BOUNDS_VIOLATED",
                "pricing",
                "Fresh independently priced fees exceed the approved per-asset, per-kind USD cap.",
                "Refresh pricing or request broader bounds with new human authorization.",
            ));
        }
    }
    Ok(())
}

#[async_trait]
pub trait BridgeProvider: Send + Sync {
    fn as_cctp(&self) -> Option<&cctp::CctpProvider> {
        None
    }
    fn capabilities(&self) -> BridgeCapabilities;
    async fn discover_routes(&self, _request: &BridgeRouteRequest) -> Result<BridgeRouteDiscovery> {
        Ok(BridgeRouteDiscovery::unsupported(
            self.capabilities().provider,
        ))
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch>;
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge>;
    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation>;
}

pub(crate) fn bridge_error(
    code: &str,
    stage: &str,
    message: impl Into<String>,
    next: &str,
) -> FinError {
    FinError::new(code, stage, message, next)
}
pub(crate) fn ensure(condition: bool, code: &str, stage: &str, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(bridge_error(
            code,
            stage,
            message,
            "Review the route or choose another supported quote.",
        ))
    }
}

/// Explicit non-EVM destination; the recipient owner remains part of the parent intent.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SolanaDestinationInput {
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    pub mint: Option<String>,
}

/// Broker-resolved immutable destination scope. Public keys are case-sensitive.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaDestination {
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    pub wallet_resource: ResourceId,
    pub wallet_address: String,
    pub recipient: String,
    pub asset: AssetId,
    pub recipient_token_account: String,
    pub destination_caller: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaMintSpec {
    pub destination: SolanaDestination,
    pub message_transmitter: String,
    pub token_messenger_minter: String,
}

impl BridgeTransferRef {
    pub fn evm_destination_chain_id(&self) -> Result<u64> {
        if self.destination_network.is_some() {
            return Err(FinError::validation(
                "This provider requires an EVM destination.",
            ));
        }
        self.destination_chain_id
            .ok_or_else(|| FinError::validation("An exact EVM destination chain is required."))
    }
}

impl BridgeIntent {
    pub fn evm_source(&self) -> Result<&EvmAsset> {
        if self.solana_source.is_some() {
            return Err(FinError::validation(
                "This bridge path requires an EVM source.",
            ));
        }
        self.source
            .as_ref()
            .ok_or_else(|| FinError::validation("An exact EVM source is required."))
    }
    pub fn source_network(&self) -> Result<String> {
        match (&self.source, &self.solana_source) {
            (Some(source), None) => Ok(format!("ethereum:{}", source.chain_id)),
            (None, Some(source)) if source.network == "solana:mainnet-beta" => {
                Ok(source.network.clone())
            }
            _ => Err(FinError::validation(
                "Select exactly one bridge source network.",
            )),
        }
    }
    pub fn source_asset_id(&self) -> Result<AssetId> {
        match (&self.source, &self.solana_source) {
            (Some(source), None) => Ok(source.asset_id()),
            (None, Some(source)) if source.network == "solana:mainnet-beta" => {
                Ok(source.asset_id())
            }
            _ => Err(FinError::validation(
                "Select exactly one bridge source asset.",
            )),
        }
    }
    pub fn source_amount(&self, atoms: Quantity) -> Result<Amount> {
        Ok(Amount {
            asset: self.source_asset_id()?,
            atoms,
        })
    }
    /// EVM-only providers must reject a non-EVM destination before interpreting addresses.
    pub fn evm_destination_chain_id(&self) -> Result<u64> {
        if self.solana_destination.is_some() {
            return Err(FinError::validation(
                "This bridge path requires an EVM destination.",
            ));
        }
        self.destination_chain_id
            .ok_or_else(|| FinError::validation("An exact EVM destination chain is required."))
    }
    pub fn destination_resource(&self) -> Result<ResourceId> {
        if let Some(destination) = &self.solana_destination {
            return Ok(destination.wallet_resource.clone());
        }
        Ok(ResourceId(format!(
            "ethereum:{}:{}",
            self.evm_destination_chain_id()?,
            self.recipient.to_ascii_lowercase()
        )))
    }
    pub fn destination_network(&self) -> Result<String> {
        if let Some(destination) = &self.solana_destination {
            return Ok(destination.network.clone());
        }
        Ok(format!("ethereum:{}", self.evm_destination_chain_id()?))
    }
}

impl BridgeStep {
    pub fn evm_call(&self) -> Result<&EvmCall> {
        if self.solana_mint.is_some() || self.solana_transaction.is_some() {
            return Err(FinError::validation(
                "This bridge stage uses Solana, not an EVM call.",
            ));
        }
        self.call
            .as_ref()
            .ok_or_else(|| FinError::validation("The bridge stage lacks its exact EVM call."))
    }
    pub fn evm_call_mut(&mut self) -> Result<&mut EvmCall> {
        if self.solana_mint.is_some() || self.solana_transaction.is_some() {
            return Err(FinError::validation(
                "This bridge stage uses Solana, not an EVM call.",
            ));
        }
        self.call
            .as_mut()
            .ok_or_else(|| FinError::validation("The bridge stage lacks its exact EVM call."))
    }
    pub fn gas_asset(&self) -> Result<AssetId> {
        if self.solana_transaction.is_some() {
            if self.call.is_some() || self.solana_mint.is_some() {
                return Err(FinError::validation(
                    "Bridge stage has ambiguous execution networks.",
                ));
            }
            return Ok(AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "native".into(),
                decimals: 9,
            });
        }
        if let Some(solana) = &self.solana_mint {
            if self.call.is_some() || solana.destination.network != "solana:mainnet-beta" {
                return Err(FinError::validation(
                    "Bridge stage has ambiguous execution networks.",
                ));
            }
            return Ok(AssetId {
                network: solana.destination.network.clone(),
                reference: "native".into(),
                decimals: 9,
            });
        }
        let call = self.evm_call()?;
        Ok(EvmAsset {
            chain_id: call.chain_id,
            token: None,
            decimals: 18,
        }
        .asset_id())
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{BridgeStepBinding, BridgeStepKind, BridgeTransferInput, BridgeTransferRef};

    #[test]
    fn engine_step_binding_is_exact_and_stable() {
        let binding =
            BridgeStepBinding::new("transfer-1", "step-2", BridgeStepKind::ResetAllowance);
        assert_eq!(binding.step_kind, "reset_allowance");
        assert!(
            binding
                .validate("transfer-1", "step-2", BridgeStepKind::ResetAllowance)
                .is_ok()
        );
        assert!(
            binding
                .validate("transfer-2", "step-2", BridgeStepKind::ResetAllowance)
                .is_err()
        );
        assert!(
            binding
                .validate("transfer-1", "step-2", BridgeStepKind::Deposit)
                .is_err()
        );
    }

    #[test]
    fn transfer_input_bounds_are_optional_and_camel_case() {
        let input: BridgeTransferInput = serde_json::from_value(serde_json::json!({
            "transferId": "transfer-1",
            "stepId": "step-1",
            "stepKind": "deposit"
        }))
        .unwrap();
        assert!(input.bounds.is_none());
        let encoded = serde_json::to_value(input).unwrap();
        assert!(encoded.get("bounds").is_none());
        assert!(encoded.get("transferId").is_some());
    }

    #[test]
    fn legacy_transfer_ref_json_keeps_order_ids_and_omits_new_locator() {
        let legacy = serde_json::json!({
            "order_id": "relay-order",
            "provider_request_id": "relay-request",
            "source_chain_id": 1,
            "destination_chain_id": 42161
        });
        let reference: BridgeTransferRef = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(reference.order_id.as_deref(), Some("relay-order"));
        assert_eq!(
            reference.provider_request_id.as_deref(),
            Some("relay-request")
        );
        assert!(reference.destination_locator.is_none());
        assert_eq!(serde_json::to_value(reference).unwrap(), legacy);
    }
}
