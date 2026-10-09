use super::*;
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanFundingDependency,
    FinancialPlanSpec, FinancialPlanStepSpec, FinancialStepCompletion,
};
use fin_domain::{Adapter, Amount, AssetId, CostBound, CostTreatment, ResourceAmount};

struct NoPriceProvider(AtomicUsize);
#[async_trait]
impl BinancePriceProvider for NoPriceProvider {
    async fn quote_usd(&self, _coin: &str) -> fin_domain::Result<PriceQuote> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(FinError::validation(
            "Plan permission must not request prices.",
        ))
    }
}

fn variations(side: &str) -> Value {
    if side == "BUY" {
        json!({
            "maximum_spend":"40",
            "maximum_price":"2100",
            "maximum_fees":{"ETH":"0.00005"},
            "allowed_time_in_force":["IOC","FOK"],
            "allowed_order_types":["LIMIT"]
        })
    } else {
        json!({
            "maximum_quantity":"0.02",
            "minimum_price":"1900",
            "maximum_fees":{"USDT":"0.05"},
            "allowed_time_in_force":["IOC","FOK"],
            "allowed_order_types":["LIMIT"]
        })
    }
}

#[tokio::test]
async fn spot_plan_permissions_validate_metadata_without_requiring_balances_or_quotes() {
    for (side, expected_credit) in [("BUY", "ETH"), ("SELL", "USDT")] {
        let venue = Venue::new(vec![]);
        let no_prices = Arc::new(NoPriceProvider(AtomicUsize::new(0)));
        // Permission preparation must work even though the chosen side's
        // likely input balance is zero. A concrete execution rechecks it.
        assert_eq!(venue.state.lock().unwrap().spot["ETH"], 0);
        let adapter = Arc::new(
            BinanceAdapter::new(config(), venue.clone(), Some(no_prices.clone()), None).unwrap(),
        );
        let permission = adapter
            .prepare_plan_permission(
                "binance.spot.order-place",
                &input(&format!("SYNTHETIC-plan-{side}"), side, false),
                &variations(side),
            )
            .await
            .unwrap();

        assert_eq!(permission.spec.fixed["account"], ACCOUNT);
        assert_eq!(permission.spec.fixed["symbol"], "ETHUSDT");
        assert_eq!(permission.spec.fixed["side"], side);
        assert_eq!(permission.possible_credits.len(), 1);
        assert_eq!(permission.possible_credits[0].resource.0, SPOT);
        assert_eq!(
            permission.possible_credits[0].asset.reference,
            expected_credit
        );
        assert_eq!(permission.fee_assets.len(), 1);
        assert!(permission.spec.summary.contains("price"));
        assert!(permission.spec.summary.contains("Cumulative fee caps:"));
        assert!(permission.spec.summary.contains(if side == "BUY" {
            "0.00005 ETH"
        } else {
            "0.05 USDT"
        }));
        assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
        assert_eq!(no_prices.0.load(Ordering::SeqCst), 0);
        let endpoints: Vec<_> = venue
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.endpoint)
            .collect();
        assert!(endpoints.contains(&E::ApiRestrictions));
        assert!(endpoints.contains(&E::Account)); // identity only; no free amount is checked
        assert!(endpoints.contains(&E::SpotExchangeInfo));
        assert!(endpoints.contains(&E::SpotCommission));
        assert!(!endpoints.contains(&E::SpotPlaceOrder));
    }
}

#[tokio::test]
async fn spot_plan_permission_rejects_seed_identity_variation_and_insufficient_fee_cap() {
    let mut wrong_account = input("SYNTHETIC-plan-wrong-account", "BUY", false);
    wrong_account["account"] = json!("unknown-account");
    let mut wrong_time = input("SYNTHETIC-plan-wrong-time", "BUY", false);
    wrong_time["time_in_force"] = json!("FOK");
    let mut too_low_fee = input("SYNTHETIC-plan-too-low-fee", "BUY", false);
    too_low_fee["maximum_fees"]["ETH"] = json!("0.000001");
    let cases: Vec<(Value, Value)> = vec![
        (wrong_account, variations("BUY")),
        (
            input("SYNTHETIC-plan-wrong-side", "SELL", false),
            variations("BUY"),
        ),
        (
            wrong_time,
            json!({"maximum_spend":"40","maximum_price":"2100","maximum_fees":{"ETH":"0.00005"},"allowed_time_in_force":["IOC"],"allowed_order_types":["LIMIT"]}),
        ),
        (too_low_fee, variations("BUY")),
    ];

    for (raw, bounds) in cases {
        let venue = Venue::new(vec![]);
        let adapter = adapter(venue.clone(), prices());
        assert!(
            adapter
                .prepare_plan_permission("binance.spot.order-place", &raw, &bounds)
                .await
                .is_err()
        );
        assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    }
}

