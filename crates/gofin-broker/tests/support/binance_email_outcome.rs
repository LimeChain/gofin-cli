use super::*;
include!("kraken_plan_client.rs");

struct EmailPayout;
#[async_trait::async_trait]
impl gofin_binance::BinanceRecipientVerifier for EmailPayout {
    async fn verify(&self, request: gofin_binance::PayoutRequest) -> gofin_binance::PayoutResult {
        assert_eq!(request.tx_hash, format!("0x{}", "ee".repeat(32)));
        gofin_binance::PayoutResult { status: gofin_domain::Status::Completed,
            amount_atoms: Some(gofin_domain::Quantity::from_decimal("0.009", request.asset.decimals).unwrap()),
            evidence: json!({"synthetic_receipt": true}) }
    }
}
fn email_runtime(db: &std::path::Path, venue: Arc<SyntheticVenue>, approval: Arc<SyntheticApproval>) -> Arc<gofin_broker::BrokerRuntime> {
    let adapter = Arc::new(BinanceAdapter::new(adapter_config(), venue, Some(Arc::new(SyntheticBinancePrice)), Some(Arc::new(EmailPayout))).unwrap());
    compose_runtime(runtime_config(), RuntimeDependencies { store: Store::open(db).unwrap(), adapters: vec![adapter], authorizer: approval, valuator: Arc::new(SyntheticValuator), bridge: None }).unwrap()
}


