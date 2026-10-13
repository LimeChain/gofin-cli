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
    summary: &'static str,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.binding["plan"]["summary"], self.summary);
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
const PLAN_SUMMARY: &str =
    "Withdraw 0.006 ETH; swap and bridge at least 9.5 USDC to Arbitrum, gas up to 0.0013 ETH.";
struct Prices(Arc<ManualClock>, Option<Arc<WorkflowRpc>>);
#[async_trait]
impl fin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<fin_pricing::UsdQuote> {
        Ok(fin_pricing::UsdQuote {
            usd_per_unit: if a.reference == "native" || a.reference == "ETH" {
                if self.1.as_ref().is_some_and(|rpc| {
                    rpc.drift_during_revalidation.load(Ordering::SeqCst)
                        && rpc.quote_calls.load(Ordering::SeqCst) > 1
                }) {
                    "2005"
                } else {
                    "2000"
                }
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
        if self.rpc.reverse {
            return Ok(match r.endpoint {
                E::ApiRestrictions => {
                    json!({"enableReading":true,"enableWithdrawals":true,"enableSpotAndMarginTrading":true})
                }
                E::Account => {
                    json!({"accountType":"SPOT","uid":"1001","balances":[{"asset":"USDC","free":*self.balance.lock().unwrap(),"locked":"0"}]})
                }
                E::CapitalConfig => {
                    json!([{"coin":"USDC","withdrawAllEnable":true,"depositAllEnable":true,"networkList":[{"network":"ARBITRUM","coin":"USDC","contractAddress":fixture::ARB_USDC,"depositEnable":true,"withdrawEnable":true,"withdrawIntegerMultiple":"0.000001","withdrawFee":"0.1","withdrawMin":"1","withdrawMax":"100","withdrawTag":false,"busy":false}]}])
                }
                E::TravelRuleRequirements => json!({"questionnaireCountryCode":"NIL"}),
                E::StandardWithdrawal => {
                    assert_eq!(
                        r.get("amount"),
                        Some("26"),
                        "gross26 includes the fee; never add it again"
                    );
                    assert_eq!(r.get("network"), Some("ARBITRUM"));
                    assert_eq!(self.posts.fetch_add(1, Ordering::SeqCst), 0);
                    *self.time.lock().unwrap() =
                        Some(self.rpc.clock.now().timestamp_millis() as u64);
                    *self.balance.lock().unwrap() = "74";
                    json!({"id":"synthetic-reverse-withdrawal"})
                }
                E::WithdrawalHistory => {
                    let time = self.time.lock().unwrap().unwrap();
                    json!([{"id":"synthetic-reverse-withdrawal","coin":"USDC","amount":"25.9","transactionFee":"0.1","network":"ARBITRUM","address":self.rpc.inner.wallet,"txId":withdrawal_hash(),"status":6,"completeTime":chrono::DateTime::from_timestamp_millis(time as i64).unwrap().format("%Y-%m-%d %H:%M:%S").to_string(),"withdrawOrderId":r.get("withdrawOrderId"),"transferType":0,"walletType":0}])
                }
                other => panic!("unexpected reverse venue endpoint {other:?}"),
            });
        }
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
    human_swap_observed: std::sync::atomic::AtomicBool,
    streaming_withdraw_observed: std::sync::atomic::AtomicBool,
}
impl Client {
    async fn new(runtime: Arc<fin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join("cli.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // This fixture owns its development-protocol socket and isolated journal.
        runtime.engine.recover_interrupted().unwrap();
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
                    let response = fin_protocol::progress::observe(
                        reader.get_mut(),
                        input["progress"].as_bool().unwrap_or(false),
                        runtime.handle(request),
                    )
                    .await;
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
            human_swap_observed: std::sync::atomic::AtomicBool::new(false),
            streaming_withdraw_observed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    async fn call(&self, mut args: Vec<String>, request: BrokerRequest) -> Value {
        if let Ok(binary) = std::env::var("FIN_TIMING_CLI") {
            let socket = self.socket.clone();
            if std::env::var_os("FIN_TIMING_HUMAN").is_some()
                && matches!(&request, BrokerRequest::ExecutePlanStep { step_id, .. } if step_id == "swap")
                && !self.human_swap_observed.swap(true, Ordering::SeqCst)
            {
                let text =
                    tokio::task::spawn_blocking(move || human_progress_cli(binary, socket, args))
                        .await
                        .unwrap();
                for stage in [
                    "Preparing transaction...",
                    "Checking final quote and limits...",
                    "Submitting transaction...",
                    "Verifying outcome...",
                ] {
                    assert!(text.contains(stage), "missing {stage}: {text}");
                }
                assert!(
                    !text.contains("Waiting for Touch ID"),
                    "plan reuses existing authority"
                );
                assert!(!text.contains("Waiting for development approval"));
                assert!(
                    !text.contains('\x1b'),
                    "NO_COLOR must apply to all progress and results"
                );
                let id = text
                    .split("Operation ID: ")
                    .nth(1)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .trim();
                let response = self
                    .runtime
                    .handle(BrokerRequest::Status { id: id.into() })
                    .await;
                assert!(response.ok);
                return response.result.unwrap();
            }
            let streaming_context = match &request {
                BrokerRequest::ExecutePlanStep {
                    plan_id,
                    step_id,
                    request,
                } if step_id == "withdraw"
                    && !self
                        .streaming_withdraw_observed
                        .swap(true, Ordering::SeqCst) =>
                {
                    args.push("--stream".into());
                    Some(
                        json!({"command":"execute","operation":request.operation,"request_id":request.request_id,"plan_id":plan_id,"step_id":step_id}),
                    )
                }
                _ => None,
            };
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
            let value: Value = if let Some(context) = streaming_context {
                let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(events.last().unwrap()["type"], "result", "{events:?}");
                assert!(
                    events[..events.len() - 1]
                        .iter()
                        .all(|event| event["type"] == "progress")
                );
                assert!(events.iter().all(|event| event["context"] == context));
                let stages: Vec<_> = events
                    .iter()
                    .filter_map(|event| event["stage"].as_str())
                    .collect();
                assert_eq!(
                    stages,
                    [
                        "connecting",
                        "preparing",
                        "revalidating",
                        "submitting",
                        "verifying"
                    ]
                );
                assert!(!output.stdout.contains(&0x1b));
                events.last().unwrap()["result"].clone()
            } else {
                serde_json::from_slice(&output.stdout).unwrap()
            };
            assert!(output.status.success(), "CLI failure: {value}");
            value
        } else {
            let response = self.runtime.handle(request).await;
            assert!(response.ok, "broker failure: {:?}", response.error);
            response.result.unwrap()
        }
    }
}
fn human_progress_cli(binary: String, socket: std::path::PathBuf, args: Vec<String>) -> String {
    use std::{
        io::Read,
        os::fd::FromRawFd,
        process::{Command, Stdio},
    };
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let read = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        String::from_utf8(output).unwrap()
    });
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(binary)
        .args([
            "--identity",
            "synthetic-plan-timing",
            "--build-id",
            "synthetic-plan-timing-build",
        ])
        .args(args.into_iter().filter(|arg| arg != "--json"))
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("TERM", "xterm")
        .env("FIN_DEV_IDENTITY", "synthetic-plan-timing")
        .env("FIN_DEV_BUILD_ID", "synthetic-plan-timing-build")
        .env("FIN_DEV_SOCKET", socket)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    let text = read.join().unwrap();
    assert!(status.success(), "human CLI failed: {text}");
    text
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
    run_with_test_stack(rehearse(false, false));
}

#[test]
fn plan_settles_with_improved_requote_and_usd_price_drift_without_reapproval() {
    run_with_test_stack(rehearse(false, true));
}

#[test]
fn pending_relative_swap_reconciles_after_expiry_and_broker_restart_without_resubmission() {
    run_with_test_stack(rehearse(true, false));
}

#[test]
fn gross_usdc_withdrawal_funds_finite_approval_and_native_swap_under_one_plan() {
    run_with_test_stack(rehearse_usdc_withdrawal());
}

async fn rehearse_usdc_withdrawal() {
    const SUMMARY: &str = "Withdraw 26 USDC including 0.1 fee; swap the 25.9 USDC received for ETH, gas up to 0.0008 ETH.";
    let tmp = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock(Mutex::new(chrono::Utc::now())));
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let wallet = fin_evm::lower_address(signer.address());
    let resource = format!("ethereum:42161:{wallet}");
    let inner = Arc::new(fixture::Rpc::destination(wallet.clone()));
    *inner.token_balance.lock().unwrap() = U256::ZERO;
    let rpc = Arc::new(WorkflowRpc::reverse(inner.clone(), clock.clone()));
    let prices = Arc::new(
        SharedUsdValuator::with_source(Arc::new(Prices(clock.clone(), None)))
            .with_clock(clock.clone()),
    );
    let mut config = EthereumConfig::wallet_only(42161, 1, vec![fixture::ARB_USDC.into()]);
    config.weth = Some(WETH.into());
    config.uniswap = Some(UniswapConfig {
        factory: FACTORY.into(),
        quoter: QUOTER.into(),
        router: SWAP_ROUTER.into(),
        router_kind: RouterKind::LegacyV3,
        v4: None,
    });
    let ethereum = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc.clone(), 42161, Some(signer))),
        )
        .unwrap()
        .with_wallet_valuation(prices.clone())
        .with_clock(clock.clone()),
    );
    let usdc = AssetId {
        network: "ethereum:42161".into(),
        reference: fixture::ARB_USDC.into(),
        decimals: 6,
    };
    let eth = AssetId {
        network: "ethereum:42161".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let binance_usdc = AssetId {
        network: "binance".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let venue = Arc::new(Venue {
        rpc: rpc.clone(),
        posts: AtomicUsize::new(0),
        time: Mutex::new(None),
        balance: Mutex::new("100"),
    });
    let binance = Arc::new(
        fin_binance::BinanceAdapter::new(
            fin_binance::BinanceConfig {
                accounts: vec![fin_binance::BinanceAccount {
                    name: "synthetic-main".into(),
                    resource: ResourceId("binance:uid:1001:SPOT".into()),
                    credential: fin_binance::CredentialHandle::new("test-only:synthetic-reverse")
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
                    chain_id: 42161,
                    confirmations: 1,
                    assets: BTreeMap::from([("USDC".into(), usdc.clone())]),
                }],
            },
            venue.clone(),
            Some(prices.clone()),
            Some(Arc::new(Recipient(ethereum.clone()))),
        )
        .unwrap(),
    );
    let approval = Arc::new(Approval {
        clock: clock.clone(),
        count: AtomicUsize::new(0),
        summary: SUMMARY,
    });
    let runtime = compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(tmp.path().join("journal.sqlite")).unwrap(),
            adapters: vec![binance, ethereum],
            authorizer: approval.clone(),
            valuator: prices,
            bridge: None,
        },
        clock.clone(),
    )
    .unwrap();
    let client = Client::new(runtime, tmp.path()).await;
    let withdraw = json!({"requestId":"SYNTHETIC-reverse-withdraw","account":"synthetic-main","destination":"synthetic-wallet","coin":"USDC","amount":"26","network":"ARBITRUM","walletType":"SPOT","maxFee":"0.1","minReceiveAmount":"25.9"});
    let permission = json!({"wallet":"synthetic-wallet","network":"evm","chain_id":42161,"request_id":"SYNTHETIC-reverse-approve","token":fixture::ARB_USDC,"spender":SWAP_ROUTER,"amount":"25.9","max_fee_eth":"0.0003"});
    let swap = json!({"wallet":"synthetic-wallet","network":"evm","chain_id":42161,"request_id":"SYNTHETIC-reverse-swap","token_in":fixture::ARB_USDC,"token_out":"native","amount":"25.9","recipient":wallet,"min_output":"0.0128","slippage_bps":100,"max_price_impact_bps":10000,"fee_tier":500,"protocol":"v3","max_fee_eth":"0.0005"});
    let gross = resource_amount(
        "binance:uid:1001:SPOT",
        &binance_usdc,
        &Quantity::from_decimal("26", 36).unwrap().to_string(),
    );
    let withheld = CostBound {
        location: AccountingLocation::Cash {
            resource: ResourceId("binance:uid:1001:SPOT".into()),
        },
        amount: Amount {
            asset: binance_usdc.clone(),
            atoms: Quantity::from_decimal("0.1", 36).unwrap(),
        },
        treatment: CostTreatment::IncludedInDebit,
    };
    let mut withdrawal_step = step(
        "withdraw",
        "binance.withdraw",
        withdraw.clone(),
        json!({"mode":"exact","maximum_debits":[gross],"maximum_costs":[withheld],"possible_credits":[{"resource":resource,"asset":usdc}]}),
        vec![],
        None,
    );
    withdrawal_step.completion.minimum_net_receipt =
        Some(resource_amount(&resource, &usdc, "25900000"));
    let mut funding = dependency("withdraw", &resource, &usdc);
    funding.funding_scope = Some(FinancialPlanFundingScope::PrincipalOnly);
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Withdraw USDC and buy ETH".into(),
        summary: Some(SUMMARY.into()),
        steps: vec![
            withdrawal_step,
            step(
                "approve",
                "wallet.approve",
                permission.clone(),
                json!({"mode":"exact","maximum_debits":[],"maximum_costs":[fee(&resource,&eth,"300000000000000")],"possible_credits":[]}),
                vec![],
                None,
            ),
            step(
                "swap",
                "uniswap.swap",
                swap.clone(),
                json!({"max_input":"25.9","max_fee_eth":"0.0005","max_slippage_bps":100,"max_price_impact_bps":10000,"minimum_rate":{"input_atoms":"25900000","output_atoms":"12800000000000000"},"allowed_fee_tiers":[500],"max_deadline_seconds":600}),
                vec![funding],
                Some(600),
            ),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![gross, resource_amount(&resource, &usdc, "25900000")],
            maximum_costs: vec![withheld, fee(&resource, &eth, "800000000000000")],
        },
    };
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-reverse-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(2700),
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
                "--valid-for",
                "45m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
        )
        .await;
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
                "--valid-for",
                "45m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    let mut withdrawal_id: Option<String> = None;
    for (step_id, operation, input) in [
        ("withdraw", "binance.withdraw", withdraw),
        ("approve", "wallet.approve", permission),
        ("swap", "uniswap.swap", swap),
    ] {
        if step_id == "approve" {
            assert_eq!(rpc.approvals.load(Ordering::SeqCst), 0);
            assert_eq!(rpc.swaps.load(Ordering::SeqCst), 0);
            clock.advance(661);
            rpc.withdrawal_ready.store(true, Ordering::SeqCst);
            let status = client
                .call(
                    args(&["plans", "status", &plan_id, "--json"]),
                    BrokerRequest::PlanStatus {
                        plan_id: plan_id.clone(),
                    },
                )
                .await;
            assert_eq!(status["steps"][0]["state"], "completed", "{status}");
            let original = client
                .call(
                    args(&["status", withdrawal_id.as_ref().unwrap(), "--json"]),
                    BrokerRequest::Status {
                        id: withdrawal_id.clone().unwrap(),
                    },
                )
                .await;
            let record: OperationRecord = serde_json::from_value(original).unwrap();
            assert!(record.effects.unwrap().effects.iter().any(|effect|matches!(effect,FinancialEffect::Transfer {debit,credit,..} if debit.atoms.to_decimal(36)=="26" && credit.atoms.to_decimal(6)=="25.9")));
        }
        let request = OperationRequest {
            request_id: format!("SYNTHETIC-reverse-{step_id}-attempt"),
            operation: operation.into(),
            input: input.clone(),
            run_id: None,
        };
        let call_args = args(&[
            "execute",
            operation,
            "--input",
            &input.to_string(),
            "--id",
            &request.request_id,
            "--plan",
            &plan_id,
            "--step",
            step_id,
            "--json",
        ]);
        let result = client
            .call(
                call_args.clone(),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: step_id.into(),
                    request: request.clone(),
                },
            )
            .await;
        assert_eq!(
            result["status"],
            if step_id == "withdraw" {
                "pending"
            } else {
                "completed"
            },
            "{result}"
        );
        if step_id == "withdraw" {
            withdrawal_id = Some(result["id"].as_str().unwrap().to_owned());
        }
        if step_id == "swap" {
            assert_eq!(
                result["preview"]["input"]["deadline"],
                clock.now().timestamp() as u64 + 600
            );
        }
        let duplicate = client
            .call(
                call_args,
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: step_id.into(),
                    request,
                },
            )
            .await;
        assert_eq!(duplicate["id"], result["id"]);
    }
    let status = client
        .call(
            args(&["plans", "status", &plan_id, "--json"]),
            BrokerRequest::PlanStatus { plan_id },
        )
        .await;
    assert!(
        status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|step| step["state"] == "completed")
    );
    assert_eq!(*venue.balance.lock().unwrap(), "74");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.swaps.load(Ordering::SeqCst), 1);
    assert_eq!(inner.broadcasts(), 0, "no bridge writes");
    assert_eq!(approval.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        rpc.runtime_balance().await,
        U256::from(112_949_999_999_860_000u64)
    );
}

