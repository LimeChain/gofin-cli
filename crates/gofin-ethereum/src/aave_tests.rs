//! Aave adapter permissions/risk regressions. All wallets and RPCs synthetic.
use super::*;
fn bounds() -> Value {
    json!({"min_amount":"0.1","max_amount":"2","max_fee_eth":"0.001","max_cumulative_borrow":"2.01"})
}
#[tokio::test]
async fn future_borrow_scope_needs_no_current_collateral_or_allowance_and_fixes_risk() {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let state = states.get_mut(&20).unwrap();
        state.scaled_supply = U256::from(10_000_000);
        state.collateral = true;
    }
    let a = adapter(rpc.clone());
    let mut input = aave_input("aave.borrow");
    input["max_debt_usd"] = json!("100");
    input["max_debt_to_collateral_bps"] = json!(2000);
    let scope = a
        .prepare_plan_permission("aave.borrow", &input, &bounds())
        .await
        .unwrap();
    assert_eq!(
        scope.possible_credits[0].asset.reference,
        lower_address(addr(101))
    );
    assert!(
        scope.spec.maximum_debits.is_empty(),
        "borrow is an obligation and wallet credit, not a cash debit"
    );
    assert_eq!(
        scope.cumulative_scope.unwrap().maximum_obligations[0]
            .amount
            .atoms,
        Quantity::new("2010000").unwrap()
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_estimateGas" || m == "eth_sendRawTransaction")
    );
    let prepared = a.prepare("aave.borrow", &input).await.unwrap();
    a.validate_boundary_candidate(&scope.spec, &prepared)
        .unwrap();
    let mut changed = prepared.clone();
    changed.input["min_health_factor"] = json!("1.1");
    assert_eq!(
        a.validate_boundary_candidate(&scope.spec, &changed)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
    let mut changed = prepared.clone();
    changed.input["amount"] = json!("3");
    assert_eq!(
        a.validate_boundary_candidate(&scope.spec, &changed)
            .unwrap_err()
            .code,
        "BOUNDARY_VIOLATION"
    );
}

#[tokio::test]
async fn withdrawal_plan_permission_declares_bounded_position_receipt_capacity() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc);
    let mut input = aave_input("aave.withdraw");
    input["amount"] = json!("all");
    input["max_amount"] = json!("2");
    let mut plan_bounds = bounds();
    plan_bounds
        .as_object_mut()
        .unwrap()
        .remove("max_cumulative_borrow");

    let scope = a
        .prepare_plan_permission("aave.withdraw", &input, &plan_bounds)
        .await
        .unwrap();
    let capacity = scope.deferred_receipt.expect("bounded receipt capacity");

    assert!(scope.spec.maximum_debits.is_empty());
    assert_eq!(capacity.resource, scope.possible_credits[0].resource);
    assert_eq!(capacity.amount.asset, scope.possible_credits[0].asset);
    assert_eq!(capacity.amount.atoms, Quantity::new("2000000").unwrap());
}

#[tokio::test]
async fn market_selection_is_enabled_only_and_keeps_legacy_default() {
    let rpc = Arc::new(Fixture::new());
    let mut a = adapter(rpc.clone());
    a.config.aave_markets.push(AaveConfig {
        pool: lower_address(addr(301)),
        oracle: lower_address(addr(400)),
    });
    let mut input = aave_input("aave.supply");
    let default = a.prepare("aave.supply", &input).await.unwrap();
    assert_eq!(default.binding["pool"], json!(lower_address(addr(300))));
    input["market"] = json!(lower_address(addr(999)));
    assert_eq!(
        a.prepare("aave.supply", &input).await.unwrap_err().code,
        "AAVE_MARKET_UNAVAILABLE"
    );
    input["market"] = json!(lower_address(addr(301)));
    rpc.call_overrides.lock().unwrap().insert(
        AddressesProvider::getPoolCall::SELECTOR,
        abi_value(addr(301)),
    );
    rpc.call_overrides
        .lock()
        .unwrap()
        .insert(AToken::POOLCall::SELECTOR, abi_value(addr(301)));
    let selected = a.prepare("aave.supply", &input).await.unwrap();
    assert_eq!(selected.binding["pool"], json!(lower_address(addr(301))));
    assert!(selected.intent.expected.iter().any(|e| matches!(e,EffectConstraint::Position {position,..} if position.contains(&lower_address(addr(301))))));
}
#[tokio::test]
async fn collateral_removal_preserves_debt_backed_by_other_collateral_and_enforces_floor() {
    let rpc = Arc::new(Fixture::new());
    rpc.states.lock().unwrap().insert(
        20,
        AtBlock {
            scaled_supply: U256::from(10_000_000),
            scaled_debt: U256::from(1_000_000),
            income: ray(),
            debt_index: ray(),
            supply_previous: ray(),
            debt_previous: ray(),
            collateral: true,
            ..Default::default()
        },
    );
    rpc.call_overrides.lock().unwrap().insert(
        Pool::getUserAccountDataCall::SELECTOR,
        abi_value((
            U256::from(2_000_000_000u64),
            U256::from(100_000_000),
            U256::from(1_500_000_000),
            U256::from(8500),
            U256::from(8000),
            U256::from(17) * U256::from(10).pow(U256::from(18)),
        )),
    );
    let a = adapter(rpc.clone());
    let mut input = aave_input("aave.collateral");
    assert_eq!(
        a.prepare("aave.collateral", &input).await.unwrap_err().code,
        "AAVE_RISK_BOUNDS_REQUIRED"
    );
    input["min_health_factor"] = json!("3");
    a.prepare("aave.collateral", &input).await.unwrap();
    input["min_health_factor"] = json!("10");
    assert_eq!(
        a.prepare("aave.collateral", &input).await.unwrap_err().code,
        "HEALTH_FACTOR_LIMIT"
    );
}
#[tokio::test]
async fn borrow_portfolio_debt_and_ratio_caps_reject_before_simulation() {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let state = states.get_mut(&20).unwrap();
        state.scaled_supply = U256::from(10_000_000);
        state.collateral = true;
    }
    let a = adapter(rpc.clone());
    let mut input = aave_input("aave.borrow");
    input["max_debt_usd"] = json!("0.5");
    assert_eq!(
        a.prepare("aave.borrow", &input).await.unwrap_err().code,
        "AAVE_DEBT_LIMIT"
    );
    input["max_debt_usd"] = json!("10");
    input["amount"] = json!("2");
    input["max_debt_to_collateral_bps"] = json!(1);
    assert_eq!(
        a.prepare("aave.borrow", &input).await.unwrap_err().code,
        "AAVE_DEBT_COLLATERAL_LIMIT"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_estimateGas" || m == "eth_sendRawTransaction")
    );
}
#[tokio::test]
async fn borrow_plan_advisory_reports_current_position_breaches_without_blocking() {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let state = states.get_mut(&20).unwrap();
        state.scaled_supply = U256::from(10_000_000);
        state.collateral = true;
    }
    let a = adapter(rpc.clone());
    let mut input = aave_input("aave.borrow");
    input["max_debt_usd"] = json!("0.5");
    let warnings = a.plan_advisories("aave.borrow", &input).await.unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("AAVE_DEBT_LIMIT"));
    assert!(warnings[0].contains("including existing debt"));
    input["max_debt_usd"] = json!("10");
    assert!(
        a.plan_advisories("aave.borrow", &input)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        a.plan_advisories("aave.supply", &input)
            .await
            .unwrap()
            .is_empty()
    );
}

