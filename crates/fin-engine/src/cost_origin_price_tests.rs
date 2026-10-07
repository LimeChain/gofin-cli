use super::*;
use async_trait::async_trait;

fn resource(value: &str) -> ResourceId {
    ResourceId(value.to_owned())
}

fn asset(network: &str, reference: &str, decimals: u8) -> fin_domain::AssetId {
    fin_domain::AssetId {
        network: network.into(),
        reference: reference.into(),
        decimals,
    }
}

fn quantity(value: &str) -> fin_domain::Quantity {
    fin_domain::Quantity::new(value).unwrap()
}

fn price(asset: fin_domain::AssetId, observed_at: String) -> AssetPrice {
    AssetPrice {
        unit_atoms: quantity(&BigUint::from(10u8).pow(asset.decimals.into()).to_string()),
        asset,
        usd_micros_per_unit: quantity("2000000"),
        observed_at,
        source: "synthetic exact-price fixture".into(),
    }
}

struct FixturePriceProvider {
    prices: BTreeMap<fin_domain::AssetId, AssetPrice>,
    requested: Mutex<Vec<fin_domain::AssetId>>,
}

#[async_trait]
impl TrustedAssetPriceProvider for FixturePriceProvider {
    async fn asset_price(&self, requested: &fin_domain::AssetId) -> Result<AssetPrice> {
        self.requested.lock().unwrap().push(requested.clone());
        self.prices.get(requested).cloned().ok_or_else(|| {
            FinError::new(
                "FIXTURE_PRICE_MISSING",
                "budget",
                "Synthetic price is unavailable.",
                "Use a configured synthetic price fixture.",
            )
        })
    }
}

struct NoopAuthorizer;

#[async_trait]
impl fin_domain::Authorizer for NoopAuthorizer {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Err(FinError::new(
            "FIXTURE_AUTH_UNAVAILABLE",
            "approval",
            "The price-only test has no authorizer.",
            "Use a test-specific approval fixture.",
        ))
    }
}

struct CountingAuthorizer(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl fin_domain::Authorizer for CountingAuthorizer {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(FinError::new(
            "UNEXPECTED_FIXTURE_AUTH",
            "approval",
            "Authorization should not be reached in this fixture.",
            "Fix the cost-origin price guard ordering.",
        ))
    }
}

struct CountingAdapter {
    prepared: PreparedOperation,
    submits: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Adapter for CountingAdapter {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        vec![fin_domain::Definition {
            name: self.prepared.operation.clone(),
            description: "Synthetic cost-origin guard fixture.".into(),
            kind: fin_domain::OperationKind::Write,
            execution_actor: fin_domain::ExecutionActor::HumanInteractive,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic test only".into(),
            completion: "synthetic receipt".into(),
            semantics: json!({}),
        }]
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Ok(self.prepared.clone())
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.submits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(FinError::new(
            "UNEXPECTED_FIXTURE_SUBMIT",
            "submission",
            "Submission should not be reached in this fixture.",
            "Fix the cost-origin price guard ordering.",
        ))
    }

    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(FinError::new(
            "UNEXPECTED_FIXTURE_RECONCILE",
            "reconciliation",
            "Reconciliation should not be reached in this fixture.",
            "Fix the cost-origin price guard ordering.",
        ))
    }
}

fn test_engine(path: &std::path::Path) -> Engine {
    Engine {
        store: Arc::new(Mutex::new(Store::open(path).unwrap())),
        adapters: RwLock::new(HashMap::new()),
        authorizer: Arc::new(NoopAuthorizer),
        options: EngineOptions::default(),
        cost_origin_price_provider: None,
        write_queue: AsyncMutex::new(()),
        instance_id: uuid::Uuid::new_v4(),
    }
}

