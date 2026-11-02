//! Public broker/engine ERC-20 allowance lifecycle using only local fixtures.
use async_trait::async_trait;
use gofin_bridge::*;
use gofin_broker::{BridgeAdapter, SharedUsdValuator};
use gofin_domain::{
    Adapter, ApprovalAudit, AuthorizationChallenge, Authorizer, BridgeStepKind as DomainStepKind,
    BridgeStepLink, OperationRequest, Quantity, Result, Status, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use gofin_evm::{EvmRuntime, LocalEnvelopeSigner};
use gofin_store::{BridgeStateRepository, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[path = "support/bridge_erc20_fixture.rs"]
mod fixture;
use fixture::{
    ALT_PAYOUT_SENDER, ARB_USDC, DestinationPayout, ETH_USDC, Erc20Provider, Prices, Rpc,
};

fn q(value: &str) -> Quantity {
    Quantity::new(value).unwrap()
}
fn omit_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                omit_nulls(v);
            }
        }
        Value::Array(values) => {
            for v in values {
                omit_nulls(v);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct CountingApproval(AtomicUsize);
struct UnavailablePrices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for UnavailablePrices {
    async fn quote(&self, _: &gofin_domain::AssetId) -> Result<gofin_pricing::UsdQuote> {
        Err(gofin_domain::GoFinError::validation(
            "synthetic price unavailable",
        ))
    }
}
#[async_trait]
impl Authorizer for CountingApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic test authorizer".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    bridge: Arc<BridgeAdapter>,
    engine: Arc<Engine>,
    state: BridgeStateRepository,
    source: Arc<Rpc>,
    destination: Arc<Rpc>,
    provider: Arc<Erc20Provider>,
    approval: Arc<CountingApproval>,
    wallet: String,
    destination_chain_id: u64,
}
impl Harness {
    fn new() -> Self {
        Self::with_source_allowance(gofin_evm::alloy::primitives::U256::from(5_000_000u64))
    }

    fn with_source_allowance(initial_allowance: gofin_evm::alloy::primitives::U256) -> Self {
        Self::with_destination_payout(initial_allowance, DestinationPayout::Router)
    }

    fn with_destination_payout(
        initial_allowance: gofin_evm::alloy::primitives::U256,
        payout: DestinationPayout,
    ) -> Self {
        Self::build(initial_allowance, payout, 42161, None)
    }

    fn with_native_withheld_fee(fee: &str) -> Self {
        Self::build(
            gofin_evm::alloy::primitives::U256::ZERO,
            DestinationPayout::Direct,
            4663,
            Some(q(fee)),
        )
    }

    fn build(
        initial_allowance: gofin_evm::alloy::primitives::U256,
        payout: DestinationPayout,
        destination_chain_id: u64,
        native_withheld_fee: Option<Quantity>,
    ) -> Self {
        Self::build_with_prices(
            initial_allowance,
            payout,
            destination_chain_id,
            native_withheld_fee,
            false,
        )
    }

    fn build_with_prices(
        initial_allowance: gofin_evm::alloy::primitives::U256,
        payout: DestinationPayout,
        destination_chain_id: u64,
        native_withheld_fee: Option<Quantity>,
        unavailable: bool,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("journal.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(store.clone());
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let wallet = gofin_evm::lower_address(signer.address());
        let source = Arc::new(Rpc::source_with_allowance(
            wallet.clone(),
            initial_allowance,
        ));
        let destination = Arc::new(Rpc::destination_on(wallet.clone(), destination_chain_id));
        let source_runtime = Arc::new(EvmRuntime::new(source.clone(), 1, Some(signer)));
        let destination_runtime = Arc::new(EvmRuntime::new(
            destination.clone(),
            destination_chain_id,
            None,
        ));
        let prices = Arc::new(SharedUsdValuator::with_source(if unavailable {
            Arc::new(UnavailablePrices)
        } else {
            Arc::new(Prices)
        }));
        let source_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: wallet.clone(),
                    signing_enabled: true,
                }],
                source_runtime.clone(),
            )
            .unwrap()
            .with_wallet_valuation(prices.clone()),
        );
        let destination_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(destination_chain_id, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: wallet.clone(),
                    signing_enabled: false,
                }],
                destination_runtime.clone(),
            )
            .unwrap(),
        );
        let provider = Arc::new(if let Some(fee) = native_withheld_fee {
            Erc20Provider::with_native_withheld_fee(destination.clone(), payout, fee)
        } else {
            Erc20Provider::with_destination(destination.clone(), payout)
        });
        let bridge = Arc::new(
            BridgeAdapter::with_provider_and_observers(
                provider.clone(),
                BTreeMap::from([
                    (1, source_wallet.clone()),
                    (destination_chain_id, destination_wallet),
                ]),
                BTreeMap::from([
                    (1, source_runtime),
                    (destination_chain_id, destination_runtime),
                ]),
                prices,
            )
            .unwrap(),
        );
        bridge.attach_state(state.clone()).unwrap();
        let approval = Arc::new(CountingApproval::default());
        let engine = Arc::new(
            Engine::new_with_shared_store(
                store,
                vec![source_wallet, bridge.clone()],
                approval.clone(),
                EngineOptions {
                    max_movement_and_cost_usd_micros: "100000000".into(),
                    max_run_allocation_usd_micros: "200000000".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap(),
        );
        bridge.attach_engine(Arc::downgrade(&engine)).unwrap();
        Self {
            _tmp: tmp,
            bridge,
            engine,
            state,
            source,
            destination,
            provider,
            approval,
            wallet,
            destination_chain_id,
        }
    }

    fn reopen(&self) -> (Arc<BridgeAdapter>, Arc<Engine>, BridgeStateRepository) {
        let store = Store::open(self._tmp.path().join("journal.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(store.clone());
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let source_runtime = Arc::new(EvmRuntime::new(self.source.clone(), 1, Some(signer)));
        let destination_runtime = Arc::new(EvmRuntime::new(
            self.destination.clone(),
            self.destination_chain_id,
            None,
        ));
        let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
        let source_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: self.wallet.clone(),
                    signing_enabled: true,
                }],
                source_runtime.clone(),
            )
            .unwrap()
            .with_wallet_valuation(prices.clone()),
        );
        let destination_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(self.destination_chain_id, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: self.wallet.clone(),
                    signing_enabled: false,
                }],
                destination_runtime.clone(),
            )
            .unwrap(),
        );
        let bridge = Arc::new(
            BridgeAdapter::with_provider_and_observers(
                self.provider.clone(),
                BTreeMap::from([
                    (1, source_wallet.clone()),
                    (self.destination_chain_id, destination_wallet),
                ]),
                BTreeMap::from([
                    (1, source_runtime),
                    (self.destination_chain_id, destination_runtime),
                ]),
                prices,
            )
            .unwrap(),
        );
        bridge.attach_state(state.clone()).unwrap();
        let engine = Arc::new(
            Engine::new_with_shared_store(
                store,
                vec![source_wallet, bridge.clone()],
                self.approval.clone(),
                EngineOptions {
                    max_movement_and_cost_usd_micros: "100000000".into(),
                    max_run_allocation_usd_micros: "200000000".into(),
                    allow_unpriced_exchange_assets: false,
                },
            )
            .unwrap(),
        );
        bridge.attach_engine(Arc::downgrade(&engine)).unwrap();
        (bridge, engine, state)
    }

    fn quote_input(&self, refresh: Option<String>) -> Value {
        self.quote_input_to(refresh, Some(ARB_USDC))
    }

    fn quote_input_to(&self, refresh: Option<String>, destination_token: Option<&str>) -> Value {
        let mut value = json!({
            "intent": {
                "wallet":"synthetic-wallet",
                "source":{"chainId":1,"token":ETH_USDC},
                "destinationChainId":42161,
                "destinationAssets":[{"chainId":42161,"token":destination_token}],
                "recipient":self.wallet,
                "refundTo":self.wallet,
                "allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],
                "amount":"10000000"
            },
            "refreshTransferId":refresh
        });
        omit_nulls(&mut value);
        value
    }

    fn native_bounds(&self) -> BridgeBounds {
        BridgeBounds {
            input: Some(AmountInterval {
                minimum: q("10000000000000000"),
                maximum: q("10000000000000000"),
            }),
            maximum_source_debit: Some(q("10000000000000000")),
            input_usd_micros: None,
            source_usd_per_unit_micros: None,
            minimum_net_receipt: Some(
                EvmAsset {
                    chain_id: 4663,
                    token: None,
                    decimals: 18,
                }
                .amount(q("9800000000000000")),
            ),
            fee_caps: vec![
                FeeCap {
                    asset: EvmAsset {
                        chain_id: 1,
                        token: None,
                        decimals: 18,
                    }
                    .asset_id(),
                    kind: FeeKind::Gas,
                    maximum: q("1000000000000000"),
                    maximum_usd_micros: None,
                },
                FeeCap {
                    asset: EvmAsset {
                        chain_id: 1,
                        token: None,
                        decimals: 18,
                    }
                    .asset_id(),
                    kind: FeeKind::WithheldFromOutput,
                    maximum: q("200000000000000"),
                    maximum_usd_micros: None,
                },
            ],
            maximum_total_usd_micros: None,
            authorization_seconds: Some(600),
            ..BridgeBounds::default()
        }
    }

    fn bounds(&self) -> BridgeBounds {
        self.bounds_for_destination(Some(ARB_USDC))
    }

    fn bounds_for_destination(&self, destination_token: Option<&str>) -> BridgeBounds {
        BridgeBounds {
            input: Some(AmountInterval {
                minimum: q("10000000"),
                maximum: q("10000000"),
            }),
            maximum_source_debit: Some(q("12000000")),
            input_usd_micros: None,
            source_usd_per_unit_micros: None,
            minimum_net_receipt: Some(
                EvmAsset {
                    chain_id: 42161,
                    token: destination_token.map(str::to_owned),
                    decimals: if destination_token.is_some() { 6 } else { 18 },
                }
                .amount(q(if destination_token.is_some() {
                    "9500000"
                } else {
                    "4800000000000000"
                })),
            ),
            fee_caps: vec![FeeCap {
                asset: EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                kind: FeeKind::Gas,
                maximum: q("1000000000000000"),
                maximum_usd_micros: None,
            }],
            maximum_total_usd_micros: None,
            authorization_seconds: Some(600),
            ..BridgeBounds::default()
        }
    }

    fn step_input(
        &self,
        transfer: &str,
        step: &BridgeStepLink,
        bounds: Option<BridgeBounds>,
    ) -> Value {
        let mut value = serde_json::to_value(BridgeTransferInput {
            transfer_id: transfer.into(),
            step_id: step.step_id.clone(),
            step_kind: step.kind.clone(),
            bounds,
        })
        .unwrap();
        omit_nulls(&mut value);
        value
    }

    async fn execute_exact(
        &self,
        transfer: &str,
        step: &BridgeStepLink,
        request_id: &str,
        bounds: &BridgeBounds,
    ) -> gofin_domain::OperationRecord {
        self.engine
            .execute(OperationRequest {
                request_id: request_id.into(),
                operation: "bridge.transfer".into(),
                input: self.step_input(transfer, step, Some(bounds.clone())),
                run_id: None,
            })
            .await
            .unwrap()
    }

    async fn assert_source_frozen(&self, transfer: &str, expected_step_ids: &[String]) {
        let refresh_error = self
            .bridge
            .read("bridge.quotes", &self.quote_input(Some(transfer.into())))
            .await
            .unwrap_err();
        assert_eq!(refresh_error.code, "BRIDGE_REFRESH_UNSAFE");
        let record = self.state.transfer(transfer).unwrap().unwrap();
        assert_eq!(
            record
                .steps
                .iter()
                .map(|step| step.step_id.clone())
                .collect::<Vec<_>>(),
            expected_step_ids,
            "submitted source scope retains its original step identities"
        );
        assert!(
            record
                .steps
                .iter()
                .all(|step| step.state == gofin_domain::BridgeStepState::Settled)
        );
    }
}

