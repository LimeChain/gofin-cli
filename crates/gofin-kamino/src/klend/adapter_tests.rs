//! Controlled DTO tests; native decoding and owned-validator coverage are separate.
use super::*;
use gofin_pricing::UsdQuote;
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime, rpc::SolanaRpc};

fn key(byte: u8) -> String {
    bs58::encode([byte; 32]).into_string()
}
fn token(byte: u8) -> TokenInfo {
    TokenInfo {
        mint: key(byte),
        decimals: 6,
        token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
    }
}
fn sf(n: u64) -> String {
    (BigUint::from(n) << 60_usize).to_string()
}
fn risk() -> LendingRisk {
    LendingRisk {
        deposited_value_sf: sf(1000),
        borrowed_assets_market_value_sf: sf(100),
        borrow_factor_adjusted_debt_value_sf: sf(100),
        allowed_borrow_value_sf: sf(700),
        unhealthy_borrow_value_sf: sf(800),
        refreshed: true,
        protocol_state: json!({}),
    }
}
fn limits() -> RiskLimits {
    RiskLimits {
        minimum_health_factor_bps: 12000,
        maximum_debt_usd_micros: "200000000".into(),
        maximum_debt_to_collateral_bps: 2000,
    }
}
fn reserve() -> ReserveInfo {
    ReserveInfo {
        address: key(2),
        market: key(3),
        program: klend_interface::KLEND_PROGRAM_ID.to_string(),
        token: token(4),
        collateral_token: token(5),
        available_liquidity_atoms: "10000000".into(),
        supply_available: true,
        withdraw_available: true,
        borrow_available: true,
        repay_available: true,
        restrictions: vec![],
        observed_slot: 10,
        observed_at: Utc::now().to_rfc3339(),
        metrics: json!({}),
        protocol_state: json!({"authority_terms":{"borrow_fee_sf":"1","farm_debt":key(8)}}),
    }
}
fn supply() -> LendingPosition {
    LendingPosition {
        reserve: key(2),
        side: LendingSide::Supply,
        token: token(4),
        collateral_token: Some(token(5)),
        native_units: "200".into(),
        underlying_atoms: Some("300".into()),
        cumulative_borrow_rate_bsf: None,
        protocol_state: json!({}),
    }
}
fn debt(n: u64) -> LendingPosition {
    LendingPosition {
        reserve: key(2),
        side: LendingSide::Debt,
        token: token(4),
        collateral_token: None,
        native_units: sf(n),
        underlying_atoms: Some(n.to_string()),
        cumulative_borrow_rate_bsf: Some(sf(1)),
        protocol_state: json!({"debt_kind":"floating"}),
    }
}
fn obligation() -> ObligationInfo {
    ObligationInfo {
        address: key(6),
        market: key(3),
        program: klend_interface::KLEND_PROGRAM_ID.to_string(),
        owner: key(1),
        exists: true,
        tag: 0,
        elevation_group: 0,
        referrer: None,
        positions: vec![supply()],
        risk: risk(),
        writes_available: true,
        restrictions: vec![],
        observed_slot: 10,
        observed_at: Utc::now().to_rfc3339(),
        complete: true,
        warnings: vec![],
        protocol_state: json!({}),
    }
}
fn intent() -> LendingIntent {
    LendingIntent {
        wallet: "wallet".into(),
        action: LendingAction::Borrow,
        market: key(3),
        reserve: key(2),
        obligation: key(6),
        token: token(4),
        amount_atoms: "100".into(),
        all: false,
        minimum_receive_atoms: None,
        maximum_debt_increase_atoms: Some("101".into()),
        maximum_protocol_fee_atoms: "1".into(),
        risk_limits: Some(limits()),
        maximum_network_fee_lamports: 5000,
        maximum_account_funding_lamports: 0,
    }
}
fn candidate() -> LendingCandidate {
    let mut post = obligation();
    post.observed_slot = 11;
    post.positions.push(debt(101));
    LendingCandidate {
        reserve: reserve(),
        obligation_before: obligation(),
        quote: LendingQuote {
            debits: vec![],
            credits: vec![TokenAmount {
                token: token(4),
                atoms: Some("100".into()),
            }],
            protocol_fees: vec![TokenAmount {
                token: token(4),
                atoms: Some("1".into()),
            }],
            projected_obligation: post,
            rent_lamports: 0,
            network_fee_lamports: 5000,
            expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            warnings: vec![],
            details: json!({}),
        },
        instructions: vec![NormalizedInstruction {
            program_id: klend_interface::KLEND_PROGRAM_ID.to_string(),
            accounts: vec![],
            data_base64: "AA==".into(),
        }],
        lookup_tables: vec![],
        blockhash: key(9),
        last_valid_block_height: 1000,
        metadata: json!({"rent_accounts":[],"possible_closed_accounts":[]}),
    }
}
fn prepared(i: &LendingIntent, c: &LendingCandidate) -> PreparedOperation {
    PreparedOperation {
        operation: OP_BORROW.into(),
        input: json!({}),
        binding: json!({"intent":i,"wallet_address":key(1),"protocol_candidate":c,"prior_anchors":[],"message_sha256":"digest",BASELINE_ANCHORS_KEY:baselines(c,i.action).unwrap()}),
        summary: String::new(),
        checks: vec![],
        warnings: vec![],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: c.quote.expires_at.clone(),
        valuation: None,
        intent: FinancialIntent::default(),
        operation_version: 1,
        action_deadline: None,
    }
}
fn bounds() -> Value {
    json!({"minimum_amount_atoms":"1","maximum_amount_atoms":"200","maximum_debt_increase_atoms":"202","maximum_cumulative_borrow_atoms":"500","maximum_protocol_fee_atoms":"2","maximum_network_fee_lamports":5000,"maximum_account_funding_lamports":0})
}
struct Prices(bool);
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
        if !self.0 {
            return Err(invalid("No test price"));
        }
        Ok(UsdQuote {
            usd_per_unit: "1.25".into(),
            updated_at: Utc::now().to_rfc3339(),
            observed_at: Utc::now().to_rfc3339(),
            source: "synthetic".into(),
        })
    }
}
struct NoRpc;
#[async_trait]
impl SolanaRpc for NoRpc {
    async fn request(&self, _: &str, _: Value) -> Result<Value> {
        Err(invalid("No network in DTO tests"))
    }
}
struct Protocol {
    absent: bool,
}
#[async_trait]
impl KlendProtocol for Protocol {
    fn default_obligation_address(&self, owner: &str, market: &str) -> Result<String> {
        assert_eq!(owner, key(1));
        assert_eq!(market, key(3));
        Ok(key(6))
    }
    async fn markets(&self, _: &SolanaRuntime, _: &LendingQuery) -> Result<MarketPage> {
        unreachable!()
    }
    async fn market(&self, _: &SolanaRuntime, _: &str) -> Result<MarketInfo> {
        unreachable!()
    }
    async fn reserves(&self, _: &SolanaRuntime, _: &str, _: &LendingQuery) -> Result<ReservePage> {
        unreachable!()
    }
    async fn reserve(&self, _: &SolanaRuntime, _: &str, _: &str) -> Result<ReserveInfo> {
        Ok(reserve())
    }
    async fn obligations(
        &self,
        _: &SolanaRuntime,
        _: &str,
        _: Option<&str>,
        _: &LendingQuery,
    ) -> Result<ObligationPage> {
        unreachable!()
    }
    async fn obligation(
        &self,
        _: &SolanaRuntime,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<ObligationInfo> {
        let mut value = obligation();
        if self.absent {
            value.exists = false;
            value.positions.clear();
            value.protocol_state = json!({"absence_verified":true});
        }
        Ok(value)
    }
    async fn build_candidate(
        &self,
        _: &SolanaRuntime,
        _: &LendingIntent,
        _: &str,
    ) -> Result<LendingCandidate> {
        unreachable!()
    }
    async fn validate_candidate(
        &self,
        _: &SolanaRuntime,
        _: &LendingIntent,
        _: &str,
        _: &LendingCandidate,
    ) -> Result<()> {
        unreachable!()
    }
    async fn verify_finalized(
        &self,
        _: &SolanaRuntime,
        _: &LendingIntent,
        _: &str,
        _: &LendingCandidate,
        _: &FinalizedReceiptProof,
    ) -> Result<VerifiedLendingEffects> {
        unreachable!()
    }
}
fn adapter() -> KlendAdapter {
    adapter_with_absence(false)
}
fn adapter_with_absence(absent: bool) -> KlendAdapter {
    let prices = Arc::new(Prices(true));
    let wallet = Arc::new(
        SolanaWalletAdapter::new("wallet", key(1), None, Arc::new(NoRpc), prices.clone()).unwrap(),
    );
    KlendAdapter::new(
        BTreeMap::from([("wallet".into(), wallet)]),
        Arc::new(Protocol { absent }),
        prices,
    )
    .unwrap()
}
fn input() -> Value {
    json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","market":key(3),"reserve":key(2),"obligation":key(6),"token_mint":key(4),"amount_atoms":"100","maximum_protocol_fee_atoms":"0","maximum_network_fee_lamports":5000,"maximum_account_funding_lamports":0})
}

#[tokio::test]
async fn public_mainnet_identity_and_exact_atomic_quantities_match_schema() {
    let a = adapter();
    let valid = input();
    let (i, _, _) = a.resolve(OP_SUPPLY, &valid, true).await.unwrap();
    assert_eq!(i.amount_atoms, "100");
    for quantity in ["0", "01", "1.1", "-1", "1e3", "18446744073709551615", "all"] {
        let mut bad = valid.clone();
        bad["amount_atoms"] = json!(quantity);
        assert!(
            a.resolve(OP_SUPPLY, &bad, true).await.is_err(),
            "{quantity}"
        );
    }
    for (key, value) in [
        ("network", "solana:mainnet-beta"),
        ("network", "ethereum"),
        ("cluster", "devnet"),
    ] {
        let mut bad = valid.clone();
        bad[key] = json!(value);
        assert!(a.resolve(OP_SUPPLY, &bad, true).await.is_err());
    }
}
#[tokio::test]
async fn repay_all_requires_finite_cap_and_withdraw_all_freezes_only_collateral() {
    let a = adapter();
    let mut raw = input();
    raw["amount_atoms"] = json!("all");
    assert!(a.resolve(OP_REPAY, &raw, true).await.is_err());
    raw["maximum_repay_atoms"] = json!("123");
    let (i, _, _) = a.resolve(OP_REPAY, &raw, true).await.unwrap();
    assert_eq!(i.amount_atoms, "123");
    assert!(i.all);
    raw["maximum_repay_atoms"] = json!("18446744073709551615");
    assert!(a.resolve(OP_REPAY, &raw, true).await.is_err());
    raw = input();
    raw.as_object_mut().unwrap().remove("amount_atoms");
    raw["collateral_atoms"] = json!("all");
    raw["minimum_receive_atoms"] = json!("1");
    raw["risk_limits"] = json!(limits());
    let (i, _, _) = a.resolve(OP_WITHDRAW, &raw, true).await.unwrap();
    assert_eq!(i.amount_atoms, "200");
    assert!(i.all);
    raw["amount_atoms"] = json!("1");
    assert!(a.resolve(OP_WITHDRAW, &raw, true).await.is_err());
}
#[test]
fn borrowed_principal_and_origination_fee_consume_gross_cumulative_authority() {
    let i = intent();
    let c = candidate();
    let p =
        boundaries::permission(&i, &key(1), &c.reserve, &c.obligation_before, &bounds()).unwrap();
    let scope = p.cumulative_scope.as_ref().unwrap();
    assert_eq!(scope.maximum_obligations[0].amount.atoms.as_str(), "500");
    assert_eq!(p.possible_credits[0].asset, asset(&i.token));
    assert!(p.spec.maximum_costs.iter().any(|cost| matches!(
        cost.location,
        AccountingLocation::Position {
            position_kind: PositionKind::VariableDebt,
            ..
        }
    ) && cost.treatment
        == CostTreatment::IncludedInDebit
        && cost.amount.atoms.as_str() == "2"));
    assert!(boundaries::prepare(&prepared(&i, &c), &bounds()).is_err());
    for name in [
        "maximum_cumulative_borrow_atoms",
        "maximum_debt_increase_atoms",
    ] {
        let mut b = bounds();
        b.as_object_mut().unwrap().remove(name);
        assert!(boundaries::permission(&i, &key(1), &c.reserve, &c.obligation_before, &b).is_err());
    }
    let mut b = bounds();
    b["maximum_cumulative_borrow_atoms"] = json!("201");
    assert!(boundaries::permission(&i, &key(1), &c.reserve, &c.obligation_before, &b).is_err());
    assert_eq!(
        operation_definitions()
            .into_iter()
            .find(|d| d.name == OP_BORROW)
            .unwrap()
            .semantics["plan_permission"]["requires_adapter_scope"],
        true
    );
}

#[tokio::test]
async fn collateral_withdrawal_receipt_plan_preserves_existing_permission_after_reload() {
    struct Approval;
    #[async_trait]
    impl Authorizer for Approval {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                source: Some(AuthorizationSource::Development),
                yolo_provenance: None,
                mechanism: "synthetic".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: Utc::now().to_rfc3339(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }
    let adapter = Arc::new(adapter());
    let mut raw = input();
    raw.as_object_mut().unwrap().remove("amount_atoms");
    raw["collateral_atoms"] = json!("all");
    raw["minimum_receive_atoms"] = json!("300");
    raw["risk_limits"] = json!(limits());
    let mut b = bounds();
    b.as_object_mut()
        .unwrap()
        .remove("maximum_debt_increase_atoms");
    b.as_object_mut()
        .unwrap()
        .remove("maximum_cumulative_borrow_atoms");
    let permission = adapter
        .prepare_plan_permission(OP_WITHDRAW, &raw, &b)
        .await
        .unwrap();
    let target = ResourceAmount {
        resource: resource(&key(1)),
        amount: Amount {
            asset: asset(&token(4)),
            atoms: Quantity::new("300").unwrap(),
        },
    };
    assert!(adapter.plan_has_finite_non_cash_source(OP_WITHDRAW, &permission, &target));
    assert!(permission.deferred_receipt.is_none());
    assert!(!adapter.plan_has_finite_non_cash_source(OP_BORROW, &permission, &target));
    for cap in [
        Value::Null,
        json!("0"),
        json!("all"),
        json!("-1"),
        json!("18446744073709551616"),
    ] {
        let mut invalid = permission.clone();
        invalid.spec.parameters["maximum_amount_atoms"] = cap;
        assert!(!adapter.plan_has_finite_non_cash_source(OP_WITHDRAW, &invalid, &target));
    }
    let mut wrong = target.clone();
    wrong.resource = resource(&key(9));
    assert!(!adapter.plan_has_finite_non_cash_source(OP_WITHDRAW, &permission, &wrong));
    wrong = target.clone();
    wrong.amount.asset = asset(&token(9));
    assert!(!adapter.plan_has_finite_non_cash_source(OP_WITHDRAW, &permission, &wrong));
    let mut request: gofin_domain::financial_plans::FinancialPlanAuthorizationRequest = serde_json::from_value(json!({
        "request_id":"withdraw-finite-source","valid_for_seconds":600,
        "plan":{"version":1,"name":"Synthetic collateral withdrawal","steps":[{
            "step_id":"withdraw","operation":OP_WITHDRAW,"input":raw,"permitted_variations":b,
            "completion":{"operation_succeeded":true,"minimum_net_receipt":target},"funding_dependencies":[]
        }],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":permission.spec.maximum_costs}}
    })).unwrap();
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(Approval),
        gofin_engine::EngineOptions::default(),
    )
    .unwrap();
    let review = engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.expected_review_hash = Some(review.review_hash);
    let status = engine.authorize_financial_plan(request).await.unwrap();
    assert_eq!(status.next_eligible_step_id.as_deref(), Some("withdraw"));
    assert_eq!(
        status.revision.permissions[0].permission.as_ref().unwrap(),
        &permission
    );
    // Public status reloads the durable plan rather than preparing new metadata.
    let reloaded = engine
        .financial_plan_status(&status.revision.plan_id)
        .await
        .unwrap();
    assert_eq!(reloaded.revision, status.revision);
    assert_eq!(reloaded.next_eligible_step_id.as_deref(), Some("withdraw"));
}

#[tokio::test]
async fn engine_previews_and_authorizes_zero_and_positive_fee_borrow_permissions() {
    struct Approval;
    #[async_trait]
    impl Authorizer for Approval {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                source: Some(AuthorizationSource::Development),
                yolo_provenance: None,
                mechanism: "synthetic".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: Utc::now().to_rfc3339(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }
    for fee in ["0", "2"] {
        let adapter = Arc::new(adapter());
        let mut raw = input();
        raw["maximum_debt_increase_atoms"] = json!("102");
        raw["maximum_protocol_fee_atoms"] = json!(fee);
        raw["risk_limits"] = json!(limits());
        let mut permission_bounds = bounds();
        permission_bounds["maximum_protocol_fee_atoms"] = json!(fee);
        let permission = adapter
            .prepare_plan_permission(OP_BORROW, &raw, &permission_bounds)
            .await
            .unwrap();
        assert_eq!(
            permission.spec.maximum_costs.len(),
            if fee == "0" { 1 } else { 2 }
        );
        assert_eq!(
            permission.spec.parameters["maximum_protocol_fee_atoms"],
            fee
        );
        for costs in [
            &permission.spec.maximum_costs,
            &permission.cumulative_scope.as_ref().unwrap().maximum_costs,
        ] {
            assert!(costs.iter().all(|cost| !cost.amount.atoms.is_zero()));
        }
        let engine = gofin_engine::Engine::new(
            gofin_store::Store::in_memory().unwrap(),
            vec![adapter],
            Arc::new(Approval),
            gofin_engine::EngineOptions::default(),
        )
        .unwrap();
        let mut request: gofin_domain::financial_plans::FinancialPlanAuthorizationRequest = serde_json::from_value(json!({
            "request_id":format!("borrow-fee-{fee}"),"valid_for_seconds":600,
            "plan":{"version":1,"name":"Synthetic native borrow","steps":[{
                "step_id":"borrow","operation":OP_BORROW,"input":raw,"permitted_variations":permission_bounds,
                "completion":{"operation_succeeded":true},"funding_dependencies":[]
            }],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":permission.spec.maximum_costs}}
        })).unwrap();
        let review = engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        request.expected_review_hash = Some(review.review_hash);
        let authorized = engine.authorize_financial_plan(request).await.unwrap();
        assert_eq!(
            authorized.revision.status,
            gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Active
        );

        // The missing zero-valued accounting row grants no fee authority:
        // canonical parameters still reject a candidate that raises its cap.
        let mut selected = intent();
        selected.maximum_protocol_fee_atoms = fee.into();
        boundaries::validate(&permission.spec, &prepared(&selected, &candidate())).unwrap();
        let mut quote = candidate();
        if fee == "0" {
            assert!(validate_candidate(&selected, &key(1), &quote).is_err());
            quote.quote.protocol_fees.clear();
            quote.quote.projected_obligation.positions = vec![supply(), debt(100)];
        }
        validate_candidate(&selected, &key(1), &quote).unwrap();
        selected.maximum_protocol_fee_atoms = if fee == "0" { "1" } else { "3" }.into();
        assert!(
            boundaries::validate(&permission.spec, &prepared(&selected, &candidate())).is_err()
        );
    }
}
#[test]
fn reusable_borrow_allows_finite_amount_variation_but_pins_health_and_fee_terms() {
    let i = intent();
    let c = candidate();
    let spec = boundaries::permission(&i, &key(1), &c.reserve, &c.obligation_before, &bounds())
        .unwrap()
        .spec;
    let mut varied = i.clone();
    varied.amount_atoms = "150".into();
    varied.maximum_debt_increase_atoms = Some("152".into());
    varied.maximum_protocol_fee_atoms = "2".into();
    boundaries::validate(&spec, &prepared(&varied, &c)).unwrap();
    for change in 0..4 {
        let mut bad = prepared(&varied, &c);
        match change {
            0 => bad.binding["intent"]["risk_limits"]["minimum_health_factor_bps"] = json!(11000),
            1 => bad.binding["intent"]["obligation"] = json!(key(9)),
            2 => {
                bad.binding["protocol_candidate"]["reserve"]["protocol_state"]["authority_terms"]
                    ["borrow_fee_sf"] = json!("2")
            }
            _ => bad.binding["wallet_address"] = json!(key(7)),
        };
        assert!(boundaries::validate(&spec, &bad).is_err());
    }
    varied.amount_atoms = "201".into();
    assert!(boundaries::validate(&spec, &prepared(&varied, &c)).is_err());
}
#[test]
fn candidate_rejects_foreign_cashflow_excess_gross_debt_and_unhealthy_projection() {
    let i = intent();
    let c = candidate();
    validate_candidate(&i, &key(1), &c).unwrap();
    let mut bad = c.clone();
    bad.quote.credits[0].token = token(7);
    assert!(validate_candidate(&i, &key(1), &bad).is_err());
    bad = c.clone();
    bad.quote.protocol_fees[0].atoms = Some("2".into());
    assert!(validate_candidate(&i, &key(1), &bad).is_err());
    bad = c.clone();
    bad.quote
        .projected_obligation
        .risk
        .borrow_factor_adjusted_debt_value_sf = sf(900);
    assert!(validate_candidate(&i, &key(1), &bad).is_err());
    bad = c.clone();
    bad.quote.projected_obligation.risk.refreshed = false;
    assert!(validate_candidate(&i, &key(1), &bad).is_err());
}
#[test]
fn exact_baseline_rejects_tampered_stale_or_missing_evidence() {
    let i = intent();
    let c = candidate();
    let p = prepared(&i, &c);
    validate_baselines(&p, &c).unwrap();
    let mut bad = p.clone();
    bad.binding[BASELINE_ANCHORS_KEY][0]["principal"]["atoms"] = json!("2");
    assert!(validate_baselines(&bad, &c).is_err());
    let mut stale = c.clone();
    stale.obligation_before.observed_at =
        (Utc::now() - chrono::Duration::seconds(121)).to_rfc3339();
    assert!(validate_baselines(&prepared(&i, &stale), &stale).is_err());
    let mut bad = p;
    bad.binding
        .as_object_mut()
        .unwrap()
        .remove(BASELINE_ANCHORS_KEY);
    assert!(validate_baselines(&bad, &c).is_err());
}
#[tokio::test]
async fn collateral_valuation_retains_native_units_and_missing_prices_are_unknown() {
    let o = obligation();
    let mut state = settlement::snapshot_positions(&o).unwrap().remove(0);
    settlement::value_position(&mut state, &Prices(true))
        .await
        .unwrap();
    assert_eq!(state.amount.atoms.as_str(), "200");
    assert_eq!(state.amount.asset, asset(&token(5)));
    let v = &state.anchor.unwrap().evidence["lending_position"]["backing_valuation"];
    assert_eq!(v["total_usd_micros"], "375");
    assert_eq!(v["complete"], false);
    let missing = settlement::public_obligation(&o, &Prices(false))
        .await
        .unwrap();
    assert!(missing["current_value"]["collateral_usd_micros"].is_null());
    assert!(
        !missing["current_value"]["missing_data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let valued = settlement::public_obligation(&o, &Prices(true))
        .await
        .unwrap();
    assert_eq!(valued["current_value"]["collateral_usd_micros"], "375");
    assert_eq!(valued["current_value"]["complete"], false);
    let mut partial = o;
    partial.complete = false;
    let partial = settlement::public_obligation(&partial, &Prices(true))
        .await
        .unwrap();
    assert!(partial["current_value"]["collateral_usd_micros"].is_null());
    assert_eq!(
        partial["current_value"]["known_collateral_subtotal_usd_micros"],
        "375"
    );
}
#[test]
fn unsupported_term_debt_is_not_imported_as_variable_debt() {
    let mut o = obligation();
    let mut term = debt(100);
    term.protocol_state["debt_kind"] = json!("fixed_term");
    o.positions.push(term);
    assert_eq!(settlement::snapshot_positions(&o).unwrap().len(), 1);
    o.positions[1].protocol_state = json!({});
    assert_eq!(settlement::snapshot_positions(&o).unwrap().len(), 1);
}

fn receipt_fixture() -> (
    OperationRecord,
    LendingIntent,
    LendingCandidate,
    VerifiedLendingEffects,
    FinalizedReceiptProof,
) {
    let i = intent();
    let c = candidate();
    let p = prepared(&i, &c);
    let post = c.quote.projected_obligation.clone();
    let verified = VerifiedLendingEffects {
        token_deltas: vec![VerifiedTokenDelta {
            token: i.token.clone(),
            atoms: SignedQuantity::new("100").unwrap(),
        }],
        transitions: vec![VerifiedLendingTransition {
            reserve: i.reserve.clone(),
            side: LendingSide::Debt,
            token: i.token.clone(),
            native_units_before: "0".into(),
            native_units_after: sf(101),
            action_delta_atoms: SignedQuantity::new("101").unwrap(),
            accrual_delta_atoms: Some(SignedQuantity::new("0").unwrap()),
            evidence: json!({"basis":"controlled_attributable_obligation"}),
        }],
        rent_accounts: vec![],
        protocol_fees: c.quote.protocol_fees.clone(),
        obligation_after: post,
        evidence: json!({"guarded":true}),
    };
    let balance = |n| json!({"accountIndex":1,"mint":i.token.mint,"owner":key(1),"programId":i.token.token_program,"uiTokenAmount":{"amount":format!("{n}"),"decimals":6}});
    let proof = FinalizedReceiptProof {
        signature: "signature".into(),
        slot: 11,
        fee_lamports: 5000,
        failed: false,
        message_sha256: "digest".into(),
        account_keys: vec![key(1), key(10)],
        message_instructions: c.instructions.clone(),
        required_signatures: vec!["signature".into()],
        pre_balances_lamports: vec![1000000, 0],
        post_balances_lamports: vec![995000, 0],
        pre_token_balances: vec![balance(1000)],
        post_token_balances: vec![balance(1100)],
        loaded_addresses: json!({}),
        inner_instructions: vec![],
        log_messages: vec![],
        error: None,
    };
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: "solana:message:digest".into(),
        data: json!({"message_sha256":"digest","wallet_signature":"signature"}),
    };
    let record = OperationRecord {
        id: "controlled".into(),
        request_id: "request".into(),
        fingerprint: "fingerprint".into(),
        fingerprint_version: 1,
        operation: OP_BORROW.into(),
        input: json!({}),
        status: Status::Pending,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
        preview: Some(p),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    (record, i, c, verified, proof)
}
#[test]
fn finalized_borrow_proves_gross_debt_net_receipt_fee_and_explicit_zero_accrual() {
    let (r, i, c, v, p) = receipt_fixture();
    let verified = settlement::verify(&r, &i, &c, &v, &p).unwrap();
    assert!(
        verified
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::Accrual{delta,..}if delta.is_zero()))
    );
    assert!(
        verified
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::DebtChange{delta,..}if delta.as_str()=="101"))
    );
    assert!(verified.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,treatment:CostTreatment::IncludedInDebit,..}if amount.atoms.as_str()=="1")));
    assert_eq!(
        verified_effect_digest(&verified).unwrap(),
        verified.proof_digest
    );
}
#[test]
fn finalized_evidence_rejects_changed_signature_missing_debt_and_unrelated_cash() {
    let (r, i, c, v, p) = receipt_fixture();
    let mut bad = p.clone();
    bad.signature = "other".into();
    assert!(settlement::verify(&r, &i, &c, &v, &bad).is_err());
    let mut bad = v.clone();
    bad.transitions.clear();
    assert!(settlement::verify(&r, &i, &c, &bad, &p).is_err());
    let mut bad = v.clone();
    bad.transitions[0].action_delta_atoms = SignedQuantity::new("102").unwrap();
    assert!(settlement::verify(&r, &i, &c, &bad, &p).is_err());
    let mut bad = p.clone();
    bad.post_token_balances[0]["mint"] = json!(key(7));
    assert!(settlement::verify(&r, &i, &c, &v, &bad).is_err());
    let mut bad = v;
    bad.obligation_after.observed_slot = 9;
    assert!(settlement::verify(&r, &i, &c, &bad, &p).is_err());
}

