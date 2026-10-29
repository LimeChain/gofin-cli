// Public CLI/broker/composite-deposit rehearsal. The shared native Ethereum fixture
// exercises signed dispatch and Kraken credit; Base/ERC20 routes have separate fixtures.
use super::*;
use crate as gofin_broker;
use gofin_domain::financial_plans::*;
use gofin_protocol::BrokerRequest;
include!("../tests/support/kraken_plan_client.rs");
mod ccxt_fixture {
    include!("kraken_deposit_ccxt_fixture.rs");
}

struct PlanApproval(AtomicUsize);
#[async_trait]
impl Authorizer for PlanApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.binding["plan"]["steps"][0]["operation"], "kraken.deposit");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "synthetic deposit plan only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn runtime(
    path: &std::path::Path,
    adapter: Arc<KrakenEvmDepositAdapter>,
    approval: Arc<PlanApproval>,
) -> Arc<crate::BrokerRuntime> {
    runtime_with_prices(path, adapter, approval, Arc::new(Prices), "100000000000000")
}
fn runtime_with_prices(
    path: &std::path::Path,
    adapter: Arc<KrakenEvmDepositAdapter>,
    approval: Arc<PlanApproval>,
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
    movement_limit: &str,
) -> Arc<crate::BrokerRuntime> {
    let mut policy = crate::BrokerPolicy::beta();
    policy.max_movement_usd_micros = movement_limit.into();
    policy.max_run_allocation_usd_micros = "100000000000000".into();
    crate::compose_runtime(
        crate::BrokerConfigV2 {
            version: 2,
            networks: crate::NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        crate::RuntimeDependencies {
            store: gofin_store::Store::open(path).unwrap(),
            adapters: vec![adapter],
            authorizer: approval,
            valuator: Arc::new(crate::SharedUsdValuator::with_source(prices)),
            bridge: None,
        },
    )
    .unwrap()
}
#[test]
fn named_deposit_cli_waits_for_usable_credit_and_replays_original_after_restart() {
    run_deposit(false, false, None);
}
#[test]
fn exact_usdc_deposit_uses_read_fallback_under_original_plan_without_replay() {
    run_deposit(true, false, None);
}
#[test]
fn exact_usdc_deposit_rejects_changed_fallback_address_before_dispatch() {
    run_deposit(true, true, None);
}
#[derive(Clone, Copy, PartialEq)]
enum PriceDrift {
    Tiny,
    TightPolicy,
}
#[test]
fn exact_usdc_deposit_tiny_price_drift_keeps_one_approval_and_one_dispatch() {
    run_deposit(true, false, Some(PriceDrift::Tiny));
}
#[test]
fn exact_usdc_deposit_price_buffer_does_not_override_current_policy() {
    run_deposit(true, false, Some(PriceDrift::TightPolicy));
}
fn run_deposit(usdc: bool, unsafe_fallback: bool, drift: Option<PriceDrift>) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(2 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse(usdc, unsafe_fallback, drift))
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn rehearse(usdc: bool, unsafe_fallback: bool, drift: Option<PriceDrift>) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("deposit-journal.sqlite");
    let (mut adapter, venue, signer) = signed_adapter(None);
    venue.usdc_evm.store(usdc, Ordering::SeqCst);
    venue.rpc.usdc_evm.store(usdc, Ordering::SeqCst);
    let ccxt = if usdc {
        Some(ccxt_fixture::attach(&mut adapter, venue.clone()).await)
    } else {
        None
    };
    let prices = drift.map(|_| Arc::new(valuation_tests::DepositPrices::new()));
    if let Some(prices) = &prices {
        adapter = valuation_tests::with_deposit_prices(adapter, prices.clone());
    }
    let price_source: Arc<dyn gofin_pricing::UsdPriceSource> = prices
        .as_ref()
        .map(|prices| prices.clone() as Arc<dyn gofin_pricing::UsdPriceSource>)
        .unwrap_or_else(|| Arc::new(Prices));
    let mut deposit_input = input();
    if usdc {
        deposit_input["amount"] = json!("100");
        deposit_input["address"] = json!("0x1111111111111111111111111111111111111111");
    }
    let adapter = Arc::new(adapter);
    let approval = Arc::new(PlanApproval(AtomicUsize::new(0)));
    let prepared = adapter
        .prepare("kraken.deposit", &deposit_input)
        .await
        .unwrap();
    let binding = decode_binding(&prepared.binding).unwrap();
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic named Kraken deposit".into(),
        summary: Some(
            if usdc {
                "Deposit 100 USDC and wait for exact usable Kraken credit."
            } else {
                "Deposit 0.01 ETH and wait for exact usable Kraken credit."
            }
            .into(),
        ),
        steps: vec![FinancialPlanStepSpec {
            step_id: "deposit".into(),
            operation: "kraken.deposit".into(),
            input: deposit_input.clone(),
            permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":[{"resource":binding.account_resource,"asset":binding.venue_asset}]}),
            funding_dependencies: vec![],
            action_deadline_seconds: None,
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: Some(ResourceAmount {
                    resource: binding.account_resource.clone(),
                    amount: Amount {
                        asset: binding.venue_asset.clone(),
                        atoms: binding.gross.clone(),
                    },
                }),
            },
        }],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: prepared.intent.debits,
            maximum_costs: prepared.intent.maximum_costs,
        },
    };
    let mut client = Client::new(
        runtime_with_prices(
            &path,
            adapter.clone(),
            approval.clone(),
            price_source.clone(),
            "100000000000000",
        ),
        temp.path(),
    )
    .await;
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-DEPOSIT-PLAN".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
            true,
        )
        .await;
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorization = client
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
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
            true,
        )
        .await;
    let plan_id = authorization["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    if let Some(prices) = &prices {
        assert_eq!(
            prepared.binding[gofin_domain::value_bounds::KEY]["movement_usd_micros"],
            "100980000"
        );
        prices.arm_after_next_wallet_quote(999_804);
    }
    if drift == Some(PriceDrift::TightPolicy) {
        // A previously approved plan cannot bypass a subsequently tighter policy.
        drop(client);
        client = Client::new(
            runtime_with_prices(
                &path,
                adapter.clone(),
                approval.clone(),
                price_source.clone(),
                "100000000",
            ),
            temp.path(),
        )
        .await;
    }
    if usdc {
        *venue.address_read_failure.lock().unwrap() = Some(
            error(
                "KRAKEN_UNAVAILABLE",
                "Synthetic v2 claimed-address server failure.",
            )
            .with_details(json!({
                "operation":E::KrakenFundingGetFundingV2DepositAddresses.operation(),
                "http_status":500,"transport_kind":"Rejected"
            })),
        );
        *venue.legacy_methods.lock().unwrap() = json!([{"method":"USDC - Ethereum (Unified)"}]);
        *venue.legacy_addresses.lock().unwrap() = json!([{"address":if unsafe_fallback {"0x9999999999999999999999999999999999999999"} else {"0x1111111111111111111111111111111111111111"},"expiretm":"0"}]);
        venue.requests.lock().unwrap().clear();
    }
    // A proven source transaction plus known Settled status does not imply usable credit.
    *venue.deposit_status_override.lock().unwrap() = json!({"status":"Settled"});
    let argv = args(&[
        "execute",
        "kraken.deposit",
        "--input",
        &deposit_input.to_string(),
        "--id",
        "SYNTHETIC-DEPOSIT-ATTEMPT",
        "--plan",
        &plan_id,
        "--step",
        "deposit",
        "--json",
    ]);
    let call = BrokerRequest::ExecutePlanStep {
        plan_id: plan_id.clone(),
        step_id: "deposit".into(),
        request: OperationRequest {
            request_id: "SYNTHETIC-DEPOSIT-ATTEMPT".into(),
            operation: "kraken.deposit".into(),
            input: deposit_input.clone(),
            run_id: None,
        },
    };
    if unsafe_fallback {
        let failure = client.call(argv, call, false).await;
        let failure = failure.get("error").unwrap_or(&failure);
        assert_eq!(
            failure["code"], "KRAKEN_DEPOSIT_ADDRESS_REQUIRED",
            "{failure}"
        );
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
        assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
        ccxt.as_ref().unwrap().assert_legacy_calls(&venue);
        return;
    }
    if drift == Some(PriceDrift::TightPolicy) {
        let record = client.call(argv, call, true).await;
        assert_eq!(record["status"], "failed");
        assert!(record["submission"].is_null());
        let failure = &record["error"];
        assert_eq!(failure["code"], "MOVEMENT_LIMIT", "{failure}");
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
        assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
        return;
    }
    let pending = client.call(argv.clone(), call.clone(), true).await;
    if let Some(prices) = &prices {
        assert_eq!(prices.usdc_micros.load(Ordering::SeqCst), 999_804);
        assert_eq!(prices.transitions.load(Ordering::SeqCst), 1);
        assert_eq!(
            pending["preview"]["binding"]["inner"]["binding"][gofin_domain::value_bounds::KEY]["movement_usd_micros"],
            "100980000"
        );
        assert_eq!(
            pending["preview"]["intent"]["debits"],
            serde_json::to_value(&plan.aggregate_bounds.maximum_debits).unwrap()
        );
        assert_eq!(
            pending["preview"]["intent"]["maximum_costs"],
            serde_json::to_value(&plan.aggregate_bounds.maximum_costs).unwrap()
        );
    }
    assert_eq!(pending["status"], "pending", "{pending}");
    assert!(pending["effects"].is_null());
    assert!(pending["error"].is_null());
    assert_eq!(
        pending["last_reconciliation_evidence"]["awaiting"],
        "venue_usable_credit"
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    if usdc {
        ccxt.as_ref().unwrap().assert_legacy_calls(&venue);
        assert_eq!(
            pending["preview"]["binding"]["discovery_api"],
            "funding-v1-legacy-address-read"
        );
        assert_eq!(
            pending["preview"]["binding"]["address"],
            deposit_input["address"]
        );
        assert_eq!(
            pending["preview"]["binding"]["method_id"],
            deposit_input["methodId"]
        );
        for field in ["fee", "limits"] {
            assert_eq!(
                pending["preview"]["binding"]["route"][field],
                prepared.binding["route"][field]
            );
        }
        let requests = venue.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|(endpoint, _)| *endpoint == E::KrakenFundingGetFundingV2DepositAddresses)
        );
        assert!(requests.iter().any(|(endpoint,params)| *endpoint == E::KrakenPrivatePostDepositAddresses && *params == json!({"asset":"USDC","aclass":"currency","method":"USDC - Ethereum (Unified)"})));
        assert!(
            requests
                .iter()
                .any(|(endpoint, _)| *endpoint == E::KrakenFundingGetFundingV1FeesMethodId)
        );
        assert!(
            requests.iter().any(|(endpoint, _)| *endpoint
                == E::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset)
        );
        assert!(
            requests
                .iter()
                .all(|(endpoint, params)| !endpoint.mutating() && params.get("new").is_none())
        );
    }
    let submission = pending["submission"].clone();
    assert!(submission["reference"].as_str().unwrap().starts_with("0x"));
    drop(client);
    let client = Client::new(
        runtime_with_prices(
            &path,
            adapter,
            approval.clone(),
            price_source,
            "100000000000000",
        ),
        temp.path(),
    )
    .await;
    let replay = client.call(argv.clone(), call.clone(), true).await;
    assert_eq!(replay["id"], pending["id"]);
    assert_eq!(replay["submission"], submission);
    assert_eq!(replay["status"], "pending");
    *venue.deposit_status_override.lock().unwrap() = json!({"status":"EarlyConfirmed"});
    let status = client
        .call(
            args(&["plans", "status", &plan_id, "--json"]),
            BrokerRequest::PlanStatus {
                plan_id: plan_id.clone(),
            },
            true,
        )
        .await;
    assert_eq!(status["steps"][0]["state"], "completed", "{status}");
    if drift.is_some() {
        assert_eq!(
            status["steps"][0]["attempts"].as_array().unwrap().len(),
            1,
            "Tiny drift must not create replacement candidates: {status}"
        );
    }
    assert!(
        status["usage"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let id = pending["id"].as_str().unwrap();
    let settled = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    let record: OperationRecord = serde_json::from_value(settled).unwrap();
    assert_eq!(record.status, Status::Completed);
    let effects = record.effects.unwrap().effects;
    assert!(effects.iter().any(|e|matches!(e,gofin_domain::FinancialEffect::Transfer{to,debit,credit,..}if to==&binding.account_resource&&credit.asset==binding.venue_asset&&credit.atoms==binding.gross&&debit.atoms.to_decimal(if usdc {6}else{18})==if usdc {"100"}else{"0.01"})));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,gofin_domain::FinancialEffect::Cost{category,amount,..}if category=="ethereum_gas"&&amount.atoms.to_decimal(18)=="0.000021"))
    );
    client
        .call(
            args(&["plans", "cancel", &plan_id, "--json"]),
            BrokerRequest::RevokePlan { plan_id },
            true,
        )
        .await;
    let replay = client.call(argv, call, true).await;
    assert_eq!(replay["submission"], submission);
    assert_eq!(replay["id"], pending["id"]);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        venue
            .rpc
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| method.as_str() == "eth_sendRawTransaction")
            .count(),
        1
    );
}

