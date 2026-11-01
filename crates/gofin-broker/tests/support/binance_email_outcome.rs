use super::*;
include!("kraken_plan_client.rs");

#[tokio::test(flavor="multi_thread",worker_threads=2)]
#[ignore="requires GOFIN_KRAKEN_PLAN_CLI built with synthetic-kraken-build"]
async fn email_confirmation_keeps_original_withdrawal_open_and_allows_unrelated_actions() {
    assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
    let temp=tempfile::tempdir().unwrap();let path=temp.path().join("journal.sqlite");
    let venue=Arc::new(SyntheticVenue::new());let approval=Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    {let mut state=venue.state.lock().unwrap();state.balances.insert("main".into(),"0.020".into());state.withdrawal_status=Some(0);}
    let client=Client::new(runtime(&path,venue.clone(),approval.clone()),temp.path()).await;
    let input=json!({"requestId":"email-wait","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.010","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"});
    let request=BrokerRequest::Execute{request:OperationRequest{request_id:"email-wait".into(),operation:"binance.withdraw".into(),input:input.clone(),run_id:None}};
    let argv=args(&["execute","binance.withdraw","--input",&input.to_string(),"--id","email-wait","--json"]);
    let pending=client.call(argv.clone(),request.clone(),true).await;
    assert_eq!(pending["status"],"pending","{pending}");
    assert_eq!(pending["outcome"]["attention"]["kind"],"action_required","{pending}");
    assert!(pending["outcome"]["attention"]["where_to_act"].as_str().unwrap().contains("Binance App"));
    assert_eq!(pending["outcome"]["next_action"]["code"],"act_at_venue");
    assert_eq!(pending["outcome"]["replay_safe"],false);
    for (id,from,to) in [("email-same",MAIN,CHILD_A),("email-different",CHILD_B,MAIN)] {
        let input=transfer(id,from,to,"0.001");
        let result=client.call(args(&["execute","binance.transfer","--input",&input.to_string(),"--id",id,"--json"]),BrokerRequest::Execute{request:OperationRequest{request_id:id.into(),operation:"binance.transfer".into(),input,run_id:None}},true).await;
        assert_eq!(result["status"],"completed","{result}");
    }
    assert_eq!(approval.0.load(Ordering::SeqCst),3);assert_eq!(venue.state.lock().unwrap().posts,3);
    drop(client);
    let client=Client::new(runtime(&path,venue.clone(),approval.clone()),temp.path()).await;
    let repeated=client.call(argv,request,true).await;
    assert_eq!(repeated["id"],pending["id"]);assert_eq!(repeated["submission"],pending["submission"]);
    assert_eq!(repeated["outcome"]["attention"]["kind"],"action_required");
    assert_eq!(approval.0.load(Ordering::SeqCst),3);assert_eq!(venue.state.lock().unwrap().posts,3);
    if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"outcome.a16","approval_count":1,"external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"stage_submissions":{"withdrawal":1}})).unwrap()).unwrap();
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
    let failed = client.call(argv.clone(), request.clone(), true).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["outcome"]["transfer"], "source_pending", "{failed}");
    assert_ne!(failed["outcome"]["phase"], "done", "{failed}");
    for (id, from, to) in [("failed-same", MAIN, CHILD_A), ("failed-different", CHILD_B, MAIN)] {
        let input = transfer(id, from, to, "0.001");
        let result = client.call(args(&["execute", "binance.transfer", "--input", &input.to_string(), "--id", id, "--json"]), BrokerRequest::Execute { request: OperationRequest { request_id:id.into(), operation:"binance.transfer".into(), input, run_id:None } }, true).await;
        assert_eq!(result["status"], "completed", "{result}");
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
        std::fs::write(path, serde_json::to_vec_pretty(&json!({"scenario":"outcome.a15.binance.unconfirmed","approval_count":1,"external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"limitation":"Binance withdrawal history documents failure status and withdrawal identity but no correlated refund credit."})).unwrap()).unwrap();
    }
}