struct SyntheticPlanApproval;

struct UnitSnapshotPrices;
#[async_trait]
impl WalletValuation for UnitSnapshotPrices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let price = Quantity::from_u64(
            if asset.reference == "native" || asset.reference == lower_address(addr(100)) {
                2_000_000_000
            } else {
                2_000_000
            },
        );
        Ok((
            units.mul_div_ceil(&price, &quantity(pow10(asset.decimals)?))?,
            "synthetic unit snapshot".into(),
        ))
    }
    async fn quote_observation(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> Result<(Quantity, String, String)> {
        let at = if units == quantity(pow10(asset.decimals)?) {
            now()
        } else {
            (chrono::Utc::now() - chrono::Duration::minutes(3)).to_rfc3339()
        };
        let (value, source) = self.quote_usd(asset, units).await?;
        Ok((value, source, at))
    }
}
#[tokio::test]
async fn wallet_and_aave_usd_scalars_use_the_same_attributable_price_rows() {
    let a = adapter(Arc::new(Fixture::new())).with_wallet_valuation(Arc::new(UnitSnapshotPrices));
    for (operation, input) in [
        ("aave.supply", aave_input("aave.supply")),
        (
            "wallet.wrap",
            json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"request_id":"synthetic-coherent-wallet","amount":"0.001","max_fee_eth":"0.001"}),
        ),
    ] {
        let p = a.prepare(operation, &input).await.unwrap();
        let v = p.valuation.as_ref().unwrap();
        assert_eq!(v.movement_usd_micros, Quantity::from_u64(2_020_000));
        let fee = p
            .intent
            .maximum_costs
            .iter()
            .find(|c| c.amount.asset.reference == "native")
            .unwrap();
        let row = v
            .prices
            .iter()
            .find(|r| r.asset == fee.amount.asset)
            .unwrap();
        let raw_fee: u64 = fee
            .amount
            .atoms
            .mul_div_ceil(&row.usd_micros_per_unit, &row.unit_atoms)
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        // These new candidates reserve the disclosed 1% price buffer before
        // upward cent rounding; the attributable unit prices remain factual.
        assert_eq!(row.usd_micros_per_unit, Quantity::from_u64(2_000_000_000));
        assert_eq!(
            v.maximum_fee_usd_micros,
            Quantity::from_u64((raw_fee * 101).div_ceil(1_000_000) * 10_000)
        );
        assert_eq!(
            v.observed_at,
            v.prices
                .iter()
                .map(|r| r.observed_at.clone())
                .min()
                .unwrap()
        );
        assert!(
            v.prices
                .iter()
                .all(|r| gofin_domain::price_freshness::is_fresh(
                    &r.observed_at,
                    &r.source,
                    chrono::Utc::now()
                ))
        );
    }
}

