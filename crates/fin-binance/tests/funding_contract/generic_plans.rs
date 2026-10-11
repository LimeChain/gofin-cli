//! Generic exact-input financial plans still use Binance's ordinary prepare,
//! submission and proof path. This fixture starts with no USDC on the main
//! account, then proves subaccount proceeds before allowing the withdrawal.
use super::*;
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanFundingDependency,
    FinancialPlanFundingIdentity, FinancialPlanSpec, FinancialPlanStepSpec,
    FinancialStepCompletion,
};
use fin_domain::{
    Amount, AssetId, CostBound, CostTreatment, OperationRequest, ResourceAmount, ResourceId,
};
use fin_engine::{Engine, EngineOptions};
use fin_store::Store;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const MAIN: &str = "binance:uid:1001:SPOT";
const CHILD: &str = "binance:uid:1002:SPOT";
const WALLET: &str = "ethereum:synthetic-dev-wallet";

fn usdc(network: &str) -> AssetId {
    AssetId {
        network: network.into(),
        reference: "USDC".into(),
        decimals: 36,
    }
}

fn wallet_usdc() -> AssetId {
    AssetId {
        network: "ethereum:1".into(),
        reference: "0x0000000000000000000000000000000000000001".into(),
        decimals: 6,
    }
}

fn resource_amount(resource: &str, network: &str, quantity: &str) -> ResourceAmount {
    ResourceAmount {
        resource: ResourceId(resource.into()),
        amount: Amount {
            asset: usdc(network),
            atoms: fin_domain::Quantity::from_decimal(quantity, 36).unwrap(),
        },
    }
}

fn exact_scope(
    maximum_debits: Vec<ResourceAmount>,
    maximum_costs: Vec<CostBound>,
    possible_credits: Vec<FinancialPlanFundingIdentity>,
) -> Value {
    json!({
        "mode":"exact",
        "maximum_debits":maximum_debits,
        "maximum_costs":maximum_costs,
        "possible_credits":possible_credits
    })
}

fn cost(resource: &str, network: &str, amount: &str) -> CostBound {
    CostBound {
        location: fin_domain::AccountingLocation::Cash {
            resource: ResourceId(resource.into()),
        },
        amount: Amount {
            asset: usdc(network),
            atoms: fin_domain::Quantity::from_decimal(amount, 36).unwrap(),
        },
        treatment: CostTreatment::IncludedInDebit,
    }
}

fn inputs() -> (Value, Value) {
    (
        json!({
            "requestId":"SYNTHETIC-plan-sub-to-main",
            "authority":"child-fixture", "from":"child-fixture", "to":"main-fixture",
            "coin":"USDC", "amount":"10", "fromWallet":"SPOT", "toWallet":"SPOT"
        }),
        json!({
            "requestId":"SYNTHETIC-plan-withdraw",
            "account":"main-fixture", "destination":"dev-wallet", "coin":"USDC",
            "amount":"9.5", "network":"ETH", "walletType":"SPOT",
            "maxFee":"0.5", "minReceiveAmount":"9.0"
        }),
    )
}

fn plan() -> FinancialPlanSpec {
    let (transfer, withdraw) = inputs();
    let transfer_credit = FinancialPlanFundingIdentity {
        resource: ResourceId(MAIN.into()),
        asset: usdc("binance"),
    };
    let wallet_credit = FinancialPlanFundingIdentity {
        resource: ResourceId(WALLET.into()),
        asset: wallet_usdc(),
    };
    FinancialPlanSpec {
        version: 1,
        name: "Synthetic USDC funding and dev-wallet withdrawal".into(),
        steps: vec![
            FinancialPlanStepSpec {
                step_id: "fund-main".into(),
                operation: "binance.transfer".into(),
                input: transfer,
                permitted_variations: exact_scope(
                    vec![resource_amount(CHILD, "binance", "10")],
                    vec![],
                    vec![transfer_credit.clone()],
                ),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: None,
                    operation_succeeded: true,
                },
                funding_dependencies: vec![],
            },
            FinancialPlanStepSpec {
                step_id: "withdraw-to-dev-wallet".into(),
                operation: "binance.withdraw".into(),
                input: withdraw,
                // Binance's API amount is the 9.5 USDC gross debit. The
                // explicitly capped 0.5 fee is included in it, so the
                // recipient must prove at least 9.0 USDC net.
                permitted_variations: exact_scope(
                    vec![resource_amount(MAIN, "binance", "9.5")],
                    vec![cost(MAIN, "binance", "0.5")],
                    vec![wallet_credit],
                ),
                completion: FinancialStepCompletion {
                    principal_spend: None,
                    minimum_net_receipt: Some(ResourceAmount {
                        resource: ResourceId(WALLET.into()),
                        amount: Amount {
                            asset: wallet_usdc(),
                            atoms: fin_domain::Quantity::from_decimal("9.0", 6).unwrap(),
                        },
                    }),
                    operation_succeeded: true,
                },
                funding_dependencies: vec![FinancialPlanFundingDependency {
                    funding_scope: None,
                    producing_step_id: "fund-main".into(),
                    resource: ResourceId(MAIN.into()),
                    asset: usdc("binance"),
                }],
            },
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![
                resource_amount(CHILD, "binance", "10"),
                resource_amount(MAIN, "binance", "9.5"),
            ],
            maximum_costs: vec![cost(MAIN, "binance", "0.5")],
        },
    }
}

