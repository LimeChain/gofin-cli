//! Public broker/engine ERC-20 allowance lifecycle using only local fixtures.
use async_trait::async_trait;
use fin_bridge::*;
use fin_broker::{BridgeAdapter, SharedUsdValuator};
use fin_domain::{
    Adapter, ApprovalAudit, AuthorizationChallenge, Authorizer, BridgeStepKind as DomainStepKind,
    BridgeStepLink, OperationRequest, Quantity, Result, Status,
    boundaries::BoundaryAuthorizationRequest,
    financial_plans::{
        FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
        FinancialPlanStepSpec, FinancialStepCompletion,
    },
    now,
};
use fin_engine::{Engine, EngineOptions};
use fin_ethereum::{EthereumAdapter, EthereumConfig, WalletConnection};
use fin_evm::{EvmRuntime, LocalEnvelopeSigner};
use fin_store::{BridgeStateRepository, Store};
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
#[async_trait]
impl Authorizer for CountingApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic test authorizer".into(),
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
    approval: Arc<CountingApproval>,
    wallet: String,
}
impl Harness {
    fn new() -> Self {
        Self::with_source_allowance(fin_evm::alloy::primitives::U256::from(5_000_000u64))
    }

    fn with_source_allowance(initial_allowance: fin_evm::alloy::primitives::U256) -> Self {
        Self::with_destination_payout(initial_allowance, DestinationPayout::Router)
    }

    fn with_destination_payout(
        initial_allowance: fin_evm::alloy::primitives::U256,
        payout: DestinationPayout,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("journal.sqlite"))
            .unwrap()
            .share();
        let state = BridgeStateRepository::new(store.clone());
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let wallet = fin_evm::lower_address(signer.address());
        let source = Arc::new(Rpc::source_with_allowance(
            wallet.clone(),
            initial_allowance,
        ));
        let destination = Arc::new(Rpc::destination(wallet.clone()));
        let source_runtime = Arc::new(EvmRuntime::new(source.clone(), 1, Some(signer)));
        let destination_runtime = Arc::new(EvmRuntime::new(destination.clone(), 42161, None));
        let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(Prices)));
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
                EthereumConfig::wallet_only(42161, 1, vec![]),
                vec![WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: wallet.clone(),
                    signing_enabled: false,
                }],
                destination_runtime.clone(),
            )
            .unwrap(),
        );
        let bridge = Arc::new(
            BridgeAdapter::with_provider_and_observers(
                Arc::new(Erc20Provider::with_destination(destination.clone(), payout)),
                BTreeMap::from([(1, source_wallet.clone()), (42161, destination_wallet)]),
                BTreeMap::from([(1, source_runtime), (42161, destination_runtime)]),
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
            approval,
            wallet,
        }
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
            input_usd_micros: Some(AmountInterval {
                minimum: q("10000000"),
                maximum: q("10000000"),
            }),
            source_usd_per_unit_micros: Some(AmountInterval {
                minimum: q("1000000"),
                maximum: q("1000000"),
            }),
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
                maximum_usd_micros: Some(2_000_000),
            }],
            maximum_total_usd_micros: Some(14_000_000),
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

    async fn execute(
        &self,
        grant: &str,
        transfer: &str,
        step: &BridgeStepLink,
        request_id: &str,
        bounds: &BridgeBounds,
    ) -> fin_domain::OperationRecord {
        self.engine
            .execute_boundary(
                grant,
                OperationRequest {
                    request_id: request_id.into(),
                    operation: "bridge.transfer".into(),
                    input: self.step_input(transfer, step, Some(bounds.clone())),
                    run_id: None,
                },
            )
            .await
            .unwrap()
    }

    async fn execute_exact(
        &self,
        transfer: &str,
        step: &BridgeStepLink,
        request_id: &str,
        bounds: &BridgeBounds,
    ) -> fin_domain::OperationRecord {
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
                .all(|step| step.state == fin_domain::BridgeStepState::Settled)
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
    assert_eq!(record.state, fin_domain::BridgeTransferState::InTransit);
    assert!(record.transit.is_some());
    (transfer, bounds)
}

