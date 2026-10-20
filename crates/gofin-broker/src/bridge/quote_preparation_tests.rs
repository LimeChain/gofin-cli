use super::*;
use std::sync::atomic::AtomicI64;
use std::time::Duration;

struct TestClock(AtomicI64);
impl gofin_domain::clock::Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
    }
}
struct Rpc {
    calls: AtomicUsize,
    allowance_mode: AtomicUsize,
    clock: Arc<TestClock>,
}
#[async_trait]
impl gofin_evm::EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!("0x10"),
            "eth_getCode" => json!("0x6000"),
            "eth_call" => {
                if params[0]["data"]
                    .as_str()
                    .unwrap()
                    .starts_with("0xdd62ed3e")
                {
                    match self.allowance_mode.load(Ordering::SeqCst) {
                        1 => return std::future::pending().await,
                        2 => {
                            self.clock.0.fetch_add(120, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                    json!(format!("0x{:064x}", 100_000_000u64))
                } else {
                    json!(format!("0x{:064x}", 6))
                }
            }
            _ => panic!("unexpected RPC {method}"),
        })
    }
}
struct Provider {
    stall: AtomicBool,
    expired: AtomicBool,
    second_allowance: AtomicBool,
    clock: Arc<TestClock>,
}
#[async_trait]
impl BridgeProvider for Provider {
    fn capabilities(&self) -> gofin_bridge::BridgeCapabilities {
        gofin_bridge::BridgeCapabilities {
            provider: "relay".into(),
            chains: [1, 42161].into_iter().collect(),
            native_eth: true,
            erc20: true,
            protocol_versions: Default::default(),
        }
    }
    async fn quote(
        &self,
        request: &BridgeQuoteRequest,
    ) -> gofin_domain::Result<gofin_bridge::BridgeQuoteBatch> {
        if self.stall.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        let (_, prepared) = bridge_execute_candidate_fixture();
        let mut quote: BridgeQuote =
            serde_json::from_value(prepared.binding["quote"].clone()).unwrap();
        quote.intent = request.intent.clone();
        quote.gross_debit = request
            .intent
            .source_amount(request.intent.amount.clone())?;
        quote.expected_receipt =
            request.intent.destination_assets[0].amount(Quantity::new("10").unwrap());
        quote.minimum_receipt = quote.expected_receipt.clone();
        quote.valid_until = (self.clock.now()
            + chrono::Duration::seconds(if self.expired.load(Ordering::SeqCst) {
                -1
            } else {
                60
            }))
        .to_rfc3339();
        let mut quotes = vec![quote.clone()];
        if self.second_allowance.load(Ordering::SeqCst) {
            quote.approval = Some(gofin_bridge::ApprovalRequirement {
                token: ETHEREUM_USDC.into(),
                spender: RELAY_DEPOSITORY.into(),
                amount: request.intent.amount.clone(),
            });
            quotes.push(quote);
        }
        Ok(gofin_bridge::BridgeQuoteBatch {
            quotes,
            failures: vec![],
        })
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> gofin_domain::Result<PreparedBridge> {
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            source_order_id: None,
            fixed_scope_digest: "synthetic".into(),
            steps: vec![BridgeStep {
                ordinal: 0,
                kind: ProviderStepKind::Deposit,
                call: Some(EvmCall {
                    chain_id: 1,
                    to: RELAY_DEPOSITORY.into(),
                    value: Quantity::zero(),
                    data: "0x".into(),
                }),
                solana_mint: None,
                solana_transaction: None,
                expected_sender: wallet.into(),
                maximum_debit: vec![quote.gross_debit.clone()],
                dependencies: vec![],
            }],
        })
    }
    async fn reconcile(
        &self,
        _: &BridgeTransferRef,
    ) -> gofin_domain::Result<gofin_bridge::BridgeReconciliation> {
        unreachable!()
    }
}
fn fixture() -> (
    BridgeAdapter,
    Arc<Provider>,
    Arc<Rpc>,
    BridgeStateRepository,
    Value,
    tempfile::TempDir,
) {
    use gofin_domain::clock::Clock;
    let clock = Arc::new(TestClock(AtomicI64::new(chrono::Utc::now().timestamp())));
    let rpc = Arc::new(Rpc {
        calls: AtomicUsize::new(0),
        allowance_mode: AtomicUsize::new(0),
        clock: clock.clone(),
    });
    let runtime = Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 1, None));
    let wallet = Arc::new(
        gofin_ethereum::EthereumAdapter::new(
            gofin_ethereum::EthereumConfig::wallet_only(1, 1, vec![]),
            vec![gofin_ethereum::WalletConnection {
                name: "test".into(),
                address: "0x1111111111111111111111111111111111111111".into(),
                signing_enabled: false,
            }],
            runtime.clone(),
        )
        .unwrap(),
    );
    let provider = Arc::new(Provider {
        stall: AtomicBool::new(false),
        expired: AtomicBool::new(false),
        second_allowance: AtomicBool::new(false),
        clock: clock.clone(),
    });
    let adapter = BridgeAdapter::with_providers(
        vec![provider.clone()],
        BTreeMap::from([(1, wallet)]),
        BTreeMap::from([(1, runtime)]),
        Arc::new(SharedUsdValuator::with_source(Arc::new(FixedBridgePrices))),
    )
    .unwrap()
    .with_clock(clock.clone());
    let directory = tempfile::tempdir().unwrap();
    let state = BridgeStateRepository::new(Arc::new(Mutex::new(
        gofin_store::Store::open(directory.path().join("state.db")).unwrap(),
    )));
    adapter.attach_state(state.clone()).unwrap();
    let input = json!({"provider":"relay","intent":{"wallet":"test","source":{"chainId":1,"token":ETHEREUM_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"native"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":ETHEREUM_USDC}],"amount":"1000000"}});
    assert!(clock.now() > chrono::DateTime::UNIX_EPOCH);
    (adapter, provider, rpc, state, input, directory)
}
use gofin_domain::clock::Clock;

#[tokio::test]
async fn quote_metadata_reuses_verified_exact_identity_but_not_network() {
    let (adapter, _, rpc, _, input, _directory) = fixture();
    adapter.quote(&input).await.unwrap();
    assert_eq!(
        rpc.calls.load(Ordering::SeqCst),
        4,
        "source/refund share one pinned identity observation"
    );
    let mut observations = BTreeMap::new();
    let token = BridgeAssetInput {
        chain_id: 1,
        token: Some(ETHEREUM_USDC.into()),
    };
    adapter
        .quote_asset(&token, 1, &mut observations)
        .await
        .unwrap();
    let mixed = BridgeAssetInput {
        chain_id: 1,
        token: Some(ETHEREUM_USDC.to_ascii_uppercase().replacen("0X", "0x", 1)),
    };
    assert_eq!(
        adapter
            .quote_asset(&mixed, 1, &mut observations)
            .await
            .unwrap()
            .decimals,
        6
    );
    assert_eq!(rpc.calls.load(Ordering::SeqCst), 8);
    assert_eq!(
        adapter
            .quote_asset(&token, 42161, &mut observations)
            .await
            .unwrap_err()
            .code,
        "BRIDGE_ASSET_CHAIN_MISMATCH"
    );
}

#[tokio::test]
async fn quote_timeout_preserves_existing_refresh_and_reports_phase() {
    let (adapter, provider, _, state, mut input, _directory) = fixture();
    let output = adapter.quote(&input).await.unwrap();
    input["refreshTransferId"] = output["transfers"][0]["transfer_id"].clone();
    let before = serde_json::to_value(state.transfers().unwrap()).unwrap();
    provider.stall.store(true, Ordering::SeqCst);
    let error = adapter
        .quote_with_timeout(&input, Duration::from_millis(20))
        .await
        .unwrap_err();
    assert_eq!(error.code, "BRIDGE_QUOTE_TIMEOUT");
    assert_eq!(error.details.as_ref().unwrap()["phase"], "provider_quote");
    assert_eq!(error.details.as_ref().unwrap()["provider"], "relay");
    assert_eq!(
        error.details.as_ref().unwrap()["refresh_transfer_id"],
        input["refreshTransferId"]
    );
    assert_eq!(
        serde_json::to_value(state.transfers().unwrap()).unwrap(),
        before
    );
}

#[tokio::test]
async fn quote_timeout_does_not_save_earlier_option() {
    let (adapter, provider, rpc, state, input, _directory) = fixture();
    provider.second_allowance.store(true, Ordering::SeqCst);
    rpc.allowance_mode.store(1, Ordering::SeqCst);
    let error = adapter
        .quote_with_timeout(&input, Duration::from_millis(20))
        .await
        .unwrap_err();
    assert_eq!(error.code, "BRIDGE_QUOTE_TIMEOUT");
    assert_eq!(error.details.as_ref().unwrap()["phase"], "allowance");
    assert!(state.transfers().unwrap().is_empty());
}

#[tokio::test]
async fn quote_expiry_after_allowance_cannot_persist_any_option() {
    let (adapter, provider, rpc, state, input, _directory) = fixture();
    provider.second_allowance.store(true, Ordering::SeqCst);
    rpc.allowance_mode.store(2, Ordering::SeqCst);
    assert_eq!(
        adapter.quote(&input).await.unwrap_err().code,
        "BRIDGE_QUOTE_EXPIRED"
    );
    assert!(state.transfers().unwrap().is_empty());
}

#[tokio::test]
async fn expired_provider_quote_is_rejected_before_allowance() {
    let (adapter, provider, rpc, state, input, _directory) = fixture();
    provider.expired.store(true, Ordering::SeqCst);
    provider.second_allowance.store(true, Ordering::SeqCst);
    rpc.allowance_mode.store(1, Ordering::SeqCst);
    assert_eq!(
        adapter.quote(&input).await.unwrap_err().code,
        "BRIDGE_QUOTE_EXPIRED"
    );
    assert_eq!(rpc.calls.load(Ordering::SeqCst), 4);
    assert!(state.transfers().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_provider_selection_fails_before_network_observation() {
    let (adapter, provider, rpc, state, input, _directory) = fixture();
    provider.stall.store(true, Ordering::SeqCst);
    for (provider_id, cctp) in [
        (Some("untrusted-provider\u{1b}[2J"), None),
        (Some("relay"), Some(json!({"transferMode":"standard"}))),
        (None, Some(json!({"transferMode":"standard"}))),
    ] {
        let mut invalid = input.clone();
        invalid["provider"] = json!(provider_id);
        if let Some(cctp) = cctp {
            invalid["cctp"] = cctp;
        }
        let error = adapter
            .quote_with_timeout(&invalid, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_ne!(error.code, "BRIDGE_QUOTE_TIMEOUT");
        assert_eq!(error.stage, "validation");
        if provider_id != Some("relay") {
            assert_eq!(error.code, "BRIDGE_PROVIDER_UNAVAILABLE");
            assert!(error.message.contains("selected bridge provider"));
        }
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("untrusted-provider")
        );
        assert_eq!(rpc.calls.load(Ordering::SeqCst), 0);
        assert!(state.transfers().unwrap().is_empty());
    }
}

#[tokio::test]
async fn expired_source_preparation_refresh_preserves_identity_bounds_and_journal() {
    let (adapter, provider, _, state, input, _directory) = fixture();
    let output = adapter.quote(&input).await.unwrap();
    let id = output["transfers"][0]["transfer_id"].as_str().unwrap();
    let before = state.transfer(id).unwrap().unwrap();
    let saved = adapter.saved_quote(&before).unwrap();
    assert!(!adapter.refresh_expired_source_quote(&before, &saved, &before.steps[0]).await.unwrap());
    provider.clock.0.fetch_add(61, Ordering::SeqCst);
    assert!(adapter.refresh_expired_source_quote(&before, &saved, &before.steps[0]).await.unwrap());
    let refreshed = state.transfer(id).unwrap().unwrap();
    assert_eq!(refreshed.id, before.id);
    assert_eq!(refreshed.steps, before.steps);
    assert_eq!(refreshed.metadata, before.metadata);
    assert_eq!(refreshed.approved_bounds, before.approved_bounds);
    assert_eq!(refreshed.plan_scope, before.plan_scope);
    assert_eq!(refreshed.quote_revisions.len(), before.quote_revisions.len() + 1);
    assert_eq!(state.transfers().unwrap().len(), 1);
    let fresh = adapter.saved_quote(&refreshed).unwrap();
    assert_eq!(fresh.bounds, saved.bounds);
    assert_eq!(fresh.quote.provider, saved.quote.provider);
    assert_eq!(fresh.quote.route_id, saved.quote.route_id);
    assert!(!adapter.refresh_expired_source_quote(&refreshed, &fresh, &refreshed.steps[0]).await.unwrap());
}

#[tokio::test]
async fn automatic_refresh_rejects_expired_provider_quote_without_replacing_original() {
    let (adapter, provider, _, state, input, _directory) = fixture();
    let output = adapter.quote(&input).await.unwrap();
    let id = output["transfers"][0]["transfer_id"].as_str().unwrap();
    let before = state.transfer(id).unwrap().unwrap();
    let saved = adapter.saved_quote(&before).unwrap();
    provider.clock.0.fetch_add(61, Ordering::SeqCst);
    provider.expired.store(true, Ordering::SeqCst);
    let error = adapter.refresh_expired_source_quote(&before, &saved, &before.steps[0]).await.unwrap_err();
    assert_eq!(error.code, "BRIDGE_QUOTE_EXPIRED");
    assert_eq!(state.transfer(id).unwrap().unwrap(), before);
    assert_eq!(state.transfers().unwrap().len(), 1);
}
