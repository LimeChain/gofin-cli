use async_trait::async_trait;
use gofin_domain::{
    AccountingLocation, Amount, ApprovalAudit, AssetPrice, AuthorizationChallenge, Authorizer,
    CostBound, CostTreatment, GoFinError, Quantity, ResourceAmount, SubmissionJournal,
    financial_plans::{
        FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanExactMode,
        FinancialPlanExactScope, FinancialPlanSpec, FinancialPlanStepSpec, FinancialStepCompletion,
    },
};
use gofin_engine::{Engine, EngineOptions};
use gofin_exchange::{
    ExchangeAdapter, ExchangeProfile,
    ccxt::{HyperliquidActionEnvelope, HyperliquidInfoRequest},
};
use gofin_hyperliquid::{
    HyperliquidAccount, HyperliquidNonces, HyperliquidPrices, HyperliquidProfile,
    HyperliquidTransport, usdc,
};
use gofin_store::Store;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ModeOnlyVenue(&'static str);

#[async_trait]
impl HyperliquidTransport for ModeOnlyVenue {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, GoFinError> {
        assert_ne!(
            self.0, "must-not-read",
            "Invalid local constraints must fail before mode reads"
        );
        match request {
            HyperliquidInfoRequest::UserRole { .. } => Ok(json!({"role":"user"})),
            HyperliquidInfoRequest::UserAbstractionState { .. } => Ok(json!(self.0)),
            _ => panic!(
                "Plan accounting admission must not read balances, positions or prices: {request:?}"
            ),
        }
    }

    async fn dispatch(
        &self,
        _: &HyperliquidAccount,
        _: &HyperliquidActionEnvelope,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        panic!("Plan review must not dispatch")
    }
}

struct NoExecution;

#[async_trait]
impl HyperliquidPrices for NoExecution {
    async fn usdc_price(&self) -> Result<AssetPrice, GoFinError> {
        panic!("Plan accounting admission must not require a live price")
    }
}

impl HyperliquidNonces for NoExecution {
    fn allocate(&self, _: &str, _: u64) -> Result<u64, GoFinError> {
        panic!("Plan review must not allocate a submission nonce")
    }
}

#[derive(Default)]
struct CountingApproval(AtomicUsize);

#[async_trait]
impl Authorizer for CountingApproval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit, GoFinError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn account() -> HyperliquidAccount {
    HyperliquidAccount {
        name: "main".into(),
        owner: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        credential_ref: Some("synthetic-ref".into()),
        signing_enabled: true,
        execution_scope: Default::default(),
    }
}

fn profile(mode: &'static str) -> HyperliquidProfile {
    HyperliquidProfile::new(
        vec![account()],
        Arc::new(ModeOnlyVenue(mode)),
        Arc::new(NoExecution),
        Arc::new(NoExecution),
    )
    .unwrap()
}

fn amount(value: &str) -> Amount {
    Amount {
        asset: usdc(),
        atoms: Quantity::from_decimal(value, 36).unwrap(),
    }
}

fn scope(wallet: &str, loss: &str, fee: &str) -> FinancialPlanExactScope {
    let resource = account().resource(wallet, None);
    FinancialPlanExactScope {
        mode: FinancialPlanExactMode::Exact,
        maximum_debits: vec![ResourceAmount {
            resource: resource.clone(),
            amount: amount(loss),
        }],
        maximum_costs: vec![
            CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: amount(fee),
                treatment: CostTreatment::AdditionalDebit,
            },
            CostBound {
                location: AccountingLocation::Cash { resource },
                amount: amount(loss),
                treatment: CostTreatment::IncludedInDebit,
            },
        ],
        possible_credits: vec![],
    }
}

fn close_input() -> Value {
    json!({"request_id":"close-sol", "account":"main", "symbol":"SOL/USDC:USDC",
        "market_type":"perpetual", "side":"sell", "order_type":"limit", "size":"0.95",
        "limit_price":"100", "time_in_force":"ioc",
        "max_notional_usdc":"110", "max_fee_usdc":"1", "max_realized_loss_usdc":"14",
        "reduce_only":true})
}

fn request(scope: &FinancialPlanExactScope) -> FinancialPlanAuthorizationRequest {
    FinancialPlanAuthorizationRequest {
        request_id: "plan-review".into(),
        plan: FinancialPlanSpec {
            version: 1,
            name: "Close exact perpetual position".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "close-sol".into(),
                operation: "hyperliquid.orders.place".into(),
                input: close_input(),
                permitted_variations: serde_json::to_value(scope).unwrap(),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: None,
                    operation_succeeded: true,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: scope.maximum_debits.clone(),
                maximum_costs: scope.maximum_costs.clone(),
            },
        },
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
    }
}