async fn execute_source_and_get_transfer(
    harness: &Harness,
    destination_token: Option<&str>,
    prefix: &str,
) -> (String, BridgeBounds) {
    let quote = harness
        .bridge
        .read(
            "bridge.quotes",
            &harness.quote_input_to(None, destination_token),
        )
        .await
        .unwrap();
    let transfer = quote["transfers"][0]["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut steps: Vec<BridgeStepLink> =
        serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
    steps.sort_by_key(|step| match step.kind {
        DomainStepKind::ResetAllowance => 0,
        DomainStepKind::Approval => 1,
        DomainStepKind::Deposit => 2,
        DomainStepKind::Prove | DomainStepKind::Finalize | DomainStepKind::Mint => {
            panic!("SYNTHETIC Relay fixture does not declare deferred destination stages")
        }
    });
    let bounds = harness.bounds_for_destination(destination_token);
    for (index, step) in steps.iter().enumerate() {
        let operation = harness
            .execute_exact(
                &transfer,
                step,
                &format!("{prefix}-source-{index}"),
                &bounds,
            )
            .await;
        assert_eq!(
            operation.status,
            Status::Completed,
            "source step {:?} failed: {:?}",
            step.kind,
            operation.error
        );
    }
    let record = harness.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(record.state, gofin_domain::BridgeTransferState::InTransit);
    assert!(record.transit.is_some());
    (transfer, bounds)
}

#[tokio::test]
async fn new_bridge_quote_and_direct_stage_reject_retired_usd_bounds() {
    let h = Harness::new();
    let mut retired = Vec::new();
    let mut bounds = h.bounds();
    bounds.input_usd_micros = Some(AmountInterval {
        minimum: q("1"),
        maximum: q("2"),
    });
    retired.push(bounds);
    let mut bounds = h.bounds();
    bounds.source_usd_per_unit_micros = Some(AmountInterval {
        minimum: q("1"),
        maximum: q("2"),
    });
    retired.push(bounds);
    let mut bounds = h.bounds();
    bounds.maximum_total_usd_micros = Some(14_000_000);
    retired.push(bounds);
    let mut bounds = h.bounds();
    bounds.fee_caps[0].maximum_usd_micros = Some(2_000_000);
    retired.push(bounds);
    for bounds in &retired {
        let mut quote_input = h.quote_input(None);
        quote_input["bounds"] = serde_json::to_value(bounds).unwrap();
        let error = h
            .bridge
            .read("bridge.quotes", &quote_input)
            .await
            .unwrap_err();
        assert!(error.message.contains("USD approval bounds are retired"));
    }
    let quote = h
        .bridge
        .read("bridge.quotes", &h.quote_input(None))
        .await
        .unwrap();
    let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
    let steps: Vec<BridgeStepLink> =
        serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
    let step = steps.iter().find(|s| s.depends_on.is_empty()).unwrap();
    for bounds in retired {
        let error = h
            .bridge
            .prepare(
                "bridge.transfer",
                &h.step_input(transfer, step, Some(bounds)),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("USD approval bounds are retired"));
    }
    // Model a saved pre-cutover quote. A fresh action may not inherit its USD
    // authority, even when the new request omits bounds.
    let mut historical = h.state.transfer(transfer).unwrap().unwrap();
    historical.metadata["quote_bounds"]["maximum_total_usd_micros"] = json!(14_000_000);
    historical.quote_revisions.last_mut().unwrap().payload["bounds"]["maximum_total_usd_micros"] =
        json!(14_000_000);
    let db = rusqlite::Connection::open(h._tmp.path().join("journal.sqlite")).unwrap();
    db.execute(
        "UPDATE bridge_transfers SET record_json=?1 WHERE transfer_id=?2",
        rusqlite::params![serde_json::to_string(&historical).unwrap(), transfer],
    )
    .unwrap();
    let error = h
        .bridge
        .prepare("bridge.transfer", &h.step_input(transfer, step, None))
        .await
        .unwrap_err();
    assert!(error.message.contains("USD approval bounds are retired"));
    let error = h
        .bridge
        .read("bridge.quotes", &h.quote_input(Some(transfer.into())))
        .await
        .unwrap_err();
    assert!(error.message.contains("USD approval bounds are retired"));
    assert_eq!(h.source.broadcasts(), 0);
}

#[tokio::test]
async fn native_quote_and_direct_stage_execute_when_usd_pricing_is_unavailable() {
    let h = Harness::build_with_prices(
        gofin_evm::alloy::primitives::U256::ZERO,
        DestinationPayout::Router,
        42161,
        None,
        true,
    );
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
        .find(|s| s.kind == DomainStepKind::Approval)
        .unwrap();
    let result = h
        .execute_exact(transfer, approval, "unpriced-native-stage", &h.bounds())
        .await;
    assert_eq!(result.status, Status::Completed, "{result:?}");
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.source.broadcasts(), 1);
}

