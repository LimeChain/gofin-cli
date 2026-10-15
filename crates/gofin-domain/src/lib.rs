//! Integration-independent contracts. Secrets and signed payloads never enter these types.
use crate::product_names::MetadataValueExt;
pub mod amendments;
pub mod authority_expiry;
pub mod boundaries;
pub mod clock;
pub mod compound_mutations;
pub mod conversion_sequences;
pub mod execution_progress;
pub mod execution_state;
pub mod financial_plans;
pub mod grouped_amendments;
pub mod money;
pub mod plan_timing;
pub mod policy;
pub mod price_freshness;
pub mod product_names;
pub mod submission_validation;
pub mod unpriced;
pub mod unresolved_consumption;
pub mod value_bounds;

#[cfg(test)]
mod settlement_progress_tests;

use async_trait::async_trait;
pub use money::{Quantity, SignedQuantity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
pub use submission_validation::{
    CandidateExpiryKind, RevalidatedOperation, VerifiedNonDispatchExpiry,
};

pub type Result<T> = std::result::Result<T, GoFinError>;
// New financial metadata uses GoFin. Exact legacy read aliases preserve old bindings.
/// Reviewed-adapter metadata in a prepared operation binding for one finite
/// bridge step; candidate inputs alone never establish this scope.
pub const BRIDGE_STEP_METADATA_KEY: &str = "__gofin_bridge_step";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct GoFinError {
    pub code: String,
    pub stage: String,
    pub message: String,
    pub next_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<Value>>,
}
impl GoFinError {
    pub fn new(code: &str, stage: &str, message: impl Into<String>, next_action: &str) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            message: message.into(),
            next_action: next_action.into(),
            details: None,
        }
    }
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(
            "INVALID_INPUT",
            "validation",
            message,
            "Inspect gofin schema for the selected command.",
        )
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(deny_unknown_fields)]
pub struct AssetId {
    pub network: String,
    pub reference: String,
    pub decimals: u8,
}

/// Resolved physical custody/signer identity, never merely a user-provided alias.
#[derive(
    Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(transparent)]
