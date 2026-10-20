//! Broker-owned receipt verification for exact Coinbase EVM destinations.
use crate::{BrokerConfigV2, EthereumPayoutVerifier};
use async_trait::async_trait;
use gofin_binance::BinanceRecipientVerifier;
use gofin_coinbase::funding::{
    CoinbaseDestination, CoinbasePayoutRequest, CoinbasePayoutResult, CoinbaseRecipientVerifier,
};
use gofin_domain::{GoFinError, Result};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct CoinbaseEvmRecipientVerifier {
    observer: EthereumPayoutVerifier,
    destinations: Vec<gofin_binance::EthereumDestination>,
}
impl CoinbaseEvmRecipientVerifier {
    pub(crate) fn new(
        config: &BrokerConfigV2,
        adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
    ) -> Result<Self> {
        Ok(Self {
            observer: EthereumPayoutVerifier::new(adapters)?,
            destinations: crate::configured_binance_destinations(config)?,
        })
    }
}
fn reviewed_chain(network: &str) -> Option<u64> {
    let id = gofin_coinbase::funding::USDC_WITHDRAWAL_NETWORKS
        .iter()
        .find(|route| {
            network == route.network || network == format!("ethereum:{}", route.chain_id)
        })?
        .chain_id;
    gofin_networks::supports_chain(id).then_some(id)
}

#[async_trait]
impl CoinbaseRecipientVerifier for CoinbaseEvmRecipientVerifier {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<CoinbaseDestination>> {
        let Some(chain_id) = reviewed_chain(network) else {
            return Ok(None);
        };
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
        let Some(asset) = self
            .observer
            .resolve_asset(chain_id, coin, contract)
            .await?
        else {
            return Ok(None);
        };
        let destination = matching.first().copied();
        Ok(Some(CoinbaseDestination {
            enrolled: destination.is_some(),
            resource: destination
                .map(|item| item.resource.clone())
                .unwrap_or_else(|| {
                    gofin_domain::ResourceId(format!("external:ethereum:{chain_id}:{address}"))
                }),
            address,
            chain_id,
            asset,
            confirmations: 1,
            network: network.into(),
        }))
    }
    async fn verify(&self, request: CoinbasePayoutRequest) -> Result<CoinbasePayoutResult> {
        if reviewed_chain(&request.destination.network) != Some(request.destination.chain_id)
            || request.destination.asset.network
                != format!("ethereum:{}", request.destination.chain_id)
        {
            return Err(GoFinError::validation(
                "The original Coinbase destination network and asset binding is inconsistent.",
            ));
        }
        let proof = self
            .observer
            .verify(gofin_binance::PayoutRequest {
                tx_hash: request.tx_hash,
                network: request.destination.asset.network.clone(),
                memo: None,
                chain_id: request.destination.chain_id,
                recipient: request.destination.address,
                asset: request.destination.asset,
                minimum_amount: request.minimum,
                maximum_amount: request.maximum,
                confirmations: request.destination.confirmations,
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
    use gofin_domain::ResourceId;
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

    fn verifier(chain_id: u64) -> CoinbaseEvmRecipientVerifier {
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
        CoinbaseEvmRecipientVerifier {
            observer: EthereumPayoutVerifier::new(BTreeMap::from([(chain_id, Arc::new(adapter))]))
                .unwrap(),
            destinations: vec![gofin_binance::EthereumDestination {
                name: "synthetic".into(),
                resource: ResourceId(format!("ethereum:{chain_id}:{address}")),
                address: address.into(),
                chain_id,
                assets: BTreeMap::new(),
                confirmations: 1,
            }],
        }
    }

    #[tokio::test]
    async fn withdrawal_routes_resolve_exact_enrolled_chain_and_onchain_token() {
        for route in gofin_coinbase::funding::USDC_WITHDRAWAL_NETWORKS {
            let verifier = verifier(route.chain_id);
            let address = "0x1111111111111111111111111111111111111111";
            let destination = verifier
                .resolve(
                    route.network,
                    "USDC",
                    Some(route.contract),
                    address,
                    Some("synthetic"),
                )
                .await
                .unwrap()
                .unwrap();
            assert!(destination.enrolled);
            assert_eq!(destination.chain_id, route.chain_id);
            assert_eq!(
                destination.asset.network,
                format!("ethereum:{}", route.chain_id)
            );
            assert_eq!(destination.asset.reference, route.contract);
            assert_eq!(destination.asset.decimals, 6);
            assert_eq!(
                reviewed_chain(&destination.asset.network),
                Some(route.chain_id)
            );
            assert!(
                verifier
                    .resolve(
                        route.network,
                        "USDC",
                        Some(route.contract),
                        address,
                        Some("another")
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            let mut mismatched = destination;
            mismatched.chain_id = if route.chain_id == 1 { 8453 } else { 1 };
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
    async fn missing_network_observer_never_falls_back_to_base() {
        let verifier = verifier(8453);
        assert!(
            verifier
                .resolve(
                    "ethereum",
                    "USDC",
                    Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                    "0x1111111111111111111111111111111111111111",
                    None
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(reviewed_chain("solana"), None);
        assert_eq!(reviewed_chain("base:1"), None);
    }
}
