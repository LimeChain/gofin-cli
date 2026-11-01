// Public CLI/broker/composite-deposit rehearsal. The shared native Ethereum fixture
// exercises signed dispatch and Kraken credit; Base/ERC20 routes have separate fixtures.
use super::*;
use crate as gofin_broker;
use gofin_protocol::BrokerRequest;
include!("../tests/support/kraken_plan_client.rs");
mod ccxt_fixture {
    include!("kraken_deposit_ccxt_fixture.rs");
}

struct PlanApproval(AtomicUsize);
#[async_trait]
impl Authorizer for PlanApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "synthetic standalone deposit".into(),
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
fn exact_usdc_deposit_uses_read_fallback_without_replay() {
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
fn exact_usdc_deposit_native_scope_survives_price_policy_change() {
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
    let approved_debits = prepared.intent.debits.clone();
    let approved_costs = prepared.intent.maximum_costs.clone();
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
    if let Some(prices) = &prices {
        prices.arm_after_next_wallet_quote(999_804);
    }
    if drift == Some(PriceDrift::TightPolicy) {
        // Retired USD policy cannot narrow an otherwise unchanged native action.
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
        "--json",
    ]);
    let call = BrokerRequest::Execute {
        request: OperationRequest {
            request_id: "SYNTHETIC-DEPOSIT-ATTEMPT".into(),
            operation: "kraken.deposit".into(),
            input: deposit_input.clone(),
            run_id: None,
        },
    };
    if unsafe_fallback {
        let failure = client.call(argv, call, true).await;
        let failure = failure.get("error").unwrap_or(&failure);
        assert_eq!(
            failure["code"], "KRAKEN_DEPOSIT_ADDRESS_REQUIRED",
            "{failure}"
        );
        assert_eq!(approval.0.load(Ordering::SeqCst), 0);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
        assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
        ccxt.as_ref().unwrap().assert_legacy_calls(&venue);
        return;
    }
    let pending = client.call(argv.clone(), call.clone(), true).await;
    if let Some(prices) = &prices {
        assert_eq!(prices.usdc_micros.load(Ordering::SeqCst), 999_804);
        assert_eq!(prices.transitions.load(Ordering::SeqCst), 1);
        assert_eq!(
            pending["preview"]["intent"]["debits"],
            serde_json::to_value(&approved_debits).unwrap()
        );
        assert_eq!(
            pending["preview"]["intent"]["maximum_costs"],
            serde_json::to_value(&approved_costs).unwrap()
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
    let id = pending["id"].as_str().unwrap();
    let settled = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(settled["status"], "completed");
    let effects: gofin_domain::VerifiedEffects = serde_json::from_value(settled["effects"].clone()).unwrap();
    let effects = effects.effects;
    assert!(effects.iter().any(|e|matches!(e,gofin_domain::FinancialEffect::Transfer{to,debit,credit,..}if to==&binding.account_resource&&credit.asset==binding.venue_asset&&credit.atoms==binding.gross&&debit.atoms.to_decimal(if usdc {6}else{18})==if usdc {"100"}else{"0.01"})));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,gofin_domain::FinancialEffect::Cost{category,amount,..}if category=="ethereum_gas"&&amount.atoms.to_decimal(18)=="0.000021"))
    );
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
fn kraken_solana_funding_standalone_dispatches_once_and_recovers_after_restart() {
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
fn kraken_solana_usdc_funding_standalone_dispatches_once_and_recovers_after_restart() {
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
    let client = FundingClient::new(
        runtime(&path, adapter.clone(), approval.clone()),
        temp.path(),
    )
    .await;
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
        "--json",
    ]);
    let request = BrokerRequest::Execute { request: operation.clone() };
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
fn kraken_solana_sequential_funding_uses_prior_withdrawal_without_prefunding_principal()
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
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("kraken-future-sol-usdc.sqlite");

    // The wallet begins with zero USDC principal. A small SOL balance covers
    // network fee and refundable recipient ATA rent, not the planned principal.
    let (withdraw_profile, withdrawal_venue, fixture) =
        signed_solana_withdrawal_profile("USDC", true);
    fixture.set_balance(10_000_000, 0);
    fixture.register_token_owner("11111111111111111111111111111111");
    let withdrawal_adapter = Arc::new(gofin_exchange::ExchangeAdapter::new(
        (*withdraw_profile).clone(),
    ));
    let withdrawal_json = withdrawal_input("USDC");
    let _withdrawal_preview = withdrawal_adapter
        .prepare("kraken.withdraw-funds", &withdrawal_json)
        .await
        .unwrap();
    assert_eq!(fixture.token_balance(&fixture.signer_address()), 0);
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
                "--json",
            ]),
            BrokerRequest::Execute { request: withdrawal_op },
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
                "--json",
            ]),
            BrokerRequest::Execute { request: deposit_op },
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
    let id = deposit["id"].as_str().unwrap();
    let status = client.call(funding_args(&["status", id, "--json"]), BrokerRequest::Status { id: id.into() }, true).await;
    assert_eq!(status["status"], "completed", "{status}");
    assert_eq!(withdrawal_venue.writes(), 1);
}
