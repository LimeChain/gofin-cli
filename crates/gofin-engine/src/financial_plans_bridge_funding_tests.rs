use super::*;

fn fixture() -> (FinancialPlanAuthorizationRecord, PreparedOperation, Store) {
    let mut plan = funding_record(
        Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
        "10",
    );
    plan.revision.spec.steps[1].operation = "bridge.execute".into();
    plan.revision.permissions[1]
        .permission
        .as_mut()
        .unwrap()
        .spec
        .maximum_debits = vec![resource_amount("acct", "USDT", "20")];
    let mut prepared = funded_prepared("0", "1");
    prepared.operation = "bridge.execute".into();
    let prospective = gofin_domain::BridgeProspectiveDebit {
        deposit_step_id: "deposit".into(),
        resource: ResourceId("acct".into()),
        maximum: Amount {
            asset: asset("USDT"),
            atoms: Quantity::new("20").unwrap(),
        },
    };
    prepared.binding = json!({(gofin_domain::BRIDGE_STEP_METADATA_KEY):{
        "transfer_id":"transfer", "step_id":"approve", "kind":"approval", "grant_id":null,
        "prospective_debit":prospective,
        "source_principal":resource_amount("acct","USDT","10")
    }});
    let transfer = gofin_domain::BridgeTransferRecord {
        version: 1,
        id: "transfer".into(),
        grant_id: None,
        plan_scope: None,
        metadata: Value::Null,
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: Some(prospective),
        state: gofin_domain::BridgeTransferState::Prepared,
        quote_revisions: vec![],
        steps: vec![
            gofin_domain::BridgeStepLink {
                transfer_id: "transfer".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "approve".into(),
                operation_id: None,
                kind: gofin_domain::BridgeStepKind::Approval,
                depends_on: vec![],
                state: gofin_domain::BridgeStepState::Unsubmitted,
            },
            gofin_domain::BridgeStepLink {
                transfer_id: "transfer".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "deposit".into(),
                operation_id: None,
                kind: gofin_domain::BridgeStepKind::Deposit,
                depends_on: vec!["approve".into()],
                state: gofin_domain::BridgeStepState::Unsubmitted,
            },
        ],
        allocation_owner: gofin_domain::BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    let mut store = Store::in_memory().unwrap();
    store.save_bridge_transfer(&transfer).unwrap();
    (plan, prepared, store)
}

#[test]
fn bridge_allowance_uses_exact_principal_not_relaxed_cap_without_consuming_it() {
    let (plan, prepared, store) = fixture();
    let before = serde_json::to_value(&plan.usage).unwrap();
    validate_plan_funding_with_store(&plan, "step-1", &prepared, &store).unwrap();
    assert_eq!(serde_json::to_value(&plan.usage).unwrap(), before);
    assert_eq!(plan.usage.verified_proceeds[0].consumed, Quantity::zero());
    assert!(plan.usage.reservations.is_empty());
    // Once the deposit is selected, exact actual debit checks remain active.
    let mut deposit = funded_prepared("10", "0");
    deposit.operation = "bridge.execute".into();
    validate_plan_funding_with_store(&plan, "step-1", &deposit, &store).unwrap();
    deposit.intent.debits[0].amount.atoms = Quantity::new("11").unwrap();
    assert_eq!(
        validate_plan_funding_with_store(&plan, "step-1", &deposit, &store)
            .unwrap_err()
            .code,
        "PLAN_FUNDING_NOT_SETTLED"
    );
}

#[test]
fn bridge_allowance_requires_verified_producer_and_preserves_fee_and_fraction_checks() {
    let (mut plan, prepared, store) = fixture();
    plan.usage.verified_proceeds[0].available = Quantity::new("9").unwrap();
    assert_eq!(
        validate_plan_funding_with_store(&plan, "step-1", &prepared, &store)
            .unwrap_err()
            .code,
        "PLAN_FUNDING_NOT_SETTLED"
    );
    plan.usage.verified_proceeds[0].available = Quantity::new("10").unwrap();
    plan.usage.verified_proceeds[0].producing_step_id = "unrelated".into();
    assert_eq!(
        validate_plan_funding_with_store(&plan, "step-1", &prepared, &store)
            .unwrap_err()
            .code,
        "PLAN_FUNDING_NOT_SETTLED"
    );
    plan.usage.verified_proceeds[0].producing_step_id = "producer".into();
    plan.revision.spec.steps[1].funding_dependencies[0].funding_scope =
        Some(financial_plans::FinancialPlanFundingScope::AllDebits);
    assert_eq!(
        validate_plan_funding_with_store(&plan, "step-1", &prepared, &store)
            .unwrap_err()
            .code,
        "PLAN_FUNDING_NOT_SETTLED",
        "the source-denominated additional fee also needs funding"
    );
    plan.revision.spec.steps[1].funding_dependencies[0].funding_scope =
        Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly);
    // Supply the directly computed ledger for this unit test; store replay has
    // separate regressions and the consuming harness verifies real replay.
    plan.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
    let principal = bridge_allowance_funding_principal(&plan, "step-1", &prepared, &store).unwrap();
    assert_eq!(
        validate_plan_funding_inner(&plan, "step-1", &prepared, None, principal.as_ref())
            .unwrap_err()
            .code,
        "PLAN_FUNDING_FRACTION_EXCEEDED"
    );
}

