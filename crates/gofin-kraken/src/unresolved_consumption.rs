//! Application evidence is independently typed before shared budget retention.
use gofin_domain::{
    CostBound, FinancialEffect, Quantity, Reconciliation, Result, Status, Submission, binding_hash,
    now,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
/// Compare independently correlated costs against the protected exact accounting
/// envelope. Unknown identities are not turned into alleged consumed budget.
pub(crate) fn costs_exceed_bounds(
    effects: &[FinancialEffect],
    bounds: &[CostBound],
) -> Result<bool> {
    let mut ceilings: BTreeMap<String, Quantity> = BTreeMap::new();
    for bound in bounds {
        let key = binding_hash(&json!([
            bound.location,
            bound.amount.asset,
            bound.treatment
        ]))?;
        let total = ceilings.entry(key).or_default();
        *total = total.checked_add(&bound.amount.atoms)?;
    }
    let mut actual: BTreeMap<String, Quantity> = BTreeMap::new();
    for effect in effects {
        if let FinancialEffect::Cost {
            location,
            amount,
            treatment,
            ..
        } = effect
        {
            if amount.atoms.is_zero() {
                continue;
            }
            let key = binding_hash(&json!([location, amount.asset, treatment]))?;
            if !ceilings.contains_key(&key) {
                return Err(crate::profile::error(
                    "KRAKEN_COST_IDENTITY_UNAPPROVED",
                    "The proven native cost has no exact protected account, asset and treatment ceiling. Preserve the receipt without inventing asset value or consumed budget.",
                ));
            }
            let total = actual.entry(key).or_default();
            *total = total.checked_add(&amount.atoms)?;
        }
    }
    Ok(actual
        .into_iter()
        .any(|(key, quantity)| quantity > ceilings[&key]))
}
pub(crate) fn cost_overrun(
    submission: &Submission,
    effects: &[FinancialEffect],
    evidence: Value,
) -> Result<Reconciliation> {
    let costs = effects
        .iter()
        .filter(
            |effect| matches!(effect,FinancialEffect::Cost{amount,..} if !amount.atoms.is_zero()),
        )
        .cloned()
        .collect();
    let proof = gofin_domain::unresolved_consumption::UnresolvedCostConsumption::new(
        submission.clone(),
        costs,
        json!({"proofEvents":evidence["proofEvents"]}),
        now(),
    )?;
    let mut reconciliation = crate::trading::unresolved(
        Status::Unknown,
        "Proven native costs exceed the approved ceiling. Preserve consumed budget, original receipts and reservations without expanding authority or replaying.",
        evidence,
    );
    gofin_domain::unresolved_consumption::attach(&mut reconciliation, &proof)?;
    Ok(reconciliation)
}

/// A breached new-financial partition may advance independently proven native
/// mutation outcomes, never its newly observed financial ledger rows.
pub(crate) fn new_write_overrun_with_progress(
    record: &gofin_domain::OperationRecord,
    submission: &Submission,
    financial_effects: &[FinancialEffect],
    markers: Vec<FinancialEffect>,
    evidence: Value,
) -> Result<Reconciliation> {
    let overrun = cost_overrun(submission, financial_effects, evidence.clone())?;
    let mut consumed =
        gofin_domain::unresolved_consumption::read(&overrun.evidence)?.ok_or_else(|| {
            crate::profile::error("KRAKEN_PROOF_INVALID", "Typed consumed costs are missing.")
        })?;
    consumed.scope =
        gofin_domain::unresolved_consumption::ConsumptionScope::NewMutationFinancialScope;
    consumed.proof_digest = gofin_domain::unresolved_consumption::digest(&consumed)?;
    let mut retained = markers;
    retained.extend(
        record
            .effects
            .as_ref()
            .into_iter()
            .flat_map(|old| old.effects.iter())
            .filter(|effect| !is_marker(effect))
            .cloned(),
    );
    let mut result = if retained.is_empty() {
        crate::trading::unresolved(
            Status::Unknown,
            "The new financial partition exceeded its own fee ceiling; costs remain consumed and unresolved without replay.",
            evidence,
        )
    } else {
        crate::trading::proof(submission, Status::Unknown, retained, evidence)?
    };
    gofin_domain::unresolved_consumption::attach(&mut result, &consumed)?;
    Ok(result)
}
pub(crate) fn is_marker(effect: &FinancialEffect) -> bool {
    matches!(
        effect,
        FinancialEffect::GroupedSubmissionAmendments { .. }
            | FinancialEffect::CompoundSubmissionMutations { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{AccountingLocation, Amount, AssetId, CostTreatment, ResourceId};
    fn amount(v: &str) -> Amount {
        Amount {
            asset: AssetId {
                network: "krakenfutures".into(),
                reference: "USD".into(),
                decimals: 36,
            },
            atoms: Quantity::from_decimal(v, 36).unwrap(),
        }
    }
    fn location() -> AccountingLocation {
        AccountingLocation::Cash {
            resource: ResourceId("SYNTHETIC:wallet:flex".into()),
        }
    }
    fn fee(v: &str) -> FinancialEffect {
        FinancialEffect::Cost {
            location: location(),
            amount: amount(v),
            category: "SYNTHETIC-native-fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        }
    }
    fn cap() -> CostBound {
        CostBound {
            location: location(),
            amount: amount("0.4"),
            treatment: CostTreatment::AdditionalDebit,
        }
    }
    #[test]
    fn aggregate_gross_native_fees_exceed_one_whole_scope_cap_without_rebate_netting() {
        let fees = vec![
            fee("0.3"),
            fee("0.2"),
            FinancialEffect::UsableCredit {
                to: ResourceId("SYNTHETIC:wallet:flex".into()),
                amount: amount("0.15"),
            },
        ];
        assert!(costs_exceed_bounds(&fees, &[cap()]).unwrap());
        assert!(!costs_exceed_bounds(&[fee("0.3")], &[cap()]).unwrap());
    }
    #[test]
    fn unknown_cost_identity_or_treatment_never_becomes_guessed_consumption() {
        for kind in 0..3 {
            let mut bound = cap();
            match kind {
                0 => {
                    bound.location = AccountingLocation::Cash {
                        resource: ResourceId("SYNTHETIC:other".into()),
                    }
                }
                1 => bound.amount.asset.reference = "USDT".into(),
                _ => bound.treatment = CostTreatment::IncludedInDebit,
            };
            assert_eq!(
                costs_exceed_bounds(&[fee("0.5")], &[bound])
                    .unwrap_err()
                    .code,
                "KRAKEN_COST_IDENTITY_UNAPPROVED"
            );
        }
    }
}
