//! Complete real-adapter named-plan rehearsal over in-memory venue/RPC transports.
//! Set FIN_TIMING_CLI to the built development CLI to include its actual parser,
//! socket transport, JSON rendering and broker dispatch. No production paths.
#![allow(dead_code)] // shared fixtures contain independent regression helpers
use async_trait::async_trait;
use fin_bridge::*;
use fin_broker::{
    BridgeAdapter, BrokerConfigV2, BrokerPolicy, DeploymentConfig, NetworkConfig,
    RuntimeDependencies, SharedUsdValuator, compose_runtime_with_clock,
};
use fin_domain::clock::Clock;
use fin_domain::financial_plans::*;
use fin_domain::*;
use fin_ethereum::{EthereumAdapter, EthereumConfig, RouterKind, UniswapConfig, WalletConnection};
use fin_evm::{EvmRuntime, LocalEnvelopeSigner, alloy::primitives::U256};
use fin_protocol::BrokerRequest;
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "support/bridge_erc20_fixture.rs"]
mod fixture;
#[path = "support/plan_timing_rpc.rs"]
mod rpc_fixture;
use rpc_fixture::*;
struct ManualClock(Mutex<chrono::DateTime<chrono::Utc>>);
impl Clock for ManualClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }
}
impl ManualClock {
    fn advance(&self, s: i64) {
        *self.0.lock().unwrap() += chrono::Duration::seconds(s);
    }
}
struct Approval {
    clock: Arc<ManualClock>,
    count: AtomicUsize,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic timing fixture only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: self.clock.now().to_rfc3339(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Prices(Arc<ManualClock>);
#[async_trait]
impl fin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<fin_pricing::UsdQuote> {
        Ok(fin_pricing::UsdQuote {
            usd_per_unit: if a.reference == "native" || a.reference == "ETH" {
                "2000"
            } else {
                "1"
            }
            .into(),
            updated_at: self.0.now().to_rfc3339(),
            observed_at: self.0.now().to_rfc3339(),
            source: "synthetic timing asset prices".into(),
        })
    }
}
struct Provider {
    inner: Arc<fixture::Erc20Provider>,
    clock: Arc<ManualClock>,
}
#[async_trait]
impl BridgeProvider for Provider {
    fn capabilities(&self) -> BridgeCapabilities {
        self.inner.capabilities()
    }
    async fn quote(&self, r: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let mut batch = self.inner.quote(r).await?;
        for q in &mut batch.quotes {
            q.observed_at = self.clock.now().to_rfc3339();
            q.valid_until = (self.clock.now() + chrono::Duration::seconds(120)).to_rfc3339();
        }
        Ok(batch)
    }
    fn prepare(&self, q: &BridgeQuote, b: &BridgeBounds, w: &str) -> Result<PreparedBridge> {
        self.inner.prepare(q, b, w)
    }
    async fn reconcile(&self, r: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        self.inner.reconcile(r).await
    }
}
struct Venue {
    rpc: Arc<WorkflowRpc>,
    posts: AtomicUsize,
    time: Mutex<Option<u64>>,
    balance: Mutex<&'static str>,
}
#[async_trait]
impl fin_binance::BinanceTransport for Venue {
    async fn request(
        &self,
        _: &fin_binance::CredentialHandle,
        r: fin_binance::BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        Ok(match r.endpoint {
            E::ApiRestrictions => {
                json!({"enableReading":true,"enableWithdrawals":true,"enableSpotAndMarginTrading":true})
            }
            E::Account => {
                json!({"accountType":"SPOT","uid":"1001","balances":[{"asset":"ETH","free":*self.balance.lock().unwrap(),"locked":"0"}]})
            }
            E::CapitalConfig => {
                json!([{"coin":"ETH","withdrawAllEnable":true,"depositAllEnable":true,"networkList":[{"network":"ETH","coin":"ETH","depositEnable":true,"withdrawEnable":true,"withdrawIntegerMultiple":"0.0001","withdrawFee":"0","withdrawMin":"0.001","withdrawMax":"100","withdrawTag":false,"busy":false}]}])
            }
            E::TravelRuleRequirements => json!({"questionnaireCountryCode":"NIL"}),
            E::StandardWithdrawal => {
                assert_eq!(r.get("amount"), Some("0.006"));
                self.posts.fetch_add(1, Ordering::SeqCst);
                *self.time.lock().unwrap() = Some(self.rpc.clock.now().timestamp_millis() as u64);
                *self.balance.lock().unwrap() = "0.994";
                json!({"id":"synthetic-clock-withdrawal"})
            }
            E::WithdrawalHistory => {
                let time = self.time.lock().unwrap().unwrap();
                json!([{"id":"synthetic-clock-withdrawal","coin":"ETH","amount":"0.006","transactionFee":"0","network":"ETH","address":self.rpc.inner.wallet,"txId":withdrawal_hash(),"status":6,"completeTime":chrono::DateTime::from_timestamp_millis(time as i64).unwrap().format("%Y-%m-%d %H:%M:%S").to_string(),"withdrawOrderId":r.get("withdrawOrderId"),"transferType":0,"walletType":0}])
            }
            e => panic!("Unexpected isolated venue endpoint {e:?}"),
        })
    }
}
struct Recipient(Arc<EthereumAdapter>);
#[async_trait]
impl fin_binance::BinanceRecipientVerifier for Recipient {
    async fn verify(&self, r: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        match self
            .0
            .verify_external_credit(
                &r.tx_hash,
                &r.recipient,
                &r.asset,
                &r.minimum_amount,
                &r.maximum_amount,
                r.confirmations,
            )
            .await
        {
            Ok(p) => fin_binance::PayoutResult {
                status: p.status,
                amount_atoms: p.amount_atoms,
                evidence: p.evidence,
            },
            Err(e) => fin_binance::PayoutResult {
                status: Status::Unknown,
                amount_atoms: None,
                evidence: json!({"synthetic_proof_error":e.code}),
            },
        }
    }
}
fn q(s: &str) -> Quantity {
    Quantity::new(s).unwrap()
}
fn resource_amount(resource: &str, asset: &AssetId, atoms: &str) -> ResourceAmount {
    ResourceAmount {
        resource: ResourceId(resource.into()),
        amount: Amount {
            asset: asset.clone(),
            atoms: q(atoms),
        },
    }
}
fn fee(resource: &str, asset: &AssetId, atoms: &str) -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: ResourceId(resource.into()),
        },
        amount: Amount {
            asset: asset.clone(),
            atoms: q(atoms),
        },
        treatment: CostTreatment::AdditionalDebit,
    }
}
fn step(
    id: &str,
    op: &str,
    input: Value,
    bounds: Value,
    deps: Vec<FinancialPlanFundingDependency>,
    window: Option<u64>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: id.into(),
        operation: op.into(),
        input,
        permitted_variations: bounds,
        funding_dependencies: deps,
        action_deadline_seconds: window,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}