fn request(plan: FinancialPlanSpec) -> FinancialPlanAuthorizationRequest {
    FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-plan-single-approval".into(),
        plan,
        valid_for_seconds: 600,
        replaces_revision_id: None,
        expected_review_hash: None,
    }
}

fn plan_with_withdrawal_quantity(quantity: &str, minimum_receipt: &str) -> FinancialPlanSpec {
    let mut plan = plan();
    let withdraw = &mut plan.steps[1];
    withdraw.input["amount"] = json!(quantity);
    withdraw.input["minReceiveAmount"] = json!(minimum_receipt);
    withdraw.permitted_variations = exact_scope(
        vec![resource_amount(MAIN, "binance", quantity)],
        vec![cost(MAIN, "binance", "0.5")],
        vec![FinancialPlanFundingIdentity {
            resource: ResourceId(WALLET.into()),
            asset: wallet_usdc(),
        }],
    );
    withdraw.completion.minimum_net_receipt = Some(ResourceAmount {
        resource: ResourceId(WALLET.into()),
        amount: Amount {
            asset: wallet_usdc(),
            atoms: fin_domain::Quantity::from_decimal(minimum_receipt, 6).unwrap(),
        },
    });
    plan.aggregate_bounds.maximum_debits[1] = resource_amount(MAIN, "binance", quantity);
    plan
}

fn retry_transfer_plan() -> FinancialPlanSpec {
    let mut step = plan().steps.remove(0);
    step.input = json!({
        "requestId":"SYNTHETIC-plan-transfer-template",
        "authority":"main-fixture", "from":"main-fixture", "to":"child-fixture",
        "coin":"USDC", "amount":"10", "fromWallet":"SPOT", "toWallet":"SPOT"
    });
    step.permitted_variations = exact_scope(
        vec![resource_amount(MAIN, "binance", "10")],
        vec![],
        vec![FinancialPlanFundingIdentity {
            resource: ResourceId(CHILD.into()),
            asset: usdc("binance"),
        }],
    );
    FinancialPlanSpec {
        version: 1,
        name: "Synthetic retry after confirmed Binance signature rejection".into(),
        steps: vec![step],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![resource_amount(MAIN, "binance", "10")],
            maximum_costs: vec![],
        },
    }
}

fn engine_with(
    transport: Arc<dyn fin_binance::BinanceTransport>,
    approvals: Arc<CountingApproval>,
) -> Engine {
    engine_with_store(Store::in_memory().unwrap(), transport, approvals)
}

fn engine_with_store(
    store: Store,
    transport: Arc<dyn fin_binance::BinanceTransport>,
    approvals: Arc<CountingApproval>,
) -> Engine {
    use std::collections::BTreeMap;
    let mut config = main_and_child_config();
    config.destinations.push(fin_binance::EthereumDestination {
        name: "dev-wallet".into(),
        resource: ResourceId(WALLET.into()),
        address: "0x1111111111111111111111111111111111111111".into(),
        chain_id: 1,
        confirmations: 2,
        assets: BTreeMap::from([("USDC".into(), wallet_usdc())]),
    });
    let adapter = Arc::new(
        fin_binance::BinanceAdapter::new(
            config,
            transport,
            Some(Arc::new(StablecoinPriceProvider)),
            Some(Arc::new(CompletedUsdcRecipient)),
        )
        .unwrap(),
    );
    Engine::new(store, vec![adapter], approvals, EngineOptions::default()).unwrap()
}

fn operation(request_id: &str, operation: &str, input: Value) -> OperationRequest {
    OperationRequest {
        request_id: request_id.into(),
        operation: operation.into(),
        input,
        run_id: None,
    }
}

