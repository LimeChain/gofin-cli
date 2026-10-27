use super::*;
use gofin_domain::{AssetId, Quantity, SignedQuantity};

fn amount(atoms: u64) -> Amount {
    Amount {
        asset: AssetId {
            network: "synthetic".into(),
            reference: "native".into(),
            decimals: 9,
        },
        atoms: Quantity::from_u64(atoms),
    }
}

fn record(cap: u64) -> OperationRecord {
    let wallet = ResourceId("synthetic:wallet".into());
    let mut record = crate::tests::record(
        vec![EffectConstraint::AccountFundingBudget {
            resource: wallet.clone(),
            asset: amount(0).asset,
            maximum_debit: Quantity::from_u64(cap),
        }],
        vec![],
    );
    let prepared = record.preview.as_mut().unwrap();
    prepared.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: amount(cap),
    }];
    prepared.intent.locks = vec![wallet];
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(prepared)).unwrap();
    record
}

fn movement(account: &str, atoms: u64, refund: bool) -> Vec<FinancialEffect> {
    let wallet = ResourceId("synthetic:wallet".into());
    let position = format!("account-funding:{account}");
    let cash = AccountingLocation::Cash {
        resource: wallet.clone(),
    };
    let locked = AccountingLocation::Position {
        resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::RefundableAccountFunding,
    };
    vec![
        if refund {
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount: amount(atoms),
            }
        } else {
            FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount: amount(atoms),
            }
        },
        FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position: position.clone(),
            asset: amount(0).asset,
            delta: SignedQuantity::new(if refund {
                format!("-{atoms}")
            } else {
                atoms.to_string()
            })
            .unwrap(),
            scaled_receipt: None,
            anchor: Some(PositionAnchor {
                resource: wallet,
                position,
                position_kind: PositionKind::RefundableAccountFunding,
                principal: amount(if refund { 0 } else { atoms }),
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({"fixture":"authenticated account owner and refund authority"}),
                observed_at: "2026-01-01T00:00:30Z".into(),
            }),
        },
        FinancialEffect::OriginMovement {
            from: if refund { locked.clone() } else { cash.clone() },
            to: if refund { cash } else { locked },
            debit: amount(atoms),
            credit: amount(atoms),
        },
    ]
}

