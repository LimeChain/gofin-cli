//! Integration-independent contracts. Secrets and signed payloads never enter these types.
pub mod money;
pub mod policy;

use async_trait::async_trait;
pub use money::{Quantity, SignedQuantity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, FinError>;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
#[serde(deny_unknown_fields)]
pub struct FinError {
    pub code: String,
    pub stage: String,
    pub message: String,
    pub next_action: String,
}
impl FinError {
    pub fn new(code: &str, stage: &str, message: impl Into<String>, next_action: &str) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            message: message.into(),
            next_action: next_action.into(),
        }
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
    fn persist_submission(&self, submission: &Submission) -> Result<()>;
}

#[async_trait]
pub trait Adapter: Send + Sync {
    fn definitions(&self) -> Vec<Definition>;
    fn connections(&self) -> Vec<ConnectionMetadata> {
        Vec::new()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Err(FinError::new(
            "SNAPSHOT_UNAVAILABLE",
            "snapshot",
            "This integration has no implemented complete financial observer.",
            "Do not begin a scoped run without a reviewed complete baseline.",
        ))
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value>;
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation>;
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
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission>;
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation>;
}

#[derive(
    Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum PositionKind {
    Supply,
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
    serde_json::json!({
        "version":1,
        "operation":prepared.operation,
        "operation_version":prepared.operation_version,
        "execution":prepared.binding,
        "financial_intent":prepared.intent,
        "value_bounds":prepared.valuation.as_ref().map(|v|serde_json::json!({
            "movement_usd_micros":v.movement_usd_micros,
            "maximum_fee_usd_micros":v.maximum_fee_usd_micros,
            "additional_allocation_usd_micros":v.additional_allocation_usd_micros,
            "prices":v.prices.iter().map(|p|serde_json::json!({"asset":p.asset,"usd_micros_per_unit":p.usd_micros_per_unit,"unit_atoms":p.unit_atoms,"source":p.source})).collect::<Vec<_>>()
        }))
    })
}
