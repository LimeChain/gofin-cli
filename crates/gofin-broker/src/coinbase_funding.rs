//! Broker-owned receipt verification for exact Coinbase withdrawal destinations.
use crate::{BrokerConfigV2, EthereumPayoutVerifier, solana_cex::SolanaCexPayoutVerifier};
use async_trait::async_trait;
use gofin_binance::BinanceRecipientVerifier;
use gofin_coinbase::funding::{
    CoinbaseChain, CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult,
    CoinbaseRecipientVerifier, coinbase_network,
};
use gofin_domain::{Adapter, GoFinError, ResourceId, Result};
use gofin_solana::SolanaWalletAdapter;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct CoinbaseChainRecipientVerifier {
    payout: SolanaCexPayoutVerifier,
    destinations: Vec<gofin_binance::EthereumDestination>,
    solana: BTreeMap<String, Arc<SolanaWalletAdapter>>,
}
impl CoinbaseChainRecipientVerifier {
    pub(crate) fn new(
        config: &BrokerConfigV2,
        ethereum: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        solana: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    ) -> Result<Self> {
        let evm: Option<Arc<dyn BinanceRecipientVerifier>> = if ethereum.is_empty() {
            None
        } else {
            Some(Arc::new(EthereumPayoutVerifier::new(ethereum)?))
        };
        Ok(Self {
            payout: SolanaCexPayoutVerifier::new(evm, solana.clone())?,
            destinations: crate::configured_binance_destinations(config)?,
            solana,
        })
    }