#[tokio::test]
async fn direct_native_order_tagged_payout_accepts_alternate_nonself_sender() {
    let harness = Harness::with_destination_payout(
        gofin_evm::alloy::primitives::U256::from(5_000_000u64),
        DestinationPayout::Direct,
    );
    let (transfer, _) = execute_source_and_get_transfer(&harness, None, "direct-native").await;
    let status = harness
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed", "{status}");
    let record = harness.state.transfer(&transfer).unwrap().unwrap();
    let proof = record.settlement_proofs.last().unwrap();
    assert_eq!(
        proof.evidence["proof_method"],
        "direct_transaction_order_tag"
    );
    assert_eq!(proof.evidence["receipt_scope"], "transaction");
    assert!(proof.evidence.get("log_index").is_none());
    assert_eq!(proof.evidence["transaction_sender"], ALT_PAYOUT_SENDER);
    assert_eq!(
        proof.evidence["correlation"],
        proof.evidence["protocol_order_id"]
    );
    assert!(
        proof.evidence["evidence_id"]
            .as_str()
            .unwrap()
            .ends_with(":transaction")
    );
}

#[tokio::test]
async fn direct_erc20_order_tagged_payout_needs_no_historical_code_read() {
    let harness = Harness::with_destination_payout(
        gofin_evm::alloy::primitives::U256::from(5_000_000u64),
        DestinationPayout::Direct,
    );
    let (transfer, _) =
        execute_source_and_get_transfer(&harness, Some(ARB_USDC), "direct-erc20").await;
    let status = harness
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed", "{status}");
    let record = harness.state.transfer(&transfer).unwrap().unwrap();
    let proof = record.settlement_proofs.last().unwrap();
    assert_eq!(
        proof.evidence["proof_method"],
        "direct_transaction_order_tag"
    );
    assert!(proof.evidence.get("receipt_scope").is_none());
    assert_eq!(proof.evidence["log_index"], 0);
    assert_eq!(proof.evidence["transaction_sender"], ALT_PAYOUT_SENDER);
    assert_eq!(proof.evidence["amount_atoms"], "9700000");
    assert_eq!(
        proof.evidence["correlation"],
        proof.evidence["protocol_order_id"]
    );
}

#[tokio::test]
async fn standalone_bridge_preserves_route_after_restart_until_destination_settlement() {
    let h = Harness::new();
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "standalone-bridge-restart".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request.clone())))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        h.state
            .transfer(&transfer_id)
            .unwrap()
            .unwrap()
            .transit
            .as_ref()
            .is_some_and(|t| !t.spendable)
    );
    let broadcasts = h.source.broadcasts();
    h.destination
        .destination_ready
        .store(true, Ordering::SeqCst);
    let (bridge, engine, state) = h.reopen();
    let mut changed = request.clone();
    changed.input["intent"]["recipient"] = json!("0x1111111111111111111111111111111111111111");
    assert!(
        Box::pin(engine.execute(changed)).await.is_err(),
        "same request cannot replace its approved route"
    );
    let restored = Box::pin(engine.status(&action.id, false)).await.unwrap();
    assert_eq!(
        restored.preview.as_ref().unwrap().binding["transfer_id"],
        transfer_id
    );
    assert_eq!(
        bridge
            .observe_transfer_side(&restored, "destination")
            .await
            .unwrap()
            .metadata["state"],
        "completed"
    );
    let observed = Box::pin(engine.status(&action.id, true)).await.unwrap();
    assert_eq!(
        engine.operation_view(&observed).unwrap()["outcome"]["phase"],
        "done"
    );
    assert_eq!(
        state
            .transfer(&transfer_id)
            .unwrap()
            .unwrap()
            .settlement_proofs
            .len(),
        1
    );
    assert_eq!(h.source.broadcasts(), broadcasts);
}

