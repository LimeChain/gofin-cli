//! Provider-documented send states. No fabricated refund or UI approval fields.
use super::*;
const OTHER_PORTFOLIO: &str = "00000000-0000-4000-8000-000000000099";
fn runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    other: Arc<Venue>,
    proof: Arc<IndependentBaseProof>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let accounts = [
        ("coinbase-dev", PORTFOLIO, SOURCE),
        ("coinbase-other", OTHER_PORTFOLIO, "coinbase:other"),
    ]
    .map(|(name, portfolio, resource)| CoinbaseAccount {
        name: name.into(),
        resource: ResourceId(resource.into()),
        credential_ref: name.into(),
        physical_identity: portfolio.into(),
        identity_verified: true,
        portfolio_id: Some(portfolio.into()),
        product: CoinbaseProduct::Advanced,
    });
    let apis: BTreeMap<String, Arc<dyn CoinbaseApi>> = BTreeMap::from([
        ("coinbase-dev".into(), venue as Arc<dyn CoinbaseApi>),
        ("coinbase-other".into(), other as Arc<dyn CoinbaseApi>),
    ]);
    let profile = CoinbaseProfile::new(accounts.into(), apis)
        .unwrap()
        .with_prices(Arc::new(Prices))
        .with_recipient_verifier(proof);
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
#[tokio::test]
async fn failed_send_remains_open_without_inventing_refund_and_other_accounts_proceed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("failed-send.sqlite");
    let venue = Arc::new(Venue::default());
    venue.state.lock().unwrap().observed_status = Some("failed".into());
    let other = Arc::new(Venue {
        portfolio: OTHER_PORTFOLIO,
        ..Default::default()
    });
    let proof = Arc::new(IndependentBaseProof {
        venue: venue.clone(),
        proofs: AtomicUsize::new(0),
    });
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let client = Client::new(
        runtime(
            &path,
            venue.clone(),
            other.clone(),
            proof.clone(),
            approval.clone(),
        ),
        dir.path(),
    )
    .await;
    let first = client.execute(input(), true).await;
    let id = first["id"].as_str().unwrap();
    let observed = client.status(id).await;
    assert_eq!(observed["outcome"]["result"], "failed", "{observed}");
    assert_eq!(observed["outcome"]["replay_safe"], false);
    assert!(
        observed["outcome"]["next_action"]["message"]
            .as_str()
            .unwrap()
            .contains("Coinbase App")
    );
    assert_ne!(observed["outcome"]["attention"]["kind"], "action_required");
    assert_ne!(observed["outcome"]["transfer"], "returned_to_source");
    let open = client
        .call(
            vec!["operations".into(), "--open".into(), "--json".into()],
            BrokerRequest::OpenOperations,
            true,
        )
        .await;
    assert!(open.as_array().unwrap().iter().any(|v| v["id"] == id));
    for (request, account, wire) in [
        ("same-account", "coinbase-dev", &venue),
        ("different-account", "coinbase-other", &other),
    ] {
        wire.hbar_funding.store(true, Ordering::SeqCst);
        let mut i = hbar_sale_input();
        i["account"] = json!(account);
        i["requestId"] = json!(request);
        let request_body = OperationRequest {
            request_id: request.into(),
            operation: "coinbase.create-order".into(),
            input: i.clone(),
            run_id: None,
        };
        let order = client
            .call(
                vec![
                    "execute".into(),
                    "coinbase.create-order".into(),
                    "--input".into(),
                    i.to_string(),
                    "--id".into(),
                    request.into(),
                    "--json".into(),
                ],
                BrokerRequest::Execute {
                    request: request_body,
                },
                true,
            )
            .await;
        assert!(order["submission"].is_object(), "{order}");
    }
    let id = id.to_owned();
    drop(client);
    let client = Client::new(
        runtime(&path, venue.clone(), other.clone(), proof, approval.clone()),
        dir.path(),
    )
    .await;
    let repeated = client.execute(input(), true).await;
    assert_eq!(repeated["id"], id);
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(other.writes.load(Ordering::SeqCst), 1);
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        assert!(std::env::var_os("GOFIN_COINBASE_PLAN_CLI").is_some());
        std::fs::write(path, serde_json::to_vec_pretty(&json!({
            "scenario":"outcome.coinbase-unresolved", "approval_count":1,
            "external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,
            "intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,
            "qualification":"Failed send retained; provider-correlated refund and send-specific human-action evidence unavailable. The user-approved Coinbase fallback advises consulting the Coinbase App."
        })).unwrap()).unwrap();
    }
}