    async fn resolve_evm(
        &self,
        network: &str,
        chain_id: u64,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<CoinbaseDestination>> {
        if !gofin_networks::supports_chain(chain_id) || !self.payout.supports_chain(chain_id) {
            return Ok(None);
        }
        let address = gofin_evm::lower_address(gofin_evm::address(address)?);
        let matching = self
            .destinations
            .iter()
            .filter(|destination| {
                destination.chain_id == chain_id
                    && destination.address.eq_ignore_ascii_case(&address)
                    && destination_name.is_none_or(|name| {
                        destination.name == name || destination.resource.0 == name
                    })
            })
            .collect::<Vec<_>>();
        if matching.len() > 1 || (destination_name.is_some() && matching.is_empty()) {
            return Ok(None);
        }
        let Some(asset) = self.payout.resolve_asset(chain_id, coin, contract).await? else {
            return Ok(None);
        };
        let destination = matching.first().copied();
        Ok(Some(CoinbaseDestination {
            enrolled: destination.is_some(),
            resource: destination
                .map(|item| item.resource.clone())
                .unwrap_or_else(|| ResourceId(format!("external:ethereum:{chain_id}:{address}"))),
            address,
            chain_id,
            asset,
            confirmations: 1,
            network: network.into(),
        }))
    }

    async fn resolve_solana(
        &self,
        network: &str,
        coin: &str,
        mint: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<CoinbaseDestination>> {
        let Some(runtime) = self.solana.values().next().map(|wallet| wallet.runtime()) else {
            return Ok(None);
        };
        let address = gofin_solana::parse_address(address)?;
        let asset = gofin_solana::funding::resolve_external_asset(&runtime, coin, mint).await?;
        if mint.is_some_and(|mint| asset.reference != mint) {
            return Ok(None);
        }
        let matching = self
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
        if matching.len() > 1 || (destination_name.is_some() && matching.is_empty()) {
            return Ok(None);
        }
        let enrolled = !matching.is_empty();
        Ok(Some(CoinbaseDestination {
            enrolled,
            resource: ResourceId(if enrolled {
                format!("{}:{address}", gofin_solana::NETWORK)
            } else {
                format!("external:{}:{address}", gofin_solana::NETWORK)
            }),
            address,
            chain_id: 0,
            asset,
            confirmations: 1,
            network: network.into(),
        }))
    }
}

#[async_trait]
impl CoinbaseRecipientVerifier for CoinbaseChainRecipientVerifier {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<CoinbaseDestination>> {
        let Some(route) = coinbase_network(network) else {
            return Ok(None);
        };
        match route.chain {
            CoinbaseChain::Evm(chain_id) => {
                self.resolve_evm(network, chain_id, coin, contract, address, destination_name)
                    .await
            }
            CoinbaseChain::Solana => {
                self.resolve_solana(network, coin, contract, address, destination_name)
                    .await
            }
        }
    }
    async fn verify(&self, request: CoinbasePayoutRequest) -> Result<CoinbasePayoutResult> {
        let destination = request.destination;
        let route = coinbase_network(&destination.network).filter(|route| {
            route.chain_id() == destination.chain_id
                && route.asset_network() == destination.asset.network
        });
        let Some(route) = route else {
            return Err(GoFinError::validation(
                "The original Coinbase destination network and asset binding is inconsistent.",
            ));
        };
        let network = match route.chain {
            CoinbaseChain::Evm(_) => destination.asset.network.clone(),
            // The shared CEX observer routes Solana proofs by Binance's network code.
            CoinbaseChain::Solana => "SOL".into(),
        };
        let proof = self
            .payout
            .verify(gofin_binance::PayoutRequest {
                tx_hash: request.tx_hash,
                network,
                memo: None,
                chain_id: destination.chain_id,
                recipient: destination.address,
                asset: destination.asset,
                minimum_amount: request.minimum,
                maximum_amount: request.maximum,
                confirmations: destination.confirmations,
            })
            .await;
        Ok(CoinbasePayoutResult {
            status: proof.status,
            amount: proof.amount_atoms,
            evidence: proof.evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    struct MetadataRpc(u64);
    #[async_trait]
    impl gofin_evm::EvmRpc for MetadataRpc {
        async fn request(&self, method: &str, _: Value) -> Result<Value> {
            Ok(match method {
                "eth_chainId" => json!(format!("0x{:x}", self.0)),
                "eth_blockNumber" => json!("0x2a"),
                "eth_getCode" => json!("0x6000"),
                "eth_call" => json!(format!("0x{:064x}", 6)),
                _ => panic!("unexpected RPC: {method}"),
            })
        }
    }

    fn verifier(chain_id: u64) -> CoinbaseChainRecipientVerifier {
        let address = "0x1111111111111111111111111111111111111111";
        let adapter = gofin_ethereum::EthereumAdapter::new(
            gofin_ethereum::EthereumConfig::wallet_only(chain_id, 2, vec![]),
            vec![gofin_ethereum::WalletConnection {
                name: "synthetic".into(),
                address: address.into(),
                signing_enabled: false,
            }],
            Arc::new(gofin_evm::EvmRuntime::new(
                Arc::new(MetadataRpc(chain_id)),
                chain_id,
                None,
            )),
        )
        .unwrap();
        let evm: Arc<dyn BinanceRecipientVerifier> = Arc::new(
            EthereumPayoutVerifier::new(BTreeMap::from([(chain_id, Arc::new(adapter))])).unwrap(),
        );
        CoinbaseChainRecipientVerifier {
            payout: SolanaCexPayoutVerifier::new(Some(evm), BTreeMap::new()).unwrap(),
            destinations: vec![gofin_binance::EthereumDestination {
                name: "synthetic".into(),
                resource: ResourceId(format!("ethereum:{chain_id}:{address}")),
                address: address.into(),
                chain_id,
                assets: BTreeMap::new(),
                confirmations: 1,
            }],
            solana: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn evm_networks_resolve_exact_enrolled_chain_and_onchain_usdc() {
        for route in gofin_coinbase::funding::COINBASE_NETWORKS {
            let CoinbaseChain::Evm(chain_id) = route.chain else {
                continue;
            };
            let verifier = verifier(chain_id);
            let address = "0x1111111111111111111111111111111111111111";
            let contract = (route.usdc != "native").then_some(route.usdc);
            let destination = verifier
                .resolve(route.network, "USDC", contract, address, Some("synthetic"))
                .await
                .unwrap()
                .unwrap();
            assert!(destination.enrolled);
            assert_eq!(destination.chain_id, chain_id);
            assert_eq!(destination.asset.network, route.asset_network());
            assert_eq!(destination.asset.reference, route.usdc);
            assert!(route.matches_destination(&destination, route.usdc));
            assert!(
                verifier
                    .resolve(route.network, "USDC", contract, address, Some("another"))
                    .await
                    .unwrap()
                    .is_none()
            );
            let mut mismatched = destination;
            mismatched.chain_id = if chain_id == 1 { 8453 } else { 1 };
            assert!(
                verifier
                    .verify(CoinbasePayoutRequest {
                        destination: mismatched,
                        tx_hash: format!("0x{}", "a".repeat(64)),
                        minimum: "1000000".into(),
                        maximum: "1000000".into(),
                    })
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn native_currency_must_be_the_selected_chain_gas_asset() {
        let address = "0x1111111111111111111111111111111111111111";
        let base = verifier(8453)
            .resolve("base", "ETH", None, address, Some("synthetic"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(base.asset.reference, "native");
        assert_eq!(base.asset.decimals, 18);
        assert!(
            verifier(137)
                .resolve("polygon", "ETH", None, address, Some("synthetic"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn missing_network_observer_never_falls_back_to_another_chain() {
        let verifier = verifier(8453);
        let address = "0x1111111111111111111111111111111111111111";
        assert!(
            verifier
                .resolve(
                    "ethereum",
                    "USDC",
                    Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                    address,
                    None
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            verifier
                .resolve(
                    "solana",
                    "SOL",
                    None,
                    "11111111111111111111111111111112",
                    None
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            verifier
                .resolve("ethereum:8453", "USDC", None, address, None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