#[tokio::test]
async fn legacy_usd_bound_admitted_bridge_reconciles_after_restart() {
    let h = Harness::new();
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "legacy-usd-bridge-restart".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request.clone())))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        h.state
            .transfer(&transfer_id)
            .unwrap()
            .unwrap()
            .transit
            .as_ref()
            .is_some_and(|t| !t.spendable)
    );
    let mut historical = h.state.transfer(&transfer_id).unwrap().unwrap();
    historical.metadata["quote_bounds"]["maximum_total_usd_micros"] = json!(14_000_000);
    historical.quote_revisions.last_mut().unwrap().payload["bounds"]["maximum_total_usd_micros"] =
        json!(14_000_000);
    let db = rusqlite::Connection::open(h._tmp.path().join("journal.sqlite")).unwrap();
    db.execute(
        "UPDATE bridge_transfers SET record_json=?1 WHERE transfer_id=?2",
        rusqlite::params![serde_json::to_string(&historical).unwrap(), transfer_id],
    )
    .unwrap();
    drop(db);
    let broadcasts = h.source.broadcasts();
    h.destination
        .destination_ready
        .store(true, Ordering::SeqCst);
    let (bridge, engine, state) = h.reopen();
    let mut changed = request.clone();
    changed.input["intent"]["recipient"] = json!("0x1111111111111111111111111111111111111111");
    assert!(
        Box::pin(engine.execute(changed)).await.is_err(),
        "same request cannot replace its approved route"
    );
    let restored = Box::pin(engine.status(&action.id, false)).await.unwrap();
    assert_eq!(
        restored.preview.as_ref().unwrap().binding["transfer_id"],
        transfer_id
    );
    assert_eq!(
        bridge
            .observe_transfer_side(&restored, "destination")
            .await
            .unwrap()
            .metadata["state"],
        "completed"
    );
    let observed = Box::pin(engine.status(&action.id, true)).await.unwrap();
    assert_eq!(
        engine.operation_view(&observed).unwrap()["outcome"]["phase"],
        "done"
    );
    assert_eq!(
        state
            .transfer(&transfer_id)
            .unwrap()
            .unwrap()
            .settlement_proofs
            .len(),
        1
    );
    assert_eq!(h.source.broadcasts(), broadcasts);
}

#[tokio::test]
async fn native_relay_settlement_evidence_is_idempotent_and_survives_restart() {
    let h = Harness::with_native_withheld_fee("100000000000000");
    let mut bounds = h.native_bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "native-relay-evidence".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1},"destinationChainId":4663,"destinationAssets":[{"chainId":4663}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1}],"amount":{"minimum":"10000000000000000","maximum":"10000000000000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request)))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        h.bridge
            .observe_transfer_side(&action, "destination")
            .await
            .unwrap()
            .metadata["state"],
        "completed"
    );
    let before = h.state.transfer(&transfer_id).unwrap().unwrap();
    let proof = before.settlement_proofs.last().unwrap().clone();
    let credit = proof
        .effects
        .iter()
        .find_map(|effect| match effect {
            gofin_domain::FinancialEffect::UsableCredit { amount, .. } => Some(amount),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        credit.atoms.as_str(),
        "9900000000000000",
        "destination credit verifies the withheld 0.0001 native fee"
    );
    let count = before.settlement_proofs.len();
    let (bridge, engine, state) = h.reopen();
    let status = bridge
        .read("bridge.status", &json!({"transferId":transfer_id}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed");
    let reopened = Box::pin(engine.status(&action.id, true)).await.unwrap();
    assert_eq!(
        engine.operation_view(&reopened).unwrap()["outcome"]["phase"],
        "done"
    );
    let after = state.transfer(&transfer_id).unwrap().unwrap();
    assert_eq!(after.settlement_proofs.len(), count);
    assert_eq!(after.settlement_proofs.last(), Some(&proof));
}

#[tokio::test]
async fn historical_native_relay_cost_refinement_preserves_original_proof_across_restart() {
    let h = Harness::with_native_withheld_fee("100000000000000");
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    let mut bounds = h.native_bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "native-relay-historical-cost".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1},"destinationChainId":4663,"destinationAssets":[{"chainId":4663}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1}],"amount":{"minimum":"10000000000000000","maximum":"10000000000000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request)))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.provider.set_native_eth_enabled(false);
    h.destination
        .destination_ready
        .store(true, Ordering::SeqCst);
    assert_eq!(
        h.bridge
            .observe_transfer_side(&action, "destination")
            .await
            .unwrap()
            .metadata["state"],
        "completed"
    );
    let original = h.state.transfer(&transfer_id).unwrap().unwrap();
    let proof = original.settlement_proofs.last().unwrap().clone();
    assert!(!proof.effects.iter().any(|e| matches!(e, gofin_domain::FinancialEffect::Cost { category, .. } if category == "withheld_from_output")));
    let original_operations = h.engine.operations().unwrap();
    let broadcast_count = h.source.broadcasts();
    h.provider.set_native_eth_enabled(true);
    let (bridge, engine, state) = h.reopen();
    let status = bridge
        .read("bridge.status", &json!({"transferId":transfer_id}))
        .await
        .unwrap();
    assert_eq!(status["state"], "completed", "{status}");
    let refined = state.transfer(&transfer_id).unwrap().unwrap();
    assert_eq!(refined.settlement_proofs.len(), 2);
    assert_eq!(refined.settlement_proofs[0], proof);
    let refined_proof = &refined.settlement_proofs[1];
    assert!(refined_proof.effects.starts_with(&proof.effects));
    let costs = refined_proof
        .effects
        .iter()
        .filter_map(|e| match e {
            gofin_domain::FinancialEffect::Cost {
                amount, category, ..
            } if category == "withheld_from_output" => Some(amount),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(costs.len(), 1);
    assert_eq!(costs[0].atoms.as_str(), "100000000000000");
    let again = bridge
        .read("bridge.status", &json!({"transferId":transfer_id}))
        .await
        .unwrap();
    assert_eq!(again["state"], "completed");
    assert_eq!(
        state
            .transfer(&transfer_id)
            .unwrap()
            .unwrap()
            .settlement_proofs,
        refined.settlement_proofs
    );
    assert_eq!(engine.operations().unwrap(), original_operations);
    assert_eq!(h.source.broadcasts(), broadcast_count);
}

#[tokio::test]
async fn native_relay_proven_over_cap_cost_is_executed_with_discrepancy() {
    let h = Harness::with_native_withheld_fee("100000000000000");
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    let mut bounds = h.native_bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
        if cap.kind == FeeKind::WithheldFromOutput {
            cap.maximum = q("100500000000000");
        }
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "native-relay-over-cap".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1},"destinationChainId":4663,"destinationAssets":[{"chainId":4663}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1}],"amount":{"minimum":"10000000000000000","maximum":"10000000000000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request)))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let transfer = h.state.transfer(transfer_id).unwrap().unwrap();
    let order = transfer
        .quote_revisions
        .last()
        .unwrap()
        .order_id
        .as_ref()
        .unwrap();
    h.destination.set_direct_destination(
        &EvmAsset {
            chain_id: 4663,
            token: None,
            decimals: 18,
        },
        &h.wallet,
        order,
        &q("9899100000000000"),
        DestinationPayout::Direct,
    );
    h.destination
        .destination_ready
        .store(true, Ordering::SeqCst);
    let destination = h
        .bridge
        .observe_transfer_side(&action, "destination")
        .await
        .unwrap();
    assert_eq!(
        destination.metadata["state"], "completed",
        "{destination:?}"
    );
    let observed = Box::pin(h.engine.status(&action.id, true)).await.unwrap();
    let view = h.engine.operation_view(&observed).unwrap();
    assert_eq!(view["outcome"]["result"], "executed", "{view}");
    assert_eq!(view["outcome"]["transfer"], "available", "{view}");
    assert_eq!(
        view["outcome"]["attention"]["kind"], "discrepancy",
        "{view}"
    );
    assert_eq!(h.source.broadcasts(), 1);
}

#[tokio::test]
async fn direct_transfer_from_payout_verifies_distinct_token_owner_without_replay() {
    let h = Harness::with_destination_payout(
        gofin_evm::alloy::primitives::U256::ZERO,
        DestinationPayout::DirectTransferFrom,
    );
    let (transfer, _) =
        execute_source_and_get_transfer(&h, Some(ARB_USDC), "direct-transfer-from").await;
    let count = h.source.broadcasts();
    for _ in 0..2 {
        let status = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(status["state"], "completed", "{status}");
    }
    let record = h.state.transfer(&transfer).unwrap().unwrap();
    let proof = record.settlement_proofs.last().unwrap();
    assert_eq!(proof.evidence["transaction_sender"], ALT_PAYOUT_SENDER);
    assert_eq!(proof.evidence["amount_atoms"], "9700000");
    assert_eq!(proof.evidence["log_index"], 0);
    assert_eq!(
        proof.evidence["correlation"],
        proof.evidence["protocol_order_id"]
    );
    assert_eq!(h.source.broadcasts(), count);
}

#[tokio::test]
async fn direct_payout_with_provider_request_tag_stays_in_transit_without_effects() {
    let harness = Harness::with_destination_payout(
        gofin_evm::alloy::primitives::U256::from(5_000_000u64),
        DestinationPayout::DirectWrongOrderTag,
    );
    let (transfer, _) = execute_source_and_get_transfer(&harness, None, "direct-wrong-tag").await;
    let status = harness
        .bridge
        .read("bridge.status", &json!({"transferId":transfer}))
        .await
        .unwrap();
    assert_eq!(status["provider_status"]["settled"], false);
    assert_eq!(
        status["provider_status"]["candidate_observations"][0]["transaction_observation"]["input_matches_order_id"],
        false
    );
    assert_eq!(
        status["provider_status"]["candidate_observations"][0]["transaction_observation"]["input_matches_provider_request_id"],
        true
    );
    assert_eq!(
        status["provider_status"]["candidate_observations"][0]["direct_payment_rejection"]["code"],
        "BRIDGE_PROOF_MISMATCH"
    );
    let record = harness.state.transfer(&transfer).unwrap().unwrap();
    assert_eq!(record.state, gofin_domain::BridgeTransferState::InTransit);
    assert!(record.transit.is_some());
    assert!(record.settlement_proofs.is_empty());
}

#[tokio::test]
async fn direct_erc20_wrong_token_or_missing_transfer_log_stays_in_transit() {
    for (index, payout) in [
        DestinationPayout::DirectWrongToken,
        DestinationPayout::DirectNoTransferLog,
    ]
    .into_iter()
    .enumerate()
    {
        let harness = Harness::with_destination_payout(
            gofin_evm::alloy::primitives::U256::from(5_000_000u64),
            payout,
        );
        let (transfer, _) = execute_source_and_get_transfer(
            &harness,
            Some(ARB_USDC),
            &format!("direct-erc20-invalid-{index}"),
        )
        .await;
        let status = harness
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(status["provider_status"]["settled"], false);
        let record = harness.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(record.state, gofin_domain::BridgeTransferState::InTransit);
        assert!(record.transit.is_some());
        assert!(record.settlement_proofs.is_empty());
    }
}

#[tokio::test]
async fn standalone_bridge_review_binds_steps_without_source_balance_or_simulation() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::from(5_000_000u64));
    *h.source.token_balance.lock().unwrap() = gofin_evm::alloy::primitives::U256::ZERO;
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds});
    let review = h.bridge.prepare("bridge.execute", &input).await.unwrap();
    let transfer_id = review.binding["transfer_id"].as_str().unwrap();
    let transfer = h.state.transfer(transfer_id).unwrap().unwrap();
    assert_eq!(
        transfer
            .steps
            .iter()
            .map(|s| s.kind.clone())
            .collect::<Vec<_>>(),
        vec![
            DomainStepKind::ResetAllowance,
            DomainStepKind::Approval,
            DomainStepKind::Deposit
        ]
    );
    assert_eq!(
        review.intent.maximum_costs[0].amount.atoms,
        q("1000000000000000")
    );
    assert_eq!(h.source.token_balance_reads(), 0);
    assert_eq!(h.source.estimate_gas_calls(), 0);
    assert_eq!(h.source.broadcasts(), 0);
}

