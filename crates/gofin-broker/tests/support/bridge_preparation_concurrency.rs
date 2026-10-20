use super::*;
use gofin_evm::EvmRpc;
use gofin_pricing::{UsdPriceSource, UsdQuote};
use std::sync::{Mutex, atomic::AtomicBool};

struct PreparationRpc {
    inner: Arc<Rpc>,
    estimated: Arc<AtomicBool>,
    code_reads: Mutex<Vec<String>>,
    missing_code: bool,
}
#[async_trait]
impl EvmRpc for PreparationRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "eth_estimateGas" {
            self.estimated.store(true, Ordering::SeqCst);
        }
        if method == "eth_getCode" {
            self.code_reads
                .lock()
                .unwrap()
                .push(params[0].as_str().unwrap().into());
            if self.missing_code {
                return Ok(json!("0x"));
            }
        }
        self.inner.request(method, params).await
    }
}
struct PreparationPrices {
    estimated: Arc<AtomicBool>,
    initial: tokio::sync::Barrier,
    final_prices: tokio::sync::Barrier,
    initial_calls: AtomicUsize,
    final_calls: AtomicUsize,
}
#[async_trait]
impl UsdPriceSource for PreparationPrices {
    async fn quotes(&self, assets: &[gofin_domain::AssetId]) -> Result<Vec<UsdQuote>> {
        if self.estimated.load(Ordering::SeqCst) {
            // The late policy batch must overlap frozen-gas pricing.
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            self.final_prices.wait().await;
        } else {
            // Quote prices cannot finish unless the independent finite-cap
            // batch has also started. No elapsed-time threshold is needed.
            self.initial_calls.fetch_add(1, Ordering::SeqCst);
            self.initial.wait().await;
        }
        Prices.quotes(assets).await
    }
    async fn quote(&self, asset: &gofin_domain::AssetId) -> Result<UsdQuote> {
        if self.estimated.load(Ordering::SeqCst) {
            self.final_calls.fetch_add(1, Ordering::SeqCst);
            self.final_prices.wait().await;
        }
        Prices.quote(asset).await
    }
}

#[tokio::test]
async fn allowance_preparation_overlaps_independent_prices_and_checks_token_once() {
    for missing_code in [false, true] {
        let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
        let steps: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        let approval = steps
            .iter()
            .find(|step| step.kind == DomainStepKind::Approval)
            .unwrap();
        let input = h.step_input(transfer, approval, Some(h.bounds()));
        let expected = h.bridge.prepare("bridge.execute", &input).await.unwrap();
        let estimated = Arc::new(AtomicBool::new(false));
        let rpc = Arc::new(PreparationRpc {
            inner: h.source.clone(),
            estimated: estimated.clone(),
            code_reads: Mutex::new(vec![]),
            missing_code,
        });
        let runtime = Arc::new(EvmRuntime::new(
            rpc.clone(),
            1,
            Some(Arc::new(
                LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap(),
            )),
        ));
        let wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: h.wallet.clone(),
                    signing_enabled: true,
                }],
                runtime.clone(),
            )
            .unwrap(),
        );
        let prices = Arc::new(PreparationPrices {
            estimated,
            initial: tokio::sync::Barrier::new(2),
            final_prices: tokio::sync::Barrier::new(2),
            initial_calls: AtomicUsize::new(0),
            final_calls: AtomicUsize::new(0),
        });
        let bridge = BridgeAdapter::with_provider_and_observers(
            h.provider.clone(),
            BTreeMap::from([(1, wallet)]),
            BTreeMap::from([(1, runtime)]),
            Arc::new(SharedUsdValuator::with_source(prices.clone())),
        )
        .unwrap();
        bridge.attach_state(h.state.clone()).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bridge.prepare("bridge.execute", &input),
        )
        .await
        .expect("independent observations must overlap, not deadlock");
        assert_eq!(prices.initial_calls.load(Ordering::SeqCst), 2);
        assert_eq!(*rpc.code_reads.lock().unwrap(), vec![ETH_USDC]);
        if missing_code {
            assert_eq!(result.unwrap_err().code, "CONTRACT_UNAVAILABLE");
            assert!(!rpc.estimated.load(Ordering::SeqCst));
        } else {
            let prepared = result.unwrap();
            assert_eq!(prepared.binding, expected.binding);
            assert_eq!(prepared.intent, expected.intent);
            let actual = prepared.valuation.unwrap();
            let expected = expected.valuation.unwrap();
            assert_eq!(actual.movement_usd_micros, expected.movement_usd_micros);
            assert_eq!(
                actual.maximum_fee_usd_micros,
                expected.maximum_fee_usd_micros
            );
            assert_eq!(
                actual
                    .prices
                    .iter()
                    .map(|p| (&p.asset, &p.usd_micros_per_unit, &p.unit_atoms))
                    .collect::<Vec<_>>(),
                expected
                    .prices
                    .iter()
                    .map(|p| (&p.asset, &p.usd_micros_per_unit, &p.unit_atoms))
                    .collect::<Vec<_>>()
            );
            assert_eq!(prices.final_calls.load(Ordering::SeqCst), 2);
        }
        assert_eq!(h.source.broadcasts(), 0);
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 0);
        assert!(
            h.state
                .transfer(transfer)
                .unwrap()
                .unwrap()
                .steps
                .iter()
                .all(|s| s.operation_id.is_none())
        );
    }
}

