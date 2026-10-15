//! Ordered conversions funded by one initial external principal and proved credits.
use crate::{Amount, AssetId, CostTreatment, Quantity, ResourceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionLegConstraint {
    pub id: String,
    pub input: AssetId,
    pub maximum_input: Quantity,
    pub output: AssetId,
    pub minimum_output: Quantity,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionSequenceConstraint {
    pub accounting_version: u32,
    pub sequence_id: String,
    pub resource: ResourceId,
    pub external_input: AssetId,
    pub maximum_external_input: Quantity,
    pub legs: Vec<ConversionLegConstraint>,
    /// Complete fee ceilings by exact asset, shared across all stages and treatments.
    pub maximum_costs: Vec<ConversionCostBound>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversionLegStatus {
    Pending,
    /// Native order is terminal; execution quantities may be partial or zero.
    Terminal,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionLeg {
    pub id: String,
    pub status: ConversionLegStatus,
    pub spent: Amount,
    pub received: Amount,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversionCostFunding {
    External,
    GeneratedCredit,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionCostBound {
    pub amount: Amount,
    pub funding: ConversionCostFunding,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionCost {
    pub leg_id: String,
    pub amount: Amount,
    pub category: String,
    /// Venue meaning on this exact leg; generated fees are projected against
    /// the sequence's prior credit, without requiring an initial holding there.
    pub treatment: CostTreatment,
    pub funding: ConversionCostFunding,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversionSequence {
    pub accounting_version: u32,
    pub sequence_id: String,
    pub resource: ResourceId,
    pub legs: Vec<ConversionLeg>,
    pub costs: Vec<ConversionCost>,
}
