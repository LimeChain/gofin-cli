//! Protected Kraken composition. No public request selects a credential or nonce DB.
use crate::{BrokerConfigV2, BrokerCredentialProvider, ConnectionConfig};
use gofin_domain::{Adapter, GoFinError, Result};
use gofin_exchange::kraken::{KrakenClient, NonceAllocator};
use std::{path::Path, sync::Arc};

pub struct DurableKrakenNonces(gofin_store::nonces::NonceStore);
impl DurableKrakenNonces {
    pub fn open(path: &Path) -> Result<Self> {
        gofin_store::nonces::NonceStore::open(path)
            .map(Self)
            .map_err(|_| {
                GoFinError::new(
                    "KRAKEN_NONCE_STORE_UNAVAILABLE",
                    "storage",
                    "The protected Kraken nonce store could not open safely.",
                    "Inspect its ownership and permissions before using the original credentials.",
                )
            })
    }
}
impl NonceAllocator for DurableKrakenNonces {
    fn allocate(
        &self,
        digest: &str,
        clock: u64,
    ) -> std::result::Result<u64, gofin_exchange::ccxt::CcxtError> {
        self.0.allocate(digest, clock).map_err(|_| {
            gofin_exchange::ccxt::CcxtError::new(gofin_exchange::ccxt::CcxtErrorKind::Unavailable)
        })
    }
}
struct PublicOnlyNonces;
impl NonceAllocator for PublicOnlyNonces {
    fn allocate(
        &self,
        _: &str,
        _: u64,
    ) -> std::result::Result<u64, gofin_exchange::ccxt::CcxtError> {
        Err(gofin_exchange::ccxt::CcxtError::new(
            gofin_exchange::ccxt::CcxtErrorKind::PermissionDenied,
        ))
    }
}
pub fn profiles(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    prices: Option<Arc<dyn gofin_pricing::UsdPriceSource>>,
    recipient: Option<Arc<dyn gofin_kraken::funding::KrakenRecipientVerifier>>,
) -> Result<Vec<gofin_kraken::KrakenProfile>> {
    use gofin_kraken::{KrakenAccount, KrakenCcxtTransport, KrakenProduct, KrakenProfile};
    let has_accounts = config.connections.iter().any(|c| {
        matches!(
            c,
            ConnectionConfig::Kraken { .. } | ConnectionConfig::KrakenFutures { .. }
        )
    });
    let nonces: Arc<dyn NonceAllocator> = if has_accounts {
        credentials.kraken_nonces()?
    } else {
        Arc::new(PublicOnlyNonces)
    };
    let client = KrakenClient::new(nonces)
        .map_err(|_| GoFinError::validation("Kraken transport could not initialize."))?;
    let transport = Arc::new(KrakenCcxtTransport::new(client, credentials));
    let mut profiles = vec![];
    for product in [KrakenProduct::Spot, KrakenProduct::Derivatives] {
        let accounts = config
            .connections
            .iter()
            .filter_map(|c| {
                let (
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    verified,
                    default_wallet_id,
                ) = match (c, product) {
                    (
                        ConnectionConfig::Kraken {
                            name,
                            resource,
                            credential_ref,
                            physical_identity,
                            physical_identity_verified,
                            default_wallet_account_id,
                        },
                        KrakenProduct::Spot,
                    ) => (
                        name,
                        resource,
                        credential_ref,
                        physical_identity,
                        *physical_identity_verified,
                        default_wallet_account_id.clone(),
                    ),
                    (
                        ConnectionConfig::KrakenFutures {
                            name,
                            resource,
                            credential_ref,
                            physical_identity,
                            physical_identity_verified,
                        },
                        KrakenProduct::Derivatives,
                    ) => (
                        name,
                        resource,
                        credential_ref,
                        physical_identity,
                        *physical_identity_verified,
                        None,
                    ),
                    _ => return None,
                };
                Some(KrakenAccount {
                    name: name.clone(),
                    resource: resource.clone(),
                    credential_ref: credential_ref.clone(),
                    physical_identity: physical_identity.clone(),
                    wallet_account_id: None,
                    default_wallet_account_id: default_wallet_id,
                    identity_verified: verified,
                    product,
                })
            })
            .collect();
        profiles.push(KrakenProfile::new(product, accounts, transport.clone())?);
    }
    let related = config
        .connections
        .iter()
        .filter_map(|c| match c {
            ConnectionConfig::Kraken {
                name,
                resource,
                credential_ref,
                physical_identity,
                physical_identity_verified,
                default_wallet_account_id,
            } => Some(KrakenAccount {
                name: name.clone(),
                resource: resource.clone(),
                credential_ref: credential_ref.clone(),
                physical_identity: physical_identity.clone(),
                wallet_account_id: None,
                default_wallet_account_id: default_wallet_account_id.clone(),
                identity_verified: *physical_identity_verified,
                product: KrakenProduct::Spot,
            }),
            ConnectionConfig::KrakenFutures {
                name,
                resource,
                credential_ref,
                physical_identity,
                physical_identity_verified,
            } => Some(KrakenAccount {
                name: name.clone(),
                resource: resource.clone(),
                credential_ref: credential_ref.clone(),
                physical_identity: physical_identity.clone(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: *physical_identity_verified,
                product: KrakenProduct::Derivatives,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(profiles
        .into_iter()
        .map(|p| {
            let mut p = p.with_related_accounts(related.clone());
            if let Some(source) = prices.clone() {
                p = p.with_prices(source);
            }
            if let Some(verifier) = recipient.clone() {
                p = p.with_recipient_verifier(verifier);
            }
            p
        })
        .collect())
}

pub(crate) fn adapters(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
    ethereum: std::collections::BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
    solana: std::collections::BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
) -> Result<Vec<Arc<dyn Adapter>>> {
    let recipient: Option<Arc<dyn gofin_kraken::funding::KrakenRecipientVerifier>> =
        if ethereum.is_empty() && solana.is_empty() {
            None
        } else {
            Some(Arc::new(KrakenEvmRecipientVerifier::with_solana(
                config,
                ethereum.clone(),
                solana.clone(),
            )?))
        };
    let mut adapters = vec![];
    for profile in profiles(config, credentials, Some(prices), recipient)? {
        let profile = Arc::new(profile);
        if gofin_exchange::ExchangeProfile::connections(profile.as_ref())
            .iter()
            .any(|c| c.provider == "kraken")
            && (!ethereum.is_empty() || !solana.is_empty())
        {
            adapters.push(
                Arc::new(crate::kraken_deposit::KrakenEvmDepositAdapter::with_solana(
                    profile.clone(),
                    ethereum.clone(),
                    solana.clone(),
                )) as Arc<dyn Adapter>,
            );
        }
        adapters.push(
            Arc::new(gofin_exchange::ExchangeAdapter::new((*profile).clone())) as Arc<dyn Adapter>,
        );
    }
    Ok(adapters)
}

/// Reuses the canonical EVM observer. Kraken metadata supplies an exact route;
/// configured custody controls whether a received credit enters GoFin accounting.
pub(crate) struct KrakenEvmRecipientVerifier {
    observer: Option<crate::EthereumPayoutVerifier>,
    destinations: Vec<gofin_binance::EthereumDestination>,
    solana: std::collections::BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
}
impl KrakenEvmRecipientVerifier {
    pub(crate) fn with_solana(
        config: &BrokerConfigV2,
        ethereum: std::collections::BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        solana: std::collections::BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
    ) -> Result<Self> {
        let observer = if ethereum.is_empty() {
            None
        } else {
            Some(crate::EthereumPayoutVerifier::new(ethereum)?)
        };
        Ok(Self {
            observer,
            destinations: crate::configured_binance_destinations(config)?,
            solana,
        })
    }
}
/// Resolve a Kraken label against GoFin's reviewed network registry. Kraken's
/// Funding API exposes opaque network IDs and names, not EVM chain IDs, so
/// normalization is deliberately limited to punctuation and conventional
/// transport suffixes. Short aliases are accepted only when they identify one
/// reviewed network.
pub(crate) fn reviewed_chain(network: &str) -> Option<u64> {
    fn normalized(value: &str) -> String {
        value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .map(|character| character.to_ascii_lowercase())
            .collect()
    }
    fn canonical(value: &str) -> String {
        let mut value = normalized(value);
        for suffix in ["mainnet", "erc20", "eth", "hex"] {
            if value.len() > suffix.len()
                && let Some(prefix) = value.strip_suffix(suffix)
            {
                value = prefix.to_owned();
                break;
            }
        }
        if value == "ether" {
            "ethereum".into()
        } else {
            value
        }
    }

    let requested = canonical(network);
    if requested.is_empty() {
        return None;
    }
    let mut matches = gofin_networks::all()
        .iter()
        .filter(|candidate| {
            let name = canonical(candidate.display_name);
            name == requested || (requested.len() >= 6 && name.starts_with(&requested))
        })
        .map(|candidate| candidate.chain_id);
    let chain_id = matches.next()?;
    if matches.any(|candidate| candidate != chain_id) {
        return None;
    }
    Some(chain_id)
}

#[async_trait::async_trait]
impl gofin_kraken::funding::KrakenRecipientVerifier for KrakenEvmRecipientVerifier {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<gofin_kraken::funding::KrakenDestination>> {
        use gofin_binance::BinanceRecipientVerifier;
        if reviewed_solana_network(network) {
            let runtime = self.solana.values().next().map(|wallet| wallet.runtime());
            let Some(runtime) = runtime else {
                return Ok(None);
            };
            let asset =
                gofin_solana::funding::resolve_external_asset(&runtime, coin, contract).await?;
            if asset.reference != "native"
                && (coin != "USDC" || asset.reference != gofin_solana::funding::MAINNET_USDC_MINT)
            {
                return Ok(None);
            }
            if let Some(mint) = contract.filter(|mint| !mint.is_empty())
                && asset.reference != mint
            {
                return Ok(None);
            }
            let matches = self
                .solana
                .values()
                .filter(|wallet| {
                    wallet.address() == address
                        && destination_name.is_none_or(|name| {
                            wallet
                                .connections()
                                .iter()
                                .any(|c| c.name == name || c.resource.0 == name)
                        })
                })
                .collect::<Vec<_>>();
            if matches.len() > 1 || (destination_name.is_some() && matches.is_empty()) {
                return Ok(None);
            }
            let destination = matches.first().copied();
            let address = gofin_solana::parse_address(address)?;
            return Ok(Some(gofin_kraken::funding::KrakenDestination {
                enrolled: destination.is_some(),
                resource: destination
                    .map(|wallet| {
                        gofin_domain::ResourceId(format!(
                            "solana:mainnet-beta:{}",
                            wallet.address()
                        ))
                    })
                    .unwrap_or_else(|| {
                        gofin_domain::ResourceId(format!("external:solana:mainnet-beta:{address}"))
                    }),
                address,
                chain_id: 0,
                network: Some(gofin_solana::NETWORK.into()),
                cluster: Some(gofin_solana::CLUSTER.into()),
                mint: (asset.reference != "native").then(|| asset.reference.clone()),
                asset,
                confirmations: 1,
            }));
        }
        let Some(chain) = reviewed_chain(network) else {
            return Ok(None);
        };
        let Some(observer) = &self.observer else {
            return Ok(None);
        };
        let matches = self
            .destinations
            .iter()
            .filter(|d| {
                d.chain_id == chain
                    && d.address.eq_ignore_ascii_case(address)
                    && destination_name.is_none_or(|name| d.name == name || d.resource.0 == name)
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 || (destination_name.is_some() && matches.is_empty()) {
            return Ok(None);
        }
        let destination = matches.first().copied();
        let canonical = if coin == "XETH" { "ETH" } else { coin };
        let Some(asset) = observer.resolve_asset(chain, canonical, contract).await? else {
            return Ok(None);
        };
        Ok(Some(gofin_kraken::funding::KrakenDestination {
            enrolled: destination.is_some(),
            resource: destination.map(|d| d.resource.clone()).unwrap_or_else(|| {
                gofin_domain::ResourceId(format!(
                    "external:ethereum:{chain}:{}",
                    address.to_ascii_lowercase()
                ))
            }),
            address: address.to_owned(),
            chain_id: chain,
            network: Some(format!("ethereum:{chain}")),
            cluster: None,
            mint: None,
            asset,
            confirmations: destination.map(|d| d.confirmations).unwrap_or(1),
        }))
    }
    async fn verify(
        &self,
        r: gofin_kraken::funding::KrakenPayoutRequest,
    ) -> Result<gofin_kraken::funding::KrakenPayoutResult> {
        use gofin_binance::BinanceRecipientVerifier;
        if r.destination.asset.network == gofin_solana::NETWORK {
            if r.destination.network.as_deref() != Some(gofin_solana::NETWORK)
                || r.destination.cluster.as_deref() != Some(gofin_solana::CLUSTER)
                || r.destination.mint.as_deref()
                    != (r.destination.asset.reference != "native")
                        .then_some(r.destination.asset.reference.as_str())
            {
                return Err(GoFinError::validation(
                    "The original Solana destination binding is incomplete or inconsistent.",
                ));
            }
            let Some(runtime) = self.solana.values().next().map(|wallet| wallet.runtime()) else {
                return Err(GoFinError::validation(
                    "No Solana observer is configured for payout verification.",
                ));
            };
            let minimum = solana_atoms(&r.minimum, r.destination.asset.decimals)?;
            let maximum = solana_atoms(&r.maximum, r.destination.asset.decimals)?;
            let request = gofin_solana::funding::ExternalPayoutRequest {
                signature: r.tx_hash,
                asset: r.destination.asset.clone(),
                recipient: r.destination.address.clone(),
                recipient_owner: None,
                source_owner: None,
                amount: gofin_solana::funding::AmountBounds {
                    minimum_atoms: minimum,
                    maximum_atoms: maximum,
                },
            };
            return match runtime.verify_external_payout(&request).await? {
                gofin_solana::funding::ExternalPayoutStatus::Finalized { evidence } => {
                    Ok(gofin_kraken::funding::KrakenPayoutResult {
                        status: gofin_domain::Status::Completed,
                        amount: Some(gofin_domain::Quantity::from_u64(evidence.credited_atoms)),
                        evidence: serde_json::to_value(evidence).unwrap_or(serde_json::Value::Null),
                    })
                }
                gofin_solana::funding::ExternalPayoutStatus::Pending { reason } => {
                    Ok(gofin_kraken::funding::KrakenPayoutResult {
                        status: gofin_domain::Status::Pending,
                        amount: None,
                        evidence: serde_json::json!({"state":"pending","reason":reason}),
                    })
                }
            };
        }
        let observer = self.observer.as_ref().ok_or_else(|| {
            GoFinError::validation("No EVM observer is configured for payout verification.")
        })?;
        let p = observer
            .verify(gofin_binance::PayoutRequest {
                tx_hash: r.tx_hash,
                network: r.destination.asset.network.clone(),
                memo: None,
                chain_id: r.destination.chain_id,
                recipient: r.destination.address,
                asset: r.destination.asset,
                minimum_amount: r.minimum,
                maximum_amount: r.maximum,
                confirmations: r.destination.confirmations,
            })
            .await;
        Ok(gofin_kraken::funding::KrakenPayoutResult {
            status: p.status,
            amount: p.amount_atoms,
            evidence: p.evidence,
        })
    }
}

pub(crate) fn reviewed_solana_network(network: &str) -> bool {
    let normalized: String = network
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "sol" | "solana" | "solanamainnet" | "solanamainnetbeta"
    )
}

fn solana_atoms(value: &str, decimals: u8) -> Result<u64> {
    gofin_domain::Quantity::from_decimal(value, decimals)?
        .as_str()
        .parse()
        .map_err(|_| GoFinError::validation("Solana payout bound exceeds supported precision."))
}

#[cfg(test)]
mod network_resolution_tests {
    use super::reviewed_chain;

    #[test]
    fn reviewed_solana_network_is_explicit_and_narrow() {
        assert!(super::reviewed_solana_network("Solana"));
        assert!(super::reviewed_solana_network("Solana Mainnet Beta"));
        assert!(super::reviewed_solana_network("SOL"));
        assert!(!super::reviewed_solana_network("BNB Smart Chain"));
        assert!(!super::reviewed_solana_network("Solana Devnet"));
    }

    #[tokio::test]
    async fn kraken_solana_funding_verifies_exact_finalized_usdc_payout_for_enrolled_recipient() {
        use gofin_kraken::funding::KrakenRecipientVerifier;
        use std::collections::BTreeMap;

        let fixture = crate::test_support::solana::SolanaFundingFixture::new(
            "kraken-solana-recipient",
            1_000_000_000,
            0,
        );
        let recipient = fixture.signer_address();
        let wallet = fixture.wallet();
        let verifier = super::KrakenEvmRecipientVerifier::with_solana(
            &crate::BrokerConfigV2 {
                version: 2,
                networks: vec![],
                deployments: vec![],
                connections: vec![],
                policy: crate::BrokerPolicy::beta(),
                approver_uid: 501,
                extensions: BTreeMap::new(),
            },
            BTreeMap::new(),
            BTreeMap::from([("kraken-solana-recipient".into(), wallet)]),
        )
        .unwrap();
        let destination = verifier
            .resolve(
                "Solana",
                "USDC",
                Some(crate::test_support::solana::USDC_MINT),
                &recipient,
                Some("kraken-solana-recipient"),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(destination.enrolled);
        assert_eq!(
            destination.asset.reference,
            crate::test_support::solana::USDC_MINT
        );
        assert_eq!(destination.network.as_deref(), Some(gofin_solana::NETWORK));
        assert_eq!(destination.cluster.as_deref(), Some(gofin_solana::CLUSTER));
        assert_eq!(
            destination.mint.as_deref(),
            Some(crate::test_support::solana::USDC_MINT)
        );
        assert!(
            verifier
                .resolve(
                    "Solana Devnet",
                    "USDC",
                    Some(crate::test_support::solana::USDC_MINT),
                    &recipient,
                    Some("kraken-solana-recipient")
                )
                .await
                .unwrap()
                .is_none()
        );
        let signature = fixture
            .inject_finalized_transfer(
                crate::test_support::solana::USDC_MINT,
                &recipient,
                1_000_000,
            )
            .unwrap();
        let result = verifier
            .verify(gofin_kraken::funding::KrakenPayoutRequest {
                destination,
                tx_hash: signature,
                minimum: "1".into(),
                maximum: "1".into(),
            })
            .await
            .unwrap();
        assert_eq!(result.status, gofin_domain::Status::Completed);
        assert_eq!(
            result.amount,
            Some(gofin_domain::Quantity::from_u64(1_000_000))
        );
    }

    #[tokio::test]
    async fn recipient_resource_and_name_resolve_the_same_enrolled_custody() {
        use gofin_kraken::funding::KrakenRecipientVerifier;
        use std::{collections::BTreeMap, sync::Arc};

        struct ChainRpc;
        #[async_trait::async_trait]
        impl gofin_evm::EvmRpc for ChainRpc {
            async fn request(
                &self,
                method: &str,
                _: serde_json::Value,
            ) -> gofin_domain::Result<serde_json::Value> {
                assert_eq!(method, "eth_chainId");
                Ok(serde_json::json!("0x1"))
            }
        }
        let address = "0x1111111111111111111111111111111111111111";
        let resource = format!("ethereum:1:{address}");
        let adapter = Arc::new(
            gofin_ethereum::EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(1, 2, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "wallet".into(),
                    address: address.into(),
                    signing_enabled: false,
                }],
                Arc::new(gofin_evm::EvmRuntime::new(Arc::new(ChainRpc), 1, None)),
            )
            .unwrap(),
        );
        let mut verifier = super::KrakenEvmRecipientVerifier {
            observer: Some(
                crate::EthereumPayoutVerifier::new(BTreeMap::from([(1, adapter)])).unwrap(),
            ),
            destinations: vec![gofin_binance::EthereumDestination {
                name: "wallet".into(),
                resource: gofin_domain::ResourceId(resource.clone()),
                address: address.into(),
                chain_id: 1,
                assets: BTreeMap::new(),
                confirmations: 2,
            }],
            solana: std::collections::BTreeMap::new(),
        };
        let named = verifier
            .resolve("Ether", "ETH", None, address, Some("wallet"))
            .await
            .unwrap()
            .unwrap();
        let exact = verifier
            .resolve("Ethereum", "XETH", None, address, Some(&resource))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(named, exact);
        assert!(exact.enrolled);
        assert_eq!(exact.resource.0, resource);
        assert_eq!(exact.confirmations, 2);
        assert!(
            verifier
                .resolve(
                    "Ethereum",
                    "ETH",
                    None,
                    address,
                    Some(&resource.to_uppercase())
                )
                .await
                .unwrap()
                .is_none()
        );
        for (network, recipient, selector) in [
            ("Base", address, resource.as_str()),
            (
                "Ethereum",
                "0x2222222222222222222222222222222222222222",
                resource.as_str(),
            ),
            ("Ethereum", address, "unknown-wallet"),
            (
                "Ethereum",
                address,
                "ethereum:8453:0x1111111111111111111111111111111111111111",
            ),
        ] {
            assert!(
                verifier
                    .resolve(network, "ETH", None, recipient, Some(selector))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let external = verifier
            .resolve(
                "Ethereum",
                "ETH",
                None,
                "0x2222222222222222222222222222222222222222",
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!external.enrolled);
        verifier.destinations.push(verifier.destinations[0].clone());
        assert!(
            verifier
                .resolve("Ethereum", "ETH", None, address, Some(&resource))
                .await
                .unwrap()
                .is_none()
        );
        // A name on one enrolled row must not override a resource match on another.
        verifier.destinations[1].name = resource.clone();
        verifier.destinations[1].resource = gofin_domain::ResourceId("legacy-custody".into());
        assert!(
            verifier
                .resolve("Ethereum", "ETH", None, address, Some(&resource))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolves_kraken_names_from_the_reviewed_network_registry() {
        for network in gofin_networks::all() {
            assert_eq!(
                reviewed_chain(network.display_name),
                Some(network.chain_id),
                "{}",
                network.display_name
            );
        }
        assert_eq!(reviewed_chain("Base"), Some(8453));
        assert_eq!(reviewed_chain("Ethereum (ERC20)"), Some(1));
        assert_eq!(reviewed_chain("Ether (Hex)"), Some(1));
        assert_eq!(reviewed_chain("Ethereum Mainnet"), Some(1));
        assert_eq!(reviewed_chain("Arbitrum (ETH)"), Some(42161));
        assert_eq!(reviewed_chain("Unknown EVM network"), None);
        assert_eq!(reviewed_chain(""), None);
    }
}
