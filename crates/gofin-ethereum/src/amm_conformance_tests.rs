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
    let config = EthereumConfig {
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

async fn expect_boundary_rejection(
    engine: &Engine,
    id: &str,
    operation: &str,
    input: &Value,
    expected_code: &str,
) {
    let error = engine
        .preview_boundary(id, operation, input)
        .await
        .expect_err(&format!("candidate unexpectedly passed: {input}"));
    assert_eq!(
        error.code, expected_code,
        "unexpected rejection for {input}: {error:?}"
    );
}

#[tokio::test]
async fn engine_boundary_authorizes_only_the_reviewed_swap_variations_and_revokes() -> Result<()> {
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let (engine, directory) = engine(&rpc);
    let mut initial = candidate();
    initial["request_id"] = json!("SYNTHETIC-conformance-grant-preview");
    let request = BoundaryAuthorizationRequest {
        request_id: "SYNTHETIC-conformance-boundary-grant".into(),
        operation: "uniswap.swap".into(),
        input: initial,
        bounds: bounds(),
        // Keep the authority valid longer than the adapter's 600-second
        // transaction deadline cap so this over-cap case reaches the adapter
        // and deterministically reports DEADLINE_INVALID.
        valid_for_seconds: 1_200,
        replaces: None,
    };
    let review = engine
        .preview_boundary_authorization(request.clone())
        .await
        .unwrap_or_else(|error| panic!("boundary review failed: {error:?}"));
    assert_eq!(review["operation"], "uniswap.swap");
    let authorization = engine
        .authorize_boundary(request)
        .await
        .unwrap_or_else(|error| panic!("boundary authorization failed: {error:?}"));
    let accepted = candidate();
    let preview = engine
        .preview_boundary(&authorization.id, "uniswap.swap", &accepted)
        .await
        .unwrap_or_else(|error| panic!("plan authorization failed: {error:?}"));
    assert_eq!(preview.prepared.operation, "uniswap.swap");

    let mut too_much = accepted.clone();
    too_much["request_id"] = json!("SYNTHETIC-amount-over-cap");
    too_much["amount"] = json!("0.003");
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &too_much,
        "BOUNDARY_VIOLATION",
    )
    .await;

    let mut excess_fee = accepted.clone();
    excess_fee["request_id"] = json!("SYNTHETIC-fee-over-cap");
    excess_fee["max_fee_eth"] = json!("0.0003");
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &excess_fee,
        "BOUNDARY_VIOLATION",
    )
    .await;

    let mut excess_slippage = accepted.clone();
    excess_slippage["request_id"] = json!("SYNTHETIC-slippage-over-cap");
    excess_slippage["slippage_bps"] = json!(101);
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &excess_slippage,
        "BOUNDARY_VIOLATION",
    )
    .await;

    let mut excess_impact = accepted.clone();
    excess_impact["request_id"] = json!("SYNTHETIC-impact-over-cap");
    excess_impact["max_price_impact_bps"] = json!(10001);
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &excess_impact,
        "INVALID_INPUT",
    )
    .await;

    let mut understated_impact = accepted.clone();
    understated_impact["request_id"] = json!("SYNTHETIC-actual-impact-over-cap");
    understated_impact["max_price_impact_bps"] = json!(100);
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &understated_impact,
        "PRICE_IMPACT_LIMIT",
    )
    .await;

    let mut weak_floor = accepted.clone();
    weak_floor["request_id"] = json!("SYNTHETIC-rate-floor-underflow");
    weak_floor["min_output"] = json!("2.97");
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &weak_floor,
        "BOUNDARY_VIOLATION",
    )
    .await;

    let mut disallowed_tier = accepted.clone();
    disallowed_tier["request_id"] = json!("SYNTHETIC-fee-tier-outside-scope");
    disallowed_tier["fee_tier"] = json!(3000);
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &disallowed_tier,
        "QUOTE_UNAVAILABLE",
    )
    .await;

    let mut wrong_recipient = accepted.clone();
    wrong_recipient["request_id"] = json!("SYNTHETIC-recipient-tamper");
    wrong_recipient["recipient"] = json!(format!("{:#x}", addr(2)));
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &wrong_recipient,
        "SWAP_RECIPIENT_UNSUPPORTED",
    )
    .await;

    let mut wrong_protocol = accepted.clone();
    wrong_protocol["request_id"] = json!("SYNTHETIC-protocol-tamper");
    wrong_protocol["protocol"] = json!("v4");
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &wrong_protocol,
        "INVALID_INPUT",
    )
    .await;

    let mut long_deadline = accepted.clone();
    long_deadline["request_id"] = json!("SYNTHETIC-deadline-over-cap");
    // Leave ample distance from the ten-minute boundary: crossing a wall-clock
    // second during preparation must not change which guard this test exercises.
    long_deadline["deadline"] = json!(chrono::Utc::now().timestamp() as u64 + 1200);
    expect_boundary_rejection(
        &engine,
        &authorization.id,
        "uniswap.swap",
        &long_deadline,
        "DEADLINE_INVALID",
    )
    .await;

    let database = directory.path().join("amm-conformance.sqlite");
    drop(engine);
    let engine = engine_at(&rpc, database);
    assert_eq!(
        engine
            .boundary_status(&authorization.id)
            .await?
            .authorization
            .status,
        gofin_domain::boundaries::BoundaryAuthorizationStatus::Active
    );
    engine.revoke_boundary(&authorization.id).await?;
    assert!(
        engine
            .preview_boundary(&authorization.id, "uniswap.swap", &accepted)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn engine_plan_authorizes_adapter_reviewed_swap_variations() -> Result<()> {
    use gofin_domain::financial_plans::{
        FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
        FinancialPlanStepSpec, FinancialStepCompletion,
    };

    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let (engine, directory) = engine(&rpc);
    let input = candidate();
    let resource = ResourceId(format!("ethereum:1:{:#x}", crate::tests::addr(1)));
    let token_in = AssetId {
        network: "ethereum:1".into(),
        reference: format!("{:#x}", crate::tests::addr(100)),
        decimals: 18,
    };
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "SYNTHETIC bounded AMM conformance".into(),
        summary: None,
        steps: vec![FinancialPlanStepSpec {
            action_deadline_seconds: None,
            step_id: "swap".into(),
            operation: "uniswap.swap".into(),
            input,
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
                    asset: token_in,
                    atoms: Quantity::from_decimal("0.002", 18)?,
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: native,
                    atoms: Quantity::from_decimal("0.0002", 18)?,
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
        },
    };
    let authorization = engine
        .authorize_financial_plan(FinancialPlanAuthorizationRequest {
            request_id: "SYNTHETIC-conformance-plan-authorization".into(),
            plan,
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        })
        .await?;
    assert_eq!(authorization.revision.spec.steps.len(), 1);
    let attempt = candidate();
    let prepared = engine
        .preview_financial_plan_step(&authorization.plan_id, "swap", "uniswap.swap", &attempt)
        .await
        .unwrap_or_else(|error| panic!("plan candidate preview failed: {error:?}"));
    assert_eq!(prepared.operation, "uniswap.swap");
    let mut changed_recipient = attempt.clone();
    changed_recipient["recipient"] = json!(format!("{:#x}", addr(2)));
    assert!(
        engine
            .preview_financial_plan_step(
                &authorization.plan_id,
                "swap",
                "uniswap.swap",
                &changed_recipient,
            )
            .await
            .is_err()
    );
    let database = directory.path().join("amm-conformance.sqlite");
    drop(engine);
    let engine = engine_at(&rpc, database);
    assert_eq!(
        engine
            .financial_plan_status(&authorization.plan_id)
            .await?
            .revision
            .status,
        gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Active
    );
    engine.revoke_financial_plan(&authorization.plan_id).await?;
    assert!(
        engine
            .preview_financial_plan_step(&authorization.plan_id, "swap", "uniswap.swap", &attempt,)
            .await
            .is_err()
    );
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
    let mut initial = candidate();
    initial["recipient"] = json!(owner);
    initial["request_id"] = json!("SYNTHETIC-uncertain-grant-preview");
    let grant = BoundaryAuthorizationRequest {
        request_id: "SYNTHETIC-uncertain-grant".into(),
        operation: "uniswap.swap".into(),
        input: initial,
        bounds: bounds(),
        valid_for_seconds: 600,
        replaces: None,
    };
    let authorization = engine.authorize_boundary(grant).await?;
    assert_eq!(
        approvals.load(Ordering::SeqCst),
        1,
        "one fresh grant approval"
    );
    let mut input = candidate();
    input["recipient"] = json!(owner);
    input["request_id"] = json!("SYNTHETIC-uncertain-write");
    let request = OperationRequest {
        request_id: "SYNTHETIC-uncertain-write".into(),
        operation: "uniswap.swap".into(),
        input: input.clone(),
        run_id: None,
    };
    let first = engine
        .execute_boundary(&authorization.id, request.clone())
        .await?;
    assert_eq!(
        first.status,
        gofin_domain::Status::Unknown,
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

    let duplicate = engine
        .execute_boundary(&authorization.id, request.clone())
        .await?;
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
    let after_restart = reopened
        .execute_boundary(&authorization.id, request)
        .await?;
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
async fn swap_usd_buffer_is_reserved_under_personal_limits_before_approval() -> Result<()> {
    for (movement, allocation, expected) in [
        ("3020000", "100000000", "MOVEMENT_LIMIT"),
        ("100000000", "3020000", "ALLOCATION_LIMIT"),
    ] {
        let rpc = Arc::new(Fixture::new());
        *rpc.allowance.lock().unwrap() = crate::exact("0.01", 18)?;
        let adapter = Arc::new(adapter(rpc.clone()));
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
                operation: "uniswap.swap".into(),
                input: candidate(),
                run_id: None,
            })
            .await?;
        assert_eq!(result.status, gofin_domain::Status::Failed);
        assert_eq!(result.error.unwrap().code, expected);
        assert_eq!(approvals.load(Ordering::SeqCst), 0);
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
async fn boundary_keeps_attempt_usd_ceiling_through_republication_and_rejects_excess_or_identity_changes()
-> Result<()> {
    use gofin_domain::Adapter;
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = crate::exact("0.01", 18)?;
    let prices = Arc::new(RepublishingPrices(AtomicU64::new(
        chrono::Utc::now().timestamp() as u64 - 2,
    )));
    let adapter = Arc::new(adapter(rpc.clone()).with_wallet_valuation(prices.clone()));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(SyntheticApproval),
        EngineOptions::default(),
    )?;
    let input = candidate();
    let grant = engine
        .authorize_boundary(BoundaryAuthorizationRequest {
            request_id: "SYNTHETIC-republication-grant".into(),
            operation: "uniswap.swap".into(),
            input: input.clone(),
            bounds: bounds(),
            valid_for_seconds: 600,
            replaces: None,
        })
        .await?;
    let preview = engine
        .preview_boundary(&grant.id, "uniswap.swap", &input)
        .await?
        .prepared;
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
    assert_ne!(
        fresh.valuation.as_ref().unwrap().source,
        preview.valuation.as_ref().unwrap().source
    );
    assert_eq!(
        adapter
            .submit(&preview, Arc::new(NoBroadcastJournal))
            .await
            .unwrap_err()
            .code,
        "PROTECTED_SIGNER_REQUIRED"
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
struct ClockApproval(Arc<ManualClock>);
#[async_trait::async_trait]
impl Authorizer for ClockApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        use gofin_domain::clock::Clock;
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-clock-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: self.0.now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
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
async fn long_plan_resolves_short_swap_after_eleven_minutes_and_rejects_insufficient_time_before_rpc()
-> Result<()> {
    use gofin_domain::clock::Clock;
    let clock = Arc::new(ManualClock(std::sync::Mutex::new(
        chrono::DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )));
    let rpc = Arc::new(Fixture::new());
    *rpc.allowance.lock().unwrap() = gofin_evm::u256(&Quantity::from_decimal("0.01", 18)?)?;
    let adapter = Arc::new(adapter(rpc.clone()).with_clock(clock.clone()));
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("timing.sqlite");
    let engine = Engine::new(
        Store::open(&database).unwrap(),
        vec![adapter.clone()],
        Arc::new(ClockApproval(clock.clone())),
        EngineOptions::default(),
    )?
    .with_clock(clock.clone());
    let mut input = candidate();
    input.as_object_mut().unwrap().remove("deadline");
    let request = relative_swap_request(input.clone());
    let review = engine.preview_financial_plan(request.clone()).await?;
    assert_eq!(review.valid_for_seconds, 2700);
    assert_eq!(
        review.permissions[0]
            .permission
            .as_ref()
            .unwrap()
            .authorization_seconds,
        None
    );
    assert!(review.plan.steps[0].input.get("deadline").is_none());
    let mut automatic = request.clone();
    automatic.valid_for_seconds = None;
    assert_eq!(
        engine
            .preview_financial_plan(automatic.clone())
            .await
            .unwrap_err()
            .code,
        "PLAN_DURATION_CANNOT_FIT_ACTION_WINDOW"
    );
    automatic.plan.steps[0].action_deadline_seconds = Some(300);
    assert_eq!(
        engine
            .preview_financial_plan(automatic)
            .await?
            .valid_for_seconds,
        600
    );
    let mut short = request.clone();
    short.valid_for_seconds = Some(300);
    short.plan.steps[0].action_deadline_seconds = Some(120);
    assert_eq!(
        engine.preview_financial_plan(short).await?.timing_warnings[0].code,
        "PLAN_DURATION_BELOW_BUDGET"
    );
    let authorized = engine.authorize_financial_plan(request).await?;
    clock.advance(661);
    let prepared = engine
        .preview_financial_plan_step(&authorized.plan_id, "swap", "uniswap.swap", &input)
        .await?;
    assert_eq!(
        prepared.input["deadline"],
        clock.now().timestamp() as u64 + 600
    );
    assert_eq!(
        prepared.action_deadline.as_ref().unwrap().resolved_at,
        clock.now().to_rfc3339()
    );
    let original_hash = gofin_domain::binding_hash(
        &gofin_domain::financial_plans::financial_plan_attempt_binding(
            &prepared,
            &authorized.plan_id,
            &authorized.revision.revision_id,
            "swap",
        ),
    )?;
    let mut rewritten = prepared.clone();
    rewritten.input["deadline"] = json!(clock.now().timestamp() as u64 + 601);
    assert_ne!(
        original_hash,
        gofin_domain::binding_hash(
            &gofin_domain::financial_plans::financial_plan_attempt_binding(
                &rewritten,
                &authorized.plan_id,
                &authorized.revision.revision_id,
                "swap"
            )
        )?
    );
    clock.advance(300);
    assert_eq!(
        adapter.revalidate(&prepared).await.unwrap_err().code,
        "PREVIEW_EXPIRED"
    );
    clock.advance(2700 - 661 - 300 - 599);
    let before = rpc.calls.lock().unwrap().len();
    let error = engine
        .preview_financial_plan_step(&authorized.plan_id, "swap", "uniswap.swap", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "PLAN_INSUFFICIENT_AUTHORIZATION_TIME");
    assert_eq!(error.details.as_ref().unwrap()["remaining_seconds"], 599);
    assert_eq!(
        error.details.as_ref().unwrap()["plan_id"],
        authorized.plan_id
    );
    assert_eq!(
        rpc.calls.lock().unwrap().len(),
        before,
        "insufficient time must not prepare through RPC"
    );
    let status = engine.financial_plan_status(&authorized.plan_id).await?;
    assert!(
        !status
            .authorization_timing
            .as_ref()
            .unwrap()
            .can_prepare_next_action
    );
    assert!(status.usage.reservations.is_empty());
    clock.advance(600);
    let restarted = Engine::new(
        Store::open(&database).unwrap(),
        vec![adapter],
        Arc::new(ClockApproval(clock.clone())),
        EngineOptions::default(),
    )?
    .with_clock(clock);
    assert_eq!(
        restarted
            .financial_plan_status(&authorized.plan_id)
            .await?
            .revision
            .status,
        gofin_domain::financial_plans::FinancialPlanAuthorizationStatus::Expired
    );
    assert_eq!(
        restarted
            .preview_financial_plan_step(&authorized.plan_id, "swap", "uniswap.swap", &input)
            .await
            .unwrap_err()
            .code,
        "PLAN_AUTHORIZATION_INACTIVE"
    );
    Ok(())
}