#[tokio::test]
async fn retired_plan_review_and_authorization_never_prompt() {
    let approval = Arc::new(CountingApproval::default());
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile("disabled")))],
        approval.clone(),
        EngineOptions::default(),
    )
    .unwrap();
    let legacy = request(&scope("perpetual", "14", "1"));
    assert_eq!(
        engine
            .preview_financial_plan(legacy.clone())
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(
        engine
            .authorize_financial_plan(legacy)
            .await
            .unwrap_err()
            .code,
        "EXECUTION_INTERFACE_RETIRED"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exact_loss_scope_binds_mode_asset_amount_and_cost_treatment_without_balances() {
    for mode in ["disabled", "unifiedAccount"] {
        let profile = profile(mode);
        let wallet = if mode == "disabled" {
            "perpetual"
        } else {
            "unified"
        };
        let valid = scope(wallet, "14", "1");
        profile
            .validate_plan_feasibility("hyperliquid.orders.place", &close_input(), Some(&valid))
            .await
            .unwrap();
        let mut variants = vec![];
        let mut wrong = valid.clone();
        wrong.maximum_costs[1].treatment = CostTreatment::DeductedFromCredit;
        variants.push(wrong);
        let mut wrong = valid.clone();
        wrong.maximum_debits[0].amount = amount("13");
        variants.push(wrong);
        let mut wrong = valid.clone();
        wrong.maximum_costs[1].amount = amount("13");
        variants.push(wrong);
        let mut wrong = valid.clone();
        wrong.maximum_debits[0].amount.asset.network = "other-network".into();
        variants.push(wrong);
        let mut wrong = valid.clone();
        wrong.maximum_debits[0].resource = account().resource("spot", None);
        variants.push(wrong);
        for incomplete in variants {
            let error = profile
                .validate_plan_feasibility(
                    "hyperliquid.orders.place",
                    &close_input(),
                    Some(&incomplete),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "PLAN_EXACT_SCOPE_INCOMPLETE");
        }
    }
}

#[tokio::test]
async fn zero_loss_and_fee_need_no_positive_scope_and_batch_requires_summed_caps() {
    let profile = profile("disabled");
    let empty = FinancialPlanExactScope {
        mode: FinancialPlanExactMode::Exact,
        maximum_debits: vec![],
        maximum_costs: vec![],
        possible_credits: vec![],
    };
    let mut zero = close_input();
    zero["max_realized_loss_usdc"] = json!("0");
    zero["max_fee_usdc"] = json!("0");
    profile
        .validate_plan_feasibility("hyperliquid.orders.place", &zero, Some(&empty))
        .await
        .unwrap();
    let batch =
        json!({"request_id":"batch", "account":"main", "orders":[close_input(), close_input()]});
    let error = profile
        .validate_plan_feasibility(
            "hyperliquid.orders.batch",
            &batch,
            Some(&scope("perpetual", "14", "1")),
        )
        .await
        .unwrap_err();
    let details = error.details.unwrap();
    assert_eq!(details["validation_errors"].as_array().unwrap().len(), 3);
    assert_eq!(details["required_debits"][0]["amount"], json!(amount("28")));
    profile
        .validate_plan_feasibility(
            "hyperliquid.orders.batch",
            &batch,
            Some(&scope("perpetual", "28", "2")),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_edit_amounts_fail_before_mode_reads_and_approval() {
    let approval = Arc::new(CountingApproval::default());
    let profile = profile("must-not-read");
    let _engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile.clone()))],
        approval.clone(),
        EngineOptions::default(),
    )
    .unwrap();
    for (field, value) in [
        ("size", "0"),
        ("size", "-1"),
        ("size", "invalid"),
        ("limit_price", "0"),
        ("limit_price", "-1"),
        ("max_notional_usdc", "0"),
        ("max_notional_usdc", "-1"),
        ("size", "2"),
    ] {
        let mut edit = json!({"request_id":"edit-sol", "account":"main", "symbol":"SOL/USDC:USDC",
            "market_type":"perpetual", "order_id":"1", "size":"0.95", "limit_price":"100",
            "max_notional_usdc":"110", "max_fee_usdc":"1", "max_realized_loss_usdc":"14"});
        edit[field] = json!(value);
        for operation in ["hyperliquid.orders.edit", "hyperliquid.orders.edit_batch"] {
            let input = if operation.ends_with("edit_batch") {
                let mut child = edit.clone();
                child.as_object_mut().unwrap().remove("request_id");
                child.as_object_mut().unwrap().remove("account");
                json!({"request_id":"edit-sol-batch", "account":"main", "orders":[child]})
            } else {
                edit.clone()
            };
            assert!(profile.prepare_amend(operation, &input).await.is_err());
        }
    }
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
}