#[tokio::test]
async fn historical_positions_pin_state_and_never_select_a_write_state() {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        states.insert(
            19,
            AtBlock {
                income: ray(),
                debt_index: ray(),
                ..Default::default()
            },
        );
        states.get_mut(&20).unwrap().scaled_supply = U256::from(1_000_000);
    }
    let a = adapter(rpc.clone());
    let mut input = json!({"wallet":"synthetic","network":"evm","chain_id":1,"tokens":[lower_address(addr(101))]});
    let current = a.read("aave.positions", &input).await.unwrap();
    input["block_number"] = json!(19);
    let historical = a.read("aave.positions", &input).await.unwrap();
    assert_eq!(historical["block"], 19);
    assert_eq!(historical["block_hash"], blockhash(19));
    assert_eq!(historical["data"]["positions"][0]["supplied"], "0");
    assert_ne!(current["data"]["positions"][0]["supplied"], "0");
    let mut write = aave_input("aave.supply");
    write["block_number"] = json!(19);
    assert!(a.prepare("aave.supply", &write).await.is_err());
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c.as_str(), "eth_estimateGas" | "eth_sendRawTransaction"))
    );
}
#[async_trait]
impl Authorizer for SyntheticPlanApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-aave-plan-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
#[tokio::test]
async fn supply_plan_covers_exact_rounding_cost_and_reaches_only_the_missing_signer() {
    use gofin_domain::financial_plans::*;
    let rpc = Arc::new(Fixture::new());
    let a = Arc::new(adapter(rpc.clone()));
    let input = aave_input("aave.supply");
    let variations = json!({"min_amount":"1","max_amount":"1","max_fee_eth":"0.001"});
    let scope = a
        .prepare_plan_permission("aave.supply", &input, &variations)
        .await
        .unwrap();
    assert!(
        scope
            .spec
            .maximum_costs
            .iter()
            .any(|c| c.treatment == CostTreatment::IncludedInDebit)
    );
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![a],
        Arc::new(SyntheticPlanApproval),
        gofin_engine::EngineOptions::default(),
    )
    .unwrap();
    let request = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-aave-cost-scope".into(),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
        plan: FinancialPlanSpec {
            version: 1,
            name: "Synthetic Aave cost scope".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "supply".into(),
                operation: "aave.supply".into(),
                input: input.clone(),
                permitted_variations: variations,
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: scope.spec.maximum_debits,
                maximum_costs: scope.spec.maximum_costs,
            },
        },
    };
    let status = engine.authorize_financial_plan(request).await.unwrap();
    let record = engine
        .execute_financial_plan_step(
            &status.plan_id,
            "supply",
            OperationRequest {
                request_id: "synthetic-aave-cost-attempt".into(),
                operation: "aave.supply".into(),
                input,
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        record.error.as_ref().unwrap().code,
        "PROTECTED_SIGNER_REQUIRED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn generic_exact_mode_cannot_bypass_borrow_plan_risk_or_gross_authority() {
    use gofin_domain::financial_plans::*;
    let rpc = Arc::new(Fixture::new());
    let a = Arc::new(adapter(rpc.clone()));
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![a],
        Arc::new(SyntheticPlanApproval),
        gofin_engine::EngineOptions::default(),
    )
    .unwrap();
    let input = aave_input("aave.borrow");
    let request = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-exact-borrow-bypass".into(),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
        plan: FinancialPlanSpec {
            version: 1,
            name: "Synthetic rejected exact borrow".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "borrow".into(),
                operation: "aave.borrow".into(),
                input,
                permitted_variations: json!({"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]}),
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![],
                maximum_costs: vec![],
            },
        },
    };
    assert_eq!(
        engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap_err()
            .code,
        "PLAN_APPLICATION_SCOPE_REQUIRED"
    );
    assert_eq!(
        engine
            .authorize_financial_plan(request)
            .await
            .unwrap_err()
            .code,
        "PLAN_APPLICATION_SCOPE_REQUIRED"
    );
    assert!(
        rpc.calls.lock().unwrap().is_empty(),
        "rejection precedes state reads, simulation, approval and signing"
    );
}