#[tokio::test]
async fn standalone_bridge_keeps_parent_and_waits_for_destination_proof() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::from(5_000_000u64));
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.minimum_net_receipt.as_mut().unwrap().atoms = q("9400000");
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "standalone-bridge-pending-destination".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request.clone())))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    assert_eq!(h.source.broadcasts(), 3, "{action:?}");
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let source = h.state.transfer(transfer_id).unwrap().unwrap();
    let ids = source
        .steps
        .iter()
        .map(|s| s.step_id.clone())
        .collect::<Vec<_>>();
    assert!(source.transit.as_ref().is_some_and(|t| !t.spendable));
    assert!(
        source.settlement_expectation.as_ref().unwrap().destination[0].minimum_atoms
            >= q("9400000")
    );
    let refresh = h
        .bridge
        .read(
            "bridge.quotes",
            &h.quote_input(Some(transfer_id.to_owned())),
        )
        .await;
    // Once the source has dispatched the route is frozen; a quote read may
    // refuse refresh but must never replay a settled child.
    if let Ok(refreshed) = refresh {
        let links: Vec<BridgeStepLink> =
            serde_json::from_value(refreshed["transfers"][0]["steps"].clone()).unwrap();
        assert_eq!(
            links.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>(),
            ids
        );
    }
    assert_eq!(h.source.broadcasts(), 3);
    h.destination
        .destination_ready
        .store(true, Ordering::SeqCst);
    let destination = h
        .bridge
        .observe_transfer_side(&action, "destination")
        .await
        .unwrap();
    assert_eq!(destination.metadata["state"], "completed");
    let observed = Box::pin(h.engine.status(&action.id, true)).await.unwrap();
    assert_eq!(
        h.engine.operation_view(&observed).unwrap()["outcome"]["phase"],
        "done"
    );
    let duplicate = Box::pin(h.engine.execute(request)).await.unwrap();
    assert_eq!(duplicate.id, action.id);
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.source.broadcasts(), 3);
}

