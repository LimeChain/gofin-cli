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
    assert!(
        prepared
            .summary
            .contains("preapproval minimum health factor")
    );
    assert!(
        prepared
            .summary
            .contains("preapproval maximum outstanding debt $100")
    );
    assert!(
        prepared
            .summary
            .contains("preapproval maximum debt/collateral 2000 bps")
    );
    assert!(
        prepared
            .intent
            .maximum_costs
            .iter()
            .any(|cost| !cost.amount.atoms.is_zero())
    );
    assert_eq!(
        gofin_domain::native_authorization_binding(&prepared)["financial_intent"]["maximum_costs"],
        serde_json::to_value(&prepared.intent.maximum_costs).unwrap()
    );
    #[cfg(target_os = "macos")]
    {
        let reason = native_approval::assert_prepared_renders(&prepared);
        let full_market = lower_address(addr(300));
        assert!(
            gofin_domain::canonical_json(&gofin_domain::authorization_binding(&prepared))
                .unwrap()
                .contains(&full_market)
        );
        assert!(reason.contains("0x0000…012c"), "market label: {reason}");
        for term in [
            "Health ≥: 1.5",
            "Debt USD ≤: 100",
            "Debt/collateral bps ≤: 2000",
        ] {
            assert!(reason.contains(term), "missing {term}: {reason}");
        }
        assert!(!reason.to_lowercase().contains("fee"), "{reason}");
    }
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
async fn standalone_supply_covers_exact_rounding_cost_and_reaches_only_the_missing_signer() {
    let rpc = Arc::new(Fixture::new());
    let a = Arc::new(adapter(rpc.clone()));
    let input = aave_input("aave.supply");
    let prepared = a.prepare("aave.supply", &input).await.unwrap();
    assert!(
        prepared
            .intent
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
    let record = engine
        .execute(OperationRequest {
            request_id: "synthetic-aave-cost-attempt".into(),
            operation: "aave.supply".into(),
            input,
            run_id: None,
        })
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
async fn retired_exact_plan_borrow_is_rejected_before_reads() {
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
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(
        engine
            .authorize_financial_plan(request)
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
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
        a.revalidate(&prepared).await.unwrap(),
        prepared,
        "Postapproval execution preserves the original anchor without recollecting financial history"
    );
    assert_eq!(rpc.historical_reads.load(Ordering::SeqCst), 0);
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
            assert!(result.unwrap().valuation.is_none());
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
async fn retired_borrow_plan_is_rejected_without_dispatch() {
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
    let request = FinancialPlanAuthorizationRequest {
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
    assert_eq!(
        engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(
        engine
            .authorize_financial_plan(request)
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
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
async fn retired_exact_supply_plan_is_rejected_before_dispatch() {
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
    let request = FinancialPlanAuthorizationRequest {
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
    assert_eq!(
        engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(
        engine
            .authorize_financial_plan(request)
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
}

#[tokio::test]
async fn supply_preparation_rpc_measurement() {
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let input = aave_input("aave.supply");
    let first = a.prepare("aave.supply", &input).await.unwrap();
    let cold = rpc.calls.lock().unwrap().len();
    rpc.calls.lock().unwrap().clear();
    a.prepare("aave.supply", &input).await.unwrap();
    let warm = rpc.calls.lock().unwrap().len();
    rpc.calls.lock().unwrap().clear();
    a.revalidate(&first).await.unwrap();
    let revalidate = rpc.calls.lock().unwrap().len();
    let next = rpc.states.lock().unwrap().get(&20).unwrap().clone();
    rpc.states.lock().unwrap().insert(21, next);
    rpc.latest.store(21, std::sync::atomic::Ordering::SeqCst);
    rpc.calls.lock().unwrap().clear();
    a.revalidate(&first).await.unwrap();
    let historical = rpc.calls.lock().unwrap().len();
    eprintln!(
        "supply downstream logical RPC calls: cold={cold}, warm={warm}, revalidate={revalidate}, historical_revalidate={historical}"
    );
}

#[tokio::test]
async fn supply_execution_retains_preapproval_facts_without_financial_rechecks() {
    for changed in ["allowance", "balance", "paused"] {
        let rpc = Arc::new(Fixture::new());
        let a = adapter(rpc.clone());
        let prepared = a
            .prepare("aave.supply", &aave_input("aave.supply"))
            .await
            .unwrap();
        match changed {
            "allowance" => *rpc.allowance.lock().unwrap() = U256::ZERO,
            "balance" => *rpc.cash_override.lock().unwrap() = Some(U256::ZERO),
            "paused" => *rpc.paused.lock().unwrap() = true,
            _ => unreachable!(),
        }
        rpc.calls.lock().unwrap().clear();
        assert_eq!(
            a.revalidate(&prepared).await.unwrap(),
            prepared,
            "{changed}"
        );
        assert!(rpc.calls.lock().unwrap().is_empty(), "{changed}");
    }
}

#[tokio::test]
async fn supply_preparation_rejects_reorg_after_observations() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Reorg {
        inner: Arc<Fixture>,
        headers: AtomicUsize,
    }
    #[async_trait]
    impl EvmRpc for Reorg {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let mut result = self.inner.request(method, params).await?;
            if method == "eth_getBlockByNumber" && self.headers.fetch_add(1, Ordering::SeqCst) >= 2
            {
                result["hash"] = json!(format!("0x{}", "ff".repeat(32)));
            }
            Ok(result)
        }
    }
    let rpc = Arc::new(Fixture::new());
    let mut a = adapter(rpc.clone());
    a.runtime = Arc::new(EvmRuntime::new(
        Arc::new(Reorg {
            inner: rpc,
            headers: AtomicUsize::new(0),
        }),
        a.config.chain_id,
        None,
    ));
    assert_eq!(
        a.prepare("aave.supply", &aave_input("aave.supply"))
            .await
            .unwrap_err()
            .code,
        "AAVE_READ_CHANGED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supply_preparation_http_rpc_measurement() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Counted {
        inner: gofin_evm::HttpRpc,
        logical: AtomicUsize,
    }
    #[async_trait]
    impl EvmRpc for Counted {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.logical.fetch_add(1, Ordering::SeqCst);
            self.inner.request(method, params).await
        }
    }
    let fixture = Arc::new(Fixture::new());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker_fixture = fixture.clone();
    let handle = tokio::runtime::Handle::current();
    let worker = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let (body_start, length) = loop {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while bytes.len() < body_start + length {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
            let result = handle
                .block_on(worker_fixture.request(
                    request["method"].as_str().unwrap(),
                    request["params"].clone(),
                ))
                .unwrap();
            let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let counted = Arc::new(Counted {
        inner: gofin_evm::HttpRpc::isolated_localhost(url.clone(), 1).unwrap(),
        logical: AtomicUsize::new(0),
    });
    let mut a = adapter(fixture.clone());
    a.runtime = Arc::new(EvmRuntime::new(counted.clone(), 1, None));
    let input = aave_input("aave.supply");
    let first = a.prepare("aave.supply", &input).await.unwrap();
    let cold = (
        counted.logical.swap(0, Ordering::SeqCst),
        fixture.calls.lock().unwrap().len(),
    );
    fixture.calls.lock().unwrap().clear();
    a.prepare("aave.supply", &input).await.unwrap();
    let warm = (
        counted.logical.swap(0, Ordering::SeqCst),
        fixture.calls.lock().unwrap().len(),
    );
    fixture.calls.lock().unwrap().clear();
    a.revalidate(&first).await.unwrap();
    let revalidate = (
        counted.logical.swap(0, Ordering::SeqCst),
        fixture.calls.lock().unwrap().len(),
    );
    let next = fixture.states.lock().unwrap().get(&20).unwrap().clone();
    fixture.states.lock().unwrap().insert(21, next);
    fixture.latest.store(21, Ordering::SeqCst);
    fixture.calls.lock().unwrap().clear();
    a.revalidate(&first).await.unwrap();
    let historical = (
        counted.logical.load(Ordering::SeqCst),
        fixture.calls.lock().unwrap().len(),
    );
    fixture.latest.store(20, Ordering::SeqCst);
    fixture.states.lock().unwrap().insert(
        19,
        AtBlock {
            income: ray(),
            debt_index: ray(),
            ..Default::default()
        },
    );
    for operation in ["aave.reserves", "aave.positions"] {
        for scoped in [true, false] {
            let counted = Arc::new(Counted {
                inner: gofin_evm::HttpRpc::isolated_localhost(url.clone(), 1).unwrap(),
                logical: AtomicUsize::new(0),
            });
            let mut a = adapter(fixture.clone());
            a.runtime = Arc::new(EvmRuntime::new(counted.clone(), 1, None));
            let mut input = json!({"wallet":"synthetic", "network":"evm", "chain_id":1});
            if scoped {
                input["tokens"] = json!([lower_address(addr(101))]);
            }
            let mut counts = Vec::new();
            for historical in [false, false, true] {
                if historical {
                    input["block_number"] = json!(19);
                }
                fixture.calls.lock().unwrap().clear();
                counted.logical.store(0, Ordering::SeqCst);
                a.read(operation, &input).await.unwrap();
                counts.push((
                    counted.logical.load(Ordering::SeqCst),
                    fixture.calls.lock().unwrap().len(),
                ));
            }
            assert_eq!(counts.len(), 3);
            assert!(
                counts
                    .iter()
                    .all(|(logical, physical)| *logical > 0 && *physical > 0)
            );
            eprintln!("{operation} scoped={scoped} HTTP fixture (logical, physical): {counts:?}");
        }
    }
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    assert!(
        [cold, warm]
            .iter()
            .all(|(logical, physical)| *logical > 0 && *physical > 0)
    );
    assert_eq!(revalidate, (0, 0), "postapproval compatibility has no RPC");
    assert_eq!(
        historical,
        (0, 0),
        "head movement does not trigger financial recollection"
    );
    eprintln!(
        "supply HTTP fixture (logical, physical): cold={cold:?}, warm={warm:?}, revalidate={revalidate:?}, historical={historical:?}"
    );
}

#[tokio::test]
async fn supply_capture_remains_pinned_and_only_new_preparation_reads_the_new_head() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct MovingHead {
        inner: Arc<Fixture>,
        moved: AtomicBool,
    }
    #[async_trait]
    impl EvmRpc for MovingHead {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_call" && !self.moved.swap(true, Ordering::SeqCst) {
                self.inner.latest.store(21, Ordering::SeqCst);
            }
            self.inner.request(method, params).await
        }
    }
    let rpc = Arc::new(Fixture::new());
    let next = rpc.states.lock().unwrap().get(&20).unwrap().clone();
    rpc.states.lock().unwrap().insert(21, next);
    let mut a = adapter(rpc.clone());
    a.runtime = Arc::new(EvmRuntime::new(
        Arc::new(MovingHead {
            inner: rpc.clone(),
            moved: AtomicBool::new(false),
        }),
        1,
        None,
    ));
    let first = a
        .prepare("aave.supply", &aave_input("aave.supply"))
        .await
        .unwrap();
    assert_eq!(
        first.binding["supply_anchor"]["evidence"]["block"],
        json!(20)
    );
    let config_selector = hex_bytes(&Pool::getReserveDataCall::SELECTOR);
    let scaled_selector = hex_bytes(&AToken::scaledBalanceOfCall::SELECTOR);
    {
        let calls = rpc.call_requests.lock().unwrap();
        for selector in [&config_selector, &scaled_selector] {
            let selected: Vec<_> = calls
                .iter()
                .filter(|params| {
                    params[0]["data"]
                        .as_str()
                        .is_some_and(|data| data.starts_with(selector))
                })
                .collect();
            assert!(!selected.is_empty());
            assert!(selected.iter().all(|params| params[1] == "0x14"));
        }
    }
    rpc.call_requests.lock().unwrap().clear();
    let refreshed = a.revalidate(&first).await.unwrap();
    // Original evidence remains historical without a postapproval financial read.
    assert_eq!(
        refreshed.binding["supply_anchor"]["evidence"]["block"],
        json!(20)
    );
    assert!(rpc.call_requests.lock().unwrap().is_empty());
    let next = a
        .prepare("aave.supply", &aave_input("aave.supply"))
        .await
        .unwrap();
    assert_eq!(
        next.binding["supply_anchor"]["evidence"]["block"],
        json!(21)
    );
    let calls = rpc.call_requests.lock().unwrap();
    let selector = hex_bytes(&Pool::getReserveDataCall::SELECTOR);
    assert!(calls.iter().any(|params| {
        params[0]["data"]
            .as_str()
            .is_some_and(|data| data.starts_with(&selector))
            && params[1] == "0x15"
    }));
}

#[tokio::test]
async fn borrow_rejects_invalid_bitmap_reserve_identity_and_flag_mismatch() {
    for case in ["missing", "duplicate", "flag_mismatch"] {
        let rpc = Arc::new(Fixture::new());
        {
            let mut states = rpc.states.lock().unwrap();
            let state = states.get_mut(&20).unwrap();
            state.scaled_supply = U256::from(10_000_000);
            state.collateral = true;
        }
        match case {
            "missing" => {
                rpc.call_overrides.lock().unwrap().insert(
                    Pool::getReserveAddressByIdCall::SELECTOR,
                    abi_value(Address::ZERO),
                );
            }
            "duplicate" => {
                rpc.call_overrides.lock().unwrap().insert(
                    Pool::getReserveAddressByIdCall::SELECTOR,
                    abi_value(addr(101)),
                );
            }
            "flag_mismatch" => {
                rpc.call_overrides.lock().unwrap().insert(
                    Pool::getUserConfigurationCall::SELECTOR,
                    abi_value(U256::ZERO),
                );
            }
            _ => unreachable!(),
        }
        let a = adapter(rpc);
        assert_eq!(
            a.prepare("aave.borrow", &aave_input("aave.borrow"))
                .await
                .unwrap_err()
                .code,
            "AAVE_USER_CONFIGURATION_INVALID",
            "{case}"
        );
    }
}

#[tokio::test]
async fn borrow_execution_does_not_recheck_collateral_and_debt() {
    for case in ["collateral", "debt"] {
        let rpc = Arc::new(Fixture::new());
        {
            let mut states = rpc.states.lock().unwrap();
            let state = states.get_mut(&20).unwrap();
            state.scaled_supply = U256::from(10_000_000);
            state.collateral = true;
        }
        let a = adapter(rpc.clone());
        let prepared = a
            .prepare("aave.borrow", &aave_input("aave.borrow"))
            .await
            .unwrap();
        {
            let mut states = rpc.states.lock().unwrap();
            let state = states.get_mut(&20).unwrap();
            if case == "collateral" {
                state.collateral = false;
            } else {
                state.scaled_debt = U256::from(1_000_000);
            }
        }
        rpc.calls.lock().unwrap().clear();
        assert_eq!(a.revalidate(&prepared).await.unwrap(), prepared, "{case}");
        assert!(rpc.calls.lock().unwrap().is_empty(), "{case}");
    }
}

#[tokio::test]
async fn borrow_rejects_reorg_and_proxy_upgrade_during_capture() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Changing {
        inner: Arc<Fixture>,
        headers: AtomicUsize,
        proxy: bool,
    }
    #[async_trait]
    impl EvmRpc for Changing {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let pinned_storage = method == "eth_getStorageAt" && params[2] == "0x14";
            let mut result = self.inner.request(method, params).await?;
            if self.proxy && pinned_storage {
                self.inner.aave_implementation.store(302, Ordering::SeqCst);
            }
            if !self.proxy
                && method == "eth_getBlockByNumber"
                && self.headers.fetch_add(1, Ordering::SeqCst) >= 2
            {
                result["hash"] = json!(format!("0x{}", "ff".repeat(32)));
            }
            Ok(result)
        }
    }
    for proxy in [false, true] {
        let rpc = Arc::new(Fixture::new());
        {
            let mut states = rpc.states.lock().unwrap();
            let state = states.get_mut(&20).unwrap();
            state.scaled_supply = U256::from(10_000_000);
            state.collateral = true;
        }
        let mut a = adapter(rpc.clone());
        a.runtime = Arc::new(EvmRuntime::new(
            Arc::new(Changing {
                inner: rpc,
                headers: AtomicUsize::new(0),
                proxy,
            }),
            1,
            None,
        ));
        let error = a
            .prepare("aave.borrow", &aave_input("aave.borrow"))
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if proxy {
                "AAVE_IMPLEMENTATION_CHANGED"
            } else {
                "AAVE_READ_CHANGED"
            }
        );
    }
}

#[tokio::test]
async fn borrow_rejects_flagged_reserve_id_mismatch() {
    struct BadId(Arc<Fixture>);
    #[async_trait]
    impl EvmRpc for BadId {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let data = if method == "eth_call" {
                gofin_evm::rpc_bytes(&params[0]["data"]).unwrap_or_default()
            } else {
                Default::default()
            };
            let bad = data.len() >= 4
                && data[..4] == Pool::getReserveDataCall::SELECTOR
                && Pool::getReserveDataCall::abi_decode_validate(&data)
                    .unwrap()
                    .asset
                    == addr(102);
            let value = self.0.request(method, params).await?;
            if bad {
                let mut bytes = gofin_evm::rpc_bytes(&value)?.to_vec();
                bytes[7 * 32 + 31] = 7;
                Ok(json!(hex_bytes(&bytes)))
            } else {
                Ok(value)
            }
        }
    }
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let state = states.get_mut(&20).unwrap();
        state.scaled_supply = U256::from(10_000_000);
        state.collateral = true;
    }
    let mut a = adapter(rpc.clone());
    a.runtime = Arc::new(EvmRuntime::new(Arc::new(BadId(rpc)), 1, None));
    assert_eq!(
        a.prepare("aave.borrow", &aave_input("aave.borrow"))
            .await
            .unwrap_err()
            .code,
        "AAVE_USER_CONFIGURATION_INVALID"
    );
}

#[tokio::test]
async fn pure_aave_write_decisions_enforce_current_bounds_without_rpc() {
    use crate::aave::write_lifecycle::*;
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let reserve = a
        .reserve(&lower_address(addr(101)), Some(20))
        .await
        .unwrap();
    let user = a
        .user_reserve(&reserve.token, addr(1), Some(20))
        .await
        .unwrap();
    let before = rpc.calls.lock().unwrap().len();
    assert_eq!(
        evaluate_supply(&SupplyEvidence {
            reserve: reserve.clone(),
            amount: U256::from(10),
            allowance: U256::from(9)
        })
        .unwrap_err()
        .code,
        "ALLOWANCE_INSUFFICIENT"
    );
    assert_eq!(
        evaluate_repay(&RepayEvidence {
            debt: U256::from(11),
            allowance: U256::from(10),
            amount: U256::from(10),
            all: true
        })
        .unwrap_err()
        .code,
        "ALLOWANCE_INSUFFICIENT"
    );
    assert_eq!(
        evaluate_withdraw(&WithdrawEvidence {
            claim: U256::from(9),
            liquidity: U256::from(20),
            amount: U256::from(10),
            all: false,
            approved_minimum: None
        })
        .unwrap_err()
        .code,
        "WITHDRAW_LIMIT"
    );
    assert_eq!(
        evaluate_collateral(&CollateralEvidence {
            reserve: reserve.clone(),
            user: user.clone(),
            enabled: true
        })
        .unwrap_err()
        .code,
        "NO_SUPPLIED_POSITION"
    );
    let mut request = request::parse("aave.borrow", &aave_input("aave.borrow")).unwrap();
    request.max_debt_usd = Some("0.00000001".into());
    assert_eq!(
        evaluate_borrow(&BorrowEvidence {
            reserve: reserve.clone(),
            amount: U256::from(10),
            collateral_base: U256::from(100),
            debt: U256::from(2),
            projected_health: U256::MAX,
            previous_debt: U256::ZERO,
            available_borrow: U256::MAX,
            request: request.clone()
        })
        .unwrap_err()
        .code,
        "AAVE_DEBT_LIMIT"
    );
    assert_eq!(rpc.calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn typed_aave_phases_match_adapter_preview_for_all_actions() {
    use crate::aave::write_lifecycle::*;
    for op in [
        "aave.supply",
        "aave.borrow",
        "aave.repay",
        "aave.withdraw",
        "aave.collateral",
    ] {
        let rpc = Arc::new(Fixture::new());
        {
            let mut states = rpc.states.lock().unwrap();
            let state = states.get_mut(&20).unwrap();
            state.scaled_supply = U256::from(10_000_000);
            state.scaled_debt = if op == "aave.repay" {
                U256::from(2_000_000)
            } else {
                U256::ZERO
            };
            state.collateral = true;
        }
        let a = adapter(rpc.clone());
        let input = aave_input(op);
        let context = PreparationContext::default();
        let typed_input = WriteInput {
            operation: op.into(),
            request: request::parse(op, &input).unwrap(),
            input: input.clone(),
            context: context.clone(),
        };
        let legacy = a
            .prepare_operation(op, &input, None, &context)
            .await
            .unwrap();
        let evidence = a.collect_aave_write(&typed_input, None).await.unwrap();
        let calls = rpc.calls.lock().unwrap().len();
        let candidate = evaluate_write(&typed_input, &evidence).unwrap();
        assert_eq!(
            rpc.calls.lock().unwrap().len(),
            calls,
            "pure candidate {op}"
        );
        let envelope = a.prepare_aave_envelope(&candidate).await.unwrap();
        let final_evidence = a
            .simulate_aave_envelope(&candidate, &envelope)
            .await
            .unwrap();
        let calls = rpc.calls.lock().unwrap().len();
        let prepared = finalize_write(&candidate, &final_evidence).unwrap();
        assert_eq!(
            rpc.calls.lock().unwrap().len(),
            calls,
            "pure finalization {op}"
        );
        fn strip_timestamps(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove("observed_at");
                    for value in map.values_mut() {
                        strip_timestamps(value);
                    }
                }
                Value::Array(items) => {
                    for value in items {
                        strip_timestamps(value);
                    }
                }
                _ => {}
            }
        }
        let mut actual_binding = prepared.binding.clone();
        let mut legacy_binding = legacy.binding.clone();
        strip_timestamps(&mut actual_binding);
        strip_timestamps(&mut legacy_binding);
        assert_eq!(actual_binding, legacy_binding, "binding {op}");
        assert_eq!(prepared.intent, legacy.intent, "intent {op}");
        assert_eq!(prepared.summary, legacy.summary, "summary {op}");
    }
}

#[tokio::test]
async fn aave_lifecycle_prepares_then_simulates_and_does_not_recollect_after_approval() {
    use gofin_domain::operation_lifecycle::CandidateResult;
    let rpc = Arc::new(Fixture::new());
    let a = adapter(rpc.clone());
    let input = aave_input("aave.supply");
    let context = PreparationContext::default();
    let mut session = a
        .operation_lifecycle("aave.supply", &input, Some(&context))
        .unwrap()
        .unwrap();
    session.collect().await.unwrap();
    let calls = rpc.calls.lock().unwrap().len();
    session.evaluate().unwrap();
    assert_eq!(
        rpc.calls.lock().unwrap().len(),
        calls,
        "evaluation must be pure"
    );
    session.prepare().await.unwrap();
    let calls = rpc.calls.lock().unwrap().len();
    let mut prepared = match session.simulate().await.unwrap() {
        CandidateResult::Write(prepared) => *prepared,
        CandidateResult::Read(_) => panic!("Aave write returned read output"),
    };
    assert!(
        rpc.calls.lock().unwrap().len() > calls,
        "simulation must perform I/O"
    );
    prepared.action_deadline = Some(gofin_domain::plan_timing::ResolvedActionDeadline {
        field: "deadline".into(),
        window_seconds: 60,
        resolved_at: "2026-01-01T00:00:00Z".into(),
        deadline_unix_seconds: 2_000_000_000,
    });
    rpc.calls.lock().unwrap().clear();
    let retained = a.revalidate(&prepared).await.unwrap();
    assert_eq!(
        retained, prepared,
        "local compatibility check preserves all approved data"
    );
    assert!(
        rpc.calls.lock().unwrap().is_empty(),
        "no postapproval financial recollection"
    );
    let mut obsolete = prepared;
    obsolete.operation_version = 1;
    assert_eq!(
        a.revalidate(&obsolete).await.unwrap_err().code,
        "AAVE_APPROVAL_OBSOLETE"
    );
    assert!(rpc.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reopened_zero_lineages_share_historical_header_bracket() {
    struct HeaderProbe {
        inner: Arc<Fixture>,
        headers: std::sync::Mutex<Vec<Value>>,
    }
    #[async_trait]
    impl EvmRpc for HeaderProbe {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_getBlockByNumber" {
                self.headers.lock().unwrap().push(params.clone());
            }
            self.inner.request(method, params).await
        }
    }
    let rpc = Arc::new(Fixture::new());
    let mut a = adapter(rpc.clone());
    let token = a.token(&lower_address(addr(101)), Some(20)).await.unwrap();
    let supply = a
        .snapshot_anchor(&token, addr(1), addr(110), PositionKind::Supply, 20)
        .await
        .unwrap();
    let debt = a
        .snapshot_anchor(&token, addr(1), addr(120), PositionKind::VariableDebt, 20)
        .await
        .unwrap();
    let next = rpc.states.lock().unwrap().get(&20).unwrap().clone();
    rpc.states.lock().unwrap().insert(21, next);
    rpc.latest.store(21, std::sync::atomic::Ordering::SeqCst);
    let probe = Arc::new(HeaderProbe {
        inner: rpc,
        headers: std::sync::Mutex::new(Vec::new()),
    });
    a.runtime = Arc::new(EvmRuntime::new(probe.clone(), 1, None));
    let context = PreparationContext {
        logical_operation_id: None,
        positions: vec![supply, debt],
    };
    a.prepare_with_context("aave.supply", &aave_input("aave.supply"), &context)
        .await
        .unwrap();
    let headers = probe.headers.lock().unwrap();
    assert_eq!(
        headers.iter().filter(|params| params[0] == "0x14").count(),
        2,
        "one independent before/after bracket for both old anchors"
    );
    assert!(headers.iter().any(|params| params[0] == "latest"));
    assert!(
        headers.iter().any(|params| params[0] == "0x15"),
        "new capture still has a numeric canonical recheck"
    );
}

struct BadAggregate {
    inner: Arc<crate::tests::Fixture>,
    truncated: bool,
}
#[async_trait::async_trait]
impl EvmRpc for BadAggregate {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let mut result = self.inner.request(method, params.clone()).await?;
        if method == "eth_call"
            && params[0]["to"] == json!(lower_address(crate::tests::addr(210)))
            && params[0]["data"].as_str().is_some_and(|data| {
                data.starts_with(&gofin_evm::hex_bytes(
                    &DataProvider::getReserveDataCall::SELECTOR,
                ))
            })
        {
            let mut raw = gofin_evm::rpc_bytes(&result)?.to_vec();
            if self.truncated {
                raw.truncate(raw.len() - 32);
            } else {
                raw[10 * 32 + 31] ^= 1;
            }
            result = json!(gofin_evm::hex_bytes(&raw));
        }
        Ok(result)
    }
}
#[tokio::test]
async fn malformed_or_uncorrelated_aggregate_refuses_before_transaction_preparation() {
    for truncated in [true, false] {
        let fixture = Arc::new(crate::tests::Fixture::new());
        let mut adapter = crate::tests::adapter(fixture.clone());
        adapter.runtime = Arc::new(gofin_evm::EvmRuntime::new(
            Arc::new(BadAggregate {
                inner: fixture.clone(),
                truncated,
            }),
            1,
            None,
        ));
        let error = adapter
            .prepare("aave.supply", &crate::tests::aave_input("aave.supply"))
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if truncated {
                "CONTRACT_DATA_INVALID"
            } else {
                "AAVE_READ_CHANGED"
            }
        );
        assert!(!fixture.calls.lock().unwrap().iter().any(|method| matches!(
            method.as_str(),
            "eth_estimateGas" | "eth_sendRawTransaction"
        )));
    }
}
#[tokio::test]
async fn scoped_aggregates_replace_overlapping_configuration_and_total_supply_views() {
    let fixture = Arc::new(crate::tests::Fixture::new());
    let adapter = crate::tests::adapter(fixture.clone());
    adapter
        .prepare("aave.supply", &crate::tests::aave_input("aave.supply"))
        .await
        .unwrap();
    let calls = fixture.call_requests.lock().unwrap();
    for selector in [
        Pool::getConfigurationCall::SELECTOR,
        DataProvider::getReserveConfigurationDataCall::SELECTOR,
        DataProvider::getReserveTokensAddressesCall::SELECTOR,
        DataProvider::getReserveCapsCall::SELECTOR,
        DataProvider::getPausedCall::SELECTOR,
        Erc20::totalSupplyCall::SELECTOR,
    ] {
        let prefix = gofin_evm::hex_bytes(&selector);
        assert!(
            !calls.iter().any(|params| params[0]["data"]
                .as_str()
                .is_some_and(|data| data.starts_with(&prefix))),
            "overlapping view remained: {prefix}"
        );
    }
}