#[tokio::test]
async fn direct_native_order_tagged_payout_accepts_alternate_nonself_sender() {
    let harness = Harness::with_destination_payout(
        fin_evm::alloy::primitives::U256::from(5_000_000u64),
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
        fin_evm::alloy::primitives::U256::from(5_000_000u64),
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
async fn direct_payout_with_provider_request_tag_stays_in_transit_without_effects() {
    let harness = Harness::with_destination_payout(
        fin_evm::alloy::primitives::U256::from(5_000_000u64),
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
    assert_eq!(record.state, fin_domain::BridgeTransferState::InTransit);
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
            fin_evm::alloy::primitives::U256::from(5_000_000u64),
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
        assert_eq!(record.state, fin_domain::BridgeTransferState::InTransit);
        assert!(record.transit.is_some());
        assert!(record.settlement_proofs.is_empty());
    }
}

#[tokio::test]
async fn named_plan_permissions_bind_exact_bridge_steps_without_source_balance_or_simulation() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let h =
            Harness::with_source_allowance(fin_evm::alloy::primitives::U256::from(5_000_000u64));
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut links: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        links.sort_by_key(|link| match link.kind {
            DomainStepKind::ResetAllowance => 0,
            DomainStepKind::Approval => 1,
            DomainStepKind::Deposit => 2,
        });
        assert_eq!(links.len(), 3);

        // A future-funded plan must be reviewable while this wallet has no
        // source USDC. Quote construction reads allowance but does not inspect
        // token balances; plan permission must not estimate/simulate a tx.
        *h.source.token_balance.lock().unwrap() = fin_evm::alloy::primitives::U256::ZERO;
        assert_eq!(h.source.token_balance_reads(), 0);
        assert_eq!(h.source.estimate_gas_calls(), 0);
        let bounds = h.bounds();
        let inputs = links
            .iter()
            .map(|link| h.step_input(&transfer, link, Some(bounds.clone())))
            .collect::<Vec<_>>();

        let mut wrong_identity = inputs[0].clone();
        wrong_identity["stepKind"] = json!("deposit");
        let error = h
            .bridge
            .prepare_plan_permission("bridge.transfer", &wrong_identity, &json!({}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "BRIDGE_STEP_KIND_MISMATCH");

        let plan_steps = links
            .iter()
            .zip(inputs.iter())
            .enumerate()
            .map(|(index, (_link, input))| FinancialPlanStepSpec {
                step_id: format!("bridge-step-{index}"),
                operation: "bridge.transfer".into(),
                input: input.clone(),
                permitted_variations: json!({}),
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
            })
            .collect::<Vec<_>>();
        let wallet_resource = fin_domain::ResourceId(format!("ethereum:1:{}", h.wallet));
        let source_asset = fin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: ETH_USDC.into(),
            decimals: 6,
        };
        let gas_asset = fin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let plan = FinancialPlanSpec {
            version: fin_domain::financial_plans::FINANCIAL_PLAN_VERSION,
            name: "Bridge route review".into(),
            steps: plan_steps,
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![fin_domain::ResourceAmount {
                    resource: wallet_resource.clone(),
                    amount: fin_domain::Amount {
                        asset: source_asset,
                        atoms: q("12000000"),
                    },
                }],
                // Each concrete EVM step has a finite per-step preview cap;
                // the transfer cumulative scope below separately limits their
                // combined paid cost to one route cap.
                maximum_costs: vec![fin_domain::CostBound {
                    location: fin_domain::AccountingLocation::Cash {
                        resource: wallet_resource,
                    },
                    amount: fin_domain::Amount {
                        asset: gas_asset,
                        atoms: q("3000000000000000"),
                    },
                    treatment: fin_domain::CostTreatment::AdditionalDebit,
                }],
            },
        };
        let request = FinancialPlanAuthorizationRequest {
            request_id: "synthetic-bridge-plan-preview".into(),
            plan,
            valid_for_seconds: 600,
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = h.engine.preview_financial_plan(request).await.unwrap();
        assert_eq!(review.permissions.len(), 3);
        for (permission, link) in review.permissions.iter().zip(links.iter()) {
            let adapter = permission.permission.as_ref().unwrap();
            assert_eq!(
                adapter.spec.fixed["plan_step_identity"]["transfer_id"],
                transfer
            );
            assert_eq!(
                adapter.spec.fixed["plan_step_identity"]["step_id"],
                link.step_id
            );
            assert_eq!(
                adapter.spec.fixed["plan_step_identity"]["step_kind"],
                serde_json::to_value(&link.kind).unwrap()
            );
            let cumulative = adapter.cumulative_scope.as_ref().unwrap();
            assert_eq!(cumulative.scope_id, transfer);
            assert_eq!(cumulative.maximum_debits.len(), 1);
            assert_eq!(cumulative.maximum_debits[0].amount.atoms, q("12000000"));
            assert_eq!(cumulative.maximum_costs.len(), 1);
            assert_eq!(
                cumulative.maximum_costs[0].amount.atoms,
                q("1000000000000000")
            );
        }
        assert_eq!(h.source.token_balance_reads(), 0);
        assert_eq!(h.source.estimate_gas_calls(), 0);
        assert_eq!(h.source.broadcasts(), 0);
    })
    .await
    .expect("future-funded bridge permissions should be read-only");
}

