//! Contracts for named, bounded financial plans.
//!
//! A plan is one approval authority over ordered step scopes. Individual
//! attempts keep their existing operation identity and journal lifecycle.

use crate::{
    ApprovalAudit, AssetId, CostBound, GoFinError, PreparedOperation, ResourceAmount, Status,
    Submission, boundaries, boundaries::BoundarySpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod definition_hash;
pub use definition_hash::exact_plan_definition_hash;

pub const FINANCIAL_PLAN_VERSION: u32 = 1;
/// Broker-owned metadata embedded in a prepared candidate, removed before an
/// immutable attempt binding is calculated.
pub const PLAN_METADATA_KEY: &str = "__fin_financial_plan";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanSpec {
    pub version: u32,
    /// Display label only. It grants no authority and is included in approval.
    pub name: String,
    /// Optional caller-authored financial sentence shown after the plan name in
    /// approval. It grants no authority; the complete plan remains bound. Use
    /// one trimmed line, with at most 140 Unicode characters across name and
    /// summary together. Control and bidirectional formatting are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 140))]
    pub summary: Option<String>,
    pub steps: Vec<FinancialPlanStepSpec>,
    pub aggregate_bounds: FinancialPlanBounds,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanStepSpec {
    pub step_id: String,
    pub operation: String,
    pub input: Value,
    /// Either an exact generic scope (`mode: "exact"`) with explicit typed
    /// debit/cost caps and possible credit identities, or adapter-reviewed
    /// variation limits for an operation that supports variable inputs.
    pub permitted_variations: Value,
    pub completion: FinancialStepCompletion,
    pub funding_dependencies: Vec<FinancialPlanFundingDependency>,
    /// Relative execution window, resolved only when an eligible attempt is prepared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub action_deadline_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanBounds {
    pub maximum_debits: Vec<ResourceAmount>,
    pub maximum_costs: Vec<CostBound>,
}

/// Cumulative principal-spend target and/or minimum net receipt target.
/// When both are present, both must be satisfied.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialStepCompletion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_spend: Option<ResourceAmount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_net_receipt: Option<ResourceAmount>,
    /// Complete this step when its normal verified operation reaches the
    /// Completed state. This is useful for writes whose outcome is not a cash
    /// amount (for example changing an allowance or cancelling an order).
    #[serde(default, skip_serializing_if = "is_false")]
    pub operation_succeeded: bool,
}

/// Caller-declared caps for a generic exact operation scope. The engine binds
/// the complete operation input and protected connection identity separately.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanExactScope {
    pub mode: FinancialPlanExactMode,
    pub maximum_debits: Vec<ResourceAmount>,
    pub maximum_costs: Vec<CostBound>,
    pub possible_credits: Vec<FinancialPlanFundingIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinancialPlanExactMode {
    Exact,
}

/// Engine-derived exact-scope identity, durable as part of the approved
/// revision. Hashes cover the registered operation contract and the adapter's
/// stable protected connection identities.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanExactPermission {
    pub scope: FinancialPlanExactScope,
    pub definition_hash: String,
    pub connections_hash: String,
}

/// Which verified expenditures a producing step must fund. Fees always retain
/// their own approved caps and available-balance checks.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinancialPlanFundingScope {
    AllDebits,
    PrincipalOnly,
}

/// A future input is available only from verified output of the named earlier
/// step, at this exact custody resource and asset identity.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanFundingDependency {
    pub producing_step_id: String,
    pub resource: crate::ResourceId,
    pub asset: AssetId,
    /// Omission retains legacy principal-plus-additional-cost funding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "all_debits (default) requires verified producer proceeds for principal and additional fees. principal_only requires producer proceeds only for principal; fees remain capped and must fit the available balance in addition to principal."
    )]
    pub funding_scope: Option<FinancialPlanFundingScope>,
    /// Maximum share of the producer's verified receipt that this dependency
    /// may consume, in basis points. This bounds agent-selected variable input
    /// such as swapping at most half of an upstream bridge receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 10000))]
    pub maximum_fraction_bps: Option<u16>,
}
impl FinancialPlanFundingDependency {
    pub fn includes_additional_costs(&self) -> bool {
        self.funding_scope != Some(FinancialPlanFundingScope::PrincipalOnly)
    }
}

