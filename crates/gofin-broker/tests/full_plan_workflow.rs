//! Ten native-authorized logical actions through Kraken, Ethereum/Aave, bridge and Hyperliquid.
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
    subsecond_conversion: bool,
}
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(self.quote_at(a, chrono::Utc::now()))
    }

    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<gofin_pricing::UsdQuote>> {
        // Freeze timestamps within a response batch, not for the entire plan.
        // Later reads return new observations even on slow CI runners; saved
        // valuations still retain their original precision and provenance.
        let current = chrono::Utc::now();
        Ok(assets.iter().map(|a| self.quote_at(a, current)).collect())
    }
}
impl Prices {
    fn quote_at(
        &self,
        a: &AssetId,
        current: chrono::DateTime<chrono::Utc>,
    ) -> gofin_pricing::UsdQuote {
        let usd = match a.reference.as_str() {
            "native" | "XETH" => "2000",
            "SOL" => "100",
            s if s.eq_ignore_ascii_case(full_plan_evm::WETH) => "2000",
            _ => "1",
        };
        let mut quote = gofin_pricing::UsdQuote {
            usd_per_unit: usd.into(),
            updated_at: current.to_rfc3339(),
            observed_at: current.to_rfc3339(),
            source: "controlled independent prices".into(),
        };
        if self.subsecond_conversion {
            let at = (current - chrono::Duration::seconds(1))
                .with_nanosecond(265_000_000)
                .unwrap();
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
        quote
    }
}
#[test]
fn precision_price_fixture_refreshes_new_reads_without_restamping_saved_quotes() {
    let prices = Prices {
        subsecond_conversion: true,
    };
    let first = chrono::DateTime::parse_from_rfc3339("2026-10-27T12:00:00.900Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let later = first + chrono::Duration::minutes(10);
    for reference in ["native", bridge_fixture::ETH_USDC] {
        let a = asset("ethereum:1", reference, 18);
        let saved = prices.quote_at(&a, first);
        let refreshed = prices.quote_at(&a, later);
        assert!(gofin_domain::price_freshness::is_fresh(
            &saved.updated_at,
            &saved.source,
            first
        ));
        assert!(!gofin_domain::price_freshness::is_fresh(
            &saved.updated_at,
            &saved.source,
            later
        ));
        assert!(gofin_domain::price_freshness::is_fresh(
            &refreshed.updated_at,
            &refreshed.source,
            later
        ));
        let updated = chrono::DateTime::parse_from_rfc3339(&refreshed.updated_at).unwrap();
        let retrieved = chrono::DateTime::parse_from_rfc3339(&refreshed.observed_at).unwrap();
        assert_eq!(updated.nanosecond(), 265_000_000);
        if reference == "native" {
            assert_eq!(updated, retrieved);
        } else {
            assert_eq!(retrieved.nanosecond(), 265_547_000);
            assert_eq!(
                retrieved - updated,
                chrono::Duration::seconds(47) + chrono::Duration::microseconds(547)
            );
        }
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
async fn call(client: &Client, request: BrokerRequest) -> Value {
    let argv = match &request {
        BrokerRequest::Read { operation, input } => {
            args(&["read", operation, "--input", &input.to_string(), "--json"])
        }
        BrokerRequest::Execute { request } => args(&[
            "execute",
            &request.operation,
            "--input",
            &request.input.to_string(),
            "--id",
            &request.request_id,
            "--json",
        ]),
        BrokerRequest::Status { id } => args(&["status", id, "--json"]),
        _ => panic!("Unexpected request"),
    };
    client.call(argv, request, true).await
}

#[test]
fn kraken_aave_bridge_hyperliquid_complete_with_ten_native_approvals() {
    run(false, false, false, false, false, false, false);
}
#[test]
fn kraken_aave_bridge_hyperliquid_recovers_original_lost_sell() {
    run(true, false, false, false, false, false, false);
}
#[test]
fn kraken_aave_bridge_hyperliquid_holds_unknown_sell_without_downstream_dispatch() {
    run(true, true, false, false, false, false, false);
}

#[test]
fn funding_v1_lower_final_fee_completes_original_withdrawal_and_downstream_actions() {
    run(false, false, false, true, false, false, false);
}

#[test]
fn native_workflow_preserves_conversion_price_precision() {
    run(false, false, false, false, false, true, false);
}
#[test]
fn compound_bridge_code_read_fails_over_without_new_approval() {
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
    _manual_resolution: bool,
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
    let price_source = Arc::new(Prices {
        subsecond_conversion: quote_recovery,
    });
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

    let mut completed = Vec::new();
    for (index, s) in steps.iter().enumerate() {
        let mut input = s.input.clone();
        if s.step_id == "bridge" {
            input["actionDeadline"] = json!(chrono::Utc::now().timestamp() + 600);
            let bounds = input["bounds"].as_object_mut().unwrap();
            bounds.remove("maximum_total_usd_micros");
            bounds.remove("authorization_seconds");
            for cap in bounds["fee_caps"].as_array_mut().unwrap() {
                cap.as_object_mut().unwrap().remove("maximum_usd_micros");
            }
            if let Some(pool) = &rpc_pool {
                pool.armed.store(true, Ordering::SeqCst);
            }
        }
        let request = BrokerRequest::Execute {
            request: OperationRequest {
                request_id: format!("FULL-NATIVE-{}", s.step_id),
                operation: s.operation.clone(),
                input,
                run_id: None,
            },
        };
        let mut record = call(&client, request.clone()).await;
        assert_eq!(
            approvals.2.load(Ordering::SeqCst),
            index + 1,
            "one approval per logical action: {record}"
        );
        if unresolved && s.step_id == "sell" {
            assert!(
                matches!(record["status"].as_str(), Some("unknown" | "pending")),
                "{record}"
            );
            assert_eq!(record["outcome"]["replay_safe"], false);
            assert!(record["submission"].is_object());
            let duplicate = call(&client, request).await;
            assert_eq!(duplicate["id"], record["id"]);
            assert_eq!(duplicate["submission"], record["submission"]);
            assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
            assert_eq!(source.writes.load(Ordering::SeqCst), 0);
            return;
        }
        if funding_v1 && s.step_id == "withdraw" {
            assert_eq!(record["status"], "pending", "{record}");
            assert_eq!(venue.funding_withdrawal_dispatches(), 1);
            assert_eq!(source.writes.load(Ordering::SeqCst), 0);
            let pending = call(
                &client,
                BrokerRequest::Status {
                    id: record["id"].as_str().unwrap().into(),
                },
            )
            .await;
            assert_eq!(pending["submission"], record["submission"]);
            assert_eq!(pending["status"], "pending");
            venue.funding_history_ready.store(true, Ordering::SeqCst);
        }
        for _ in 0..4 {
            if record["outcome"]["phase"] == "done" {
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
        assert_eq!(
            record["outcome"]["phase"], "done",
            "{}: {record}",
            s.step_id
        );
        assert_eq!(
            record["outcome"]["replay_safe"], false,
            "finality cannot permit replay"
        );
        if funding_v1 && s.step_id == "withdraw" {
            let effects: VerifiedEffects =
                serde_json::from_value(record["effects"].clone()).unwrap();
            assert!(effects.effects.iter().any(|effect|matches!(effect,FinancialEffect::CashDebit {amount,..} if amount.asset==eth && amount.atoms==Quantity::from_decimal("0.01489",36).unwrap())));
            assert!(effects.effects.iter().any(|effect|matches!(effect,FinancialEffect::Cost {amount,treatment:CostTreatment::IncludedInDebit,..} if amount.asset==eth && amount.atoms==Quantity::from_decimal("0.00009",36).unwrap())));
        }
        if s.step_id == "bridge" {
            let settled = call(
                &client,
                BrokerRequest::Read {
                    operation: "bridge.status".into(),
                    input: json!({"transferId":record["preview"]["binding"]["transfer_id"]}),
                },
            )
            .await;
            assert_eq!(settled["state"], "completed", "{settled}");
            assert_eq!(
                settled["settlement"]["cash_credits"][0]["amount_atoms"],
                "11600000"
            );
            // Reflect the independently specified and now verified payout in the fixture's current balance.
            *destination.token_balance.lock().unwrap() = U256::from(11_600_000);
        }
        if s.step_id == "order" {
            let effects: VerifiedEffects =
                serde_json::from_value(record["effects"].clone()).unwrap();
            assert!(effects.effects.iter().any(|effect|matches!(effect,FinancialEffect::PositionChange {resource,anchor:Some(anchor),..} if resource.0==hl_resource && anchor.position_kind==PositionKind::DerivativeLong && anchor.principal.asset.reference=="SOL" && anchor.principal.atoms.to_decimal(anchor.principal.asset.decimals)=="0.34")));
        }
        let duplicate = call(&client, request.clone()).await;
        assert_eq!(duplicate["id"], record["id"]);
        assert_eq!(duplicate["submission"], record["submission"]);
        assert_eq!(approvals.2.load(Ordering::SeqCst), index + 1);
        completed.push((request, record));
    }
    assert_eq!(completed.len(), 10);
    assert_eq!(
        approvals.0.load(Ordering::SeqCst),
        0,
        "no named-plan approval"
    );
    assert_eq!(approvals.1.load(Ordering::SeqCst), 0, "no override");
    assert_eq!(approvals.2.load(Ordering::SeqCst), 10);
    assert_eq!(
        approvals.3.load(Ordering::SeqCst),
        0,
        "no human settlement assertion"
    );
    for (request, record) in completed {
        let observed = call(&client, request).await;
        assert_eq!(observed["id"], record["id"]);
        assert_eq!(observed["submission"], record["submission"]);
    }
    if let Some(pool) = rpc_pool {
        assert!(pool.rejected.load(Ordering::SeqCst) > 0);
        assert!(pool.fallback.load(Ordering::SeqCst) > 0);
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(source.supply(), U256::from(14_500_000_000_000_000u64));
    assert_eq!(source.debt(), U256::from(11_700_000));
    assert_eq!(source.weth(), U256::ZERO);
    assert_eq!(source.usdc(), U256::ZERO);
    assert_eq!(source.allowance(), U256::ZERO);
    assert_eq!(source.writes.load(Ordering::SeqCst), 4);
    assert_eq!(source.native(), U256::from(9_932_000_000_000_000u64));
    assert_eq!(*source_bridge.token_balance.lock().unwrap(), U256::ZERO);
    assert_eq!(*source_bridge.allowance.lock().unwrap(), U256::ZERO);
    assert_eq!(
        source_bridge.broadcasts(),
        2,
        "one approval + one bridge deposit provider submission"
    );
    assert_eq!(*destination.token_balance.lock().unwrap(), U256::ZERO);
    assert_eq!(hl.deposits.load(Ordering::SeqCst), 1);
    assert_eq!(hl.native_dispatches.load(Ordering::SeqCst), 2);
}