#[tokio::test]
async fn named_bridge_plan_keeps_parent_through_refresh_and_waits_for_destination_proof() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let h =
            Harness::with_source_allowance(fin_evm::alloy::primitives::U256::from(5_000_000u64));
        let quote = h
            .bridge
            .read("bridge.quotes", &h.quote_input(None))
            .await
            .unwrap();
        let transfer = quote["transfers"][0]["transfer_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut links: Vec<BridgeStepLink> =
            serde_json::from_value(quote["transfers"][0]["steps"].clone()).unwrap();
        links.sort_by_key(|link| match link.kind {
            DomainStepKind::ResetAllowance => 0,
            DomainStepKind::Approval => 1,
            DomainStepKind::Deposit => 2,
        });
        let bounds = h.bounds();
        let inputs = links
            .iter()
            .map(|link| h.step_input(&transfer, link, Some(bounds.clone())))
            .collect::<Vec<_>>();
        let mut plan = bridge_plan_spec(&h, &links, &inputs);
        plan.steps[2].completion.minimum_net_receipt = Some(fin_domain::ResourceAmount {
            resource: fin_domain::ResourceId(format!("ethereum:42161:{}", h.wallet)),
            amount: bounds.minimum_net_receipt.clone().unwrap(),
        });
        plan.steps.push(FinancialPlanStepSpec {
            step_id: "bridge-status-after-settlement".into(),
            operation: "bridge.status".into(),
            input: json!({"transferId":transfer}),
            permitted_variations: json!({
                "mode":"exact",
                "maximum_debits":[],
                "maximum_costs":[],
                "possible_credits":[]
            }),
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: None,
            },
            funding_dependencies: vec![],
        });
        let preview_request = FinancialPlanAuthorizationRequest {
            request_id: format!("plan-preview-{transfer}"),
            plan: plan.clone(),
            valid_for_seconds: 600,
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = h
            .engine
            .preview_financial_plan(preview_request)
            .await
            .unwrap();
        let plan_id = h
            .engine
            .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                request_id: format!("plan-authorize-{transfer}"),
                plan,
                valid_for_seconds: 600,
                replaces_revision_id: None,
                expected_review_hash: Some(review.review_hash),
            })
            .await
            .unwrap()
            .plan_id;
        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            1,
            "one Touch ID authorizes the named plan"
        );

        // Simulate an independently completed upstream funding step after plan
        // review. The plan permission itself succeeded while the balance was 0.
        *h.source.token_balance.lock().unwrap() =
            fin_evm::alloy::primitives::U256::from(20_000_000u64);

        // Model a provider quote expiring while an upstream funding step is
        // being completed. Refresh after plan approval, before the first
        // bridge claim, and retain the exact selected bounds and step IDs.
        let latest = h.state.transfer(&transfer).unwrap().unwrap();
        let mut expired = latest.quote_revisions.last().unwrap().clone();
        expired.revision += 1;
        expired.observed_at = "2026-10-01T00:00:00Z".into();
        expired.valid_until = "2026-10-01T00:01:00Z".into();
        expired.payload["quote"]["observed_at"] = json!(expired.observed_at);
        expired.payload["quote"]["valid_until"] = json!(expired.valid_until);
        h.state.append_quote_revision(&transfer, &expired).unwrap();
        let preclaim_refresh = h
            .bridge
            .read("bridge.quotes", &h.quote_input(Some(transfer.clone())))
            .await
            .unwrap();
        let preclaim_links: Vec<BridgeStepLink> =
            serde_json::from_value(preclaim_refresh["transfers"][0]["steps"].clone()).unwrap();
        assert_eq!(
            preclaim_links
                .iter()
                .map(|link| link.step_id.clone())
                .collect::<Vec<_>>(),
            links
                .iter()
                .map(|link| link.step_id.clone())
                .collect::<Vec<_>>(),
            "refresh after plan approval retains exact provider step identities"
        );
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);

        let execute_plan_step = |index: usize, attempt: String| {
            let engine = h.engine.clone();
            let plan_id = plan_id.clone();
            let input = inputs[index].clone();
            async move {
                engine
                    .execute_financial_plan_step(
                        &plan_id,
                        &format!("bridge-step-{index}"),
                        OperationRequest {
                            request_id: attempt,
                            operation: "bridge.transfer".into(),
                            input,
                            run_id: None,
                        },
                    )
                    .await
                    .unwrap()
            }
        };

        let reset = execute_plan_step(0, "plan-erc20-reset".into()).await;
        assert_eq!(
            reset.status,
            Status::Completed,
            "reset error={:?}",
            reset.error
        );
        let approve = execute_plan_step(1, "plan-erc20-approve".into()).await;
        assert_eq!(
            approve.status,
            Status::Completed,
            "approval error={:?}",
            approve.error
        );
        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            1,
            "plan steps reuse the revision approval without child grants"
        );

        // A quote refresh before principal dispatch is still on the same
        // protected parent and preserves each exact step and plan authority.
        let refreshed = h
            .bridge
            .read("bridge.quotes", &h.quote_input(Some(transfer.clone())))
            .await
            .unwrap();
        let refreshed_links: Vec<BridgeStepLink> =
            serde_json::from_value(refreshed["transfers"][0]["steps"].clone()).unwrap();
        assert_eq!(
            refreshed_links
                .iter()
                .map(|link| link.step_id.clone())
                .collect::<Vec<_>>(),
            links
                .iter()
                .map(|link| link.step_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(refreshed_links[..2].iter().all(|link| {
            link.state == fin_domain::BridgeStepState::Settled
                && link
                    .plan_authority
                    .as_ref()
                    .is_some_and(|authority| authority.plan_id == plan_id)
        }));
        assert!(refreshed_links[2].plan_authority.is_none());
        let saved = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(
            saved.grant_id, None,
            "named plans do not synthesize a bridge boundary grant"
        );
        assert_eq!(saved.plan_scope.as_ref().unwrap().plan_id, plan_id);

        let deposit = execute_plan_step(2, "plan-erc20-deposit".into()).await;
        assert_eq!(
            deposit.status,
            Status::Completed,
            "deposit error={:?}",
            deposit.error
        );
        let source_only_status = h.engine.financial_plan_status(&plan_id).await.unwrap();
        assert!(
            !source_only_status.blockers.is_empty(),
            "source proof cannot satisfy the parent-level destination settlement obligation"
        );
        assert_eq!(
            source_only_status.steps[2].state,
            fin_domain::financial_plans::FinancialPlanStepProgressState::InProgress,
            "an explicit destination receipt floor remains pending after source proof"
        );
        assert!(
            source_only_status.usage.verified_proceeds.is_empty(),
            "no destination proceeds become spendable before final receipt proof"
        );
        assert!(
            source_only_status.next_eligible_step_id.is_none(),
            "pending destination floor must block later plan steps: {:?}",
            source_only_status.next_eligible_step_id
        );
        assert_eq!(
            h.source.broadcasts(),
            3,
            "status must not replay source writes"
        );
        let in_transit = h.state.transfer(&transfer).unwrap().unwrap();
        assert!(
            in_transit
                .transit
                .as_ref()
                .is_some_and(|transit| !transit.spendable),
            "source settlement records nonspendable transit while destination remains pending"
        );
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);

        let status = h
            .bridge
            .read("bridge.status", &json!({"transferId":transfer}))
            .await
            .unwrap();
        assert_eq!(status["state"], "completed", "final status={status}");
        let settled = h.engine.financial_plan_status(&plan_id).await.unwrap();
        assert!(
            settled.blockers.is_empty(),
            "final proof clears bridge blocker: {:?}",
            settled.blockers
        );
        assert!(settled.steps.iter().take(3).all(|step| {
            step.state == fin_domain::financial_plans::FinancialPlanStepProgressState::Completed
        }));
        assert_eq!(
            settled.next_eligible_step_id.as_deref(),
            Some("bridge-status-after-settlement"),
            "destination proof completes the receipt-bound deposit and exposes the next independent step"
        );
        assert_eq!(
            h.approval.0.load(Ordering::SeqCst),
            1,
            "proof-only status reconciliation needs no extra approval"
        );
    })
    .await
    .expect("named bridge plan should retain its parent until final destination proof");
}

