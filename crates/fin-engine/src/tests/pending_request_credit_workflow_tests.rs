use super::*;

#[test]
fn pending_request_credit_survives_restart_and_grants_only_the_new_delta_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending-request-credit.sqlite");
    let wallet = resource("solana:mainnet-beta:wallet:synthetic");
    let request = resource("solana:mainnet-beta:request:synthetic");
    let position_asset = asset("solana:mainnet-beta", "jupiter:position-usd:test", 6);
    let payout_asset = asset("solana:mainnet-beta", "synthetic-usdc", 6);
    let pnl_asset = asset("solana:mainnet-beta", "jupiter:margin-usd", 6);
    let constraint = EffectConstraint::PendingRequestLifecycle {
        wallet_resource: wallet.clone(),
        request_resource: request.clone(),
        request_id: "request-pda".into(),
        request_kind: "take_profit".into(),
        conditional_positions: vec![fin_domain::PendingRequestPositionBound {
            resource: wallet.clone(),
            position: "position-pda".into(),
            position_kind: PositionKind::DerivativeLong,
            asset: position_asset.clone(),
            minimum_delta: SignedQuantity::new("0").unwrap(),
            maximum_delta: SignedQuantity::new("1000").unwrap(),
        }],
        conditional_credits: vec![fin_domain::PendingRequestCreditBound {
            asset: payout_asset.clone(),
            minimum_credit: quantity("25"),
            maximum_credit: Some(quantity("100")),
        }],
        conditional_pnl: vec![fin_domain::PendingRequestPnlBound {
            resource: wallet.clone(),
            position: "position-pda".into(),
            position_kind: PositionKind::DerivativeLong,
            asset: pnl_asset.clone(),
            maximum_loss: quantity("10"),
        }],
        protection: None,
    };
    let mut operation = record(vec![constraint], vec![]);
    operation.status = Status::Submitted;
    let submission = operation.submission.clone().unwrap();
    let mut store = Store::open(&path).unwrap();
    store
        .create_request(
            &operation.id,
            &operation.request_id,
            &operation.fingerprint,
            &operation.operation,
            None,
            "submitted",
            &to_value(&operation).unwrap(),
            &operation.created_at,
        )
        .unwrap();
    store
        .persist_submission(
            &operation.id,
            &SubmissionRow {
                kind: submission.kind.clone(),
                reference: submission.reference.clone(),
                data: submission.data.clone(),
            },
            &operation.updated_at,
        )
        .unwrap();
    let engine = Engine::new(
        store,
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();

    let open = progress_proof(
        &operation,
        1,
        "request-open-1",
        &[("request-pda", "a".repeat(64).as_str())],
        vec![FinancialEffect::PendingRequestProgress {
            wallet_resource: wallet.clone(),
            request_resource: request.clone(),
            request_id: "request-pda".into(),
            request_kind: "take_profit".into(),
            state: fin_domain::PendingRequestState::Open,
            positions: vec![],
            credits: vec![],
            realized_pnl: vec![],
            protection: None,
        }],
    );
    let pending = engine
        .apply_reconciliation(
            operation.clone(),
            Reconciliation {
                status: Status::Pending,
                evidence: open.evidence.clone(),
                verified: Some(open.clone()),
            },
        )
        .unwrap();
    assert_eq!(pending.status, Status::Pending);
    drop(engine);

    // Continue from the durable Pending record as though the process restarted
    // before a later keeper fill was observed.
    let engine = Engine::new(
        Store::open(&path).unwrap(),
        vec![],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let persisted: OperationRecord = from_value(
        engine
            .store
            .lock()
            .unwrap()
            .get_record(&pending.id)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted.effects.as_ref(), Some(&open));

    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: "position-pda".into(),
        position_kind: PositionKind::DerivativeLong,
        principal: Amount {
            asset: position_asset.clone(),
            atoms: quantity("50"),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"slot":100,"position":"position-pda"}),
        observed_at: "2026-01-01T00:00:30Z".into(),
    };
    let filled = progress_proof(
        &persisted,
        2,
        "request-filled-2",
        &[("request-pda", "a".repeat(64).as_str())],
        vec![
            FinancialEffect::PendingRequestProgress {
                wallet_resource: wallet.clone(),
                request_resource: request,
                request_id: "request-pda".into(),
                request_kind: "take_profit".into(),
                state: fin_domain::PendingRequestState::Filled,
                positions: vec![fin_domain::PendingRequestPositionProgress {
                    resource: wallet.clone(),
                    position: "position-pda".into(),
                    position_kind: PositionKind::DerivativeLong,
                    asset: position_asset.clone(),
                    delta: SignedQuantity::new("50").unwrap(),
                    anchor: anchor.clone(),
                }],
                credits: vec![fin_domain::PendingRequestCreditProgress {
                    asset: payout_asset.clone(),
                    amount: quantity("25"),
                }],
                realized_pnl: vec![fin_domain::PendingRequestPnlProgress {
                    resource: wallet.clone(),
                    position: "position-pda".into(),
                    position_kind: PositionKind::DerivativeLong,
                    asset: pnl_asset.clone(),
                    event_id: "keeper-fill-signature:event-0".into(),
                    delta: SignedQuantity::new("3").unwrap(),
                    anchor: anchor.clone(),
                }],
                protection: None,
            },
            FinancialEffect::PositionChange {
                resource: wallet.clone(),
                position: "position-pda".into(),
                asset: position_asset,
                delta: SignedQuantity::new("50").unwrap(),
                scaled_receipt: None,
                anchor: Some(anchor.clone()),
            },
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount: Amount {
                    asset: payout_asset,
                    atoms: quantity("25"),
                },
            },
            FinancialEffect::PositionRealizedPnl {
                resource: wallet.clone(),
                position: "position-pda".into(),
                position_kind: PositionKind::DerivativeLong,
                asset: pnl_asset,
                event_id: "keeper-fill-signature:event-0".into(),
                delta: SignedQuantity::new("3").unwrap(),
                anchor: Box::new(anchor),
            },
        ],
    );
    let delta = cumulative_boundary_delta(Some(&open), &filled).unwrap();
    assert_eq!(
        delta
            .effects
            .iter()
            .filter(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
            .count(),
        1
    );
    assert!(delta.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::UsableCredit { to, amount }
            if to == &wallet && amount.atoms == quantity("25")
    )));

    let completed = engine
        .apply_reconciliation(
            persisted.clone(),
            Reconciliation {
                status: Status::Completed,
                evidence: filled.evidence.clone(),
                verified: Some(filled.clone()),
            },
        )
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.effects.as_ref(), Some(&filled));
    let replay_delta = cumulative_boundary_delta(Some(&filled), &filled).unwrap();
    assert!(
        !replay_delta
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
    );
    let replay = engine
        .apply_reconciliation(
            completed.clone(),
            Reconciliation {
                status: Status::Completed,
                evidence: filled.evidence.clone(),
                verified: Some(filled),
            },
        )
        .unwrap();
    assert_eq!(replay.effects, completed.effects);
}