#[test]
fn kraken_solana_funding_named_plan_dispatches_once_and_recovers_after_restart() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_solana_named_plan(false))
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn kraken_solana_funding_usdc_named_plan_dispatches_once_and_recovers_after_restart() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_solana_named_plan(true))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse_solana_named_plan(usdc: bool) {
    use crate::test_support::cli::{Client as FundingClient, args as funding_args};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("kraken-solana-plan.sqlite");
    let (adapter, venue, fixture) = if usdc {
        super::submission_tests::signed_solana_usdc_adapter()
    } else {
        super::submission_tests::signed_solana_adapter()
    };
    let adapter = Arc::new(adapter);
    let approval = Arc::new(PlanApproval(AtomicUsize::new(0)));
    let mut input = json!({"requestId":if usdc{"synthetic-sol-usdc-plan-source"}else{"synthetic-sol-plan-source"},"account":"fixture-spot","wallet":"fixture-solana","methodId":"00000000-0000-0000-0000-000000000001","network":"solana","cluster":"mainnet-beta","amount":if usdc{"1"}else{"0.1"},"maxFeeSol":"0.00001"});
    if usdc {
        input["mint"] = json!(crate::test_support::solana::USDC_MINT);
        input["maxAccountFundingSol"] = json!("0.01");
    }
    let prepared = adapter.prepare("kraken.deposit", &input).await.unwrap();
    let binding = decode_binding(&prepared.binding).unwrap();
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic Kraken Solana funding".into(),
        summary: Some("Transfer SOL and wait for the exact usable Kraken credit.".into()),
        steps: vec![FinancialPlanStepSpec {
            step_id: "deposit".into(),
            operation: "kraken.deposit".into(),
            input: input.clone(),
            permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":[{"resource":binding.account_resource,"asset":binding.venue_asset}]}),
            funding_dependencies: vec![],
            action_deadline_seconds: None,
            completion: FinancialStepCompletion {
                operation_succeeded: true,
                principal_spend: None,
                minimum_net_receipt: Some(ResourceAmount {
                    resource: binding.account_resource.clone(),
                    amount: Amount {
                        asset: binding.venue_asset.clone(),
                        atoms: binding.gross.clone(),
                    },
                }),
            },
        }],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: prepared.intent.debits,
            maximum_costs: prepared.intent.maximum_costs,
        },
    };
    let client = FundingClient::new(
        runtime(&path, adapter.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let mut auth = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-KRAKEN-SOLANA-PLAN".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let plan_json = serde_json::to_string(&plan).unwrap();
    let preview = client
        .call(
            funding_args(&[
                "plans",
                "preview",
                "--input",
                &plan_json,
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: auth.clone(),
            },
            true,
        )
        .await;
    auth.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            funding_args(&[
                "plans",
                "authorize",
                "--input",
                &plan_json,
                "--id",
                &auth.request_id,
                "--review-hash",
                auth.expected_review_hash.as_ref().unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request: auth },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let operation = OperationRequest {
        request_id: "SYNTHETIC-KRAKEN-SOLANA-ATTEMPT".into(),
        operation: "kraken.deposit".into(),
        input: input.clone(),
        run_id: None,
    };
    let input_json = input.to_string();
    let argv = funding_args(&[
        "execute",
        "kraken.deposit",
        "--input",
        &input_json,
        "--id",
        &operation.request_id,
        "--plan",
        &plan_id,
        "--step",
        "deposit",
        "--json",
    ]);
    let request = BrokerRequest::ExecutePlanStep {
        plan_id: plan_id.clone(),
        step_id: "deposit".into(),
        request: operation.clone(),
    };
    let pending = client.call(argv.clone(), request.clone(), true).await;
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(
        pending["last_reconciliation_evidence"]["awaiting"],
        "venue_usable_credit"
    );
    let signature = fixture.last_signature().unwrap();
    assert_eq!(pending["submission"]["data"]["wallet_signature"], signature);
    assert_eq!(fixture.rpc().send_count(), 1);
    drop(client);
    let client = FundingClient::new(runtime(&path, adapter, approval.clone()), temp.path()).await;
    venue.credit.store(true, Ordering::SeqCst);
    let replay = client.call(argv, request, true).await;
    assert_eq!(replay["id"], pending["id"]);
    assert_eq!(replay["submission"], pending["submission"]);
    assert_eq!(replay["status"], "completed", "{replay}");
    assert_eq!(
        fixture.rpc().send_count(),
        1,
        "restart must not replay the source write"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        venue
            .solana_rpc
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .last_signature()
            .as_deref(),
        Some(signature.as_str())
    );
}

struct FutureSolanaPlanApproval;
#[async_trait]
impl Authorizer for FutureSolanaPlanApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(
            challenge.binding["plan"]["steps"][0]["operation"],
            "kraken.withdraw-funds"
        );
        assert_eq!(
            challenge.binding["plan"]["steps"][1]["operation"],
            "kraken.deposit"
        );
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "synthetic future funding approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[test]
fn kraken_solana_funding_future_usdc_named_plan_uses_prior_withdrawal_without_prefunding_principal()
{
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_future_funded_usdc())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse_future_funded_usdc() {
    use crate::{
        kraken_solana_funding_tests::{
            request as withdrawal_input, signed_solana_withdrawal_profile,
        },
        test_support::cli::{Client as FundingClient, args as funding_args},
    };
    use gofin_domain::{AccountingLocation, CostBound, CostTreatment, ResourceAmount};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("kraken-future-sol-usdc.sqlite");

    // The wallet begins with zero USDC principal. A small SOL balance covers
    // network fee and refundable recipient ATA rent, not the planned principal.
    let (withdraw_profile, withdrawal_venue, fixture) =
        signed_solana_withdrawal_profile("USDC", true);
    fixture.set_balance(10_000_000, 0);
    fixture.register_token_owner("11111111111111111111111111111111");
    let wallet_resource = ResourceId(format!("solana:mainnet-beta:{}", fixture.signer_address()));
    let usdc = AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: crate::test_support::solana::USDC_MINT.into(),
        decimals: 6,
    };
    let withdrawal_adapter = Arc::new(gofin_exchange::ExchangeAdapter::new(
        (*withdraw_profile).clone(),
    ));
    let withdrawal_json = withdrawal_input("USDC");
    let withdrawal_preview = withdrawal_adapter
        .prepare("kraken.withdraw-funds", &withdrawal_json)
        .await
        .unwrap();
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 0);
    let deposit_amount = Quantity::from_decimal("1", 6).unwrap();
    let source_native = AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let venue_usdc = AssetId {
        network: "kraken".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let deposit_debits = vec![
        ResourceAmount {
            resource: wallet_resource.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: deposit_amount.clone(),
            },
        },
        ResourceAmount {
            resource: wallet_resource.clone(),
            amount: Amount {
                asset: source_native.clone(),
                atoms: Quantity::from_u64(2_039_280),
            },
        },
    ];
    let deposit_costs = vec![
        CostBound {
            location: AccountingLocation::Cash {
                resource: wallet_resource.clone(),
            },
            amount: Amount {
                asset: source_native.clone(),
                atoms: Quantity::from_u64(10_000),
            },
            treatment: CostTreatment::AdditionalDebit,
        },
        CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("kraken:fixture-spot".into()),
            },
            amount: Amount {
                asset: venue_usdc.clone(),
                atoms: Quantity::zero(),
            },
            treatment: CostTreatment::DeductedFromCredit,
        },
    ];
    let producer_identity = FinancialPlanFundingIdentity {
        resource: wallet_resource.clone(),
        asset: usdc.clone(),
    };
    let producer_debits = withdrawal_preview.intent.debits.clone();
    let producer_costs = withdrawal_preview.intent.maximum_costs.clone();
    let producer_possible_credit = producer_identity.clone();
    let producer_receipt = ResourceAmount {
        resource: wallet_resource.clone(),
        amount: Amount {
            asset: usdc.clone(),
            atoms: deposit_amount.clone(),
        },
    };
    // The source adapter's venue and signer are composed against the same
    // zero-USDC wallet that the earlier Kraken payout funds.
    let deposit_mock = super::submission_tests::signed_solana_usdc_adapter();
    deposit_mock
        .1
        .solana_rpc
        .lock()
        .unwrap()
        .clone_from(&Some(fixture.rpc()));
    let deposit_profile = deposit_mock.0.kraken.clone();
    let deposit_adapter = Arc::new(KrakenEvmDepositAdapter::with_solana(
        deposit_profile,
        BTreeMap::new(),
        BTreeMap::from([("synthetic-solana-recipient".into(), fixture.wallet())]),
    ));
    let deposit_json = json!({"requestId":"future-funded-kraken-deposit","account":"fixture-spot","wallet":"synthetic-solana-recipient","methodId":"00000000-0000-0000-0000-000000000001","network":"solana","cluster":"mainnet-beta","mint":crate::test_support::solana::USDC_MINT,"amount":"1","maxFeeSol":"0.00001","maxAccountFundingSol":"0.01"});
    let withdrawal_scope = json!({"mode":"exact","maximum_debits":producer_debits,"maximum_costs":producer_costs,"possible_credits":[producer_possible_credit]});
    let deposit_scope = json!({"mode":"exact","maximum_debits":deposit_debits,"maximum_costs":deposit_costs,"possible_credits":[{"resource":"kraken:fixture-spot","asset":venue_usdc}]});
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Withdraw USDC then deposit from the funded Solana wallet".into(),
        summary: Some(
            "A Kraken USDC payout funds the later Kraken deposit; principal is not pre-funded."
                .into(),
        ),
        steps: vec![
            FinancialPlanStepSpec {
                step_id: "withdraw-usdc".into(),
                operation: "kraken.withdraw-funds".into(),
                input: withdrawal_json.clone(),
                permitted_variations: withdrawal_scope,
                funding_dependencies: vec![],
                action_deadline_seconds: None,
                completion: FinancialStepCompletion {
                    operation_succeeded: false,
                    principal_spend: None,
                    minimum_net_receipt: Some(producer_receipt),
                },
            },
            FinancialPlanStepSpec {
                step_id: "deposit-usdc".into(),
                operation: "kraken.deposit".into(),
                input: deposit_json.clone(),
                permitted_variations: deposit_scope,
                funding_dependencies: vec![FinancialPlanFundingDependency {
                    producing_step_id: "withdraw-usdc".into(),
                    resource: producer_identity.resource,
                    asset: producer_identity.asset,
                    funding_scope: Some(FinancialPlanFundingScope::PrincipalOnly),
                    maximum_fraction_bps: None,
                }],
                action_deadline_seconds: None,
                completion: FinancialStepCompletion {
                    operation_succeeded: false,
                    principal_spend: None,
                    minimum_net_receipt: Some(ResourceAmount {
                        resource: ResourceId("kraken:fixture-spot".into()),
                        amount: Amount {
                            asset: venue_usdc.clone(),
                            atoms: Quantity::from_decimal("1", 36).unwrap(),
                        },
                    }),
                },
            },
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: [withdrawal_preview.intent.debits, deposit_debits].concat(),
            maximum_costs: [producer_costs, deposit_costs].concat(),
        },
    };
    let mut config = crate::BrokerConfigV2 {
        version: 2,
        networks: crate::NetworkConfig::reviewed_evm_networks(),
        deployments: vec![],
        connections: vec![],
        policy: crate::BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() },
        extensions: BTreeMap::new(),
    };
    config.policy.max_movement_usd_micros = "100000000000000".into();
    config.policy.max_run_allocation_usd_micros = "100000000000000".into();
    let client = FundingClient::new(
        crate::compose_runtime(
            config,
            crate::RuntimeDependencies {
                store: gofin_store::Store::open(&path).unwrap(),
                adapters: vec![withdrawal_adapter, deposit_adapter],
                authorizer: Arc::new(FutureSolanaPlanApproval),
                valuator: Arc::new(crate::SharedUsdValuator::with_source(Arc::new(Prices))),
                bridge: None,
            },
        )
        .unwrap(),
        temp.path(),
    )
    .await;
    let plan_json = serde_json::to_string(&plan).unwrap();
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-FUTURE-USDC-PLAN".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            funding_args(&[
                "plans",
                "preview",
                "--input",
                &plan_json,
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            },
            true,
        )
        .await;
    authorization.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            funding_args(&[
                "plans",
                "authorize",
                "--input",
                &plan_json,
                "--id",
                &authorization.request_id,
                "--review-hash",
                authorization.expected_review_hash.as_ref().unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan {
                request: authorization,
            },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 0);
    let withdrawal_op = OperationRequest {
        request_id: "SYNTHETIC-KRAKEN-SOLANA-WITHDRAWAL".into(),
        operation: "kraken.withdraw-funds".into(),
        input: withdrawal_json,
        run_id: None,
    };
    let withdrawal = client
        .call(
            funding_args(&[
                "execute",
                "kraken.withdraw-funds",
                "--input",
                &serde_json::to_string(&withdrawal_op.input).unwrap(),
                "--id",
                &withdrawal_op.request_id,
                "--plan",
                &plan_id,
                "--step",
                "withdraw-usdc",
                "--json",
            ]),
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw-usdc".into(),
                request: withdrawal_op,
            },
            true,
        )
        .await;
    assert_eq!(withdrawal["status"], "completed", "{withdrawal}");
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 1_000_000);
    let deposit_op = OperationRequest {
        request_id: "SYNTHETIC-FUTURE-FUNDED-KRAKEN-DEPOSIT".into(),
        operation: "kraken.deposit".into(),
        input: deposit_json,
        run_id: None,
    };
    let deposit = client
        .call(
            funding_args(&[
                "execute",
                "kraken.deposit",
                "--input",
                &serde_json::to_string(&deposit_op.input).unwrap(),
                "--id",
                &deposit_op.request_id,
                "--plan",
                &plan_id,
                "--step",
                "deposit-usdc",
                "--json",
            ]),
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "deposit-usdc".into(),
                request: deposit_op,
            },
            true,
        )
        .await;
    assert_eq!(deposit["status"], "pending", "{deposit}");
    assert_eq!(
        fixture.rpc().send_count(),
        1,
        "exactly one inner Solana deposit RPC dispatch"
    );
    deposit_mock.1.credit.store(true, Ordering::SeqCst);
    let status = client
        .call(
            funding_args(&["plans", "status", &plan_id, "--json"]),
            BrokerRequest::PlanStatus { plan_id },
            true,
        )
        .await;
    assert_eq!(status["steps"][0]["state"], "completed", "{status}");
    assert_eq!(status["steps"][1]["state"], "completed", "{status}");
    assert_eq!(withdrawal_venue.writes(), 1);
}
