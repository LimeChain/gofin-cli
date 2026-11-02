//! Cross-layer Uniswap boundary and plan checks over the shared synthetic RPC fixture.

use crate::tests::{Fixture, adapter, addr, swap_input};
use crate::{
    AaveConfig, EthereumAdapter, EthereumConfig, RouterKind, UniswapConfig, WalletConnection,
    WalletValuation,
};
use gofin_domain::Adapter;
use gofin_domain::boundaries::BoundaryAuthorizationRequest;
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{
    AccountingLocation, Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer,
    CostBound, CostTreatment, OperationRequest, Quantity, ResourceAmount, ResourceId, Result, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_evm::{EnvelopeSigner, EvmRuntime, LocalEnvelopeSigner};
use gofin_store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

struct SyntheticApproval;

struct CountingApproval(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Authorizer for SyntheticApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-conformance-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[async_trait::async_trait]
impl Authorizer for CountingApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-conformance-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn engine(rpc: &Arc<Fixture>) -> (Engine, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let engine = engine_at(rpc, directory.path().join("amm-conformance.sqlite"));
    (engine, directory)
}

fn engine_at(rpc: &Arc<Fixture>, path: impl AsRef<std::path::Path>) -> Engine {
    let store = Store::open(path).unwrap();
    let adapter = Arc::new(adapter(rpc.clone()));
    Engine::new(
        store,
        vec![adapter],
        Arc::new(SyntheticApproval),
        EngineOptions::default(),
    )
    .unwrap()
}

fn signed_engine(
    rpc: &Arc<Fixture>,
    path: impl AsRef<std::path::Path>,
) -> (Engine, String, Arc<AtomicUsize>) {
    // Public, deterministic synthetic key. The RPC is the in-memory Fixture;
    // its sendRawTransaction branch always fails closed and never reaches a node.
    let signer = Arc::new(
        LocalEnvelopeSigner::from_private_key(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
    );
    let owner = format!("{:#x}", signer.address());
    let mut config = EthereumConfig {
        weth: Some(format!("{:#x}", addr(100))),
        snapshot_tokens: vec![format!("{:#x}", addr(101))],
        uniswap: Some(UniswapConfig {
            factory: format!("{:#x}", addr(450)),
            quoter: format!("{:#x}", addr(460)),
            router: format!("{:#x}", addr(470)),
            router_kind: RouterKind::LegacyV3,
            v4: None,
        }),
        aave: Some(AaveConfig {
            pool: format!("{:#x}", addr(300)),
            oracle: format!("{:#x}", addr(400)),
        }),
        ..Default::default()
    };
    crate::tests::install_sushi_fixture(&mut config, rpc);
    let adapter = Arc::new(
        EthereumAdapter::new(
            config,
            vec![WalletConnection {
                name: "synthetic".into(),
                address: owner.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(
                rpc.clone(),
                1,
                Some(signer as Arc<dyn EnvelopeSigner>),
            )),
        )
        .unwrap(),
    );
    let approvals = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(
        Store::open(path).unwrap(),
        vec![adapter],
        Arc::new(CountingApproval(approvals.clone())),
        EngineOptions::default(),
    )
    .unwrap();
    (engine, owner, approvals)
}

fn bounds() -> Value {
    json!({
        "max_input":"0.002",
        "max_fee_eth":"0.0002",
        "max_slippage_bps":100,
        "max_price_impact_bps":10000,
        "minimum_rate":{"output_atoms":"2980000000","input_atoms":"1000000000000000000"},
        "absolute_minimum_output":"0.000001",
        "allowed_fee_tiers":[500],
        "max_deadline_seconds":600
    })
}

fn candidate() -> Value {
    let mut input = swap_input();
    input["request_id"] = json!("SYNTHETIC-conformance-attempt");
    input["max_price_impact_bps"] = json!(10000);
    input["fee_tier"] = json!(500);
    input["min_output"] = json!("2.98");
    input
}

#[tokio::test]
async fn retired_swap_boundary_is_rejected_before_authorization() -> Result<()> {
    let rpc = Arc::new(Fixture::new());
    let (engine, _directory) = engine(&rpc);
    let request = BoundaryAuthorizationRequest {
        request_id: "SYNTHETIC-retired-boundary".into(),
        operation: "uniswap.swap".into(),
        input: candidate(),
        bounds: bounds(),
        valid_for_seconds: 600,
        replaces: None,
    };
    assert_eq!(
        engine
            .preview_boundary_authorization(request.clone())
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(
        engine.authorize_boundary(request).await.unwrap_err().code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
    Ok(())
}

#[tokio::test]
async fn standalone_swap_approval_rejects_changed_financial_intent() -> Result<()> {
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let adapter = crate::tests::local_sushi_adapter(rpc);
    let mut input = candidate();
    input["dex"] = json!("sushi");
    input["allowed_dexes"] = json!(["sushi"]);
    let approved = adapter.prepare("swap.execute", &input).await?;
    adapter.validate_approved_candidate(&approved, &approved)?;
    for (field, value) in [
        ("recipient", json!(format!("{:#x}", addr(2)))),
        ("amount", json!("0.002")),
        ("max_fee_eth", json!("0.0002")),
    ] {
        let mut altered = approved.clone();
        altered.input[field] = value;
        assert!(
            adapter
                .validate_approved_candidate(&approved, &altered)
                .is_err(),
            "{field}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn uncertain_dispatch_is_deduplicated_across_restart() -> Result<()> {
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let tx_hash = format!("0x{}", "ab".repeat(32));
    *rpc.send_hash.lock().unwrap() = Some(tx_hash.clone());
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("uncertain-amm.sqlite");
    let (engine, owner, approvals) = signed_engine(&rpc, &database);
    let mut input = candidate();
    input["recipient"] = json!(owner);
    input["request_id"] = json!("SYNTHETIC-uncertain-write");
    let request = OperationRequest {
        request_id: "SYNTHETIC-uncertain-write".into(),
        operation: "sushi.swap".into(),
        input: input.clone(),
        run_id: None,
    };
    let first = engine.execute(request.clone()).await?;
    assert!(
        matches!(
            first.status,
            gofin_domain::Status::Pending | gofin_domain::Status::Unknown
        ),
        "receipt is intentionally unavailable: {first:?}"
    );
    let submission = first
        .submission
        .as_ref()
        .expect("submission persisted before send");
    assert!(submission.reference.starts_with("0x"));
    assert_ne!(
        submission.reference, tx_hash,
        "local submission identity is frozen independently of RPC response"
    );
    let dispatched = rpc
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| *method == "eth_sendRawTransaction")
        .count();
    assert_eq!(
        dispatched, 1,
        "first attempt reaches the deterministic synthetic RPC once"
    );

    let duplicate = engine.execute(request.clone()).await?;
    assert_eq!(duplicate.id, first.id);
    assert_eq!(
        approvals.load(Ordering::SeqCst),
        1,
        "attempt and duplicate require no second approval"
    );
    assert_eq!(
        rpc.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "eth_sendRawTransaction")
            .count(),
        dispatched,
        "duplicate must reconcile without resubmission"
    );
    drop(engine);

    let (reopened, _, _) = signed_engine(&rpc, &database);
    let after_restart = reopened.execute(request).await?;
    assert_eq!(after_restart.id, first.id);
    assert_eq!(
        rpc.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "eth_sendRawTransaction")
            .count(),
        dispatched,
        "restart must preserve duplicate protection for an uncertain write"
    );
    Ok(())
}

struct RepublishingPrices(AtomicU64);

#[tokio::test]
async fn standalone_swap_is_not_blocked_by_retired_usd_policy() -> Result<()> {
    for (movement, allocation, _expected) in [
        ("3020000", "100000000", "MOVEMENT_LIMIT"),
        ("100000000", "3020000", "ALLOCATION_LIMIT"),
    ] {
        let rpc = Arc::new(Fixture::new());
        *rpc.allowance.lock().unwrap() = crate::exact("0.01", 18)?;
        let adapter = Arc::new(crate::tests::local_sushi_adapter(rpc.clone()));
        let approvals = Arc::new(AtomicUsize::new(0));
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter],
            Arc::new(CountingApproval(approvals.clone())),
            EngineOptions {
                max_movement_and_cost_usd_micros: movement.into(),
                max_run_allocation_usd_micros: allocation.into(),
                ..EngineOptions::default()
            },
        )?;
        let result = engine
            .execute(OperationRequest {
                request_id: "SYNTHETIC-buffer-policy".into(),
                operation: "sushi.swap".into(),
                input: candidate(),
                run_id: None,
            })
            .await?;
        assert_eq!(result.status, gofin_domain::Status::Failed);
        assert_eq!(result.error.unwrap().code, "PROTECTED_SIGNER_REQUIRED");
        assert_eq!(approvals.load(Ordering::SeqCst), 1);
        assert!(result.submission.is_none());
        assert!(
            !rpc.calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "eth_sendRawTransaction")
        );
    }
    Ok(())
}

#[async_trait::async_trait]
impl WalletValuation for RepublishingPrices {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let unit = if asset.decimals == 18 {
            3_000_000_000u64
        } else {
            1_000_000
        };
        let value = gofin_evm::u256(&units)? * gofin_evm::alloy::primitives::U256::from(unit)
            / crate::pow10(asset.decimals)?;
        let published = chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst) as i64, 0)
            .unwrap()
            .to_rfc3339();
        Ok((
            gofin_evm::quantity(value),
            format!(
                "CoinGecko public USD quote; asset={}:{}; updated:{published}",
                asset.network, asset.reference
            ),
        ))
    }
}
struct NoBroadcastJournal;
impl gofin_domain::SubmissionJournal for NoBroadcastJournal {
    fn assert_authorized(&self) -> Result<()> {
        Err(gofin_domain::GoFinError::validation(
            "No broadcast is authorized in this regression.",
        ))
    }
    fn persist_submission(&self, _: &gofin_domain::Submission) -> Result<()> {
        panic!("No submission may be persisted")
    }
}