#[test]
fn finalized_empty_obligation_closure_refunds_only_exact_reviewed_rent() {
    let (mut r, mut i, mut c, mut v, mut p) = receipt_fixture();
    i.action = LendingAction::Withdraw;
    i.amount_atoms = "200".into();
    i.minimum_receive_atoms = Some("300".into());
    i.maximum_debt_increase_atoms = None;
    i.maximum_protocol_fee_atoms = "0".into();
    c.quote.projected_obligation.positions.clear();
    c.quote.projected_obligation.exists = false;
    c.metadata["possible_closed_accounts"] = json!([{"account":i.obligation,"maximum_lamports":2000,"observed_slot":c.obligation_before.observed_slot,"observed_at":c.obligation_before.observed_at}]);
    r.preview = Some(prepared(&i, &c));
    v.obligation_after = c.quote.projected_obligation.clone();
    v.protocol_fees.clear();
    v.token_deltas[0].atoms = SignedQuantity::new("300").unwrap();
    v.transitions = vec![VerifiedLendingTransition {
        reserve: i.reserve.clone(),
        side: LendingSide::Supply,
        token: token(5),
        native_units_before: "200".into(),
        native_units_after: "0".into(),
        action_delta_atoms: SignedQuantity::new("-200").unwrap(),
        accrual_delta_atoms: None,
        evidence: json!({"basis":"controlled_collateral_burn"}),
    }];
    v.rent_accounts = vec![VerifiedRentAccountDelta {
        account: i.obligation.clone(),
        atoms: SignedQuantity::new("-2000").unwrap(),
    }];
    p.account_keys.push(i.obligation.clone());
    p.pre_balances_lamports.push(2000);
    p.post_balances_lamports.push(0);
    p.post_balances_lamports[0] = 997000;
    p.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("1300");
    let verified = settlement::verify(&r, &i, &c, &v, &p).unwrap();
    assert!(verified.effects.iter().any(|e|matches!(e,FinancialEffect::UsableCredit{amount,..}if amount.asset==native_asset()&&amount.atoms.as_str()=="2000")));
    assert!(verified.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{delta,anchor:Some(a),..}if delta.as_str()=="-2000"&&a.position_kind==PositionKind::RefundableAccountFunding&&a.principal.atoms.is_zero())));
    p.post_balances_lamports[2] = 1;
    assert!(settlement::verify(&r, &i, &c, &v, &p).is_err());
    p.post_balances_lamports[2] = 0;
    v.obligation_after.exists = true;
    assert!(settlement::verify(&r, &i, &c, &v, &p).is_err());
}