#[tokio::test(flavor="multi_thread",worker_threads=2)]
#[ignore="requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn email_confirmation_keeps_original_withdrawal_open_and_allows_unrelated_actions() {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("journal.sqlite");
    let venue=Arc::new(SyntheticVenue::new());let approval=Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    {let mut state=venue.state.lock().unwrap();state.balances.insert("main".into(),"0.020".into());state.withdrawal_status=Some(0);}
    let client=Client::new(email_runtime(&path,venue.clone(),approval.clone()),temp.path()).await;
    let input=json!({"requestId":"email-wait","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.010","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"});
    let request=BrokerRequest::Execute{request:OperationRequest{request_id:"email-wait".into(),operation:"binance.withdraw".into(),input:input.clone(),run_id:None}};
    let argv=args(&["execute","binance.withdraw","--input",&input.to_string(),"--id","email-wait","--json"]);
    let before_action = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
    let pending=client.call(argv.clone(),request.clone(),true).await;
    let after_action = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
    assert_eq!(pending["status"],"pending","{pending}");
    assert_eq!(pending["outcome"]["phase"], "needs_you", "{pending}");
    assert_eq!(pending["outcome"]["attention"]["kind"],"action_required","{pending}");
    assert!(pending["outcome"]["attention"]["where_to_act"].as_str().unwrap().contains("Binance App"));
    assert_eq!(pending["outcome"]["next_action"]["code"],"act_at_venue");
    assert_eq!(pending["outcome"]["replay_safe"],false);
    let mut probes = Vec::new();
    for (scope,id,from,to) in [("same_account","email-same",MAIN,CHILD_A),("different_account","email-different",CHILD_B,MAIN)] {
        let before = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
        let input=transfer(id,from,to,"0.001");
        let result=client.call(args(&["execute","binance.transfer","--input",&input.to_string(),"--id",id,"--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:id.into(),operation:"binance.transfer".into(),input,run_id:None}},true).await;
        assert_eq!(result["status"],"completed","{result}");
        let after = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
        probes.push(outcome_evidence::admission_probe(scope, before, after, &result));
    }
    assert_eq!(approval.0.load(Ordering::SeqCst),3);assert_eq!(venue.state.lock().unwrap().posts,3);
    drop(client);
    let client=Client::new(email_runtime(&path,venue.clone(),approval.clone()),temp.path()).await;
    let repeated=client.call(argv,request,true).await;
    assert_eq!(repeated["id"],pending["id"]);assert_eq!(repeated["submission"],pending["submission"]);
    assert_eq!(repeated["outcome"]["attention"]["kind"],"action_required");
    assert_eq!(approval.0.load(Ordering::SeqCst),3);assert_eq!(venue.state.lock().unwrap().posts,3);
    {
        let mut state = venue.state.lock().unwrap();
        state.withdrawal_history[0]["status"] = json!(6);
        state.withdrawal_history[0]["txId"] = json!(format!("0x{}", "ee".repeat(32)));
    }
    let id = pending["id"].as_str().unwrap();
    let settled = client.call(args(&["status", id, "--json"]), BrokerRequest::Status { id: id.into() }, true).await;
    assert_eq!(settled["outcome"]["transfer"], "credited", "evidence: {} error: {}", settled["last_reconciliation_evidence"], settled["error"]);
    assert_eq!(settled["outcome"]["phase"], "done", "{settled}");
    assert_eq!(settled["submission"], pending["submission"]);
    if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let mut evidence = json!({"scenario":"outcome.a16","approval_count":after_action.0-before_action.0,"external_submission_count":after_action.1-before_action.1,"intervention_count":0,"human_action_count":1,"remaining_external_uncertainty":false,"stage_submissions":{"withdrawal":after_action.1-before_action.1}});
        outcome_evidence::attach_probes(&mut evidence, &probes);
        std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
#[ignore="requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn failed_withdrawal_remains_unresolved_without_correlated_refund_and_does_not_block_other_actions() {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let venue = Arc::new(SyntheticVenue::new());
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    {
        let mut state = venue.state.lock().unwrap();
        state.balances.insert("main".into(), "0.020".into());
        state.withdrawal_status = Some(5);
    }
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let input = json!({"requestId":"failed-withdrawal","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.010","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"});
    let request = BrokerRequest::Execute { request: OperationRequest { request_id:"failed-withdrawal".into(), operation:"binance.withdraw".into(), input:input.clone(), run_id:None } };
    let argv = args(&["execute", "binance.withdraw", "--input", &input.to_string(), "--id", "failed-withdrawal", "--json"]);
    let before_action = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
    let failed = client.call(argv.clone(), request.clone(), true).await;
    let after_action = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["outcome"]["transfer"], "source_pending", "{failed}");
    assert_ne!(failed["outcome"]["phase"], "done", "{failed}");
    let mut probes = Vec::new();
    for (scope, id, from, to) in [("same_account", "failed-same", MAIN, CHILD_A), ("different_account", "failed-different", CHILD_B, MAIN)] {
        let before = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
        let input = transfer(id, from, to, "0.001");
        let result = client.call(args(&["execute", "binance.transfer", "--input", &input.to_string(), "--id", id, "--json"]), BrokerRequest::Execute { request: OperationRequest { request_id:id.into(), operation:"binance.transfer".into(), input, run_id:None } }, true).await;
        assert_eq!(result["status"], "completed", "{result}");
        let after = (approval.0.load(Ordering::SeqCst), venue.state.lock().unwrap().posts);
        probes.push(outcome_evidence::admission_probe(scope, before, after, &result));
    }
    let id = failed["id"].as_str().unwrap();
    let observed = client.call(args(&["status", id, "--json"]), BrokerRequest::Status { id:id.into() }, true).await;
    assert_eq!(observed["outcome"]["transfer"], "source_pending", "a free balance must not fabricate a correlated refund: {observed}");
    drop(client);
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let repeated = client.call(argv, request, true).await;
    assert_eq!(repeated["id"], failed["id"]);
    assert_eq!(repeated["submission"], failed["submission"]);
    assert_eq!(repeated["outcome"]["transfer"], "source_pending");
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    assert_eq!(venue.state.lock().unwrap().posts, 3);
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let mut evidence = json!({"scenario":"outcome.a15.binance.unconfirmed","approval_count":after_action.0-before_action.0,"external_submission_count":after_action.1-before_action.1,"intervention_count":0,"remaining_external_uncertainty":true,"limitation":"No correlated refund credit was observed."});
        outcome_evidence::attach_probes(&mut evidence, &probes);
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
#[ignore="requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn available_wallet_funds_can_fund_a_new_action_while_withdrawal_correlation_is_unconfirmed() {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let venue=Arc::new(SyntheticVenue::new());
    { let mut state=venue.state.lock().unwrap(); state.balances.insert("main".into(),"0.020".into()); state.withdrawal_status=Some(0); }
    let signer=gofin_evm::LocalEnvelopeSigner::from_private_key("0x0000000000000000000000000000000000000000000000000000000000000001").unwrap();
    let address=format!("{:#x}",signer.address());
    let wallet=ResourceId(format!("ethereum:1:{address}"));
    let wallet_balance=Arc::new(Mutex::new("0".to_owned()));
    *venue.wallet_balance.lock().unwrap()=Some(wallet_balance.clone());
    let temp=tempfile::tempdir().unwrap(); let path=temp.path().join("withdrawal-independent-funding.sqlite");
    let transaction=Arc::new(Mutex::new(None));
    let journal=Arc::new(SyntheticSubmissionJournal::default());
    let rpc=Arc::new(DepositExecutionRpc { oracle_updated_at:synthetic_oracle_updated_at(), priority_fee:AtomicU64::new(10_000_000), base_fee:AtomicU64::new(1_000_000_000), nonce:AtomicU64::new(0), gas_estimate:AtomicU64::new(21_000), simulation_failure:AtomicBool::new(false), transaction:transaction.clone(), hash:Mutex::new(None), accepted:AtomicUsize::new(0), venue:venue.clone(), journal:journal.inner.clone(), durable_store_path:Some(path.clone()), wallet_address:address.clone(), wallet_balance:wallet_balance.clone(), payout_amount:"0.009".into(), lose_submit_response:AtomicBool::new(false) });
    let ethereum=Arc::new(gofin_ethereum::EthereumAdapter::new(synthetic_mainnet_ethereum_config(),vec![gofin_ethereum::WalletConnection{name:"synthetic-wallet".into(),address:address.clone(),signing_enabled:true}],Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(),1,Some(Arc::new(RecordingSigner{signer,transaction}))))).unwrap());
    let mut binance_config=adapter_config(); binance_config.destinations[0].address=address.clone(); binance_config.destinations[0].resource=wallet.clone();
    let binance=Arc::new(BinanceAdapter::new(binance_config,venue.clone(),Some(Arc::new(SyntheticBinancePrice)),None).unwrap());
    let composite=Arc::new(gofin_broker::BinanceEthereumDepositAdapter::new(binance.clone(),ethereum));
    let observer=Arc::new(WalletSnapshotObserver{resource:wallet.clone(),address:address.clone(),balance:wallet_balance.clone()});
    let mut config=runtime_config();
    for connection in &mut config.connections { if let ConnectionConfig::Ethereum{resource,address:configured,..}=connection { *resource=wallet.clone(); *configured=address.clone(); } }
    let approval=Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let runtime=compose_runtime(config,RuntimeDependencies{store:Store::open(&path).unwrap(),adapters:vec![binance,composite,observer],authorizer:approval.clone(),valuator:Arc::new(SyntheticValuator),bridge:None}).unwrap();
    let client=Client::new(runtime,temp.path()).await;
    let counters=|| (approval.0.load(Ordering::SeqCst),venue.state.lock().unwrap().posts+rpc.accepted.load(Ordering::SeqCst));
    let before_action=counters();
    let input=json!({"requestId":"a17-withdraw","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.010","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"});
    let withdrawal=client.call(args(&["execute","binance.withdraw","--input",&input.to_string(),"--id","a17-withdraw","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"a17-withdraw".into(),operation:"binance.withdraw".into(),input,run_id:None}},true).await;
    assert_eq!(withdrawal["outcome"]["transfer"],"source_pending","{withdrawal}");
    // Binance reports completion while independent receipt correlation remains unavailable.
    { let mut state=venue.state.lock().unwrap(); state.withdrawal_history[0]["status"]=json!(6); state.withdrawal_history[0]["txId"]=json!(format!("0x{}", "ee".repeat(32))); }
    let withdrawal_id=withdrawal["id"].as_str().unwrap();
    let in_transit=client.call(args(&["status",withdrawal_id,"--json"]),BrokerRequest::Status{id:withdrawal_id.into()},true).await;
    assert_eq!(in_transit["outcome"]["transfer"],"in_transit","{in_transit}");
    assert_eq!(in_transit["submission"],withdrawal["submission"]);
    assert_eq!(*wallet_balance.lock().unwrap(),"0.009");
    let mut probes=Vec::new();
    for (scope,id,from,to) in [("same_account","a17-same",MAIN,CHILD_A),("different_account","a17-other",CHILD_B,MAIN)] {
        let before=counters(); let input=transfer(id,from,to,"0.001");
        let result=client.call(args(&["execute","binance.transfer","--input",&input.to_string(),"--id",id,"--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:id.into(),operation:"binance.transfer".into(),input,run_id:None}},true).await;
        probes.push(outcome_evidence::admission_probe(scope,before,counters(),&result));
    }
    // The independently observed destination balance funds a distinct signed deposit;
    // it is not used as proof of which withdrawal produced that balance.
    let input=json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.008","maxFeeEth":"0.001"});
    let funded=client.call(args(&["execute","binance.deposit","--input",&input.to_string(),"--id","scoped-deposit-forward","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"scoped-deposit-forward".into(),operation:"binance.deposit".into(),input,run_id:None}},true).await;
    assert!(funded["submission"].is_object(),"{funded}"); assert_eq!(rpc.accepted.load(Ordering::SeqCst),1);
    let id=withdrawal["id"].as_str().unwrap(); let later=client.call(args(&["status",id,"--json"]),BrokerRequest::Status{id:id.into()},true).await;
    assert_eq!(later["outcome"]["transfer"],"in_transit","{later}"); assert_eq!(later["submission"],withdrawal["submission"]);
    let after=counters(); let probe_approvals:usize=probes.iter().map(|p|p["approval_count"].as_u64().unwrap() as usize).sum(); let probe_writes:usize=probes.iter().map(|p|p["external_submission_count"].as_u64().unwrap() as usize).sum();
    if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { let mut evidence=json!({"scenario":"outcome.a17.binance","approval_count":after.0-before_action.0-probe_approvals,"external_submission_count":after.1-before_action.1-probe_writes,"intervention_count":0,"remaining_external_uncertainty":true,"destination_balance":"0.009 ETH observed independently","funded_action_submission":funded["submission"]}); outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
}

struct UnpricedApproval { count: AtomicUsize, reviews: Mutex<Vec<gofin_domain::approval_review::ApprovalReview>> }
#[async_trait::async_trait]
impl gofin_domain::Authorizer for UnpricedApproval {
    fn requires_financial_review(&self) -> bool { true }
    async fn authorize(&self, c: &gofin_domain::AuthorizationChallenge) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
        self.count.fetch_add(1,Ordering::SeqCst);
        self.reviews.lock().unwrap().push(c.review.clone().expect("financial review"));
        Ok(gofin_domain::ApprovalAudit{mechanism:"synthetic".into(),source:None,yolo_provenance:None,binding_hash:c.binding_hash.clone(),challenge_nonce:c.nonce.clone(),approved_at:gofin_domain::now(),expires_at:c.expires_at.clone()})
    }
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
#[ignore="requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn native_exchange_transfer_completes_without_usd_and_does_not_block_probes() {
    let temp=tempfile::tempdir().unwrap(); let path=temp.path().join("unpriced-transfer.sqlite");
    let venue=Arc::new(SyntheticVenue::new()); let approval=Arc::new(UnpricedApproval{count:AtomicUsize::new(0),reviews:Mutex::new(Vec::new())});
    let adapter=Arc::new(BinanceAdapter::new(adapter_config(),venue.clone(),None,None).unwrap());
    let runtime=compose_runtime(runtime_config(),RuntimeDependencies{store:Store::open(&path).unwrap(),adapters:vec![adapter],authorizer:approval.clone(),valuator:Arc::new(SyntheticValuator),bridge:None}).unwrap();
    let client=Client::new(runtime,temp.path()).await;
    let counters=|| (approval.count.load(Ordering::SeqCst),venue.state.lock().unwrap().posts);
    let before=counters(); let input=transfer("unpriced",CHILD_A,MAIN,"0.001");
    let original=client.call(args(&["execute","binance.transfer","--input",&input.to_string(),"--id","unpriced","--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:"unpriced".into(),operation:"binance.transfer".into(),input,run_id:None}},true).await;
    let after=counters();
    assert_eq!(original["outcome"]["phase"],"done","{original}");
    assert_eq!(original["outcome"]["result"],"executed","{original}");
    assert_eq!(original["outcome"]["reporting"],"partial","{original}");
    assert!(original["preview"]["valuation"].is_null(),"{original}");
    {
        let reviews=approval.reviews.lock().unwrap();
        assert!(reviews[0].terms.iter().any(|term|term.label=="Expected USD value" && term.value=="Unavailable" && matches!(&term.enforcement,gofin_domain::approval_review::ApprovalTermEnforcement::Estimate)));
    }
    let mut probes=Vec::new();
    for (scope,id,from,to) in [("same_account","unpriced-same",CHILD_A,MAIN),("different_account","unpriced-other",CHILD_B,MAIN)] {
        let before=counters(); let input=transfer(id,from,to,"0.001");
        let probe=client.call(args(&["execute","binance.transfer","--input",&input.to_string(),"--id",id,"--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:id.into(),operation:"binance.transfer".into(),input,run_id:None}},true).await;
        probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));
    }
    if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { let mut evidence=json!({"scenario":std::env::var("GOFIN_OUTCOME_SCENARIO").unwrap_or_else(|_|"outcome.a1.binance".into()),"approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":false,"usd_estimate":"unavailable"}); outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
}
