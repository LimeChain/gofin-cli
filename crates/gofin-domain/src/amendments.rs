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

/// Original financial owners touched by a native mutation and the scope the
/// candidate was prepared against. Admission must compare these with durable
/// state in the same transaction that records the submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationAdmissionTarget {
    pub operation_id: String,
    pub previous_scope: Option<AmendmentScope>,
    pub requires_previous_scope: bool,
}

pub fn mutation_admission_targets(
    prepared: &PreparedOperation,
) -> crate::Result<Vec<MutationAdmissionTarget>> {
    use crate::{EffectConstraint, compound_mutations::CompoundAmendmentContext};
    use std::collections::BTreeMap;
    let mut targets = BTreeMap::<String, (Option<AmendmentScope>, bool)>::new();
    let context = prepared
        .binding
        .get("unifiedOriginalContext")
        .or_else(|| prepared.binding.get("compoundContext"))
        .map(|v| serde_json::from_value::<CompoundAmendmentContext>(v.clone()))
        .transpose()
        .map_err(|_| crate::GoFinError::validation("Invalid original mutation context."))?;
    let mut add =
        |id: &str, prior: Option<AmendmentScope>, requires_scope: bool| -> crate::Result<()> {
            if id.is_empty() {
                return Err(crate::GoFinError::validation(
                    "Mutation original identity is missing.",
                ));
            }
            if let Some((old, old_requires)) = targets.get_mut(id) {
                if *old_requires && requires_scope && *old != prior {
                    return Err(crate::GoFinError::validation(
                        "Mutation original scopes disagree.",
                    ));
                }
                if requires_scope {
                    *old = prior;
                    *old_requires = true;
                }
            } else {
                targets.insert(id.to_owned(), (prior, requires_scope));
            }
            Ok(())
        };
    for effect in &prepared.intent.expected {
        match effect {
            EffectConstraint::SubmissionAmendment { operation_id, .. } => {
                let prior = prepared
                    .binding
                    .get("amendmentPreviousScope")
                    .map(|v| serde_json::from_value(v.clone()))
                    .transpose()
                    .map_err(|_| {
                        crate::GoFinError::validation("Invalid previous amendment scope.")
                    })?;
                add(operation_id, prior, true)?;
            }
            EffectConstraint::GroupedSubmissionAmendments { group } => {
                add(
                    &group.operation_id,
                    Some((*group.original_scope).clone()),
                    true,
                )?;
            }
            EffectConstraint::CompoundSubmissionMutations { children } => {
                for child in children {
                    if matches!(
                        child.mutation,
                        crate::compound_mutations::ChildMutation::Cancel
                    ) {
                        continue;
                    }
                    let prior =
                        context
                            .as_ref()
                            .and_then(|ctx| ctx.children.get(&child.child_tag))
                            .filter(|linked| linked.operation_id == child.operation_id)
                            .and_then(|linked| {
                                linked.context.effective_scope().cloned().or_else(|| {
                                    linked.context.original.as_ref().map(|original| {
                                        AmendmentScope {
                                            intent: original.intent.clone(),
                                            binding: original.binding.clone(),
                                            valuation: original.valuation.clone(),
                                        }
                                    })
                                })
                            });
                    add(
                        &child.operation_id,
                        prior,
                        matches!(
                            child.mutation,
                            crate::compound_mutations::ChildMutation::Amend { .. }
                        ),
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(targets
        .into_iter()
        .map(
            |(operation_id, (previous_scope, requires_previous_scope))| MutationAdmissionTarget {
                operation_id,
                previous_scope,
                requires_previous_scope,
            },
        )
        .collect())
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::{
        EffectConstraint, FinancialIntent,
        compound_mutations::{ChildMutation, MutationChild},
    };
    use serde_json::json;

    fn candidate() -> PreparedOperation {
        serde_json::from_value(json!({"operation":"test.edit","input":{},"binding":{},"summary":"edit",
            "checks":[],"warnings":[],"observed_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:01:00Z",
            "operation_version":1,"intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]}})).unwrap()
    }
    fn submission() -> Submission {
        Submission {
            kind: "venue".into(),
            reference: "original".into(),
            data: json!({}),
        }
    }
    #[test]
    fn targets_retain_per_owner_scope_and_include_cancellations() {
        let original = candidate();
        let scope = AmendmentScope {
            intent: FinancialIntent::default(),
            binding: json!({"price":"1"}),
            valuation: None,
        };
        let mut edit = candidate();
        edit.binding["amendmentPreviousScope"] = serde_json::to_value(&scope).unwrap();
        edit.intent
            .expected
            .push(EffectConstraint::SubmissionAmendment {
                operation_id: "one".into(),
                submission: submission(),
                revision: Box::new(scope.clone()),
            });
        let targets = mutation_admission_targets(&edit).unwrap();
        assert_eq!(targets[0].previous_scope, Some(scope.clone()));
        assert!(targets[0].requires_previous_scope);
        edit.intent.expected.clear();
        edit.intent
            .expected
            .push(EffectConstraint::SubmissionCancellation {
                operation_id: "two".into(),
                submission: submission(),
            });
        assert!(mutation_admission_targets(&edit).unwrap().is_empty());
        edit.intent.expected.clear();
        edit.binding = json!({"compoundContext":{"children":{"tag":{"operation_id":"one","context":{"original":original,"revisions":[]}}}}});
        edit.intent
            .expected
            .push(EffectConstraint::CompoundSubmissionMutations {
                children: vec![MutationChild {
                    child_tag: "tag".into(),
                    operation_id: "one".into(),
                    submission: submission(),
                    mutation: ChildMutation::Amend {
                        revision: Box::new(scope),
                    },
                    incremental_valuation: None,
                }],
            });
        let targets = mutation_admission_targets(&edit).unwrap();
        assert!(targets[0].previous_scope.is_some());
        assert!(targets[0].requires_previous_scope);
        edit.binding["compoundContext"]["children"]["tag"]["operation_id"] = json!("different");
        assert!(
            mutation_admission_targets(&edit).unwrap()[0]
                .previous_scope
                .is_none()
        );
    }
}