struct MetadataMcp {
    decimals: u8,
    calls: AtomicUsize,
    fallback: SyntheticAaveMcp,
}

#[async_trait]
impl aave_mcp::AaveMcpTransport for MetadataMcp {
    async fn post(
        &self,
        request: aave_mcp::McpHttpRequest,
    ) -> std::result::Result<aave_mcp::McpHttpResponse, aave_mcp::AaveMcpError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let result = match body["method"].as_str() {
            Some("tools/list") => json!({"tools":[{
                "name":"get_reserve_details",
                "inputSchema":{"type":"object","properties":{"version":{"type":"string"},"chainId":{"type":"integer"},"market":{"type":"string"},"token":{"type":"string"}},"required":["version","chainId","market","token"],"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false,"destructiveHint":false}
            }]}),
            Some("tools/call") if body["params"]["name"] == "get_reserve_details" => {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let args = &body["params"]["arguments"];
                json!({"structuredContent":{"data":{"chainId":args["chainId"],"market":args["market"],"token":args["token"],"symbol":"MCPUSD","decimals":self.decimals}},"content":[]})
            }
            _ => return self.fallback.post(request).await,
        };
        Ok(aave_mcp::McpHttpResponse {
            status: 200,
            headers: std::collections::BTreeMap::from([(
                "content-type".into(),
                "application/json".into(),
            )]),
            body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
                .unwrap(),
        })
    }
}

