//! Protected Kraken composition. No public request selects a credential or nonce DB.
use crate::{BrokerConfigV2, BrokerCredentialProvider, ConnectionConfig};
use fin_domain::{Adapter, FinError, Result};
use fin_exchange::kraken::{KrakenClient, NonceAllocator};
use std::{path::Path, sync::Arc};

pub struct DurableKrakenNonces(fin_store::nonces::NonceStore);
impl DurableKrakenNonces {
    pub fn open(path: &Path) -> Result<Self> {
        fin_store::nonces::NonceStore::open(path)
            .map(Self)
            .map_err(|_| {
                FinError::new(
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
    ) -> std::result::Result<u64, fin_exchange::ccxt::CcxtError> {
        self.0.allocate(digest, clock).map_err(|_| {
            fin_exchange::ccxt::CcxtError::new(fin_exchange::ccxt::CcxtErrorKind::Unavailable)
        })
    }
}
struct PublicOnlyNonces;
impl NonceAllocator for PublicOnlyNonces {
    fn allocate(&self, _: &str, _: u64) -> std::result::Result<u64, fin_exchange::ccxt::CcxtError> {
        Err(fin_exchange::ccxt::CcxtError::new(
            fin_exchange::ccxt::CcxtErrorKind::PermissionDenied,
        ))
    }
}
pub fn profiles(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    prices: Option<Arc<dyn fin_pricing::UsdPriceSource>>,
    recipient: Option<Arc<dyn fin_kraken::funding::KrakenRecipientVerifier>>,
) -> Result<Vec<fin_kraken::KrakenProfile>> {
    use fin_kraken::{KrakenAccount, KrakenCcxtTransport, KrakenProduct, KrakenProfile};
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
        .map_err(|_| FinError::validation("Kraken transport could not initialize."))?;
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
    prices: Arc<dyn fin_pricing::UsdPriceSource>,
    ethereum: std::collections::BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
) -> Result<Vec<Arc<dyn Adapter>>> {
    let recipient: Option<Arc<dyn fin_kraken::funding::KrakenRecipientVerifier>> =
        if ethereum.is_empty() {
            None
        } else {
            Some(Arc::new(KrakenEvmRecipientVerifier::new(
                config,
                ethereum.clone(),
            )?))
        };
    let mut adapters = vec![];
    for profile in profiles(config, credentials, Some(prices), recipient)? {
        let profile = Arc::new(profile);
        if fin_exchange::ExchangeProfile::connections(profile.as_ref())
            .iter()
            .any(|c| c.provider == "kraken")
            && !ethereum.is_empty()
        {
            adapters.push(
                Arc::new(crate::kraken_deposit::KrakenEvmDepositAdapter::new(
                    profile.clone(),
                    ethereum.clone(),
                )) as Arc<dyn Adapter>,
            );
        }
        adapters.push(
            Arc::new(fin_exchange::ExchangeAdapter::new((*profile).clone())) as Arc<dyn Adapter>,
        );
    }
    Ok(adapters)
}

/// Reuses the canonical EVM observer. Kraken metadata supplies an exact route;
/// configured custody controls whether a received credit enters Fin accounting.
pub(crate) struct KrakenEvmRecipientVerifier {
    observer: crate::EthereumPayoutVerifier,
    destinations: Vec<fin_binance::EthereumDestination>,
}
impl KrakenEvmRecipientVerifier {
    pub(crate) fn new(
        config: &BrokerConfigV2,
        ethereum: std::collections::BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
    ) -> Result<Self> {
        Ok(Self {
            observer: crate::EthereumPayoutVerifier::new(ethereum)?,
            destinations: crate::configured_binance_destinations(config)?,
        })
    }
}
/// Resolve a Kraken label against Fin's reviewed network registry. Kraken's
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
    let mut matches = fin_networks::all()
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
impl fin_kraken::funding::KrakenRecipientVerifier for KrakenEvmRecipientVerifier {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<fin_kraken::funding::KrakenDestination>> {
        use fin_binance::BinanceRecipientVerifier;
        let Some(chain) = reviewed_chain(network) else {
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
        let Some(asset) = self
            .observer
            .resolve_asset(chain, canonical, contract)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(fin_kraken::funding::KrakenDestination {
            enrolled: destination.is_some(),
            resource: destination.map(|d| d.resource.clone()).unwrap_or_else(|| {
                fin_domain::ResourceId(format!(
                    "external:ethereum:{chain}:{}",
                    address.to_ascii_lowercase()
                ))
            }),
            address: address.to_owned(),
            chain_id: chain,
            asset,
            confirmations: destination.map(|d| d.confirmations).unwrap_or(1),
        }))
    }
    async fn verify(
        &self,
        r: fin_kraken::funding::KrakenPayoutRequest,
    ) -> Result<fin_kraken::funding::KrakenPayoutResult> {
        use fin_binance::BinanceRecipientVerifier;
        let p = self
            .observer
            .verify(fin_binance::PayoutRequest {
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
        Ok(fin_kraken::funding::KrakenPayoutResult {
            status: p.status,
            amount: p.amount_atoms,
            evidence: p.evidence,
        })
    }
}

#[cfg(test)]
mod network_resolution_tests {
    use super::reviewed_chain;

    #[tokio::test]
    async fn recipient_resource_and_name_resolve_the_same_enrolled_custody() {
        use fin_kraken::funding::KrakenRecipientVerifier;
        use std::{collections::BTreeMap, sync::Arc};

        struct ChainRpc;
        #[async_trait::async_trait]
        impl fin_evm::EvmRpc for ChainRpc {
            async fn request(
                &self,
                method: &str,
                _: serde_json::Value,
            ) -> fin_domain::Result<serde_json::Value> {
                assert_eq!(method, "eth_chainId");
                Ok(serde_json::json!("0x1"))
            }
        }
        let address = "0x1111111111111111111111111111111111111111";
        let resource = format!("ethereum:1:{address}");
        let adapter = Arc::new(
            fin_ethereum::EthereumAdapter::new(
                fin_ethereum::EthereumConfig::wallet_only(1, 2, vec![]),
                vec![fin_ethereum::WalletConnection {
                    name: "wallet".into(),
                    address: address.into(),
                    signing_enabled: false,
                }],
                Arc::new(fin_evm::EvmRuntime::new(Arc::new(ChainRpc), 1, None)),
            )
            .unwrap(),
        );
        let mut verifier = super::KrakenEvmRecipientVerifier {
            observer: crate::EthereumPayoutVerifier::new(BTreeMap::from([(1, adapter)])).unwrap(),
            destinations: vec![fin_binance::EthereumDestination {
                name: "wallet".into(),
                resource: fin_domain::ResourceId(resource.clone()),
                address: address.into(),
                chain_id: 1,
                assets: BTreeMap::new(),
                confirmations: 2,
            }],
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
        verifier.destinations[1].resource = fin_domain::ResourceId("legacy-custody".into());
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
        for network in fin_networks::all() {
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
