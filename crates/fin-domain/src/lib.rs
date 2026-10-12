//! Integration-independent contracts. Secrets and signed payloads never enter these types.
pub mod authority_expiry;
pub mod boundaries;
pub mod clock;
pub mod execution_state;
pub mod financial_plans;
pub mod money;
pub mod plan_timing;
pub mod policy;
pub mod price_freshness;
pub mod unpriced;
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

pub type Result<T> = std::result::Result<T, FinError>;
/// Reviewed-adapter metadata in a prepared operation binding for one finite
/// bridge step; candidate inputs alone never establish this scope.
pub const BRIDGE_STEP_METADATA_KEY: &str = "__fin_bridge_step";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct FinError {
    pub code: String,
    pub stage: String,
    pub message: String,
    pub next_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<Value>>,
}
impl FinError {
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
            "Inspect fin schema for the selected command.",
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
}

impl BridgeStepKind {
    pub const fn is_destination_stage(&self) -> bool {
        matches!(self, Self::Prove | Self::Finalize)
    }
    pub const fn pending_state(&self) -> BridgeTransferState {
        match self {
            Self::Deposit => BridgeTransferState::SourcePending,
            Self::Prove | Self::Finalize => BridgeTransferState::DestinationPending,
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
            | Self::Finalize => "bridge.transfer",
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
    pub destination_chain_id: u64,
    pub transaction_hash: String,
    /// Withdrawal messages do not determine a finalization transaction hash.
    /// The scheme binds this immutable message hash instead; old locators omit it.
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectConstraint {
    /// A separately authorized request to remove the remainder of one durable submission.
    SubmissionCancellation {
        operation_id: String,
        submission: Submission,
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
    /// Long/short exposure quantities, never custody or borrowed principal.
    DerivativePosition {
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
pub const REPLACEMENT_METADATA_KEY: &str = "__fin_replacement";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplacementBinding {
    pub version: u32,
    pub parent_operation_id: String,
    pub parent_submission: Submission,
}

impl ReplacementBinding {
    pub fn from_prepared_binding(binding: &Value) -> Result<Option<Self>> {
        let Some(value) = binding.get(REPLACEMENT_METADATA_KEY) else {
            return Ok(None);
        };
        let replacement: Self = serde_json::from_value(value.clone()).map_err(|_| {
            FinError::new(
                "INVALID_REPLACEMENT_BINDING",
                "validation",
                "The prepared replacement link has an unsupported shape.",
                "Prepare the replacement from the original durable submission identity.",
            )
        })?;
        if replacement.version != 1 || replacement.parent_operation_id.trim().is_empty() {
            return Err(FinError::new(
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
pub const RECOVERY_METADATA_KEY: &str = "__fin_recovery";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBinding {
    pub version: u32,
    pub parent_operation_id: String,
    pub parent_submission: Submission,
}

impl RecoveryBinding {
    pub fn from_prepared_binding(binding: &Value) -> Result<Option<Self>> {
        let Some(value) = binding.get(RECOVERY_METADATA_KEY) else {
            return Ok(None);
        };
        let recovery: Self = serde_json::from_value(value.clone()).map_err(|_| {
            FinError::new(
                "INVALID_RECOVERY_BINDING",
                "validation",
                "The prepared recovery link has an unsupported shape.",
                "Prepare a recovery child from the exact durable parent submission.",
            )
        })?;
        if recovery.version != 1 || recovery.parent_operation_id.trim().is_empty() {
            return Err(FinError::new(
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
pub enum FinancialEffect {
    /// A separately authorized request to remove the remainder of one durable submission.
    SubmissionCancellation {
        operation_id: String,
        submission: Submission,
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
}

/// Prevent a proved expense included in a repayment/supply debit being charged twice.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostTreatment {
    AdditionalDebit,
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
}

impl SettlementProgress {
    pub fn from_evidence(evidence: &Value) -> Result<Option<Self>> {
        let Some(value) = evidence.get("settlement_progress") else {
            return Ok(None);
        };
        let progress: Self = serde_json::from_value(value.clone()).map_err(|_| {
            FinError::new(
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
        {
            return Err(FinError::new(
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
    pub effects: Option<VerifiedEffects>,
    pub last_reconciliation_evidence: Option<Value>,
    pub error: Option<FinError>,
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
        Err(FinError::new(
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
    /// Stable protected identities whose change can alter an approved
    /// operation's authority, such as account UIDs or resolved destination
    /// aliases. This collection is broker-only and must never be reduced by
    /// display name. The default preserves adapters whose public connection
    /// metadata already contains every stable identity relevant to writes.
    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        self.connections()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Err(FinError::new(
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
            Err(FinError::new(
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
        Err(FinError::new(
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
        Err(FinError::new(
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
            .ok_or_else(|| FinError::validation("Unknown plan operation."))?;
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
        Err(FinError::new(
            "PLAN_PERMISSION_UNSUPPORTED",
            "authorization",
            "This integration does not support named financial plan permissions.",
            "Use an operation with an explicitly supported plan permission scope.",
        ))
    }
    /// Independently validate a concrete candidate against the approved
    /// adapter-derived scope. Implementations must reject undeclared changes.
    fn validate_boundary_candidate(
        &self,
        _spec: &boundaries::BoundarySpec,
        _prepared: &PreparedOperation,
    ) -> Result<()> {
        Err(FinError::new(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This integration does not support boundary-based authorization.",
            "Use the ordinary exact-operation approval flow.",
        ))
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
        Err(FinError::new(
            "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
            "budget",
            "This integration cannot verify a conditional return-route step.",
            "Do not begin the scoped run until every planned operation has a reviewed route proof.",
        ))
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation>;
    /// Revalidate a candidate through the explicit human configuration route.
    async fn revalidate_configuration(
        &self,
        _prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        Err(FinError::new(
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
    /// Submit only through the explicit human configuration lifecycle.
    async fn submit_configuration(
        &self,
        _prepared: &PreparedOperation,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(FinError::new(
            "CONFIGURATION_UNSUPPORTED",
            "configuration",
            "This integration has no supported human configuration operation.",
            "Keep the request unresolved and use a reviewed human-only configuration operation.",
        ))
    }
    /// Adapter-verified historical position observations bound to the original
    /// preview/submission. Used only when the journal has no earlier Fin anchor;
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
    StableDebt,
    VariableDebt,
    DerivativeLong,
    DerivativeShort,
}
impl PositionKind {
    pub fn is_derivative(&self) -> bool {
        matches!(self, Self::DerivativeLong | Self::DerivativeShort)
    }
}

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
                return Err(FinError::validation(
                    "Floating point JSON is forbidden in execution bindings.",
                ));
            }
            other => other.clone(),
        })
    }
    serde_json::to_string(&sorted(value)?)
        .map_err(|_| FinError::validation("Invalid canonical JSON."))
}
pub fn binding_hash(value: &Value) -> Result<String> {
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
    if let Some(binding) = prepared
        .binding
        .get(boundaries::BOUNDARY_METADATA_KEY)
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
        "value_bounds":prepared.binding.get(value_bounds::KEY).cloned().or_else(|| prepared.valuation.as_ref().map(|v|serde_json::json!({
            "movement_usd_micros":v.movement_usd_micros,
            "maximum_fee_usd_micros":v.maximum_fee_usd_micros,
            "additional_allocation_usd_micros":v.additional_allocation_usd_micros,
            "prices":v.prices.iter().map(|p|serde_json::json!({"asset":p.asset,"usd_micros_per_unit":p.usd_micros_per_unit,"unit_atoms":p.unit_atoms,"source":p.source})).collect::<Vec<_>>()
        })))
    })
}
