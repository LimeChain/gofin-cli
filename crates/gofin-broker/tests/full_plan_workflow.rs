//! One bounded plan through real Kraken, Ethereum/Aave, bridge and Hyperliquid adapters.
//! All credentials, venue responses and RPC evidence are controlled local fixtures.
//! This is adapter/broker evidence, not a live trade or deployed-contract fork test.
use async_trait::async_trait;
use chrono::Timelike;
use gofin_broker::{
    BridgeAdapter, BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies,
    SharedUsdValuator, compose_runtime_with_clock,
};
use gofin_domain::{financial_plans::*, *};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{EvmRuntime, LocalEnvelopeSigner, alloy::primitives::U256};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "../../gofin-ethereum/tests/support/aave_mcp_fixture.rs"]
mod aave_mcp_fixture;
#[allow(dead_code)]
#[path = "support/bridge_erc20_fixture.rs"]
mod bridge_fixture;
#[path = "support/full_plan_evm.rs"]
mod full_plan_evm;
#[path = "support/full_plan_exact_bridge_restart.rs"]
mod full_plan_exact_bridge_restart;
#[path = "support/full_plan_hyperliquid.rs"]
mod full_plan_hyperliquid;
#[path = "support/full_plan_quote_recovery.rs"]
mod full_plan_quote_recovery;
#[path = "support/full_plan_rpc_failover.rs"]
mod full_plan_rpc_failover;
#[path = "support/full_plan_venue.rs"]
mod full_plan_venue;

struct Approval(AtomicUsize, AtomicUsize, AtomicUsize, AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let source = if c.purpose == "operation_resolution" {
            gofin_authorization::validate_challenge(c, &now()).unwrap();
            assert_eq!(c.binding["action"], "operation_resolution");
            self.3.fetch_add(1, Ordering::SeqCst);
            Some(AuthorizationSource::TouchId)
        } else if c.purpose == "reservation_override" {
            assert_eq!(c.binding["action"], "reservation_override");
            self.1.fetch_add(1, Ordering::SeqCst);
            // Controlled authorizer evidence only; no native Touch ID is invoked.
            Some(AuthorizationSource::TouchId)
        } else if c.binding["plan"].is_object() {
            assert_eq!(c.binding["plan"]["steps"].as_array().unwrap().len(), 10);
            self.0.fetch_add(1, Ordering::SeqCst);
            None
        } else {
            self.2.fetch_add(1, Ordering::SeqCst);
            None
        };
        Ok(ApprovalAudit {
            mechanism: "controlled full-plan fixture".into(),
            source,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Prices {
    frozen_precision: Option<chrono::DateTime<chrono::Utc>>,
}
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        let usd = match a.reference.as_str() {
            "native" | "XETH" => "2000",
            "SOL" => "100",
            s if s.eq_ignore_ascii_case(full_plan_evm::WETH) => "2000",
            _ => "1",
        };
        let mut quote = gofin_pricing::UsdQuote {
            usd_per_unit: usd.into(),
            updated_at: now(),
            observed_at: now(),
            source: "controlled independent prices".into(),
        };
        if let Some(at) = self.frozen_precision {
            quote.updated_at = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            quote.observed_at = quote.updated_at.clone();
            if a.reference.eq_ignore_ascii_case(bridge_fixture::ETH_USDC)
                || a.reference.eq_ignore_ascii_case(bridge_fixture::ARB_USDC)
                || a.reference == "USDC"
            {
                let updated = at - chrono::Duration::seconds(47);
                let retrieved = at + chrono::Duration::microseconds(547);
                quote.updated_at = updated.to_rfc3339();
                quote.observed_at = retrieved.to_rfc3339();
                quote.source = format!(
                    "Stablecoin/USD conversion; exchange=coinbase; exact_asset=USDC; controlled prices; updated:{}; conversion retrieval; updated:{}",
                    quote.updated_at, quote.observed_at,
                );
            }
        }
        Ok(quote)
    }
}
fn asset(network: &str, reference: &str, decimals: u8) -> AssetId {
    AssetId {
        network: network.into(),
        reference: reference.into(),
        decimals,
    }
}
fn amount(resource: &str, asset: &AssetId, decimal: &str) -> ResourceAmount {
    ResourceAmount {
        resource: ResourceId(resource.into()),
        amount: Amount {
            asset: asset.clone(),
            atoms: Quantity::from_decimal(decimal, asset.decimals).unwrap(),
        },
    }
}
fn fee(resource: &str, asset: &AssetId, decimal: &str, treatment: CostTreatment) -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: ResourceId(resource.into()),
        },
        amount: Amount {
            asset: asset.clone(),
            atoms: Quantity::from_decimal(decimal, asset.decimals).unwrap(),
        },
        treatment,
    }
}
fn dep(id: &str, resource: &str, asset: &AssetId) -> FinancialPlanFundingDependency {
    FinancialPlanFundingDependency {
        producing_step_id: id.into(),
        resource: ResourceId(resource.into()),
        asset: asset.clone(),
        funding_scope: Some(FinancialPlanFundingScope::PrincipalOnly),
        maximum_fraction_bps: None,
    }
}
fn step(
    id: &str,
    operation: &str,
    input: Value,
    debits: Vec<ResourceAmount>,
    costs: Vec<CostBound>,
    credits: Value,
    deps: Vec<FinancialPlanFundingDependency>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: id.into(),
        operation: operation.into(),
        input,
        permitted_variations: json!({"mode":"exact","maximum_debits":debits,"maximum_costs":costs,"possible_credits":credits}),
        funding_dependencies: deps,
        action_deadline_seconds: None,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}
fn evm_input(id: &str, extra: Value) -> Value {
    let mut v = json!({"wallet":"synthetic-wallet","network":"evm","chain_id":1,"request_id":id,"max_fee_eth":"0.001"});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}
