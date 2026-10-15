use super::*;
use gofin_domain::PositionEventOrder;

fn position_anchor(atoms: &str, observed_at: &str) -> PositionAnchor {
    PositionAnchor {
        resource: resource("solana:mainnet-beta:wallet:owner"),
        position: "perps:position:shared".into(),
        position_kind: PositionKind::DerivativeLong,
        principal: Amount {
            asset: asset("solana:mainnet-beta", "jupiter:position-usd", 6),
            atoms: quantity(atoms),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"verified":"synthetic position account"}),
        observed_at: observed_at.into(),
    }
}

fn order(submission: Submission, slot: u64, index: u32, event_id: &str) -> PositionEventOrder {
    PositionEventOrder {
        submission,
        domain: "solana:mainnet-beta".into(),
        slot,
        transaction_index: index,
        event_id: event_id.into(),
    }
}

fn protection(
    anchor: PositionAnchor,
    direction: gofin_domain::PendingRequestTriggerDirection,
) -> gofin_domain::PendingRequestProtection {
    gofin_domain::PendingRequestProtection {
        position_anchor: anchor,
        position_lock_resource: resource("solana:mainnet-beta:jupiter-perps-position:shared"),
        maximum_reduction: quantity("10"),
        trigger_direction: direction,
        trigger_price: Amount {
            asset: asset("solana:mainnet-beta", "jupiter:margin-usd", 6),
            atoms: quantity("100"),
        },
        reduce_only: true,
    }
}

fn lifecycle(
    request_id: &str,
    protection: Option<gofin_domain::PendingRequestProtection>,
) -> EffectConstraint {
    EffectConstraint::PendingRequestLifecycle {
        wallet_resource: resource("solana:mainnet-beta:wallet:owner"),
        request_resource: resource(&format!(
            "solana:mainnet-beta:jupiter-perps-request:{request_id}"
        )),
        request_id: request_id.into(),
        request_kind: "tpsl".into(),
        conditional_positions: vec![gofin_domain::PendingRequestPositionBound {
            resource: resource("solana:mainnet-beta:wallet:owner"),
            position: "perps:position:shared".into(),
            position_kind: PositionKind::DerivativeLong,
            asset: asset("solana:mainnet-beta", "jupiter:position-usd", 6),
            minimum_delta: SignedQuantity::new("-10").unwrap(),
            maximum_delta: SignedQuantity::new("0").unwrap(),
        }],
        conditional_credits: vec![],
        conditional_pnl: vec![],
        protection,
    }
}