struct RevalidationRpc {
    inner: Arc<Rpc>,
    chains_started: Arc<tokio::sync::Barrier>,
    calls: Mutex<Vec<(String, Value)>>,
    wrong_decimals: bool,
}
#[async_trait]
impl EvmRpc for RevalidationRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), params.clone()));
        if method == "eth_blockNumber" {
            // Each fresh metadata chain must start before either can finish.
            self.chains_started.wait().await;
        }
        if self.wrong_decimals
            && method == "eth_call"
            && params[0]["data"]
                .as_str()
                .is_some_and(|data| data.starts_with("0x313ce567"))
        {
            return Ok(json!(format!("0x{:064x}", 18)));
        }
        self.inner.request(method, params).await
    }
}
struct RevalidationPrices {
    phases: tokio::sync::Barrier,
    singles: AtomicUsize,
    batches: AtomicUsize,
}
#[async_trait]
impl UsdPriceSource for RevalidationPrices {
    async fn quotes(&self, assets: &[gofin_domain::AssetId]) -> Result<Vec<UsdQuote>> {
        self.batches.fetch_add(1, Ordering::SeqCst);
        self.phases.wait().await;
        Prices.quotes(assets).await
    }
    async fn quote(&self, asset: &gofin_domain::AssetId) -> Result<UsdQuote> {
        // Frozen-gas pricing must start alongside the quote, selected-cap and
        // policy batches. The later selected-cap USD ceiling keeps its ordering.
        if self.singles.fetch_add(1, Ordering::SeqCst) == 0 {
            self.phases.wait().await;
        }
        Prices.quote(asset).await
    }
}

