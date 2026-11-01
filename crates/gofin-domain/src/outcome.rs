//! Durable observations and read-time projection of an operation's outcome.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! states { ($name:ident { $($variant:ident),+ $(,)? }) => {
    #[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum $name { $($variant),+ }
}; }
states!(Dispatch {
    NotDispatched,
    Dispatched,
    Uncertain,
    Rejected
});
states!(OutcomeResult {
    Pending,
    Partial,
    Executed,
    Failed,
    Cancelled,
    Unknown
});
states!(TransferState {
    SourcePending,
    InTransit,
    Credited,
    Available,
    ReturnedToSource,
    Unknown
});
states!(Reporting {
    Complete,
    Partial,
    Unavailable
});
states!(Phase {
    Failed,
    Done,
    NeedsYou,
    Unconfirmed,
    InProgress
});
states!(RetryApproval { Covered, Required });

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attention {
    None,
    ActionRequired {
        venue: String,
        reason: String,
        where_to_act: String,
    },
    Discrepancy {
        actual: Value,
        approved: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_transfer_progress_never_makes_whole_action_replay_safe() {
        for transfer in [
            TransferState::InTransit,
            TransferState::Credited,
            TransferState::Available,
            TransferState::ReturnedToSource,
        ] {
            let facts = OutcomeFacts {
                dispatch: Dispatch::Rejected,
                result: OutcomeResult::Failed,
                transfer: Some(transfer),
                attention: Attention::None,
                reporting: Reporting::Partial,
                duplicate_execution_excluded: true,
                next_check_after: None,
                observed_at: "2026-01-01T00:00:00Z".into(),
            };
            let outcome = facts.project(false, true);
            assert!(!outcome.replay_safe);
            assert!(outcome.retry_approval.is_none());
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct OutcomeFacts {
    pub dispatch: Dispatch,
    pub result: OutcomeResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferState>,
    pub attention: Attention,
    pub reporting: Reporting,
    /// Correlated proof that this logical action cannot execute later or duplicate a prior execution.
    #[serde(default)]
    pub duplicate_execution_excluded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_check_after: Option<String>,
    pub observed_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct OutcomeObservation {
    pub facts: OutcomeFacts,
    pub source: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct NextAction {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct OperationOutcome {
    pub version: u32,
    pub phase: Phase,
    pub dispatch: Dispatch,
    pub result: OutcomeResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferState>,
    pub attention: Attention,
    pub reporting: Reporting,
    pub replay_safe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_approval: Option<RetryApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_check_after: Option<String>,
    pub next_action: NextAction,
    pub observed_at: String,
}
impl OutcomeFacts {
    /// Approval validity is evaluated by the caller at read time, never stored here.
    pub fn project(&self, horizon_passed: bool, approval_valid: bool) -> OperationOutcome {
        let failed = self.dispatch == Dispatch::Rejected || self.result == OutcomeResult::Failed;
        let transfer_done = self.transfer.as_ref().is_none_or(|t| {
            matches!(
                t,
                TransferState::Credited
                    | TransferState::Available
                    | TransferState::ReturnedToSource
            )
        });
        let done = matches!(
            self.result,
            OutcomeResult::Executed | OutcomeResult::Cancelled
        ) && transfer_done;
        let phase = if failed {
            Phase::Failed
        } else if done {
            Phase::Done
        } else if matches!(self.attention, Attention::ActionRequired { .. }) {
            Phase::NeedsYou
        } else if horizon_passed {
            Phase::Unconfirmed
        } else {
            Phase::InProgress
        };
        // An external failure alone is not proof that no earlier stage executed.
        let replay_safe =
            !matches!(
                self.result,
                OutcomeResult::Executed | OutcomeResult::Partial
            ) && !matches!(
                self.transfer,
                Some(
                    TransferState::InTransit
                        | TransferState::Credited
                        | TransferState::Available
                        | TransferState::ReturnedToSource
                )
            ) && (matches!(self.dispatch, Dispatch::NotDispatched | Dispatch::Rejected)
                || self.duplicate_execution_excluded);
        let retry_approval = replay_safe.then_some({
            if approval_valid
                && matches!(self.dispatch, Dispatch::NotDispatched | Dispatch::Rejected)
            {
                RetryApproval::Covered
            } else {
                RetryApproval::Required
            }
        });
        let (code, message) = match phase {
            Phase::NeedsYou => (
                "act_at_venue",
                "Follow the venue's instructions, then check this operation again.",
            ),
            Phase::Failed if replay_safe => (
                "retry",
                "The attempt did not execute; check whether a new approval is required before retrying.",
            ),
            Phase::Done => ("none", "No further action is needed."),
            _ => (
                "wait_and_check",
                "Check this operation again; do not submit it again.",
            ),
        };
        OperationOutcome {
            version: 1,
            phase,
            dispatch: self.dispatch.clone(),
            result: self.result.clone(),
            transfer: self.transfer.clone(),
            attention: self.attention.clone(),
            reporting: self.reporting.clone(),
            replay_safe,
            retry_approval,
            next_check_after: self.next_check_after.clone(),
            next_action: NextAction {
                code: code.into(),
                message: message.into(),
                continuation_token: None,
                operation_id: None,
                command: None,
                arguments: None,
            },
            observed_at: self.observed_at.clone(),
        }
    }
}

/// Economic terms are reviewed separately from each exact candidate payload.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TermEnforcement {
    Enforced,
    Estimate,
    AfterExecution,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct NativeIntentTerm {
    pub name: String,
    pub value: Value,
    pub enforcement: TermEnforcement,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct NativeIntent {
    pub operation: String,
    pub terms: Vec<NativeIntentTerm>,
    pub expires_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ExecutionStage {
    pub id: String,
    pub state: StageState,
    pub candidate: Value,
}
states!(StageState {
    Ready,
    Admitted,
    Dispatched,
    Completed,
    Failed
});