fn plan_asset(reference: &str) -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: reference.into(),
        decimals: 36,
    }
}

fn resource_amount(reference: &str, amount: &str) -> ResourceAmount {
    ResourceAmount {
        resource: ResourceId(SPOT.into()),
        amount: Amount {
            asset: plan_asset(reference),
            atoms: fin_domain::Quantity::from_decimal(amount, 36).unwrap(),
        },
    }
}

fn cost_bound(reference: &str, amount: &str, treatment: CostTreatment) -> CostBound {
    CostBound {
        location: fin_domain::AccountingLocation::Cash {
            resource: ResourceId(SPOT.into()),
        },
        amount: Amount {
            asset: plan_asset(reference),
            atoms: fin_domain::Quantity::from_decimal(amount, 36).unwrap(),
        },
        treatment,
    }
}

fn two_step_plan() -> FinancialPlanSpec {
    let buy = input("SYNTHETIC-plan-buy-template", "BUY", false);
    let mut sell = input("SYNTHETIC-plan-sell-template", "SELL", false);
    sell["quantity"] = json!("0.01998");
    FinancialPlanSpec {
        version: 1,
        name: "Synthetic ETH round trip".into(),
        steps: vec![
            FinancialPlanStepSpec {
                step_id: "buy-eth".into(),
                operation: "binance.spot.order-place".into(),
                input: buy,
                permitted_variations: variations("BUY"),
                completion: FinancialStepCompletion {
                    principal_spend: Some(resource_amount("USDT", "40")),
                    minimum_net_receipt: None,
                    operation_succeeded: false,
                },
                funding_dependencies: vec![],
            },
            FinancialPlanStepSpec {
                step_id: "sell-eth".into(),
                operation: "binance.spot.order-place".into(),
                input: sell,
                permitted_variations: variations("SELL"),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: Some(resource_amount("USDT", "39")),
                    operation_succeeded: false,
                },
                funding_dependencies: vec![FinancialPlanFundingDependency {
                    producing_step_id: "buy-eth".into(),
                    resource: ResourceId(SPOT.into()),
                    asset: plan_asset("ETH"),
                }],
            },
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![
                resource_amount("USDT", "40"),
                resource_amount("ETH", "0.02"),
            ],
            maximum_costs: vec![
                cost_bound("ETH", "0.00005", CostTreatment::DeductedFromCredit),
                cost_bound("USDT", "0.05", CostTreatment::DeductedFromCredit),
            ],
        },
    }
}

fn plan_request() -> FinancialPlanAuthorizationRequest {
    FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-plan-approval".into(),
        plan: two_step_plan(),
        valid_for_seconds: 600,
        replaces_revision_id: None,
        expected_review_hash: None,
    }
}

fn plan_operation_request(request_id: &str, side: &str, quantity: &str) -> OperationRequest {
    let template_request_id = match side {
        "BUY" => "SYNTHETIC-plan-buy-template",
        "SELL" => "SYNTHETIC-plan-sell-template",
        _ => panic!("unsupported synthetic side"),
    };
    let mut raw = input(template_request_id, side, false);
    raw["quantity"] = json!(quantity);
    OperationRequest {
        request_id: request_id.into(),
        operation: "binance.spot.order-place".into(),
        input: raw,
        run_id: None,
    }
}

fn destination_config(address: &str) -> BinanceConfig {
    use std::collections::BTreeMap;
    let mut config = config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "dev-wallet".into(),
        resource: ResourceId("ethereum:synthetic-dev-wallet".into()),
        address: address.into(),
        chain_id: 1,
        assets: BTreeMap::from([(
            "ETH".into(),
            AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
        )]),
        confirmations: 2,
    });
    config
}