#[tokio::test]
async fn historical_usd_ceiling_remains_enforced_without_new_standalone_ceiling() -> Result<()> {
    use gofin_domain::Adapter;
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = crate::exact("0.01", 18)?;
    let prices = Arc::new(RepublishingPrices(AtomicU64::new(
        chrono::Utc::now().timestamp() as u64 - 2,
    )));
    let adapter = Arc::new(
        adapter(rpc.clone())
            .with_wallet_valuation(prices.clone())
            .with_uniswap_api(crate::uniswap_api::tests::classic_fixture_client()),
    );
    let mut input = candidate();
    input.as_object_mut().unwrap().remove("fee_tier");
    input["min_output"] = json!("2.97");
    let preview = adapter.prepare("uniswap.swap", &input).await?;
    assert!(
        preview
            .binding
            .product_get(gofin_domain::value_bounds::KEY)
            .unwrap()
            .is_none()
    );
    prices.0.fetch_add(1, Ordering::SeqCst);
    let fresh = adapter.revalidate(&preview).await?;
    assert_eq!(fresh.binding, preview.binding);
    let blocked = adapter
        .submit(&preview, Arc::new(NoBroadcastJournal))
        .await
        .unwrap_err();
    assert_eq!(blocked.code, "INVALID_INPUT");
    assert_eq!(
        blocked.message,
        "No broadcast is authorized in this regression."
    );
    let mut guarded = preview.clone();
    gofin_domain::value_bounds::bind(&mut guarded);
    let mut checked = fresh.clone();
    gofin_domain::value_bounds::carry(&guarded, &mut checked)?;
    let mut excess = checked.clone();
    excess.valuation.as_mut().unwrap().movement_usd_micros = Quantity::new(
        (fresh
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros
            .integer()
            + 1u32)
            .to_string(),
    )?;
    assert_eq!(
        gofin_domain::value_bounds::validate(&excess)
            .unwrap_err()
            .code,
        "VALUE_BOUND_EXCEEDED"
    );
    let mut changed_identity = checked;
    changed_identity.valuation.as_mut().unwrap().prices[0]
        .asset
        .network = "ethereum:999".into();
    assert_eq!(
        gofin_domain::value_bounds::validate(&changed_identity)
            .unwrap_err()
            .code,
        "VALUE_BOUND_EXCEEDED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) use crate::tests::native_approval as uniswap_native_approval;

#[cfg(target_os = "macos")]
struct NativeApiPlanApproval;
#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl Authorizer for NativeApiPlanApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let reason = uniswap_native_approval::assert_review_renders(
            challenge.review.clone().expect("typed plan review"),
            &challenge.purpose,
            &challenge.operation_id,
            challenge.binding.clone(),
            &challenge.summary,
        );
        assert_eq!(challenge.purpose, "financial_plan");
        assert!(reason.contains("approve 1 steps"), "{reason}");
        assert!(reason.contains("45 min"), "{reason}");
        SyntheticApproval.authorize(challenge).await
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn hosted_uniswap_preparation_renders_and_retired_plan_is_rejected() -> Result<()> {
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = crate::exact("0.01", 18)?;
    let adapter = Arc::new(
        adapter(rpc).with_uniswap_api(crate::uniswap_api::tests::classic_fixture_client()),
    );
    let mut input = candidate();
    input.as_object_mut().unwrap().remove("fee_tier");
    input["min_output"] = json!("2.97");
    let prepared = adapter.prepare("uniswap.swap", &input).await?;
    let reason = uniswap_native_approval::assert_prepared_renders(&prepared);
    for term in ["swap 0.001", "for at least 2.97", "Uniswap", "Slippage ≤1%"] {
        assert!(reason.contains(term), "missing {term}: {reason}");
    }
    assert!(!reason.to_lowercase().contains("fee"), "{reason}");
    assert!(
        prepared
            .intent
            .maximum_costs
            .iter()
            .any(|cost| !cost.amount.atoms.is_zero())
    );
    assert_eq!(
        gofin_domain::native_authorization_binding(&prepared)["financial_intent"]["maximum_costs"],
        serde_json::to_value(&prepared.intent.maximum_costs).unwrap()
    );
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(NativeApiPlanApproval),
        EngineOptions::default(),
    )?;
    input.as_object_mut().unwrap().remove("deadline");
    let mut request = relative_swap_request(input);
    request.plan.steps[0].permitted_variations["allowed_fee_tiers"] = json!([]);
    request.plan.steps[0].permitted_variations["minimum_rate"]["output_atoms"] =
        json!("2970000000");
    assert_eq!(
        engine
            .authorize_financial_plan(request)
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    Ok(())
}

struct ManualClock(std::sync::Mutex<chrono::DateTime<chrono::Utc>>);
impl ManualClock {
    fn advance(&self, seconds: i64) {
        *self.0.lock().unwrap() += chrono::Duration::seconds(seconds);
    }
}
impl gofin_domain::clock::Clock for ManualClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }
}