#[test]
fn repayment_expenses_only_proven_unpaid_interest_and_rejects_external_principal_changes() {
    let (mut r, mut i, mut c, mut v, mut p) = receipt_fixture();
    i.action = LendingAction::Repay;
    i.amount_atoms = "112".into();
    i.all = true;
    i.maximum_debt_increase_atoms = None;
    i.maximum_protocol_fee_atoms = "0".into();
    i.risk_limits = None;
    let mut before = debt(110);
    before.cumulative_borrow_rate_bsf = Some(sf(11));
    c.obligation_before.positions.push(before);
    c.quote
        .projected_obligation
        .positions
        .retain(|row| row.side == LendingSide::Supply);
    let mut old = c.obligation_before.clone();
    old.positions = vec![debt(100)];
    old.positions[0].cumulative_borrow_rate_bsf = Some(sf(10));
    let mut prior = settlement::snapshot_positions(&old)
        .unwrap()
        .remove(0)
        .anchor
        .unwrap();
    prior.evidence["known_unpaid_interest_atoms"] = json!("3");
    let mut preview = prepared(&i, &c);
    preview.binding["prior_anchors"] = json!([prior]);
    r.preview = Some(preview);
    v.obligation_after = c.quote.projected_obligation.clone();
    v.protocol_fees.clear();
    v.token_deltas[0].atoms = SignedQuantity::new("-112").unwrap();
    v.transitions[0].native_units_before = sf(110);
    v.transitions[0].native_units_after = "0".into();
    v.transitions[0].action_delta_atoms = SignedQuantity::new("-112").unwrap();
    v.transitions[0].accrual_delta_atoms = Some(SignedQuantity::new("2").unwrap());
    p.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("888");
    let verified = settlement::verify(&r, &i, &c, &v, &p).unwrap();
    assert!(
        verified
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::Accrual{delta,..}if delta.as_str()=="12"))
    );
    assert!(verified.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,category,..}if category=="klend_verified_variable_interest"&&amount.atoms.as_str()=="15")));
    r.preview.as_mut().unwrap().binding["prior_anchors"][0]["evidence"]["native_units"] =
        json!(sf(99));
    assert!(settlement::verify(&r, &i, &c, &v, &p).is_err());
}