#[tokio::test]
async fn allowance_revalidation_deduplicates_exact_assets_and_overlaps_chains_and_prices() {
    for wrong_decimals in [false, true] {
        let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
        let mut quote_input = h.quote_input(None);
        quote_input["intent"]["allowedRefundAssets"]
            .as_array_mut()
            .unwrap()
            .push(json!({"chainId":42161,"token":ARB_USDC}));
        let quote = h.bridge.read("bridge.quotes", &quote_input).await.unwrap();
        let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
        let steps: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        let approval = steps
            .iter()
            .find(|step| step.kind == DomainStepKind::Approval)
            .unwrap();
        let prepared = h
            .bridge
            .prepare(
                "bridge.execute",
                &h.step_input(transfer, approval, Some(h.bounds())),
            )
            .await
            .unwrap();
        let chains_started = Arc::new(tokio::sync::Barrier::new(2));
        let source = Arc::new(RevalidationRpc {
            inner: h.source.clone(),
            chains_started: chains_started.clone(),
            calls: Mutex::new(vec![]),
            wrong_decimals: false,
        });
        let destination = Arc::new(RevalidationRpc {
            inner: h.destination.clone(),
            chains_started,
            calls: Mutex::new(vec![]),
            wrong_decimals,
        });
        let source_runtime = Arc::new(EvmRuntime::new(
            source.clone(),
            1,
            Some(Arc::new(
                LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap(),
            )),
        ));
        let destination_runtime = Arc::new(EvmRuntime::new(destination.clone(), 42161, None));
        let wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: h.wallet.clone(),
                    signing_enabled: true,
                }],
                source_runtime.clone(),
            )
            .unwrap(),
        );
        let prices = Arc::new(RevalidationPrices {
            phases: tokio::sync::Barrier::new(4),
            singles: AtomicUsize::new(0),
            batches: AtomicUsize::new(0),
        });
        let bridge = BridgeAdapter::with_provider_and_observers(
            h.provider.clone(),
            BTreeMap::from([(1, wallet)]),
            BTreeMap::from([(1, source_runtime), (42161, destination_runtime)]),
            Arc::new(SharedUsdValuator::with_source(prices.clone())),
        )
        .unwrap();
        bridge.attach_state(h.state.clone()).unwrap();
        for _ in 0..if wrong_decimals { 1 } else { 2 } {
            source.calls.lock().unwrap().clear();
            destination.calls.lock().unwrap().clear();
            prices.singles.store(0, Ordering::SeqCst);
            prices.batches.store(0, Ordering::SeqCst);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                bridge.revalidate(&prepared),
            )
            .await
            .expect("independent chain and pricing phases must overlap");
            if wrong_decimals {
                assert!(
                    result
                        .unwrap_err()
                        .message
                        .contains("precision or deployment changed")
                );
                assert_eq!(prices.batches.load(Ordering::SeqCst), 0);
            } else {
                let refreshed = result.unwrap();
                assert_eq!(refreshed.binding, prepared.binding);
                assert_eq!(refreshed.intent, prepared.intent);
                let valuation = refreshed.valuation.unwrap();
                let prior = prepared.valuation.as_ref().unwrap();
                assert_eq!(valuation.movement_usd_micros, prior.movement_usd_micros);
                assert_eq!(
                    valuation.maximum_fee_usd_micros,
                    prior.maximum_fee_usd_micros
                );
                assert_eq!(prices.batches.load(Ordering::SeqCst), 3);
                assert_eq!(prices.singles.load(Ordering::SeqCst), 2);
                // Fresh every revalidation, even though source/destination each
                // occurs again in refund scope. No cross-attempt metadata cache.
                for (rpc, count) in [(&source, 11), (&destination, 4)] {
                    let calls = rpc.calls.lock().unwrap();
                    assert_eq!(calls.len(), count, "{calls:?}");
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|(method, _)| method == "eth_blockNumber")
                            .count(),
                        1
                    );
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|(method, params)| method == "eth_call"
                                && params[0]["data"]
                                    .as_str()
                                    .is_some_and(|data| data.starts_with("0x313ce567")))
                            .count(),
                        1
                    );
                }
                let calls = source.calls.lock().unwrap();
                assert_eq!(
                    calls
                        .iter()
                        .filter(|(method, params)| method == "eth_getCode" && params[1] == "latest")
                        .count(),
                    1
                );
                for required in [
                    "eth_getTransactionCount",
                    "eth_estimateGas",
                    "eth_getBalance",
                    "eth_getBlockByNumber",
                ] {
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|(method, _)| method == required)
                            .count(),
                        1,
                        "{required}"
                    );
                }
            }
        }
        assert_eq!(h.source.broadcasts(), 0);
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 0);
    }
}

struct StalledPreparationRpc {
    inner: Arc<Rpc>,
    method: &'static str,
    target: &'static str,
    cancelled: Arc<AtomicUsize>,
}
#[async_trait]
impl EvmRpc for StalledPreparationRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == self.method && params[0]["to"].as_str() == Some(self.target) {
            struct Cancelled(Arc<AtomicUsize>);
            impl Drop for Cancelled {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            let _cancelled = Cancelled(self.cancelled.clone());
            return std::future::pending().await;
        }
        self.inner.request(method, params).await
    }
}