fn selected_cross_asset_run(
    source_asset: &fin_domain::AssetId,
    current_asset: &fin_domain::AssetId,
) -> RunLedger {
    let source_resource = resource("synthetic-wallet");
    let source_holding = ResourceAmount {
        resource: source_resource.clone(),
        amount: Amount {
            asset: source_asset.clone(),
            atoms: quantity("1000000000000000000"),
        },
    };
    let baseline = StateSnapshot {
        state: fin_domain::FinancialState {
            holdings: vec![source_holding.clone()],
            ..Default::default()
        },
        observed_at: Utc::now().to_rfc3339(),
        complete: true,
        coverage: vec![source_resource.0.clone()],
        pending: vec![],
    };
    let allocation_origin = format!(
        "allocation:{}",
        resource_asset_key(&source_holding).unwrap()
    );
    RunLedger {
        purpose: None,

        max_movement_usd_micros: legacy_movement_limit(),
        id: "synthetic-provenance".into(),
        status: RunStatus::Active,
        baseline,
        initial_allocation_usd_micros: "1000000".into(),
        borrowed_proceeds_reserve_usd_micros: "0".into(),
        return_cost_reserve_usd_micros: "1000".into(),
        planned_operations: vec![],
        unwind: vec![],
        route_proofs: vec![],
        asset_prices: vec![],
        settled_route_steps: vec![],
        route_invalidated: false,
        legacy_unwind_steps: vec![],
        legacy_return_routes_verified: false,
        allocations: vec![RunAllocation {
            holding: source_holding,
            value_usd_micros: "1000000".into(),
            observed_at: Utc::now().to_rfc3339(),
            source: "synthetic fixed-price fixture".into(),
        }],
        available: vec![],
        reservations: BTreeMap::new(),
        costs: vec![],
        spent_cost_usd_micros: "0".into(),
        borrowed_allocation_used_usd_micros: "0".into(),
        final_state: None,
        final_observed_at: None,
        differences: BTreeMap::new(),
        provenance: vec![ProvenanceLot {
            origin: allocation_origin,
            location: AccountingLocation::Cash {
                resource: source_resource,
            },
            asset: current_asset.clone(),
            atoms: "10500000".into(),
        }],
        provenance_events: vec![],
        supply_accruals: vec![],
        position_anchors: vec![],
        route_revisions: vec![],
        net_conversion_deltas: vec![],
    }
}

fn prepared_cost(fee_asset: fin_domain::AssetId) -> PreparedOperation {
    let source_resource = resource("synthetic-wallet");
    PreparedOperation {
        operation: "synthetic.transfer".into(),
        input: json!({}),
        binding: json!({"synthetic":true}),
        summary: "synthetic separately charged cost".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
        valuation: Some(fin_domain::Valuation {
            movement_usd_micros: quantity("1000000"),
            maximum_fee_usd_micros: quantity("1000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: Utc::now().to_rfc3339(),
            source: "synthetic valuation fixture".into(),
            prices: vec![price(fee_asset.clone(), Utc::now().to_rfc3339())],
        }),
        intent: FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source_resource.clone(),
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: quantity("10000000"),
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource,
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: quantity("500000"),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![],
        },
        operation_version: 1,
    }
}

#[tokio::test]
async fn cross_asset_cost_prices_only_the_selected_origin() {
    let directory = tempfile::tempdir().unwrap();
    let source_asset = asset("ethereum:1", "WETH", 18);
    let fee_asset = asset("ethereum:1", "USDC", 6);
    let engine = test_engine(&directory.path().join("engine.sqlite3"));
    engine
        .save_run(&selected_cross_asset_run(&source_asset, &fee_asset))
        .unwrap();
    let prepared = prepared_cost(fee_asset);
    let required = engine
        .cost_origin_assets(&prepared, Some("synthetic-provenance"))
        .unwrap();
    assert_eq!(required, BTreeSet::from([source_asset.clone()]));

    let fixture = Arc::new(FixturePriceProvider {
        prices: BTreeMap::from([(
            source_asset.clone(),
            price(source_asset.clone(), Utc::now().to_rfc3339()),
        )]),
        requested: Mutex::new(vec![]),
    });
    let engine = engine.with_cost_origin_price_provider(fixture.clone());
    let mut prepared = prepared;
    engine
        .complete_cost_origin_prices(&mut prepared, &required)
        .await
        .unwrap();

    assert_eq!(
        *fixture.requested.lock().unwrap(),
        vec![source_asset.clone()]
    );
    let prices = &prepared.valuation.unwrap().prices;
    assert!(prices.iter().any(|row| row.asset == source_asset));
    assert!(
        prices
            .iter()
            .any(|row| row.asset == asset("ethereum:1", "USDC", 6))
    );
}