#[tokio::test]
async fn exact_step_flow_refreshes_finite_allowance_cap_after_reset() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let h = Harness::new();
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut steps: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        steps.sort_by_key(|step| match step.kind {
            DomainStepKind::ResetAllowance => 0,
            DomainStepKind::Approval => 1,
            DomainStepKind::Deposit => 2,
            DomainStepKind::Prove | DomainStepKind::Finalize | DomainStepKind::Mint => {
                panic!("SYNTHETIC Relay fixture does not declare deferred destination stages")
            }
        });
        assert_eq!(
            steps.iter().map(|s| s.kind.clone()).collect::<Vec<_>>(),
            vec![
                DomainStepKind::ResetAllowance,
                DomainStepKind::Approval,
                DomainStepKind::Deposit
            ]
        );
        let ids = steps.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>();
        let bounds = h.bounds();

        // Exercise the public ordinary-write path directly. No operation-boundary
        // grant and no explicit preview occurs between quote and each write.
        for (step, request_id) in
            steps
                .iter()
                .zip(["exact-reset", "exact-approve", "exact-deposit"])
        {
            let operation = h.execute_exact(&transfer, step, request_id, &bounds).await;
            assert_eq!(
                operation.status,
                Status::Completed,
                "{} error={:?}",
                step.kind.operation(),
                operation.error
            );
        }

        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            3,
            "ordinary exact-step writes each require fresh authorization"
        );
        assert_eq!(h.source.broadcasts(), 3);
        assert_eq!(
            *h.source.allowance.lock().unwrap(),
            gofin_evm::alloy::primitives::U256::from(2_000_000u64)
        );
        assert_eq!(
            *h.source.token_balance.lock().unwrap(),
            gofin_evm::alloy::primitives::U256::from(10_000_000u64)
        );
        h.assert_source_frozen(&transfer, &ids).await;
        let status = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(
            status["state"], "completed",
            "exact-step ERC-20 proof path: {status}"
        );
        let saved = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(
            saved
                .steps
                .iter()
                .map(|s| s.step_id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert!(
            saved
                .steps
                .iter()
                .all(|s| s.state == gofin_domain::BridgeStepState::Settled)
        );
        assert!(saved.applied_proofs.len() >= 2);
        assert_eq!(
            saved.settlement_proofs.last().unwrap().outcome,
            gofin_domain::BridgeSettlementOutcome::Destination
        );
    })
    .await
    .expect("ordinary ERC-20 lifecycle should not deadlock");
}

#[tokio::test]
async fn exact_step_flow_with_zero_initial_allowance_approves_then_deposits() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut steps: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        steps.sort_by_key(|step| match step.kind {
            DomainStepKind::Approval => 0,
            DomainStepKind::Deposit => 1,
            DomainStepKind::ResetAllowance => 2,
            DomainStepKind::Prove | DomainStepKind::Finalize | DomainStepKind::Mint => {
                panic!("SYNTHETIC Relay fixture does not declare deferred destination stages")
            }
        });
        assert_eq!(
            steps.iter().map(|s| s.kind.clone()).collect::<Vec<_>>(),
            vec![DomainStepKind::Approval, DomainStepKind::Deposit]
        );
        let ids = steps.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>();
        let bounds = h.bounds();

        // This is the common zero-allowance route: approve once to the human's
        // selected finite cap (12 USDC), then deposit 10 USDC, directly through
        // separate exact-operation approvals with no preview/grant.
        for (step, request_id) in steps
            .iter()
            .zip(["zero-allowance-approve", "zero-allowance-deposit"])
        {
            let operation = h.execute_exact(&transfer, step, request_id, &bounds).await;
            assert_eq!(
                operation.status,
                Status::Completed,
                "{:?} error={:?}",
                step.kind,
                operation.error
            );
        }

        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            2,
            "each ordinary wallet write needs fresh authorization"
        );
        assert_eq!(h.source.broadcasts(), 2);
        assert_eq!(
            *h.source.allowance.lock().unwrap(),
            gofin_evm::alloy::primitives::U256::from(2_000_000u64)
        );
        assert_eq!(
            *h.source.token_balance.lock().unwrap(),
            gofin_evm::alloy::primitives::U256::from(10_000_000u64)
        );
        h.assert_source_frozen(&transfer, &ids).await;
        let status = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(
            status["state"], "completed",
            "zero-allowance exact-step proof path: {status}"
        );
        let saved = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(
            saved
                .steps
                .iter()
                .map(|s| s.step_id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert!(
            saved
                .steps
                .iter()
                .all(|s| s.state == gofin_domain::BridgeStepState::Settled)
        );
        assert!(saved.applied_proofs.len() >= 2);
        assert_eq!(
            saved.settlement_proofs.last().unwrap().outcome,
            gofin_domain::BridgeSettlementOutcome::Destination
        );
    })
    .await
    .expect("zero-allowance ERC-20 lifecycle should not deadlock");
}

#[tokio::test]
async fn source_receipts_reject_envelope_changes_and_actual_fee_overruns() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for (index, mutation) in [
            "to",
            "value",
            "calldata",
            "gas",
            "max_fee",
            "receipt_gas",
            "receipt_price",
        ]
        .into_iter()
        .enumerate()
        {
            let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
            let quote = h
                .bridge
                .read("bridge.quotes", &h.quote_input(None))
                .await
                .unwrap();
            let transfer = quote["transfers"][0]["transfer_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let steps: Vec<BridgeStepLink> =
                serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
            let approval = steps
                .iter()
                .find(|step| step.kind == DomainStepKind::Approval)
                .unwrap();
            h.source.mutate_reported_transaction(mutation);
            let operation = h
                .execute_exact(
                    &transfer,
                    approval,
                    &format!("bad-frozen-receipt-{index}"),
                    &h.bounds(),
                )
                .await;
            assert!(
                matches!(operation.status, Status::Pending | Status::Unknown),
                "mismatched receipt must remain unresolved; mutation={mutation}, status={:?}",
                operation.status
            );
            assert!(operation.submission.is_some(), "mutation={mutation}");
            assert!(operation.effects.is_none(), "mutation={mutation}");
            assert_eq!(h.source.broadcasts(), 1, "mutation={mutation}");
        }
    })
    .await
    .expect("invalid source receipt checks should remain bounded");
}

#[tokio::test]
async fn erc20_reset_finite_approval_deposit_and_settlement_share_one_grant() {
    let h = Harness::new();
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "erc20-reset-compound".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request.clone())))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    assert_eq!(
        h.source.broadcasts(),
        3,
        "reset, approval and deposit must each execute once: {action:?}"
    );
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let saved = h.state.transfer(transfer_id).unwrap().unwrap();
    assert_eq!(
        saved
            .steps
            .iter()
            .map(|s| s.kind.clone())
            .collect::<Vec<_>>(),
        vec![
            DomainStepKind::ResetAllowance,
            DomainStepKind::Approval,
            DomainStepKind::Deposit
        ]
    );
    assert!(
        saved
            .steps
            .iter()
            .all(|s| s.state == gofin_domain::BridgeStepState::Settled)
    );
    assert_eq!(
        *h.source.allowance.lock().unwrap(),
        gofin_evm::alloy::primitives::U256::from(2_000_000u64)
    );
    assert_eq!(
        *h.source.token_balance.lock().unwrap(),
        gofin_evm::alloy::primitives::U256::from(10_000_000u64)
    );
    let destination = h
        .bridge
        .observe_transfer_side(&action, "destination")
        .await
        .unwrap();
    assert_eq!(destination.metadata["state"], "completed");
    let saved = h.state.transfer(transfer_id).unwrap().unwrap();
    assert!(saved.applied_proofs.len() >= 2);
    assert_eq!(
        saved.settlement_proofs.last().unwrap().outcome,
        gofin_domain::BridgeSettlementOutcome::Destination
    );
    assert_eq!(
        saved.settlement_expectation.as_ref().unwrap().destination[0].minimum_atoms,
        q("9500000")
    );
    let duplicate = Box::pin(h.engine.execute(request)).await.unwrap();
    assert_eq!(duplicate.id, action.id);
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(h.source.broadcasts(), 3, "completed stages must not repeat");
}

