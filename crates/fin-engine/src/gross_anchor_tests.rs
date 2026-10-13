use super::*;
#[test]
fn gross_position_changes_share_only_one_identical_native_post_state() {
    let asset = fin_domain::AssetId {
        network: "synthetic".into(),
        reference: "CONTRACT".into(),
        decimals: 0,
    };
    let resource = ResourceId("synthetic:margin".into());
    let observed = now();
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: "exact-exposure".into(),
        position_kind: PositionKind::MarginLong,
        principal: Amount {
            asset: asset.clone(),
            atoms: fin_domain::Quantity::from_u64(5),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"native_position":"five","trade_ids":["open-seven","close-two"]}),
        observed_at: observed.clone(),
    };
    let effect = |delta, anchor| FinancialEffect::PositionChange {
        resource: resource.clone(),
        position: "exact-exposure".into(),
        asset: asset.clone(),
        delta: fin_domain::SignedQuantity::new(delta).unwrap(),
        scaled_receipt: None,
        anchor: Some(anchor),
    };
    let mut zero = anchor.clone();
    zero.principal.atoms = fin_domain::Quantity::zero();
    let prior = PreparationContext {
        positions: vec![zero],
    };
    let mut proof = VerifiedEffects {
        submission: Submission {
            kind: "synthetic".into(),
            reference: "once".into(),
            data: json!({}),
        },
        effects: vec![effect("7", anchor.clone()), effect("-2", anchor.clone())],
        evidence_version: 1,
        evidence: json!({"native_receipt":"exact"}),
        proof_digest: "synthetic".into(),
        observed_at: observed,
    };
    validate_anchor_transitions(&proof, &prior).unwrap();
    let mut conflicting = anchor;
    conflicting.principal.atoms = fin_domain::Quantity::from_u64(6);
    proof.effects[1] = effect("-2", conflicting);
    assert!(validate_anchor_transitions(&proof, &prior).is_err());
}
