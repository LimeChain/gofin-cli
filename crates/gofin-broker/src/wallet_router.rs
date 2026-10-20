//! Route the shared wallet operation family across EVM and Solana.
//!
//! The engine registers operation names once. New requests select an adapter
//! with their explicit `network`; recovery reuses the original prepared input,
//! preserving the immutable venue/network choice across restarts.
use super::*;
use gofin_domain::{PreparationContext, RecoveryBinding, ReturnRouteContext, ReturnRouteProof};

pub(super) struct WalletFamilyRouter {
    evm: Option<Arc<dyn Adapter>>,
    solana: BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
}

impl WalletFamilyRouter {
    pub fn new(
        evm: Option<Arc<dyn Adapter>>,
        solana: BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
    ) -> Self {
        Self { evm, solana }
    }

    fn route(&self, input: &Value) -> gofin_domain::Result<&dyn Adapter> {
        match input.get("network").and_then(Value::as_str) {
            Some("solana") | Some("solana:mainnet-beta") => {
                let wallet = input.get("wallet").and_then(Value::as_str).ok_or_else(|| {
                    GoFinError::validation(
                        "Solana wallet operations must select an enrolled wallet.",
                    )
                })?;
                self.solana
                    .get(wallet)
                    .map(|adapter| adapter.as_ref() as &dyn Adapter)
                    .ok_or_else(|| {
                        GoFinError::new(
                            "SOLANA_WALLET_NOT_CONFIGURED",
                            "connection",
                            "The selected Solana wallet is not enrolled.",
                            "Enroll a Solana wallet through protected human-terminal setup.",
                        )
                    })
            }
            Some("evm" | "ethereum") => self
                .evm
                .as_deref()
                .ok_or_else(|| GoFinError::validation("No EVM wallet is enrolled.")),
            Some(network) if network.starts_with("ethereum:") => self
                .evm
                .as_deref()
                .ok_or_else(|| GoFinError::validation("No EVM wallet is enrolled.")),
            _ => Err(GoFinError::validation(
                "Wallet operations require an explicit reviewed network.",
            )),
        }
    }

    fn prepared(&self, prepared: &PreparedOperation) -> gofin_domain::Result<&dyn Adapter> {
        self.route(&prepared.input)
    }

    fn merge_definitions(&self) -> Vec<Definition> {
        merge_wallet_definitions(
            self.evm
                .as_ref()
                .into_iter()
                .flat_map(|evm| evm.definitions())
                .chain(
                    self.solana
                        .values()
                        .flat_map(|adapter| adapter.definitions()),
                )
                .collect(),
        )
    }

    async fn combined_snapshot(
        &self,
        scope: &SnapshotScope,
    ) -> gofin_domain::Result<StateSnapshot> {
        let mut result = empty_snapshot();
        let evm_connections = self
            .evm
            .as_ref()
            .map(|evm| evm.connections())
            .unwrap_or_default();
        let all_connections = evm_connections
            .iter()
            .cloned()
            .chain(
                self.solana
                    .values()
                    .flat_map(|adapter| adapter.connections()),
            )
            .collect::<Vec<_>>();
        let evm_scope = scoped_family(scope, &evm_connections, true);
        if let (Some(evm), Some(evm_scope)) = (&self.evm, evm_scope.as_ref()) {
            merge_snapshot(&mut result, evm.snapshot_scoped(evm_scope).await?);
        }
        let mut any_family_selected = evm_scope.is_some();
        // Solana RPC calls enforce their own transport/retry deadlines. The
        // broker exempts this selected read from the non-RPC observer cutoff.
        for adapter in self.solana.values() {
            let connection = adapter.connections().into_iter().next();
            let Some(connection) = connection else {
                continue;
            };
            let Some(wallet_scope) = scoped_family(scope, std::slice::from_ref(&connection), false)
            else {
                continue;
            };
            any_family_selected = true;
            merge_snapshot(&mut result, adapter.snapshot_scoped(&wallet_scope).await?);
        }
        if !scope.is_full() {
            if !any_family_selected {
                result.complete = false;
                result
                    .pending
                    .push("wallet:scope-no-matching-family".into());
            }
            for resource in &scope.resources {
                if !result.coverage.contains(&resource.0) {
                    result.complete = false;
                    result
                        .pending
                        .push(format!("wallet:scope-resource-not-covered:{}", resource.0));
                }
            }
            for network in &scope.networks {
                if !all_connections
                    .iter()
                    .any(|connection| connection_networks(connection).iter().any(|n| n == network))
                {
                    result.complete = false;
                    result
                        .pending
                        .push(format!("wallet:scope-network-not-covered:{network}"));
                }
            }
            for asset in &scope.assets {
                if !all_connections.iter().any(|connection| {
                    connection_networks(connection)
                        .iter()
                        .any(|network| network == &asset.network)
                }) {
                    result.complete = false;
                    result.pending.push(format!(
                        "wallet:scope-asset-network-not-covered:{}",
                        asset.network
                    ));
                }
            }
        }
        Ok(result)
    }
}