include!("support/kraken_plan_client.rs");
fn plan_args(request: &FinancialPlanAuthorizationRequest, authorize: bool) -> Vec<String> {
    let mut argv = args(&[
        "plans",
        if authorize { "authorize" } else { "preview" },
        "--input",
        &serde_json::to_string(&request.plan).unwrap(),
        "--json",
    ]);
    if authorize {
        argv.extend(args(&[
            "--id",
            &request.request_id,
            "--review-hash",
            request.expected_review_hash.as_ref().unwrap(),
        ]));
    }
    if let Some(seconds) = request.valid_for_seconds {
        argv.extend(args(&["--valid-for", &format!("{seconds}s")]));
    }
    if let Some(revision) = &request.replaces_revision_id {
        argv.extend(args(&["--replaces", revision]));
    }
    argv
}
async fn call(client: &Client, request: BrokerRequest) -> Value {
    let argv = match &request {
        BrokerRequest::Read { operation, input } => {
            args(&["read", operation, "--input", &input.to_string(), "--json"])
        }
        BrokerRequest::PreviewPlan { request } => plan_args(request, false),
        BrokerRequest::AuthorizePlan { request } => plan_args(request, true),
        BrokerRequest::ExecutePlanStep {
            plan_id,
            step_id,
            request,
        } => args(&[
            "execute",
            &request.operation,
            "--input",
            &request.input.to_string(),
            "--id",
            &request.request_id,
            "--plan",
            plan_id,
            "--step",
            step_id,
            "--json",
        ]),
        BrokerRequest::Execute { request } => args(&[
            "execute",
            &request.operation,
            "--input",
            &request.input.to_string(),
            "--id",
            &request.request_id,
            "--json",
        ]),
        BrokerRequest::PreviewReservationRelease { operation_id } => {
            args(&["reservations", "preview", operation_id, "--json"])
        }
        BrokerRequest::ReleaseReservation {
            operation_id,
            request_id,
            review_hash,
        } => args(&[
            "reservations",
            "release",
            operation_id,
            "--id",
            request_id,
            "--review-hash",
            review_hash,
            "--json",
        ]),
        BrokerRequest::PreviewOperationResolution {
            operation_id,
            input,
        } => args(&[
            "operations",
            "resolve-preview",
            operation_id,
            "--input",
            &serde_json::to_string(input).unwrap(),
            "--json",
        ]),
        BrokerRequest::ResolveOperation {
            operation_id,
            request_id,
            review_hash,
            input,
        } => args(&[
            "operations",
            "force-resolve",
            operation_id,
            "--input",
            &serde_json::to_string(input).unwrap(),
            "--id",
            request_id,
            "--review-hash",
            review_hash,
            "--json",
        ]),
        BrokerRequest::Status { id } => args(&["status", id, "--json"]),
        BrokerRequest::PlanStatus { plan_id } => args(&["plans", "status", plan_id, "--json"]),
        _ => panic!("Unexpected request"),
    };
    client.call(argv, request, true).await
}