/// Exact resource/asset identity that an operation may credit; amount is
/// deliberately absent because future venue proceeds are not yet known.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanFundingIdentity {
    pub resource: crate::ResourceId,
    pub asset: AssetId,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanPermission {
    pub spec: BoundarySpec,
    /// This step is an explicitly authorized cleanup action for an unresolved
    /// earlier write in the same plan. The engine resolves the parent to its
    /// durable operation and submission before preparing the cleanup attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_parent: Option<FinancialPlanRecoveryParent>,
    pub possible_credits: Vec<FinancialPlanFundingIdentity>,
    pub fee_assets: Vec<AssetId>,
    /// Bounded receipt capacity backed by an existing claim or other
    /// adapter-verified source that is not a plan debit, such as a deferred
    /// claim or withdrawal from an existing position. This grants no new debit
    /// authority; the adapter independently verifies the backing and exact
    /// receipt before settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_receipt: Option<ResourceAmount>,
    /// Optional adapter-imposed upper bound on the parent plan consent life.
    /// It is reviewed with the permission and applies to every step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_seconds: Option<u64>,
    /// Optional cap shared across multiple concrete steps in one adapter-owned
    /// parent action. Each step retains its own `spec`; the engine applies
    /// these cumulative caps once to the aggregate verified/reserved use for
    /// the stable scope ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_scope: Option<FinancialPlanCumulativeScope>,
}

