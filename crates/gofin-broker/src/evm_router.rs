//! Route shared operation names using the explicit immutable chain identity.
use super::*;
use gofin_domain::{PreparationContext, ReturnRouteContext, ReturnRouteProof};

#[doc(hidden)]
pub struct EvmNetworkRouter {
    adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
}
impl EvmNetworkRouter {
    pub fn new(adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>) -> Self {
        Self { adapters }
    }
    fn network_scope(&self, chain: u64, scope: &SnapshotScope) -> SnapshotScope {
        let mut selected = scope.clone();
        if !scope.resources.is_empty() {
            selected.resources = self.adapters[&chain]
                .connections()
                .into_iter()
                .filter(|connection| {
                    scope.matches_resource(&connection.resource)
                        || connection.address.as_ref().is_some_and(|address| {
                            scope.resources.iter().any(|resource| {
                                resource.0.eq_ignore_ascii_case(&format!(
                                    "evm:{}",
                                    address.to_ascii_lowercase()
                                ))
                            })
                        })
                })
                .map(|connection| connection.resource)
                .collect();
        }
        selected
    }
    async fn observe(&self, scope: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        let mut combined = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        };
        let permits = Arc::new(tokio::sync::Semaphore::new(4));
        let mut tasks = tokio::task::JoinSet::new();
        let mut selected = BTreeMap::new();
        for (&chain, adapter) in &self.adapters {
            if !scope.matches_network(&format!("ethereum:{chain}")) {
                continue;
            }
            let local_scope = self.network_scope(chain, scope);
            if !scope.resources.is_empty() && local_scope.resources.is_empty() {
                continue;
            }
            selected.insert(chain, local_scope.resources.clone());
            let adapter = adapter.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                // Bound concurrency, not response time. Caller cancellation
                // drops this JoinSet and aborts both queued and running reads.
                let result = match permits.acquire().await {
                    Ok(_permit) => adapter.snapshot_scoped(&local_scope).await,
                    Err(_) => Err(internal_error()),
                };
                (chain, result)
            });
        }
        let mut results = BTreeMap::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok((chain, result)) => {
                    results.insert(chain, result);
                }
                Err(_) => {
                    combined.complete = false;
                    combined
                        .pending
                        .push("evm:network-snapshot-task-failed".into());
                }
            }
        }
        let mut complete_resources = BTreeSet::new();
        for &chain in selected.keys() {
            match results.remove(&chain) {
                Some(Ok(snapshot)) => {
                    if snapshot.complete {
                        complete_resources.extend(snapshot.coverage.iter().cloned());
                    }
                    combined.complete &= snapshot.complete;
                    combined.coverage.extend(snapshot.coverage);
                    combined.pending.extend(
                        snapshot
                            .pending
                            .into_iter()
                            .map(|reason| format!("ethereum:{chain}:{reason}")),
                    );
                    combined.state.holdings.extend(snapshot.state.holdings);
                    combined.state.positions.extend(snapshot.state.positions);
                    combined
                        .state
                        .authorities
                        .extend(snapshot.state.authorities);
                    combined.state.in_flight.extend(snapshot.state.in_flight);
                }
                failure => {
                    combined.complete = false;
                    let code = match failure {
                        Some(Err(error)) => error.code,
                        _ => "SNAPSHOT_TASK_FAILED".into(),
                    };
                    combined
                        .pending
                        .push(format!("observer:ethereum:{chain}:{code}"));
                }
            }
        }
        if selected.is_empty() {
            combined.complete = false;
            combined
                .pending
                .push("evm:scope-no-matching-network-wallet".into());
        }
        // Public reusable-wallet IDs denote the selected physical network
        // resources. Never claim that alias covered if one capture is missing.
        for connection in self.connections() {
            let physical = self
                .authorization_identities()
                .into_iter()
                .filter(|identity| {
                    identity.address == connection.address
                        && identity
                            .network
                            .as_ref()
                            .is_some_and(|network| scope.matches_network(network))
                        && (scope.resources.is_empty()
                            || scope.matches_resource(&identity.resource)
                            || scope.resources.iter().any(|resource| {
                                resource.0.eq_ignore_ascii_case(&connection.resource.0)
                            }))
                })
                .map(|identity| identity.resource.0)
                .collect::<Vec<_>>();
            if !physical.is_empty()
                && physical
                    .iter()
                    .all(|resource| complete_resources.contains(resource))
            {
                let aliases = scope
                    .resources
                    .iter()
                    .filter(|resource| resource.0.eq_ignore_ascii_case(&connection.resource.0))
                    .map(|resource| resource.0.clone())
                    .collect::<Vec<_>>();
                if aliases.is_empty() {
                    combined.coverage.push(connection.resource.0);
                } else {
                    combined.coverage.extend(aliases);
                }
            }
        }
        for resource in &scope.resources {
            if !combined.coverage.contains(&resource.0) {
                combined.complete = false;
                combined
                    .pending
                    .push(format!("evm:scope-resource-not-covered:{}", resource.0));
            }
        }
        for network in &scope.networks {
            if !selected
                .keys()
                .any(|chain| network == &format!("ethereum:{chain}"))
            {
                combined.complete = false;
                combined
                    .pending
                    .push(format!("evm:scope-network-not-covered:{network}"));
            }
        }
        for asset in &scope.assets {
            if !combined
                .state
                .holdings
                .iter()
                .any(|holding| holding.amount.asset == *asset)
            {
                combined.complete = false;
                combined.pending.push(format!(
                    "evm:scope-asset-not-covered:{}:{}:{}",
                    asset.network, asset.reference, asset.decimals
                ));
            }
        }
        combined.coverage.sort();
        combined.coverage.dedup();
        combined.pending.sort();
        combined.pending.dedup();
        Ok(combined)
    }
    fn route(&self, input: &Value) -> gofin_domain::Result<&Arc<gofin_ethereum::EthereumAdapter>> {
        input["chain_id"]
            .as_u64()
            .and_then(|chain| self.adapters.get(&chain))
            .ok_or_else(|| {
                error(
                    "NETWORK_NOT_CONFIGURED",
                    "validation",
                    "This EVM network has no configured wallet.",
                    "Select an enabled chain and enrolled EVM wallet.",
                )
            })
    }
    fn prepared(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<&Arc<gofin_ethereum::EthereumAdapter>> {
        if prepared.input["chain_id"] != prepared.binding["transaction"]["chain_id"] {
            return Err(error(
                "BINDING_CHAIN_MISMATCH",
                "validation",
                "The original transaction and request have different chain identities.",
                "Reconcile the unchanged original operation.",
            ));
        }
        self.route(&prepared.input)
    }
}
#[async_trait]
impl Adapter for EvmNetworkRouter {
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        if matches!(operation, "gofin.snapshot" | "fin.snapshot") {
            let Ok(scope) = serde_json::from_value::<SnapshotScope>(input.clone()) else {
                return false;
            };
            return self.adapters.iter().any(|(&chain, adapter)| {
                let local = self.network_scope(chain, &scope);
                scope.matches_network(&format!("ethereum:{chain}"))
                    && (scope.resources.is_empty() || !local.resources.is_empty())
                    && adapter.uses_rpc_reads(operation, &serde_json::to_value(local).unwrap())
            });
        }
        self.route(input)
            .is_ok_and(|adapter| adapter.uses_rpc_reads(operation, input))
    }
    fn definitions(&self) -> Vec<Definition> {
        let mut definitions = BTreeMap::new();
        for adapter in self.adapters.values() {
            for definition in adapter.definitions() {
                definitions
                    .entry(definition.name.clone())
                    .or_insert(definition);
            }
        }
        definitions.into_values().collect()
    }
    fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        let mut connections = BTreeMap::<String, gofin_domain::ConnectionMetadata>::new();
        for adapter in self.adapters.values() {
            for connection in adapter.connections() {
                let entry = connections
                    .entry(connection.name.clone())
                    .or_insert_with(|| {
                        let mut shared = connection.clone();
                        shared.resource = ResourceId(format!(
                            "evm:{}",
                            connection
                                .address
                                .as_deref()
                                .unwrap_or_default()
                                .to_ascii_lowercase()
                        ));
                        shared.network = None;
                        shared.details = json!({"networks": []});
                        shared
                    });
                entry.details["networks"]
                    .as_array_mut()
                    .expect("array")
                    .push(
                        json!({"network": connection.network, "chain_id": adapter.config.chain_id}),
                    );
            }
        }
        connections.into_values().collect()
    }
    fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.adapters
            .values()
            .flat_map(|adapter| adapter.authorization_identities())
            .collect()
    }
    async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        self.observe(&SnapshotScope::default()).await
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        self.observe(scope).await
    }
    async fn read(&self, operation: &str, input: &Value) -> gofin_domain::Result<Value> {
        self.route(input)?.read(operation, input).await
    }
    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> gofin_domain::Result<PreparedOperation> {
        self.route(input)?.prepare(operation, input).await
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> gofin_domain::Result<gofin_domain::boundaries::BoundarySpec> {
        self.prepared(prepared)?
            .prepare_boundaries(prepared, bounds)
            .await
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> gofin_domain::Result<gofin_domain::financial_plans::PlanPermission> {
        self.route(input)?
            .prepare_plan_permission(operation, input, bounds)
            .await
    }
    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<()> {
        self.prepared(prepared)?
            .validate_boundary_candidate(spec, prepared)
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> gofin_domain::Result<PreparedOperation> {
        self.route(input)?
            .prepare_with_context(operation, input, context)
            .await
    }
    async fn preflight_return_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> gofin_domain::Result<ReturnRouteProof> {
        self.route(input)?
            .preflight_return_route(operation, input, context)
            .await
    }
    fn preparation_reuse_window(
        &self,
        prepared: &PreparedOperation,
    ) -> Option<std::time::Duration> {
        self.prepared(prepared)
            .ok()?
            .preparation_reuse_window(prepared)
    }
    async fn validate_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
    ) -> gofin_domain::Result<Vec<String>> {
        self.route(input)?
            .validate_plan_feasibility(operation, input)
            .await
    }
    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<PreparedOperation> {
        self.prepared(prepared)?.revalidate(prepared).await
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> gofin_domain::Result<Submission> {
        self.prepared(prepared)?.submit(prepared, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> gofin_domain::Result<Submission> {
        self.prepared(validated.prepared())?
            .submit_revalidated(validated, journal)
            .await
    }
    async fn verified_position_baselines(
        &self,
        record: &OperationRecord,
    ) -> gofin_domain::Result<Vec<gofin_domain::PositionAnchor>> {
        self.prepared(
            record
                .preview
                .as_ref()
                .ok_or_else(|| GoFinError::validation("Original preview missing."))?,
        )?
        .verified_position_baselines(record)
        .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
        self.prepared(
            record
                .preview
                .as_ref()
                .ok_or_else(|| GoFinError::validation("The original preview is missing."))?,
        )?
        .reconcile(record)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct ReadRpc(u64);
    struct NoCredentialAccess;
    impl gofin_exchange::ccxt::CredentialProvider for NoCredentialAccess {
        fn credentials(
            &self,
            _: &str,
        ) -> std::result::Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError>
        {
            panic!("read-only wallet composition must not obtain exchange credentials")
        }
    }
    impl BrokerCredentialProvider for NoCredentialAccess {
        fn envelope_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
            panic!("Read-only composition requested a wallet key")
        }
    }
    fn shared_config() -> BrokerConfigV2 {
        let address = "0x1111111111111111111111111111111111111111";
        BrokerConfigV2 {
            version: 2,
            approver_uid: 501,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![ConnectionConfig::Evm {
                name: "synthetic".into(),
                resource: ResourceId(format!("evm:{address}")),
                address: address.into(),
                credential_ref: None,
                signing_enabled: false,
            }],
            policy: BrokerPolicy::beta(),
            extensions: BTreeMap::new(),
        }
    }
    #[test]
    fn broker_composes_one_wallet_on_all_reviewed_networks_with_reviewed_base_protocols_without_keys()
     {
        let mut config = shared_config();
        config.validate().unwrap();
        let applications =
            build_protected_applications(&config, Arc::new(NoCredentialAccess)).unwrap();
        assert_eq!(applications.adapters.len(), 5);
        let exchange = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "exchange.markets")
            })
            .expect("exchange adapter is composed");
        assert!(exchange.connections().is_empty());
        assert!(
            exchange
                .definitions()
                .iter()
                .any(|definition| definition.name == "exchange.markets")
        );
        let evm = applications
            .adapters
            .iter()
            .find(|adapter| !adapter.connections().is_empty())
            .expect("shared EVM adapter is composed");
        assert_eq!(evm.connections().len(), 1);
        let networks = &evm.connections()[0].details["networks"];
        let ids = networks
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value["chain_id"].as_u64().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            [1, 8453, 42161, 4663, 10, 137, 43114, 5042, 999]
                .into_iter()
                .collect()
        );
        assert!(
            evm.definitions()
                .iter()
                .any(|definition| definition.name == "uniswap.swap")
        );
        let mut duplicate = config.connections[0].clone();
        if let ConnectionConfig::Evm { name, .. } = &mut duplicate {
            *name = "another-name".into();
        }
        config.connections.push(duplicate);
        assert!(config.validate().is_err());
        config.connections.pop();
        let address = "0x1111111111111111111111111111111111111111";
        config.connections.push(ConnectionConfig::Ethereum {
            name: "legacy-alias".into(),
            resource: ResourceId(format!("ethereum:1:{address}")),
            network: "ethereum:1".into(),
            address: address.into(),
            signing_enabled: false,
            credential_ref: None,
            assets: vec![AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            }],
        });
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_DUPLICATE_PHYSICAL_WALLET"
        );
        config.connections.pop();
        config.networks[1].chain_id = 1;
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_NETWORK_INVALID"
        );
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for ReadRpc {
        async fn request(&self, method: &str, _: Value) -> gofin_domain::Result<Value> {
            match method {
                "eth_chainId" => Ok(json!(format!("{:#x}", self.0))),
                "eth_blockNumber" => Ok(json!("0x16")),
                "eth_getBalance" => Ok(json!(format!("{:#x}", self.0))),
                _ => panic!("Unexpected financial transport in read-only router fixture"),
            }
        }
    }
    fn router() -> EvmNetworkRouter {
        EvmNetworkRouter::new(
            [42161, 4663]
                .into_iter()
                .map(|chain| {
                    let adapter = gofin_ethereum::EthereumAdapter::new(
                        gofin_ethereum::EthereumConfig::wallet_only(chain, 2, vec![]),
                        vec![gofin_ethereum::WalletConnection {
                            name: "synthetic".into(),
                            address: "0x1111111111111111111111111111111111111111".into(),
                            signing_enabled: false,
                        }],
                        Arc::new(gofin_evm::EvmRuntime::new(
                            Arc::new(ReadRpc(chain)),
                            chain,
                            None,
                        )),
                    )
                    .unwrap();
                    (chain, Arc::new(adapter))
                })
                .collect(),
        )
    }
    #[tokio::test]
    async fn shared_names_route_to_explicit_chain_and_return_one_reusable_wallet() {
        let router = router();
        let definitions = router.definitions();
        assert_eq!(definitions.len(), 6);
        assert_eq!(
            definitions
                .iter()
                .filter(|definition| definition.name == "wallet.transfer")
                .count(),
            1
        );
        let connections = router.connections();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].provider, "evm");
        assert_eq!(connections[0].network, None);
        assert_eq!(
            connections[0].details["networks"].as_array().unwrap().len(),
            2
        );
        let authorization_identities = router.authorization_identities();
        assert_eq!(authorization_identities.len(), 2);
        assert_eq!(
            authorization_identities
                .iter()
                .map(|identity| identity.details["chain_id"].as_u64().unwrap())
                .collect::<BTreeSet<_>>(),
            [42161, 4663].into_iter().collect()
        );
        assert!(authorization_identities.iter().all(|identity| {
            identity.provider == "evm"
                && !identity.signing_enabled
                && identity.details["confirmations"].as_u64().is_some()
        }));
        for chain in [42161, 4663] {
            let result = router
                .read(
                    "wallet.balances",
                    &json!({"wallet":"synthetic","network":"evm","chain_id":chain,"tokens":[]}),
                )
                .await
                .unwrap();
            assert_eq!(result["chain_id"], chain);
            assert_eq!(result["data"][0]["amount"]["atoms"], chain.to_string());
            assert_eq!(
                result["data"][0]["amount"]["asset"]["network"],
                format!("ethereum:{chain}")
            );
        }
        assert_eq!(
            router
                .read(
                    "wallet.balances",
                    &json!({"wallet":"synthetic","network":"evm","chain_id":999})
                )
                .await
                .unwrap_err()
                .code,
            "NETWORK_NOT_CONFIGURED"
        );
        assert_eq!(
            router
                .read(
                    "aave.positions",
                    &json!({"wallet":"synthetic","network":"evm","chain_id":4663})
                )
                .await
                .unwrap_err()
                .code,
            "OPERATION_UNAVAILABLE"
        );
    }
    #[test]
    fn original_binding_cannot_be_rerouted_by_changing_request_chain() {
        let router = router();
        let mut prepared = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({"chain_id":42161}),
            binding: json!({"transaction":{"chain_id":4663}}),
            summary: "Synthetic routing fixture only".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: now(),
            valuation: None,
            intent: Default::default(),
            operation_version: 1,
            action_deadline: None,
        };
        assert_eq!(
            router.prepared(&prepared).err().unwrap().code,
            "BINDING_CHAIN_MISMATCH"
        );
        prepared.input["chain_id"] = json!(4663);
        assert_eq!(router.prepared(&prepared).unwrap().config.chain_id, 4663);
    }
    struct SnapshotRpc {
        chain: u64,
        stalled: bool,
        failed: bool,
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
    }
    struct ActiveRead(Arc<AtomicUsize>);
    impl Drop for ActiveRead {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for SnapshotRpc {
        async fn request(&self, method: &str, _: Value) -> gofin_domain::Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let _read = ActiveRead(self.active.clone());
            if self.stalled {
                return std::future::pending().await;
            }
            if self.failed {
                return Err(GoFinError::new(
                    "SYNTHETIC_RPC_FAILED",
                    "snapshot",
                    "The synthetic RPC returned an error.",
                    "Retry the synthetic observer.",
                ));
            }
            match method {
                "eth_chainId" => Ok(json!(format!("{:#x}", self.chain))),
                "eth_blockNumber" => Ok(json!("0x16")),
                "eth_getBlockByNumber" => {
                    Ok(json!({"number":"0x16","hash":format!("0x{:064x}", self.chain)}))
                }
                "eth_getBalance" => Ok(json!("0x7")),
                "eth_getTransactionCount" => Ok(json!("0x0")),
                _ => panic!("Unreviewed method in isolated router snapshot fixture: {method}"),
            }
        }
    }
    fn snapshot_router() -> (Arc<EvmNetworkRouter>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        snapshot_router_with_failure(false)
    }
    fn snapshot_router_with_failure(
        failed: bool,
    ) -> (Arc<EvmNetworkRouter>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let slow_calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let adapters = [42161, 4663]
            .into_iter()
            .map(|chain| {
                let rpc = SnapshotRpc {
                    chain,
                    stalled: chain == 4663 && !failed,
                    failed: chain == 4663 && failed,
                    calls: if chain == 4663 {
                        slow_calls.clone()
                    } else {
                        Arc::new(AtomicUsize::new(0))
                    },
                    active: active.clone(),
                };
                let adapter = gofin_ethereum::EthereumAdapter::new(
                    gofin_ethereum::EthereumConfig::wallet_only(chain, 1, vec![]),
                    vec![gofin_ethereum::WalletConnection {
                        name: "synthetic".into(),
                        address: "0x1111111111111111111111111111111111111111".into(),
                        signing_enabled: false,
                    }],
                    Arc::new(gofin_evm::EvmRuntime::new(Arc::new(rpc), chain, None)),
                )
                .unwrap();
                (chain, Arc::new(adapter))
            })
            .collect();
        (
            Arc::new(EvmNetworkRouter::new(adapters)),
            slow_calls,
            active,
        )
    }
    struct NoApproval;
    #[async_trait]
    impl Authorizer for NoApproval {
        async fn authorize(
            &self,
            _: &gofin_domain::AuthorizationChallenge,
        ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
            panic!("Snapshot tests must not request approval")
        }
    }
    #[tokio::test]
    async fn production_composed_router_scoped_snapshot_expands_grouped_wallet_without_unrelated_rpc()
     {
        let config = shared_config();
        // Keep the protected composition's actual global bridge observer and
        // public exchange profile. Inject only isolated RPC child adapters in
        // the same concrete router registered by production composition.
        let mut applications =
            build_protected_applications(&config, Arc::new(NoCredentialAccess)).unwrap();
        let (router, slow_calls, _) = snapshot_router();
        for adapter in &mut applications.adapters {
            if adapter
                .definitions()
                .iter()
                .any(|definition| definition.name == "wallet.balances")
            {
                *adapter = router.clone();
            }
        }
        let runtime = compose_runtime(
            config,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: applications.adapters,
                authorizer: Arc::new(NoApproval),
                valuator: applications.valuator,
                bridge: applications.bridge,
            },
        )
        .unwrap();
        let grouped = router.connections()[0].resource.clone();
        for resource in [
            grouped.clone(),
            ResourceId("ethereum:42161:0x1111111111111111111111111111111111111111".into()),
        ] {
            let before = std::time::Instant::now();
            let result = runtime
                .handle(BrokerRequest::Read {
                    operation: "gofin.snapshot".into(),
                    input: json!({"resources":[resource],"networks":["ethereum:42161"]}),
                })
                .await;
            assert!(result.ok, "{:?}", result.error);
            let result = result.result.unwrap();
            let snapshot: StateSnapshot = serde_json::from_value(result).unwrap();
            assert!(snapshot.complete, "{:?}", snapshot.pending);
            assert_eq!(snapshot.state.holdings.len(), 1);
            assert_eq!(
                snapshot.state.holdings[0].amount.asset.network,
                "ethereum:42161"
            );
            assert!(snapshot.coverage.contains(&resource.0));
            assert!(before.elapsed() < Duration::from_secs(1));
        }
        assert_eq!(slow_calls.load(Ordering::SeqCst), 0);
        let unknown = router
            .snapshot_scoped(&SnapshotScope {
                resources: vec![ResourceId(
                    "evm:0x9999999999999999999999999999999999999999".into(),
                )],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!unknown.complete && unknown.state.holdings.is_empty());
        assert_eq!(slow_calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn router_partial_error_preserves_fast_network_and_cancellation_aborts_stalled_network() {
        let (router, slow_calls, active) = snapshot_router_with_failure(true);
        let snapshot = router.observe(&SnapshotScope::default()).await.unwrap();
        assert!(!snapshot.complete);
        assert_eq!(snapshot.state.holdings.len(), 1);
        assert_eq!(
            snapshot.state.holdings[0].amount.asset.network,
            "ethereum:42161"
        );
        assert!(
            snapshot
                .pending
                .iter()
                .any(|pending| pending == "observer:ethereum:4663:SYNTHETIC_RPC_FAILED")
        );
        assert!(
            !snapshot
                .coverage
                .iter()
                .any(|coverage| coverage.starts_with("evm:")),
            "grouped coverage requires every selected physical network"
        );
        assert_eq!(slow_calls.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        // Cancellation drops the router JoinSet, which aborts child readers.
        // This timeout belongs to the test's caller, not the snapshot router.
        let (router, slow_calls, active) = snapshot_router();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), router.snapshot())
                .await
                .is_err()
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(slow_calls.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn protected_composition_unknown_scope_is_partial_without_network_access() {
        let config = shared_config();
        let applications =
            build_protected_applications(&config, Arc::new(NoCredentialAccess)).unwrap();
        let runtime = compose_runtime(
            config,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: applications.adapters,
                authorizer: Arc::new(NoApproval),
                valuator: applications.valuator,
                bridge: applications.bridge,
            },
        )
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.handle(BrokerRequest::Read {
                operation: "gofin.snapshot".into(),
                input: json!({"networks":["ethereum:999999"]}),
            }),
        )
        .await
        .unwrap();
        assert!(result.ok, "{:?}", result.error);
        let result = result.result.unwrap();
        let snapshot: StateSnapshot = serde_json::from_value(result).unwrap();
        assert!(!snapshot.complete);
        assert!(
            snapshot
                .pending
                .iter()
                .any(|pending| pending.contains("network-not-covered"))
        );
        assert!(snapshot.state.holdings.is_empty());
    }
}
