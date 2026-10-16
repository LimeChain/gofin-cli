use super::*;
use async_trait::async_trait;
use gofin_domain::{
    Adapter, Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Definition, ExecutionActor,
    FinancialEffect, FinancialState, GoFinError, LiquidityAssetDelta, LiquidityCompositionAmount,
    LiquidityPositionEvent, LiquidityPositionEvidence, OperationKind, OperationRecord,
    PositionAnchor, PositionState, PreparedOperation, Quantity, Reconciliation, Result,
    SignedQuantity, StateSnapshot, Status, Submission, SubmissionJournal,
};
use std::sync::Arc;

fn asset(mint: &str, decimals: u8) -> AssetId {
    AssetId {
        network: "solana:mainnet-beta".into(),
        reference: mint.into(),
        decimals,
    }
}

fn event(action: &str, delta: &str, slot: u64) -> LiquidityPositionEvent {
    LiquidityPositionEvent {
        signature: format!("signature-{slot}"),
        submission: None,
        slot,
        action: action.into(),
        liquidity_delta_atoms: SignedQuantity::new(delta).unwrap(),
        token_deltas: vec![LiquidityAssetDelta {
            asset: asset("mint-a", 6),
            token_program: "token-program".into(),
            delta: SignedQuantity::new("10").unwrap(),
        }],
        fees: vec![],
        rewards: vec![],
    }
}

fn evidence(
    a_atoms: Option<&str>,
    b_atoms: Option<&str>,
    history: Vec<LiquidityPositionEvent>,
    historical_principal_known: bool,
) -> LiquidityPositionEvidence {
    LiquidityPositionEvidence {
        version: 1,
        family: "dlmm".into(),
        pool: "pool".into(),
        position: "position".into(),
        anchor_position_id: Some("meteora:liquidity:dlmm:position".into()),
        observed_slot: Some(100),
        pre_transaction_units_atoms: None,
        liquidity_units_atoms: Some(Quantity::new("7").unwrap()),
        liquidity_units_known: true,
        historical_principal_known,
        token_a: LiquidityCompositionAmount {
            asset: asset("mint-a", 6),
            token_program: "token-program".into(),
            atoms: a_atoms.map(Quantity::new).transpose().unwrap(),
        },
        token_b: LiquidityCompositionAmount {
            asset: asset("mint-b", 9),
            token_program: "token-program".into(),
            atoms: b_atoms.map(Quantity::new).transpose().unwrap(),
        },
        backing_valuation: None,
        history,
    }
}

fn state_anchor(evidence: LiquidityPositionEvidence) -> PositionState {
    let resource = ResourceId("solana:mainnet-beta:wallet".into());
    let position = "meteora:liquidity:dlmm:position".to_owned();
    let principal = Amount {
        asset: asset(&position, 0),
        atoms: Quantity::new("7").unwrap(),
    };
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: PositionKind::Liquidity,
        principal: principal.clone(),
        scaled_receipt: None,
        evidence_version: 3,
        evidence: json!({
            "version":3,
            "historical_principal_known":evidence.historical_principal_known,
            "liquidity_position":evidence,
        }),
        observed_at: "2026-10-14T00:00:00Z".into(),
    };
    PositionState {
        resource,
        position,
        kind: PositionKind::Liquidity,
        amount: principal,
        scaled_receipt: None,
        anchor: Some(anchor),
    }
}

struct SnapshotReadAdapter(StateSnapshot);

#[async_trait]
impl Adapter for SnapshotReadAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Definition {
            name: "gofin.snapshot".into(),
            description: "Synthetic composite snapshot.".into(),
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic".into(),
            completion: "synthetic".into(),
            semantics: json!({}),
        }]
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
        serde_json::to_value(&self.0)
            .map_err(|_| GoFinError::validation("Synthetic snapshot serialization failed."))
    }

    async fn prepare(&self, _operation: &str, _input: &Value) -> Result<PreparedOperation> {
        Err(GoFinError::validation(
            "This synthetic adapter is read only.",
        ))
    }

    async fn revalidate(&self, _prepared: &PreparedOperation) -> Result<PreparedOperation> {
        Err(GoFinError::validation(
            "This synthetic adapter is read only.",
        ))
    }

    async fn submit(
        &self,
        _prepared: &PreparedOperation,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(GoFinError::validation(
            "This synthetic adapter is read only.",
        ))
    }

    async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
        Err(GoFinError::validation(
            "This synthetic adapter is read only.",
        ))
    }
}

struct ReadOnlyAuthorizer;