#[test]
fn expired_native_candidate_is_rejected_before_signing_or_network_dispatch() {
    let mut candidate = candidate();
    candidate.quote.expires_at = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    let error = validate_candidate(&intent(), &key(1), &candidate).unwrap_err();
    assert_eq!(error.code, "PREVIEW_EXPIRED");
    candidate.quote.expires_at = "not-a-timestamp".into();
    assert!(validate_candidate(&intent(), &key(1), &candidate).is_err());
}

#[tokio::test]
async fn default_obligation_discovery_supports_first_supply_and_preserves_explicit_selection() {
    let input = json!({"wallet":"wallet","market":key(3)});
    let absent = adapter_with_absence(true)
        .read(OP_OBLIGATION, &input)
        .await
        .unwrap();
    assert_eq!(absent["address"], key(6));
    assert_eq!(absent["exists"], false);
    assert_eq!(absent["complete"], true);
    assert_eq!(absent["owner"], key(1));
    assert!(absent["positions"].as_array().unwrap().is_empty());
    let existing = adapter().read(OP_OBLIGATION, &input).await.unwrap();
    assert_eq!(existing["address"], key(6));
    assert_eq!(existing["exists"], true);
    assert_eq!(existing["positions"].as_array().unwrap().len(), 1);
    let mut explicit = input.clone();
    explicit["obligation"] = json!(key(7));
    assert!(
        adapter().read(OP_OBLIGATION, &explicit).await.is_err(),
        "Explicit addresses must not be replaced with the default"
    );
    assert!(
        adapter()
            .read(OP_OBLIGATION, &json!({"market":key(3)}))
            .await
            .is_err()
    );
    assert!(
        adapter()
            .read(OP_OBLIGATION, &json!({"wallet":"wallet"}))
            .await
            .is_err()
    );
    let definition = operation_definitions()
        .into_iter()
        .find(|d| d.name == OP_OBLIGATION)
        .unwrap();
    assert_eq!(
        definition.input_schema["required"],
        json!(["wallet", "market"])
    );
    assert!(
        definition.semantics["default_obligation_discovery"]
            .as_str()
            .unwrap()
            .contains("exists=false")
    );
}

