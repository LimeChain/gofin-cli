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
    let id = match network {
        "ethereum" | "ethereum:1" => 1,
        "base" | "ethereum:8453" => 8453,
        "arbitrum" | "ethereum:42161" => 42161,
        "optimism" | "ethereum:10" => 10,
        _ => return None,
    };
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