fn dependency(id: &str, resource: &str, asset: &AssetId) -> FinancialPlanFundingDependency {
    FinancialPlanFundingDependency {
        producing_step_id: id.into(),
        resource: ResourceId(resource.into()),
        asset: asset.clone(),
        funding_scope: None,
    }
}
fn omit_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            for v in m.values_mut() {
                omit_nulls(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                omit_nulls(v);
            }
        }
        _ => {}
    }
}

struct Client {
    runtime: Arc<fin_broker::BrokerRuntime>,
    socket: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Client {
    async fn new(runtime: Arc<fin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join("cli.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let r = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = r.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], "synthetic-plan-timing");
                    let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let input: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(input["request"].clone()).unwrap();
                    let response = runtime.handle(request).await;
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        Self {
            runtime,
            socket,
            server,
        }
    }
    async fn call(&self, args: Vec<String>, request: BrokerRequest) -> Value {
        if let Ok(binary) = std::env::var("FIN_TIMING_CLI") {
            let socket = self.socket.clone();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args([
                        "--identity",
                        "synthetic-plan-timing",
                        "--build-id",
                        "synthetic-plan-timing-build",
                    ])
                    .args(args)
                    .env("FIN_DEV_IDENTITY", "synthetic-plan-timing")
                    .env("FIN_DEV_BUILD_ID", "synthetic-plan-timing-build")
                    .env("FIN_DEV_SOCKET", socket)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.stderr.is_empty(),
                "JSON must be quiet: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(output.status.success(), "CLI failure: {value}");
            value
        } else {
            let response = self.runtime.handle(request).await;
            assert!(response.ok, "broker failure: {:?}", response.error);
            response.result.unwrap()
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

fn run_with_test_stack(future: impl Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn withdrawal_swap_bridge_settles_with_one_45_minute_authorization_and_late_funding() {
    run_with_test_stack(rehearse(false));
}

#[test]
fn pending_relative_swap_reconciles_after_expiry_and_broker_restart_without_resubmission() {
    run_with_test_stack(rehearse(true));
}

async fn rehearse(pending_swap: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock(Mutex::new(chrono::Utc::now())));
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let wallet = fin_evm::lower_address(signer.address());
    let resource = format!("ethereum:1:{wallet}");
    let inner = Arc::new(fixture::Rpc::source_with_allowance(
        wallet.clone(),
        U256::from(10_000_000u64),
    ));
    *inner.token_balance.lock().unwrap() = U256::ZERO;
    let source = Arc::new(WorkflowRpc::new(inner.clone(), clock.clone()));
    let destination = Arc::new(fixture::Rpc::destination(wallet.clone()));
    let source_runtime = Arc::new(EvmRuntime::new(source.clone(), 1, Some(signer)));
    let destination_runtime = Arc::new(EvmRuntime::new(destination.clone(), 42161, None));
    let prices = Arc::new(
        SharedUsdValuator::with_source(Arc::new(Prices(clock.clone()))).with_clock(clock.clone()),
    );
    let mut config = EthereumConfig::wallet_only(1, 1, vec![fixture::ETH_USDC.into()]);
    config.weth = Some(WETH.into());
    config.uniswap = Some(UniswapConfig {
        factory: FACTORY.into(),
        quoter: QUOTER.into(),
        router: SWAP_ROUTER.into(),
        router_kind: RouterKind::LegacyV3,
        v4: None,
    });
    let source_wallet = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet.clone(),
                signing_enabled: true,
            }],
            source_runtime.clone(),
        )
        .unwrap()
        .with_wallet_valuation(prices.clone())
        .with_clock(clock.clone()),
    );
    let destination_wallet = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(42161, 1, vec![fixture::ARB_USDC.into()]),
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet.clone(),
                signing_enabled: false,
            }],
            destination_runtime.clone(),
        )
        .unwrap()
        .with_clock(clock.clone()),
    );
    let provider = Arc::new(fixture::Erc20Provider::with_destination(
        destination,
        fixture::DestinationPayout::Router,
    ));
    provider.omit_approval.store(true, Ordering::SeqCst);
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            Arc::new(Provider {
                inner: provider.clone(),
                clock: clock.clone(),
            }),
            BTreeMap::from([
                (1, source_wallet.clone()),
                (42161, destination_wallet.clone()),
            ]),
            BTreeMap::from([
                (1, source_runtime.clone()),
                (42161, destination_runtime.clone()),
            ]),
            prices.clone(),
        )
        .unwrap()
        .with_clock(clock.clone()),
    );
    let eth = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let usdc = AssetId {
        network: "ethereum:1".into(),
        reference: fixture::ETH_USDC.into(),
        decimals: 6,
    };
    let binance_eth = AssetId {
        network: "binance".into(),
        reference: "ETH".into(),
        decimals: 36,
    };
    let venue = Arc::new(Venue {
        rpc: source.clone(),
        posts: AtomicUsize::new(0),
        time: Mutex::new(None),
        balance: Mutex::new("1"),
    });
    let binance = Arc::new(
        fin_binance::BinanceAdapter::new(
            fin_binance::BinanceConfig {
                accounts: vec![fin_binance::BinanceAccount {
                    name: "synthetic-main".into(),
                    resource: ResourceId("binance:uid:1001:SPOT".into()),
                    credential: fin_binance::CredentialHandle::new("test-only:synthetic-timing")
                        .unwrap(),
                    role: fin_binance::AccountRole::Main,
                    parent: None,
                    email: None,
                    wallet_type: "SPOT".into(),
                }],
                destinations: vec![fin_binance::EthereumDestination {
                    name: "synthetic-wallet".into(),
                    resource: ResourceId(resource.clone()),
                    address: wallet.clone(),
                    chain_id: 1,
                    confirmations: 1,
                    assets: BTreeMap::from([("ETH".into(), eth.clone())]),
                }],
            },
            venue.clone(),
            Some(prices.clone()),
            Some(Arc::new(Recipient(source_wallet.clone()))),
        )
        .unwrap(),
    );
    let approval = Arc::new(Approval {
        clock: clock.clone(),
        count: AtomicUsize::new(0),
    });
    let runtime = compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![DeploymentConfig::ethereum_mainnet()],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(tmp.path().join("journal.sqlite")).unwrap(),
            adapters: vec![binance.clone(), source_wallet.clone(), bridge.clone()],
            authorizer: approval.clone(),
            valuator: prices.clone(),
            bridge: Some(bridge.clone()),
        },
        clock.clone(),
    )
    .unwrap();
    let client = Client::new(runtime, tmp.path()).await;
    if pending_swap {
        source.swap_receipt_ready.store(false, Ordering::SeqCst);
    }
    let quote_input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":fixture::ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":fixture::ARB_USDC}],"recipient":wallet,"refundTo":wallet,"allowedRefundAssets":[{"chainId":1,"token":fixture::ETH_USDC}],"amount":"10000000"}});
    let quoted = client
        .call(
            args(&[
                "read",
                "bridge.quotes",
                "--input",
                &quote_input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "bridge.quotes".into(),
                input: quote_input.clone(),
            },
        )
        .await;
    let transfer = quoted["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_string();
    let link: BridgeStepLink =
        serde_json::from_value(quoted["transfers"][0]["steps"][0].clone()).unwrap();
    assert_eq!(link.kind, fin_domain::BridgeStepKind::Deposit);
    let withdraw = json!({"requestId":"SYNTHETIC-withdraw-template","account":"synthetic-main","destination":"synthetic-wallet","coin":"ETH","amount":"0.006","network":"ETH","walletType":"SPOT","maxFee":"0","minReceiveAmount":"0.006"});
    let swap = json!({"wallet":"synthetic-wallet","network":"evm","chain_id":1,"request_id":"SYNTHETIC-swap-template","max_fee_eth":"0.0005","token_in":"native","token_out":fixture::ETH_USDC,"amount":"0.004","recipient":wallet,"min_output":"9.9","slippage_bps":100,"max_price_impact_bps":10000,"fee_tier":500,"protocol":"v3"});
    let mut bridge_input = json!({"transferId":transfer,"stepId":link.step_id,"stepKind":"deposit","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":fixture::ARB_USDC,"decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":eth,"kind":"gas","maximum":"800000000000000","maximum_usd_micros":1600000}],"maximum_total_usd_micros":11600000,"authorization_seconds":2700}});
    omit_nulls(&mut bridge_input);
    let withdraw_scope = json!({"mode":"exact","maximum_debits":[resource_amount("binance:uid:1001:SPOT",&binance_eth,"6000000000000000000000000000000000")],"maximum_costs":[],"possible_credits":[{"resource":resource,"asset":eth}]});
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic withdrawal → swap → bridge".into(),
        steps: vec![
            step(
                "withdraw",
                "binance.withdraw",
                withdraw.clone(),
                withdraw_scope,
                vec![],
                None,
            ),
            step(
                "swap",
                "uniswap.swap",
                swap.clone(),
                json!({"max_input":"0.004","max_fee_eth":"0.0005","max_slippage_bps":100,"max_price_impact_bps":10000,"minimum_rate":{"output_atoms":"9900000","input_atoms":"4000000000000000"},"allowed_fee_tiers":[500],"max_deadline_seconds":600}),
                vec![dependency("withdraw", &resource, &eth)],
                Some(600),
            ),
            step(
                "bridge",
                "bridge.transfer",
                bridge_input.clone(),
                json!({}),
                vec![
                    dependency("swap", &resource, &usdc),
                    dependency("withdraw", &resource, &eth),
                ],
                None,
            ),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![
                resource_amount(
                    "binance:uid:1001:SPOT",
                    &binance_eth,
                    "6000000000000000000000000000000000",
                ),
                resource_amount(&resource, &eth, "4000000000000000"),
                resource_amount(&resource, &usdc, "10000000"),
            ],
            maximum_costs: vec![fee(&resource, &eth, "1300000000000000")],
        },
    };
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-timing-authority".into(),
        plan: plan.clone(),
        valid_for_seconds: None,
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let review = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
        )
        .await;
    assert_eq!(review["valid_for_seconds"], 2700);
    assert_eq!(
        *inner.token_balance.lock().unwrap(),
        U256::ZERO,
        "future funds remain unavailable during approval"
    );
    request.expected_review_hash = Some(review["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                &request.request_id,
                "--review-hash",
                request.expected_review_hash.as_ref().unwrap(),
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_string();
    assert_eq!(authorized["revision"]["valid_for_seconds"], 2700);
    for (id, op, input) in [
        ("withdraw", "binance.withdraw", withdraw.clone()),
        ("swap", "uniswap.swap", swap.clone()),
        ("bridge", "bridge.transfer", bridge_input.clone()),
    ] {
        if id == "swap" {
            clock.advance(661);
            source.withdrawal_ready.store(true, Ordering::SeqCst);
            let status = client
                .call(
                    args(&["plans", "status", &plan_id, "--json"]),
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
            assert_eq!(status["steps"][0]["state"], "completed");
            let preview = client
                .call(
                    args(&[
                        "execute",
                        op,
                        "--input",
                        &input.to_string(),
                        "--plan",
                        &plan_id,
                        "--step",
                        id,
                        "--dry-run",
                        "--json",
                    ]),
                    BrokerRequest::PreviewPlanStep {
                        plan_id: plan_id.clone(),
                        step_id: id.into(),
                        operation: op.into(),
                        input: input.clone(),
                    },
                )
                .await;
            assert_eq!(preview["action_deadline"]["window_seconds"], 600);
            assert_eq!(
                preview["input"]["deadline"],
                clock.now().timestamp() as u64 + 600
            );
        }
        if id == "bridge" {
            let mut refresh = quote_input.clone();
            refresh["refreshTransferId"] = json!(transfer);
            client
                .call(
                    args(&[
                        "read",
                        "bridge.quotes",
                        "--input",
                        &refresh.to_string(),
                        "--json",
                    ]),
                    BrokerRequest::Read {
                        operation: "bridge.quotes".into(),
                        input: refresh,
                    },
                )
                .await;
        }
        let request = OperationRequest {
            request_id: format!("SYNTHETIC-{id}-attempt"),
            operation: op.into(),
            input: input.clone(),
            run_id: None,
        };
        let result = client
            .call(
                args(&[
                    "execute",
                    op,
                    "--input",
                    &input.to_string(),
                    "--id",
                    &request.request_id,
                    "--plan",
                    &plan_id,
                    "--step",
                    id,
                    "--json",
                ]),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: id.into(),
                    request: request.clone(),
                },
            )
            .await;
        assert!(
            result["status"] == "completed"
                || ((id == "withdraw" || (id == "swap" && pending_swap))
                    && result["status"] == "pending"),
            "{id}: {result}"
        );
        let duplicate = client
            .call(
                args(&[
                    "execute",
                    op,
                    "--input",
                    &input.to_string(),
                    "--id",
                    &request.request_id,
                    "--plan",
                    &plan_id,
                    "--step",
                    id,
                    "--json",
                ]),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: id.into(),
                    request,
                },
            )
            .await;
        assert_eq!(duplicate["id"], result["id"]);
        if id == "swap" && pending_swap {
            assert_eq!(result["status"], "pending");
            let original_id = result["id"].clone();
            clock.advance(2700);
            source.swap_receipt_ready.store(true, Ordering::SeqCst);
            let fresh_bridge = Arc::new(
                BridgeAdapter::with_provider_and_observers(
                    Arc::new(Provider {
                        inner: provider.clone(),
                        clock: clock.clone(),
                    }),
                    BTreeMap::from([
                        (1, source_wallet.clone()),
                        (42161, destination_wallet.clone()),
                    ]),
                    BTreeMap::from([
                        (1, source_runtime.clone()),
                        (42161, destination_runtime.clone()),
                    ]),
                    prices.clone(),
                )
                .unwrap()
                .with_clock(clock.clone()),
            );
            let restarted = compose_runtime_with_clock(
                client.runtime.configuration().clone(),
                RuntimeDependencies {
                    store: Store::open(tmp.path().join("journal.sqlite")).unwrap(),
                    adapters: vec![binance.clone(), source_wallet.clone(), fresh_bridge.clone()],
                    authorizer: approval.clone(),
                    valuator: prices.clone(),
                    bridge: Some(fresh_bridge),
                },
                clock.clone(),
            )
            .unwrap();
            let restart_dir = tmp.path().join("restart");
            std::fs::create_dir(&restart_dir).unwrap();
            let restarted = Client::new(restarted, &restart_dir).await;
            let status = restarted
                .call(
                    args(&["plans", "status", &plan_id, "--json"]),
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
            assert_eq!(status["revision"]["status"], "expired");
            assert_eq!(status["steps"][1]["state"], "completed");
            assert_eq!(status["steps"][1]["attempts"][0]["attempt_id"], original_id);
            assert!(
                status["usage"]["reservations"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let blocked = restarted
                .runtime
                .handle(BrokerRequest::PreviewPlanStep {
                    plan_id: plan_id.clone(),
                    step_id: "bridge".into(),
                    operation: "bridge.transfer".into(),
                    input: bridge_input.clone(),
                })
                .await;
            assert_eq!(blocked.error.unwrap().code, "PLAN_AUTHORIZATION_INACTIVE");
            assert_eq!(source.swaps.load(Ordering::SeqCst), 1);
            assert_eq!(inner.broadcasts(), 0);
            assert_eq!(approval.count.load(Ordering::SeqCst), 1);
            return;
        }
    }
    let status = client
        .call(
            args(&["plans", "status", &plan_id, "--json"]),
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
        "final status: {status}"
    );
    assert!(
        status["usage"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(approval.count.load(Ordering::SeqCst), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(source.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(inner.broadcasts(), 1);
    assert_eq!(*venue.balance.lock().unwrap(), "0.994");
    assert_eq!(*inner.token_balance.lock().unwrap(), U256::ZERO);
    assert_eq!(*inner.allowance.lock().unwrap(), U256::ZERO);
    let native = source.runtime_balance().await;
    assert_eq!(
        native,
        U256::from(101_927_999_999_900_000u64),
        "baseline gas + withdrawal - swap input - independently specified receipt gas"
    );
    let bridge_status_input = json!({"transferId":transfer});
    let bridge_status = client
        .call(
            args(&[
                "read",
                "bridge.status",
                "--input",
                &bridge_status_input.to_string(),
                "--json",
            ]),
            BrokerRequest::Read {
                operation: "bridge.status".into(),
                input: bridge_status_input,
            },
        )
        .await;
    assert_eq!(
        bridge_status["settlement"]["cash_credits"][0]["amount_atoms"], "9700000",
        "independently verified destination credit: {bridge_status}"
    );
    clock.advance(2700);
    let after = client
        .call(
            args(&["plans", "status", &plan_id, "--json"]),
            BrokerRequest::PlanStatus { plan_id },
        )
        .await;
    assert!(
        after["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "completed")
    );
    assert_eq!(inner.broadcasts(), 1);
}