#[test]
fn bridge_allowance_does_not_exempt_unbound_or_unrelated_candidates() {
    let (plan, prepared, store) = fixture();
    for change in [
        "missing_marker",
        "missing_principal",
        "ordinary_write",
        "deposit",
    ] {
        let mut candidate = prepared.clone();
        match change {
            "missing_marker" => candidate.binding = json!({}),
            "missing_principal" => {
                candidate.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]
                    .as_object_mut()
                    .unwrap()
                    .remove("source_principal");
            }
            "ordinary_write" => candidate.operation = "synthetic.write".into(),
            "deposit" => {
                candidate.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["kind"] = json!("deposit")
            }
            _ => unreachable!(),
        }
        assert_eq!(
            validate_plan_funding_with_store(&plan, "step-1", &candidate, &store)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_UNUSED",
            "{change}"
        );
    }
    for change in ["wrong_asset", "wrong_link", "over_cap", "wrong_parent"] {
        let mut candidate = prepared.clone();
        let mut changed_plan = plan.clone();
        match change {
            "wrong_asset" => {
                candidate.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["source_principal"]["amount"]
                    ["asset"]["reference"] = json!("ETH")
            }
            "wrong_link" => {
                candidate.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["step_id"] =
                    json!("deposit")
            }
            "over_cap" => {
                candidate.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["source_principal"]["amount"]
                    ["atoms"] = json!("21")
            }
            "wrong_parent" => changed_plan.revision.permissions[1]
                .permission
                .as_mut()
                .unwrap()
                .spec
                .maximum_debits
                .clear(),
            _ => unreachable!(),
        }
        assert_eq!(
            validate_plan_funding_with_store(&changed_plan, "step-1", &candidate, &store)
                .unwrap_err()
                .code,
            "PLAN_BRIDGE_FUNDING_INVALID",
            "{change}"
        );
    }
}

#[test]
fn bridge_allowance_rejects_a_foreign_claimed_or_changed_saved_transfer() {
    for change in [
        "foreign_plan",
        "claimed_child",
        "changed_bound",
        "missing_deposit",
    ] {
        let (plan, prepared, store) = fixture();
        let mut transfer = store.bridge_transfer("transfer").unwrap().unwrap();
        transfer.version += 1;
        match change {
            "foreign_plan" => {
                transfer.plan_scope = Some(gofin_domain::BridgePlanScope {
                    plan_id: "other".into(),
                    revision_id: "other".into(),
                })
            }
            "claimed_child" => transfer.steps[0].operation_id = Some("earlier-attempt".into()),
            "changed_bound" => {
                transfer.prospective_debit.as_mut().unwrap().maximum.atoms =
                    Quantity::new("30").unwrap()
            }
            "missing_deposit" => transfer.steps.truncate(1),
            _ => unreachable!(),
        }
        assert_eq!(
            bridge_allowance_principal_from_transfer(
                &plan,
                "step-1",
                &prepared.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY],
                &transfer
            )
            .unwrap_err()
            .code,
            "PLAN_BRIDGE_FUNDING_INVALID",
            "{change}"
        );
    }
}

#[test]
fn bridge_allowance_reset_then_approval_preserves_parent_and_unspent_proceeds() {
    let (plan, prepared, store) = fixture();
    let mut transfer = store.bridge_transfer("transfer").unwrap().unwrap();
    let mut reset = transfer.steps[0].clone();
    reset.step_id = "reset".into();
    reset.kind = gofin_domain::BridgeStepKind::ResetAllowance;
    transfer.steps[0].depends_on = vec!["reset".into()];
    transfer.steps.insert(0, reset);
    transfer.version += 1;
    let mut reset_prepared = prepared.clone();
    reset_prepared.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["step_id"] = json!("reset");
    reset_prepared.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["kind"] =
        json!("reset_allowance");
    let principal = bridge_allowance_principal_from_transfer(
        &plan,
        "step-1",
        &reset_prepared.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY],
        &transfer,
    )
    .unwrap();
    validate_plan_funding_inner(&plan, "step-1", &reset_prepared, None, Some(&principal)).unwrap();
    transfer.steps[0].state = gofin_domain::BridgeStepState::Settled;
    transfer.steps[0].operation_id = Some("reset-operation".into());
    transfer.steps[0].plan_authority = Some(gofin_domain::BridgePlanAuthority {
        plan_id: plan.revision.plan_id.clone(),
        revision_id: plan.revision.revision_id.clone(),
        plan_step_id: "step-1".into(),
    });
    transfer.plan_scope = Some(gofin_domain::BridgePlanScope {
        plan_id: plan.revision.plan_id.clone(),
        revision_id: plan.revision.revision_id.clone(),
    });
    transfer.state = gofin_domain::BridgeTransferState::ApprovalPending;
    transfer.version += 1;
    let principal = bridge_allowance_principal_from_transfer(
        &plan,
        "step-1",
        &prepared.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY],
        &transfer,
    )
    .unwrap();
    validate_plan_funding_inner(&plan, "step-1", &prepared, None, Some(&principal)).unwrap();
    assert_eq!(plan.usage.verified_proceeds[0].consumed, Quantity::zero());
}