#[test]
fn kraken_aave_bridge_hyperliquid_complete_under_one_plan() {
    run(false, false, false, false, false, false, false);
}
#[test]
fn kraken_aave_bridge_hyperliquid_recovers_lost_sell_response_under_same_plan() {
    run(true, false, false, false, false, false, false);
}
#[test]
fn kraken_aave_bridge_hyperliquid_holds_unknown_sell_without_downstream_dispatch() {
    run(true, true, false, false, false, false, false);
}
#[test]
fn historical_lock_override_preserves_uncertainty_and_allows_one_new_plan() {
    run(false, false, true, false, false, false, false);
}
#[test]
fn funding_v1_lower_final_fee_completes_original_withdrawal_and_downstream_plan() {
    run(false, false, false, true, false, false, false);
}
#[test]
fn human_attested_pending_withdrawal_completes_one_plan_and_late_proof_never_double_credits() {
    run(false, false, false, true, true, false, false);
}
#[test]
fn frozen_conversion_precision_and_expired_bridge_quote_recover_under_original_plan() {
    run(false, false, false, false, false, true, false);
}
#[test]
fn saved_plan_bridge_code_read_fails_over_to_validated_rpc_without_new_approval() {
    run(false, false, false, false, false, false, true);
}
fn run(
    lose_response: bool,
    unresolved: bool,
    override_historical: bool,
    funding_v1: bool,
    manual_resolution: bool,
    quote_recovery: bool,
    rpc_failover: bool,
) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                // Public CLI requests run on workers with the broker's stack limit.
                .thread_stack_size(2 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse(
                    lose_response,
                    unresolved,
                    override_historical,
                    funding_v1,
                    manual_resolution,
                    quote_recovery,
                    rpc_failover,
                ))
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn rehearse(
    lose_response: bool,
    unresolved: bool,
    override_historical: bool,
    funding_v1: bool,
    manual_resolution: bool,
    quote_recovery: bool,
    rpc_failover: bool,
) {
    let temp = tempfile::tempdir().unwrap();
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let wallet = gofin_evm::lower_address(signer.address());
    let src = format!("ethereum:1:{wallet}");
    let dst = format!("ethereum:42161:{wallet}");
    let source_bridge = Arc::new(bridge_fixture::Rpc::source_with_allowance(
        wallet.clone(),
        U256::ZERO,
    ));
    *source_bridge.token_balance.lock().unwrap() = U256::ZERO;
    let source = Arc::new(full_plan_evm::WorkflowEvm::new(
        wallet.clone(),
        source_bridge.clone(),
    ));
    let destination = Arc::new(bridge_fixture::Rpc::destination_on(wallet.clone(), 42161));
    *destination.token_balance.lock().unwrap() = U256::ZERO;
    let hl = full_plan_hyperliquid::Fixture::new(wallet.clone(), destination.clone());
    let rpc_pool = if rpc_failover {
        Some(full_plan_rpc_failover::Pool::new(source.clone()).await)
    } else {
        None
    };
    let source_rpc: Arc<dyn gofin_evm::EvmRpc> = match &rpc_pool {
        Some(pool) => pool.rpc.clone(),
        None => source.clone(),
    };
    let source_runtime = Arc::new(EvmRuntime::new(source_rpc, 1, Some(signer.clone())));
    let destination_runtime = Arc::new(EvmRuntime::new(hl.clone(), 42161, Some(signer)));
    let frozen_precision = quote_recovery.then(|| {
        (chrono::Utc::now() - chrono::Duration::seconds(1))
            .with_nanosecond(265_000_000)
            .unwrap()
    });
    let price_source = Arc::new(Prices { frozen_precision });
    let valuator = Arc::new(SharedUsdValuator::with_source(price_source.clone()));
    let connection = vec![WalletConnection {
        name: "synthetic-wallet".into(),
        address: wallet.clone(),
        signing_enabled: true,
    }];
    let source_wallet = Arc::new(
        EthereumAdapter::new(
            full_plan_evm::config(),
            connection.clone(),
            source_runtime.clone(),
        )
        .unwrap()
        .with_aave_mcp(gofin_ethereum::aave_mcp::AaveMcpClient::with_transport(
            aave_mcp_fixture::ForkAaveMcp::new(),
        ))
        .with_wallet_valuation(valuator.clone()),
    );
    let mut destination_config =
        EthereumConfig::wallet_only(42161, 1, vec![bridge_fixture::ARB_USDC.into()]);
    destination_config.require_finalized = false;
    let destination_wallet = Arc::new(
        EthereumAdapter::new(destination_config, connection, destination_runtime.clone())
            .unwrap()
            .with_wallet_valuation(valuator.clone()),
    );
    let venue = Arc::new(full_plan_venue::Venue::new(
        wallet.clone(),
        source.clone(),
        source_wallet.clone(),
    ));
    venue.lose_response.store(lose_response, Ordering::SeqCst);
    venue.hide_history.store(unresolved, Ordering::SeqCst);
    venue
        .historical_unknown
        .store(override_historical, Ordering::SeqCst);
    venue.funding_v1.store(funding_v1, Ordering::SeqCst);
    let withdrawal_operation = if funding_v1 {
        "kraken.create-funding-withdrawal-v1"
    } else {
        "kraken.withdraw-funds"
    };
    let kraken = full_plan_venue::adapter(venue.clone(), price_source);
    let mut withdrawal_definition = kraken
        .definitions()
        .into_iter()
        .find(|definition| definition.name == withdrawal_operation)
        .unwrap();
    assert!(
        withdrawal_definition.semantics["withdrawal_discovery"].is_object(),
        "the executing adapter must expose the updated discovery guidance"
    );
    withdrawal_definition
        .semantics
        .as_object_mut()
        .unwrap()
        .remove("withdrawal_discovery");
    // Independently reproduce the saved contract hash from before guidance was
    // added; execution below uses the actual current adapter throughout.
    let pre_guidance_withdrawal_hash = binding_hash(&json!({
        "operation":withdrawal_definition.name,
        "kind":withdrawal_definition.kind,
        "execution_actor":withdrawal_definition.execution_actor,
        "input_schema":withdrawal_definition.input_schema,
        "semantics":withdrawal_definition.semantics,
    }))
    .unwrap();
    let profile = Arc::new(hl.profile());
    let native_hl = Arc::new(gofin_exchange::ExchangeAdapter::new(
        profile.without_operation("hyperliquid.account.deposit"),
    ));
    let deposit_hl = Arc::new(gofin_broker::HyperliquidEthereumDepositAdapter::new(
        profile,
        destination_wallet.clone(),
        hl.clone(),
    ));
    let wallets = BTreeMap::from([
        (1, source_wallet.clone()),
        (42161, destination_wallet.clone()),
    ]);
    let provider = Arc::new(full_plan_quote_recovery::Provider {
        inner: bridge_fixture::Erc20Provider::with_destination_token_receipt(
            destination.clone(),
            bridge_fixture::DestinationPayout::DirectWithHistoricalReads,
            Quantity::from_u64(11_600_000),
        ),
        expire_first: quote_recovery,
        calls: AtomicUsize::new(0),
    });
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            provider.clone(),
            wallets.clone(),
            BTreeMap::from([(1, source_runtime), (42161, destination_runtime)]),
            valuator.clone(),
        )
        .unwrap(),
    );
    let approvals = Arc::new(Approval(
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ));
    let runtime = compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy {
                max_run_allocation_usd_micros: "300000000".into(),
                ..BrokerPolicy::beta()
            },
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(temp.path().join("journal.sqlite")).unwrap(),
            adapters: vec![
                kraken,
                Arc::new(gofin_broker::EvmNetworkRouter::new(wallets)),
                bridge.clone(),
                native_hl,
                deposit_hl,
            ],
            authorizer: approvals.clone(),
            valuator,
            bridge: Some(bridge),
        },
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let client = Client::new(runtime.clone(), temp.path()).await;
    let native = asset("ethereum:1", "native", 18);
    let arb_native = asset("ethereum:42161", "native", 18);
    let weth = asset("ethereum:1", full_plan_evm::WETH, 18);
    let usdc = asset("ethereum:1", bridge_fixture::ETH_USDC, 6);
    let arb_usdc = asset("ethereum:42161", bridge_fixture::ARB_USDC, 6);
    let eth = asset("kraken", "XETH", 36);
    let sol = asset("kraken", "SOL", 36);
    let gas = fee(&src, &native, "0.001", CostTreatment::AdditionalDebit);
    let sell = json!({"requestId":"FULL-SELL","account":"synthetic-spot","pair":"SOLETH","type":"sell","ordertype":"limit","volume":"0.3","price":"0.05","maximumFees":{"SOL":"0.0012","XETH":"0.00007"},"minimumReceive":"0.01493","timeinforce":"IOC"});
    let withdraw = if funding_v1 {
        json!({"requestId":"FULL-WITHDRAW","account":"synthetic-spot","scope":{"method_id":full_plan_venue::METHOD},"address_id":full_plan_venue::ADDRESS_ID,"amount":{"asset_amount":{"asset":{"class":"currency","name":"XETH"},"amount":"0.0148"}},"fee":{"fee_included":false,"current_fee":{}},"maxFee":"0.0001","destination":src,"verificationRequirement":"destination_receipt"})
    } else {
        json!({"requestId":"FULL-WITHDRAW","account":"synthetic-spot","asset":"XETH","key":"synthetic-ethereum","amount":"0.0149","max_fee":"0.0001","destination":src})
    };
    let mut steps = vec![
        step(
            "sell",
            "kraken.add-order",
            sell,
            vec![amount(full_plan_venue::ACCOUNT, &sol, "0.3")],
            vec![
                fee(
                    full_plan_venue::ACCOUNT,
                    &sol,
                    "0.0012",
                    CostTreatment::AdditionalDebit,
                ),
                fee(
                    full_plan_venue::ACCOUNT,
                    &eth,
                    "0.00007",
                    CostTreatment::DeductedFromCredit,
                ),
            ],
            json!([{"resource":full_plan_venue::ACCOUNT,"asset":eth}]),
            vec![],
        ),
        step(
            "withdraw",
            withdrawal_operation,
            withdraw,
            vec![amount(full_plan_venue::ACCOUNT, &eth, "0.0149")],
            vec![fee(
                full_plan_venue::ACCOUNT,
                &eth,
                "0.0001",
                CostTreatment::IncludedInDebit,
            )],
            json!([{"resource":src,"asset":native}]),
            vec![dep("sell", full_plan_venue::ACCOUNT, &eth)],
        ),
        step(
            "wrap",
            "wallet.wrap",
            evm_input("FULL-WRAP", json!({"amount":"0.0145"})),
            vec![amount(&src, &native, "0.0145")],
            vec![gas.clone()],
            json!([{"resource":src,"asset":weth}]),
            vec![dep("withdraw", &src, &native)],
        ),
        step(
            "approve-aave",
            "wallet.approve",
            evm_input(
                "FULL-APPROVE",
                json!({"token":full_plan_evm::WETH,"spender":full_plan_evm::POOL,"amount":"0.0145"}),
            ),
            vec![],
            vec![gas.clone()],
            json!([]),
            vec![],
        ),
        step(
            "supply",
            "aave.supply",
            evm_input(
                "FULL-SUPPLY",
                json!({"market":full_plan_evm::POOL,"token":full_plan_evm::WETH,"amount":"0.0145"}),
            ),
            vec![amount(&src, &weth, "0.0145")],
            vec![gas.clone()],
            json!([]),
            vec![dep("wrap", &src, &weth)],
        ),
        step(
            "borrow",
            "aave.borrow",
            evm_input(
                "FULL-BORROW",
                json!({"market":full_plan_evm::POOL,"token":bridge_fixture::ETH_USDC,"amount":"11.7","min_health_factor":"2","max_debt_usd":"11.700003","max_debt_to_collateral_bps":5000}),
            ),
            vec![],
            vec![gas.clone()],
            json!([{"resource":src,"asset":usdc}]),
            vec![],
        ),
    ];
    let bridge_bounds = json!({"input":{"minimum":"11700000","maximum":"11700000"},"maximum_source_debit":"11700000","minimum_net_receipt":{"asset":arb_usdc,"atoms":"9500000"},"fee_caps":[{"asset":native,"kind":"gas","maximum":"2000000000000000","maximum_usd_micros":4000000}],"maximum_total_usd_micros":15700000,"authorization_seconds":1800});
    let bridge_intent = json!({"wallet":"synthetic-wallet","source":{"chainId":1,"token":bridge_fixture::ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":bridge_fixture::ARB_USDC}],"recipient":wallet,"refundTo":wallet,"allowedRefundAssets":[{"chainId":1,"token":bridge_fixture::ETH_USDC}],"amount":{"minimum":"11700000","maximum":"11700000"}});
    let mut bridge_step = step(
        "bridge",
        "bridge.execute",
        json!({"intent":bridge_intent,"allowedProviders":["relay"],"bounds":bridge_bounds}),
        vec![],
        vec![],
        json!([]),
        vec![dep("borrow", &src, &usdc)],
    );
    bridge_step.permitted_variations = json!({"mode":"agent_nominated"});
    bridge_step.completion.minimum_net_receipt = Some(amount(&dst, &arb_usdc, "11.6"));
    steps.push(bridge_step);
    steps[4].permitted_variations =
        json!({"min_amount":"0.0145","max_amount":"0.0145","max_fee_eth":"0.001"});
    steps[5].permitted_variations = json!({"min_amount":"11.7","max_amount":"11.7","max_fee_eth":"0.001","max_cumulative_borrow":"11.700003"});
    let hl_resource = format!("hyperliquid:mainnet:{}:perpetual:primary", wallet);
    let hl_usdc = gofin_hyperliquid::usdc();
    steps.push(step(
        "deposit",
        "hyperliquid.account.deposit",
        full_plan_hyperliquid::deposit_input("11.6"),
        vec![amount(&dst, &arb_usdc, "11.6")],
        vec![fee(
            &dst,
            &arb_native,
            "0.001",
            CostTreatment::AdditionalDebit,
        )],
        json!([{"resource":hl_resource,"asset":hl_usdc}]),
        vec![dep("bridge", &dst, &arb_usdc)],
    ));
    steps.push(step(
        "leverage",
        "hyperliquid.account.set_leverage",
        full_plan_hyperliquid::leverage_input(),
        vec![],
        vec![],
        json!([]),
        vec![],
    ));
    steps.push(step(
        "order",
        "hyperliquid.orders.place",
        full_plan_hyperliquid::order_input(),
        // A zero-loss perpetual order consumes no principal cash. Its real
        // trading fee remains explicitly funded and bounded below.
        vec![],
        vec![fee(
            &hl_resource,
            &hl_usdc,
            "0.1",
            CostTreatment::AdditionalDebit,
        )],
        json!([]),
        vec![dep("deposit", &hl_resource, &hl_usdc)],
    ));
    steps.last_mut().unwrap().funding_dependencies[0].funding_scope = None;
    let mut bounds = FinancialPlanBounds {
        maximum_debits: vec![],
        maximum_costs: vec![],
    };
    let mut manually_resolved = None;
    for s in &steps {
        if let Some(v) = s.permitted_variations.get("maximum_debits") {
            bounds
                .maximum_debits
                .extend(serde_json::from_value::<Vec<ResourceAmount>>(v.clone()).unwrap());
        }
        if let Some(v) = s.permitted_variations.get("maximum_costs") {
            bounds
                .maximum_costs
                .extend(serde_json::from_value::<Vec<CostBound>>(v.clone()).unwrap());
        }
    }
    bounds.maximum_debits.push(amount(&src, &weth, "0.0145"));
    bounds.maximum_costs.extend([gas.clone(), gas.clone()]);
    bounds
        .maximum_costs
        .push(fee(&src, &weth, "0.0145", CostTreatment::IncludedInDebit));
    bounds.maximum_debits.push(amount(&src, &usdc, "11.7"));
    bounds.maximum_costs.extend([gas.clone(), gas.clone()]);
    let mut normalized_costs: Vec<CostBound> = vec![];
    for cost in bounds.maximum_costs {
        if let Some(existing) = normalized_costs.iter_mut().find(|c| {
            c.location == cost.location
                && c.amount.asset == cost.amount.asset
                && c.treatment == cost.treatment
        }) {
            existing.amount.atoms = existing
                .amount
                .atoms
                .checked_add(&cost.amount.atoms)
                .unwrap();
        } else {
            normalized_costs.push(cost);
        }
    }
    bounds.maximum_costs = normalized_costs;
    let mut normalized_debits: Vec<ResourceAmount> = vec![];
    for debit in bounds.maximum_debits {
        if let Some(existing) = normalized_debits
            .iter_mut()
            .find(|d| d.resource == debit.resource && d.amount.asset == debit.amount.asset)
        {
            existing.amount.atoms = existing
                .amount
                .atoms
                .checked_add(&debit.amount.atoms)
                .unwrap();
        } else {
            normalized_debits.push(debit);
        }
    }
    bounds.maximum_debits = normalized_debits;
    let spec=FinancialPlanSpec{version:1,name:"Kraken → Aave → bridge → Hyperliquid".into(),summary:Some("Sell SOL for ETH; supply wrapped ETH to Aave, borrow bounded USDC, fund isolated SOL position.".into()),steps,aggregate_bounds:bounds};
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "FULL-PLAN".into(),
        plan: spec.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    if !lose_response {
        // A derivative's notional is exposure, not a cash principal debit.
        // An invented exact debit ceiling must not make its principal-only
        // funding dependency appear usable during full-plan review.
        let mut malformed = request.clone();
        let order = malformed.plan.steps.last_mut().unwrap();
        assert_eq!(order.step_id, "order");
        let notional = order.input["max_notional_usdc"].as_str().unwrap();
        let fictitious_debit = amount(&hl_resource, &hl_usdc, notional);
        order.permitted_variations["maximum_debits"] = json!([fictitious_debit]);
        order.funding_dependencies[0].funding_scope =
            Some(FinancialPlanFundingScope::PrincipalOnly);
        malformed
            .plan
            .aggregate_bounds
            .maximum_debits
            .push(fictitious_debit);
        let rejected = client
            .call(
                args(&[
                    "plans",
                    "preview",
                    "--input",
                    &serde_json::to_string(&malformed.plan).unwrap(),
                    "--valid-for",
                    "30m",
                    "--json",
                ]),
                BrokerRequest::PreviewPlan { request: malformed },
                false,
            )
            .await;
        let error = rejected.get("error").unwrap_or(&rejected);
        assert_eq!(error["code"], "PLAN_FUNDING_UNUSED", "{rejected}");
        assert_eq!(error["details"]["step_id"], "order", "{rejected}");
        assert_eq!(
            error["details"]["operation"], "hyperliquid.orders.place",
            "{rejected}"
        );
        assert_eq!(error["details"]["before_plan_approval"], true, "{rejected}");

        // Removing the impossible dependency is also a valid review. Keep the
        // executed plan's existing all-debits dependency below: it genuinely
        // funds trading fees and must remain supported.
        let mut no_dependency = request.clone();
        no_dependency
            .plan
            .steps
            .last_mut()
            .unwrap()
            .funding_dependencies
            .clear();
        let corrected = call(
            &client,
            BrokerRequest::PreviewPlan {
                request: no_dependency,
            },
        )
        .await;
        assert!(corrected["review_hash"].is_string(), "{corrected}");
        let mut exact_supply = request.clone();
        exact_supply.plan.steps[4].permitted_variations = json!({
            "mode":"exact",
            "maximum_debits":[amount(&src,&weth,"0.0145")],
            "maximum_costs":[gas.clone()],
            "possible_credits":[]
        });
        let rejected = client
            .call(
                args(&[
                    "plans",
                    "preview",
                    "--input",
                    &serde_json::to_string(&exact_supply.plan).unwrap(),
                    "--valid-for",
                    "30m",
                    "--json",
                ]),
                BrokerRequest::PreviewPlan {
                    request: exact_supply.clone(),
                },
                false,
            )
            .await;
        let error = rejected.get("error").unwrap_or(&rejected);
        assert_eq!(
            error["code"], "AAVE_SUPPLY_COST_SCOPE_INCOMPLETE",
            "{rejected}"
        );
        assert_eq!(error["details"]["step_id"], "supply");
        assert_eq!(error["details"]["before_plan_approval"], true);
        let rounding_cost = error["details"]["required_rounding_cost"].clone();
        assert_eq!(rounding_cost["treatment"], "included_in_debit");
        exact_supply.plan.steps[4].permitted_variations["maximum_costs"]
            .as_array_mut()
            .unwrap()
            .push(rounding_cost);
        call(
            &client,
            BrokerRequest::PreviewPlan {
                request: exact_supply,
            },
        )
        .await;
        assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
        assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
        assert_eq!(source.writes.load(Ordering::SeqCst), 0);
        assert_eq!(source_bridge.broadcasts(), 0);
        assert_eq!(destination.broadcasts(), 0);
        assert_eq!(hl.deposits.load(Ordering::SeqCst), 0);
        assert_eq!(hl.native_dispatches.load(Ordering::SeqCst), 0);
    }
    let historical = if override_historical {
        // Current cash covers the new .3 SOL plus its .0012 fee ceiling, but
        // cannot cover both the old uncertain order and the new order.
        let historical_input = json!({"requestId":"HISTORICAL-UNKNOWN","account":"synthetic-spot","pair":"SOLETH","type":"sell","ordertype":"limit","volume":"0.01","price":"0.05","maximumFees":{"SOL":"0.00004","XETH":"0.000002"},"minimumReceive":"0.000498","timeinforce":"IOC"});
        let original = call(
            &client,
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: "HISTORICAL-UNKNOWN".into(),
                    operation: "kraken.add-order".into(),
                    input: historical_input,
                    run_id: None,
                },
            },
        )
        .await;
        assert_eq!(original["status"], "unknown", "{original}");
        let operation_id = original["id"].as_str().unwrap().to_owned();
        let stored = Store::open(temp.path().join("journal.sqlite")).unwrap();
        let reservations = stored.reservations(&operation_id).unwrap();
        assert!(!reservations.is_empty());
        drop(stored);
        let blocked = client
            .call(
                args(&[
                    "plans",
                    "preview",
                    "--input",
                    &serde_json::to_string(&request.plan).unwrap(),
                    "--valid-for",
                    "30m",
                    "--json",
                ]),
                BrokerRequest::PreviewPlan {
                    request: request.clone(),
                },
                false,
            )
            .await;
        let error = blocked.get("error").unwrap_or(&blocked);
        assert_eq!(error["code"], "RESOURCE_RESERVED", "{blocked}");
        assert_eq!(error["details"]["blocking_operation_id"], operation_id);
        assert_eq!(error["details"]["before_plan_approval"], true);
        assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
        let reviewed = call(
            &client,
            BrokerRequest::PreviewReservationRelease {
                operation_id: operation_id.clone(),
            },
        )
        .await;
        assert!(
            !reviewed["review"]["released_reservations"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{reviewed}"
        );
        let released = call(
            &client,
            BrokerRequest::ReleaseReservation {
                operation_id: operation_id.clone(),
                request_id: "HISTORICAL-LOCK-OVERRIDE".into(),
                review_hash: reviewed["review_hash"].as_str().unwrap().into(),
            },
        )
        .await;
        assert_eq!(released["status"], "released", "{released}");
        assert_eq!(approvals.1.load(Ordering::SeqCst), 1);
        assert_eq!(approvals.2.load(Ordering::SeqCst), 1);
        assert_eq!(
            Store::open(temp.path().join("journal.sqlite"))
                .unwrap()
                .reservations(&operation_id)
                .unwrap(),
            reservations
        );
        assert_eq!(
            venue.writes.load(Ordering::SeqCst),
            1,
            "release is not a financial write"
        );
        Some((original, reservations))
    } else {
        None
    };
    let preview = call(
        &client,
        BrokerRequest::PreviewPlan {
            request: request.clone(),
        },
    )
    .await;
    assert_eq!(
        preview["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|permission| permission["step_id"] == "withdraw")
            .unwrap()["exact"]["definition_hash"],
        pre_guidance_withdrawal_hash,
        "discovery guidance must preserve the original approved withdrawal contract"
    );
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = call(&client, BrokerRequest::AuthorizePlan { request }).await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        usize::from(override_historical)
    );
    let mut completed_prefix: Vec<(String, Value, Value)> = Vec::new();
    for s in &spec.steps {
        // The reviewed bridge step is provider-neutral. Obtain its short-lived
        // executable quote only when funded and eligible, as a real caller does.
        let quotes = if s.step_id == "bridge" {
            call(&client, BrokerRequest::Read {
                operation:"bridge.quotes".into(),
                input:json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":bridge_fixture::ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":bridge_fixture::ARB_USDC}],"recipient":wallet,"refundTo":wallet,"allowedRefundAssets":[{"chainId":1,"token":bridge_fixture::ETH_USDC}],"amount":"11700000"}}),
            }).await
        } else {
            Value::Null
        };
        let transfer = quotes["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap_or_default();
        let original_transfer = if quote_recovery && s.step_id == "bridge" {
            assert_eq!(completed_prefix.len(), 6);
            let stored = Store::open(temp.path().join("journal.sqlite")).unwrap();
            let original = stored.bridge_transfer(transfer).unwrap().unwrap();
            assert_eq!(original.quote_revisions.len(), 1);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(source_bridge.broadcasts(), 0);
            let deadline =
                chrono::DateTime::parse_from_rfc3339(&original.quote_revisions[0].valid_until)
                    .unwrap();
            if let Ok(wait) = (deadline.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std() {
                tokio::time::sleep(wait + std::time::Duration::from_millis(5)).await;
            }
            assert!(deadline < chrono::Utc::now());
            Some(original)
        } else {
            None
        };
        let inputs = if s.step_id == "bridge" {
            let children = quotes["transfers"][0]["steps"].as_array().unwrap();
            assert_eq!(
                children.len(),
                2,
                "one logical bridge step owns approval and deposit"
            );
            children.iter().map(|child| json!({"transferId":transfer,"stepId":child["step_id"],"stepKind":child["kind"],"bounds":bridge_bounds})).collect::<Vec<_>>()
        } else {
            vec![s.input.clone()]
        };
        for (child_index, input) in inputs.into_iter().enumerate() {
            println!("full plan executing {} child {}", s.step_id, child_index);
            if let Some(pool) = &rpc_pool
                && s.step_id == "bridge"
                && child_index == 0
            {
                assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
                assert_eq!(source.supply(), U256::from(14_500_000_000_000_000u64));
                assert_eq!(source.debt(), U256::from(11_700_000));
                assert_eq!(pool.rejected.load(Ordering::SeqCst), 0);
                pool.armed.store(true, Ordering::SeqCst);
            }
            let req = BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: s.step_id.clone(),
                request: OperationRequest {
                    request_id: format!("FULL-EXEC-{}-{child_index}", s.step_id),
                    operation: s.operation.clone(),
                    input,
                    run_id: None,
                },
            };
            let mut record = call(&client, req.clone()).await;
            if quote_recovery && s.step_id == "borrow" {
                let settled: OperationRecord = serde_json::from_value(record.clone()).unwrap();
                let valuation = settled.preview.unwrap().valuation.unwrap();
                let price = valuation
                    .prices
                    .iter()
                    .find(|row| row.asset == usdc)
                    .unwrap();
                let frozen = frozen_precision.unwrap();
                let retrieved = frozen + chrono::Duration::microseconds(547);
                assert!(price.source.contains(&retrieved.to_rfc3339()));
                assert_eq!(
                    chrono::DateTime::parse_from_rfc3339(&price.observed_at).unwrap(),
                    frozen - chrono::Duration::seconds(47)
                );
                assert!(
                    valuation.prices.iter().all(|row| {
                        chrono::DateTime::parse_from_rfc3339(&row.observed_at).unwrap() < retrieved
                    }),
                    "the attributed retrieval must be newer than every ordinary frozen row"
                );
                assert!(
                    chrono::DateTime::parse_from_rfc3339(&valuation.observed_at).unwrap()
                        < retrieved
                );
                assert!(!gofin_domain::price_freshness::is_fresh(
                    &price.observed_at,
                    &price.source,
                    frozen
                ));
                assert!(gofin_domain::price_freshness::is_fresh(
                    &price.observed_at,
                    &price.source,
                    chrono::Utc::now()
                ));
            }
            if let Some(original) = &original_transfer {
                let stored = Store::open(temp.path().join("journal.sqlite")).unwrap();
                let refreshed = stored.bridge_transfer(transfer).unwrap().unwrap();
                assert_eq!(refreshed.id, original.id);
                assert_eq!(refreshed.quote_revisions.len(), 2);
                assert_ne!(
                    refreshed.quote_revisions[0].quote_hash,
                    refreshed.quote_revisions[1].quote_hash
                );
                assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
                assert_eq!(
                    refreshed
                        .steps
                        .iter()
                        .map(|s| &s.step_id)
                        .collect::<Vec<_>>(),
                    original
                        .steps
                        .iter()
                        .map(|s| &s.step_id)
                        .collect::<Vec<_>>()
                );
                assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
                assert_eq!(approvals.2.load(Ordering::SeqCst), 0);
                for (id, request_id, submission) in &completed_prefix {
                    let preserved = stored.get_record(id).unwrap().unwrap();
                    assert_eq!(&preserved["request_id"], request_id);
                    assert_eq!(&preserved["submission"], submission);
                    assert_eq!(preserved["status"], "completed");
                }
            }
            if let Some(pool) = &rpc_pool
                && s.step_id == "bridge"
            {
                assert!(pool.rejected.load(Ordering::SeqCst) > 0);
                assert!(pool.fallback.load(Ordering::SeqCst) > 0);
                assert_eq!(
                    pool.rpc.diagnostics()["last_read"]["selected_endpoint"],
                    "rpc-2"
                );
                assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
                assert_eq!(approvals.2.load(Ordering::SeqCst), 0);
                assert_eq!(record["input"]["transferId"], transfer);
                assert_eq!(
                    record["input"]["stepId"],
                    quotes["transfers"][0]["steps"][child_index]["step_id"]
                );
            }
            let original_submission = record["submission"].clone();
            if funding_v1 && s.step_id == "withdraw" {
                assert_eq!(record["status"], "pending", "{record}");
                assert_eq!(venue.funding_withdrawal_dispatches(), 1);
                assert_eq!(source.writes.load(Ordering::SeqCst), 0);
                let status = call(
                    &client,
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
                assert!(status["next_eligible_step_id"].is_null(), "{status}");
                assert_eq!(status["steps"][2]["step_id"], "wrap");
                assert_ne!(status["steps"][2]["state"], "completed");
                if manual_resolution {
                    use gofin_domain::operation_recovery::{
                        OperationResolutionInput, OperationResolutionOutcome,
                    };
                    let operation_id = record["id"].as_str().unwrap().to_string();
                    let input = OperationResolutionInput {
                        outcome: OperationResolutionOutcome::Succeeded,
                        reason: "Human attests destination receipt without external verification"
                            .into(),
                        actual_debits: vec![amount(full_plan_venue::ACCOUNT, &eth, "0.01489")],
                        actual_costs: vec![fee(
                            full_plan_venue::ACCOUNT,
                            &eth,
                            "0.00009",
                            CostTreatment::IncludedInDebit,
                        )],
                        reported_receipts: vec![amount(&src, &native, "0.0148")],
                    };
                    let review = call(
                        &client,
                        BrokerRequest::PreviewOperationResolution {
                            operation_id: operation_id.clone(),
                            input: input.clone(),
                        },
                    )
                    .await;
                    assert_eq!(review["review"]["record_hash"].as_str().unwrap().len(), 64);
                    assert_eq!(
                        review["review"]["impact"]["receipt_provenance"],
                        "human_attested"
                    );
                    assert_eq!(venue.funding_withdrawal_dispatches(), 1);
                    let resolution_request = BrokerRequest::ResolveOperation {
                        operation_id: operation_id.clone(),
                        request_id: "FULL-HUMAN-RESOLUTION".into(),
                        review_hash: review["review_hash"].as_str().unwrap().into(),
                        input,
                    };
                    let resolved = call(&client, resolution_request.clone()).await;
                    assert_eq!(resolved["status"], "human_resolved", "{resolved}");
                    assert_eq!(resolved["approval"]["source"], "touch_id", "{resolved}");
                    assert_eq!(
                        call(&client, resolution_request).await,
                        resolved,
                        "resolution replay uses the original biometric audit"
                    );
                    record = call(
                        &client,
                        BrokerRequest::Status {
                            id: operation_id.clone(),
                        },
                    )
                    .await;
                    assert_eq!(record["status"], "pending", "{record}");
                    assert!(record["effects"].is_null());
                    assert_eq!(record["submission"], original_submission);
                    assert_eq!(
                        record["effective_outcome"],
                        json!({"provenance":"human_attested","outcome":"succeeded"})
                    );
                    assert!(record["manual_resolution"]["external_confirmation"].is_null());
                    let advanced = call(
                        &client,
                        BrokerRequest::PlanStatus {
                            plan_id: plan_id.clone(),
                        },
                    )
                    .await;
                    assert_eq!(advanced["next_eligible_step_id"], "wrap", "{advanced}");
                    assert_eq!(advanced["steps"][1]["state"], "completed", "{advanced}");
                    let usage: FinancialPlanUsage =
                        serde_json::from_value(advanced["usage"].clone()).unwrap();
                    let human = usage
                        .human_attested_proceeds
                        .iter()
                        .find(|r| r.producing_step_id == "withdraw")
                        .unwrap();
                    assert_eq!(human.resource.0, src);
                    assert_eq!(
                        human.available,
                        Quantity::from_decimal("0.0148", 18).unwrap()
                    );
                    assert!(human.consumed.is_zero());
                    assert!(
                        !usage
                            .verified_proceeds
                            .iter()
                            .any(|r| r.producing_step_id == "withdraw")
                    );
                    assert_eq!(source.writes.load(Ordering::SeqCst), 0);
                    assert_eq!(venue.funding_withdrawal_dispatches(), 1);
                    assert_eq!(approvals.3.load(Ordering::SeqCst), 1);
                    manually_resolved = Some((operation_id, original_submission.clone()));
                } else {
                    venue.funding_history_ready.store(true, Ordering::SeqCst);
                }
            }
            if unresolved && s.step_id == "sell" {
                assert_eq!(record["status"], "unknown", "{record}");
                assert!(record["effects"].is_null());
                let replay = call(&client, req).await;
                assert_eq!(replay["id"], record["id"]);
                let status = call(&client, BrokerRequest::PlanStatus { plan_id }).await;
                assert_ne!(status["steps"][0]["state"], "completed");
                assert!(
                    !status["usage"]["reservations"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
                assert_eq!(source.writes.load(Ordering::SeqCst), 0);
                assert_eq!(hl.deposits.load(Ordering::SeqCst), 0);
                assert_eq!(hl.native_dispatches.load(Ordering::SeqCst), 0);
                assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
                return;
            }
            if lose_response && s.step_id == "sell" {
                assert_eq!(
                    record["status"], "completed",
                    "Lost response must reconcile the original sell before execute returns: {record}"
                );
                assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
            }
            for _ in 0..4 {
                if record["status"] == "completed" || (manual_resolution && s.step_id == "withdraw")
                {
                    break;
                }
                record = call(
                    &client,
                    BrokerRequest::Status {
                        id: record["id"].as_str().unwrap().into(),
                    },
                )
                .await;
            }
            if !(manual_resolution && s.step_id == "withdraw") {
                assert_eq!(record["status"], "completed", "{}: {record}", s.step_id);
            }
            if funding_v1 && !manual_resolution && s.step_id == "withdraw" {
                assert_eq!(record["submission"], original_submission);
                let settled: OperationRecord = serde_json::from_value(record.clone()).unwrap();
                assert!(
                    settled
                        .preview
                        .as_ref()
                        .unwrap()
                        .intent
                        .maximum_costs
                        .iter()
                        .any(|cost| cost.amount.asset == eth
                            && cost.amount.atoms == Quantity::from_decimal("0.0001", 36).unwrap()),
                    "the prepared fee ceiling remains the original approved value"
                );
                let effects = &settled.effects.as_ref().unwrap().effects;
                assert!(effects.iter().any(|effect| matches!(effect,
                    FinancialEffect::CashDebit { amount, .. }
                    if amount.asset==eth && amount.atoms==Quantity::from_decimal("0.01489",36).unwrap()
                )), "actual debit must use the lower proved fee");
                assert!(effects.iter().any(|effect| matches!(effect,
                    FinancialEffect::Cost { amount, treatment:CostTreatment::IncludedInDebit, .. }
                    if amount.asset==eth && amount.atoms==Quantity::from_decimal("0.00009",36).unwrap()
                )), "actual cost must not retain the higher preview fee");
                let status = call(
                    &client,
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
                assert_eq!(status["next_eligible_step_id"], "wrap", "{status}");
                assert_eq!(venue.funding_withdrawal_dispatches(), 1);
                assert_eq!(source.writes.load(Ordering::SeqCst), 0);
            }
            if s.step_id == "order" {
                let settled: OperationRecord = serde_json::from_value(record.clone()).unwrap();
                assert_eq!(s.permitted_variations["maximum_debits"], json!([]));
                let debits = &settled.preview.as_ref().unwrap().intent.debits;
                assert!(
                    !debits.is_empty(),
                    "exercise the adapter's zero debit identity"
                );
                assert!(debits.iter().all(|debit| debit.amount.atoms.is_zero()));
                assert!(settled.effects.unwrap().effects.iter().any(|effect| matches!(
                effect,
                FinancialEffect::PositionChange { resource, anchor: Some(anchor), .. }
                    if resource.0 == hl_resource
                        && anchor.position_kind == PositionKind::DerivativeLong
                        && anchor.principal.asset.reference == "SOL"
                        && anchor.principal.atoms.to_decimal(anchor.principal.asset.decimals) == "0.34"
            )), "The final native fills must prove the intended 0.34 SOL long");
            }
            let replay = call(&client, req).await;
            assert_eq!(replay["id"], record["id"]);
            if quote_recovery && s.step_id != "bridge" && completed_prefix.len() < 6 {
                let original = Store::open(temp.path().join("journal.sqlite"))
                    .unwrap()
                    .get_record(record["id"].as_str().unwrap())
                    .unwrap()
                    .unwrap();
                completed_prefix.push((
                    original["id"].as_str().unwrap().to_owned(),
                    original["request_id"].clone(),
                    original["submission"].clone(),
                ));
            }
            if s.step_id == "bridge" {
                // A source-stage receipt is not spendable destination funding.
                let before = call(
                    &client,
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
                let bridge_state = before["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|step| step["step_id"] == "bridge")
                    .unwrap();
                assert_ne!(bridge_state["state"], "completed");
                let usage: FinancialPlanUsage =
                    serde_json::from_value(before["usage"].clone()).unwrap();
                let borrowed = usage
                    .verified_proceeds
                    .iter()
                    .find(|proceeds| {
                        proceeds.producing_step_id == "borrow"
                            && proceeds.resource.0 == src
                            && proceeds.asset == usdc
                    })
                    .unwrap();
                assert_eq!(borrowed.available, Quantity::from_u64(11_700_000));
                assert_eq!(
                    borrowed.consumed,
                    Quantity::from_u64(if child_index == 0 { 0 } else { 11_700_000 }),
                    "only the deposit child consumes the settled borrow proceeds"
                );
                assert!(usage.reservations.is_empty());
                let deposit = spec
                    .steps
                    .iter()
                    .find(|step| step.step_id == "deposit")
                    .unwrap();
                let premature_id = format!("FULL-PREMATURE-DEPOSIT-{child_index}");
                let premature = client
                    .call(
                        args(&[
                            "execute",
                            &deposit.operation,
                            "--input",
                            &deposit.input.to_string(),
                            "--id",
                            &premature_id,
                            "--plan",
                            &plan_id,
                            "--step",
                            "deposit",
                            "--json",
                        ]),
                        BrokerRequest::ExecutePlanStep {
                            plan_id: plan_id.clone(),
                            step_id: "deposit".into(),
                            request: OperationRequest {
                                request_id: premature_id.clone(),
                                operation: deposit.operation.clone(),
                                input: deposit.input.clone(),
                                run_id: None,
                            },
                        },
                        false,
                    )
                    .await;
                assert!(
                    premature.to_string().contains("PLAN_STEP_NOT_ELIGIBLE"),
                    "{premature}"
                );
                assert_eq!(hl.deposits.load(Ordering::SeqCst), 0);
                if child_index == 0 {
                    assert_eq!(
                        *source_bridge.token_balance.lock().unwrap(),
                        U256::from(11_700_000),
                        "allowance child must not consume funded borrow proceeds"
                    );
                    assert_eq!(
                        *source_bridge.allowance.lock().unwrap(),
                        U256::from(11_700_000)
                    );
                    assert_eq!(
                        source_bridge.broadcasts(),
                        1,
                        "replay must not submit a second approval"
                    );
                    assert_eq!(hl.deposits.load(Ordering::SeqCst), 0);
                    continue;
                }
                assert_eq!(
                    source_bridge.broadcasts(),
                    2,
                    "one approval and one deposit under the same step"
                );
                let settled = call(
                    &client,
                    BrokerRequest::Read {
                        operation: "bridge.status".into(),
                        input: json!({"transferId":transfer}),
                    },
                )
                .await;
                assert_eq!(settled["state"], "completed", "{settled}");
                assert_eq!(
                    settled["settlement"]["cash_credits"][0]["amount_atoms"],
                    "11600000"
                );
                // Apply the independently specified external payout to the transport's
                // wallet balance only after the bridge proof has been observed.
                *destination.token_balance.lock().unwrap() = U256::from(11_600_000);
            }
        }
    }
    let status = call(
        &client,
        BrokerRequest::PlanStatus {
            plan_id: plan_id.clone(),
        },
    )
    .await;
    assert!(
        status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "completed"),
        "{status}"
    );
    assert!(
        status["usage"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{status}"
    );
    if let Some((operation_id, submission)) = manually_resolved {
        let before_usage = status["usage"].clone();
        let usage: FinancialPlanUsage = serde_json::from_value(before_usage.clone()).unwrap();
        let human = usage
            .human_attested_proceeds
            .iter()
            .find(|r| r.producing_step_id == "withdraw")
            .unwrap();
        assert!(
            human.consumed.integer() >= Quantity::from_decimal("0.0145", 18).unwrap().integer()
        );
        assert!(human.consumed.integer() <= human.available.integer());
        venue.funding_history_ready.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            let externally_observed = call(
                &client,
                BrokerRequest::Status {
                    id: operation_id.clone(),
                },
            )
            .await;
            assert_eq!(externally_observed["submission"], submission);
            assert_eq!(
                externally_observed["manual_resolution"]["external_confirmation"]["status"],
                "agrees",
                "{externally_observed}"
            );
            assert!(
                externally_observed["manual_resolution"]["external_confirmation"]["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|observation| observation["accounted_again"] == false)
            );
            let after = call(
                &client,
                BrokerRequest::PlanStatus {
                    plan_id: plan_id.clone(),
                },
            )
            .await;
            assert_eq!(
                after["usage"], before_usage,
                "late original proof must not double-credit or reset consumed proceeds"
            );
            assert_eq!(venue.funding_withdrawal_dispatches(), 1);
        }
    }
    assert_eq!(
        approvals.3.load(Ordering::SeqCst),
        usize::from(manual_resolution)
    );
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        if quote_recovery { 2 } else { 1 }
    );
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        2 + usize::from(override_historical)
    );
    if let Some((original, reservations)) = historical {
        let operation_id = original["id"].as_str().unwrap();
        let still_unknown = call(
            &client,
            BrokerRequest::Status {
                id: operation_id.into(),
            },
        )
        .await;
        assert_eq!(still_unknown["status"], "unknown");
        assert_eq!(still_unknown["submission"], original["submission"]);
        assert_eq!(still_unknown["request_id"], original["request_id"]);
        assert!(still_unknown["effects"].is_null());
        assert_eq!(
            Store::open(temp.path().join("journal.sqlite"))
                .unwrap()
                .reservations(operation_id)
                .unwrap(),
            reservations
        );
        assert_eq!(
            venue.writes.load(Ordering::SeqCst),
            3,
            "historical reconciliation cannot replay the old request"
        );
    }
    assert_eq!(source.supply(), U256::from(14_500_000_000_000_000u64));
    assert_eq!(source.debt(), U256::from(11_700_000));
    assert_eq!(source.weth(), U256::ZERO);
    assert_eq!(source.usdc(), U256::ZERO);
    assert_eq!(source.allowance(), U256::ZERO);
    assert_eq!(source.writes.load(Ordering::SeqCst), 4);
    assert_eq!(source.native(), U256::from(9_932_000_000_000_000u64));
    assert_eq!(*source_bridge.token_balance.lock().unwrap(), U256::ZERO);
    assert_eq!(*source_bridge.allowance.lock().unwrap(), U256::ZERO);
    assert_eq!(source_bridge.broadcasts(), 2);
    assert_eq!(*destination.token_balance.lock().unwrap(), U256::ZERO);
    assert_eq!(hl.deposits.load(Ordering::SeqCst), 1);
    assert_eq!(hl.native_dispatches.load(Ordering::SeqCst), 2);
}