#[async_trait]
impl gofin_domain::Authorizer for ReadOnlyAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            mechanism: "synthetic".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: "2026-10-14T00:00:00Z".into(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn public_fin_snapshot_read_preserves_broker_verified_liquidity_history() {
    let prior_evidence = evidence(
        Some("100"),
        Some("200"),
        vec![event("open", "100", 10), event("add", "25", 20)],
        true,
    );
    let prior = state_anchor(prior_evidence);
    let anchor = prior.anchor.clone().unwrap();
    let asset = anchor.principal.asset.clone();
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: Submission {
            kind: "synthetic".into(),
            reference: "synthetic-history".into(),
            data: json!({"id":"prior-position-proof"}),
        },
        effects: vec![FinancialEffect::PositionChange {
            resource: anchor.resource.clone(),
            position: anchor.position.clone(),
            asset,
            delta: SignedQuantity::new("7").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor),
        }],
        evidence: json!({"fixture":"verified prior liquidity history"}),
        observed_at: "2026-10-14T00:00:00Z".into(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
    let prior_record = OperationRecord {
        id: "prior-operation".into(),
        request_id: "prior-request".into(),
        fingerprint: "prior-fingerprint".into(),
        fingerprint_version: 1,
        operation: "synthetic.position".into(),
        input: json!({}),
        status: Status::Completed,
        created_at: "2026-10-14T00:00:00Z".into(),
        updated_at: "2026-10-14T00:00:00Z".into(),
        preview: None,
        approval: None,
        submission: Some(verified.submission.clone()),
        dispatch_evidence: None,
        position_baseline: None,
        effects: Some(verified),
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let mut store = gofin_store::Store::in_memory().unwrap();
    store
        .create_request(
            &prior_record.id,
            &prior_record.request_id,
            &prior_record.fingerprint,
            &prior_record.operation,
            None,
            "completed",
            &serde_json::to_value(&prior_record).unwrap(),
            &prior_record.created_at,
        )
        .unwrap();

    let fresh = state_anchor(evidence(Some("70"), Some("240"), vec![], false));
    let snapshot = StateSnapshot {
        state: FinancialState {
            positions: vec![fresh],
            ..FinancialState::default()
        },
        observed_at: "2026-10-14T00:01:00Z".into(),
        complete: true,
        coverage: vec!["solana:mainnet-beta:wallet".into()],
        pending: vec![],
    };
    let engine = Engine::new(
        store,
        vec![Arc::new(SnapshotReadAdapter(snapshot))],
        Arc::new(ReadOnlyAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let public_snapshot = engine.read("gofin.snapshot", &json!({})).await.unwrap();
    let current: LiquidityPositionEvidence = serde_json::from_value(
        public_snapshot["state"]["positions"][0]["anchor"]["evidence"]["liquidity_position"]
            .clone(),
    )
    .unwrap();
    assert_eq!(current.token_a.atoms.unwrap().as_str(), "70");
    assert_eq!(current.token_b.atoms.unwrap().as_str(), "240");
    assert_eq!(current.history.len(), 2);
    assert!(current.historical_principal_known);

    let legacy_snapshot = engine.read("fin.snapshot", &json!({})).await.unwrap();
    assert_eq!(legacy_snapshot, public_snapshot);
}

#[test]
fn fresh_composition_observation_preserves_open_add_collect_history() {
    let prior = state_anchor(evidence(
        Some("100"),
        Some("200"),
        vec![
            event("open", "100", 10),
            event("add", "25", 20),
            event("collect", "0", 30),
        ],
        true,
    ));
    let observed = state_anchor(evidence(Some("70"), Some("240"), vec![], false));
    let mut snapshot = StateSnapshot {
        state: FinancialState {
            positions: vec![observed],
            ..FinancialState::default()
        },
        observed_at: "2026-10-14T00:01:00Z".into(),
        complete: true,
        coverage: vec!["solana:mainnet-beta:wallet".into()],
        pending: vec![],
    };
    let prior_anchor = prior.anchor.unwrap();

    preserve_liquidity_history(&mut snapshot, &[prior_anchor]).unwrap();

    let current: LiquidityPositionEvidence = serde_json::from_value(
        snapshot.state.positions[0]
            .anchor
            .as_ref()
            .unwrap()
            .evidence["liquidity_position"]
            .clone(),
    )
    .unwrap();
    assert_eq!(current.token_a.atoms.unwrap().as_str(), "70");
    assert_eq!(current.token_b.atoms.unwrap().as_str(), "240");
    assert_eq!(
        current
            .history
            .iter()
            .map(|event| event.action.as_str())
            .collect::<Vec<_>>(),
        ["open", "add", "collect"]
    );
    assert!(current.historical_principal_known);
}

#[test]
fn unaccounted_liquidity_unit_change_preserves_events_but_marks_basis_unknown() {
    let prior = state_anchor(evidence(
        Some("100"),
        Some("200"),
        vec![event("open", "100", 10), event("add", "25", 20)],
        true,
    ));
    let mut observed_evidence = evidence(Some("75"), Some("240"), vec![], false);
    observed_evidence.liquidity_units_atoms = Some(Quantity::new("8").unwrap());
    let observed = state_anchor(observed_evidence);
    let mut snapshot = StateSnapshot {
        state: FinancialState {
            positions: vec![observed],
            ..FinancialState::default()
        },
        observed_at: "2026-10-14T00:01:00Z".into(),
        complete: true,
        coverage: vec!["solana:mainnet-beta:wallet".into()],
        pending: vec![],
    };
    preserve_liquidity_history(&mut snapshot, &[prior.anchor.unwrap()]).unwrap();
    let current: LiquidityPositionEvidence = serde_json::from_value(
        snapshot.state.positions[0]
            .anchor
            .as_ref()
            .unwrap()
            .evidence["liquidity_position"]
            .clone(),
    )
    .unwrap();
    assert!(!current.historical_principal_known);
    assert_eq!(current.history.len(), 2);
    assert_eq!(current.liquidity_units_atoms.unwrap().as_str(), "8");
}

#[test]
fn liquidity_transition_rebases_only_from_the_immutable_typed_prestated() {
    let mut before = state_anchor(evidence(Some("100"), Some("200"), vec![], false));
    let prior_anchor = before.anchor.as_mut().unwrap();
    prior_anchor.principal.atoms = Quantity::new("7").unwrap();

    let mut post_evidence = evidence(
        Some("120"),
        Some("210"),
        vec![event("add", "5", 101)],
        false,
    );
    post_evidence.pre_transaction_units_atoms = Some(Quantity::new("12").unwrap());
    post_evidence.liquidity_units_atoms = Some(Quantity::new("17").unwrap());
    let submission = Submission {
        kind: "synthetic".into(),
        reference: "canonical-submission-reference".into(),
        data: json!({"message":"immutable"}),
    };
    post_evidence.history[0].submission = Some(submission.clone());
    let mut post = state_anchor(post_evidence);
    let post_anchor = post.anchor.as_mut().unwrap();
    post_anchor.principal.atoms = Quantity::new("17").unwrap();
    let resource = post_anchor.resource.clone();
    let position = post_anchor.position.clone();
    let asset = post_anchor.principal.asset.clone();
    let verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: position.clone(),
            asset: asset.clone(),
            delta: SignedQuantity::new("5").unwrap(),
            scaled_receipt: None,
            anchor: Some(post_anchor.clone()),
        }],
        evidence: json!({}),
        observed_at: "2026-10-14T00:02:00Z".into(),
        proof_digest: String::new(),
    };
    let binding = gofin_domain::LiquidityTransitionBinding {
        version: 1,
        resource,
        position,
        asset,
        pre_transaction_units_atoms: Quantity::new("12").unwrap(),
    };
    let mut binding_value = json!({});
    binding_value[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY] =
        serde_json::to_value(binding).unwrap();
    let prepared = PreparedOperation {
        operation: "meteora.add".into(),
        input: json!({}),
        binding: binding_value,
        summary: String::new(),
        checks: vec![],
        warnings: vec![],
        observed_at: "2026-10-14T00:00:00Z".into(),
        expires_at: "2026-10-14T00:05:00Z".into(),
        valuation: None,
        intent: gofin_domain::FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![],
            locks: vec![],
        },
        operation_version: 1,
        action_deadline: None,
    };
    let prior = PreparationContext {
        logical_operation_id: None,
        positions: vec![before.anchor.unwrap()],
    };
    validate_anchor_transitions_with_candidate(&verified, &prior, Some(&prepared)).unwrap();

    let reject = |bad_prepared: PreparedOperation, bad_verified: VerifiedEffects| {
        validate_anchor_transitions_with_candidate(&bad_verified, &prior, Some(&bad_prepared))
            .is_err()
    };
    let mut bad_binding = prepared.clone();
    bad_binding.binding[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY]["pre_transaction_units_atoms"] =
        json!("13");
    assert!(reject(bad_binding, verified.clone()));

    let mut wrong_position = prepared.clone();
    wrong_position.binding[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY]["position"] =
        json!("other-position");
    assert!(reject(wrong_position, verified.clone()));

    let mut wrong_resource = prepared.clone();
    wrong_resource.binding[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY]["resource"] =
        json!("solana:mainnet-beta:other-wallet");
    assert!(reject(wrong_resource, verified.clone()));

    let mut wrong_asset = prepared.clone();
    wrong_asset.binding[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY]["asset"]["reference"] =
        json!("other-asset");
    assert!(reject(wrong_asset, verified.clone()));

    let mut fake_known_basis = verified.clone();
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut fake_known_basis.effects[0]
    {
        anchor.evidence["liquidity_position"]["historical_principal_known"] = json!(true);
    }
    assert!(
        validate_anchor_transitions_with_candidate(&fake_known_basis, &prior, Some(&prepared))
            .is_err()
    );

    let mut wrong_event = verified.clone();
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut wrong_event.effects[0]
    {
        anchor.evidence["liquidity_position"]["history"][0]["signature"] =
            json!("different-chain-signature");
    }
    validate_anchor_transitions_with_candidate(&wrong_event, &prior, Some(&prepared)).unwrap();

    let mut wrong_submission = verified.clone();
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut wrong_submission.effects[0]
    {
        anchor.evidence["liquidity_position"]["history"][0]["submission"]["reference"] =
            json!("different-canonical-submission");
    }
    assert!(
        validate_anchor_transitions_with_candidate(&wrong_submission, &prior, Some(&prepared))
            .is_err()
    );

    let mut non_liquidity = verified;
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut non_liquidity.effects[0]
    {
        anchor.position_kind = PositionKind::Supply;
    }
    assert!(
        validate_anchor_transitions_with_candidate(&non_liquidity, &prior, Some(&prepared))
            .is_err()
    );
}