fn bridge_plan_spec(h: &Harness, links: &[BridgeStepLink], inputs: &[Value]) -> FinancialPlanSpec {
    assert_eq!(links.len(), inputs.len());
    let steps = links
        .iter()
        .zip(inputs)
        .enumerate()
        .map(|(index, (_, input))| FinancialPlanStepSpec {
            step_id: format!("bridge-step-{index}"),
            operation: "bridge.transfer".into(),
            input: input.clone(),
            permitted_variations: json!({}),
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: None,
            },
            funding_dependencies: vec![],
        })
        .collect();
    FinancialPlanSpec {
        version: fin_domain::financial_plans::FINANCIAL_PLAN_VERSION,
        name: "Synthetic bridge plan".into(),
        steps,
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![fin_domain::ResourceAmount {
                resource: fin_domain::ResourceId(format!("ethereum:1:{}", h.wallet)),
                amount: fin_domain::Amount {
                    asset: fin_domain::AssetId {
                        network: "ethereum:1".into(),
                        reference: ETH_USDC.into(),
                        decimals: 6,
                    },
                    atoms: q("12000000"),
                },
            }],
            maximum_costs: vec![fin_domain::CostBound {
                location: fin_domain::AccountingLocation::Cash {
                    resource: fin_domain::ResourceId(format!("ethereum:1:{}", h.wallet)),
                },
                amount: fin_domain::Amount {
                    asset: fin_domain::AssetId {
                        network: "ethereum:1".into(),
                        reference: "native".into(),
                        decimals: 18,
                    },
                    atoms: q("3000000000000000"),
                },
                treatment: fin_domain::CostTreatment::AdditionalDebit,
            }],
        },
    }
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
            fin_evm::alloy::primitives::U256::from(2_000_000u64)
        );
        assert_eq!(
            *h.source.token_balance.lock().unwrap(),
            fin_evm::alloy::primitives::U256::from(10_000_000u64)
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
                .all(|s| s.state == fin_domain::BridgeStepState::Settled)
        );
        assert!(saved.applied_proofs.len() >= 2);
        assert_eq!(
            saved.settlement_proofs.last().unwrap().outcome,
            fin_domain::BridgeSettlementOutcome::Destination
        );
    })
    .await
    .expect("ordinary ERC-20 lifecycle should not deadlock");
}

