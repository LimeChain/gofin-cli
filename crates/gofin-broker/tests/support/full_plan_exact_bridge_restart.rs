//! Real CLI with reconstructed broker components and the same durable journal.
//! The current issuer canonicalizes both catalog versions; separate engine
//! regressions exercise genuinely issued historical definition hashes.
use super::*;

struct CatalogBeforeUpgrade(Arc<BridgeAdapter>);
#[async_trait]
impl Adapter for CatalogBeforeUpgrade {
    fn definitions(&self) -> Vec<Definition> {
        let mut definitions = self.0.definitions();
        let transfer = definitions
            .iter_mut()
            .find(|definition| definition.name == "bridge.transfer")
            .unwrap();
        let prior = format!(
            "{}; quote refresh stays on the persisted provider and route",
            transfer.semantics["request_identity"].as_str().unwrap()
        );
        transfer.semantics["request_identity"] = json!(prior);
        definitions
    }
    fn snapshot_is_global(&self) -> bool {
        true
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.0.snapshot().await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.0.read(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.0.prepare(operation, input).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.0.revalidate(prepared).await
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.0.submit(prepared, journal).await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.0.reconcile(record).await
    }
}

#[derive(Default)]
struct PlanApproval(AtomicUsize);
#[async_trait]
impl Authorizer for PlanApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(
            challenge.binding["plan"]["steps"].as_array().unwrap().len(),
            2
        );
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic exact bridge plan approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct Environment {
    dir: tempfile::TempDir,
    wallet: String,
    source: Arc<bridge_fixture::Rpc>,
    destination: Arc<bridge_fixture::Rpc>,
    provider: Arc<bridge_fixture::Erc20Provider>,
    approval: Arc<PlanApproval>,
}
impl Environment {
    fn new() -> Self {
        let signer = LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap();
        let wallet = gofin_evm::lower_address(signer.address());
        let source = Arc::new(bridge_fixture::Rpc::source_with_allowance(
            wallet.clone(),
            U256::ZERO,
        ));
        let destination = Arc::new(bridge_fixture::Rpc::destination_on(wallet.clone(), 42161));
        let provider = Arc::new(
            bridge_fixture::Erc20Provider::with_destination_token_receipt(
                destination.clone(),
                bridge_fixture::DestinationPayout::DirectWithHistoricalReads,
                Quantity::from_u64(9_900_000),
            ),
        );
        Self {
            dir: tempfile::tempdir().unwrap(),
            wallet,
            source,
            destination,
            provider,
            approval: Arc::new(PlanApproval::default()),
        }
    }
    fn runtime(&self, old_catalog: bool) -> Arc<gofin_broker::BrokerRuntime> {
        let signer = Arc::new(LocalEnvelopeSigner::from_private_key(&"11".repeat(32)).unwrap());
        let prices = Arc::new(SharedUsdValuator::with_source(Arc::new(
            bridge_fixture::Prices,
        )));
        let source_runtime = Arc::new(EvmRuntime::new(self.source.clone(), 1, Some(signer)));
        let destination_runtime = Arc::new(EvmRuntime::new(self.destination.clone(), 42161, None));
        let connection = WalletConnection {
            name: "synthetic-wallet".into(),
            address: self.wallet.clone(),
            signing_enabled: true,
        };
        let source_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(1, 1, vec![]),
                vec![connection.clone()],
                source_runtime.clone(),
            )
            .unwrap()
            .with_wallet_valuation(prices.clone()),
        );
        let destination_wallet = Arc::new(
            EthereumAdapter::new(
                EthereumConfig::wallet_only(42161, 1, vec![]),
                vec![WalletConnection {
                    signing_enabled: false,
                    ..connection
                }],
                destination_runtime.clone(),
            )
            .unwrap(),
        );
        let wallets = BTreeMap::from([(1, source_wallet), (42161, destination_wallet)]);
        let bridge = Arc::new(
            BridgeAdapter::with_provider_and_observers(
                self.provider.clone(),
                wallets.clone(),
                BTreeMap::from([(1, source_runtime), (42161, destination_runtime)]),
                prices.clone(),
            )
            .unwrap(),
        );
        let bridge_adapter: Arc<dyn Adapter> = if old_catalog {
            Arc::new(CatalogBeforeUpgrade(bridge.clone()))
        } else {
            bridge.clone()
        };
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
                store: Store::open(self.dir.path().join("journal.sqlite")).unwrap(),
                adapters: vec![
                    Arc::new(gofin_broker::EvmNetworkRouter::new(wallets)),
                    bridge_adapter,
                ],
                authorizer: self.approval.clone(),
                valuator: prices,
                bridge: Some(bridge),
            },
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap()
    }
    fn stored_transfer(&self, id: &str) -> BridgeTransferRecord {
        Store::open(self.dir.path().join("journal.sqlite"))
            .unwrap()
            .bridge_transfer(id)
            .unwrap()
            .unwrap()
    }
}

