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
        assert_eq!(challenge.binding["operation"], "bridge.execute");
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
}

#[test]
fn completed_compound_bridge_survives_catalog_reload_without_reapproval_or_replay() {
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
    let runtime = env.runtime(false);
    let client = Client::new(runtime.clone(), env.dir.path()).await;
    let input = json!({"intent":{"wallet":"synthetic-wallet","source":{"chainId":1,"token":bridge_fixture::ETH_USDC},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":bridge_fixture::ARB_USDC}],"recipient":env.wallet,"refundTo":env.wallet,"allowedRefundAssets":[{"chainId":1,"token":bridge_fixture::ETH_USDC}],"amount":{"minimum":"10000000","maximum":"10000000"}},"allowedProviders":["relay"],"bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":asset("ethereum:42161",bridge_fixture::ARB_USDC,6),"atoms":"9500000"},"fee_caps":[{"asset":asset("ethereum:1","native",18),"kind":"gas","maximum":"2000000000000000"}],"latest_protocol_deadline":null},"actionDeadline":chrono::Utc::now().timestamp()+600});
    let request = BrokerRequest::Execute {
        request: OperationRequest {
            request_id: "CATALOG-COMPOUND-BRIDGE".into(),
            operation: "bridge.execute".into(),
            input,
            run_id: None,
        },
    };
    let mut executed = call(&client, request.clone()).await;
    for _ in 0..3 {
        if executed["outcome"]["phase"] == "done" {
            break;
        }
        executed = call(
            &client,
            BrokerRequest::Status {
                id: executed["id"].as_str().unwrap().into(),
            },
        )
        .await;
    }
    assert_eq!(executed["outcome"]["phase"], "done", "{executed}");
    assert_eq!(env.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(env.source.broadcasts(), 2);
    let original = Store::open(env.dir.path().join("journal.sqlite"))
        .unwrap()
        .get_record(executed["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    drop(client);
    drop(runtime);
    let client = Client::new(env.runtime(true), env.dir.path()).await;
    let observed = call(&client, request).await;
    assert_eq!(observed["id"], executed["id"]);
    assert_eq!(observed["submission"], executed["submission"]);
    assert_eq!(observed["outcome"]["replay_safe"], false);
    assert_eq!(observed["outcome"]["phase"], "done");
    let stored = Store::open(env.dir.path().join("journal.sqlite"))
        .unwrap()
        .get_record(executed["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored["preview"], original["preview"]);
    assert_eq!(stored["approval"], original["approval"]);
    assert_eq!(env.approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(env.source.broadcasts(), 2);
}
