//! Route shared operation names using the explicit immutable chain identity.
use super::*;
use fin_domain::{PreparationContext, ReturnRouteContext, ReturnRouteProof};

pub(super) struct EvmNetworkRouter {
    adapters: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
}
impl EvmNetworkRouter {
    pub fn new(adapters: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>) -> Self {
        Self { adapters }
    }
    fn route(&self, input: &Value) -> fin_domain::Result<&Arc<fin_ethereum::EthereumAdapter>> {
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
    ) -> fin_domain::Result<&Arc<fin_ethereum::EthereumAdapter>> {
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
    fn connections(&self) -> Vec<fin_domain::ConnectionMetadata> {
        let mut connections = BTreeMap::<String, fin_domain::ConnectionMetadata>::new();
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
    fn authorization_identities(&self) -> Vec<fin_domain::ConnectionMetadata> {
        self.adapters
            .values()
            .flat_map(|adapter| adapter.authorization_identities())
            .collect()
    }
    async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
        let mut combined = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        };
        for adapter in self.adapters.values() {
            let snapshot = adapter.snapshot().await?;
            combined.complete &= snapshot.complete;
            combined.coverage.extend(snapshot.coverage);
            combined.pending.extend(snapshot.pending);
            combined.state.holdings.extend(snapshot.state.holdings);
            combined.state.positions.extend(snapshot.state.positions);
            combined
                .state
                .authorities
                .extend(snapshot.state.authorities);
            combined.state.in_flight.extend(snapshot.state.in_flight);
        }
        Ok(combined)
    }
    async fn read(&self, operation: &str, input: &Value) -> fin_domain::Result<Value> {
        self.route(input)?.read(operation, input).await
    }
    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> fin_domain::Result<PreparedOperation> {
        self.route(input)?.prepare(operation, input).await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> fin_domain::Result<PreparedOperation> {
        self.route(input)?
            .prepare_with_context(operation, input, context)
            .await
    }
    async fn preflight_return_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> fin_domain::Result<ReturnRouteProof> {
        self.route(input)?
            .preflight_return_route(operation, input, context)
            .await
    }
    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> fin_domain::Result<PreparedOperation> {
        self.prepared(prepared)?.revalidate(prepared).await
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> fin_domain::Result<Submission> {
        self.prepared(prepared)?.submit(prepared, journal).await
    }
    async fn reconcile(&self, record: &OperationRecord) -> fin_domain::Result<Reconciliation> {
        self.prepared(
            record
                .preview
                .as_ref()
                .ok_or_else(|| FinError::validation("The original preview is missing."))?,
        )?
        .reconcile(record)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ReadRpc(u64);
    struct NoCredentialAccess;
    impl fin_exchange::ccxt::CredentialProvider for NoCredentialAccess {
        fn credentials(
            &self,
            _: &str,
        ) -> std::result::Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError>
        {
            panic!("read-only wallet composition must not obtain exchange credentials")
        }
    }
    impl BrokerCredentialProvider for NoCredentialAccess {
        fn envelope_signer(
            &self,
            _: &fin_credentials::CredentialRef,
        ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
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
    fn broker_composes_one_wallet_on_all_reviewed_networks_without_protocols_or_keys() {
        let mut config = shared_config();
        config.validate().unwrap();
        let applications =
            build_protected_applications(&config, Arc::new(NoCredentialAccess)).unwrap();
        assert_eq!(applications.adapters.len(), 3);
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
        assert_eq!(ids, [1, 42161, 4663].into_iter().collect());
        assert!(
            !evm.definitions()
                .iter()
                .any(|definition| definition.name.starts_with("aave.")
                    || definition.name.starts_with("uniswap."))
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
    impl fin_evm::EvmRpc for ReadRpc {
        async fn request(&self, method: &str, _: Value) -> fin_domain::Result<Value> {
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
                    let adapter = fin_ethereum::EthereumAdapter::new(
                        fin_ethereum::EthereumConfig::wallet_only(chain, 2, vec![]),
                        vec![fin_ethereum::WalletConnection {
                            name: "synthetic".into(),
                            address: "0x1111111111111111111111111111111111111111".into(),
                            signing_enabled: false,
                        }],
                        Arc::new(fin_evm::EvmRuntime::new(
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
        assert_eq!(definitions.len(), 5);
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
        };
        assert_eq!(
            router.prepared(&prepared).err().unwrap().code,
            "BINDING_CHAIN_MISMATCH"
        );
        prepared.input["chain_id"] = json!(4663);
        assert_eq!(router.prepared(&prepared).unwrap().config.chain_id, 4663);
    }
}