#[test]
fn old_liquidity_history_without_submission_identity_remains_readable() {
    let legacy = json!({
        "signature": "legacy-chain-signature",
        "slot": 10,
        "action": "open",
        "liquidity_delta_atoms": "5",
        "token_deltas": [],
        "fees": [],
        "rewards": []
    });
    let parsed: LiquidityPositionEvent = serde_json::from_value(legacy.clone()).unwrap();
    assert!(parsed.submission.is_none());
    let serialized = serde_json::to_value(parsed).unwrap();
    assert!(serialized.get("submission").is_none());
}

#[test]
fn observer_projection_carries_backing_value_without_creating_cash_or_pricing_units() {
    let mut current = evidence(Some("1000000"), Some("2000000000"), vec![], false);
    current.backing_valuation = Some(gofin_domain::LiquidityBackingValuation {
        token_a_usd_micros: Quantity::new("1000000").unwrap(),
        token_b_usd_micros: Quantity::new("200000000").unwrap(),
        total_usd_micros: Quantity::new("201000000").unwrap(),
        observed_at: "2026-10-14T00:00:00Z".into(),
        source: "test".into(),
    });
    let position = state_anchor(current);
    let wallet = ResourceId("solana:mainnet-beta:wallet".into());
    let snapshot = StateSnapshot {
        state: FinancialState {
            holdings: vec![
                ResourceAmount {
                    resource: wallet.clone(),
                    amount: Amount {
                        asset: asset("mint-a", 6),
                        atoms: Quantity::new("500000").unwrap(),
                    },
                },
                ResourceAmount {
                    resource: wallet,
                    amount: Amount {
                        asset: asset("mint-b", 9),
                        atoms: Quantity::new("1000000000").unwrap(),
                    },
                },
            ],
            positions: vec![position],
            ..FinancialState::default()
        },
        observed_at: "2026-10-14T00:00:00Z".into(),
        complete: true,
        coverage: vec!["solana:mainnet-beta:wallet".into()],
        pending: vec![],
    };

    let projected = projected_state(&snapshot).unwrap();
    assert_eq!(projected.cash.len(), 2);
    assert_eq!(
        projected.cash[0].amount.maximum,
        Quantity::new("500000").unwrap()
    );
    assert_eq!(
        projected.cash[1].amount.maximum,
        Quantity::new("1000000000").unwrap()
    );
    assert_eq!(projected.positions.len(), 1);
    assert_eq!(
        projected.positions[0]
            .backing_valuation
            .as_ref()
            .unwrap()
            .total_usd_micros,
        Quantity::new("201000000").unwrap()
    );
    assert_eq!(
        projected.positions[0].asset.reference,
        "meteora:liquidity:dlmm:position"
    );
    assert_eq!(
        projected.positions[0].amount.maximum,
        Quantity::new("7").unwrap()
    );
}