fn proof(record: &OperationRecord, effects: Vec<FinancialEffect>) -> VerifiedEffects {
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: record.submission.clone().unwrap(),
        effects,
        evidence: json!({"fixture":"finalized funding receipt"}),
        observed_at: "2026-01-01T00:00:30Z".into(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    verified
}

#[test]
fn account_funding_budget_accepts_balanced_new_accounts_without_mutating_authority() {
    let record = record(8);
    let original = record.preview.as_ref().unwrap().intent.clone();
    let mut effects = movement("new-a", 3, false);
    effects.extend(movement("new-b", 5, false));
    let verified = proof(&record, effects);
    validate_effects(&record, &verified, &Status::Completed).unwrap();
    validate_anchor_transitions(&verified, &PreparationContext::default()).unwrap();
    assert_eq!(record.preview.as_ref().unwrap().intent, original);
    validate_effects(&record, &proof(&record, vec![]), &Status::Completed).unwrap();
}

#[test]
fn account_funding_budget_accepts_closure_without_funding_allowance_but_requires_prior_anchor() {
    let record = record(0);
    let verified = proof(&record, movement("old", 12, true));
    validate_effects(&record, &verified, &Status::Completed).unwrap();
    assert_eq!(
        validate_anchor_transitions(&verified, &PreparationContext::default())
            .unwrap_err()
            .code,
        "PRIOR_POSITION_ANCHOR_REQUIRED"
    );
    let FinancialEffect::PositionChange {
        anchor: Some(post), ..
    } = &verified.effects[1]
    else {
        panic!("fixture position missing");
    };
    let mut prior = post.clone();
    prior.principal = amount(12);
    let mut context = PreparationContext::default();
    context.positions.push(prior);
    validate_anchor_transitions(&verified, &context).unwrap();
    context.positions[0].principal = amount(11);
    assert!(validate_anchor_transitions(&verified, &context).is_err());
}

#[test]
fn account_funding_budget_rejects_aggregate_overflow_even_with_larger_refunds() {
    let record = record(8);
    let mut effects = movement("new-a", 5, false);
    effects.extend(movement("new-b", 4, false));
    effects.extend(movement("old", 100, true));
    assert!(validate_effects(&record, &proof(&record, effects), &Status::Completed).is_err());
}

#[test]
fn account_funding_budget_rejects_mismatched_missing_and_duplicate_pairs() {
    let record = record(8);
    let complete = movement("new", 8, false);
    for remove in 0..complete.len() {
        let mut effects = complete.clone();
        effects.remove(remove);
        assert!(validate_effects(&record, &proof(&record, effects), &Status::Completed).is_err());
    }
    for index in 0..complete.len() {
        let mut effects = complete.clone();
        effects.push(complete[index].clone());
        assert!(validate_effects(&record, &proof(&record, effects), &Status::Completed).is_err());
    }
    for mutate in 0..4 {
        let mut effects = complete.clone();
        if let FinancialEffect::OriginMovement {
            from,
            debit,
            credit,
            ..
        } = &mut effects[2]
        {
            match mutate {
                0 => credit.atoms = Quantity::from_u64(7),
                1 => debit.asset.reference = "other".into(),
                2 => {
                    *from = AccountingLocation::Cash {
                        resource: ResourceId("other".into()),
                    }
                }
                _ => credit.asset.network = "other".into(),
            }
        }
        assert!(validate_effects(&record, &proof(&record, effects), &Status::Completed).is_err());
    }
}

#[test]
fn account_funding_budget_rejects_unrelated_effects_and_excess_refunds() {
    let record = record(8);
    let complete = movement("new", 8, false);
    let mut extra_credit = complete.clone();
    extra_credit.push(FinancialEffect::UsableCredit {
        to: ResourceId("synthetic:wallet".into()),
        amount: amount(1),
    });
    assert!(validate_effects(&record, &proof(&record, extra_credit), &Status::Completed).is_err());
    let mut wrong_kind = complete.clone();
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut wrong_kind[1]
    {
        anchor.position_kind = PositionKind::Supply;
    }
    assert!(validate_effects(&record, &proof(&record, wrong_kind), &Status::Completed).is_err());
    let mut extra_position = complete;
    extra_position.push(movement("unrelated", 1, false)[1].clone());
    assert!(
        validate_effects(&record, &proof(&record, extra_position), &Status::Completed).is_err()
    );
    let mut excessive_refund = movement("old", 12, true);
    if let FinancialEffect::UsableCredit { amount, .. } = &mut excessive_refund[0] {
        amount.atoms = Quantity::from_u64(13);
    }
    assert!(
        validate_effects(
            &record,
            &proof(&record, excessive_refund),
            &Status::Completed
        )
        .is_err()
    );
}

#[test]
fn account_funding_budget_preserves_failure_scope_and_bounded_reauthorization() {
    let record = record(8);
    validate_effects(&record, &proof(&record, vec![]), &Status::Failed).unwrap();
    assert!(
        validate_effects(
            &record,
            &proof(&record, movement("new", 8, false)),
            &Status::Failed,
        )
        .is_err()
    );
    let approved = &record.preview.as_ref().unwrap().intent.expected[0];
    let mut tighter = approved.clone();
    if let EffectConstraint::AccountFundingBudget { maximum_debit, .. } = &mut tighter {
        *maximum_debit = Quantity::from_u64(7);
    }
    assert!(constraint_within(&tighter, approved));
    assert!(!constraint_within(approved, &tighter));
    if let EffectConstraint::AccountFundingBudget { asset, .. } = &mut tighter {
        asset.reference = "other".into();
    }
    assert!(!constraint_within(&tighter, approved));
}

#[test]
fn account_funding_budget_requires_a_separate_reserve_from_native_conversion_principal() {
    let mut intent = record(8).preview.unwrap().intent;
    validate(&intent).unwrap();
    intent.debits[0].amount = amount(7);
    assert_eq!(
        validate(&intent).unwrap_err().code,
        "ACCOUNT_FUNDING_BUDGET_UNBACKED"
    );
    intent.expected.push(EffectConstraint::Conversion {
        resource: ResourceId("synthetic:wallet".into()),
        input: amount(0).asset,
        maximum_input: Quantity::from_u64(10),
        output: AssetId {
            network: "synthetic".into(),
            reference: "output".into(),
            decimals: 6,
        },
        minimum_output: Quantity::from_u64(1),
    });
    intent.debits[0].amount = amount(17);
    assert_eq!(
        validate(&intent).unwrap_err().code,
        "ACCOUNT_FUNDING_BUDGET_UNBACKED"
    );
    intent.debits[0].amount = amount(18);
    validate(&intent).unwrap();
    intent.expected.push(intent.expected[0].clone());
    assert!(validate(&intent).is_err());
}