#[tokio::test]
async fn rounded_supply_preserves_requested_cap_and_records_actual_unspent_input() {
    let (mut record, mut intent, mut candidate, mut verified, mut proof) = receipt_fixture();
    intent.action = LendingAction::Supply;
    intent.maximum_debt_increase_atoms = None;
    intent.maximum_protocol_fee_atoms = "0".into();
    intent.risk_limits = None;
    candidate.quote.debits = vec![TokenAmount {
        token: intent.token.clone(),
        atoms: Some("99".into()),
    }];
    candidate.quote.credits.clear();
    candidate.quote.protocol_fees.clear();
    candidate.quote.projected_obligation.positions = vec![supply()];
    candidate.quote.projected_obligation.positions[0].native_units = "230".into();
    candidate.quote.projected_obligation.positions[0].underlying_atoms = Some("398".into());
    validate_candidate(&intent, &key(1), &candidate).unwrap();
    for debit in ["0", "101"] {
        let mut bad = candidate.clone();
        bad.quote.debits[0].atoms = Some(debit.into());
        assert!(validate_candidate(&intent, &key(1), &bad).is_err());
    }
    record.preview = Some(prepared(&intent, &candidate));
    verified.obligation_after = candidate.quote.projected_obligation.clone();
    verified.protocol_fees.clear();
    verified.token_deltas[0].atoms = SignedQuantity::new("-98").unwrap();
    verified.transitions = vec![VerifiedLendingTransition {
        reserve: intent.reserve.clone(),
        side: LendingSide::Supply,
        token: token(5),
        native_units_before: "200".into(),
        native_units_after: "230".into(),
        action_delta_atoms: SignedQuantity::new("30").unwrap(),
        accrual_delta_atoms: None,
        evidence: json!({"basis":"controlled_native_collateral_rounding"}),
    }];
    proof.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("902");
    let settled = settlement::verify(&record, &intent, &candidate, &verified, &proof).unwrap();
    assert!(settled.effects.iter().any(|e|matches!(e,FinancialEffect::CashDebit{amount,..}if amount.asset==asset(&intent.token)&&amount.atoms.as_str()=="98")));
    assert_eq!(settled.evidence["supply_input"]["maximum_atoms"], "100");
    assert_eq!(settled.evidence["supply_input"]["actual_debit_atoms"], "98");
    assert_eq!(settled.evidence["supply_input"]["unspent_input_atoms"], "2");
    for (post, actual) in [("1000", "0"), ("899", "-101")] {
        let mut bad = proof.clone();
        bad.post_token_balances[0]["uiTokenAmount"]["amount"] = json!(post);
        let mut v = verified.clone();
        v.token_deltas[0].atoms = SignedQuantity::new(actual).unwrap();
        assert!(settlement::verify(&record, &intent, &candidate, &v, &bad).is_err());
    }
    let signer = gofin_solana::SolanaSigner::parse(
        "99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM",
    )
    .unwrap();
    candidate.obligation_before.owner = signer.address().into();
    candidate.quote.projected_obligation.owner = signer.address().into();
    struct HeightRpc;
    #[async_trait]
    impl SolanaRpc for HeightRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            match method {
                "getBlockHeight" => assert_eq!(params, json!([{"commitment":"confirmed"}])),
                "getSlot" => assert_eq!(params, json!([{"commitment":"finalized"}])),
                _ => panic!("Unexpected controlled RPC method {method}"),
            }
            Ok(json!(1))
        }
    }
    let runtime = SolanaRuntime::new(Arc::new(HeightRpc));
    let frozen = runtime
        .freeze_instructions(
            &candidate.instructions,
            &[],
            &candidate.blockhash,
            &signer,
            candidate.last_valid_block_height,
            intent.maximum_network_fee_lamports,
            None,
            &[],
        )
        .await
        .unwrap();
    let preview = build_prepared(
        OP_SUPPLY,
        &input(),
        &intent,
        signer.address(),
        &candidate,
        &frozen,
        5000,
        &PreparationContext::default(),
    )
    .unwrap();
    assert_eq!(preview.intent.debits[0].amount.atoms.as_str(), "100");
    assert!(preview.intent.expected.iter().any(
        |e| matches!(e,EffectConstraint::Debit{maximum_debit,..}if maximum_debit.as_str()=="100")
    ));
    assert!(preview.intent.expected.iter().any(|e|matches!(e,EffectConstraint::OriginMovement{maximum_debit,..}if maximum_debit.as_str()=="100")));
    assert!(
        preview
            .summary
            .contains("expected debit 99 atoms and expected unspent input 1 atoms")
    );
    assert!(
        operation_definitions()
            .into_iter()
            .find(|d| d.name == OP_SUPPLY)
            .unwrap()
            .semantics["supply_rounding"]
            .as_str()
            .unwrap()
            .contains("maximum")
    );
}