#[tokio::test]
async fn deployed_mode_and_receipt_restrictions_reject_before_simulation_or_submission() {
    for (case, expected) in [
        ("unknown-mode", "AAVE_MODE_UNSUPPORTED"),
        ("emode-bitmap", "AAVE_EMODE_BORROW_UNAVAILABLE"),
        ("siloed", "AAVE_SILOED_BORROW_UNAVAILABLE"),
        ("isolation-asset", "AAVE_ISOLATION_BORROW_UNAVAILABLE"),
        ("isolation-ceiling", "AAVE_ISOLATION_DEBT_CEILING"),
        ("pool-revision", "AAVE_POOL_REVISION_UNSUPPORTED"),
        ("oracle-units", "AAVE_ORACLE_UNITS_UNSUPPORTED"),
        ("receipt-asset", "AAVE_RECEIPT_IDENTITY_MISMATCH"),
    ] {
        let rpc = Arc::new(Fixture::new());
        let a = adapter(rpc.clone());
        {
            let mut overrides = rpc.call_overrides.lock().unwrap();
            match case {
                "unknown-mode" => {
                    overrides.insert(Pool::getUserEModeCall::SELECTOR, abi_value(U256::from(256)));
                }
                "emode-bitmap" => {
                    overrides.insert(Pool::getUserEModeCall::SELECTOR, abi_value(U256::from(1)));
                    overrides.insert(
                        Pool::getEModeCategoryBorrowableBitmapCall::SELECTOR,
                        abi_value(0u128),
                    );
                }
                "siloed" => {
                    overrides.insert(
                        Pool::getConfigurationCall::SELECTOR,
                        abi_value(U256::from(1) << 62),
                    );
                }
                "isolation-asset" | "isolation-ceiling" => {
                    let permitted = if case == "isolation-ceiling" {
                        U256::from(1) << 61
                    } else {
                        U256::ZERO
                    };
                    overrides.insert(
                        Pool::getConfigurationCall::SELECTOR,
                        abi_value(permitted | (U256::from(1) << 212)),
                    );
                    let mut states = rpc.states.lock().unwrap();
                    let state = states.get_mut(&20).unwrap();
                    state.collateral = true;
                    state.scaled_supply = U256::from(10_000_000);
                }
                "pool-revision" => {
                    overrides.insert(Pool::POOL_REVISIONCall::SELECTOR, abi_value(U256::from(12)));
                }
                "oracle-units" => {
                    overrides.insert(
                        Oracle::BASE_CURRENCY_UNITCall::SELECTOR,
                        abi_value(U256::from(10).pow(U256::from(18))),
                    );
                }
                "receipt-asset" => {
                    overrides.insert(
                        AToken::UNDERLYING_ASSET_ADDRESSCall::SELECTOR,
                        abi_value(addr(999)),
                    );
                }
                _ => unreachable!(),
            }
        }
        let result = if matches!(case, "pool-revision" | "receipt-asset" | "oracle-units") {
            a.prepare("aave.borrow", &aave_input("aave.borrow"))
                .await
                .map(|_| ())
        } else {
            let token = a.token(&lower_address(addr(101)), None).await.unwrap();
            a.aave_borrow_restrictions(addr(1), &token, U256::from(1_000_000))
                .await
        };
        assert_eq!(result.unwrap_err().code, expected, "{case}");
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c.as_str(), "eth_estimateGas" | "eth_sendRawTransaction")),
            "{case}"
        );
    }
}