fn metadata_fixture(decimals: u8) -> (Arc<Fixture>, EthereumAdapter, Arc<MetadataMcp>) {
    let fixture = Arc::new(Fixture::new());
    let transport = Arc::new(MetadataMcp {
        decimals,
        calls: AtomicUsize::new(0),
        fallback: SyntheticAaveMcp {
            calls: Arc::new(AtomicUsize::new(0)),
        },
    });
    let adapter = adapter(fixture.clone())
        .with_aave_mcp(aave_mcp::AaveMcpClient::with_transport(transport.clone()));
    (fixture, adapter, transport)
}

fn metadata_rpc_count(fixture: &Fixture, selector: [u8; 4]) -> usize {
    let prefix = gofin_evm::hex_bytes(&selector);
    fixture
        .call_requests
        .lock()
        .unwrap()
        .iter()
        .filter(|params| {
            params[0]["data"]
                .as_str()
                .is_some_and(|data| data.starts_with(&prefix))
        })
        .count()
}

#[tokio::test]
async fn scoped_mcp_metadata_is_verified_and_reused() {
    let (fixture, adapter, mcp) = metadata_fixture(6);
    let input = json!({"wallet":"synthetic","network":"evm","chain_id":1,"tokens":[lower_address(addr(101))]});
    for pass in 0..2 {
        let result = adapter.read("aave.reserves", &input).await.unwrap();
        assert_eq!(result["data"][0]["token"]["symbol"], "MCPUSD");
        assert_eq!(mcp.calls.load(Ordering::SeqCst), 1, "pass {pass}");
        assert_eq!(metadata_rpc_count(&fixture, Erc20::symbolCall::SELECTOR), 0);
        assert_eq!(
            metadata_rpc_count(&fixture, Erc20::decimalsCall::SELECTOR),
            0
        );
    }
}