pub struct ResourceId(pub String);

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Amount {
    pub asset: AssetId,
    pub atoms: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceAmount {
    pub resource: ResourceId,
    pub amount: Amount,
}

/// A transfer outlives any single operation used to move its principal.
/// These contracts deliberately describe protocol-neutral persisted state.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeTransferState {
    Prepared,
    ApprovalPending,
    SourcePending,
    InTransit,
    DestinationPending,
    Completed,
    RefundPending,
    Refunded,
    Failed,
    NeedsAttention,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStepKind {
    Approval,
    ResetAllowance,
    Deposit,
    Prove,
    Finalize,
    Mint,
}

impl BridgeStepKind {
    pub const fn is_destination_stage(&self) -> bool {
        matches!(self, Self::Prove | Self::Finalize | Self::Mint)
    }
    pub const fn pending_state(&self) -> BridgeTransferState {
        match self {
            Self::Deposit => BridgeTransferState::SourcePending,
            Self::Prove | Self::Finalize | Self::Mint => BridgeTransferState::DestinationPending,
            Self::Approval | Self::ResetAllowance => BridgeTransferState::ApprovalPending,
        }
    }
    pub const fn settled_stage_state(&self) -> BridgeTransferState {
        if self.is_destination_stage() {
            BridgeTransferState::DestinationPending
        } else {
            BridgeTransferState::ApprovalPending
        }
    }
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Approval
            | Self::ResetAllowance
            | Self::Deposit
            | Self::Prove
            | Self::Finalize
            | Self::Mint => "bridge.transfer",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeQuoteRevision {
    pub revision: u32,
    pub provider: String,
    pub route_id: String,
    /// Some protocols expose an external order identity; direct canonical
    /// message routes may be identified only after source proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    pub quote_hash: String,
    /// Adapter-normalized exact quote, bounds and validation evidence. Opaque
    /// to shared accounting but durable for restart and reconciliation.
    pub payload: Value,
    pub observed_at: String,
    pub valid_until: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStepState {
    Unsubmitted,
    Submitted,
    Settled,
    Superseded,
    NeedsAttention,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeStepLink {
    pub transfer_id: String,
    pub grant_id: Option<String>,
    /// Named-plan ownership is separate from a finite boundary grant. It is
    /// assigned only alongside the plan attempt and concrete operation row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_authority: Option<BridgePlanAuthority>,
    pub step_id: String,
    /// Bound by Engine when it durably creates the concrete attempt.
    pub operation_id: Option<String>,
    pub kind: BridgeStepKind,
    pub depends_on: Vec<String>,
    pub state: BridgeStepState,
}

/// Identity of the approved plan step that owns one concrete bridge write.
/// Keeping this distinct from `grant_id` prevents plan consent from becoming
/// a standalone bridge grant or authorizing other links in the same transfer.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgePlanAuthority {
    pub plan_id: String,
    pub revision_id: String,
    pub plan_step_id: String,
}

/// Immutable plan/revision scope for a bridge parent. Individual finite writes
/// retain their own `BridgePlanAuthority` on the corresponding step link.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgePlanScope {
    pub plan_id: String,
    pub revision_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum BridgeAllocationOwner {
    Operation { operation_id: Option<String> },
    Transfer { transfer_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeSettlementOutcome {
    Destination,
    Refund,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeSettlementTarget {
    pub resource: ResourceId,
    pub asset: AssetId,
    pub minimum_atoms: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeSettlementExpectation {
    pub destination: Vec<BridgeSettlementTarget>,
    pub refund: Vec<BridgeSettlementTarget>,
}

/// A finite bridge plan's source-principal debit, which can be authorized
/// before its Deposit step is dispatched (for example, while approving a
/// preceding token allowance). This is reviewed scope, never an operation
/// effect or spendable resource.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeProspectiveDebit {
    pub deposit_step_id: String,
    pub resource: ResourceId,
    pub maximum: Amount,
}

/// Application-verified final destination/refund proof. Its exact typed
/// effects and evidence are persisted with the transfer and keyed by digest.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeSettlementProof {
    pub transfer_id: String,
    pub outcome: BridgeSettlementOutcome,
    #[serde(default)]
    pub evidence_id: String,
    pub effects: Vec<FinancialEffect>,
    pub evidence: Value,
    pub observed_at: String,
    pub proof_digest: String,
}

/// A destination transaction identity deterministically derived from a
/// finalized source proof. `proof` carries protocol-specific derivation data;
/// shared accounting stores it without interpreting the protocol.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeDestinationLocator {
    pub scheme: String,
    pub source_evidence_id: String,
    /// Legacy EVM destination identity. Exactly one destination identity is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_chain_id: Option<u64>,
    /// Non-EVM destination network, currently only Solana mainnet-beta for CCTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_network: Option<String>,
    pub transaction_hash: String,
    /// Deferred bridge messages do not determine a destination transaction hash.
    /// The scheme binds an immutable message digest instead; old locators omit it.
    /// CCTP V2 uses the normalized immutable burn digest because attestation
    /// fills protocol fields that were not known when the source emitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_hash: Option<String>,
    pub proof: Value,
}

/// Proven principal held outside both wallets while a bridge is unresolved.
/// It is intentionally not a `ResourceAmount` and cannot satisfy a debit.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransitResource {
    pub transfer_id: String,
    pub amount: Amount,
    #[serde(default)]
    pub evidence_id: String,
    /// Optional source-derived destination transaction locator. Older Relay
    /// records and transfers without deterministic destination IDs omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_locator: Option<BridgeDestinationLocator>,
    pub proof_digest: String,
    pub observed_at: String,
    pub spendable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BridgeTransferRecord {
    pub version: u32,
    pub id: String,
    /// Bound once after the human approves a finite bridge authorization.
    pub grant_id: Option<String>,
    /// Optional named-plan ownership, distinct from a boundary grant. A
    /// parent cannot be adopted by another plan or revision after dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_scope: Option<BridgePlanScope>,
    /// Exact original intent, fixed route identity and accepted bounds.
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub settlement_expectation: Option<BridgeSettlementExpectation>,
    /// Human-selected execution bounds frozen on the first authorized step.
    /// Provider quote refreshes never modify this scope.
    #[serde(default)]
    pub approved_bounds: Option<Value>,
    /// Exact finite source debit tied to a declared Deposit step. Legacy and
    /// non-bridge records omit it.
    #[serde(default)]
    pub prospective_debit: Option<BridgeProspectiveDebit>,
    pub state: BridgeTransferState,
    pub quote_revisions: Vec<BridgeQuoteRevision>,
    pub steps: Vec<BridgeStepLink>,
    pub allocation_owner: BridgeAllocationOwner,
    pub transit: Option<TransitResource>,
    /// Proof digests already applied to this transfer, for idempotent settlement.
    pub applied_proofs: Vec<String>,
    #[serde(default)]
    pub settlement_proofs: Vec<BridgeSettlementProof>,
}

pub fn bridge_settlement_digest(proof: &BridgeSettlementProof) -> Result<String> {
    binding_hash(&serde_json::json!({
        "transfer_id":proof.transfer_id,
        "outcome":proof.outcome,
        "evidence_id":proof.evidence_id,
        "effects":proof.effects,
        "evidence":proof.evidence,
        "observed_at":proof.observed_at,
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Read,
    Write,
    Configuration,
    Lifecycle,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionActor {
    AgentReadable,
    AgentInvocable,
    HumanInteractive,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    pub description: String,
    pub kind: OperationKind,
    pub execution_actor: ExecutionActor,
    pub input_schema: Value,
    pub output_schema: Value,
    pub error_schema: Value,
    pub examples: Vec<Value>,
    pub coverage: String,
    pub completion: String,
    pub semantics: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Unchecked,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Valuation {
    pub movement_usd_micros: Quantity,
    pub maximum_fee_usd_micros: Quantity,
    pub additional_allocation_usd_micros: Quantity,
    pub observed_at: String,
    pub source: String,
    pub prices: Vec<AssetPrice>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetPrice {
    pub asset: AssetId,
    pub usd_micros_per_unit: Quantity,
    pub unit_atoms: Quantity,
    pub observed_at: String,
    pub source: String,
}

/// A link identifies principal flow; application-owned receipt verification proves its cash/position legs.
#[derive(
    Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingLocation {
    Cash {
        resource: ResourceId,
    },
    Position {
        resource: ResourceId,
        position: String,
        position_kind: PositionKind,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Increase,
    Decrease,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountFundingLimit {
    /// Stable accounting position identity for this refundable account.
    pub position: String,
    pub maximum_amount: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RefundableFundingLimit {
    pub position: String,
    pub account: String,
    pub maximum_amount: Quantity,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountFundingState {
    Open,
    Closed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectConstraint {
    CompoundSubmissionMutations {
        children: Vec<compound_mutations::MutationChild>,
    },
    GroupedSubmissionAmendments {
        group: Box<grouped_amendments::GroupedAmendmentGroup>,
    },
    ConversionSequence {
        scope: conversion_sequences::ConversionSequenceConstraint,
    },
    /// A separately authorized request to remove the remainder of one durable submission.
    SubmissionCancellation {
        operation_id: String,
        submission: Submission,
    },
    /// Fresh exact approval for one linked authority revision, never a second fill owner.
    SubmissionAmendment {
        operation_id: String,
        submission: Submission,
        revision: Box<amendments::AmendmentScope>,
    },
    OriginMovement {
        from: AccountingLocation,
        to: AccountingLocation,
        debit_asset: AssetId,
        credit_asset: AssetId,
        maximum_debit: Quantity,
        minimum_credit: Quantity,
    },
    Accrual {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        maximum_change: Quantity,
    },
    Debit {
        resource: ResourceId,
        asset: AssetId,
        maximum_debit: Quantity,
    },
    Transfer {
        from: ResourceId,
        to: ResourceId,
        debit_asset: AssetId,
        credit_asset: AssetId,
        maximum_debit: Quantity,
        minimum_credit: Quantity,
    },
    /// A bounded noncustodial order or other protocol escrow lifecycle. The
    /// ratio is minimum output atoms per filled input atom; fees are reserved
    /// separately through `maximum_costs`.
    EscrowLifecycle {
        wallet_resource: ResourceId,
        escrow_resource: ResourceId,
        input_asset: AssetId,
        output_asset: AssetId,
        maximum_wallet_debit: Quantity,
        minimum_output_per_input_numerator: Quantity,
        minimum_output_per_input_denominator: Quantity,
    },
    /// A noncustodial escrow whose finalized fill becomes a typed position,
    /// rather than spendable wallet credit. Refunds remain wallet cash.
    EscrowToPosition {
        wallet_resource: ResourceId,
        escrow_resource: ResourceId,
        input_asset: AssetId,
        position_resource: ResourceId,
        position: String,
        position_kind: PositionKind,
        position_asset: AssetId,
        maximum_wallet_debit: Quantity,
        maximum_position_change: Quantity,
        minimum_position_per_input_numerator: Quantity,
        minimum_position_per_input_denominator: Quantity,
        /// Optional non-custodial exposure created only when some escrow
        /// input is finalized as filled. A no-fill cancellation proves zero.
        conditional_position: Option<EscrowConditionalPosition>,
    },
    /// A protocol request whose placement is pending and whose eventual
    /// keeper outcome may change only these exact, anchored positions. This
    /// does not represent wallet cash or escrow principal.
    PendingRequestLifecycle {
        wallet_resource: ResourceId,
        request_resource: ResourceId,
        request_id: String,
        request_kind: String,
        conditional_positions: Vec<PendingRequestPositionBound>,
        /// Optional cumulative wallet proceeds that may arise from a
        /// finalized keeper fill. They are never available while the request
        /// is merely placed; each positive progress row must pair with one
        /// exact UsableCredit to `wallet_resource`.
        #[serde(default)]
        conditional_credits: Vec<PendingRequestCreditBound>,
        /// Optional realized PnL events that may exist only after a finalized
        /// fill. Open/cancelled requests carry no rows.
        #[serde(default)]
        conditional_pnl: Vec<PendingRequestPnlBound>,
        /// Optional, explicitly reviewed reduce-only protection envelope.
        /// Shared position locks are never inferred from a request's name;
        /// adapters supply this only when the protocol binds the request to
        /// the exact anchored position and threshold.
        #[serde(default)]
        protection: Option<PendingRequestProtection>,
    },
    Conversion {
        resource: ResourceId,
        input: AssetId,
        maximum_input: Quantity,
        output: AssetId,
        minimum_output: Quantity,
    },
    Position {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        direction: Direction,
        maximum_change: Quantity,
    },
    /// A bounded typed position movement. Unlike the legacy `Position`
    /// constraint, this binds the exact protocol position kind.
    PositionOfKind {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        direction: Direction,
        maximum_change: Quantity,
    },
    /// Optional refundable account rent, for idempotent account-creation
    /// instructions that can succeed without funding when the account appears
    /// between preview and execution. When nonzero, adapters must emit a
    /// balanced cash debit, typed refundable position and origin link.
    OptionalAccountFunding {
        resource: ResourceId,
        asset: AssetId,
        accounts: Vec<AccountFundingLimit>,
    },
    /// Cumulative, receipt-backed lifecycle for rent-bearing accounts.
    AccountFundingLifecycle {
        resource: ResourceId,
        asset: AssetId,
        accounts: Vec<RefundableFundingLimit>,
    },
    /// Long/short exposure quantities, never custody or borrowed principal.
    DerivativePosition {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        minimum_delta: SignedQuantity,
        maximum_delta: SignedQuantity,
    },
    /// Native-asset segregated margin exposure; the corresponding loan remains separate debt.
    MarginPosition {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        minimum_delta: SignedQuantity,
        maximum_delta: SignedQuantity,
    },
    /// Realized collateral only; mark-to-market and unrealized PnL are excluded.
    Settlement {
        resource: ResourceId,
        asset: AssetId,
        category: SettlementCategory,
        minimum_delta: SignedQuantity,
        maximum_delta: SignedQuantity,
    },
    /// Bounds a finalized realized PnL event attached to an exact derivative
    /// position. This is not cash, a credit, or a budget debit; the proved
    /// wallet payout remains a separate effect.
    PositionRealizedPnl {
        resource: ResourceId,
        position: String,
        position_kind: PositionKind,
        asset: AssetId,
        maximum_loss: Quantity,
    },
    Debt {
        resource: ResourceId,
        debt: String,
        asset: AssetId,
        direction: Direction,
        maximum_change: Quantity,
    },
    Authority {
        resource: ResourceId,
        authority: String,
        before: String,
        after: String,
    },
    AuthorityTransition {
        resource: ResourceId,
        authority: String,
        before: String,
        allowed_after: Vec<String>,
    },
    Credit {
        to: ResourceId,
        asset: AssetId,
        minimum_credit: Quantity,
    },
    /// Binds a protocol fee's native denomination to its separately accounted
    /// typed Cost. This constraint creates no ledger movement or extra budget.
    CostDenomination {
        location: AccountingLocation,
        cost_asset: AssetId,
        category: String,
        treatment: CostTreatment,
        denomination_asset: AssetId,
        maximum_amount: Quantity,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialIntent {
    pub debits: Vec<ResourceAmount>,
    pub maximum_costs: Vec<CostBound>,
    pub expected: Vec<EffectConstraint>,
    pub locks: Vec<ResourceId>,
}

/// A user-described operation in a scoped run. It is a plan input only; each
/// operation still requires a fresh normal preview and authorization at execution.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub operation: String,
    pub input: Value,
    pub maximum_fee_usd_micros: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectedEffect {
    CashDelta {
        resource: ResourceId,
        asset: AssetId,
        minimum_delta: SignedQuantity,
        maximum_delta: SignedQuantity,
    },
    PositionDelta {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        minimum_delta: SignedQuantity,
        maximum_delta: SignedQuantity,
    },
    AuthorityValue {
        resource: ResourceId,
        authority: String,
        value: String,
    },
    CashSweep {
        resource: ResourceId,
        asset: AssetId,
        maximum_amount: Quantity,
        source_step: u32,
    },
    PositionClose {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        maximum_amount: Quantity,
        source_step: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuantityInterval {
    pub minimum: Quantity,
    pub maximum: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectedCash {
    pub resource: ResourceId,
    pub asset: AssetId,
    pub amount: QuantityInterval,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectedPosition {
    pub resource: ResourceId,
    pub position: String,
    pub asset: AssetId,
    pub position_kind: PositionKind,
    pub amount: QuantityInterval,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectedState {
    pub cash: Vec<ProjectedCash>,
    pub positions: Vec<ProjectedPosition>,
    pub authorities: Vec<AuthorityState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectedRequirement {
    CashAtLeast {
        resource: ResourceId,
        asset: AssetId,
        amount: Quantity,
        source_step: Option<u32>,
    },
    PositionAtLeast {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        amount: Quantity,
        source_step: Option<u32>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReturnRouteContext {
    pub baseline: StateSnapshot,
    pub projected: ProjectedState,
    pub allocations: Vec<ResourceAmount>,
    pub asset_prices: Vec<AssetPrice>,
    pub step_index: u32,
    pub prior_proofs: Vec<ReturnRouteProof>,
    pub remaining_cost_budget_usd_micros: Quantity,
}

/// Fresh adapter evidence and bounded projected effects for a plan step.
/// This does not authorize or submit an operation.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReturnRouteProof {
    pub step_index: u32,
    pub operation: String,
    pub input_hash: String,
    pub observed_at: String,
    pub expires_at: String,
    pub intent: FinancialIntent,
    pub effects: Vec<ProjectedEffect>,
    pub requires: Vec<ProjectedRequirement>,
    pub evidence: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparedOperation {
    pub operation: String,
    pub input: Value,
    /// Frozen exact execution fields and enforceable bounds, excluding observation times.
    pub binding: Value,
    pub summary: String,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
    pub observed_at: String,
    pub expires_at: String,
    pub valuation: Option<Valuation>,
    pub intent: FinancialIntent,
    pub operation_version: u32,
    /// Broker-resolved relative deadline, immutable for this plan attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_deadline: Option<plan_timing::ResolvedActionDeadline>,
}
pub type Preview = PreparedOperation;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub kind: String,
    pub reference: String,
    /// Original nonce/client identity and receipt correlation data. No signed bytes or secrets.
    pub data: Value,
}

/// Prepared binding metadata for a trusted adapter-produced replacement
/// attempt. The engine accepts this only for a definition whose semantics
/// explicitly opt into replacement version 1.
pub const REPLACEMENT_METADATA_KEY: &str = "__gofin_replacement";
/// Exact durable relationship used when two compatible reduce-only requests
/// share one protected position lock.
pub const PROTECTION_PARENT_METADATA_KEY: &str = "__gofin_protection_parent";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplacementBinding {
    pub version: u32,
    pub parent_operation_id: String,
    pub parent_submission: Submission,
}

impl ReplacementBinding {
    pub fn from_prepared_binding(binding: &Value) -> Result<Option<Self>> {
        let Some(value) = binding.product_get(REPLACEMENT_METADATA_KEY)? else {
            return Ok(None);
        };
        let replacement: Self = serde_json::from_value(value.clone()).map_err(|_| {
            GoFinError::new(
                "INVALID_REPLACEMENT_BINDING",
                "validation",
                "The prepared replacement link has an unsupported shape.",
                "Prepare the replacement from the original durable submission identity.",
            )
        })?;
        if replacement.version != 1 || replacement.parent_operation_id.trim().is_empty() {
            return Err(GoFinError::new(
                "INVALID_REPLACEMENT_BINDING",
                "validation",
                "The prepared replacement link is incomplete or unsupported.",
                "Prepare the replacement from the original durable submission identity.",
            ));
        }
        Ok(Some(replacement))
    }
}

/// Durable link from a zero-financial-effect recovery action to the unresolved
/// operation whose outcome it is intended to establish. The adapter creates
/// this metadata; the engine and store independently validate it before
/// persisting the child for approval or dispatch.
pub const RECOVERY_METADATA_KEY: &str = "__gofin_recovery";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBinding {
    pub version: u32,
    pub parent_operation_id: String,
    pub parent_submission: Submission,
}

impl RecoveryBinding {
    pub fn from_prepared_binding(binding: &Value) -> Result<Option<Self>> {
        let Some(value) = binding.product_get(RECOVERY_METADATA_KEY)? else {
            return Ok(None);
        };
        let recovery: Self = serde_json::from_value(value.clone()).map_err(|_| {
            GoFinError::new(
                "INVALID_RECOVERY_BINDING",
                "validation",
                "The prepared recovery link has an unsupported shape.",
                "Prepare a recovery child from the exact durable parent submission.",
            )
        })?;
        if recovery.version != 1 || recovery.parent_operation_id.trim().is_empty() {
            return Err(GoFinError::new(
                "INVALID_RECOVERY_BINDING",
                "validation",
                "The prepared recovery link is incomplete or unsupported.",
                "Prepare a recovery child from the exact durable parent submission.",
            ));
        }
        Ok(Some(recovery))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
// Keep the financial-effect wire and Rust construction API as value types.
// Boxing both optional anchors would ripple through every protocol adapter;
// this larger proof row is emitted only for escrow lifecycles and keeps its
// anchors adjacent to the cumulative movement they authorize.
#[allow(clippy::large_enum_variant)]
pub enum FinancialEffect {
    CompoundSubmissionMutations {
        children: Vec<compound_mutations::MutationChildProof>,
    },
    GroupedSubmissionAmendments {
        proof: grouped_amendments::GroupedAmendmentProof,
    },
    ConversionSequence {
        execution: conversion_sequences::ConversionSequence,
    },
    /// A separately authorized request to remove the remainder of one durable submission.
    SubmissionCancellation {
        operation_id: String,
        submission: Submission,
    },
    /// Proven mutation acknowledgment; monetary fills remain owned by the original operation.
    SubmissionAmendment {
        operation_id: String,
        submission: Submission,
        successor: Option<Submission>,
    },
    /// Provenance only: matching cash/position/debt effects apply actual balances once.
    OriginMovement {
        from: AccountingLocation,
        to: AccountingLocation,
        debit: Amount,
        credit: Amount,
    },
    Accrual {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        position_kind: PositionKind,
        delta: SignedQuantity,
    },
    CashDebit {
        resource: ResourceId,
        amount: Amount,
    },
    Transfer {
        from: ResourceId,
        to: ResourceId,
        debit: Amount,
        credit: Amount,
    },
    /// Cumulative, receipt-backed state for an externally executable escrow.
    /// `wallet_debit = escrow_deposit + input_transfer_fee` and
    /// `escrow_deposit = input_filled + input_refunded + remaining_input`.
    /// Input/output fees remain explicit Cost effects where they are actual
    /// additional wallet debits; input token transfer fees are included here.
    EscrowProgress {
        wallet_resource: ResourceId,
        escrow_resource: ResourceId,
        input_asset: AssetId,
        output_asset: AssetId,
        wallet_debit: Quantity,
        escrow_deposit: Quantity,
        input_transfer_fee: Quantity,
        input_filled: Quantity,
        output_filled: Quantity,
        input_refunded: Quantity,
        remaining_input: Quantity,
        state: EscrowLifecycleState,
    },
    /// Cumulative, receipt-backed progress for an escrow that converts into a
    /// position. `position_change` is not a wallet credit; it is paired with
    /// an anchored PositionChange. Only `input_refunded` becomes wallet cash.
    EscrowPositionProgress {
        wallet_resource: ResourceId,
        escrow_resource: ResourceId,
        input_asset: AssetId,
        wallet_debit: Quantity,
        escrow_deposit: Quantity,
        input_transfer_fee: Quantity,
        input_filled: Quantity,
        input_refunded: Quantity,
        remaining_input: Quantity,
        position_resource: ResourceId,
        position: String,
        position_kind: PositionKind,
        position_asset: AssetId,
        position_change: Quantity,
        state: EscrowLifecycleState,
        position_anchor: Option<PositionAnchor>,
        conditional_position_change: Quantity,
        conditional_position_anchor: Option<PositionAnchor>,
    },
    /// Cumulative exact funding and refund state for one protocol account.
    AccountFundingProgress {
        resource: ResourceId,
        position: String,
        account: String,
        asset: AssetId,
        funded: Quantity,
        returned: Quantity,
        remaining: Quantity,
        state: AccountFundingState,
        anchor: Option<PositionAnchor>,
    },
    /// Cumulative finalized state for a keeper-executable request. The
    /// position changes are repeated cumulatively and paired with their
    /// matching anchored PositionChange effects; this marker never creates
    /// spendable cash.
    PendingRequestProgress {
        wallet_resource: ResourceId,
        request_resource: ResourceId,
        request_id: String,
        request_kind: String,
        state: PendingRequestState,
        positions: Vec<PendingRequestPositionProgress>,
        /// Cumulative proceeds actually credited to the wallet for this
        /// request. Omitted/empty means no spendable proceeds have been
        /// proved, including an open or cancelled no-fill request.
        #[serde(default)]
        credits: Vec<PendingRequestCreditProgress>,
        /// Cumulative finalized realized PnL events for this request.
        #[serde(default)]
        realized_pnl: Vec<PendingRequestPnlProgress>,
        /// Exact protection terms decoded from the finalized request state.
        #[serde(default)]
        protection: Option<PendingRequestProtection>,
    },
    Conversion {
        resource: ResourceId,
        spent: Amount,
        received: Amount,
    },
    PositionChange {
        resource: ResourceId,
        position: String,
        asset: AssetId,
        delta: SignedQuantity,
        scaled_receipt: Option<Quantity>,
        anchor: Option<PositionAnchor>,
    },
    Settlement {
        resource: ResourceId,
        asset: AssetId,
        category: SettlementCategory,
        delta: SignedQuantity,
    },
    /// Append-only realized PnL attributed to one finalized derivative
    /// position event. `anchor` must match that event's anchored net position
    /// change. It never creates or removes spendable wallet cash.
    PositionRealizedPnl {
        resource: ResourceId,
        position: String,
        position_kind: PositionKind,
        asset: AssetId,
        event_id: String,
        delta: SignedQuantity,
        anchor: Box<PositionAnchor>,
    },
    DebtChange {
        resource: ResourceId,
        debt: String,
        asset: AssetId,
        delta: SignedQuantity,
        anchor: Option<PositionAnchor>,
    },
    AuthorityChange {
        resource: ResourceId,
        authority: String,
        before: String,
        after: String,
    },
    UsableCredit {
        to: ResourceId,
        amount: Amount,
    },
    /// Proven principal committed to an external transfer but not yet
    /// spendable at either custody location. The containing proof digest binds it.
    TransitPrincipal {
        transfer_id: String,
        amount: Amount,
    },
    Cost {
        location: AccountingLocation,
        amount: Amount,
        category: String,
        treatment: CostTreatment,
    },
    /// Links an explicitly typed IncludedInDebit cost to the gross source
    /// debit and net amount credited to an exact position. Application
    /// adapters prove the conversion from finalized protocol evidence; the
    /// engine verifies identities, arithmetic and that no source debit or
    /// position movement is reused. This marker books no balance or budget.
    IncludedCostConversion {
        location: AccountingLocation,
        source: IncludedCostSource,
        gross_debit: Amount,
        gross_converted: Amount,
        net_position: Amount,
        included_cost: Amount,
        evidence_id: String,
    },
    /// Evidence-only denomination for one typed Cost row, such as a protocol
    /// fee reported in collateral-token atoms while the ledger cost is margin
    /// USD. `evidence_id` binds it to the same finalized protocol event.
    CostDenomination {
        location: AccountingLocation,
        cost_asset: AssetId,
        category: String,
        treatment: CostTreatment,
        denomination: Amount,
        evidence_id: String,
    },
    /// Non-accounting evidence that an exact prior finalized transaction
    /// changed the predecessor anchor for a protected position. The engine
    /// validates the source operation and both canonical order rows before
    /// using `source_anchor` for this one transition.
    PositionAnchorPredecessor {
        predecessor: Box<PositionAnchorPredecessor>,
    },
}

/// Exact source identity for an included protocol cost conversion.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IncludedCostSource {
    Cash {
        resource: ResourceId,
    },
    Escrow {
        wallet_resource: ResourceId,
        escrow_resource: ResourceId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EscrowConditionalPosition {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub asset: AssetId,
    pub maximum_change: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestPositionBound {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub asset: AssetId,
    pub minimum_delta: SignedQuantity,
    pub maximum_delta: SignedQuantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestPositionProgress {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub asset: AssetId,
    pub delta: SignedQuantity,
    pub anchor: PositionAnchor,
}

/// Bounds a request's conditional proceeds in one exact wallet asset. The
/// same asset may be used by several requests, but may occur only once within
/// one request's bound set.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestCreditBound {
    pub asset: AssetId,
    pub minimum_credit: Quantity,
    /// Optional venue/approval-defined upper limit. `None` avoids inventing
    /// a cap for legitimate proceeds whose protocol amount is not bounded at
    /// request creation.
    pub maximum_credit: Option<Quantity>,
}

/// Cumulative, finalized proceeds for a pending request. The recipient is
/// intentionally implicit and fixed to `PendingRequestProgress.wallet_resource`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestCreditProgress {
    pub asset: AssetId,
    pub amount: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestPnlBound {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    /// PnL denomination, which may differ from the derivative notional asset.
    pub asset: AssetId,
    pub maximum_loss: Quantity,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestPnlProgress {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub asset: AssetId,
    pub event_id: String,
    pub delta: SignedQuantity,
    pub anchor: PositionAnchor,
}

/// A request that may only reduce an exact existing position. The anchor
/// excludes observation timestamps/evidence from the shared group identity;
/// it still binds the exact principal and position state reviewed at prepare.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingRequestProtection {
    pub position_anchor: PositionAnchor,
    /// Exact position-scoped reservation shared by compatible protection
    /// siblings. Empty on legacy records, which cannot grant a new lease.
    #[serde(default = "empty_resource_id")]
    pub position_lock_resource: ResourceId,
    pub maximum_reduction: Quantity,
    pub trigger_direction: PendingRequestTriggerDirection,
    pub trigger_price: Amount,
    /// True only when the reviewed protocol instruction is explicitly
    /// reduce-only for this exact position. This is a typed adapter claim;
    /// generic lock sharing still requires a compatible durable sibling.
    #[serde(default)]
    pub reduce_only: bool,
}

impl PendingRequestProtection {
    /// Stable identity for the exposure being protected. Observation time and
    /// receipt evidence are deliberately excluded so a fresh read of the same
    /// position does not create a new group identity.
    pub fn same_exposure(&self, other: &Self) -> bool {
        let left = &self.position_anchor;
        let right = &other.position_anchor;
        left.resource == right.resource
            && left.position == right.position
            && self.position_lock_resource == other.position_lock_resource
            && left.position_kind == right.position_kind
            && left.principal == right.principal
            && left.scaled_receipt == right.scaled_receipt
    }

    /// Whether two independently approved requests can share the exclusive
    /// exposure lock as an opposite-direction reduce-only protection pair.
    pub fn compatible_sibling(&self, other: &Self) -> bool {
        self.same_exposure(other)
            && self.position_anchor.position_kind.is_derivative()
            && self.reduce_only
            && other.reduce_only
            && self.trigger_direction != other.trigger_direction
            && self.trigger_price.asset == other.trigger_price.asset
            && !self.trigger_price.atoms.is_zero()
            && !other.trigger_price.atoms.is_zero()
            && !self.maximum_reduction.is_zero()
            && !other.maximum_reduction.is_zero()
            && self.maximum_reduction <= self.position_anchor.principal.atoms
            && other.maximum_reduction <= other.position_anchor.principal.atoms
    }
}

fn empty_resource_id() -> ResourceId {
    ResourceId(String::new())
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingRequestTriggerDirection {
    Above,
    Below,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingRequestState {
    Open,
    Filled,
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EscrowLifecycleState {
    Open,
    Filled,
    Cancelled,
    Expired,
}

/// Prevent a proved expense included in a repayment/supply debit being charged twice.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostTreatment {
    AdditionalDebit,
    /// Venue-calculated additional cash debit explicitly consented to by the
    /// user. `CostBound.amount` is the finite upfront reservation, not a cap
    /// on the ultimately proved fee. Supported only by narrowly reviewed
    /// standalone operations.
    VenueCalculatedAdditionalDebit,
    IncludedInDebit,
    /// Withheld from the gross proved credit, applied exactly once by the ledger.
    DeductedFromCredit,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CostBound {
    pub location: AccountingLocation,
    pub amount: Amount,
    pub treatment: CostTreatment,
}

impl AccountingLocation {
    pub fn resource(&self) -> &ResourceId {
        match self {
            Self::Cash { resource } | Self::Position { resource, .. } => resource,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedEffects {
    pub evidence_version: u32,
    pub submission: Submission,
    pub effects: Vec<FinancialEffect>,
    pub evidence: Value,
    pub observed_at: String,
    pub proof_digest: String,
}

/// Versioned ordering metadata for adapter-produced cumulative reconciliation
/// snapshots. The sequence is scoped to the immutable external submission;
/// `snapshot_id` is stable for the same venue snapshot across retries.
///
/// Adapters encode this value at `VerifiedEffects.evidence.settlement_progress`.
/// Effects in such a bundle are cumulative totals for the original submission,
/// never deltas. Each later sequence must include all earlier proved effects.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettlementProgress {
    pub version: u32,
    pub sequence: u64,
    pub snapshot_id: String,
    /// Stable adapter event identities mapped to canonical receipt digests.
    /// Entries are append-only: a later snapshot may add events but may not
    /// remove an identity or change its digest.
    pub event_digests: std::collections::BTreeMap<String, String>,
    /// Canonical transaction ordering for finalized events that can change a
    /// protected position. These rows are append-only with the event set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub position_orders: Vec<PositionEventOrder>,
}

/// Canonical transaction order for one finalized position event. The exact
/// submission is included so an index cannot be detached from its transaction.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionEventOrder {
    pub submission: Submission,
    pub domain: String,
    pub slot: u64,
    pub transaction_index: u32,
    pub event_id: String,
}

/// A narrow, exact predecessor proof for a position transition after a
/// compatible protected sibling has already reduced the same exposure.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionAnchorPredecessor {
    pub source_operation_id: String,
    pub source_submission: Submission,
    pub source_proof_digest: String,
    pub source_anchor: PositionAnchor,
    pub source_order: PositionEventOrder,
    pub current_order: PositionEventOrder,
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub asset: AssetId,
}

impl SettlementProgress {
    pub fn from_evidence(evidence: &Value) -> Result<Option<Self>> {
        let Some(value) = evidence.get("settlement_progress") else {
            return Ok(None);
        };
        let progress: Self = serde_json::from_value(value.clone()).map_err(|_| {
            GoFinError::new(
                "INVALID_SETTLEMENT_PROGRESS",
                "reconciliation",
                "Cumulative settlement progress metadata has an unsupported shape.",
                "Reconcile the original submission with a versioned, ordered cumulative snapshot.",
            )
        })?;
        if progress.version == 0
            || progress.sequence == 0
            || progress.sequence > i64::MAX as u64
            || progress.snapshot_id.is_empty()
            || progress
                .event_digests
                .iter()
                .any(|(identity, digest)| identity.is_empty() || digest.is_empty())
            || progress
                .event_digests
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != progress.event_digests.len()
            || progress.position_orders.iter().any(|order| {
                order.domain.trim().is_empty()
                    || order.submission.reference.trim().is_empty()
                    || order.event_id.trim().is_empty()
                    || !progress.event_digests.contains_key(&order.event_id)
            })
            || progress
                .position_orders
                .iter()
                .filter_map(|order| {
                    serde_json::to_string(&order.submission)
                        .ok()
                        .map(|submission| (submission, order.event_id.clone()))
                })
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != progress.position_orders.len()
        {
            return Err(GoFinError::new(
                "INVALID_SETTLEMENT_PROGRESS",
                "reconciliation",
                "Cumulative settlement progress metadata is incomplete.",
                "Reconcile the original submission with a versioned, ordered cumulative snapshot.",
            ));
        }
        Ok(Some(progress))
    }
}

pub type VerifiedEffectBundle = VerifiedEffects;

pub fn verified_effect_digest(bundle: &VerifiedEffects) -> Result<String> {
    binding_hash(&serde_json::json!({
        "version":bundle.evidence_version,
        "submission":bundle.submission,
        "effects":bundle.effects,
        "evidence":bundle.evidence,
        "observed_at":bundle.observed_at,
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    AwaitingApproval,
    Rejected,
    Expired,
    Cancelled,
    Submitted,
    Pending,
    Completed,
    Failed,
    Unknown,
}
impl Status {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Expired | Self::Cancelled | Self::Completed | Self::Failed
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAudit {
    pub mechanism: String,
    pub binding_hash: String,
    pub challenge_nonce: String,
    pub approved_at: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub id: String,
    pub request_id: String,
    pub fingerprint: String,
    pub fingerprint_version: u32,
    pub operation: String,
    pub input: Value,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    pub preview: Option<PreparedOperation>,
    pub approval: Option<ApprovalAudit>,
    pub submission: Option<Submission>,
    /// Immutable venue evidence correlated to a dispatch after submission.
    /// Older journal records predate this field and deserialize as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_evidence: Option<Value>,
    /// Finalized observer anchors captured before dispatch when this operation
    /// needs a prior position state that is not already in the durable history.
    /// This is immutable recovery context; it is never refreshed after submit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_baseline: Option<PositionAnchorBaseline>,
    pub effects: Option<VerifiedEffects>,
    pub last_reconciliation_evidence: Option<Value>,
    pub error: Option<GoFinError>,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub request_id: String,
    pub operation: String,
    pub input: Value,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reconciliation {
    pub status: Status,
    pub evidence: Value,
    /// Required for terminal financial results, including cost-only proved failure.
    /// Pending results may include cumulative verified effects. Such evidence must
    /// contain immutable `proofEvents` with stable `eventId`s, preserve every prior
    /// event, and respect the original cumulative caps. Completion minima apply
    /// only at terminal settlement; unresolved reservations remain owned.
    pub verified: Option<VerifiedEffects>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationChallenge {
    pub purpose: String,
    pub operation_id: String,
    pub binding: Value,
    pub binding_hash: String,
    pub summary: String,
    pub nonce: String,
    pub expires_at: String,
}

#[async_trait]
pub trait Authorizer: Send + Sync {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit>;
}
/// Broker-owned hook; implementations atomically journal before returning.
pub trait SubmissionJournal: Send + Sync {
    fn assert_authorized(&self) -> Result<()>;
    /// Called after a fresh adapter revalidation immediately before dispatch.
    /// Existing exact-operation implementations retain their old check.
    fn assert_candidate(&self, _prepared: &PreparedOperation) -> Result<()> {
        self.assert_authorized()
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()>;
    /// Persist receipt evidence for the already durable submission identity.
    /// Implementations must fail closed unless that exact submission exists.
    /// This records evidence after dispatch and therefore must not recheck
    /// approval freshness or authorize another write.
    fn persist_dispatch_evidence(&self, _submission: &Submission, _evidence: &Value) -> Result<()> {
        Err(GoFinError::new(
            "DISPATCH_EVIDENCE_UNSUPPORTED",
            "reconciliation",
            "This execution journal cannot persist dispatch evidence.",
            "Keep the operation unresolved and reconcile its original submission.",
        ))
    }
}

#[async_trait]
pub trait Adapter: Send + Sync {
    fn attach_execution_state(
        &self,
        _state: Arc<dyn execution_state::ExecutionState>,
    ) -> Result<()> {
        Ok(())
    }
    fn definitions(&self) -> Vec<Definition>;
    /// Public, user-facing enrolled connections for discovery and snapshots.
    fn connections(&self) -> Vec<ConnectionMetadata> {
        Vec::new()
    }
    /// Observed transport health, without network probes or credentials.
    /// Unsupported adapters return null. Public management output accepts only
    /// reviewed diagnostic fields, identifiers and reason codes.
    fn diagnostics(&self) -> Value {
        Value::Null
    }
    /// Broker-owned RPC dependency for read and original-submission evidence.
    /// Other integrations retain their existing wait limits. This metadata
    /// does not grant authority or weaken operation-kind and evidence checks.
    fn uses_rpc_reads(&self, _operation: &str, _input: &Value) -> bool {
        false
    }
    /// Stable protected identities whose change can alter an approved
    /// operation's authority, such as account UIDs or resolved destination
    /// aliases. This collection is broker-only and must never be reduced by
    /// display name. The default preserves adapters whose public connection
    /// metadata already contains every stable identity relevant to writes.
    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        self.connections()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Err(GoFinError::new(
            "SNAPSHOT_UNAVAILABLE",
            "snapshot",
            "This integration has no implemented complete financial observer.",
            "Do not begin a scoped run without a reviewed complete baseline.",
        ))
    }
    /// Capture only the requested financial scope. Implementations must retain
    /// related exposure and report a bounded coverage scope; unsupported scoped
    /// observers fail closed instead of silently querying the whole portfolio.
    /// Actual financial observers, distinct from identities bound by a composite
    /// write adapter that delegates observation to its constituent providers.
    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        self.authorization_identities()
    }
    /// Journal-only global observers contribute unresolved activity to every
    /// scope without querying unrelated accounts or external networks.
    fn snapshot_is_global(&self) -> bool {
        false
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        if scope.is_full() || self.snapshot_is_global() {
            self.snapshot().await
        } else {
            Err(GoFinError::new(
                "SNAPSHOT_SCOPE_UNSUPPORTED",
                "snapshot",
                "This observer does not implement scoped financial snapshots.",
                "Use an observer with scoped coverage or request a full snapshot explicitly.",
            ))
        }
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value>;
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation>;
    /// Validate an adapter-produced recovery child against the exact durable
    /// parent. The default denies recovery links; ordinary requests cannot
    /// create this authority by supplying metadata in their input.
    fn validate_recovery_child(
        &self,
        _prepared: &PreparedOperation,
        _parent: &OperationRecord,
    ) -> Result<Option<RecoveryBinding>> {
        Ok(None)
    }
    /// Human-only configuration operations use a separate engine entry point
    /// and a distinct approval mechanism. The default fails closed so an
    /// adapter cannot accidentally make financial writes available here.
    async fn prepare_configuration(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> Result<PreparedOperation> {
        Err(GoFinError::new(
            "CONFIGURATION_UNSUPPORTED",
            "configuration",
            "This integration has no supported human configuration operation.",
            "Use a reviewed human-only configuration operation when available.",
        ))
    }
    /// Derive the enforceable, operation-specific scope for a bounded
    /// authorization. Adapters opt in explicitly; the default fails closed.
    /// Scope derivation may asynchronously resolve fresh pinned protocol
    /// identities; callers recheck prepared freshness after the awaited result.
    async fn prepare_boundaries(
        &self,
        _prepared: &PreparedOperation,
        _bounds: &Value,
    ) -> Result<boundaries::BoundarySpec> {
        Err(GoFinError::new(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This integration does not support boundary-based authorization.",
            "Use the ordinary exact-operation approval flow.",
        ))
    }
    /// Trusted planning allowance; None requires an explicit parent duration. No network calls.
    fn plan_step_allowance(&self, operation: &str, _input: &Value) -> Result<Option<u64>> {
        let definition = self
            .definitions()
            .into_iter()
            .find(|d| d.name == operation)
            .ok_or_else(|| GoFinError::validation("Unknown plan operation."))?;
        plan_timing::definition_allowance(&definition, _input)
    }
    /// Derive a reusable, balance-independent permission scope for one plan
    /// step. Implementations validate operation support, static identities,
    /// permissions and market metadata without requiring balances or freezing
    /// short-lived prices. The default fails closed.
    async fn prepare_plan_permission(
        &self,
        _operation: &str,
        _input: &Value,
        _bounds: &Value,
    ) -> Result<financial_plans::PlanPermission> {
        Err(GoFinError::new(
            "PLAN_PERMISSION_UNSUPPORTED",
            "authorization",
            "This integration does not support named financial plan permissions.",
            "Use an operation with an explicitly supported plan permission scope.",
        ))
    }
    /// Balance-independent admission before plan approval. Protected read-only
    /// permission, fee, route and metadata queries are allowed; no mutations,
    /// reservations or checks requiring future funding. Return explicit warnings
    /// for deferred checks. Execution still validates mutable evidence.
    async fn validate_plan_feasibility(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> Result<Vec<String>> {
        Ok(vec![
            "Adapter-specific feasibility checks are deferred until execution.".into(),
        ])
    }
    /// Independently validate a concrete candidate against the approved
    /// adapter-derived scope. Implementations must reject undeclared changes.
    fn validate_boundary_candidate(
        &self,
        _spec: &boundaries::BoundarySpec,
        _prepared: &PreparedOperation,
    ) -> Result<()> {
        Err(GoFinError::new(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This integration does not support boundary-based authorization.",
            "Use the ordinary exact-operation approval flow.",
        ))
    }
    async fn prepare_with_compound_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        _linked: &compound_mutations::CompoundAmendmentContext,
    ) -> Result<PreparedOperation> {
        self.prepare_with_linked_context(
            operation,
            input,
            context,
            &amendments::AmendmentContext::default(),
        )
        .await
    }
    async fn prepare_with_linked_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        _linked: &amendments::AmendmentContext,
    ) -> Result<PreparedOperation> {
        self.prepare_with_context(operation, input, context).await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.prepare(operation, input).await
    }
    /// Prove that an ordered planned step is reachable from the broker's
    /// projected state. Implementations must read current application state,
    /// bind the exact input, and return bounded projected effects. Unsupported
    /// steps remain unavailable by default.
    async fn preflight_return_route(
        &self,
        _operation: &str,
        _input: &Value,
        _context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof> {
        Err(GoFinError::new(
            "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
            "budget",
            "This integration cannot verify a conditional return-route step.",
            "Do not begin the scoped run until every planned operation has a reviewed route proof.",
        ))
    }
    /// Explicit opt-in to reuse initial preparation as final execution evidence.
    /// The broker measures this window from before preparation, checks all
    /// candidate/price deadlines, and serializes conflicting writes. Implementors
    /// must perform exact final nonce/authority checks through their submit path.
    /// None preserves the adapter's ordinary final revalidation contract.
    fn preparation_reuse_window(
        &self,
        _prepared: &PreparedOperation,
    ) -> Option<std::time::Duration> {
        None
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation>;
    /// Refresh observation evidence after slow reservation work. The engine
    /// requires the exact execution binding and financial intent to stay fixed.
    async fn finalize_after_reservation(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }
    /// Revalidate a candidate through the explicit human configuration route.
    async fn revalidate_configuration(
        &self,
        _prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        Err(GoFinError::new(
            "CONFIGURATION_UNSUPPORTED",
            "configuration",
            "This integration has no supported human configuration operation.",
            "Keep the request unresolved and use a reviewed human-only configuration operation.",
        ))
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission>;
    /// Reuse the broker's final adapter revalidation. Integrations that do not
    /// implement this contract keep their ordinary validating submission path.
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        self.submit(validated.prepared(), journal).await
    }
    /// Re-verify whether a durable terminal attempt proves a candidate expiry
    /// with no network dispatch. Implementations must inspect the exact
    /// persisted submission and adapter-specific reconciliation evidence;
    /// absence of dispatch flags or history is not proof. The engine independently
    /// binds the returned opaque attestation to the durable record.
    async fn verify_non_dispatch_expiry(
        &self,
        _record: &OperationRecord,
    ) -> Result<Option<VerifiedNonDispatchExpiry>> {
        Ok(None)
    }
    async fn submit_configuration_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        self.submit_configuration(validated.prepared(), journal)
            .await
    }
    /// Submit only through the explicit human configuration lifecycle.
    async fn submit_configuration(
        &self,
        _prepared: &PreparedOperation,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(GoFinError::new(
            "CONFIGURATION_UNSUPPORTED",
            "configuration",
            "This integration has no supported human configuration operation.",
            "Keep the request unresolved and use a reviewed human-only configuration operation.",
        ))
    }
    /// Adapter-verified historical position observations bound to the original
    /// preview/submission. Used only when the journal has no earlier GoFin anchor;
    /// never replaces trusted lineage or creates wallet cash/funding authority.
    async fn verified_position_baselines(
        &self,
        _record: &OperationRecord,
    ) -> Result<Vec<PositionAnchor>> {
        Ok(Vec::new())
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation>;
    /// Reconcile persisted human configuration submissions through the exact
    /// submission identity. This is a read/recovery operation, not authority
    /// to dispatch another configuration write.
    async fn reconcile_configuration(&self, _record: &OperationRecord) -> Result<Reconciliation> {
        Ok(Reconciliation {
            status: Status::Unknown,
            evidence: serde_json::json!({"reason":"Human configuration reconciliation is unsupported."}),
            verified: None,
        })
    }
    async fn reconcile_with_compound_context(
        &self,
        record: &OperationRecord,
        _context: &compound_mutations::CompoundAmendmentContext,
    ) -> Result<Reconciliation> {
        self.reconcile_with_context(record, &amendments::AmendmentContext::default())
            .await
    }
    async fn reconcile_with_context(
        &self,
        record: &OperationRecord,
        _context: &amendments::AmendmentContext,
    ) -> Result<Reconciliation> {
        self.reconcile(record).await
    }
}

#[derive(
    Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum PositionKind {
    Supply,
    /// Application-reported investment equity, with units and redemption
    /// restrictions retained in its verified anchor evidence.
    Investment,
    /// Cash committed to a position's margin, distinct from exposure units.
    LockedCollateral,
    /// Assets held by a noncustodial protocol request or order. This balance
    /// is not wallet cash and becomes usable only after separately verified
    /// refund or settlement evidence.
    Escrow,
    /// Refundable native value held as rent in a wallet-owned account. This is
    /// distinct from protocol collateral and remains an asset until returned.
    RefundableAccountFunding,
    StableDebt,
    VariableDebt,
    DerivativeLong,
    DerivativeShort,
    MarginLong,
    MarginShort,
}
impl PositionKind {
    pub fn is_margin(&self) -> bool {
        matches!(self, Self::MarginLong | Self::MarginShort)
    }
    pub fn is_derivative(&self) -> bool {
        matches!(self, Self::DerivativeLong | Self::DerivativeShort)
    }
}

/// Broker-owned pre-action anchors in the immutable approved execution binding.
pub const BASELINE_ANCHORS_KEY: &str = "_gofin_verified_baseline_anchors";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettlementCategory {
    RealizedPnl,
    Funding,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionState {
    pub resource: ResourceId,
    pub position: String,
    pub kind: PositionKind,
    pub amount: Amount,
    pub scaled_receipt: Option<Quantity>,
    pub anchor: Option<PositionAnchor>,
}

/// Broker-owned previously proved backing. JSON evidence is interpreted by the reviewed application.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionAnchor {
    pub resource: ResourceId,
    pub position: String,
    pub position_kind: PositionKind,
    pub principal: Amount,
    pub scaled_receipt: Option<Quantity>,
    pub evidence_version: u32,
    pub evidence: Value,
    pub observed_at: String,
}

/// Versioned, broker-captured position anchors used to bootstrap an operation
/// from an independently verified pre-dispatch observer snapshot. Historical
/// operation/run anchors take precedence; this contains only missing anchors.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionAnchorBaseline {
    pub version: u32,
    pub captured_at: String,
    pub operation_id: String,
    pub prepared_binding_hash: String,
    pub scope: Vec<ResourceId>,
    pub coverage: Vec<String>,
    pub anchors: Vec<PositionAnchor>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparationContext {
    pub positions: Vec<PositionAnchor>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorityState {
    pub resource: ResourceId,
    pub authority: String,
    pub value: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialState {
    pub holdings: Vec<ResourceAmount>,
    pub positions: Vec<PositionState>,
    pub authorities: Vec<AuthorityState>,
    pub in_flight: Vec<String>,
}

/// Optional financial read scope. Empty selectors preserve the full snapshot.
/// Selectors intersect; asset selection never excludes related account exposure
/// (for example collateral/debt or the native asset required for execution fees).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotScope {
    #[serde(default)]
    pub resources: Vec<ResourceId>,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub assets: Vec<AssetId>,
}
impl SnapshotScope {
    pub fn is_full(&self) -> bool {
        self.resources.is_empty() && self.networks.is_empty() && self.assets.is_empty()
    }
    pub fn matches_resource(&self, resource: &ResourceId) -> bool {
        self.resources.is_empty() || self.resources.contains(resource)
    }
    pub fn matches_network(&self, network: &str) -> bool {
        (self.networks.is_empty() || self.networks.iter().any(|n| n == network))
            && (self.assets.is_empty() || self.assets.iter().any(|a| a.network == network))
    }
    pub fn matches_asset(&self, asset: &AssetId) -> bool {
        self.matches_network(&asset.network)
            && (self.assets.is_empty() || self.assets.contains(asset))
    }
    pub fn matches_connection(&self, connection: &ConnectionMetadata) -> bool {
        self.matches_resource(&connection.resource)
            && connection
                .network
                .as_deref()
                .is_none_or(|n| self.matches_network(n))
    }
}

/// Created by reviewed observers, never proof merely because a client sets `complete`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StateSnapshot {
    pub state: FinancialState,
    pub observed_at: String,
    pub complete: bool,
    pub coverage: Vec<String>,
    pub pending: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectionMetadata {
    pub name: String,
    pub provider: String,
    pub resource: ResourceId,
    pub role: Option<String>,
    pub parent: Option<String>,
    pub network: Option<String>,
    pub address: Option<String>,
    pub credential_ref: Option<String>,
    pub signing_enabled: bool,
    /// Stable, non-secret identity/configuration attributes for this
    /// connection. Adapters must omit volatile observations and secrets; the
    /// engine includes this value in exact-plan connection bindings.
    pub details: Value,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn canonical_json(value: &Value) -> Result<String> {
    fn sorted(value: &Value) -> Result<Value> {
        Ok(match value {
            Value::Object(map) => {
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                let mut result = serde_json::Map::new();
                for key in keys {
                    result.insert(key.clone(), sorted(&map[key])?);
                }
                Value::Object(result)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect::<Result<_>>()?),
            Value::Number(n) if n.is_f64() => {
                return Err(GoFinError::validation(
                    "Floating point JSON is forbidden in execution bindings.",
                ));
            }
            other => other.clone(),
        })
    }
    serde_json::to_string(&sorted(value)?)
        .map_err(|_| GoFinError::validation("Invalid canonical JSON."))
}
pub fn binding_hash(value: &Value) -> Result<String> {
    product_names::validate(value)?;
    Ok(hex::encode(Sha256::digest(
        canonical_json(value)?.as_bytes(),
    )))
}
pub fn request_fingerprint(request: &OperationRequest) -> Result<String> {
    binding_hash(
        &serde_json::json!({"version":1,"operation":request.operation,"input":request.input,"run_id":request.run_id}),
    )
}

/// Native review binds execution, exact units, and finite financial outcomes together.
pub fn authorization_binding(prepared: &PreparedOperation) -> Value {
    // Preserve an ambiguous object intact so binding_hash rejects it.
    if product_names::validate(&prepared.binding).is_err() {
        return prepared.binding.clone();
    }
    if let Some(binding) = prepared
        .binding
        .product_get(boundaries::BOUNDARY_METADATA_KEY)
        .ok()
        .flatten()
        .and_then(|metadata| metadata.get("grant_binding"))
    {
        return binding.clone();
    }
    serde_json::json!({
        "version":1,
        "operation":prepared.operation,
        "operation_version":prepared.operation_version,
        "execution":prepared.binding,
        "financial_intent":prepared.intent,
        "value_bounds":prepared.binding.product_get(value_bounds::KEY).ok().flatten().cloned().or_else(|| prepared.valuation.as_ref().map(|v|serde_json::json!({
            "movement_usd_micros":v.movement_usd_micros,
            "maximum_fee_usd_micros":v.maximum_fee_usd_micros,
            "additional_allocation_usd_micros":v.additional_allocation_usd_micros,
            "prices":v.prices.iter().map(|p|serde_json::json!({"asset":p.asset,"usd_micros_per_unit":p.usd_micros_per_unit,"unit_atoms":p.unit_atoms,"source":p.source})).collect::<Vec<_>>()
        })))
    })
}