struct PrunedClosedStateRpc {
    inner: Arc<Fixture>,
    pruned_through: AtomicU64,
    historical_reads: AtomicU64,
}
#[async_trait]
impl EvmRpc for PrunedClosedStateRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if matches!(method, "eth_call" | "eth_getCode" | "eth_getBalance")
            && params[1]
                .as_str()
                .and_then(|tag| tag.strip_prefix("0x"))
                .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                .is_some_and(|block| block <= self.pruned_through.load(Ordering::SeqCst))
        {
            self.historical_reads.fetch_add(1, Ordering::SeqCst);
            return Err(error(
                "RPC_HISTORICAL_STATE_UNAVAILABLE",
                "rpc",
                "Synthetic archive state pruned",
            ));
        }
        self.inner.request(method, params).await
    }
}
#[tokio::test]
async fn reopening_closed_positions_reobserves_zero_but_frozen_attempts_never_change_anchors() {
    let inner = Arc::new(Fixture::new());
    let mut a = adapter(inner.clone());
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let supply = a
        .snapshot_anchor(&token, addr(1), addr(110), PositionKind::Supply, 20)
        .await
        .unwrap();
    let debt = a
        .snapshot_anchor(&token, addr(1), addr(120), PositionKind::VariableDebt, 20)
        .await
        .unwrap();
    let context = PreparationContext {
        logical_operation_id: None,
        positions: vec![supply.clone(), debt],
    };
    inner.latest.store(22, Ordering::SeqCst);
    let rpc = Arc::new(PrunedClosedStateRpc {
        inner,
        pruned_through: AtomicU64::new(20),
        historical_reads: AtomicU64::new(0),
    });
    a.runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, None));
    let prepared = a
        .prepare_with_context("aave.supply", &aave_input("aave.supply"), &context)
        .await
        .unwrap();
    let bound: Binding = serde_json::from_value(prepared.binding.clone()).unwrap();
    assert_eq!(
        bound.supply_anchor.as_ref().unwrap().evidence["block"],
        json!(22)
    );
    assert_eq!(
        bound.debt_anchor.as_ref().unwrap().evidence["block"],
        json!(22)
    );
    assert_eq!(
        context.positions[0], supply,
        "Historical journal context must remain immutable"
    );
    assert_eq!(rpc.historical_reads.load(Ordering::SeqCst), 0);
    a.revalidate(&prepared).await.unwrap();
    rpc.pruned_through.store(22, Ordering::SeqCst);
    rpc.inner.latest.store(23, Ordering::SeqCst);
    assert_eq!(
        a.revalidate(&prepared).await.unwrap_err().code,
        "RPC_HISTORICAL_STATE_UNAVAILABLE",
        "Frozen preview must validate its original block, never silently reanchor"
    );
    assert!(rpc.historical_reads.load(Ordering::SeqCst) > 0);
}
#[tokio::test]
async fn closed_lineage_rejects_changed_canonical_hash_and_malformed_zero_evidence() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let zero = a
        .snapshot_anchor(&token, addr(1), addr(110), PositionKind::Supply, 20)
        .await
        .unwrap();
    for field in [
        "block_hash",
        "scaled_balance",
        "deferred_unpaid_interest",
        "underlying",
    ] {
        let mut changed = zero.clone();
        changed.evidence[field] = if field == "block_hash" {
            json!(blockhash(999))
        } else {
            json!("1")
        };
        let context = PreparationContext {
            logical_operation_id: None,
            positions: vec![changed],
        };
        let e = a
            .prepare_with_context("aave.supply", &aave_input("aave.supply"), &context)
            .await
            .unwrap_err();
        assert_eq!(e.code, "ANCHOR_PROOF_MISMATCH", "{field}");
    }
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_sendRawTransaction")
    );
}
#[tokio::test]
async fn nonzero_lineage_still_requires_original_archived_backing() {
    let inner = Arc::new(Fixture::new());
    inner
        .states
        .lock()
        .unwrap()
        .get_mut(&20)
        .unwrap()
        .scaled_supply = U256::from(1_000_000);
    let mut a = adapter(inner.clone());
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let prior = a
        .snapshot_anchor(&token, addr(1), addr(110), PositionKind::Supply, 20)
        .await
        .unwrap();
    assert!(!prior.principal.atoms.is_zero());
    inner.latest.store(22, Ordering::SeqCst);
    let rpc = Arc::new(PrunedClosedStateRpc {
        inner,
        pruned_through: AtomicU64::new(20),
        historical_reads: AtomicU64::new(0),
    });
    a.runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, None));
    let context = PreparationContext {
        logical_operation_id: None,
        positions: vec![prior],
    };
    assert_eq!(
        a.prepare_with_context("aave.supply", &aave_input("aave.supply"), &context)
            .await
            .unwrap_err()
            .code,
        "RPC_HISTORICAL_STATE_UNAVAILABLE"
    );
    assert!(rpc.historical_reads.load(Ordering::SeqCst) > 0);
    assert!(
        !rpc.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn wallet_preparation_batches_native_and_wrapped_asset_and_rejects_incomplete_rows() {
    struct BatchPrices {
        calls: std::sync::Mutex<Vec<Vec<Amount>>>,
        incomplete: bool,
    }
    #[async_trait]
    impl WalletValuation for BatchPrices {
        async fn quote_usd(&self, _: &AssetId, _: Quantity) -> Result<(Quantity, String)> {
            panic!("preparation must use the exact batch interface")
        }
        async fn quote_observations(
            &self,
            amounts: &[Amount],
        ) -> Result<Vec<(Quantity, String, String)>> {
            self.calls.lock().unwrap().push(amounts.to_vec());
            let mut values = Vec::new();
            for amount in amounts {
                values.push(
                    UnitSnapshotPrices
                        .quote_observation(&amount.asset, amount.atoms.clone())
                        .await?,
                );
            }
            if self.incomplete {
                values.pop();
            }
            Ok(values)
        }
    }
    for incomplete in [false, true] {
        let prices = Arc::new(BatchPrices {
            calls: Default::default(),
            incomplete,
        });
        let a = adapter(Arc::new(Fixture::new())).with_wallet_valuation(prices.clone());
        let result = a.prepare("wallet.wrap", &json!({"wallet":"synthetic","network":"ethereum","chain_id":1,"request_id":"synthetic-batch-wrap","amount":"0.001","max_fee_eth":"0.001"})).await;
        if incomplete {
            assert_eq!(result.unwrap_err().code, "VALUATION_UNAVAILABLE");
        } else {
            let prepared = result.unwrap();
            assert_eq!(prepared.valuation.unwrap().prices.len(), 2);
        }
        let calls = prices.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 2);
        assert_eq!(calls[0][0].asset.reference, "native");
        assert_eq!(calls[0][1].asset.reference, lower_address(addr(100)));
        assert!(
            calls[0]
                .iter()
                .all(|amount| amount.atoms == quantity(pow10(amount.asset.decimals).unwrap()))
        );
    }
}

