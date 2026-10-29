//! Explicit human dispositions and evidence selectors, never fabricated proofs.
use crate::{CostBound, ResourceAmount};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationResolutionOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationResolutionInput {
    pub outcome: OperationResolutionOutcome,
    pub reason: String,
    pub actual_debits: Vec<ResourceAmount>,
    pub actual_costs: Vec<CostBound>,
    pub reported_receipts: Vec<ResourceAmount>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReferenceKind {
    VenueReference,
    TransactionHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReference {
    pub kind: RecoveryReferenceKind,
    pub reference: String,
}
