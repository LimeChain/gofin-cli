//! Exact operation-linked authority revisions. Original receipts retain one owner.
use crate::{
    FinancialIntent, OperationRecord, PreparedOperation, Submission, Valuation, VerifiedEffects,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmendmentScope {
    pub intent: FinancialIntent,
    pub binding: Value,
    pub valuation: Option<Valuation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovedAmendment {
    pub amendment_operation_id: String,
    pub amendment_submission: Submission,
    pub scope: AmendmentScope,
    pub successor: Option<Submission>,
    pub proof_digest: String,
}
/// Read-only native identities from the protected original journal. This is
/// adapter context, never input-supplied authority or a replacement approval.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OriginalSubmissionEvidence {
    pub submission: Option<Submission>,
    pub dispatch_evidence: Option<Value>,
    pub verified_effects: Option<VerifiedEffects>,
    pub created_at: String,
}
impl OriginalSubmissionEvidence {
    pub fn capture(record: &OperationRecord) -> Self {
        Self {
            submission: record.submission.clone(),
            dispatch_evidence: record.dispatch_evidence.clone(),
            verified_effects: record.effects.clone(),
            created_at: record.created_at.clone(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmendmentContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_evidence: Option<OriginalSubmissionEvidence>,
    pub original: Option<PreparedOperation>,
    pub revisions: Vec<ApprovedAmendment>,
}
impl AmendmentContext {
    pub fn effective_scope(&self) -> Option<&AmendmentScope> {
        self.revisions.last().map(|revision| &revision.scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn old_contexts_remain_readable_and_original_evidence_is_closed_typed_metadata() {
        let old: AmendmentContext =
            serde_json::from_value(json!({"original":null,"revisions":[]})).unwrap();
        assert!(old.original_evidence.is_none());
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            json!({"original":null,"revisions":[]})
        );
        assert!(serde_json::from_value::<OriginalSubmissionEvidence>(json!({"submission":null,"dispatch_evidence":null,"verified_effects":null,"created_at":"2026-10-10T00:00:00Z","authority":"caller-grant"})).is_err());
    }
}
