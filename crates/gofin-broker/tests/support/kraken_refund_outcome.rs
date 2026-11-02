// Provider-correlated principal refund, exercised through the public CLI.
#[path = "outcome_evidence.rs"]
mod outcome_evidence;
struct RefundVenue {
    main: Arc<Venue>,
    other: Arc<Venue>,
    refunded: AtomicBool,
}
#[async_trait]
impl KrakenTransport for RefundVenue {
    async fn request(
        &self,
        endpoint: E,
        account: Option<&KrakenAccount>,
        payload: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let main = account.is_none_or(|a| a.name == "synthetic-spot");
        if endpoint == E::KrakenPrivatePostGetApiKeyInfo {
            assert!(guard.is_none());
            return Ok(KrakenResponse::Json(
                json!({"error":[],"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades","withdraw-funds"],"valid_until":"0","iban":if main {"SYNTHETIC-MAIN"} else {"SYNTHETIC-OTHER"}}}),
            ));
        }
        if main
            && matches!(
                endpoint,
                E::KrakenPrivatePostWithdrawStatus | E::KrakenPrivatePostLedgers
            )
        {
            assert!(guard.is_none());
            let result = if endpoint == E::KrakenPrivatePostWithdrawStatus {
                json!([{"refid":"SYNTHETIC-WITHDRAWAL","status":"Failed","asset":"XETH","method":"Ether (Base)","key":"synthetic-base","info":WALLET,"amount":"0.0058","fee":"0.0001"}])
            } else {
                let mut ledger = json!({"WITHDRAW-LEDGER":{"refid":"SYNTHETIC-WITHDRAWAL","type":"withdrawal","asset":"XETH","amount":"-0.0058","fee":"0.0001","balance":"0.0001"}});
                if self.refunded.load(Ordering::SeqCst) {
                    ledger["REFUND-LEDGER"] = json!({"refid":"SYNTHETIC-WITHDRAWAL","type":"deposit","asset":"XETH","amount":"0.0058","fee":"0","balance":"0.0059"});
                }
                json!({"ledger":ledger})
            };
            return Ok(KrakenResponse::Json(json!({"error":[],"result":result})));
        }
        let venue = if main { &self.main } else { &self.other };
        venue.request(endpoint, account, payload, guard).await
    }
}
fn refund_runtime(
    path: &std::path::Path,
    venue: Arc<RefundVenue>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let accounts = [
        ("synthetic-spot", ACCOUNT),
        ("synthetic-other", "kraken:synthetic-other"),
    ]
    .into_iter()
    .map(|(name, resource)| KrakenAccount {
        name: name.into(),
        resource: ResourceId(resource.into()),
        credential_ref: format!("synthetic-{name}"),
        physical_identity: format!("synthetic-{name}"),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    })
    .collect();
    let profile = KrakenProfile::new(KrakenProduct::Spot, accounts, venue.clone())
        .unwrap()
        .with_prices(Arc::new(Prices))
        .with_recipient_verifier(venue.main.clone());
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![Arc::new(ExchangeAdapter::new(profile))],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn failed_kraken_withdrawal_waits_for_correlated_refund_without_blocking_other_actions() {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let venue = Arc::new(RefundVenue {
        main: Arc::new(Venue::default()),
        other: Arc::new(Venue::default()),
        refunded: AtomicBool::new(false),
    });
    venue.main.preparation_funds.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let client = Client::new(
        refund_runtime(&path, venue.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let input = json!({"requestId":"failed-withdrawal","account":"synthetic-spot","asset":"XETH","key":"synthetic-base","amount":"0.0059","max_fee":"0.0001","destination":DESTINATION});
    let request = BrokerRequest::Execute {
        request: OperationRequest {
            request_id: "failed-withdrawal".into(),
            operation: "kraken.withdraw-funds".into(),
            input: input.clone(),
            run_id: None,
        },
    };
    let argv = args(&[
        "execute",
        "kraken.withdraw-funds",
        "--input",
        &input.to_string(),
        "--id",
        "failed-withdrawal",
        "--json",
    ]);
    let counters = || (approval.0.load(Ordering::SeqCst),venue.main.writes.load(Ordering::SeqCst)+venue.other.writes.load(Ordering::SeqCst));
    let before_action = counters();
    let failed = client.call(argv.clone(), request.clone(), true).await;
    let after_action = counters();
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["outcome"]["transfer"], "source_pending", "{failed}");
    assert_eq!(failed["outcome"]["replay_safe"], false);
    let id = failed["id"].as_str().unwrap();
    let open = client
        .call(
            args(&["operations", "--open", "--json"]),
            BrokerRequest::OpenOperations,
            true,
        )
        .await;
    assert!(open.to_string().contains(id), "{open}");
    let mut probes=Vec::new();
    for (scope, probe, account) in [
        ("same_account", "same-account", "synthetic-spot"),
        ("different_account", "other-account", "synthetic-other"),
    ] {
        let before=counters();
        let input = json!({"requestId":probe,"account":account,"pair":"ETHUSDC","type":"buy","ordertype":"limit","volume":"0.006","price":"2000","maximumFees":{"XETH":"0.00003","USDC":"0.05"},"minimumReceive":"0.00597","timeinforce":"IOC"});
        let result = client
            .call(
                args(&[
                    "execute",
                    "kraken.add-order",
                    "--input",
                    &input.to_string(),
                    "--id",
                    probe,
                    "--json",
                ]),
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: probe.into(),
                        operation: "kraken.add-order".into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(result["status"], "completed", "{result}");
        probes.push(outcome_evidence::admission_probe(scope,before,counters(),&result));
    }
    let repeat = client.call(argv.clone(), request.clone(), true).await;
    assert_eq!(repeat["submission"], failed["submission"]);
    assert_eq!(repeat["outcome"]["transfer"], "source_pending");
    venue.refunded.store(true, Ordering::SeqCst);
    let returned = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(returned["status"], "failed", "{returned}");
    assert_eq!(
        returned["outcome"]["transfer"], "returned_to_source",
        "{returned}"
    );
    assert_eq!(returned["outcome"]["replay_safe"], false);
    assert_eq!(returned["submission"], failed["submission"]);
    let proof: VerifiedEffects = serde_json::from_value(returned["effects"].clone()).unwrap();
    assert!(proof.effects.iter().any(|effect|matches!(effect, FinancialEffect::CashDebit{resource,amount} if resource.0==ACCOUNT && amount.atoms.to_decimal(36)=="0.0001")));
    assert!(
        !proof
            .effects
            .iter()
            .any(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
    );
    drop(client);
    let client = Client::new(
        refund_runtime(&path, venue.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let repeat = client.call(argv, request, true).await;
    assert_eq!(repeat["id"], failed["id"]);
    assert_eq!(repeat["submission"], failed["submission"]);
    assert_eq!(repeat["outcome"]["transfer"], "returned_to_source");
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    assert_eq!(venue.main.writes.load(Ordering::SeqCst), 2);
    assert_eq!(venue.other.writes.load(Ordering::SeqCst), 1);
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let mut evidence=json!({"scenario":"outcome.a15.kraken","approval_count":after_action.0-before_action.0,"external_submission_count":after_action.1-before_action.1,"intervention_count":0,"remaining_external_uncertainty":false,"refund_evidence":"Unique withdrawal and principal-return ledger entries share original refid; only proven fee remains spent."});
        outcome_evidence::attach_probes(&mut evidence,&probes);
        std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}
