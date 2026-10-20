use super::*;

fn fixture(state: gofin_domain::BridgeStepState) -> (FinancialPlanAuthorizationRecord, Vec<gofin_domain::BridgeTransferRecord>) {
    let mut prior = prior_record();
    prior.attempts.clear();
    prior.usage = FinancialPlanUsage::default();
    let step = &mut prior.revision.spec.steps[0];
    step.operation = "bridge.transfer".into();
    step.input = json!({"transferId":"transfer", "stepId":"deposit", "stepKind":"deposit", "bounds":{"maximum_source_debit":"30","minimum_net_receipt":{"atoms":"29"}}});
    prior.revision.permissions[0] = FinancialPlanStepPermission {
        step_id: step.step_id.clone(),
        generated_input_fields: vec![],
        permission: None,
        exact: Some(FinancialPlanExactPermission {
            scope: FinancialPlanExactScope {
                mode: financial_plans::FinancialPlanExactMode::Exact,
                maximum_debits: vec![resource_amount("acct", "USDT", "30")],
                maximum_costs: vec![],
                possible_credits: vec![],
            },
            definition_hash: "original-definition".into(),
            connections_hash: "original-connections".into(),
        }),
    };
    let transfer = gofin_domain::BridgeTransferRecord {
        version: 1,
        id: "transfer".into(),
        grant_id: None,
        plan_scope: Some(gofin_domain::BridgePlanScope {
            plan_id: prior.revision.plan_id.clone(),
            revision_id: prior.revision.revision_id.clone(),
        }),
        metadata: Value::Null,
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: gofin_domain::BridgeTransferState::Prepared,
        quote_revisions: vec![],
        steps: vec![gofin_domain::BridgeStepLink {
            transfer_id: "transfer".into(),
            grant_id: None,
            plan_authority: Some(gofin_domain::BridgePlanAuthority {
                plan_id: prior.revision.plan_id.clone(),
                revision_id: prior.revision.revision_id.clone(),
                plan_step_id: "step-1".into(),
            }),
            step_id: "deposit".into(),
            operation_id: (state != gofin_domain::BridgeStepState::Unsubmitted)
                .then(|| "original-operation".into()),
            kind: gofin_domain::BridgeStepKind::Deposit,
            depends_on: vec![],
            state,
        }],
        allocation_owner: gofin_domain::BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    (prior, vec![transfer])
}

#[test]
fn exact_bridge_parent_renews_without_adapter_cumulative_scope() {
    for state in [
        gofin_domain::BridgeStepState::Unsubmitted,
        gofin_domain::BridgeStepState::Submitted,
        gofin_domain::BridgeStepState::Settled,
    ] {
        let (prior, store) = fixture(state);
        let before = store.clone();
        validate_bridge_revision_scope_records(
            &prior,
            &prior.revision.spec,
            &prior.revision.permissions,
            &store,
        )
        .unwrap();
        assert_eq!(store, before);
        assert!(prior.revision.permissions[0].permission.is_none());
    }
}

#[test]
fn exact_bridge_parent_rejects_changed_child_route_bounds_and_permission() {
    let (prior, store) = fixture(gofin_domain::BridgeStepState::Unsubmitted);
    for (pointer, value) in [
        ("/transferId", json!("other-transfer")),
        ("/stepId", json!("other-child")),
        ("/stepKind", json!("mint")),
        ("/bounds/maximum_source_debit", json!("31")),
        ("/bounds/minimum_net_receipt/atoms", json!("28")),
    ] {
        let mut next = prior.revision.spec.clone();
        *next.steps[0].input.pointer_mut(pointer).unwrap() = value;
        assert_eq!(
            validate_bridge_revision_scope_records(
                &prior,
                &next,
                &prior.revision.permissions,
                    &store
            )
            .unwrap_err()
            .code,
            "PLAN_BRIDGE_PARENT_SCOPE_CHANGED",
            "{pointer}"
        );
    }
    let mut next = prior.revision.spec.clone();
    next.steps[0].input["route"] = json!("other-route");
    assert_eq!(
        validate_bridge_revision_scope_records(
            &prior,
            &next,
            &prior.revision.permissions,
            &store
        )
        .unwrap_err()
        .code,
        "PLAN_BRIDGE_PARENT_SCOPE_CHANGED"
    );
    let mut permissions = prior.revision.permissions.clone();
    permissions[0].exact.as_mut().unwrap().scope.maximum_debits[0]
        .amount
        .atoms = Quantity::new("31").unwrap();
    assert_eq!(
        validate_bridge_revision_scope_records(
            &prior,
            &prior.revision.spec,
            &permissions,
            &store
        )
        .unwrap_err()
        .code,
        "PLAN_BRIDGE_PARENT_SCOPE_CHANGED"
    );
}

#[test]
fn exact_bridge_parent_cannot_drop_submitted_child_or_hide_missing_parent() {
    let (prior, store) = fixture(gofin_domain::BridgeStepState::Submitted);
    let mut next = prior.revision.spec.clone();
    next.steps.clear();
    assert_eq!(
        validate_bridge_revision_scope_records(&prior, &next, &[], &store)
            .unwrap_err()
            .code,
        "PLAN_BRIDGE_PARENT_SCOPE_CHANGED"
    );
    let mut corrupt = prior.clone();
    corrupt.revision.spec.steps[0].input["transferId"] = json!("other-parent");
    assert_eq!(
        validate_bridge_revision_scope_records(
            &corrupt,
            &corrupt.revision.spec,
            &corrupt.revision.permissions,
            &store
        )
        .unwrap_err()
        .code,
        "PLAN_BRIDGE_PARENT_SCOPE_MISSING"
    );
}