mod preparation_concurrency {
    include!("support/bridge_preparation_concurrency.rs");
}

#[tokio::test]
async fn standalone_bridge_advances_approval_and_deposit_with_one_authorization() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "synthetic-standalone-compound".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let mut retired = request.input.clone();
    retired["bounds"]["authorization_seconds"] = json!(600);
    assert!(
        h.bridge
            .prepare("bridge.execute", &retired)
            .await
            .unwrap_err()
            .message
            .contains("retired")
    );
    retired["bounds"]["authorization_seconds"] = Value::Null;
    retired["bounds"]["maximum_total_usd_micros"] = json!(1);
    assert!(
        h.bridge
            .prepare("bridge.execute", &retired)
            .await
            .unwrap_err()
            .message
            .contains("USD")
    );
    assert_eq!(h.source.broadcasts(), 0);
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(h.engine.execute(request.clone()))
        .await
        .unwrap();
    assert_eq!(
        h.approval.0.load(Ordering::SeqCst),
        1,
        "parent outcome: {action:?}"
    );
    assert_eq!(
        h.source.broadcasts(),
        2,
        "the active invocation advances both ready stages: {action:?}"
    );
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let transfer = h.state.transfer(transfer_id).unwrap().unwrap();
    assert_eq!(transfer.steps.len(), 2);
    assert!(
        transfer
            .steps
            .iter()
            .all(|step| step.state == gofin_domain::BridgeStepState::Settled)
    );
    let destination = h
        .bridge
        .observe_transfer_side(&action, "destination")
        .await
        .unwrap_or_else(|error| {
            panic!(
                "destination={error:?}; quote IDs={:?}; source evidence={:?}",
                transfer
                    .quote_revisions
                    .iter()
                    .map(|q| &q.order_id)
                    .collect::<Vec<_>>(),
                h.engine
                    .operations()
                    .unwrap()
                    .iter()
                    .filter_map(|record| record
                        .effects
                        .as_ref()
                        .and_then(|effects| effects.evidence.get("correlation")))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(destination.metadata["state"], "completed");
    let observed = Box::pin(h.engine.status(&action.id, true)).await.unwrap();
    let view = h.engine.operation_view(&observed).unwrap();
    assert_eq!(view["outcome"]["phase"], "done", "parent outcome: {view}");
    assert_ne!(
        view["outcome"]["next_action"]["code"], "continue",
        "settled action must not offer another stage: {view}"
    );
    assert!(view["outcome"]["next_action"]["continuation_token"].is_null());
    let context = gofin_domain::PreparationContext {
        logical_operation_id: Some(action.id.clone()),
        positions: vec![],
    };
    assert!(
        h.bridge
            .prepare_remaining_action(&action, &request.input, &[], &context)
            .await
            .is_err(),
        "a completed bridge has no stage to reauthorize"
    );
    let duplicate = Box::pin(h.engine.execute(request)).await.unwrap();
    assert_eq!(duplicate.id, action.id);
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.source.broadcasts(),
        2,
        "completed stages are never repeated"
    );
}

#[tokio::test]
async fn revised_bridge_review_keeps_transfer_identity_and_refuses_changed_recipient() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds});
    let preview = h.bridge.prepare("bridge.execute", &input).await.unwrap();
    let original: gofin_domain::OperationRecord = serde_json::from_value(json!({"id":"synthetic-parent","request_id":"synthetic-request","fingerprint":"synthetic","fingerprint_version":1,"operation":"bridge.execute","input":input,"status":"pending","created_at":now(),"updated_at":now(),"preview":preview,"approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null})).unwrap();
    let mut revised = original.input.clone();
    revised["bounds"]["fee_caps"][0]["maximum"] = json!("2000000000000000");
    revised["actionDeadline"] = json!(chrono::Utc::now().timestamp() as u64 + 900);
    let context = gofin_domain::PreparationContext {
        logical_operation_id: Some(original.id.clone()),
        positions: vec![],
    };
    let candidate = h
        .bridge
        .prepare_remaining_action(&original, &revised, &[], &context)
        .await
        .unwrap();
    assert_eq!(
        candidate.binding["transfer_id"],
        original.preview.as_ref().unwrap().binding["transfer_id"]
    );
    assert_eq!(
        candidate.binding["provider"],
        original.preview.as_ref().unwrap().binding["provider"]
    );
    assert_eq!(candidate.input, revised);
    assert_eq!(h.source.broadcasts(), 0);
    let mut wrong = revised;
    wrong["intent"]["recipient"] = json!("0x1111111111111111111111111111111111111111");
    assert!(
        h.bridge
            .prepare_remaining_action(&original, &wrong, &[], &context)
            .await
            .is_err()
    );
    assert_eq!(h.source.broadcasts(), 0);
}

#[tokio::test]
async fn bridge_reauthorizes_remaining_gas_without_repeating_settled_allowance() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
    h.source.rise_gas_after_first.store(true, Ordering::SeqCst);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds});
    let request = OperationRequest {
        request_id: "synthetic-bridge-reauthorization".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: input.clone(),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request)))
        .await
        .unwrap();
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "{action:?}");
    assert_eq!(
        h.source.broadcasts(),
        1,
        "only allowance should be submitted: {action:?}"
    );
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let transfer = h.state.transfer(transfer_id).unwrap().unwrap();
    assert_eq!(
        transfer.steps[0].state,
        gofin_domain::BridgeStepState::Settled
    );
    let allowance_id = transfer.steps[0].operation_id.as_ref().unwrap();
    let allowance = h
        .engine
        .operations()
        .unwrap()
        .into_iter()
        .find(|record| &record.id == allowance_id)
        .unwrap();
    assert!(allowance.effects.as_ref().unwrap().effects.iter().any(|effect| matches!(effect, gofin_domain::FinancialEffect::Cost { amount, .. } if !amount.atoms.is_zero())), "settled allowance has a proved consumed native fee");
    let view = h.engine.operation_view(&action).unwrap();
    let token = view
        .pointer("/outcome/next_action/continuation_token")
        .or_else(|| view.pointer("/next_action/continuation_token"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("reauthorization token: {view}"));
    let mut revised = input;
    revised["bounds"]["fee_caps"][0]["maximum"] = json!("4000000000000000");
    revised["actionDeadline"] = json!(chrono::Utc::now().timestamp() as u64 + 900);
    let review = invocation
        .scope(Box::pin(
            h.engine.reauthorize_operation(&action.id, token, revised),
        ))
        .await
        .unwrap();
    assert_eq!(review["reauthorization"], "approved", "{review}");
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.source.broadcasts(),
        2,
        "the active reauthorization invocation advances only the remaining deposit"
    );
    let refreshed = h.state.transfer(transfer_id).unwrap().unwrap();
    assert!(
        refreshed
            .steps
            .iter()
            .all(|step| step.state == gofin_domain::BridgeStepState::Settled)
    );
    assert_eq!(
        refreshed.steps[0].operation_id, transfer.steps[0].operation_id,
        "settled allowance identity is never replaced"
    );
}