fn relative_swap_request(
    input: Value,
) -> gofin_domain::financial_plans::FinancialPlanAuthorizationRequest {
    use gofin_domain::financial_plans::*;
    let resource = ResourceId(format!("ethereum:1:{:#x}", addr(1)));
    FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-relative-swap-authority".into(),
        valid_for_seconds: Some(2700),
        replaces_revision_id: None,
        expected_review_hash: None,
        plan: FinancialPlanSpec {
            version: 1,
            name: "Synthetic future swap with long parent".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "swap".into(),
                operation: "uniswap.swap".into(),
                input,
                action_deadline_seconds: Some(600),
                permitted_variations: bounds(),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: None,
                    operation_succeeded: true,
                },
                funding_dependencies: vec![],
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: AssetId {
                            network: "ethereum:1".into(),
                            reference: format!("{:#x}", addr(100)),
                            decimals: 18,
                        },
                        atoms: Quantity::from_decimal("0.002", 18).unwrap(),
                    },
                }],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash { resource },
                    amount: Amount {
                        asset: AssetId {
                            network: "ethereum:1".into(),
                            reference: "native".into(),
                            decimals: 18,
                        },
                        atoms: Quantity::from_decimal("0.0002", 18).unwrap(),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
            },
        },
    }
}

#[tokio::test]
async fn standalone_swap_deadline_and_preview_expiry_remain_bound() -> Result<()> {
    use gofin_domain::clock::Clock;
    let clock = Arc::new(ManualClock(std::sync::Mutex::new(chrono::Utc::now())));
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let adapter = crate::tests::local_sushi_adapter(rpc.clone()).with_clock(clock.clone());
    let mut input = candidate();
    input["deadline"] = json!(clock.now().timestamp() as u64 + 600);
    let prepared = adapter.prepare("sushi.swap", &input).await?;
    let mut changed = prepared.clone();
    changed.input["deadline"] = json!(clock.now().timestamp() as u64 + 601);
    assert!(adapter.revalidate(&changed).await.is_err());
    clock.advance(300);
    assert_eq!(
        adapter.revalidate(&prepared).await.unwrap_err().code,
        "PREVIEW_EXPIRED"
    );
    assert!(
        !rpc.calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
    Ok(())
}