#[test]
fn venue_precision_origin_is_kept_separate_from_native_cost_asset() {
    let directory = tempfile::tempdir().unwrap();
    let venue_eth = asset("binance", "ETH", 36);
    let native_eth = asset("ethereum:1", "native", 18);
    let engine = test_engine(&directory.path().join("engine.sqlite3"));
    engine
        .save_run(&selected_cross_asset_run(&venue_eth, &native_eth))
        .unwrap();
    let prepared = prepared_cost(native_eth);

    let required = engine
        .cost_origin_assets(&prepared, Some("synthetic-provenance"))
        .unwrap();
    assert_eq!(required, BTreeSet::from([venue_eth]));
}

#[tokio::test]
async fn missing_stale_and_mismatched_cost_origin_prices_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let source_asset = asset("binance", "ETH", 36);
    let fee_asset = asset("ethereum:1", "native", 18);
    let engine = test_engine(&directory.path().join("engine.sqlite3"));
    let required = BTreeSet::from([source_asset.clone()]);
    let mut prepared = prepared_cost(fee_asset);

    assert_eq!(
        engine
            .complete_cost_origin_prices(&mut prepared.clone(), &required)
            .await
            .unwrap_err()
            .code,
        "COST_ORIGIN_PRICE_UNAVAILABLE"
    );

    let mut stale = price(
        source_asset.clone(),
        (Utc::now() - chrono::Duration::seconds(121)).to_rfc3339(),
    );
    stale.source = "synthetic stale fixture".into();
    let stale_engine = test_engine(&directory.path().join("stale.sqlite3"))
        .with_cost_origin_price_provider(Arc::new(FixturePriceProvider {
            prices: BTreeMap::from([(source_asset.clone(), stale)]),
            requested: Mutex::new(vec![]),
        }));
    assert_eq!(
        stale_engine
            .complete_cost_origin_prices(&mut prepared.clone(), &required)
            .await
            .unwrap_err()
            .code,
        "COST_ORIGIN_PRICE_STALE"
    );

    let wrong = asset("ethereum:1", "DAI", 18);
    let wrong_engine = test_engine(&directory.path().join("wrong.sqlite3"))
        .with_cost_origin_price_provider(Arc::new(FixturePriceProvider {
            prices: BTreeMap::from([(source_asset, price(wrong, Utc::now().to_rfc3339()))]),
            requested: Mutex::new(vec![]),
        }));
    assert_eq!(
        wrong_engine
            .complete_cost_origin_prices(&mut prepared, &required)
            .await
            .unwrap_err()
            .code,
        "COST_ORIGIN_PRICE_INVALID"
    );
}

#[tokio::test]
async fn unavailable_origin_price_blocks_before_authorization_and_submission() {
    let directory = tempfile::tempdir().unwrap();
    let source_asset = asset("ethereum:1", "WETH", 18);
    let fee_asset = asset("ethereum:1", "USDC", 6);
    let prepared = prepared_cost(fee_asset.clone());
    let authorizations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine = Engine::new(
        Store::open(directory.path().join("engine.sqlite3")).unwrap(),
        vec![Arc::new(CountingAdapter {
            prepared,
            submits: submissions.clone(),
        })],
        Arc::new(CountingAuthorizer(authorizations.clone())),
        EngineOptions::default(),
    )
    .unwrap();
    let run = selected_cross_asset_run(&source_asset, &fee_asset);
    engine.save_run(&run).unwrap();

    let record = engine
        .execute(OperationRequest {
            request_id: "synthetic-cost-origin-request".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: Some(run.id),
        })
        .await
        .unwrap();
    assert_eq!(
        record.error.as_ref().unwrap().code,
        "COST_ORIGIN_PRICE_UNAVAILABLE"
    );
    assert_eq!(authorizations.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(submissions.load(std::sync::atomic::Ordering::SeqCst), 0);
}
