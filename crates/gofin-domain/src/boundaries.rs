//! Durable, exchange-neutral contracts for bounded financial authorizations.
//!
//! Boundaries describe permission for a logical intent. Concrete requests and
//! submissions remain separate operation records with their own identities.

use crate::{CostBound, OperationRecord, PreparedOperation, ResourceAmount, Submission};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Broker-owned metadata carried inside a prepared binding so existing
/// adapters can verify approval without confusing grant and attempt identity.
pub const BOUNDARY_METADATA_KEY: &str = "__gofin_boundary_authorization";

pub fn authorization_binding(
    operation: &str,
    intent_id: &str,
    spec: &BoundarySpec,
    valid_for_seconds: u64,
) -> Value {
    serde_json::json!({
        "version": 1,
        "kind": "operation_boundaries",
        "operation": operation,
        "intent_id": intent_id,
        "fixed": spec.fixed,
        "parameters": spec.parameters,
        "maximum_debits": spec.maximum_debits,
        "maximum_costs": spec.maximum_costs,
        "valid_for_seconds": valid_for_seconds,
    })
}

/// Exact immutable execution binding for one attempt. Dynamic USD evidence and
/// broker metadata are intentionally excluded; exact inputs, typed intent and
/// adapter execution fields remain bound.
pub fn attempt_binding(prepared: &PreparedOperation) -> Value {
    let mut execution = prepared.binding.clone();
    if let Some(object) = execution.as_object_mut() {
        crate::product_names::remove(object, BOUNDARY_METADATA_KEY);
        crate::product_names::remove(object, crate::value_bounds::KEY);
        // A finite bridge step is candidate-bound by its transfer, step and
        // kind. The grant ID is injected by Engine only after authorization
        // and must not change the hash that identifies that candidate.
        let bridge_key = if object.contains_key(crate::BRIDGE_STEP_METADATA_KEY) {
            crate::BRIDGE_STEP_METADATA_KEY.to_owned()
        } else {
            crate::product_names::legacy_key(crate::BRIDGE_STEP_METADATA_KEY).unwrap()
        };
        if let Some(bridge_step) = object.get_mut(&bridge_key).and_then(Value::as_object_mut) {
            bridge_step.remove("grant_id");
        }
    }
    serde_json::json!({
        "operation": prepared.operation,
        "operation_version": prepared.operation_version,
        "execution": execution,
        "financial_intent": prepared.intent,
    })
}

fn default_valid_for_seconds() -> u64 {
    600
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAuthorizationRequest {
    pub request_id: String,
    pub operation: String,
    pub input: Value,
    /// Operation-family-specific limits supplied by the requester and reviewed
    /// by the adapter before they are bound to human approval.
    pub bounds: Value,
    #[serde(default = "default_valid_for_seconds")]
    pub valid_for_seconds: u64,
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundarySpec {
    /// Identity and scope fields that remain fixed for every candidate.
    pub fixed: Value,
    /// The explicitly permitted varying fields, including their constraints.
    pub parameters: Value,
    pub maximum_debits: Vec<ResourceAmount>,
    pub maximum_costs: Vec<CostBound>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryViolation {
    pub constraint: String,
    pub approved_bound: Value,
    pub candidate_value: Value,
    pub evidence_time: Option<String>,
    /// Accepting this candidate as proposed requires changed human-approved
    /// boundaries; revising the candidate to existing bounds may still work.
    pub reauthorization_required: bool,
    pub reconciliation_required: bool,
}

pub fn violation_error(message: &str, violation: BoundaryViolation) -> crate::GoFinError {
    crate::GoFinError::new(
        "BOUNDARY_VIOLATION",
        "authorization",
        message,
        if violation.reconciliation_required {
            "Reconcile the original attempt before proposing another candidate."
        } else if violation.reauthorization_required {
            "Revise the candidate to existing bounds, or review changed boundaries with fresh authorization."
        } else {
            "Revise the candidate to stay within the approved boundaries."
        },
    )
    .with_details(serde_json::json!({ "boundary_violation": violation }))
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryAuthorizationStatus {
    Active,
    Expired,
    Revoked,
    Superseded,
    Exhausted,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAuthorization {
    /// Opaque protected-state reference; this is never an approval receipt.
    pub id: String,
    pub intent_id: String,
    pub operation: String,
    pub bounds: Value,
    pub spec: BoundarySpec,
    pub approved_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_source: Option<crate::AuthorizationSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_provenance: Option<crate::YoloAuthorizationProvenance>,
    pub valid_for_seconds: u64,
    pub status: BoundaryAuthorizationStatus,
    pub remaining_debits: Vec<ResourceAmount>,
    pub remaining_costs: Vec<CostBound>,
    pub spent_debits: Vec<ResourceAmount>,
    pub spent_costs: Vec<CostBound>,
    pub replaces: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAttemptSummary {
    pub id: String,
    pub request_id: String,
    pub operation: String,
    pub input: Value,
    pub status: crate::Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_source: Option<crate::AuthorizationSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_provenance: Option<crate::YoloAuthorizationProvenance>,
    pub submission: Option<Submission>,
    pub effects: Option<crate::VerifiedEffects>,
    pub error: Option<crate::GoFinError>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryStatus {
    pub authorization: BoundaryAuthorization,
    pub attempts: Vec<BoundaryAttemptSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryPreview {
    pub authorization_id: String,
    pub operation: String,
    pub input: Value,
    pub prepared: PreparedOperation,
    pub spec: BoundarySpec,
    pub remaining_debits: Vec<ResourceAmount>,
    pub remaining_costs: Vec<CostBound>,
}

/// Protected engine-side representation for generic store serialization.
/// The store persists this value opaquely and does no financial interpretation.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryAuthorizationRecord {
    pub authorization: BoundaryAuthorization,
    pub approval: ApprovalAudit,
    pub request_fingerprint: String,
    pub attempts: Vec<String>,
    pub active_attempt: Option<String>,
}

pub type BoundaryAttemptRecord = OperationRecord;

pub use crate::ApprovalAudit;