#[tokio::test]
async fn mismatched_mcp_metadata_falls_back_to_canonical_rpc() {
    let (fixture, adapter, mcp) = metadata_fixture(18);
    let input = json!({"wallet":"synthetic","network":"evm","chain_id":1,"tokens":[lower_address(addr(101))]});
    let result = adapter.read("aave.reserves", &input).await.unwrap();
    assert_ne!(result["data"][0]["token"]["symbol"], "MCPUSD");
    assert_eq!(result["data"][0]["token"]["decimals"], 6);
    assert_eq!(mcp.calls.load(Ordering::SeqCst), 1);
    assert_eq!(metadata_rpc_count(&fixture, Erc20::symbolCall::SELECTOR), 1);
    assert_eq!(
        metadata_rpc_count(&fixture, Erc20::decimalsCall::SELECTOR),
        1
    );
}

#[tokio::test]
async fn historical_and_unscoped_reserves_do_not_fan_out_metadata_mcp() {
    let (fixture, adapter, mcp) = metadata_fixture(6);
    fixture.states.lock().unwrap().insert(
        19,
        AtBlock {
            income: ray(),
            debt_index: ray(),
            ..Default::default()
        },
    );
    let historical = json!({"wallet":"synthetic","network":"evm","chain_id":1,"tokens":[lower_address(addr(101))],"block_number":19});
    let result = adapter.read("aave.reserves", &historical).await.unwrap();
    assert_eq!(result["block"], 19);
    assert_eq!(mcp.calls.load(Ordering::SeqCst), 0);
    assert_eq!(metadata_rpc_count(&fixture, Erc20::symbolCall::SELECTOR), 1);
    assert_eq!(
        metadata_rpc_count(&fixture, Erc20::decimalsCall::SELECTOR),
        1
    );
    let unscoped = json!({"wallet":"synthetic","network":"evm","chain_id":1});
    adapter.read("aave.reserves", &unscoped).await.unwrap();
    assert_eq!(mcp.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn aave_v2_approval_allows_only_execution_nonce_to_change() {
    let rpc = Arc::new(Fixture::new());
    {
        let mut states = rpc.states.lock().unwrap();
        let state = states.get_mut(&20).unwrap();
        state.scaled_supply = U256::from(10_000_000);
        state.collateral = true;
    }
    let a = adapter(rpc.clone());
    let prepared = a
        .prepare("aave.borrow", &aave_input("aave.borrow"))
        .await
        .unwrap();
    rpc.calls.lock().unwrap().clear();
    let next = a.apply_claimed_evm_nonce(&prepared, 42).unwrap();
    a.validate_approved_candidate(&prepared, &next).unwrap();
    assert!(rpc.calls.lock().unwrap().is_empty());
    for pointer in [
        "/transaction/value",
        "/transaction/gas",
        "/transaction/to",
        "/transaction/max_fee_per_gas",
        "/pool",
        "/units",
        "/recipient",
        "/token/reference",
    ] {
        let mut altered = next.clone();
        *altered.binding.pointer_mut(pointer).unwrap() = json!("changed");
        assert_eq!(
            a.validate_approved_candidate(&prepared, &altered)
                .unwrap_err()
                .code,
            "CANDIDATE_OUTSIDE_APPROVAL",
            "{pointer}"
        );
    }
    let mut altered = next.clone();
    altered.input["min_health_factor"] = json!("1.1");
    assert_eq!(
        a.validate_approved_candidate(&prepared, &altered)
            .unwrap_err()
            .code,
        "CANDIDATE_OUTSIDE_APPROVAL"
    );
    let mut legacy = prepared;
    legacy.operation_version = 1;
    let legacy_next = a.apply_claimed_evm_nonce(&legacy, 42).unwrap();
    assert_eq!(
        a.validate_approved_candidate(&legacy, &legacy_next)
            .unwrap_err()
            .code,
        "CANDIDATE_OUTSIDE_APPROVAL"
    );
}