fn withdrawal_plan() -> FinancialPlanSpec {
    let input = json!({
        "requestId":"SYNTHETIC-plan-withdraw-request",
        "account":ACCOUNT,"destination":"dev-wallet","coin":"ETH","amount":"0.01",
        "network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"
    });
    let amount = |quantity: &str| ResourceAmount {
        resource: ResourceId(SPOT.into()),
        amount: Amount {
            asset: plan_asset("ETH"),
            atoms: fin_domain::Quantity::from_decimal(quantity, 36).unwrap(),
        },
    };
    let cost = CostBound {
        location: fin_domain::AccountingLocation::Cash {
            resource: ResourceId(SPOT.into()),
        },
        amount: Amount {
            asset: plan_asset("ETH"),
            atoms: fin_domain::Quantity::from_decimal("0.001", 36).unwrap(),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let destination_asset = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let destination_resource = ResourceId("ethereum:synthetic-dev-wallet".into());
    FinancialPlanSpec {
        version: 1,
        name: "Synthetic named destination withdrawal".into(),
        steps: vec![FinancialPlanStepSpec {
            step_id: "withdraw".into(),
            operation: "binance.withdraw".into(),
            input,
            permitted_variations: json!({
                "mode":"exact",
                "maximum_debits":[amount("0.01")],
                "maximum_costs":[cost.clone()],
                "possible_credits":[{"resource":destination_resource,"asset":destination_asset}]
            }),
            completion: FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: None,
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
        }],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![amount("0.01")],
            maximum_costs: vec![cost],
        },
    }
}

struct AliasPlanTransport {
    posts: AtomicUsize,
}

#[async_trait]
impl BinanceTransport for AliasPlanTransport {
    async fn request(
        &self,
        _credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({"enableReading":true,"enableWithdrawals":true})),
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::CapitalConfig => Ok(json!([{
                "coin":"ETH","withdrawAllEnable":true,"depositAllEnable":true,
                "networkList":[{"network":"ETH","coin":"ETH","withdrawEnable":true,
                    "depositEnable":true,"withdrawIntegerMultiple":"0.00000001","withdrawFee":"0.001",
                    "withdrawMin":"0.001","withdrawMax":"1000","withdrawTag":false,"busy":false}]
            }])),
            E::Account => Ok(json!({"uid":"1001","accountType":"SPOT","balances":[{
                "asset":"ETH","free":"1","locked":"0"
            }]})),
            E::StandardWithdrawal => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"id":"synthetic-alias-withdrawal"}))
            }
            E::WithdrawalHistory => Ok(json!([])),
            endpoint => panic!("unexpected alias-plan endpoint {endpoint:?}"),
        }
    }
}

fn alias_plan_engine(
    venue: Arc<AliasPlanTransport>,
    authorizer: Arc<Approval>,
    store: Store,
    config: BinanceConfig,
) -> Engine {
    let prices = prices();
    let adapter = Arc::new(BinanceAdapter::new(config, venue, Some(prices.clone()), None).unwrap());
    Engine::new(
        store,
        vec![adapter],
        authorizer,
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices)
}

#[test]
fn public_connections_exclude_destinations_but_plan_identities_keep_full_route() {
    use fin_domain::Adapter;

    let address = "0x1111111111111111111111111111111111111111";
    let adapter = BinanceAdapter::new(
        destination_config(address),
        Arc::new(AliasPlanTransport {
            posts: AtomicUsize::new(0),
        }),
        Some(prices()),
        None,
    )
    .unwrap();

    let public = adapter.connections();
    assert_eq!(public.len(), 1);
    assert_eq!(public[0].provider, "binance");
    assert_eq!(public[0].name, ACCOUNT);

    let identities = adapter.authorization_identities();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0], public[0]);
    let destination = &identities[1];
    assert_eq!(destination.name, "dev-wallet");
    assert_eq!(destination.provider, "binance-ethereum-destination");
    assert_eq!(destination.resource.0, "ethereum:synthetic-dev-wallet");
    assert_eq!(destination.role.as_deref(), Some("deposit-destination"));
    assert_eq!(destination.network.as_deref(), Some("ethereum:1"));
    assert_eq!(destination.address.as_deref(), Some(address));
    assert!(!destination.signing_enabled);
    assert_eq!(destination.details["chain_id"], 1);
    assert_eq!(destination.details["confirmations"], 2);
    assert_eq!(
        destination.details["assets"]["ETH"]["network"],
        "ethereum:1"
    );
    assert_eq!(destination.details["assets"]["ETH"]["reference"], "native");
    assert_eq!(destination.details["assets"]["ETH"]["decimals"], 18);
}

