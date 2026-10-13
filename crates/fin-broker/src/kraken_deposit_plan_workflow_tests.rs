// Public CLI/broker/composite-deposit rehearsal. The shared native Ethereum fixture
// exercises signed dispatch and Kraken credit; Base/ERC20 routes have separate fixtures.
use super::*;
use crate as fin_broker;
use fin_domain::financial_plans::*;
use fin_protocol::BrokerRequest;
include!("../tests/support/kraken_plan_client.rs");

struct PlanApproval(AtomicUsize);
#[async_trait]
impl Authorizer for PlanApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(c.binding["plan"]["steps"][0]["operation"], "kraken.deposit");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
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
    crate::compose_runtime(
        crate::BrokerConfigV2 {
            version: 2,
            networks: crate::NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: crate::BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        crate::RuntimeDependencies {
            store: fin_store::Store::open(path).unwrap(),
            adapters: vec![adapter],
            authorizer: approval,
            valuator: Arc::new(crate::SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
    )
    .unwrap()
}
#[test]
fn named_deposit_cli_waits_for_usable_credit_and_replays_original_after_restart() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse())
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn rehearse() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("deposit-journal.sqlite");
    let (adapter, venue, signer) = signed_adapter(None);
    let adapter = Arc::new(adapter);
    let approval = Arc::new(PlanApproval(AtomicUsize::new(0)));
    let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    let binding = decode_binding(&prepared.binding).unwrap();
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic named Kraken deposit".into(),
        summary: Some("Deposit 0.01 ETH and wait for exact usable Kraken credit.".into()),
        steps: vec![FinancialPlanStepSpec {
            step_id: "deposit".into(),
            operation: "kraken.deposit".into(),
            input: input(),
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
    let client = Client::new(
        runtime(&path, adapter.clone(), approval.clone()),
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
    // A proven source transaction plus known Settled status does not imply usable credit.
    *venue.deposit_status_override.lock().unwrap() = json!({"status":"Settled"});
    let argv = args(&[
        "execute",
        "kraken.deposit",
        "--input",
        &input().to_string(),
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
            input: input(),
            run_id: None,
        },
    };
    let pending = client.call(argv.clone(), call.clone(), true).await;
    assert_eq!(pending["status"], "pending", "{pending}");
    assert!(pending["effects"].is_null());
    assert!(pending["error"].is_null());
    assert_eq!(
        pending["last_reconciliation_evidence"]["awaiting"],
        "venue_usable_credit"
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    let submission = pending["submission"].clone();
    assert!(submission["reference"].as_str().unwrap().starts_with("0x"));
    drop(client);
    let client = Client::new(runtime(&path, adapter, approval.clone()), temp.path()).await;
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
    assert!(effects.iter().any(|e|matches!(e,fin_domain::FinancialEffect::Transfer{to,debit,credit,..}if to==&binding.account_resource&&credit.asset==binding.venue_asset&&credit.atoms==binding.gross&&debit.atoms.to_decimal(18)=="0.01")));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e,fin_domain::FinancialEffect::Cost{category,amount,..}if category=="ethereum_gas"&&amount.atoms.to_decimal(18)=="0.000021"))
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
