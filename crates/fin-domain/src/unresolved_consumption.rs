//! Proven costs may consume budget while execution remains unresolved.
//! This evidence never grants wider authority or releases reservations.
use crate::{FinError, FinancialEffect, Reconciliation, Result, Submission, binding_hash};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub const KEY: &str = "_fin_unresolved_cost_consumption_v1";
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumptionScope {
    #[default]
    WholeOperation,
    NewMutationFinancialScope,
}
impl ConsumptionScope {
    pub fn is_whole(&self) -> bool {
        *self == Self::WholeOperation
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumptionReason {
    #[default]
    CostBoundExceeded,
    ProvenUnsettledCost,
}
impl ConsumptionReason {
    pub fn is_bound_exceeded(&self) -> bool {
        *self == Self::CostBoundExceeded
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedCostConsumption {
    pub evidence_version: u32,
    #[serde(default, skip_serializing_if = "ConsumptionScope::is_whole")]
    pub scope: ConsumptionScope,
    #[serde(default, skip_serializing_if = "ConsumptionReason::is_bound_exceeded")]
    pub reason: ConsumptionReason,
    pub submission: Submission,
    pub costs: Vec<FinancialEffect>,
    pub evidence: Value,
    pub observed_at: String,
    pub proof_digest: String,
}
pub fn digest(proof: &UnresolvedCostConsumption) -> Result<String> {
    let mut value = serde_json::to_value(proof)
        .map_err(|_| invalid("The typed unresolved cost proof cannot be serialized."))?;
    value.as_object_mut().unwrap().remove("proof_digest");
    binding_hash(&value)
}
fn invalid(message: &str) -> FinError {
    FinError::new(
        "INVALID_UNRESOLVED_CONSUMPTION",
        "reconciliation",
        message,
        "Preserve the original submission and reservations; recover exact native cost evidence.",
    )
}
impl UnresolvedCostConsumption {
    pub fn new(
        submission: Submission,
        costs: Vec<FinancialEffect>,
        evidence: Value,
        observed_at: String,
    ) -> Result<Self> {
        let mut proof = Self {
            evidence_version: 1,
            scope: ConsumptionScope::WholeOperation,
            reason: ConsumptionReason::CostBoundExceeded,
            submission,
            costs,
            evidence,
            observed_at,
            proof_digest: String::new(),
        };
        proof.proof_digest = digest(&proof)?;
        Ok(proof)
    }
}
/// Adapter-only metadata. The broker validates scope, identity, costs, prices
/// and immutable events independently before recording a consumption watermark.
pub fn attach(
    reconciliation: &mut Reconciliation,
    proof: &UnresolvedCostConsumption,
) -> Result<()> {
    let fields = reconciliation
        .evidence
        .as_object_mut()
        .ok_or_else(|| invalid("Unresolved reconciliation evidence must be an object."))?;
    if fields.contains_key(KEY) {
        return Err(invalid(
            "Unresolved consumption proof was supplied more than once.",
        ));
    }
    fields.insert(
        KEY.into(),
        serde_json::to_value(proof)
            .map_err(|_| invalid("The typed unresolved proof cannot be serialized."))?,
    );
    Ok(())
}
pub fn read(evidence: &Value) -> Result<Option<UnresolvedCostConsumption>> {
    evidence
        .get(KEY)
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|_| invalid("Unresolved consumption metadata is malformed."))
        })
        .transpose()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn digest_and_reserved_metadata_preserve_exact_submission_and_closed_shape() {
        let proof = UnresolvedCostConsumption::new(
            Submission {
                kind: "synthetic".into(),
                reference: "synthetic-submission".into(),
                data: json!({}),
            },
            vec![],
            json!({"proofEvents":[{"eventId":"synthetic-fee-1","amount":"1"}]}),
            crate::now(),
        )
        .unwrap();
        let mut reconciliation = Reconciliation {
            status: crate::Status::Unknown,
            evidence: json!({"unresolved":true}),
            verified: None,
        };
        attach(&mut reconciliation, &proof).unwrap();
        assert_eq!(read(&reconciliation.evidence).unwrap(), Some(proof.clone()));
        assert!(attach(&mut reconciliation, &proof).is_err());
        let mut modified = proof.clone();
        modified.submission.reference = "different".into();
        assert_ne!(digest(&modified).unwrap(), proof.proof_digest);
        let mut value = serde_json::to_value(proof).unwrap();
        value["grant"] = json!(true);
        assert!(serde_json::from_value::<UnresolvedCostConsumption>(value).is_err());
    }
    #[test]
    fn default_scope_and_reason_keep_legacy_json_and_digest_unchanged() {
        let proof = UnresolvedCostConsumption::new(
            Submission {
                kind: "synthetic".into(),
                reference: "old-submit".into(),
                data: json!({}),
            },
            vec![],
            json!({"proofEvents":[]}),
            crate::now(),
        )
        .unwrap();
        let value = serde_json::to_value(&proof).unwrap();
        assert!(value.get("scope").is_none());
        assert!(value.get("reason").is_none());
        let old: UnresolvedCostConsumption = serde_json::from_value(value).unwrap();
        assert_eq!(old, proof);
        assert_eq!(digest(&old).unwrap(), proof.proof_digest);
        let mut scoped = old.clone();
        scoped.scope = ConsumptionScope::NewMutationFinancialScope;
        assert_ne!(digest(&scoped).unwrap(), proof.proof_digest);
        let mut unsettled = old;
        unsettled.reason = ConsumptionReason::ProvenUnsettledCost;
        assert_ne!(digest(&unsettled).unwrap(), proof.proof_digest);
    }
}