#[tokio::test]
async fn exact_withdrawal_plan_rejects_a_remapped_named_destination_before_dispatch() {
    let original_address = "0x1111111111111111111111111111111111111111";
    let changed_address = "0x2222222222222222222222222222222222222222";
    let original_config = destination_config(original_address);
    let mut remapped_config = destination_config(changed_address);
    let public_rows = |config| {
        BinanceAdapter::new(
            config,
            Arc::new(AliasPlanTransport {
                posts: AtomicUsize::new(0),
            }),
            Some(prices()),
            None,
        )
        .unwrap()
        .connections()
    };
    use fin_domain::Adapter;
    let original_public_rows = public_rows(original_config.clone());
    assert_eq!(original_public_rows.len(), 1);
    assert_eq!(public_rows(remapped_config.clone()), original_public_rows);

    let venue = Arc::new(AliasPlanTransport {
        posts: AtomicUsize::new(0),
    });
    let approvals = approve(ApprovalMode::Accept);
    let directory = tempfile::tempdir().unwrap();
    let database = directory
        .path()
        .join("SYNTHETIC-plan-destination-alias.sqlite");
    let engine = alias_plan_engine(
        venue.clone(),
        approvals.clone(),
        Store::open(&database).unwrap(),
        original_config.clone(),
    );
    let mut authorization = plan_request();
    authorization.plan = withdrawal_plan();
    authorization.request_id = "SYNTHETIC-plan-destination-alias-approval".into();
    let review = engine
        .preview_financial_plan(authorization.clone())
        .await
        .unwrap();
    authorization.expected_review_hash = Some(review.review_hash);
    let approved = engine
        .authorize_financial_plan(authorization)
        .await
        .unwrap();
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    drop(engine);

    // Keep the alias name, resource and exact operation input unchanged while
    // remapping only its address. The destination metadata must still alter
    // the generic plan's durable connection binding.
    let restarted = alias_plan_engine(
        venue.clone(),
        approvals.clone(),
        Store::open(&database).unwrap(),
        remapped_config.clone(),
    );
    let mut raw = withdrawal_plan().steps[0].input.clone();
    let request_id = "SYNTHETIC-plan-destination-alias-attempt";
    let result = restarted
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw",
            OperationRequest {
                request_id: request_id.into(),
                operation: "binance.withdraw".into(),
                input: std::mem::take(&mut raw),
                run_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(result.code, "PLAN_OPERATION_CONTRACT_CHANGED");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);

    // A chain-only remap is also private authorization identity, even though
    // the public account list and operation input are unchanged.
    remapped_config.destinations[0].address = original_address.into();
    remapped_config.destinations[0].chain_id = 5;
    assert_eq!(public_rows(remapped_config.clone()), original_public_rows);
    let changed_chain = alias_plan_engine(
        venue.clone(),
        approvals.clone(),
        Store::open(&database).unwrap(),
        remapped_config.clone(),
    );
    let result = changed_chain
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw",
            OperationRequest {
                request_id: format!("{request_id}-chain-change"),
                operation: "binance.withdraw".into(),
                input: withdrawal_plan().steps[0].input.clone(),
                run_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(result.code, "PLAN_OPERATION_CONTRACT_CHANGED");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);

    // Asset mapping is stored in the hashed stable details too, not just the
    // alias name/address fields.
    remapped_config.destinations[0].chain_id = 1;
    remapped_config.destinations[0].address = original_address.into();
    remapped_config.destinations[0]
        .assets
        .get_mut("ETH")
        .unwrap()
        .decimals = 17;
    assert_eq!(public_rows(remapped_config.clone()), original_public_rows);
    let changed_asset = alias_plan_engine(
        venue.clone(),
        approvals.clone(),
        Store::open(&database).unwrap(),
        remapped_config,
    );
    let result = changed_asset
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw",
            OperationRequest {
                request_id: format!("{request_id}-asset-change"),
                operation: "binance.withdraw".into(),
                input: withdrawal_plan().steps[0].input.clone(),
                run_id: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(result.code, "PLAN_OPERATION_CONTRACT_CHANGED");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);

    // With the unchanged protected routing configuration, the same saved
    // authorization remains usable through BinanceAdapter -> ExchangeAdapter
    // -> BinanceProfile forwarding.
    let unchanged = alias_plan_engine(
        venue.clone(),
        approvals,
        Store::open(&database).unwrap(),
        original_config,
    );
    let _ = unchanged
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw",
            OperationRequest {
                request_id: format!("{request_id}-unchanged-config"),
                operation: "binance.withdraw".into(),
                input: withdrawal_plan().steps[0].input.clone(),
                run_id: None,
            },
        )
        .await;
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_approval_unlocks_sell_only_after_verified_partial_buy_proceeds() {
    let venue = Venue::new(vec![
        Fill::partial(),
        Fill::full("ETH", "0.00001"),
        Fill::full("USDT", "0.04"),
    ]);
    assert_eq!(venue.state.lock().unwrap().spot["ETH"], 0);
    let prices = prices();
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices,
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut authorization_request = plan_request();
    let review = engine
        .preview_financial_plan(authorization_request.clone())
        .await
        .unwrap();
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 0);
    authorization_request.expected_review_hash = Some(review.review_hash);
    let approved = engine
        .authorize_financial_plan(authorization_request)
        .await
        .unwrap();
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(approved.next_eligible_step_id.as_deref(), Some("buy-eth"));

    let premature = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "sell-eth",
            plan_operation_request("SYNTHETIC-plan-premature-sell", "SELL", "0.01998"),
        )
        .await
        .unwrap_err();
    assert_eq!(premature.code, "PLAN_STEP_NOT_ELIGIBLE");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);

    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-partial", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");
    let repeated_first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-partial", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(repeated_first.id, first.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    let partial_status = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(
        partial_status.next_eligible_step_id.as_deref(),
        Some("buy-eth")
    );
    assert_eq!(
        partial_status.steps[0].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::InProgress
    );

    let second = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-remainder", "BUY", "0.01"),
        )
        .await
        .unwrap();
    assert_eq!(second.status, Status::Completed, "{second:?}");
    let repeated_second = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-remainder", "BUY", "0.01"),
        )
        .await
        .unwrap();
    assert_eq!(repeated_second.id, second.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
    let funded = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(funded.next_eligible_step_id.as_deref(), Some("sell-eth"));
    assert!(!funded.steps[0].verified_receipts.is_empty());
    assert_eq!(
        funded.usage.verified_proceeds[0].available.to_decimal(36),
        "0.01998"
    );
    assert_eq!(
        funded.usage.verified_proceeds[0].consumed.to_decimal(36),
        "0"
    );
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);

    let repeated_completed_step = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-remainder", "BUY", "0.01"),
        )
        .await
        .unwrap();
    assert_eq!(repeated_completed_step.id, second.id);
    let completed_step_refusal = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-buy-too-late", "BUY", "0.01"),
        )
        .await
        .unwrap_err();
    assert_eq!(completed_step_refusal.code, "PLAN_STEP_NOT_ELIGIBLE");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);

    let sold = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "sell-eth",
            plan_operation_request("SYNTHETIC-plan-sell-proceeds", "SELL", "0.01998"),
        )
        .await
        .unwrap();
    assert_eq!(sold.status, Status::Completed, "{sold:?}");
    let completed = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(completed.next_eligible_step_id, None);
    assert_eq!(
        completed.usage.verified_proceeds[0]
            .available
            .to_decimal(36),
        "0.01998"
    );
    assert_eq!(
        completed.usage.verified_proceeds[0].consumed.to_decimal(36),
        "0.01998"
    );
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 3);
    assert_eq!(venue.state.lock().unwrap().spot["ETH"], 0);
}