#[tokio::test]
async fn default_wallet_price_batch_bounds_overlap_and_preserves_input_order_and_errors() {
    struct Prices {
        active: AtomicUsize,
        peak: AtomicUsize,
        fail: bool,
    }
    #[async_trait]
    impl WalletValuation for Prices {
        async fn quote_usd(&self, _: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.fail && units == Quantity::from_u64(4) {
                return Err(GoFinError::validation("Synthetic price failure."));
            }
            Ok((units, "synthetic exact price".into()))
        }
    }
    let amounts = (1..=7)
        .map(|value| Amount {
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            atoms: Quantity::from_u64(value),
        })
        .collect::<Vec<_>>();
    for fail in [false, true] {
        let prices = Prices {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            fail,
        };
        let result = prices.quote_observations(&amounts).await;
        assert_eq!(prices.peak.load(Ordering::SeqCst), 3);
        if fail {
            assert_eq!(result.unwrap_err().message, "Synthetic price failure.");
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .into_iter()
                    .map(|row| row.0)
                    .collect::<Vec<_>>(),
                amounts
                    .iter()
                    .map(|amount| amount.atoms.clone())
                    .collect::<Vec<_>>()
            );
            assert_eq!(prices.active.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn borrow_plan_rounding_review_matches_dispatch_math_without_future_balances() {
    for (index, rounding) in [
        (ray(), 3u64),
        (ray() + U256::from(1), 4),
        (ray() * U256::from(2), 4),
        (ray() * U256::from(2) + U256::from(1), 5),
    ] {
        let rpc = Arc::new(Fixture::new());
        rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index = index;
        *rpc.allowance.lock().unwrap() = U256::ZERO;
        *rpc.cash_override.lock().unwrap() = Some(U256::ZERO);
        let a = adapter(rpc.clone());
        let mut input = aave_input("aave.borrow");
        input["amount"] = json!("11.7");
        input["max_debt_usd"] = json!("11.71");
        input["max_debt_to_collateral_bps"] = json!(5000);
        let mut variations = json!({"min_amount":"11.7","max_amount":"11.7","max_fee_eth":"0.001","max_cumulative_borrow":"11.7"});
        for cap in [11_700_000, 11_700_001, 11_700_000 + rounding - 1] {
            variations["max_cumulative_borrow"] = json!(Quantity::from_u64(cap).to_decimal(6));
            let error = a
                .prepare_plan_permission("aave.borrow", &input, &variations)
                .await
                .unwrap_err();
            assert_eq!(error.code, "AAVE_BORROW_BOUNDS_TOO_TIGHT");
            let details = error.details.unwrap();
            assert_eq!(details["mint_rounding_atoms"], rounding.to_string());
            assert_eq!(
                details["minimum_cumulative_borrow_atoms"],
                (11_700_000 + rounding).to_string()
            );
            assert_eq!(
                details["minimum_cumulative_borrow"],
                Quantity::from_u64(11_700_000 + rounding).to_decimal(6)
            );
            assert_eq!(details["asset"]["reference"], lower_address(addr(101)));
            assert_eq!(details["asset"]["network"], "ethereum:1");
            assert_eq!(details["market"], lower_address(addr(300)));
            assert_eq!(
                details["normalized_debt_index"],
                quantity(index).to_string()
            );
            assert_eq!(details["violations"], json!(["max_cumulative_borrow"]));
        }
        variations["max_cumulative_borrow"] =
            json!(Quantity::from_u64(11_700_000 + rounding).to_decimal(6));
        let permission = a
            .prepare_plan_permission("aave.borrow", &input, &variations)
            .await
            .unwrap();
        assert_eq!(
            permission.cumulative_scope.unwrap().maximum_obligations[0]
                .amount
                .atoms,
            Quantity::from_u64(11_700_000 + rounding)
        );
        assert!(!rpc.calls.lock().unwrap().iter().any(|method| matches!(
            method.as_str(),
            "eth_getBalance" | "eth_estimateGas" | "eth_sendRawTransaction"
        )));
    }
}

#[tokio::test]
async fn borrow_plan_review_reports_both_tight_bounds_and_does_not_bind_observation_drift() {
    let rpc = Arc::new(Fixture::new());
    rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index = ray() + U256::from(1);
    let a = adapter(rpc.clone());
    let mut input = aave_input("aave.borrow");
    input["amount"] = json!("11.7");
    input["max_debt_usd"] = json!("11.7");
    input["max_debt_to_collateral_bps"] = json!(5000);
    let mut variations = json!({"min_amount":"11.7","max_amount":"11.7","max_fee_eth":"0.001","max_cumulative_borrow":"11.700001"});
    let error = a
        .prepare_plan_permission("aave.borrow", &input, &variations)
        .await
        .unwrap_err();
    let details = error.details.unwrap();
    assert_eq!(
        details["violations"],
        json!(["max_cumulative_borrow", "max_debt_usd"])
    );
    assert_eq!(details["minimum_incremental_debt_usd"], "11.700004");
    assert_eq!(details["minimum_incremental_debt_usd_atoms"], "1170000400");
    assert_eq!(details["protocol_usd_decimals"], 8);
    variations["max_cumulative_borrow"] = json!("11.71");
    let error = a
        .prepare_plan_permission("aave.borrow", &input, &variations)
        .await
        .unwrap_err();
    assert_eq!(
        error.details.unwrap()["violations"],
        json!(["max_debt_usd"])
    );
    input["max_debt_usd"] = json!("11.71");
    let first = a
        .prepare_plan_permission("aave.borrow", &input, &variations)
        .await
        .unwrap();
    rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index += U256::from(1);
    let second = a
        .prepare_plan_permission("aave.borrow", &input, &variations)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap(),
        "fresh observations do not change an unchanged finite permission's approval binding"
    );
    rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index = ray() - U256::from(1);
    assert_eq!(
        a.prepare_plan_permission("aave.borrow", &input, &variations)
            .await
            .unwrap_err()
            .code,
        "DEBT_INDEX_INVALID"
    );
    rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index = ray();
    rpc.call_overrides.lock().unwrap().insert(
        Oracle::BASE_CURRENCY_UNITCall::SELECTOR,
        abi_value(U256::from(1)),
    );
    assert_eq!(
        a.prepare_plan_permission("aave.borrow", &input, &variations)
            .await
            .unwrap_err()
            .code,
        "AAVE_ORACLE_UNITS_UNSUPPORTED"
    );
    rpc.call_overrides
        .lock()
        .unwrap()
        .remove(&Oracle::BASE_CURRENCY_UNITCall::SELECTOR);
    rpc.call_overrides
        .lock()
        .unwrap()
        .insert(Oracle::getAssetPriceCall::SELECTOR, abi_value(U256::ZERO));
    assert_eq!(
        a.prepare_plan_permission("aave.borrow", &input, &variations)
            .await
            .unwrap_err()
            .code,
        "ORACLE_INVALID"
    );
}

#[tokio::test]
async fn borrow_plan_preview_rejects_rounding_shortfall_before_authorization() {
    use gofin_domain::financial_plans::*;
    struct NoApproval;
    #[async_trait]
    impl Authorizer for NoApproval {
        async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            panic!("invalid borrowing bounds must fail before human approval")
        }
    }
    let rpc = Arc::new(Fixture::new());
    rpc.states.lock().unwrap().get_mut(&20).unwrap().debt_index = ray() + U256::from(1);
    *rpc.allowance.lock().unwrap() = U256::ZERO;
    *rpc.cash_override.lock().unwrap() = Some(U256::ZERO);
    let a = Arc::new(adapter(rpc.clone()));
    let mut input = aave_input("aave.borrow");
    input["amount"] = json!("11.7");
    input["max_debt_usd"] = json!("11.71");
    input["max_debt_to_collateral_bps"] = json!(5000);
    let variations = json!({"min_amount":"11.7","max_amount":"11.7","max_fee_eth":"0.001","max_cumulative_borrow":"11.71"});
    let scope = a
        .prepare_plan_permission("aave.borrow", &input, &variations)
        .await
        .unwrap();
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![a],
        Arc::new(NoApproval),
        gofin_engine::EngineOptions::default(),
    )
    .unwrap();
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-borrow-rounding-review".into(),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
        plan: FinancialPlanSpec {
            version: 1,
            name: "Synthetic future-funded borrowing".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "borrow".into(),
                operation: "aave.borrow".into(),
                input,
                permitted_variations: variations,
                action_deadline_seconds: None,
                funding_dependencies: vec![],
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![],
                maximum_costs: scope.spec.maximum_costs,
            },
        },
    };
    engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.plan.steps[0].permitted_variations["max_cumulative_borrow"] = json!("11.700001");
    for authorize in [false, true] {
        let error = if authorize {
            engine
                .authorize_financial_plan(request.clone())
                .await
                .unwrap_err()
        } else {
            engine
                .preview_financial_plan(request.clone())
                .await
                .unwrap_err()
        };
        assert_eq!(error.code, "AAVE_BORROW_BOUNDS_TOO_TIGHT");
        assert_eq!(
            error.details.unwrap()["minimum_cumulative_borrow"],
            "11.700004"
        );
    }
    assert!(!rpc.calls.lock().unwrap().iter().any(|method| matches!(
        method.as_str(),
        "eth_getBalance" | "eth_estimateGas" | "eth_sendRawTransaction"
    )));
}