/// A cleanup step that is authorized to reference one earlier write attempt.
/// JSON pointers identify the adapter's typed operation-id and Submission
/// fields; the engine fills them from the immutable parent journal record.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanRecoveryParent {
    pub step_id: String,
    pub operation_id_pointer: String,
    pub submission_pointer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_binding_fields: Vec<FinancialPlanRecoveryBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanRecoveryBinding {
    pub parent_binding_pointer: String,
    pub child_input_pointer: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanCumulativeScope {
    /// Gross newly incurred debt, independent of wallet debits. Verified debt
    /// decreases never replenish this authority. Older JSON scopes omit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maximum_obligations: Vec<ResourceAmount>,
    pub scope_id: String,
    pub maximum_debits: Vec<ResourceAmount>,
    pub maximum_costs: Vec<CostBound>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanAuthorizationRequest {
    pub request_id: String,
    pub plan: FinancialPlanSpec,
    /// Parent lifetime from native approval; omission uses the sequential planning budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_for_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_revision_id: Option<String>,
    /// Hash returned by the latest preview. Authorization fails before Touch ID
    /// if freshly prepared review material differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_review_hash: Option<String>,
}

impl FinancialPlanAuthorizationRequest {
    pub fn validate(&self) -> crate::Result<()> {
        self.plan.validate()?;
        if let Some(seconds) = self.valid_for_seconds {
            let Some(duration) = i64::try_from(seconds)
                .ok()
                .and_then(chrono::Duration::try_seconds)
            else {
                return Err(GoFinError::validation(
                    "Plan consent duration must be a positive representable duration.",
                ));
            };
            if duration <= chrono::Duration::zero() {
                return Err(GoFinError::validation(
                    "Plan consent duration must be a positive representable duration.",
                ));
            }
        }
        if self
            .replaces_revision_id
            .as_ref()
            .is_some_and(|revision| revision.trim().is_empty() || revision.len() > 256)
        {
            return Err(GoFinError::validation(
                "A replacement revision ID must be nonempty and at most 256 bytes.",
            ));
        }
        if self.expected_review_hash.as_ref().is_some_and(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }) {
            return Err(GoFinError::validation(
                "A plan review hash must be a lowercase SHA-256 digest.",
            ));
        }
        Ok(())
    }
}

impl FinancialPlanSpec {
    /// Validate the plan's stable structural contract and approval text.
    pub fn validate(&self) -> crate::Result<()> {
        if self.version != FINANCIAL_PLAN_VERSION {
            return Err(GoFinError::validation(
                "The financial plan version is unsupported.",
            ));
        }
        validate_financial_plan_name(&self.name)?;
        if let Some(summary) = &self.summary {
            // Leave room for the native approval prefix, step count and full
            // duration. Reject long text before approval instead of truncating
            // material financial words in the prompt.
            if summary.is_empty()
                || summary.trim() != summary
                || self.name.chars().count() + summary.chars().count() > 140
                || summary
                    .chars()
                    .any(|c| c.is_control() || is_bidi_format_control(c))
            {
                return Err(GoFinError::validation(
                    "Plan summaries must be one trimmed, nonempty line with no control or bidirectional formatting characters; name and summary together must contain at most 140 Unicode characters.",
                ));
            }
        }
        if self.steps.is_empty() {
            return Err(GoFinError::validation(
                "A financial plan must contain at least one step.",
            ));
        }
        let mut aggregate_identities = std::collections::BTreeSet::new();
        for debit in &self.aggregate_bounds.maximum_debits {
            if debit.amount.atoms.is_zero()
                || !aggregate_identities.insert((&debit.resource, &debit.amount.asset))
            {
                return Err(GoFinError::validation(
                    "Aggregate debit bounds must be positive and unique for each resource and asset.",
                ));
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for step in &self.steps {
            if step.step_id.trim().is_empty()
                || step.step_id.trim() != step.step_id
                || step.operation.trim().is_empty()
                || step.operation.trim() != step.operation
                || !ids.insert(&step.step_id)
            {
                return Err(GoFinError::validation(
                    "Plan step IDs and operation names must be nonempty, and step IDs must be unique.",
                ));
            }
            if step.completion.principal_spend.is_none()
                && step.completion.minimum_net_receipt.is_none()
                && !step.completion.operation_succeeded
            {
                return Err(GoFinError::validation(
                    "Every plan step must declare a spend target, a minimum net receipt, or verified operation success.",
                ));
            }
            if step
                .completion
                .principal_spend
                .iter()
                .chain(step.completion.minimum_net_receipt.iter())
                .any(|target| target.amount.atoms.is_zero())
            {
                return Err(GoFinError::validation(
                    "Plan completion targets must have a positive exact quantity.",
                ));
            }
        }
        for (index, step) in self.steps.iter().enumerate() {
            let mut seen_dependencies = std::collections::BTreeSet::new();
            for dependency in &step.funding_dependencies {
                if dependency
                    .maximum_fraction_bps
                    .is_some_and(|fraction| fraction == 0 || fraction > 10_000)
                {
                    return Err(GoFinError::validation(
                        "A funding fraction must be between 1 and 10000 basis points.",
                    ));
                }
                if !seen_dependencies.insert((
                    dependency.producing_step_id.as_str(),
                    &dependency.resource,
                    &dependency.asset,
                )) {
                    return Err(GoFinError::validation(
                        "A plan step cannot repeat the same funding dependency.",
                    ));
                }
                let Some(producer_index) = self
                    .steps
                    .iter()
                    .position(|s| s.step_id == dependency.producing_step_id)
                else {
                    return Err(GoFinError::validation(
                        "A funding dependency names an unknown producing step.",
                    ));
                };
                if producer_index >= index {
                    return Err(GoFinError::validation(
                        "Funding must depend on an earlier step.",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Names are labels, but must remain unambiguous in terminal and native review.
pub fn validate_financial_plan_name(name: &str) -> crate::Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 80
        || name
            .chars()
            .any(|c| c.is_control() || is_bidi_format_control(c))
    {
        return Err(GoFinError::validation(
            "Plan names must be trimmed, contain 1 to 80 Unicode characters, and have no control or bidirectional formatting characters.",
        ));
    }
    Ok(())
}

fn is_bidi_format_control(c: char) -> bool {
    matches!(
        c as u32,
        0x061c | 0x200e..=0x200f | 0x202a..=0x202e | 0x2028..=0x2029 | 0x2066..=0x2069
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanStepPermission {
    pub step_id: String,
    /// JSON pointers to string input fields assigned by the broker for each
    /// concrete attempt. The persisted plan input remains the caller's exact
    /// reviewed template.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_input_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<FinancialPlanExactPermission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PlanPermission>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanReview {
    pub plan: FinancialPlanSpec,
    /// Existing stable identity for a revision preview; new plans receive an
    /// identity only after authorization succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    pub permissions: Vec<FinancialPlanStepPermission>,
    pub review_hash: String,
    pub valid_for_seconds: u64,
    pub replaces_revision_id: Option<String>,
    pub carried_forward_usage: FinancialPlanUsage,
    pub remaining_debits: Vec<ResourceAmount>,
    pub remaining_costs: Vec<CostBound>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_warnings: Vec<GoFinError>,
    /// Feasibility checks deferred until the step has actual execution state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feasibility_warnings: Vec<String>,
}

pub fn financial_plan_review_binding(
    spec: &FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
    valid_for_seconds: u64,
    replaces_revision_id: Option<&str>,
    carried_forward_usage: &FinancialPlanUsage,
) -> Value {
    serde_json::json!({
        "version": 1,
        "kind": "financial_plan_review",
        "plan": spec,
        "permissions": permissions,
        "valid_for_seconds": valid_for_seconds,
        "replaces_revision_id": replaces_revision_id,
        "carried_forward_usage": carried_forward_usage,
    })
}

pub fn financial_plan_review_hash(
    spec: &FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
    valid_for_seconds: u64,
    replaces_revision_id: Option<&str>,
    carried_forward_usage: &FinancialPlanUsage,
) -> crate::Result<String> {
    crate::binding_hash(&financial_plan_review_binding(
        spec,
        permissions,
        valid_for_seconds,
        replaces_revision_id,
        carried_forward_usage,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanRevision {
    pub plan_id: String,
    pub revision_id: String,
    pub spec: FinancialPlanSpec,
    pub permissions: Vec<FinancialPlanStepPermission>,
    pub approval_binding_hash: String,
    pub approved_at: String,
    pub expires_at: String,
    pub valid_for_seconds: u64,
    pub status: FinancialPlanAuthorizationStatus,
    pub replaces_revision_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinancialPlanAuthorizationStatus {
    Active,
    Expired,
    Revoked,
    Superseded,
    Exhausted,
}

/// Cumulative verified use and open reservations, keyed by exact resource and
/// asset. Reservations are represented once and carry their step linkage.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanUsage {
    pub consumed_debits: Vec<ResourceAmount>,
    pub consumed_costs: Vec<CostBound>,
    pub reservations: Vec<FinancialPlanReservation>,
    pub verified_proceeds: Vec<FinancialPlanProceeds>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanReservation {
    pub attempt_id: String,
    pub step_id: String,
    pub debits: Vec<ResourceAmount>,
    pub costs: Vec<CostBound>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanProceeds {
    pub producing_step_id: String,
    pub resource: crate::ResourceId,
    pub asset: AssetId,
    pub available: crate::Quantity,
    pub consumed: crate::Quantity,
}

/// One canonical binding for one approval. Carries forward already consumed
/// usage so a revised scope cannot reset cumulative authority.
pub fn financial_plan_approval_binding(
    plan_id: &str,
    revision_id: &str,
    spec: &FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
    valid_for_seconds: u64,
    replaces_revision_id: Option<&str>,
    carried_forward_usage: &FinancialPlanUsage,
) -> Value {
    serde_json::json!({
        "version": 1,
        "kind": "financial_plan",
        "plan_id": plan_id,
        "revision_id": revision_id,
        "name": spec.name,
        "plan": spec,
        "permissions": permissions,
        "valid_for_seconds": valid_for_seconds,
        "replaces_revision_id": replaces_revision_id,
        "carried_forward_usage": carried_forward_usage,
    })
}

pub fn financial_plan_attempt_binding(
    prepared: &PreparedOperation,
    plan_id: &str,
    revision_id: &str,
    step_id: &str,
) -> Value {
    let mut candidate = prepared.clone();
    if let Some(binding) = candidate.binding.as_object_mut() {
        binding.remove(PLAN_METADATA_KEY);
    }
    let mut binding = serde_json::json!({
        "version": 1,
        "kind": "financial_plan_step_attempt",
        "plan_id": plan_id,
        "revision_id": revision_id,
        "step_id": step_id,
        "attempt": boundaries::attempt_binding(&candidate),
    });
    if let Some(deadline) = &candidate.action_deadline {
        binding["execution_input"] = candidate.input.clone();
        binding["action_deadline"] =
            serde_json::to_value(deadline).expect("deadline serialization");
    }
    binding
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanAttemptSummary {
    pub attempt_id: String,
    pub request_id: String,
    pub step_id: String,
    pub status: Status,
    pub submission: Option<Submission>,
    pub error: Option<GoFinError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinancialPlanStepProgressState {
    NotStarted,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanStepProgress {
    pub step_id: String,
    pub state: FinancialPlanStepProgressState,
    pub attempts: Vec<FinancialPlanAttemptSummary>,
    pub consumed_debits: Vec<ResourceAmount>,
    pub consumed_costs: Vec<CostBound>,
    pub verified_receipts: Vec<FinancialPlanProceeds>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanStatus {
    pub plan_id: String,
    pub revision: FinancialPlanRevision,
    pub usage: FinancialPlanUsage,
    pub steps: Vec<FinancialPlanStepProgress>,
    pub remaining_debits: Vec<ResourceAmount>,
    pub remaining_costs: Vec<CostBound>,
    pub next_eligible_step_id: Option<String>,
    pub blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_timing: Option<crate::plan_timing::PlanAuthorizationTiming>,
}

/// Durable linkage allows retries to recover their original revision binding.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanAttemptLinkage {
    pub plan_id: String,
    pub revision_id: String,
    pub step_id: String,
    pub attempt_id: String,
    pub request_fingerprint: String,
    pub execution_binding_hash: String,
    /// Original caller request ID shared by any automatically refreshed
    /// candidates for this logical plan-step request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_request_id: Option<String>,
    pub submission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// Protected representation; the store persists it opaquely.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPlanAuthorizationRecord {
    pub revision: FinancialPlanRevision,
    pub approval: ApprovalAudit,
    pub request_fingerprint: String,
    pub usage: FinancialPlanUsage,
    pub attempts: Vec<FinancialPlanAttemptLinkage>,
    pub active_attempt_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Amount, Quantity, ResourceId};

    fn sample_plan() -> FinancialPlanSpec {
        let asset = AssetId {
            network: "binance".into(),
            reference: "BTC".into(),
            decimals: 36,
        };
        let debit = ResourceAmount {
            resource: ResourceId("binance:account:synthetic".into()),
            amount: Amount {
                asset: asset.clone(),
                atoms: Quantity::from_u64(10),
            },
        };
        FinancialPlanSpec {
            version: FINANCIAL_PLAN_VERSION,
            name: "Synthetic sale".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                action_deadline_seconds: None,
                step_id: "sell".into(),
                operation: "binance.spot.order-place".into(),
                input: serde_json::json!({}),
                permitted_variations: serde_json::json!({}),
                completion: FinancialStepCompletion {
                    principal_spend: Some(debit.clone()),
                    minimum_net_receipt: None,
                    operation_succeeded: false,
                },
                funding_dependencies: vec![],
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![debit],
                maximum_costs: vec![],
            },
        }
    }

    fn plan_name(name: &str) -> crate::Result<()> {
        validate_financial_plan_name(name)
    }

    #[test]
    fn validates_display_name_unicode_and_bidi_controls() {
        assert!(plan_name("  ").is_err());
        assert!(plan_name(" leading").is_err());
        assert!(plan_name("trailing ").is_err());
        assert!(plan_name(&"é".repeat(80)).is_ok());
        assert!(plan_name(&"界".repeat(81)).is_err());
        assert!(plan_name("line\nbreak").is_err());
        assert!(plan_name("line\u{2028}break").is_err());
        assert!(plan_name("paragraph\u{2029}break").is_err());
        assert!(plan_name("hidden\u{202e}name").is_err());
        assert!(plan_name("Arabic \u{061c} label").is_err());
        assert!(plan_name("duplicate labels are allowed").is_ok());
    }

    #[test]
    fn validates_complete_summary_before_approval_without_truncation() {
        let mut plan = sample_plan();
        plan.summary = Some("Sell up to 10 BTC atoms within the reviewed fee limits.".into());
        plan.validate().unwrap();
        let remaining = 140 - plan.name.chars().count();
        plan.summary = Some("é".repeat(remaining));
        plan.validate().unwrap();
        plan.summary = Some("界".repeat(remaining + 1));
        assert!(plan.validate().is_err());
        for text in [
            "",
            " ",
            " leading",
            "trailing ",
            "line\nbreak",
            "tab\there",
            "escape\u{1b}",
            "line\u{2028}break",
            "paragraph\u{2029}break",
            "hidden\u{202e}amount",
            "Arabic\u{061c}amount",
            "isolate\u{2066}amount\u{2069}",
        ] {
            plan.summary = Some(text.into());
            assert!(plan.validate().is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn optional_summary_preserves_legacy_json_and_binds_changed_display_text() {
        let original = sample_plan();
        let legacy = serde_json::to_value(&original).unwrap();
        assert!(legacy.get("summary").is_none());
        let restored: FinancialPlanSpec = serde_json::from_value(legacy.clone()).unwrap();
        assert!(restored.summary.is_none());
        assert_eq!(serde_json::to_value(&restored).unwrap(), legacy);
        let usage = FinancialPlanUsage::default();
        let legacy_review = serde_json::json!({
            "version": 1, "kind": "financial_plan_review", "plan": legacy,
            "permissions": [], "valid_for_seconds": 600,
            "replaces_revision_id": null, "carried_forward_usage": usage,
        });
        let legacy_hash = crate::binding_hash(&legacy_review).unwrap();
        assert_eq!(
            legacy_hash,
            "bde77e6a82728c30b158bd000c06958c90d1ec7d8a752445e1112795f695647a"
        );
        assert_eq!(
            financial_plan_review_hash(&restored, &[], 600, None, &usage).unwrap(),
            legacy_hash,
        );
        let approval =
            financial_plan_approval_binding("plan", "revision", &restored, &[], 600, None, &usage);
        let mut summarized = restored;
        summarized.summary = Some("Sell up to 10 BTC atoms within the reviewed fee limits.".into());
        let value = serde_json::to_value(&summarized).unwrap();
        assert_eq!(
            serde_json::from_value::<FinancialPlanSpec>(value).unwrap(),
            summarized
        );
        let summary_hash = financial_plan_review_hash(&summarized, &[], 600, None, &usage).unwrap();
        assert_ne!(summary_hash, legacy_hash);
        let summarized_approval = financial_plan_approval_binding(
            "plan",
            "revision",
            &summarized,
            &[],
            600,
            None,
            &usage,
        );
        assert_ne!(
            crate::binding_hash(&approval).unwrap(),
            crate::binding_hash(&summarized_approval).unwrap()
        );
        summarized.summary = Some("Sell up to 9 BTC atoms within the reviewed fee limits.".into());
        assert_ne!(
            financial_plan_review_hash(&summarized, &[], 600, None, &usage).unwrap(),
            summary_hash
        );
    }

    #[test]
    fn legacy_plan_json_preserves_explicit_duration_and_omitted_deadlines() {
        let historical = serde_json::json!({"request_id":"legacy", "plan":sample_plan(), "valid_for_seconds":600});
        let request: FinancialPlanAuthorizationRequest =
            serde_json::from_value(historical.clone()).unwrap();
        assert_eq!(request.valid_for_seconds, Some(600));
        assert!(request.plan.steps[0].action_deadline_seconds.is_none());
        assert_eq!(serde_json::to_value(&request).unwrap(), historical);
        let mut automatic = historical;
        automatic
            .as_object_mut()
            .unwrap()
            .remove("valid_for_seconds");
        let request: FinancialPlanAuthorizationRequest =
            serde_json::from_value(automatic.clone()).unwrap();
        assert_eq!(request.valid_for_seconds, None);
        assert_eq!(serde_json::to_value(&request).unwrap(), automatic);
    }

    #[test]
    fn step_permission_generated_fields_are_optional_for_historical_json() {
        let historical = serde_json::json!({
            "step_id":"deposit",
            "exact":{
                "scope":{"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]},
                "definition_hash":"definition",
                "connections_hash":"connections"
            }
        });
        let permission: FinancialPlanStepPermission =
            serde_json::from_value(historical.clone()).unwrap();
        assert!(permission.generated_input_fields.is_empty());
        assert_eq!(serde_json::to_value(&permission).unwrap(), historical);

        let mut current = permission;
        current.generated_input_fields = vec!["/requestId".into()];
        assert_eq!(
            serde_json::to_value(current).unwrap()["generated_input_fields"],
            serde_json::json!(["/requestId"])
        );
    }

    #[test]
    fn plan_structure_checks_bounds_dependencies_and_duration() {
        let plan = sample_plan();
        plan.validate().unwrap();
        let mut duplicate = plan.clone();
        let mut dependent = duplicate.steps[0].clone();
        dependent.step_id = "buy".into();
        dependent.funding_dependencies = vec![
            FinancialPlanFundingDependency {
                funding_scope: None,
                maximum_fraction_bps: None,
                producing_step_id: "sell".into(),
                resource: ResourceId("binance:account:synthetic".into()),
                asset: AssetId {
                    network: "binance".into(),
                    reference: "BTC".into(),
                    decimals: 36,
                },
            },
            FinancialPlanFundingDependency {
                funding_scope: None,
                maximum_fraction_bps: None,
                producing_step_id: "sell".into(),
                resource: ResourceId("binance:account:synthetic".into()),
                asset: AssetId {
                    network: "binance".into(),
                    reference: "BTC".into(),
                    decimals: 36,
                },
            },
        ];
        duplicate.steps.push(dependent);
        assert!(duplicate.validate().is_err());
        let request = FinancialPlanAuthorizationRequest {
            request_id: "request-1".into(),
            plan,
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: Some("a".repeat(64)),
        };
        request.validate().unwrap();
        let mut invalid_duration = request;
        invalid_duration.valid_for_seconds = Some(u64::MAX);
        assert!(invalid_duration.validate().is_err());
    }

    #[test]
    fn plan_attempt_binding_removes_its_own_embedded_metadata() {
        let prepared = PreparedOperation {
            operation: "binance.spot.order-place".into(),
            input: serde_json::json!({}),
            binding: serde_json::json!({"execution":"synthetic","__fin_financial_plan":{"marker":true}}),
            summary: "synthetic".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:01:00Z".into(),
            valuation: None,
            intent: crate::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let binding = financial_plan_attempt_binding(&prepared, "plan-1", "revision-1", "sell");
        assert!(
            binding["attempt"]["execution"]
                .get(PLAN_METADATA_KEY)
                .is_none()
        );
        assert_eq!(binding["attempt"]["execution"]["execution"], "synthetic");
    }
    #[test]
    fn funding_scope_preserves_legacy_bytes_hash_and_rejects_unknown_scopes() {
        let legacy = r#"{"producing_step_id":"producer","resource":"wallet","asset":{"network":"ethereum:1","reference":"native","decimals":18}}"#;
        let dependency: FinancialPlanFundingDependency = serde_json::from_str(legacy).unwrap();
        assert!(dependency.funding_scope.is_none());
        assert!(dependency.maximum_fraction_bps.is_none());
        assert!(dependency.includes_additional_costs());
        assert_eq!(serde_json::to_string(&dependency).unwrap(), legacy);
        let legacy_hash = crate::binding_hash(&serde_json::to_value(&dependency).unwrap()).unwrap();
        assert_eq!(
            legacy_hash,
            "ba84f9a70996d51d9256e78d23f8c2baaa711899a1b1ab7cb296e74412c0f164"
        );
        for scope in [
            FinancialPlanFundingScope::AllDebits,
            FinancialPlanFundingScope::PrincipalOnly,
        ] {
            let mut explicit = dependency.clone();
            explicit.funding_scope = Some(scope);
            let value = serde_json::to_value(&explicit).unwrap();
            assert_ne!(crate::binding_hash(&value).unwrap(), legacy_hash);
            assert_eq!(
                serde_json::from_value::<FinancialPlanFundingDependency>(value).unwrap(),
                explicit
            );
        }
        let mut bad = serde_json::to_value(dependency).unwrap();
        bad["funding_scope"] = serde_json::json!("future_balance");
        assert!(serde_json::from_value::<FinancialPlanFundingDependency>(bad).is_err());
        let schema =
            serde_json::to_value(schemars::schema_for!(FinancialPlanFundingDependency)).unwrap();
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "funding_scope")
        );
        assert_eq!(
            schema["$defs"]["FinancialPlanFundingScope"]["enum"],
            serde_json::json!(["all_debits", "principal_only"])
        );
    }

    #[test]
    fn funding_fraction_is_bounded_and_included_in_the_plan_contract() {
        let mut plan = sample_plan();
        let mut consumer = plan.steps[0].clone();
        consumer.step_id = "consumer".into();
        consumer.funding_dependencies = vec![FinancialPlanFundingDependency {
            producing_step_id: "sell".into(),
            resource: ResourceId("binance:account:synthetic".into()),
            asset: plan.steps[0]
                .completion
                .principal_spend
                .as_ref()
                .unwrap()
                .amount
                .asset
                .clone(),
            funding_scope: None,
            maximum_fraction_bps: Some(5_000),
        }];
        plan.steps[0].completion.principal_spend = None;
        plan.steps[0].completion.operation_succeeded = true;
        plan.steps.push(consumer.clone());
        plan.validate().unwrap();
        assert_eq!(
            plan.steps[1].funding_dependencies[0].maximum_fraction_bps,
            Some(5_000)
        );

        plan.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(10_001);
        assert!(plan.validate().is_err());
        plan.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(0);
        assert!(plan.validate().is_err());
    }
}