#[tokio::test]
async fn terminal_no_fill_keeps_the_step_open_and_unlocks_no_dependent_funding() {
    let venue = Venue::new(vec![Fill::none()]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-no-fill-approval".into();
    let approved = engine.authorize_financial_plan(request).await.unwrap();
    let no_fill = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-no-fill-attempt", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(no_fill.status, Status::Failed, "{no_fill:?}");
    let status = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(status.next_eligible_step_id.as_deref(), Some("buy-eth"));
    assert_eq!(
        status.steps[0].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::InProgress
    );
    assert!(status.steps[0].verified_receipts.is_empty());
    assert!(status.steps[1].verified_receipts.is_empty());
    let premature = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "sell-eth",
            plan_operation_request("SYNTHETIC-plan-no-fill-premature-sell", "SELL", "0.01998"),
        )
        .await
        .unwrap_err();
    assert_eq!(premature.code, "PLAN_STEP_NOT_ELIGIBLE");
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unrelated_deposit_cannot_replace_declared_predecessor_proceeds() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-attributed-funding-approval".into();
    // The BUY spends 20 USDT and produces 0.00999 ETH after commission.
    request.plan.steps[0].completion.principal_spend = Some(resource_amount("USDT", "20"));
    let approved = engine.authorize_financial_plan(request).await.unwrap();
    let buy = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-attributed-funding-buy", "BUY", "0.01"),
        )
        .await
        .unwrap();
    assert_eq!(buy.status, Status::Completed, "{buy:?}");
    let funded = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(funded.next_eligible_step_id.as_deref(), Some("sell-eth"));
    assert_eq!(
        funded.usage.verified_proceeds[0].available.to_decimal(36),
        "0.00999"
    );

    // Make the current account balance enough for the SELL by adding an
    // unrelated deposit. The declared dependency still contributes only the
    // verified receipt from buy-eth, so this cannot fund the shortfall.
    venue
        .state
        .lock()
        .unwrap()
        .spot
        .insert("ETH".into(), units("0.01998"));
    let sell = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "sell-eth",
            plan_operation_request("SYNTHETIC-plan-unrelated-deposit-sell", "SELL", "0.01998"),
        )
        .await;
    assert!(
        sell.is_err(),
        "unrelated ETH deposit substituted for predecessor proceeds: {sell:?}"
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
    let blocked = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(blocked.next_eligible_step_id.as_deref(), Some("sell-eth"));
    assert_eq!(
        blocked.usage.verified_proceeds[0].consumed.to_decimal(36),
        "0"
    );
}

