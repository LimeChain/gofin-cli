#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires public development CLI and owned Anvil"]
async fn public_cli_expired_fee_candidate_refreshes_within_native_authority() {
    let mut admission_probes=Vec::new();
    let mut main_counts=(0,0);
    for breach in [false, true] {
        let (_node, rpc, filler) = node(1).await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.sqlite");
        let provider = Arc::new(Provider {
            rpc: rpc.clone(), chain: 1, routing: "CLASSIC", deadline: chrono::Utc::now().timestamp() as u64 + 300,
            filler, fill: Mutex::new(BTreeMap::new()), orders: AtomicUsize::new(0), swaps: AtomicUsize::new(0),
            quote_unavailable: false, adverse_fill: false, store_path: path.clone(),
        });
        let approval = Arc::new(Approval(AtomicUsize::new(0), Some(Arc::new(ExpiryApproval {
            clock: Arc::new(QuoteClock(AtomicBool::new(true))), rpc: rpc.clone(), breach,
        }))));
        let client = Client::new(runtime(&path, rpc.clone(), provider, approval.clone()), temp.path()).await;
        let id = if breach { "expired-fee-refused" } else { "expired-fee-refreshed" };
        let make_request = |id: &str, wallet: &str, fee: &str| {
            let input = json!({"request_id":id,"wallet":wallet,"network":"ethereum","chain_id":1,"token":"native","amount":"0.001","recipient":"0x000000000000000000000000000000000000aAaA","max_fee_eth":fee});
            (args(&["execute","wallet.transfer","--input",&input.to_string(),"--id",id,"--json"]), BrokerRequest::Execute { request: OperationRequest {
                request_id:id.into(),operation:"wallet.transfer".into(),input,run_id:None,
            }})
        };
        let (argv, request) = make_request(id, "synthetic", "0.001");
        let mut result = client.call(argv.clone(), request.clone(), true).await;
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        let expected = usize::from(!breach);
        assert_eq!(rpc.broadcasts.load(Ordering::SeqCst), expected, "{result}");
        if breach {
            assert_eq!(result["outcome"]["dispatch"], "not_dispatched", "{result}");
            assert_eq!(result["outcome"]["result"], "failed", "{result}");
            assert!(result["submission"].is_null());
        } else {
            let logical_id = result["id"].as_str().unwrap().to_owned();
            for _ in 0..10 {
                if result["status"] == "completed" { break; }
                rpc.request("anvil_mine", json!(["0x40"])).await.unwrap();
                result = client.call(args(&["status", &logical_id, "--json"]), BrokerRequest::Status { id:logical_id.clone() }, true).await;
            }
            assert_eq!(result["status"], "completed", "{result}");
            assert_eq!(result["outcome"]["replay_safe"], false);
            assert!(result["effects"].is_object());
        }
        let repeated = client.call(argv, request, true).await;
        assert_eq!(repeated["id"], result["id"]);
        assert_eq!(approval.0.load(Ordering::SeqCst), 1);
        assert_eq!(rpc.broadcasts.load(Ordering::SeqCst), expected);
        rpc.request("anvil_setBalance", json!([other_owner(), "0x56bc75e2d63100000"])).await.unwrap();
        let snapshot=|| (approval.0.load(Ordering::SeqCst),rpc.broadcasts.load(Ordering::SeqCst));
        let counts=snapshot();main_counts.0+=counts.0;main_counts.1+=counts.1;
        for (probe_id, wallet) in [("expiry-same", "synthetic"), ("expiry-other", "synthetic-other")] {
            let (argv, request) = make_request(probe_id, wallet, "1");
            let before=snapshot();
            let probe = client.call(argv, request, true).await;
            admission_probes.push(outcome_evidence::admission_probe(if wallet=="synthetic" {"same_account"} else {"different_account"}, before, snapshot(), &probe));
            assert!(probe["submission"].is_object(), "{probe}");
        }
        assert_eq!(approval.0.load(Ordering::SeqCst), 3);
        assert_eq!(rpc.broadcasts.load(Ordering::SeqCst), expected + 2);
    }
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        let mut evidence = json!({"scenario":"outcome.a9.anvil","approval_count":main_counts.0,"external_submission_count":main_counts.1,"probe_approval_count":4,"probe_submission_count":4,"intervention_count":0,"remaining_external_uncertainty":false,"blocked_unrelated_activity":false});
        outcome_evidence::attach_probes(&mut evidence,&admission_probes);
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}