async fn rehearse(pending_swap: bool, price_drift: bool) {
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
        SharedUsdValuator::with_source(Arc::new(Prices(clock.clone(), Some(source.clone()))))
            .with_clock(clock.clone()),
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
        summary: PLAN_SUMMARY,
    });
    // A killed standalone preparation must not block the next named plan after
    // an update. Seed the exact no-preview/no-submission durable shape seen live.
    let mut journal = Store::open(tmp.path().join("journal.sqlite")).unwrap();
    let interrupted = OperationRecord {
        id: "SYNTHETIC-interrupted-preparation".into(),
        request_id: "SYNTHETIC-interrupted-request".into(),
        fingerprint: "SYNTHETIC-interrupted-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.deposit".into(),
        input: json!({}),
        status: Status::AwaitingApproval,
        created_at: clock.now().to_rfc3339(),
        updated_at: clock.now().to_rfc3339(),
        preview: None,
        approval: None,
        submission: None,
        dispatch_evidence: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    journal
        .create_request(
            &interrupted.id,
            &interrupted.request_id,
            &interrupted.fingerprint,
            &interrupted.operation,
            None,
            "awaiting_approval",
            &serde_json::to_value(&interrupted).unwrap(),
            &interrupted.created_at,
        )
        .unwrap();
    drop(journal);
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
    let client = Client::new(runtime.clone(), tmp.path()).await;
    let recovered = runtime.engine.status(&interrupted.id, false).await.unwrap();
    assert_eq!(recovered.status, Status::Cancelled);
    assert!(recovered.submission.is_none());
    assert_eq!(recovered.error.unwrap().code, "INTERRUPTED_APPROVAL");
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
        summary: Some(PLAN_SUMMARY.into()),
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
    assert_eq!(review["plan"]["summary"], PLAN_SUMMARY);
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
    assert_eq!(authorized["revision"]["spec"]["summary"], PLAN_SUMMARY);
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
        if id == "swap" && price_drift {
            // Age preparation beyond the 30-second reuse window, forcing
            // final quote/USD refresh while the original candidate stays valid.
            let preparation_clock = clock.clone();
            *source.advance_during_preparation.lock().unwrap() =
                Some(Box::new(move || preparation_clock.advance(31)));
            // The refreshed evidence improves by 0.5% / rises by 0.25%.
            source.quote_calls.store(0, Ordering::SeqCst);
            source
                .drift_during_revalidation
                .store(true, Ordering::SeqCst);
        }
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
        if id == "swap" && price_drift {
            assert_eq!(source.quote_calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                result["preview"]["valuation"]["movement_usd_micros"],
                "8080000"
            );
            let native_price = result["preview"]["valuation"]["prices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["asset"]["reference"] == "native")
                .unwrap();
            assert_eq!(native_price["usd_micros_per_unit"], "2005000000");
            // This scenario isolates drift during swap revalidation; the
            // bridge's separate, explicitly USD-capped fees keep their fixture price.
            source
                .drift_during_revalidation
                .store(false, Ordering::SeqCst);
        }
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