#[tokio::test(start_paused = true)]
async fn deposit_preparation_diagnostics_identify_pending_allowance_simulation_and_estimate() {
    use gofin_domain::evidence_phase::{Observation, Phase};
    for (method, target, expected) in [
        ("eth_call", ETH_USDC, vec![Phase::BridgeAllowance]),
        (
            "eth_call",
            fixture::DEPOSITORY,
            vec![Phase::BridgeTransactionPreparation, Phase::EvmSimulation],
        ),
        (
            "eth_estimateGas",
            fixture::DEPOSITORY,
            vec![Phase::BridgeTransactionPreparation, Phase::EvmGasEstimate],
        ),
    ] {
        let h =
            Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::from(12_000_000));
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
        let steps: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        let deposit = steps
            .iter()
            .find(|step| step.kind == DomainStepKind::Deposit)
            .unwrap();
        let input = h.step_input(transfer, deposit, Some(h.bounds()));
        let cancelled = Arc::new(AtomicUsize::new(0));
        let rpc = Arc::new(StalledPreparationRpc {
            inner: h.source.clone(),
            method,
            target,
            cancelled: cancelled.clone(),
        });
        let runtime = Arc::new(EvmRuntime::new(
            rpc,
            1,
            Some(Arc::new(
                LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap(),
            )),
        ));
        let wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: h.wallet.clone(),
                    signing_enabled: true,
                }],
                runtime.clone(),
            )
            .unwrap(),
        );
        let bridge = BridgeAdapter::with_provider_and_observers(
            h.provider.clone(),
            BTreeMap::from([(1, wallet)]),
            BTreeMap::from([(1, runtime)]),
            Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
        )
        .unwrap();
        bridge.attach_state(h.state.clone()).unwrap();
        let observation = Observation::default();
        let mut work = Box::pin(observation.observe(bridge.prepare("bridge.execute", &input)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(120), &mut work)
                .await
                .is_err()
        );
        assert_eq!(observation.pending(), expected, "{method} {target}");
        drop(work);
        assert!(observation.pending().is_empty());
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(h.source.broadcasts(), 0);
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn settled_allowance_status_identifies_the_next_ready_child_without_submitting_it() {
    let h = Harness::new();
    let quote = h
        .bridge
        .read("bridge.quotes", &h.quote_input(None))
        .await
        .unwrap();
    let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
    let steps: Vec<BridgeStepLink> =
        serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
    let reset = steps
        .iter()
        .find(|step| step.kind == DomainStepKind::ResetAllowance)
        .unwrap();
    let approval = steps
        .iter()
        .find(|step| step.kind == DomainStepKind::Approval)
        .unwrap();
    let deposit = steps
        .iter()
        .find(|step| step.kind == DomainStepKind::Deposit)
        .unwrap();
    for (step, next, request) in [
        (reset, approval, "status-reset"),
        (approval, deposit, "status-approval"),
    ] {
        let operation = h.execute_exact(transfer, step, request, &h.bounds()).await;
        assert_eq!(operation.status, Status::Completed);
        let broadcasts = h.source.broadcasts();
        let status = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(status["state"], "approval_pending");
        let action = status["next_action"].as_str().unwrap();
        assert!(action.contains("ready for preview"), "{action}");
        assert!(action.contains(&next.step_id), "{action}");
        assert!(!action.contains("Reconcile the original approval"));
        let saved = h.state.transfer(transfer).unwrap().unwrap();
        let next = saved
            .steps
            .iter()
            .find(|link| link.step_id == next.step_id)
            .unwrap();
        assert_eq!(next.state, gofin_domain::BridgeStepState::Unsubmitted);
        assert!(next.operation_id.is_none());
        assert_eq!(h.source.broadcasts(), broadcasts);
    }
    // Repeated observation leaves the ready deposit unclaimed and uses the same
    // authoritative dependency states rather than changing the parent enum.
    assert_eq!(h.source.broadcasts(), 2);
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 2);
}