#[tokio::test]
async fn remaining_step_spend_target_caps_a_later_candidate_before_dispatch() {
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-step-cap-approval".into();
    request.plan.aggregate_bounds.maximum_debits[0] = resource_amount("USDT", "60");
    let approved = engine.authorize_financial_plan(request).await.unwrap();

    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-step-cap-first", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");
    assert_eq!(
        engine
            .financial_plan_status(&approved.plan_id)
            .await
            .unwrap()
            .next_eligible_step_id
            .as_deref(),
        Some("buy-eth")
    );

    // The first partial order spent 20 USDT of the 40 USDT completion target.
    // A second 40 USDT candidate fits each attempt's scope and the 60 USDT
    // aggregate cap, but exceeds the remaining 20 USDT input target.
    let remainder = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-step-cap-overspend", "BUY", "0.02"),
        )
        .await;
    assert!(
        remainder.is_err(),
        "oversized remainder unexpectedly reached execution: {remainder:?}"
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn aggregate_debit_cap_is_enforced_across_individually_larger_step_scopes() {
    let venue = Venue::new(vec![
        Fill::full("ETH", "0.00001"),
        Fill::full("ETH", "0.00001"),
    ]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut plan = two_step_plan();
    let mut first = plan.steps[0].clone();
    first.completion.principal_spend = Some(resource_amount("USDT", "20"));
    let mut second = first.clone();
    second.step_id = "buy-eth-again".into();
    second.input["request_id"] = json!("SYNTHETIC-plan-aggregate-buy-template");
    second.completion.principal_spend = Some(resource_amount("USDT", "20"));
    let mut third = plan.steps[1].clone();
    third.step_id = "sell-eth-after-blocked-buy".into();
    third.input["request_id"] = json!("SYNTHETIC-plan-aggregate-sell-template");
    third.completion.minimum_net_receipt = Some(resource_amount("USDT", "9"));
    plan.steps = vec![first, second, third];
    // Each step independently allows and targets 20 USDT, while the plan's
    // cumulative authority allows only 30 USDT.
    plan.aggregate_bounds.maximum_debits[0] = resource_amount("USDT", "30");
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-aggregate-approval".into();
    request.plan = plan;
    let approved = engine.authorize_financial_plan(request).await.unwrap();

    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-aggregate-first", "BUY", "0.01"),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");
    let status = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    // The first BUY is complete, but its $20 use leaves only $10 under the
    // $30 aggregate cap, below the second BUY's $20 target. The later SELL
    // could fit the remaining ETH debit cap and has verified ETH funding, but
    // sequential plans may not skip the blocked second step.
    assert_eq!(status.next_eligible_step_id, None);
    assert_eq!(
        status.steps[1].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::Blocked
    );
    assert_eq!(
        status.steps[2].state,
        fin_domain::financial_plans::FinancialPlanStepProgressState::NotStarted
    );
    assert!(!status.steps[1].blockers.is_empty());

    let second = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth-again",
            plan_operation_request("SYNTHETIC-plan-aggregate-second", "BUY", "0.01"),
        )
        .await;
    assert!(
        second.is_err(),
        "the plan spent beyond its aggregate cap: {second:?}"
    );

    let skipped_sell = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "sell-eth-after-blocked-buy",
            plan_operation_request("SYNTHETIC-plan-aggregate-skipped-sell", "SELL", "0.005"),
        )
        .await;
    assert!(
        skipped_sell.is_err(),
        "a feasible later SELL skipped the blocked second BUY: {skipped_sell:?}"
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn remaining_step_fee_cap_rejects_an_attempt_before_dispatch() {
    let venue = Venue::new(vec![Fill::partial(), Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-step-fee-approval".into();
    request.plan.aggregate_bounds.maximum_debits[0] = resource_amount("USDT", "60");
    request.plan.aggregate_bounds.maximum_costs[0] =
        cost_bound("ETH", "0.0001", CostTreatment::DeductedFromCredit);
    let approved = engine.authorize_financial_plan(request).await.unwrap();

    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-step-fee-first", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");

    // The first partial fill consumed 0.00001 ETH commission from the
    // step's 0.00005 ETH fee authority, leaving only 0.00004. The remaining
    // principal target could be met with this quantity, but its requested fee
    // reservation must not exceed the remaining fee authority.
    let mut raw = plan_operation_request("SYNTHETIC-plan-step-fee-exhausted", "BUY", "0.01");
    raw.input["maximum_fees"]["ETH"] = json!("0.00005");
    let attempt = engine
        .execute_financial_plan_step(&approved.plan_id, "buy-eth", raw)
        .await;
    assert!(attempt.is_err(), "fee authority was exceeded: {attempt:?}");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_plan_recovers_duplicate_attempt_and_accepts_a_fresh_revision() {
    let venue = Venue::new(vec![Fill::none()]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-expiring-approval".into();
    request.valid_for_seconds = 2;
    let approved = engine.authorize_financial_plan(request).await.unwrap();
    let original = plan_operation_request("SYNTHETIC-plan-expired-original-attempt", "BUY", "0.02");
    let result = engine
        .execute_financial_plan_step(&approved.plan_id, "buy-eth", original.clone())
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;

    let expired_retry = engine
        .execute_financial_plan_step(&approved.plan_id, "buy-eth", original)
        .await
        .unwrap();
    assert_eq!(expired_retry.id, result.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    let expired = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(
        expired.revision.status,
        fin_domain::financial_plans::FinancialPlanAuthorizationStatus::Expired
    );
    assert_eq!(expired.next_eligible_step_id, None);

    let mut replacement = plan_request();
    replacement.request_id = "SYNTHETIC-plan-after-expiry".into();
    replacement.replaces_revision_id = Some(approved.revision.revision_id.clone());
    let review = engine
        .preview_financial_plan(replacement.clone())
        .await
        .unwrap();
    replacement.expected_review_hash = Some(review.review_hash);
    let renewed = engine.authorize_financial_plan(replacement).await.unwrap();
    assert_eq!(renewed.plan_id, approved.plan_id);
    assert_ne!(renewed.revision.revision_id, approved.revision.revision_id);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 2);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_pending_plan_reconciles_after_restart_without_replay_and_then_credits_funding() {
    let mut pending_fill = Fill::partial();
    pending_fill.pending = true;
    let venue = Venue::new(vec![pending_fill]);
    let approvals = approve(ApprovalMode::Accept);
    let prices = prices();
    let directory = tempfile::tempdir().unwrap();
    let database = directory
        .path()
        .join("SYNTHETIC-plan-pending-restart.sqlite");

    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-pending-restart-approval".into();
    request.plan.steps[0].completion = FinancialStepCompletion {
        principal_spend: None,
        minimum_net_receipt: Some(resource_amount("ETH", "0.004")),
        operation_succeeded: false,
    };
    let engine = make_engine(
        venue.clone(),
        prices.clone(),
        approvals.clone(),
        Store::open(&database).unwrap(),
    );
    let approved = engine.authorize_financial_plan(request).await.unwrap();
    let original = plan_operation_request("SYNTHETIC-plan-pending-restart-attempt", "BUY", "0.02");
    let pending = engine
        .execute_financial_plan_step(&approved.plan_id, "buy-eth", original.clone())
        .await
        .unwrap();
    assert_eq!(pending.status, Status::Pending, "{pending:?}");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);

    let before_reconciliation = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert!(before_reconciliation.steps[0].verified_receipts.is_empty());
    assert!(before_reconciliation.usage.verified_proceeds.is_empty());
    assert_eq!(before_reconciliation.next_eligible_step_id, None);

    // Revocation blocks future dispatch while preserving the submitted order
    // for reconciliation. Reopening the same SQLite journal must retain the
    // original plan and attempt IDs and must not prompt or send another write.
    let revoked = engine
        .revoke_financial_plan(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(
        revoked.revision.status,
        fin_domain::financial_plans::FinancialPlanAuthorizationStatus::Revoked
    );
    drop(engine);

    let restarted = make_engine(
        venue.clone(),
        prices,
        approvals.clone(),
        Store::open(&database).unwrap(),
    );
    let retry = restarted
        .execute_financial_plan_step(&approved.plan_id, "buy-eth", original)
        .await
        .unwrap();
    assert_eq!(retry.id, pending.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);

    // The fixture now reports that the originally submitted partial order is
    // terminal. Status reconciles its exact order/trades and only then records
    // verified proceeds for the dependent SELL step.
    venue
        .state
        .lock()
        .unwrap()
        .orders
        .values_mut()
        .next()
        .unwrap()
        .row["status"] = json!("CANCELED");
    let reconciled = restarted
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(
        reconciled.revision.status,
        fin_domain::financial_plans::FinancialPlanAuthorizationStatus::Revoked
    );
    assert_eq!(reconciled.steps[0].attempts.len(), 1);
    assert_eq!(reconciled.steps[0].attempts[0].attempt_id, pending.id);
    assert_eq!(
        reconciled.steps[0].attempts[0].request_id,
        "SYNTHETIC-plan-pending-restart-attempt"
    );
    assert!(!reconciled.steps[0].verified_receipts.is_empty());
    assert_eq!(
        reconciled.usage.verified_proceeds[0]
            .available
            .to_decimal(36),
        "0.00999"
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_review_hash_rejects_a_renamed_plan_before_authorization() {
    let venue = Venue::new(vec![]);
    let approvals = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        approvals.clone(),
        Store::in_memory().unwrap(),
    );
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-renamed-binding".into();
    let original_review = engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    request.expected_review_hash = Some(original_review.review_hash);
    request.plan.name = "Renamed after review".into();

    let result = engine.authorize_financial_plan(request).await;
    assert!(
        result.is_err(),
        "renamed plan retained a stale approval binding: {result:?}"
    );
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 0);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn protected_policy_still_blocks_a_plan_after_its_approval() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let approvals = approve(ApprovalMode::Accept);
    let policy_prices = prices();
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), policy_prices.clone())],
        approvals.clone(),
        EngineOptions {
            max_movement_and_cost_usd_micros: "10000000".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(policy_prices);
    let mut request = plan_request();
    request.request_id = "SYNTHETIC-plan-policy-approval".into();
    let approved = engine.authorize_financial_plan(request).await.unwrap();
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);

    let attempt = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "buy-eth",
            plan_operation_request("SYNTHETIC-plan-policy-blocked-buy", "BUY", "0.02"),
        )
        .await
        .unwrap();
    assert_eq!(attempt.status, Status::Failed);
    assert_eq!(
        attempt.error.as_ref().map(|error| error.code.as_str()),
        Some("MOVEMENT_LIMIT")
    );
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert_eq!(approvals.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spot_plan_permission_rejects_unsupported_operation_and_out_of_scope_input() {
    let venue = Venue::new(vec![]);
    let adapter = adapter(venue.clone(), prices());
    let error = adapter
        .prepare_plan_permission(
            "binance.futures.order-place",
            &input("SYNTHETIC-plan-futures", "BUY", true),
            &variations("BUY"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "PLAN_PERMISSION_UNSUPPORTED");
    assert!(venue.seen.lock().unwrap().is_empty());

    let error = adapter
        .prepare_plan_permission(
            "connections.remove",
            &json!({"request_id":"SYNTHETIC-plan-admin","name":"synthetic-main"}),
            &variations("BUY"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "UNSUPPORTED_OPERATION");
    assert!(venue.seen.lock().unwrap().is_empty());

    let mut raw = input("SYNTHETIC-plan-out-of-scope", "BUY", false);
    raw["limit_price"] = json!("2200");
    let error = adapter
        .prepare_plan_permission("binance.spot.order-place", &raw, &variations("BUY"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "INVALID_INPUT");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}