#[tokio::test]
async fn exact_step_flow_with_zero_initial_allowance_approves_then_deposits() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let h = Harness::with_source_allowance(fin_evm::alloy::primitives::U256::ZERO);
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
            fin_evm::alloy::primitives::U256::from(2_000_000u64)
        );
        assert_eq!(
            *h.source.token_balance.lock().unwrap(),
            fin_evm::alloy::primitives::U256::from(10_000_000u64)
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
                .all(|s| s.state == fin_domain::BridgeStepState::Settled)
        );
        assert!(saved.applied_proofs.len() >= 2);
        assert_eq!(
            saved.settlement_proofs.last().unwrap().outcome,
            fin_domain::BridgeSettlementOutcome::Destination
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
            let h = Harness::with_source_allowance(fin_evm::alloy::primitives::U256::ZERO);
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
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let h = Harness::new();
        let first = h.bridge.read("bridge.quotes", &h.quote_input(None)).await.unwrap();
        let transfer = first["transfers"][0]["transfer_id"].as_str().unwrap().to_owned();
        let mut steps: Vec<BridgeStepLink> = serde_json::from_value(first["transfers"][0]["steps"].clone()).unwrap();
        steps.sort_by_key(|step| match step.kind { DomainStepKind::ResetAllowance => 0, DomainStepKind::Approval => 1, DomainStepKind::Deposit => 2 });
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].kind, DomainStepKind::ResetAllowance);
        assert_eq!(steps[1].kind, DomainStepKind::Approval);
        assert_eq!(steps[2].kind, DomainStepKind::Deposit);
        let step_ids = steps.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>();

        let bounds = h.bounds();
        let mut serialized_bounds = serde_json::to_value(&bounds).unwrap();
        omit_nulls(&mut serialized_bounds);
        let authorization = h.engine.authorize_boundary(BoundaryAuthorizationRequest {
            request_id: format!("authorize-{transfer}"),
            operation: "bridge.transfer".into(),
            input: h.step_input(&transfer, &steps[0], Some(bounds.clone())),
            bounds: serialized_bounds,
            valid_for_seconds: 600,
            replaces: None,
        }).await.unwrap();
        let grant = authorization.id;
        assert_eq!(authorization.spec.maximum_costs.len(), 1, "one cumulative source gas cap must cover the three wallet steps");
        assert_eq!(authorization.spec.maximum_costs[0].amount.asset.reference, "native");
        assert_eq!(authorization.spec.maximum_costs[0].amount.atoms, q("1000000000000000"));

        let reset = h.execute(&grant, &transfer, &steps[0], "erc20-reset", &bounds).await;
        assert_eq!(reset.status, Status::Completed, "reset error={:?}", reset.error);
        assert_eq!(*h.source.allowance.lock().unwrap(), fin_evm::alloy::primitives::U256::ZERO);
        let approval = h.execute(&grant, &transfer, &steps[1], "erc20-approve", &bounds).await;
        assert_eq!(approval.status, Status::Completed, "approve error={:?}", approval.error);
        assert_eq!(*h.source.allowance.lock().unwrap(), fin_evm::alloy::primitives::U256::from(12_000_000u64));
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 1, "one finite grant authorizes all three steps");

        let status_before_refresh = h.engine.boundary_status(&grant).await.unwrap();
        let spent_before_refresh = status_before_refresh.authorization.spent_costs.clone();
        assert_eq!(spent_before_refresh.len(), 1);
        assert!(spent_before_refresh[0].amount.atoms > q("0"));
        let after_approval = h.state.transfer(&transfer).unwrap().unwrap();
        assert_eq!(after_approval.steps.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>(), step_ids);
        assert!(after_approval.steps[..2].iter().all(|s| s.state == fin_domain::BridgeStepState::Settled));

        let refreshed = h.bridge.read("bridge.quotes", &h.quote_input(Some(transfer.clone()))).await.unwrap();
        let refreshed_steps: Vec<BridgeStepLink> = serde_json::from_value(refreshed["transfers"][0]["steps"].clone()).unwrap();
        assert_eq!(refreshed_steps.iter().map(|s| s.step_id.clone()).collect::<Vec<_>>(), step_ids, "refresh keeps every deterministic step identity");
        assert!(refreshed_steps[..2].iter().all(|s| s.state == fin_domain::BridgeStepState::Settled));
        assert_eq!(*h.source.allowance.lock().unwrap(), fin_evm::alloy::primitives::U256::from(12_000_000u64), "refresh observes the already finite allowance and does not reset it");
        let status_after_refresh = h.engine.boundary_status(&grant).await.unwrap();
        assert_eq!(status_after_refresh.authorization.spent_costs, spent_before_refresh, "quote refresh does not replay or refund settled gas");
        assert_eq!(h.source.broadcasts(), 2);

        let deposit = h.execute(&grant, &transfer, &refreshed_steps[2], "erc20-deposit", &bounds).await;
        assert_eq!(deposit.status, Status::Completed, "deposit error={:?}", deposit.error);
        assert_eq!(h.source.broadcasts(), 3);
        assert_eq!(h.approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(*h.source.token_balance.lock().unwrap(), fin_evm::alloy::primitives::U256::from(10_000_000u64), "the source transferFrom consumes exactly 10 USDC");
        let final_status = h.bridge.read("bridge.status", &json!({"transferId":transfer})).await.unwrap();
        assert_eq!(final_status["state"], "completed", "destination settlement requires exact router and ERC-20 evidence: {final_status}");
        let saved = h.state.transfer(&transfer).unwrap().unwrap();
        assert!(saved.applied_proofs.len() >= 2, "source RelayErc20Deposit and canonical token Transfer, plus destination FundsMovement and Transfer are verified");
        let settlement = saved.settlement_proofs.last().expect("destination receipt proof is persisted");
        assert_eq!(settlement.outcome, fin_domain::BridgeSettlementOutcome::Destination);
        assert_eq!(saved.settlement_expectation.as_ref().unwrap().destination[0].minimum_atoms, q("9500000"));
        let final_grant = h.engine.boundary_status(&grant).await.unwrap();
        assert_eq!(final_grant.authorization.spec.maximum_costs.len(), 1, "cumulative gas ceiling is recorded once");
        assert!(final_grant.authorization.spent_costs[0].amount.atoms <= q("1000000000000000"));
        // Baseline was 5 USDC and approved cap was 12: the deposit consumes
        // 10, leaving 2. Restoring the baseline requires separately approved
        // allowance writes and is deliberately outside this lifecycle test.
        assert_eq!(*h.source.allowance.lock().unwrap(), fin_evm::alloy::primitives::U256::from(2_000_000u64));
    }).await.expect("ERC-20 lifecycle should not deadlock");
}