#[test]
fn exact_bridge_plan_survives_catalog_reload_and_same_scope_renewal() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                // Keep restarted broker handlers on production-sized workers.
                .thread_stack_size(2 * 1024 * 1024)
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
    let env = Environment::new();
    let old_runtime = env.runtime(true);
    let old_def = old_runtime
        .engine
        .definitions()
        .unwrap()
        .into_iter()
        .find(|d| d.name == "bridge.transfer")
        .unwrap();
    let client = Client::new(old_runtime.clone(), env.dir.path()).await;
    let quote = call(&client, BrokerRequest::Read {
        operation: "bridge.quotes".into(),
        input: json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":bridge_fixture::ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":bridge_fixture::ARB_USDC}],"recipient":env.wallet,"refundTo":env.wallet,"allowedRefundAssets":[{"chainId":1,"token":bridge_fixture::ETH_USDC}],"amount":"10000000"}}),
    }).await;
    let transfer = quote["transfers"][0]["transfer_id"].as_str().unwrap();
    let links = quote["transfers"][0]["steps"].as_array().unwrap();
    assert_eq!(links.len(), 2);
    let resource = format!("ethereum:1:{}", env.wallet);
    let usdc = asset("ethereum:1", bridge_fixture::ETH_USDC, 6);
    let native = asset("ethereum:1", "native", 18);
    let bounds = json!({
        "input":{"minimum":"10000000","maximum":"10000000"},
        "maximum_source_debit":"10000000",
        "minimum_net_receipt":{"asset":asset("ethereum:42161",bridge_fixture::ARB_USDC,6),"atoms":"9500000"},
        "fee_caps":[{"asset":native,"kind":"gas","maximum":"2000000000000000","maximum_usd_micros":4000000}],
        "maximum_total_usd_micros":14000000,
        "authorization_seconds":600
    });
    let gas = fee(&resource, &native, "0.001", CostTreatment::AdditionalDebit);
    let debit = amount(&resource, &usdc, "10");
    let steps = links.iter().enumerate().map(|(index, link)| step(
        if index == 0 { "approve" } else { "deposit" },
        "bridge.transfer",
        json!({"transferId":transfer,"stepId":link["step_id"],"stepKind":link["kind"],"bounds":bounds}),
        if index == 0 { vec![] } else { vec![debit.clone()] },
        vec![gas.clone()],if index == 0 { json!([]) } else { json!([{ "resource":format!("ethereum:42161:{}",env.wallet), "asset":asset("ethereum:42161",bridge_fixture::ARB_USDC,6)}]) },vec![],
    )).collect::<Vec<_>>();
    let spec = FinancialPlanSpec {
        version: FINANCIAL_PLAN_VERSION,
        name: "Exact bridge catalog reload".into(),
        summary: None,
        steps,
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: vec![debit],
            maximum_costs: vec![fee(
                &resource,
                &native,
                "0.002",
                CostTreatment::AdditionalDebit,
            )],
        },
    };
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "EXACT-BRIDGE-APPROVE".into(),
        plan: spec.clone(),
        valid_for_seconds: Some(600),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let review = call(
        &client,
        BrokerRequest::PreviewPlan {
            request: request.clone(),
        },
    )
    .await;
    for permission in review["permissions"].as_array().unwrap() {
        assert!(permission["exact"].is_object(), "{permission}");
        assert!(permission["permission"].is_null(), "{permission}");
    }
    request.expected_review_hash = Some(review["review_hash"].as_str().unwrap().into());
    let approved = call(&client, BrokerRequest::AuthorizePlan { request }).await;
    let plan_id = approved["plan_id"].as_str().unwrap().to_string();
    assert_eq!(env.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(env.source.broadcasts(), 0);
    drop(client);
    drop(old_runtime);

    let runtime = env.runtime(false);
    let current_def = runtime
        .engine
        .definitions()
        .unwrap()
        .into_iter()
        .find(|d| d.name == "bridge.transfer")
        .unwrap();
    // Public catalog hash from the affected released version, independently
    // recorded before the compatibility repair (not a synthetic grant edit).
    assert!(
        exact_plan_definition_hash_matches(
            &current_def,
            "5019f5ba9716bdc0b100634c4294a391a0a87d4012ef69471cad6f2f06e2a260",
        )
        .unwrap()
    );
    assert_ne!(
        old_def.semantics["request_identity"],
        current_def.semantics["request_identity"]
    );
    assert_eq!(old_def.input_schema, current_def.input_schema);
    let client = Client::new(runtime, env.dir.path()).await;
    let reloaded = call(
        &client,
        BrokerRequest::PlanStatus {
            plan_id: plan_id.clone(),
        },
    )
    .await;
    assert_eq!(reloaded["revision"], approved["revision"]);
    let execute = |index: usize| BrokerRequest::ExecutePlanStep {
        plan_id: plan_id.clone(),
        step_id: spec.steps[index].step_id.clone(),
        request: OperationRequest {
            request_id: format!("EXACT-BRIDGE-CHILD-{index}"),
            operation: "bridge.transfer".into(),
            input: spec.steps[index].input.clone(),
            run_id: None,
        },
    };
    let allowance = call(&client, execute(0)).await;
    assert_eq!(allowance["status"], "completed", "{allowance}");
    assert_eq!(env.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(env.source.broadcasts(), 1);
    let before = env.stored_transfer(transfer);
    let status = call(
        &client,
        BrokerRequest::PlanStatus {
            plan_id: plan_id.clone(),
        },
    )
    .await;
    let mut renewal = FinancialPlanAuthorizationRequest {
        request_id: "EXACT-BRIDGE-RENEW".into(),
        plan: spec.clone(),
        valid_for_seconds: Some(600),
        replaces_revision_id: Some(status["revision"]["revision_id"].as_str().unwrap().into()),
        expected_review_hash: None,
    };
    let review = call(
        &client,
        BrokerRequest::PreviewPlan {
            request: renewal.clone(),
        },
    )
    .await;
    renewal.expected_review_hash = Some(review["review_hash"].as_str().unwrap().into());
    let renewed = call(&client, BrokerRequest::AuthorizePlan { request: renewal }).await;
    assert_eq!(renewed["plan_id"], plan_id);
    assert_eq!(
        env.approval.0.load(Ordering::SeqCst),
        2,
        "only the deliberately requested renewal asks for another approval"
    );
    assert_eq!(renewed["usage"], status["usage"]);
    let parent = env.stored_transfer(transfer);
    assert_eq!(parent.approved_bounds, before.approved_bounds);
    assert_eq!(parent.steps, before.steps);
    assert_eq!(parent.quote_revisions, before.quote_revisions);
    let replay = call(&client, execute(0)).await;
    assert_eq!(replay["id"], allowance["id"]);
    assert_eq!(replay["submission"], allowance["submission"]);
    assert_eq!(env.source.broadcasts(), 1);
    let deposit = call(&client, execute(1)).await;
    assert_eq!(deposit["status"], "completed", "{deposit}");
    assert_eq!(env.source.broadcasts(), 2);
    let settled = call(
        &client,
        BrokerRequest::Read {
            operation: "bridge.status".into(),
            input: json!({"transferId":transfer}),
        },
    )
    .await;
    assert_eq!(settled["state"], "completed", "{settled}");
    let final_status = call(&client, BrokerRequest::PlanStatus { plan_id }).await;
    assert!(
        final_status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "completed"),
        "{final_status}"
    );
    assert_eq!(env.approval.0.load(Ordering::SeqCst), 2);
    assert_eq!(env.source.broadcasts(), 2);
}