fn verified_request(
    submission: Submission,
    request_id: &str,
    delta: &str,
    anchor: PositionAnchor,
    protection: gofin_domain::PendingRequestProtection,
    order: PositionEventOrder,
) -> VerifiedEffects {
    let wallet_resource = resource("solana:mainnet-beta:wallet:owner");
    let position = "perps:position:shared".to_owned();
    let asset = asset("solana:mainnet-beta", "jupiter:position-usd", 6);
    let progress = gofin_domain::SettlementProgress {
        version: 1,
        sequence: 1,
        snapshot_id: format!("event:{request_id}"),
        event_digests: std::collections::BTreeMap::from([(
            order.event_id.clone(),
            format!("digest-{}", order.event_id),
        )]),
        position_orders: vec![order],
    };
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![
            FinancialEffect::PendingRequestProgress {
                wallet_resource: wallet_resource.clone(),
                request_resource: resource(&format!(
                    "solana:mainnet-beta:jupiter-perps-request:{request_id}"
                )),
                request_id: request_id.into(),
                request_kind: "tpsl".into(),
                state: gofin_domain::PendingRequestState::Filled,
                positions: vec![gofin_domain::PendingRequestPositionProgress {
                    resource: wallet_resource.clone(),
                    position: position.clone(),
                    position_kind: PositionKind::DerivativeLong,
                    asset: asset.clone(),
                    delta: SignedQuantity::new(delta).unwrap(),
                    anchor: anchor.clone(),
                }],
                credits: vec![],
                realized_pnl: vec![],
                protection: Some(protection),
            },
            FinancialEffect::PositionChange {
                resource: wallet_resource,
                position,
                asset,
                delta: SignedQuantity::new(delta).unwrap(),
                scaled_receipt: None,
                anchor: Some(anchor),
            },
        ],
        evidence: json!({
            "settlement_progress": serde_json::to_value(progress).unwrap(),
            "synthetic_finalized": true
        }),
        observed_at: "2026-01-01T00:00:30Z".into(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    verified
}

fn source_record(verified: VerifiedEffects) -> OperationRecord {
    let protection = verified.effects.iter().find_map(|effect| match effect {
        FinancialEffect::PendingRequestProgress { protection, .. } => protection.clone(),
        _ => None,
    });
    let mut record = record(vec![lifecycle("request-source", protection)], vec![]);
    record.id = "source-op".into();
    record.status = Status::Completed;
    record.submission = Some(verified.submission.clone());
    record.effects = Some(verified);
    record
}

fn current_record(verified: &VerifiedEffects) -> OperationRecord {
    let protection = verified.effects.iter().find_map(|effect| match effect {
        FinancialEffect::PendingRequestProgress { protection, .. } => protection.clone(),
        _ => None,
    });
    let mut record = record(vec![lifecycle("request-current", protection)], vec![]);
    record.id = "current-op".into();
    record.status = Status::Submitted;
    record.submission = Some(verified.submission.clone());
    record
}

#[test]
fn verified_sibling_anchor_is_used_only_for_exact_later_position_transition() {
    let initial = position_anchor("10", "2026-01-01T00:00:01Z");
    let source_anchor = position_anchor("7", "2026-01-01T00:00:20Z");
    let current_anchor = position_anchor("0", "2026-01-01T00:00:30Z");
    let source_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "source-signature".into(),
        data: json!({"message_hash":"source-message"}),
    };
    let current_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "current-signature".into(),
        data: json!({"message_hash":"current-message"}),
    };
    let source_order = order(source_submission.clone(), 100, 0, "source-event");
    let current_order = order(current_submission.clone(), 101, 0, "current-event");
    let source_protection = protection(
        initial.clone(),
        gofin_domain::PendingRequestTriggerDirection::Above,
    );
    let current_protection = protection(
        initial.clone(),
        gofin_domain::PendingRequestTriggerDirection::Below,
    );
    let source_verified = verified_request(
        source_submission.clone(),
        "request-source",
        "-3",
        source_anchor.clone(),
        source_protection,
        source_order.clone(),
    );
    let source = source_record(source_verified.clone());
    let mut current_verified = verified_request(
        current_submission.clone(),
        "request-current",
        "-7",
        current_anchor.clone(),
        current_protection,
        current_order.clone(),
    );
    current_verified
        .effects
        .push(FinancialEffect::PositionAnchorPredecessor {
            predecessor: Box::new(PositionAnchorPredecessor {
                source_operation_id: source.id.clone(),
                source_submission: source_submission.clone(),
                source_proof_digest: source_verified.proof_digest.clone(),
                source_anchor: source_anchor.clone(),
                source_order,
                current_order,
                resource: source_anchor.resource.clone(),
                position: source_anchor.position.clone(),
                position_kind: source_anchor.position_kind.clone(),
                asset: source_anchor.principal.asset.clone(),
            }),
        });
    current_verified.proof_digest = verified_effect_digest(&current_verified).unwrap();
    let mut current = current_record(&current_verified);
    current.preview.as_mut().unwrap().binding["__gofin_protection_parent"] = json!({
        "version": 1,
        "parent_operation_id": source.id,
        "parent_submission": source_submission,
        "parent_request_id": "request-source",
        "shared_locks": ["solana:mainnet-beta:jupiter-perps-position:shared"]
    });
    let prior = PreparationContext {
        positions: vec![initial.clone()],
    };

    let advanced = position_predecessor_context(
        &current,
        &current_verified,
        &[source.clone(), current.clone()],
        prior.clone(),
    )
    .unwrap();
    assert_eq!(advanced.positions, vec![source_anchor.clone()]);
    assert_eq!(prior.positions, vec![initial]);
    validate_anchor_transitions(&current_verified, &advanced).unwrap();

    let mut reverse_source = source.clone();
    reverse_source.preview.as_mut().unwrap().binding["__gofin_protection_parent"] = json!({
        "version": 1,
        "parent_operation_id": current.id,
        "parent_submission": current_submission,
        "parent_request_id": "request-current",
        "shared_locks": ["solana:mainnet-beta:jupiter-perps-position:shared"]
    });
    let mut reverse_current = current.clone();
    reverse_current
        .preview
        .as_mut()
        .unwrap()
        .binding
        .as_object_mut()
        .unwrap()
        .remove("__gofin_protection_parent");
    let reverse = position_predecessor_context(
        &reverse_current,
        &current_verified,
        &[reverse_source.clone(), reverse_current.clone()],
        prior.clone(),
    )
    .unwrap();
    assert_eq!(reverse.positions, vec![source_anchor.clone()]);

    let mut unrelated_current = current.clone();
    unrelated_current.preview.as_mut().unwrap().binding["__gofin_protection_parent"] = json!({
        "version": 1,
        "parent_operation_id": "unrelated-operation",
        "parent_submission": {"kind":"solana_transaction","reference":"unrelated","data":{}},
        "parent_request_id": "unrelated-request",
        "shared_locks": ["solana:mainnet-beta:jupiter-perps-position:shared"]
    });
    assert!(
        position_predecessor_context(
            &unrelated_current,
            &current_verified,
            &[source.clone(), unrelated_current.clone()],
            prior.clone(),
        )
        .is_err()
    );

    let mut reordered = current_verified.clone();
    let FinancialEffect::PositionAnchorPredecessor { predecessor } =
        reordered.effects.last_mut().unwrap()
    else {
        unreachable!()
    };
    predecessor.current_order.slot = predecessor.source_order.slot;
    predecessor.current_order.transaction_index = predecessor.source_order.transaction_index;
    reordered.proof_digest = verified_effect_digest(&reordered).unwrap();
    assert!(
        position_predecessor_context(
            &current,
            &reordered,
            &[source.clone(), current.clone()],
            prior.clone(),
        )
        .is_err()
    );

    let mut stale_source = current_verified.clone();
    let FinancialEffect::PositionAnchorPredecessor { predecessor } =
        stale_source.effects.last_mut().unwrap()
    else {
        unreachable!()
    };
    predecessor.source_proof_digest = "00".repeat(32);
    stale_source.proof_digest = verified_effect_digest(&stale_source).unwrap();
    assert!(
        position_predecessor_context(&current, &stale_source, &[source, current.clone()], prior,)
            .is_err()
    );
}