fn exact_supply_scope(
    a: &EthereumAdapter,
    rounding: Option<u64>,
) -> gofin_domain::financial_plans::FinancialPlanExactScope {
    let cash = AccountingLocation::Cash {
        resource: resource(1, addr(1)),
    };
    let mut costs = vec![CostBound {
        location: cash.clone(),
        amount: Amount {
            asset: a.native_asset(),
            atoms: Quantity::from_decimal("0.001", 18).unwrap(),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    if let Some(atoms) = rounding {
        costs.push(CostBound {
            location: cash,
            amount: Amount {
                asset: a.asset(&lower_address(addr(101)), 6),
                atoms: Quantity::from_u64(atoms),
            },
            treatment: CostTreatment::IncludedInDebit,
        });
    }
    gofin_domain::financial_plans::FinancialPlanExactScope {
        mode: gofin_domain::financial_plans::FinancialPlanExactMode::Exact,
        maximum_debits: vec![ResourceAmount {
            resource: resource(1, addr(1)),
            amount: Amount {
                asset: a.asset(&lower_address(addr(101)), 6),
                atoms: Quantity::from_u64(1_000_000),
            },
        }],
        maximum_costs: costs,
        possible_credits: vec![],
    }
}

#[tokio::test]
async fn exact_supply_review_requires_current_rounding_inventory_without_future_funds() {
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = U256::ZERO;
    *rpc.cash_override.lock().unwrap() = Some(U256::ZERO);
    let a = adapter(rpc.clone());
    let input = aave_input("aave.supply");
    for (index, minimum) in [
        (ray(), 3),
        (ray() + U256::from(1), 4),
        (ray() * U256::from(2) + U256::from(1), 5),
    ] {
        rpc.states.lock().unwrap().get_mut(&20).unwrap().income = index;
        for cap in [None, Some(0), Some(minimum - 1)] {
            let scope = exact_supply_scope(&a, cap);
            let err = a
                .validate_plan_feasibility("aave.supply", &input, Some(&scope))
                .await
                .unwrap_err();
            assert_eq!(err.code, "AAVE_SUPPLY_COST_SCOPE_INCOMPLETE");
            let details = err.details.unwrap();
            assert_eq!(
                details["minimum_rounding_atoms"],
                json!(minimum.to_string())
            );
            assert_eq!(
                details["minimum_rounding"],
                json!(Quantity::from_u64(minimum).to_decimal(6))
            );
            assert_eq!(details["market"], lower_address(addr(300)));
            assert_eq!(
                details["asset"],
                json!(a.asset(&lower_address(addr(101)), 6))
            );
            assert_eq!(
                details["required_rounding_cost"]["treatment"],
                "included_in_debit"
            );
            assert_eq!(
                details["native_gas_input_ceiling"]["treatment"],
                "additional_debit"
            );
        }
        for cap in [minimum, minimum + 100] {
            a.validate_plan_feasibility(
                "aave.supply",
                &input,
                Some(&exact_supply_scope(&a, Some(cap))),
            )
            .await
            .unwrap();
        }
    }
    for wrong in 0..4 {
        let mut scope = exact_supply_scope(&a, Some(100));
        let cost = &mut scope.maximum_costs[1];
        match wrong {
            0 => cost.amount.asset.network = "eip155:42161".into(),
            1 => cost.amount.asset.reference = lower_address(addr(100)),
            2 => {
                cost.location = AccountingLocation::Cash {
                    resource: resource(1, addr(2)),
                }
            }
            _ => cost.treatment = CostTreatment::AdditionalDebit,
        }
        let err = a
            .validate_plan_feasibility("aave.supply", &input, Some(&scope))
            .await
            .unwrap_err();
        assert_eq!(
            err.details.unwrap()["violation"],
            "missing_rounding_cost_identity"
        );
    }
    assert!(!rpc.calls.lock().unwrap().iter().any(|method| matches!(
        method.as_str(),
        "eth_getBalance" | "eth_estimateGas" | "eth_getTransactionCount" | "eth_sendRawTransaction"
    )));
    assert!(!rpc.call_requests.lock().unwrap().iter().any(|params| {
        gofin_evm::rpc_bytes(&params[0]["data"])
            .is_ok_and(|data| data.as_ref() == Erc20::balanceOfCall { owner: addr(1) }.abi_encode())
    }));
}

#[tokio::test]
async fn exact_supply_review_pins_selected_market_and_underlying_decimals() {
    let rpc = Arc::new(Fixture::new());
    let mut a = adapter(rpc.clone());
    a.config.aave_markets.push(AaveConfig {
        pool: lower_address(addr(301)),
        oracle: lower_address(addr(400)),
    });
    let mut input = aave_input("aave.supply");
    input["market"] = json!(lower_address(addr(999)));
    let scope = exact_supply_scope(&a, None);
    assert_eq!(
        a.validate_plan_feasibility("aave.supply", &input, Some(&scope))
            .await
            .unwrap_err()
            .code,
        "AAVE_MARKET_UNAVAILABLE"
    );
    input["market"] = json!(lower_address(addr(301)));
    input["token"] = json!(lower_address(addr(100)));
    rpc.call_overrides.lock().unwrap().insert(
        AddressesProvider::getPoolCall::SELECTOR,
        abi_value(addr(301)),
    );
    rpc.call_overrides
        .lock()
        .unwrap()
        .insert(AToken::POOLCall::SELECTOR, abi_value(addr(301)));
    rpc.call_overrides.lock().unwrap().insert(
        AToken::UNDERLYING_ASSET_ADDRESSCall::SELECTOR,
        abi_value(addr(100)),
    );
    let err = a
        .validate_plan_feasibility("aave.supply", &input, Some(&scope))
        .await
        .unwrap_err();
    assert_eq!(err.code, "AAVE_SUPPLY_COST_SCOPE_INCOMPLETE");
    let d = err.details.unwrap();
    assert_eq!(d["market"], lower_address(addr(301)));
    assert_eq!(d["asset"]["decimals"], 18);
    assert_eq!(d["asset"]["reference"], lower_address(addr(100)));
    assert_eq!(d["minimum_rounding"], "0.000000000000000004");
}

#[tokio::test]
async fn exact_supply_plan_checks_before_both_approval_paths_and_keeps_execution_bound() {
    use gofin_domain::financial_plans::*;
    let rpc = Arc::new(Fixture::new());
    let a = Arc::new(adapter(rpc.clone()));
    let scope = exact_supply_scope(&a, Some(4));
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![a],
        Arc::new(SyntheticPlanApproval),
        gofin_engine::EngineOptions::default(),
    )
    .unwrap();
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-exact-supply-review".into(),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
        plan: FinancialPlanSpec {
            version: 1,
            name: "Synthetic exact supply".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "supply".into(),
                operation: "aave.supply".into(),
                input: aave_input("aave.supply"),
                permitted_variations: json!(scope),
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: scope.maximum_debits.clone(),
                maximum_costs: scope.maximum_costs.clone(),
            },
        },
    };
    engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.plan.steps[0].permitted_variations["maximum_costs"] = json!([scope.maximum_costs[0]]);
    for authorize in [false, true] {
        let err = if authorize {
            engine
                .authorize_financial_plan(request.clone())
                .await
                .unwrap_err()
        } else {
            engine
                .preview_financial_plan(request.clone())
                .await
                .unwrap_err()
        };
        assert_eq!(err.code, "AAVE_SUPPLY_COST_SCOPE_INCOMPLETE");
        assert_eq!(err.details.unwrap()["step_id"], "supply");
    }
    request.plan.steps[0].permitted_variations = json!(scope);
    let status = engine.authorize_financial_plan(request).await.unwrap();
    rpc.states.lock().unwrap().get_mut(&20).unwrap().income = ray() * U256::from(2) + U256::from(1);
    let error = engine
        .execute_financial_plan_step(
            &status.plan_id,
            "supply",
            OperationRequest {
                request_id: "synthetic-exact-supply-drift".into(),
                operation: "aave.supply".into(),
                input: aave_input("aave.supply"),
                run_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "BOUNDARY_VIOLATION");
    assert_eq!(
        error.details.unwrap()["boundary_violation"]["candidate_value"]["amount"]["atoms"],
        "5"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}