struct PlanFundingTransport {
    child: Mutex<String>,
    main: Mutex<String>,
    transfer_posts: AtomicUsize,
    withdrawal_posts: AtomicUsize,
    transfer_time_ms: Mutex<Option<u64>>,
    withdrawal_time_ms: Mutex<Option<u64>>,
}

impl PlanFundingTransport {
    fn new() -> Self {
        Self {
            child: Mutex::new("10".into()),
            main: Mutex::new("0".into()),
            transfer_posts: AtomicUsize::new(0),
            withdrawal_posts: AtomicUsize::new(0),
            transfer_time_ms: Mutex::new(None),
            withdrawal_time_ms: Mutex::new(None),
        }
    }
    fn account(&self, child: bool) -> Value {
        json!({"accountType":"SPOT", "uid":if child {"1002"} else {"1001"}, "balances":[{
            "asset":"USDC", "free":if child { self.child.lock().unwrap().clone() } else { self.main.lock().unwrap().clone() }, "locked":"0"
        }]})
    }
    fn bounded_history_timestamp(request: &BinanceRequest, timestamp: Option<u64>) -> u64 {
        let timestamp = timestamp.expect("the synthetic venue accepted the original write");
        let end_time = request.get("endTime").unwrap().parse::<u64>().unwrap();
        let start_time = request.get("startTime").unwrap().parse::<u64>().unwrap();
        assert!(timestamp >= start_time);
        assert!(timestamp <= end_time);
        timestamp
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for PlanFundingTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        let is_child = credential.as_str().ends_with("child");
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({"enableReading":true,"enableWithdrawals":true,
                "enableSpotAndMarginTrading":true,
                "enableInternalTransfer":true,"permitsUniversalTransfer":true})),
            E::Account => Ok(self.account(is_child)),
            E::SubAccountList => Ok(
                json!({"subAccounts":[{"email":"child@example.invalid","isFreeze":false,"isMaster":false}]}),
            ),
            E::SubAccountSpotAssets => Ok(
                json!({"balances":[{"asset":"USDC","free":self.child.lock().unwrap().clone(),"locked":"0"}]}),
            ),
            E::SubAccountTransferHistory => {
                if request.get("limit") == Some("1") {
                    return Ok(json!([]));
                }
                Ok(
                    json!([{"counterParty":"master","email":"main@example.invalid","type":2,
                    "asset":"USDC","qty":"10","fromAccountType":"SPOT","toAccountType":"SPOT",
                    "status":"SUCCESS","tranId":123456789,"time":Self::bounded_history_timestamp(&request, *self.transfer_time_ms.lock().unwrap())}]),
                )
            }
            E::CapitalConfig => Ok(
                json!([{"coin":"USDC","withdrawAllEnable":true,"depositAllEnable":true,
                "networkList":[{"network":"ETH","coin":"USDC","contractAddress":"0x0000000000000000000000000000000000000001","depositEnable":true,"withdrawEnable":true,
                    "withdrawIntegerMultiple":"0.1","withdrawFee":"0.5","withdrawMin":"1","withdrawMax":"10000",
                    "withdrawTag":false,"busy":false}]}]),
            ),
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::SubToMasterTransfer => {
                self.transfer_posts.fetch_add(1, Ordering::SeqCst);
                *self.transfer_time_ms.lock().unwrap() =
                    Some(chrono::Utc::now().timestamp_millis().max(0) as u64 / 1_000 * 1_000);
                *self.child.lock().unwrap() = "0".into();
                *self.main.lock().unwrap() = "10".into();
                Ok(json!({"txnId":123456789}))
            }
            E::UniversalTransferHistory => {
                let timestamp = Self::bounded_history_timestamp(
                    &request,
                    *self.transfer_time_ms.lock().unwrap(),
                );
                Ok(json!({"totalCount":1,"result":[{
                    "tranId":123456789,"clientTranId":request.get("clientTranId"),"asset":"USDC","amount":"10",
                    "fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":"child@example.invalid",
                    "status":"SUCCESS","createTimeStamp":timestamp
                }]}))
            }
            E::StandardWithdrawal => {
                self.withdrawal_posts.fetch_add(1, Ordering::SeqCst);
                *self.withdrawal_time_ms.lock().unwrap() =
                    Some(chrono::Utc::now().timestamp_millis().max(0) as u64);
                assert_eq!(request.get("amount"), Some("9.5"));
                *self.main.lock().unwrap() = "0".into();
                Ok(json!({"id":"synthetic-withdrawal-1"}))
            }
            E::WithdrawalHistory => {
                let timestamp = Self::bounded_history_timestamp(
                    &request,
                    *self.withdrawal_time_ms.lock().unwrap(),
                );
                let complete_time = chrono::DateTime::from_timestamp_millis(timestamp as i64)
                    .unwrap()
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string();
                Ok(json!([{
                    "id":"synthetic-withdrawal-1","coin":"USDC","amount":"9.0","transactionFee":"0.5",
                    "network":"ETH","address":"0x1111111111111111111111111111111111111111",
                    "txId":format!("0x{}", "a".repeat(64)),"status":6,"completeTime":complete_time,
                    "withdrawOrderId":request.get("withdrawOrderId"),"transferType":0,"walletType":0
                }]))
            }
            endpoint => panic!("unexpected synthetic financial-plan endpoint {endpoint:?}"),
        }
    }
}

