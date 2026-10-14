use super::*;

fn fixture(
    source_chain: u64,
    cap: &str,
) -> (
    Store,
    fin_domain::BridgeTransferRecord,
    fin_domain::BridgeSettlementProof,
    fin_domain::financial_plans::FinancialPlanStepPermission,
    CostBound,
) {
    let transfer_id = "SYNTHETIC-relay-transfer";
    let step_id = "bridge";
    let plan_id = "SYNTHETIC-plan";
    let source_asset = asset(&format!("ethereum:{source_chain}"), "native", 18);
    let destination_asset = asset("ethereum:4663", "native", 18);
    let source_resource = resource("ethereum:1:0x1111111111111111111111111111111111111111");
    let destination_resource = resource("ethereum:4663:0x2222222222222222222222222222222222222222");
    let principal = Amount {
        asset: source_asset.clone(),
        atoms: quantity("100"),
    };
    let fee_cap = CostBound {
        location: AccountingLocation::Cash {
            resource: source_resource.clone(),
        },
        amount: Amount {
            asset: source_asset.clone(),
            atoms: quantity("20"),
        },
        treatment: CostTreatment::IncludedInDebit,
    };
    let reviewed_cap = CostBound {
        amount: Amount {
            atoms: quantity(cap),
            ..fee_cap.amount.clone()
        },
        ..fee_cap.clone()
    };
    let quote = json!({
        "provider":"relay",
        "intent":{
            "source":{"chain_id":source_chain,"token":null,"decimals":18},
            "destination_chain_id":4663,
            "destination_assets":[{"chain_id":4663,"token":null,"decimals":18}]
        },
        "gross_debit":{"asset":source_asset,"atoms":"100"},
        "fees":[{"kind":"withheld_from_output","amount":{"asset":source_asset,"atoms":"1"}}]
    });
    let prepared = PreparedOperation {
        operation: "bridge.execute".into(),
        input: json!({"transferId":transfer_id,"stepId":"relay-deposit","stepKind":"deposit"}),
        binding: json!({"transfer_id":transfer_id,"step_id":"relay-deposit","step_kind":"deposit","quote":quote}),
        summary: "synthetic Relay deposit".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: now(),
        expires_at: now(),
        valuation: None,
        intent: FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source_resource.clone(),
                amount: principal.clone(),
            }],
            maximum_costs: vec![fee_cap.clone()],
            expected: vec![],
            locks: vec![],
        },
        operation_version: 1,
        action_deadline: None,
    };
    let mut effects = VerifiedEffects {
        evidence_version: 1,
        submission: Submission {
            kind: "synthetic".into(),
            reference: "SYNTHETIC-tx".into(),
            data: json!({}),
        },
        effects: vec![
            FinancialEffect::CashDebit {
                resource: source_resource.clone(),
                amount: principal.clone(),
            },
            FinancialEffect::TransitPrincipal {
                transfer_id: transfer_id.into(),
                amount: principal.clone(),
            },
        ],
        evidence: json!({"fixture":"synthetic"}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    effects.proof_digest = verified_effect_digest(&effects).unwrap();
    let operation = OperationRecord {
        id: "SYNTHETIC-deposit-operation".into(),
        request_id: "SYNTHETIC-deposit-request".into(),
        fingerprint: "SYNTHETIC-fingerprint".into(),
        fingerprint_version: 1,
        operation: "bridge.execute".into(),
        input: prepared.input.clone(),
        status: Status::Completed,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(effects.submission.clone()),
        dispatch_evidence: None,
        position_baseline: None,
        effects: Some(effects),
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let mut store = Store::in_memory().unwrap();
    store
        .create_request(
            &operation.id,
            &operation.request_id,
            &operation.fingerprint,
            &operation.operation,
            None,
            "completed",
            &to_value(&operation).unwrap(),
            "synthetic-time",
        )
        .unwrap();
    let transfer = fin_domain::BridgeTransferRecord {
        version: 1,
        id: transfer_id.into(),
        grant_id: None,
        plan_scope: Some(fin_domain::BridgePlanScope {
            plan_id: plan_id.into(),
            revision_id: "SYNTHETIC-revision".into(),
        }),
        metadata: json!({}),
        settlement_expectation: Some(fin_domain::BridgeSettlementExpectation {
            destination: vec![fin_domain::BridgeSettlementTarget {
                resource: destination_resource.clone(),
                asset: destination_asset.clone(),
                minimum_atoms: quantity("1"),
            }],
            refund: vec![],
        }),
        approved_bounds: Some(
            json!({"fee_caps":[{"asset":source_asset,"kind":"withheld_from_output","maximum":cap}]}),
        ),
        prospective_debit: None,
        state: fin_domain::BridgeTransferState::Completed,
        quote_revisions: vec![],
        steps: vec![fin_domain::BridgeStepLink {
            transfer_id: transfer_id.into(),
            grant_id: None,
            plan_authority: Some(fin_domain::BridgePlanAuthority {
                plan_id: plan_id.into(),
                revision_id: "SYNTHETIC-revision".into(),
                plan_step_id: step_id.into(),
            }),
            step_id: "relay-deposit".into(),
            operation_id: Some(operation.id),
            kind: fin_domain::BridgeStepKind::Deposit,
            depends_on: vec![],
            state: fin_domain::BridgeStepState::Settled,
        }],
        allocation_owner: fin_domain::BridgeAllocationOwner::Transfer {
            transfer_id: transfer_id.into(),
        },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    let mut proof = fin_domain::BridgeSettlementProof {
        transfer_id: transfer_id.into(),
        outcome: fin_domain::BridgeSettlementOutcome::Destination,
        evidence_id: "SYNTHETIC-receipt".into(),
        effects: vec![FinancialEffect::UsableCredit {
            to: destination_resource,
            amount: Amount {
                asset: destination_asset,
                atoms: quantity("90"),
            },
        }],
        evidence: json!({}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    proof.proof_digest = fin_domain::bridge_settlement_digest(&proof).unwrap();
    let permission = fin_domain::financial_plans::FinancialPlanStepPermission {
        step_id: step_id.into(),
        generated_input_fields: vec![],
        exact: None,
        permission: Some(fin_domain::financial_plans::PlanPermission {
            spec: fin_domain::boundaries::BoundarySpec {
                fixed: json!({}),
                parameters: json!({}),
                maximum_debits: vec![],
                maximum_costs: vec![reviewed_cap],
                summary: "synthetic fee cap".into(),
            },
            recovery_parent: None,
            possible_credits: vec![],
            fee_assets: vec![],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        }),
    };
    let candidate = CostBound {
        location: AccountingLocation::Cash {
            resource: source_resource,
        },
        amount: Amount {
            asset: source_asset,
            atoms: quantity("10"),
        },
        treatment: CostTreatment::IncludedInDebit,
    };
    (store, transfer, proof, permission, candidate)
}

#[test]
fn measured_relay_cost_may_differ_from_quote_estimate_but_must_fit_frozen_cap() {
    let (store, transfer, proof, permission, candidate) = fixture(1, "10");
    crate::validate_bridge_native_withheld_cost(&store, &transfer, &proof, &permission, &candidate)
        .unwrap();

    let (store, transfer, proof, permission, candidate) = fixture(1, "9");
    let error = crate::validate_bridge_native_withheld_cost(
        &store,
        &transfer,
        &proof,
        &permission,
        &candidate,
    )
    .unwrap_err();
    assert_eq!(error.code, "PLAN_BRIDGE_COST_OUT_OF_SCOPE");
}

#[test]
fn measured_relay_cost_rejects_non_registry_native_networks() {
    let (store, transfer, proof, permission, candidate) = fixture(56, "20");
    let error = crate::validate_bridge_native_withheld_cost(
        &store,
        &transfer,
        &proof,
        &permission,
        &candidate,
    )
    .unwrap_err();
    assert_eq!(error.code, "PLAN_BRIDGE_SOURCE_EVIDENCE_INVALID");
}