#[tokio::test]
async fn bridge_keeps_executed_allowance_but_defers_deposit_when_native_fee_missing() {
    let h = Harness::with_source_allowance(gofin_evm::alloy::primitives::U256::ZERO);
    h.destination
        .destination_ready
        .store(false, Ordering::SeqCst);
    h.source
        .omit_source_receipt_fee
        .store(true, Ordering::SeqCst);
    let mut bounds = h.bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    bounds.latest_protocol_deadline = Some(chrono::Utc::now().timestamp() as u64 + 600);
    let request = OperationRequest {
        request_id: "synthetic-bridge-missing-fee".into(),
        operation: "bridge.execute".into(),
        run_id: None,
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":h.wallet,"refundTo":h.wallet,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds}),
    };
    let invocation =
        gofin_domain::execution_progress::Invocation::new(std::time::Duration::from_secs(20));
    let action = invocation
        .scope(Box::pin(h.engine.execute(request)))
        .await
        .unwrap();
    assert_eq!(
        h.source.broadcasts(),
        1,
        "missing fee must not consume zero fee room: {action:?}"
    );
    let transfer_id = action.preview.as_ref().unwrap().binding["transfer_id"]
        .as_str()
        .unwrap();
    let transfer = h.state.transfer(transfer_id).unwrap().unwrap();
    assert_eq!(
        transfer.steps[0].state,
        gofin_domain::BridgeStepState::Settled
    );
    let id = transfer.steps[0].operation_id.as_ref().unwrap();
    let child = h
        .engine
        .operations()
        .unwrap()
        .into_iter()
        .find(|r| &r.id == id)
        .unwrap();
    assert_eq!(child.status, Status::Completed);
    assert_eq!(
        child.last_reconciliation_evidence.as_ref().unwrap()["native_fee_observed"],
        false
    );
    assert_eq!(
        child.last_reconciliation_evidence.as_ref().unwrap()["outcome_reporting"],
        "partial"
    );
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
    h.source
        .omit_source_receipt_fee
        .store(false, Ordering::SeqCst);
    let observed = Box::pin(h.engine.status(&action.id, true)).await.unwrap();
    let updated = h
        .engine
        .operations()
        .unwrap()
        .into_iter()
        .find(|r| &r.id == id)
        .unwrap();
    assert_eq!(updated.status, Status::Completed);
    assert_eq!(
        updated.last_reconciliation_evidence.as_ref().unwrap()["native_fee_observed"],
        true
    );
    assert_eq!(h.source.broadcasts(), 1, "status never sends a write");
    let view = h.engine.operation_view(&observed).unwrap();
    assert_ne!(view["outcome"]["phase"], "done", "{view}");
    let token = view
        .pointer("/outcome/next_action/continuation_token")
        .or_else(|| view.pointer("/next_action/continuation_token"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("continuation token: {view}"));
    invocation
        .scope(Box::pin(h.engine.continue_operation(&action.id, token)))
        .await
        .unwrap();
    assert_eq!(
        h.source.broadcasts(),
        2,
        "only the remaining deposit is submitted"
    );
    assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
}

use gofin_protocol::BrokerRequest;
include!("support/kraken_plan_client.rs");

#[tokio::test]
#[ignore = "requires an explicitly built development CLI"]
async fn cli_relay_evm_compound_runs_on_normal_worker_stack() {
    Box::pin(async {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let dir = tempfile::tempdir().unwrap();
    let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
    let address = gofin_evm::lower_address(signer.address());
    let source = Arc::new(Rpc::source_with_allowance(
        address.clone(),
        gofin_evm::alloy::primitives::U256::from(5_000_000u64),
    ));
    let destination = Arc::new(Rpc::destination_on(address.clone(), 42161));
    let source_runtime = Arc::new(EvmRuntime::new(source.clone(), 1, Some(signer)));
    let destination_runtime = Arc::new(EvmRuntime::new(destination.clone(), 42161, None));
    let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
    let source_wallet = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(1, 1, vec![]),
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            source_runtime.clone(),
        )
        .unwrap()
        .with_wallet_valuation(prices.clone()),
    );
    let destination_wallet = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::wallet_only(42161, 1, vec![]),
            vec![WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: false,
            }],
            destination_runtime.clone(),
        )
        .unwrap(),
    );
    let provider = Arc::new(Erc20Provider::with_destination(
        destination,
        DestinationPayout::Router,
    ));
    let bridge = Arc::new(
        BridgeAdapter::with_provider_and_observers(
            provider,
            BTreeMap::from([(1, source_wallet.clone()), (42161, destination_wallet)]),
            BTreeMap::from([(1, source_runtime), (42161, destination_runtime)]),
            prices.clone(),
        )
        .unwrap(),
    );
    let approval = Arc::new(CountingApproval::default());
    let broker = gofin_broker::compose_runtime(
        gofin_broker::BrokerConfigV2 {
            version: 2,
            networks: gofin_broker::NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: gofin_broker::BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        gofin_broker::RuntimeDependencies {
            store: Store::open(dir.path().join("journal.sqlite")).unwrap(),
            adapters: vec![source_wallet, bridge.clone()],
            authorizer: approval.clone(),
            valuator: prices,
            bridge: Some(bridge),
        },
    )
    .unwrap();
    let client = Client::new(broker, dir.path()).await;
    let mut bounds = Harness::new().bounds();
    bounds.input_usd_micros = None;
    bounds.source_usd_per_unit_micros = None;
    bounds.maximum_total_usd_micros = None;
    bounds.authorization_seconds = None;
    for cap in &mut bounds.fee_caps {
        cap.maximum_usd_micros = None;
    }
    let input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":ARB_USDC}],"recipient":address,"refundTo":address,"allowedRefundAssets":[{"chainId":1,"token":ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"actionDeadline":chrono::Utc::now().timestamp() as u64 + 600,"bounds":bounds});
    let request = OperationRequest {
        request_id: "cli-relay-normal-stack".into(),
        operation: "bridge.execute".into(),
        input: input.clone(),
        run_id: None,
    };
    let result = client
        .call(
            args(&[
                "execute",
                "bridge.execute",
                "--input",
                &input.to_string(),
                "--id",
                &request.request_id,
                "--json",
            ]),
            BrokerRequest::Execute { request: request.clone() },
            true,
        )
        .await;
    assert_eq!(approval.0.load(Ordering::SeqCst), 1, "{result}");
    assert_eq!(source.broadcasts(), 3, "{result}");
        assert_eq!(result["outcome"]["result"], "executed", "{result}");
        let repeated = client.call(args(&["execute", "bridge.execute", "--input", &input.to_string(), "--id", &request.request_id, "--json"]), BrokerRequest::Execute { request }, true).await;
        assert_eq!(repeated["id"], result["id"]);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(source.broadcasts(), 3);
    }).await;
}
