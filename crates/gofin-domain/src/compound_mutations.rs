//! Immutable child ownership for one protected native mutation batch.
use crate::{
    Submission,
    amendments::{AmendmentContext, AmendmentScope},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MutationChild {
    pub child_tag: String,
    pub operation_id: String,
    pub submission: Submission,
    pub mutation: ChildMutation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incremental_valuation: Option<crate::Valuation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildMutation {
    Amend { revision: Box<AmendmentScope> },
    Cancel,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildMutationOutcome {
    Accepted { successor: Option<Submission> },
    Rejected { code: String },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MutationChildProof {
    pub child_tag: String,
    pub operation_id: String,
    pub submission: Submission,
    pub outcome: ChildMutationOutcome,
    pub native_evidence: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinkedAmendmentContext {
    pub operation_id: String,
    pub context: AmendmentContext,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompoundAmendmentContext {
    pub children: BTreeMap<String, LinkedAmendmentContext>,
}