struct CompletedUsdcRecipient;

#[async_trait::async_trait]
impl fin_binance::BinanceRecipientVerifier for CompletedUsdcRecipient {
    async fn verify(&self, request: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        assert_eq!(
            request.recipient,
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(
            request.asset.reference,
            "0x0000000000000000000000000000000000000001"
        );
        fin_binance::PayoutResult {
            status: fin_domain::Status::Completed,
            amount_atoms: Some(fin_domain::Quantity::from_decimal("9.0", 6).unwrap()),
            evidence: json!({"fixture":"synthetic dev-wallet receipt","principal":"9.0 USDC net; 9.5 USDC gross"}),
        }
    }
}

struct StablecoinPriceProvider;

#[async_trait::async_trait]
impl fin_binance::BinancePriceProvider for StablecoinPriceProvider {
    async fn quote_usd(&self, _coin: &str) -> fin_domain::Result<fin_binance::PriceQuote> {
        Ok(fin_binance::PriceQuote {
            usd_per_unit: "1".into(),
            observed_at: fin_domain::now(),
            source: "synthetic-USDC-parity".into(),
        })
    }
}

struct CountingApproval(AtomicUsize);

#[async_trait::async_trait]
impl fin_domain::Authorizer for CountingApproval {
    async fn authorize(
        &self,
        challenge: &fin_domain::AuthorizationChallenge,
    ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic-plan-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct OfflinePlanTransport;

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for OfflinePlanTransport {
    async fn request(
        &self,
        _credential: &fin_binance::CredentialHandle,
        _request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        panic!("Exact plan input review must not call Binance or depend on future funds")
    }
}

#[tokio::test]
async fn malformed_decimal_in_a_later_step_blocks_the_entire_plan_before_approval() {
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(Arc::new(OfflinePlanTransport), approvals.clone());
    let invalid = [
        ".00415",
        "1e-8",
        "+1",
        "-1",
        "01.2",
        "1.",
        " 1",
        "1\n",
        "1.0000000000000000000000000000000000000",
    ];
    for (step_index, field) in [
        (0, "amount"),
        (1, "amount"),
        (1, "maxFee"),
        (1, "minReceiveAmount"),
    ] {
        for value in invalid {
            let mut candidate = plan();
            candidate.steps[step_index].input[field] = json!(value);
            let authorization = request(candidate);
            let preview_error = engine
                .preview_financial_plan(authorization.clone())
                .await
                .unwrap_err();
            assert_eq!(preview_error.code, "PLAN_INPUT_INVALID", "{field}={value}");
            let approval_error = engine
                .authorize_financial_plan(authorization)
                .await
                .unwrap_err();
            assert_eq!(approval_error.code, "PLAN_INPUT_INVALID", "{field}={value}");
        }
    }
    for field in ["amount", "minReceiveAmount"] {
        let mut candidate = plan();
        candidate.steps[1].input[field] = json!("0.000");
        assert_eq!(
            engine
                .preview_financial_plan(request(candidate))
                .await
                .unwrap_err()
                .code,
            "PLAN_INPUT_INVALID"
        );
    }
    let mut valid = plan();
    valid.steps[0].input["amount"] = json!("0.00415");
    valid.steps[1].input["maxFee"] = json!("0");
    valid.steps[1].input["minReceiveAmount"] = json!("0.000000000000000000000000000000000001");
    engine.preview_financial_plan(request(valid)).await.unwrap();
    assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn order_decimal_schema_keeps_null_zero_bounds_and_all_while_rejecting_malformed_inputs() {
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(Arc::new(OfflinePlanTransport), approvals.clone());
    let mut candidate = plan();
    let order = &mut candidate.steps[1];
    order.operation = "binance.spot.order-place".into();
    order.input = json!({
        "request_id":"SYNTHETIC-offline-order", "account":"main-fixture", "symbol":"ETHUSDC",
        "side":"BUY", "quantity":"0.0042", "limit_price":"2500", "time_in_force":"FOK",
        "maximum_fees":{"ETH":"0.00002", "BNB":"0.00005"},
        "maximum_quantity":null, "maximum_loss":null, "maximum_profit":null,
        "maximum_funding_loss":null, "maximum_funding_profit":null
    });
    engine
        .preview_financial_plan(request(candidate.clone()))
        .await
        .unwrap();
    let mut omitted_optional_fields = candidate.clone();
    for field in [
        "maximum_quantity",
        "maximum_loss",
        "maximum_profit",
        "maximum_funding_loss",
        "maximum_funding_profit",
    ] {
        omitted_optional_fields.steps[1]
            .input
            .as_object_mut()
            .unwrap()
            .remove(field);
    }
    engine
        .preview_financial_plan(request(omitted_optional_fields))
        .await
        .unwrap();
    for field in [
        "quantity",
        "limit_price",
        "maximum_quantity",
        "maximum_loss",
        "maximum_profit",
        "maximum_funding_loss",
        "maximum_funding_profit",
    ] {
        for value in [".00415", "1e-8", "01.2", "-1"] {
            let mut malformed = candidate.clone();
            malformed.steps[1].input[field] = json!(value);
            let authorization = request(malformed);
            assert_eq!(
                engine
                    .preview_financial_plan(authorization.clone())
                    .await
                    .unwrap_err()
                    .code,
                "PLAN_INPUT_INVALID"
            );
            assert_eq!(
                engine
                    .authorize_financial_plan(authorization)
                    .await
                    .unwrap_err()
                    .code,
                "PLAN_INPUT_INVALID"
            );
        }
    }
    for value in [".00002", "0", "0.0", "1e-8"] {
        let mut malformed = candidate.clone();
        malformed.steps[1].input["maximum_fees"]["ETH"] = json!(value);
        assert_eq!(
            engine
                .preview_financial_plan(request(malformed))
                .await
                .unwrap_err()
                .code,
            "PLAN_INPUT_INVALID"
        );
    }
    let order = &mut candidate.steps[1].input;
    order["quantity"] = json!("all");
    order["maximum_quantity"] = json!("0.0042");
    for field in [
        "maximum_loss",
        "maximum_profit",
        "maximum_funding_loss",
        "maximum_funding_profit",
    ] {
        order[field] = json!("0");
    }
    // Syntax review preserves documented sentinels and zero limits. Runtime
    // preparation still enforces scoped closing eligibility and market checks.
    engine
        .preview_financial_plan(request(candidate))
        .await
        .unwrap();
    assert_eq!(approvals.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_named_plan_uses_verified_subaccount_proceeds_before_dev_wallet_withdrawal() {
    let transport = Arc::new(PlanFundingTransport::new());
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(transport.clone(), approvals.clone());
    let original_plan = plan();
    let mut authorize = request(original_plan.clone());
    let review = engine
        .preview_financial_plan(authorize.clone())
        .await
        .unwrap();
    authorize.expected_review_hash = Some(review.review_hash);
    let approved = engine.authorize_financial_plan(authorize).await.unwrap();
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        *transport.main.lock().unwrap(),
        "0",
        "future funding starts at zero"
    );

    let (_, withdraw_input) = inputs();
    let early = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw-to-dev-wallet",
            operation(
                "SYNTHETIC-premature-withdraw-attempt",
                "binance.withdraw",
                withdraw_input.clone(),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(early.code, "PLAN_STEP_NOT_ELIGIBLE");
    assert_eq!(transport.transfer_posts.load(Ordering::SeqCst), 0);
    assert_eq!(transport.withdrawal_posts.load(Ordering::SeqCst), 0);

    let (transfer_input, _) = inputs();
    let transfer = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-transfer-attempt",
                "binance.transfer",
                transfer_input,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        transfer.status,
        fin_domain::Status::Completed,
        "{transfer:?}"
    );
    assert_eq!(*transport.child.lock().unwrap(), "0");
    assert_eq!(*transport.main.lock().unwrap(), "10");
    assert_eq!(transport.transfer_posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);

    let withdrawal = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw-to-dev-wallet",
            operation(
                "SYNTHETIC-withdraw-attempt",
                "binance.withdraw",
                withdraw_input,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        withdrawal.status,
        fin_domain::Status::Completed,
        "{withdrawal:?}"
    );
    assert_eq!(*transport.main.lock().unwrap(), "0");
    assert_eq!(transport.withdrawal_posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);

    let status = engine
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(status.next_eligible_step_id, None);
    assert_eq!(status.steps.len(), 2);
    assert!(
        status.steps.iter().all(|step| step.state
            == fin_domain::financial_plans::FinancialPlanStepProgressState::Completed)
    );
    assert!(
        status
            .usage
            .verified_proceeds
            .iter()
            .any(|proceeds| proceeds.resource.0 == WALLET
                && proceeds.asset == wallet_usdc()
                && proceeds.available.to_decimal(6) == "9")
    );
}

#[tokio::test]
async fn exact_plan_input_tampering_cannot_change_withdrawal_destination_amount_or_network() {
    let transport = Arc::new(PlanFundingTransport::new());
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(transport.clone(), approvals.clone());
    let authorize = request(plan());
    let review = engine
        .preview_financial_plan(authorize.clone())
        .await
        .unwrap();
    let mut authorize = authorize;
    authorize.expected_review_hash = Some(review.review_hash);
    let approved = engine.authorize_financial_plan(authorize).await.unwrap();

    let (transfer_input, withdraw_input) = inputs();
    engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-tamper-transfer",
                "binance.transfer",
                transfer_input,
            ),
        )
        .await
        .unwrap();
    assert_eq!(*transport.main.lock().unwrap(), "10");

    for (attempt, field, changed) in [
        ("destination", "destination", "other-wallet"),
        ("amount", "amount", "9.4"),
        ("network", "network", "ARBITRUM"),
    ] {
        let mut tampered = withdraw_input.clone();
        tampered[field] = json!(changed);
        let error = engine
            .execute_financial_plan_step(
                &approved.plan_id,
                "withdraw-to-dev-wallet",
                operation(
                    &format!("SYNTHETIC-tampered-{attempt}"),
                    "binance.withdraw",
                    tampered,
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "PLAN_STEP_SCOPE_MISMATCH", "{field}");
        assert_eq!(*transport.main.lock().unwrap(), "10");
        assert_eq!(transport.withdrawal_posts.load(Ordering::SeqCst), 0);
    }
    assert_eq!(transport.transfer_posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unrelated_balance_cannot_make_up_insufficient_predecessor_proceeds() {
    let transport = Arc::new(PlanFundingTransport::new());
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(transport.clone(), approvals.clone());
    let mut authorize = request(plan_with_withdrawal_quantity("10.5", "10.0"));
    let review = engine
        .preview_financial_plan(authorize.clone())
        .await
        .unwrap();
    authorize.expected_review_hash = Some(review.review_hash);
    let approved = engine.authorize_financial_plan(authorize).await.unwrap();

    let (transfer_input, withdraw_input) = inputs();
    engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-unrelated-transfer",
                "binance.transfer",
                transfer_input,
            ),
        )
        .await
        .unwrap();
    // Simulate an unrelated external deposit after the predecessor settled.
    // Current venue balance now covers the 10.5 gross debit, but the
    // declared producer supplied only 10 USDC.
    *transport.main.lock().unwrap() = "11".into();
    let mut candidate = withdraw_input;
    candidate["amount"] = json!("10.5");
    candidate["minReceiveAmount"] = json!("10.0");
    let error = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "withdraw-to-dev-wallet",
            operation(
                "SYNTHETIC-unrelated-withdrawal",
                "binance.withdraw",
                candidate,
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "PLAN_FUNDING_NOT_SETTLED");
    assert_eq!(*transport.main.lock().unwrap(), "11");
    assert_eq!(transport.withdrawal_posts.load(Ordering::SeqCst), 0);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
}

struct RetryTransferTransport {
    main: Mutex<String>,
    child: Mutex<String>,
    client_ids: Mutex<Vec<String>>,
    history_client_ids: Mutex<Vec<String>>,
    accepted_transfer_at_ms: Mutex<Option<u64>>,
    posts: AtomicUsize,
    lose_first_response: bool,
}

impl RetryTransferTransport {
    fn new() -> Self {
        Self::with_lost_response(false)
    }

    fn with_lost_response(lose_first_response: bool) -> Self {
        Self {
            main: Mutex::new("10".into()),
            child: Mutex::new("0".into()),
            client_ids: Mutex::new(Vec::new()),
            history_client_ids: Mutex::new(Vec::new()),
            accepted_transfer_at_ms: Mutex::new(None),
            posts: AtomicUsize::new(0),
            lose_first_response,
        }
    }

    fn account(&self, child: bool) -> Value {
        json!({"accountType":"SPOT", "uid":if child {"1002"} else {"1001"}, "balances":[{
            "asset":"USDC", "free":if child { self.child.lock().unwrap().clone() } else { self.main.lock().unwrap().clone() }, "locked":"0"
        }]})
    }
}

#[async_trait::async_trait]
impl fin_binance::BinanceTransport for RetryTransferTransport {
    async fn request(
        &self,
        credential: &fin_binance::CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, fin_binance::TransportError> {
        use fin_binance::BinanceEndpoint as E;
        let is_child = credential.as_str().ends_with("child");
        match request.endpoint {
            E::ApiRestrictions => Ok(json!({"enableReading":true,"enableWithdrawals":true,
                "enableSpotAndMarginTrading":true,
                "enableInternalTransfer":true,"permitsUniversalTransfer":true})),
            E::Account => Ok(self.account(is_child)),
            E::SubAccountList => Ok(
                json!({"subAccounts":[{"email":"child@example.invalid","isFreeze":false,"isMaster":false}]}),
            ),
            E::SubAccountSpotAssets => Ok(
                json!({"balances":[{"asset":"USDC","free":self.child.lock().unwrap().clone(),"locked":"0"}]}),
            ),
            E::SubAccountTransferHistory => Ok(json!([])),
            E::CapitalConfig => Ok(json!([{
                "coin":"USDC","withdrawAllEnable":true,"depositAllEnable":true,
                "networkList":[{"network":"ETH","coin":"USDC","contractAddress":"0x0000000000000000000000000000000000000001","depositEnable":true,"withdrawEnable":true,
                    "withdrawIntegerMultiple":"0.1","withdrawFee":"0.5","withdrawMin":"1","withdrawMax":"10000",
                    "withdrawTag":false,"busy":false}]
            }])),
            E::UniversalTransfer => {
                let attempt = self.posts.fetch_add(1, Ordering::SeqCst);
                let client_id = request.get("clientTranId").unwrap().to_owned();
                self.client_ids.lock().unwrap().push(client_id.clone());
                if attempt == 0 {
                    if self.lose_first_response {
                        *self.accepted_transfer_at_ms.lock().unwrap() = Some(
                            chrono::Utc::now().timestamp_millis().max(0) as u64 / 1_000 * 1_000,
                        );
                        *self.main.lock().unwrap() = "0".into();
                        *self.child.lock().unwrap() = "10".into();
                        return Err(fin_binance::TransportError::Timeout);
                    }
                    return Err(fin_binance::TransportError::HttpFailure {
                        status: 400,
                        binance_code: Some(-1022),
                        kind: fin_binance::HttpFailureKind::Rejected,
                    });
                }
                *self.accepted_transfer_at_ms.lock().unwrap() =
                    Some(chrono::Utc::now().timestamp_millis().max(0) as u64 / 1_000 * 1_000);
                *self.main.lock().unwrap() = "0".into();
                *self.child.lock().unwrap() = "10".into();
                Ok(json!({"tranId":987654322,"clientTranId":client_id}))
            }
            E::UniversalTransferHistory => {
                let client_id = request.get("clientTranId").unwrap().to_owned();
                self.history_client_ids
                    .lock()
                    .unwrap()
                    .push(client_id.clone());
                if self.posts.load(Ordering::SeqCst) < if self.lose_first_response { 1 } else { 2 }
                {
                    return Ok(json!({"totalCount":0,"result":[]}));
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let wall_time = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let end_time = request.get("endTime").unwrap().parse::<u64>().unwrap();
                let start_time = request.get("startTime").unwrap().parse::<u64>().unwrap();
                assert!(
                    wall_time > end_time,
                    "fixture delayed history past query endTime"
                );
                let timestamp = self
                    .accepted_transfer_at_ms
                    .lock()
                    .unwrap()
                    .expect("history row must use the timestamp of the accepted transfer");
                assert!(timestamp >= start_time);
                assert!(timestamp <= end_time);
                Ok(json!({"totalCount":1,"result":[{
                    "tranId":987654322,"clientTranId":client_id,"asset":"USDC","amount":"10",
                    "fromAccountType":"SPOT","toAccountType":"SPOT",
                    "fromEmail":null,"toEmail":"child@example.invalid",
                    "status":"SUCCESS","createTimeStamp":timestamp
                }]}))
            }
            endpoint => panic!("unexpected synthetic retry endpoint {endpoint:?}"),
        }
    }
}

#[tokio::test]
async fn lost_response_restart_reconciles_the_generated_transfer_identity_without_replay() {
    let transport = Arc::new(RetryTransferTransport::with_lost_response(true));
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("plans.sqlite");
    let authorize = request(retry_transfer_plan());

    let first_engine = engine_with_store(
        Store::open(&database).unwrap(),
        transport.clone(),
        approvals.clone(),
    );
    let review = first_engine
        .preview_financial_plan(authorize.clone())
        .await
        .unwrap();
    let mut authorize = authorize;
    authorize.expected_review_hash = Some(review.review_hash);
    let approved = first_engine
        .authorize_financial_plan(authorize)
        .await
        .unwrap();
    let first = first_engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-transfer-lost-response",
                "binance.transfer",
                retry_transfer_plan().steps[0].input.clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(first.status, fin_domain::Status::Unknown);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    first_engine
        .revoke_financial_plan(&approved.plan_id)
        .await
        .unwrap();
    drop(first_engine);

    let expected_generated_id = first.preview.as_ref().unwrap().binding["clientTranId"]
        .as_str()
        .unwrap()
        .to_owned();
    let template_id = fin_binance::ClientIdentity::transfer(
        retry_transfer_plan().steps[0].input["requestId"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_ne!(expected_generated_id, template_id.as_str());
    assert_eq!(
        transport.client_ids.lock().unwrap().as_slice(),
        std::slice::from_ref(&expected_generated_id)
    );

    let restarted = engine_with_store(
        Store::open(&database).unwrap(),
        transport.clone(),
        approvals.clone(),
    );
    let recovered = restarted
        .financial_plan_status(&approved.plan_id)
        .await
        .unwrap();
    assert_eq!(recovered.steps[0].attempts.len(), 1);
    assert_eq!(
        recovered.steps[0].attempts[0].status,
        fin_domain::Status::Completed
    );
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        transport.history_client_ids.lock().unwrap().as_slice(),
        &[expected_generated_id],
        "reconciliation queries the original generated identity after restart"
    );
    assert_eq!(transport.main.lock().unwrap().as_str(), "0");
    assert_eq!(transport.child.lock().unwrap().as_str(), "10");
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);

    let duplicate = restarted
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-transfer-lost-response",
                "binance.transfer",
                retry_transfer_plan().steps[0].input.clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.id, first.id);
    assert_eq!(duplicate.status, fin_domain::Status::Completed);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn confirmed_signature_rejection_allows_a_new_plan_attempt_with_a_distinct_binance_identity()
{
    let transport = Arc::new(RetryTransferTransport::new());
    let approvals = Arc::new(CountingApproval(AtomicUsize::new(0)));
    let engine = engine_with(transport.clone(), approvals.clone());
    let authorize = request(retry_transfer_plan());
    let review = engine
        .preview_financial_plan(authorize.clone())
        .await
        .unwrap();
    let mut authorize = authorize;
    authorize.expected_review_hash = Some(review.review_hash);
    let approved = engine.authorize_financial_plan(authorize).await.unwrap();
    let input = retry_transfer_plan().steps[0].input.clone();

    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation(
                "SYNTHETIC-transfer-rejected",
                "binance.transfer",
                input.clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(first.status, fin_domain::Status::Unknown);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    let first_failed = engine.status(&first.id, true).await.unwrap();
    assert_eq!(first_failed.status, fin_domain::Status::Failed);
    assert!(first_failed.effects.as_ref().unwrap().effects.is_empty());
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(*transport.main.lock().unwrap(), "10");
    assert_eq!(*transport.child.lock().unwrap(), "0");

    let second = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "fund-main",
            operation("SYNTHETIC-transfer-retry", "binance.transfer", input),
        )
        .await
        .unwrap();
    assert_eq!(second.status, fin_domain::Status::Completed, "{second:?}");
    let client_ids = transport.client_ids.lock().unwrap();
    assert_eq!(client_ids.len(), 2);
    assert_ne!(client_ids[0], client_ids[1]);
    assert_eq!(second.submission.as_ref().unwrap().reference, client_ids[1]);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 2);
    assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
    assert_eq!(*transport.main.lock().unwrap(), "0");
    assert_eq!(*transport.child.lock().unwrap(), "10");
}