fn empty_snapshot() -> StateSnapshot {
    StateSnapshot {
        state: gofin_domain::FinancialState::default(),
        observed_at: gofin_domain::now(),
        complete: true,
        coverage: Vec::new(),
        pending: Vec::new(),
    }
}

fn merge_snapshot(target: &mut StateSnapshot, source: StateSnapshot) {
    target.state.holdings.extend(source.state.holdings);
    target.state.positions.extend(source.state.positions);
    target.state.authorities.extend(source.state.authorities);
    target.state.in_flight.extend(source.state.in_flight);
    target.coverage.extend(source.coverage);
    target.pending.extend(source.pending);
    target.complete &= source.complete;
}

fn scoped_family(
    scope: &SnapshotScope,
    connections: &[gofin_domain::ConnectionMetadata],
    evm: bool,
) -> Option<SnapshotScope> {
    if connections.is_empty() {
        return None;
    }
    let selected = connections
        .iter()
        .filter(|connection| {
            let networks = connection_networks(connection);
            if networks.is_empty() {
                return false;
            }
            let resource_matches = scope.resources.is_empty()
                || scope.resources.iter().any(|resource| {
                    connection_resources(connection, &networks, evm)
                        .iter()
                        .any(|candidate| candidate == resource)
                });
            let network_matches = scope.networks.is_empty()
                || scope
                    .networks
                    .iter()
                    .any(|requested| networks.contains(requested));
            let asset_network_matches = scope.assets.is_empty()
                || scope
                    .assets
                    .iter()
                    .any(|asset| networks.contains(&asset.network));
            resource_matches && network_matches && asset_network_matches
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return None;
    }
    let networks = scope
        .networks
        .iter()
        .filter(|requested| {
            selected
                .iter()
                .any(|connection| connection_networks(connection).contains(requested))
        })
        .cloned()
        .collect();
    let assets = scope
        .assets
        .iter()
        .filter(|asset| {
            selected
                .iter()
                .any(|connection| connection_networks(connection).contains(&asset.network))
        })
        .cloned()
        .collect();
    let resources = scope
        .resources
        .iter()
        .filter(|resource| {
            selected.iter().any(|connection| {
                let networks = connection_networks(connection);
                connection_resources(connection, &networks, evm).contains(resource)
            })
        })
        .cloned()
        .collect();
    Some(SnapshotScope {
        resources,
        networks,
        assets,
    })
}

fn connection_networks(connection: &gofin_domain::ConnectionMetadata) -> Vec<String> {
    if let Some(network) = &connection.network {
        return vec![network.clone()];
    }
    connection
        .details
        .get("networks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|network| network.get("network").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn connection_resources(
    connection: &gofin_domain::ConnectionMetadata,
    networks: &[String],
    evm: bool,
) -> Vec<ResourceId> {
    let mut resources = vec![connection.resource.clone()];
    if evm && let Some(address) = &connection.address {
        resources.push(ResourceId(format!("evm:{}", address.to_ascii_lowercase())));
        resources.extend(
            networks
                .iter()
                .map(|network| ResourceId(format!("{}:{}", network, address.to_ascii_lowercase()))),
        );
    }
    resources
}

pub(super) fn merge_wallet_definitions(items: Vec<Definition>) -> Vec<Definition> {
    let mut output = Vec::new();
    let mut positions = BTreeMap::<String, usize>::new();
    for definition in items {
        if !definition.name.starts_with("wallet.") {
            output.push(definition);
            continue;
        }
        if let Some(index) = positions.get(&definition.name).copied() {
            let existing = &mut output[index];
            merge_schema_alternatives(&mut existing.input_schema, definition.input_schema);
            merge_schema_alternatives(&mut existing.output_schema, definition.output_schema);
            merge_schema_alternatives(&mut existing.error_schema, definition.error_schema);
            merge_semantics(&mut existing.semantics, definition.semantics);
            for example in definition.examples {
                if !existing.examples.contains(&example) {
                    existing.examples.push(example);
                }
            }
            if existing.kind != definition.kind
                || existing.execution_actor != definition.execution_actor
            {
                existing.description.push_str(
                    " Provider-specific execution semantics are preserved by the selected network.",
                );
            }
            if !existing.coverage.contains(&definition.coverage) {
                existing.coverage.push_str(" Also: ");
                existing.coverage.push_str(&definition.coverage);
            }
            if !existing.completion.contains(&definition.completion) {
                existing.completion.push_str(" Also: ");
                existing.completion.push_str(&definition.completion);
            }
        } else {
            positions.insert(definition.name.clone(), output.len());
            output.push(definition);
        }
    }
    output
}

fn merge_semantics(target: &mut Value, source: Value) {
    if *target == source {
        return;
    }
    let (Some(target_object), Some(source_object)) = (target.as_object(), source.as_object())
    else {
        merge_semantics_variants(target, source);
        return;
    };
    let target_original = target.clone();
    let source_original = source.clone();
    let mut common = serde_json::Map::new();
    for (key, value) in target_object {
        if source_object.get(key) == Some(value) {
            common.insert(key.clone(), value.clone());
        }
    }
    let mut variants = semantics_variants(&target_original);
    variants.extend(semantics_variants(&source_original));
    variants.dedup();
    common.insert("provider_variants".into(), Value::Array(variants));
    *target = Value::Object(common);
}

fn semantics_variants(semantics: &Value) -> Vec<Value> {
    if let Some(variants) = semantics.get("provider_variants").and_then(Value::as_array) {
        return variants.clone();
    }
    vec![json!({
        "network":semantics.get("network"),
        "provider":semantics.get("provider"),
        "semantics":semantics
    })]
}

fn merge_semantics_variants(target: &mut Value, source: Value) {
    *target = json!({
        "provider_variants":[
            {"provider":"first","semantics":target.clone()},
            {"provider":"second","semantics":source}
        ]
    });
}

fn merge_schema_alternatives(target: &mut Value, source: Value) {
    if *target == source {
        return;
    }
    let alternatives = match target.get_mut("anyOf") {
        Some(Value::Array(alternatives)) => alternatives,
        _ => {
            let previous = target.clone();
            *target = json!({"anyOf": [previous]});
            target
                .get_mut("anyOf")
                .and_then(Value::as_array_mut)
                .expect("array")
        }
    };
    if !alternatives.contains(&source) {
        alternatives.push(source);
    }
}

#[async_trait]
impl Adapter for WalletFamilyRouter {
    fn diagnostics(&self) -> Value {
        let mut snapshots = Vec::new();
        for snapshot in self.evm.iter().map(|adapter| adapter.diagnostics()).chain(
            self.solana
                .values()
                .map(|adapter| adapter.runtime().rpc().diagnostics()),
        ) {
            let values = match snapshot {
                Value::Array(values) => values,
                value => vec![value],
            };
            for value in values {
                if !value.is_null() && !snapshots.contains(&value) {
                    snapshots.push(value);
                }
            }
        }
        json!(snapshots)
    }

    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        if matches!(operation, "gofin.snapshot" | "fin.snapshot") {
            let Ok(scope) = serde_json::from_value::<SnapshotScope>(input.clone()) else {
                return false;
            };
            let evm_uses_rpc = self.evm.as_ref().is_some_and(|evm| {
                scoped_family(&scope, &evm.connections(), true)
                    .and_then(|scope| serde_json::to_value(scope).ok())
                    .is_some_and(|scope| evm.uses_rpc_reads(operation, &scope))
            });
            let solana_selected = self.solana.values().any(|adapter| {
                adapter
                    .connections()
                    .into_iter()
                    .next()
                    .is_some_and(|connection| {
                        scoped_family(&scope, std::slice::from_ref(&connection), false).is_some()
                    })
            });
            return evm_uses_rpc || solana_selected;
        }
        self.route(input)
            .is_ok_and(|adapter| adapter.uses_rpc_reads(operation, input))
    }

    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> gofin_domain::Result<()> {
        if let Some(evm) = &self.evm {
            evm.attach_execution_state(state.clone())?;
        }
        for adapter in self.solana.values() {
            adapter.attach_execution_state(state.clone())?;
        }
        Ok(())
    }
    fn definitions(&self) -> Vec<Definition> {
        self.merge_definitions()
    }
    fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.evm
            .as_ref()
            .into_iter()
            .flat_map(|evm| evm.connections())
            .chain(
                self.solana
                    .values()
                    .flat_map(|adapter| adapter.connections()),
            )
            .collect()
    }
    fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.evm
            .as_ref()
            .into_iter()
            .flat_map(|evm| evm.authorization_identities())
            .chain(
                self.solana
                    .values()
                    .flat_map(|adapter| adapter.authorization_identities()),
            )
            .collect()
    }
    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        let evm_grouped = self
            .evm
            .as_ref()
            .into_iter()
            .flat_map(|evm| evm.connections());
        let evm_physical = self
            .evm
            .as_ref()
            .into_iter()
            .flat_map(|evm| evm.authorization_identities());
        let solana = self
            .solana
            .values()
            .flat_map(|adapter| adapter.snapshot_identities());
        let mut seen = BTreeSet::new();
        evm_grouped
            .chain(evm_physical)
            .chain(solana)
            .filter(|identity| seen.insert(identity.resource.0.clone()))
            .collect()
    }
    async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        self.combined_snapshot(&SnapshotScope::default()).await
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        self.combined_snapshot(scope).await
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
    fn validate_recovery_child(
        &self,
        prepared: &PreparedOperation,
        parent: &OperationRecord,
    ) -> gofin_domain::Result<Option<RecoveryBinding>> {
        self.prepared(prepared)?
            .validate_recovery_child(prepared, parent)
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
        exact_scope: Option<&gofin_domain::financial_plans::FinancialPlanExactScope>,
    ) -> gofin_domain::Result<Vec<String>> {
        self.route(input)?
            .validate_plan_feasibility(operation, input, exact_scope)
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
                .ok_or_else(|| GoFinError::validation("The original preview is missing."))?,
        )?
        .verified_position_baselines(record)
        .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| GoFinError::validation("The original preview is missing."))?;
        self.prepared(prepared)?.reconcile(record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RoutedAdapter {
        reconciles: Arc<AtomicUsize>,
        snapshots: Arc<AtomicUsize>,
        revalidated_submits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for RoutedAdapter {
        fn definitions(&self) -> Vec<Definition> {
            Vec::new()
        }
        async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
            Ok(Value::Null)
        }
        fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![gofin_domain::ConnectionMetadata {
                name: "evm-wallet".into(),
                provider: "ethereum".into(),
                resource: ResourceId(
                    "ethereum:42161:0x1111111111111111111111111111111111111111".into(),
                ),
                role: Some("wallet".into()),
                parent: None,
                network: Some("ethereum:42161".into()),
                address: Some("0x1111111111111111111111111111111111111111".into()),
                credential_ref: None,
                signing_enabled: false,
                details: Value::Null,
            }]
        }
        async fn snapshot(&self) -> gofin_domain::Result<gofin_domain::StateSnapshot> {
            self.snapshot_scoped(&SnapshotScope::default()).await
        }
        async fn snapshot_scoped(
            &self,
            _: &SnapshotScope,
        ) -> gofin_domain::Result<gofin_domain::StateSnapshot> {
            self.snapshots.fetch_add(1, Ordering::SeqCst);
            Ok(gofin_domain::StateSnapshot {
                state: gofin_domain::FinancialState::default(),
                observed_at: gofin_domain::now(),
                complete: true,
                coverage: vec!["ethereum:42161:0x1111111111111111111111111111111111111111".into()],
                pending: vec![],
            })
        }
        async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("test adapter does not prepare"))
        }
        async fn revalidate(
            &self,
            prepared: &PreparedOperation,
        ) -> gofin_domain::Result<PreparedOperation> {
            Ok(prepared.clone())
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            Err(GoFinError::validation("test adapter does not submit"))
        }
        async fn submit_revalidated(
            &self,
            validated: &gofin_domain::RevalidatedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            validated.assert_fresh()?;
            self.revalidated_submits.fetch_add(1, Ordering::SeqCst);
            Ok(Submission {
                kind: "revalidated-test".into(),
                reference: "routed".into(),
                data: Value::Null,
            })
        }
        async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
            self.reconciles.fetch_add(1, Ordering::SeqCst);
            Ok(Reconciliation {
                status: gofin_domain::Status::Unknown,
                evidence: json!({"routed":"evm"}),
                verified: None,
            })
        }
    }

    struct EmptySolanaRpc(Arc<AtomicUsize>);
    #[async_trait]
    impl gofin_solana::SolanaRpc for EmptySolanaRpc {
        async fn request(&self, method: &str, _: Value) -> gofin_domain::Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            match method {
                "getBalance" => Ok(json!({"context":{"slot":1},"value":1_000_000_000})),
                "getEpochInfo" => Ok(json!({"epoch":1})),
                "getTokenAccountsByOwner" => Ok(json!({"context":{"slot":1},"value":[]})),
                _ => Err(GoFinError::validation("Unexpected router test RPC.")),
            }
        }
    }

    #[test]
    fn diagnostics_forward_shared_solana_pool_without_rpc_reads() {
        struct ObservedRpc;
        #[async_trait]
        impl gofin_solana::SolanaRpc for ObservedRpc {
            fn diagnostics(&self) -> Value {
                json!({"network":"solana:mainnet-beta","endpoints":[],"last_read":null})
            }
            async fn request(&self, _: &str, _: Value) -> gofin_domain::Result<Value> {
                panic!("doctor diagnostics must not issue RPC reads");
            }
        }
        let wallet = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "solana-wallet",
                "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB",
                None,
                Arc::new(ObservedRpc),
                Arc::new(EmptyPrices),
            )
            .unwrap(),
        );
        let router = WalletFamilyRouter::new(
            None,
            BTreeMap::from([
                ("solana-wallet".into(), wallet.clone()),
                ("same-pool".into(), wallet),
            ]),
        );
        assert_eq!(
            router.diagnostics(),
            json!([{
                "network":"solana:mainnet-beta","endpoints":[],"last_read":null
            }])
        );
    }

    struct EmptyPrices;
    #[async_trait]
    impl gofin_pricing::UsdPriceSource for EmptyPrices {
        async fn quote(
            &self,
            _: &gofin_domain::AssetId,
        ) -> gofin_domain::Result<gofin_pricing::UsdQuote> {
            Err(GoFinError::validation(
                "The empty wallet snapshot needs no prices.",
            ))
        }
    }

    struct TestJournal;
    impl SubmissionJournal for TestJournal {
        fn assert_authorized(&self) -> gofin_domain::Result<()> {
            Ok(())
        }
        fn persist_submission(&self, _: &Submission) -> gofin_domain::Result<()> {
            Ok(())
        }
    }

    struct EvmReadRpc {
        chain_id: u64,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl gofin_evm::EvmRpc for EvmReadRpc {
        async fn request(&self, method: &str, _: Value) -> gofin_domain::Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match method {
                "eth_chainId" => Ok(json!(format!("{:#x}", self.chain_id))),
                "eth_blockNumber" => Ok(json!("0x16")),
                "eth_getBlockByNumber" => Ok(json!({
                    "number":"0x16",
                    "hash":format!("0x{:064x}", self.chain_id)
                })),
                "eth_getBalance" => Ok(json!("0x7")),
                "eth_getTransactionCount" => Ok(json!("0x0")),
                _ => Err(GoFinError::validation("Unexpected EVM router test RPC.")),
            }
        }
    }

    fn prepared(network: &str) -> PreparedOperation {
        let now = chrono::Utc::now();
        PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({"network":network,"wallet":"saved-wallet"}),
            binding: json!({"immutable":"original"}),
            summary: "pending transfer".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        }
    }

    #[tokio::test]
    async fn routes_legacy_ethereum_input_and_pending_recovery_from_original_preview() {
        let reconciles = Arc::new(AtomicUsize::new(0));
        let evm: Arc<dyn Adapter> = Arc::new(RoutedAdapter {
            reconciles: reconciles.clone(),
            snapshots: Arc::new(AtomicUsize::new(0)),
            revalidated_submits: Arc::new(AtomicUsize::new(0)),
        });
        let router = WalletFamilyRouter::new(Some(evm), BTreeMap::new());
        for network in ["evm", "ethereum"] {
            assert_eq!(
                router.route(&json!({"network":network})).unwrap() as *const _,
                router.evm.as_deref().unwrap() as *const _
            );
        }
        let original = prepared("ethereum");
        let record = OperationRecord {
            id: "operation-1".into(),
            request_id: "request-1".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: original.operation.clone(),
            input: json!({"network":"solana"}),
            status: gofin_domain::Status::Pending,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            preview: Some(original),
            approval: None,
            authorization_source: None,
            submission: Some(Submission {
                kind: "evm".into(),
                reference: "original-tx".into(),
                data: Value::Null,
            }),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let reconciliation = router.reconcile(&record).await.unwrap();
        assert_eq!(reconciliation.evidence["routed"], "evm");
        assert_eq!(reconciles.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn router_delegates_the_final_revalidated_candidate_without_revalidation() {
        let delegated = Arc::new(AtomicUsize::new(0));
        let evm: Arc<dyn Adapter> = Arc::new(RoutedAdapter {
            reconciles: Arc::new(AtomicUsize::new(0)),
            snapshots: Arc::new(AtomicUsize::new(0)),
            revalidated_submits: delegated.clone(),
        });
        let router = WalletFamilyRouter::new(Some(evm), BTreeMap::new());
        let candidate = prepared("ethereum");
        let validated = gofin_domain::RevalidatedOperation::after_revalidation(
            candidate,
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();

        let submission = router
            .submit_revalidated(&validated, Arc::new(TestJournal))
            .await
            .unwrap();

        assert_eq!(submission.kind, "revalidated-test");
        assert_eq!(delegated.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mixed_wallet_router_partitions_snapshot_scopes_by_family() {
        let evm_snapshots = Arc::new(AtomicUsize::new(0));
        let evm: Arc<dyn Adapter> = Arc::new(RoutedAdapter {
            reconciles: Arc::new(AtomicUsize::new(0)),
            snapshots: evm_snapshots.clone(),
            revalidated_submits: Arc::new(AtomicUsize::new(0)),
        });
        let solana_calls = Arc::new(AtomicUsize::new(0));
        let solana_adapter = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "solana-wallet",
                "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB",
                None,
                Arc::new(EmptySolanaRpc(solana_calls.clone())),
                Arc::new(EmptyPrices),
            )
            .unwrap(),
        );
        let solana_resource = solana_adapter.connections()[0].resource.clone();
        let router = WalletFamilyRouter::new(
            Some(evm),
            BTreeMap::from([("solana-wallet".into(), solana_adapter)]),
        );

        let solana_only = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec!["solana:mainnet-beta".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(solana_only.complete, "{:?}", solana_only.pending);
        assert!(solana_only.coverage.contains(&solana_resource.0));
        assert_eq!(evm_snapshots.load(Ordering::SeqCst), 0);
        assert!(solana_calls.load(Ordering::SeqCst) > 0);

        let evm_only = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec!["ethereum:42161".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(evm_only.complete, "{:?}", evm_only.pending);
        assert_eq!(evm_snapshots.load(Ordering::SeqCst), 1);
        let calls_after_evm = solana_calls.load(Ordering::SeqCst);

        let mixed = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec!["ethereum:42161".into(), "solana:mainnet-beta".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(mixed.complete, "{:?}", mixed.pending);
        assert_eq!(evm_snapshots.load(Ordering::SeqCst), 2);
        assert!(solana_calls.load(Ordering::SeqCst) > calls_after_evm);

        let before_unknown = (
            evm_snapshots.load(Ordering::SeqCst),
            solana_calls.load(Ordering::SeqCst),
        );
        let unknown = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec!["ethereum:999999".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!unknown.complete);
        assert_eq!(
            before_unknown,
            (
                evm_snapshots.load(Ordering::SeqCst),
                solana_calls.load(Ordering::SeqCst)
            ),
            "unknown scopes do not cause unrelated family RPCs"
        );
    }

    #[tokio::test]
    async fn production_evm_group_and_solana_scopes_partition_with_full_coverage() {
        let evm_calls = Arc::new(AtomicUsize::new(0));
        let chain_id = 42161;
        let address = "0x1111111111111111111111111111111111111111";
        let ethereum = Arc::new(
            gofin_ethereum::EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(chain_id, 1, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "shared-wallet".into(),
                    address: address.into(),
                    signing_enabled: false,
                }],
                Arc::new(gofin_evm::EvmRuntime::new(
                    Arc::new(EvmReadRpc {
                        chain_id,
                        calls: evm_calls.clone(),
                    }),
                    chain_id,
                    None,
                )),
            )
            .unwrap(),
        );
        let evm: Arc<dyn Adapter> = Arc::new(super::super::evm_router::EvmNetworkRouter::new(
            BTreeMap::from([(chain_id, ethereum)]),
        ));
        let solana_calls = Arc::new(AtomicUsize::new(0));
        let solana = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "solana-wallet",
                "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB",
                None,
                Arc::new(EmptySolanaRpc(solana_calls.clone())),
                Arc::new(EmptyPrices),
            )
            .unwrap(),
        );
        let solana_resource = solana.connections()[0].resource.clone();
        let evm_resource = ResourceId(format!("evm:{}", address));
        let router = WalletFamilyRouter::new(
            Some(evm),
            BTreeMap::from([("solana-wallet".into(), solana)]),
        );
        for (scope, expected) in [
            (json!({}), true),
            (json!({"networks":["ethereum:42161"]}), true),
            (json!({"networks":["solana:mainnet-beta"]}), true),
            (
                json!({"networks":["ethereum:42161", "solana:mainnet-beta"]}),
                true,
            ),
            (json!({"networks":["unknown"]}), false),
            (json!({"resources":[evm_resource.0]}), true),
            (json!({"resources":[solana_resource.0]}), true),
        ] {
            assert_eq!(
                router.uses_rpc_reads("gofin.snapshot", &scope),
                expected,
                "{scope}"
            );
        }
        assert!(router.uses_rpc_reads(
            "wallet.balances",
            &json!({"network":"ethereum:42161","chain_id":42161,"wallet":"shared-wallet"}),
        ));
        assert!(!router.uses_rpc_reads(
            "wallet.balances",
            &json!({"network":"solana:mainnet-beta","wallet":"solana-wallet"}),
        ));
        assert!(router.uses_rpc_reads(
            "wallet.balances",
            &json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-wallet"}),
        ));
        assert_eq!(evm_calls.load(Ordering::SeqCst), 0);
        assert_eq!(solana_calls.load(Ordering::SeqCst), 0);

        let snapshot_resources = router
            .snapshot_identities()
            .into_iter()
            .map(|identity| identity.resource.0)
            .collect::<BTreeSet<_>>();
        assert!(snapshot_resources.contains(&format!(
            "ethereum:{chain_id}:{}",
            address.to_ascii_lowercase()
        )));
        assert!(snapshot_resources.contains(&evm_resource.0));
        assert!(snapshot_resources.contains(&solana_resource.0));

        let solana_only = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec!["solana:mainnet-beta".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(solana_only.complete, "{:?}", solana_only.pending);
        assert_eq!(evm_calls.load(Ordering::SeqCst), 0);
        assert!(solana_calls.load(Ordering::SeqCst) > 0);

        let before_solana = solana_calls.load(Ordering::SeqCst);
        let evm_only = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec![format!("ethereum:{chain_id}")],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(evm_only.complete, "{:?}", evm_only.pending);
        assert!(evm_calls.load(Ordering::SeqCst) > 0);
        assert_eq!(solana_calls.load(Ordering::SeqCst), before_solana);

        let mixed = router
            .snapshot_scoped(&SnapshotScope {
                networks: vec![format!("ethereum:{chain_id}"), "solana:mainnet-beta".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(mixed.complete, "{:?}", mixed.pending);
        assert!(mixed.coverage.contains(&evm_resource.0));
        assert!(mixed.coverage.contains(&solana_resource.0));

        let full = router
            .snapshot_scoped(&SnapshotScope::default())
            .await
            .unwrap();
        assert!(full.complete, "{:?}", full.pending);
        assert!(full.coverage.contains(&evm_resource.0));
        assert!(full.coverage.contains(&solana_resource.0));

        let evm_resource_scope = router
            .snapshot_scoped(&SnapshotScope {
                resources: vec![ResourceId(format!(
                    "ethereum:{chain_id}:{}",
                    address.to_ascii_lowercase()
                ))],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            evm_resource_scope.complete,
            "{:?}",
            evm_resource_scope.pending
        );
        assert!(evm_resource_scope.coverage.iter().any(|resource| {
            resource == &format!("ethereum:{chain_id}:{}", address.to_ascii_lowercase())
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn rpc_backed_solana_snapshot_and_wallet_read_can_exceed_non_rpc_deadline() {
        struct DelayedSolanaRpc;
        #[async_trait]
        impl gofin_solana::SolanaRpc for DelayedSolanaRpc {
            async fn request(&self, method: &str, _: Value) -> gofin_domain::Result<Value> {
                tokio::time::sleep(Duration::from_secs(4)).await;
                match method {
                    "getBalance" => Ok(json!({"context":{"slot":1},"value":1_000_000_000})),
                    "getEpochInfo" => Ok(json!({"epoch":1})),
                    "getTokenAccountsByOwner" => Ok(json!({"context":{"slot":1},"value":[]})),
                    _ => Err(GoFinError::validation(
                        "Unexpected delayed router test RPC.",
                    )),
                }
            }
        }
        let evm_snapshots = Arc::new(AtomicUsize::new(0));
        let evm: Arc<dyn Adapter> = Arc::new(RoutedAdapter {
            reconciles: Arc::new(AtomicUsize::new(0)),
            snapshots: evm_snapshots.clone(),
            revalidated_submits: Arc::new(AtomicUsize::new(0)),
        });
        let solana = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "solana-wallet",
                "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB",
                None,
                Arc::new(DelayedSolanaRpc),
                Arc::new(EmptyPrices),
            )
            .unwrap(),
        );
        let router = WalletFamilyRouter::new(
            Some(evm),
            BTreeMap::from([("solana-wallet".into(), solana)]),
        );
        let snapshot = router.snapshot().await.unwrap();
        assert!(snapshot.complete, "{:?}", snapshot.pending);
        assert_eq!(evm_snapshots.load(Ordering::SeqCst), 1);
        assert!(router.uses_rpc_reads(
            "wallet.balances",
            &json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-wallet"}),
        ));
        assert!(!router.uses_rpc_reads(
            "wallet.balances",
            &json!({"network":"solana","cluster":"devnet","wallet":"solana-wallet"}),
        ));
        let read = tokio::time::timeout(
            Duration::from_secs(17),
            router.read(
                "wallet.balances",
                &json!({"network":"solana","cluster":"mainnet-beta","wallet":"solana-wallet"}),
            ),
        )
        .await
        .expect("the three serialized RPC reads finish after the former ten-second limit")
        .unwrap();
        assert_eq!(read["native"]["amount_atoms"], "1000000000");
    }

    #[tokio::test]
    async fn cancelling_solana_router_snapshot_drops_active_rpc_without_followup_calls() {
        struct BlockingRpc {
            calls: Arc<AtomicUsize>,
            entered: tokio::sync::Notify,
            dropped: Arc<AtomicUsize>,
        }
        struct DropGuard(Arc<AtomicUsize>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        #[async_trait]
        impl gofin_solana::SolanaRpc for BlockingRpc {
            async fn request(&self, _: &str, _: Value) -> gofin_domain::Result<Value> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _guard = DropGuard(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending().await
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let rpc = Arc::new(BlockingRpc {
            calls: calls.clone(),
            entered: tokio::sync::Notify::new(),
            dropped: dropped.clone(),
        });
        let adapter = Arc::new(
            gofin_solana::SolanaWalletAdapter::new(
                "solana-wallet",
                "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB",
                None,
                rpc.clone(),
                Arc::new(EmptyPrices),
            )
            .unwrap(),
        );
        let router = Arc::new(WalletFamilyRouter::new(
            None,
            BTreeMap::from([("solana-wallet".into(), adapter)]),
        ));
        let scope = SnapshotScope {
            networks: vec!["solana:mainnet-beta".into()],
            ..Default::default()
        };
        assert!(router.uses_rpc_reads("gofin.snapshot", &json!(scope)));

        let snapshot_router = router.clone();
        let snapshot_scope = scope.clone();
        let snapshot =
            tokio::spawn(async move { snapshot_router.snapshot_scoped(&snapshot_scope).await });
        rpc.entered.notified().await;
        snapshot.abort();
        assert!(snapshot.await.unwrap_err().is_cancelled());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn merged_wallet_semantics_keep_common_metadata_outside_provider_variants() {
        let definition = |semantics: Value| Definition {
            name: "wallet.balances".into(),
            description: "wallet balances".into(),
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentReadable,
            input_schema: json!({"type":"object","properties":{"network":{"const":"ethereum:1"}}}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "bounded".into(),
            completion: "finalized".into(),
            semantics,
        };
        let merged = merge_wallet_definitions(vec![
            definition(json!({"authority_expiry":"not_applicable","network":"ethereum:1"})),
            definition(
                json!({"authority_expiry":"not_applicable","network":"solana:mainnet-beta"}),
            ),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].semantics["authority_expiry"], "not_applicable");
        assert_eq!(
            merged[0].semantics["provider_variants"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(merged[0].semantics.get("anyOf").is_none());
    }
}
