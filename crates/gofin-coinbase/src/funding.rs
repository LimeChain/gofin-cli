//! Typed recipient verification seam. Coinbase venue proof is delegated to a
//! protected network observer; the profile never trusts an address string as
//! proof that a withdrawal settled.
use async_trait::async_trait;
use gofin_domain::{AssetId, Quantity, ResourceId, Result, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Reviewed native-USDC routes. Structural support does not prove that Coinbase
/// currently permits a send on this network for a particular account.
#[derive(Clone, Copy, Debug)]
pub struct CoinbaseUsdcNetwork {
    pub network: &'static str,
    pub chain_id: u64,
    pub contract: &'static str,
}

pub const USDC_WITHDRAWAL_NETWORKS: &[CoinbaseUsdcNetwork] = &[
    CoinbaseUsdcNetwork {
        network: "ethereum",
        chain_id: 1,
        contract: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
    },
    CoinbaseUsdcNetwork {
        network: "base",
        chain_id: 8453,
        contract: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
    },
    CoinbaseUsdcNetwork {
        network: "arbitrum",
        chain_id: 42161,
        contract: "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
    },
    CoinbaseUsdcNetwork {
        network: "optimism",
        chain_id: 10,
        contract: "0x0b2c639c533813f4aa9d7837caf62653d097ff85",
    },
    CoinbaseUsdcNetwork {
        network: "polygon",
        chain_id: 137,
        contract: "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359",
    },
    CoinbaseUsdcNetwork {
        network: "avalanche",
        chain_id: 43114,
        contract: "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e",
    },
];

pub fn usdc_withdrawal_network(network: &str) -> Option<&'static CoinbaseUsdcNetwork> {
    USDC_WITHDRAWAL_NETWORKS
        .iter()
        .find(|route| route.network == network)
}

impl CoinbaseUsdcNetwork {
    pub fn matches_destination(&self, destination: &CoinbaseDestination) -> bool {
        destination.enrolled
            && destination.network == self.network
            && destination.chain_id == self.chain_id
            && destination.asset.network == format!("ethereum:{}", self.chain_id)
            && destination
                .asset
                .reference
                .eq_ignore_ascii_case(self.contract)
            && destination.asset.decimals == 6
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoinbaseDestination {
    pub enrolled: bool,
    pub resource: ResourceId,
    pub address: String,
    pub chain_id: u64,
    pub asset: AssetId,
    pub confirmations: u32,
    pub network: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoinbasePayoutRequest {
    pub destination: CoinbaseDestination,
    pub tx_hash: String,
    pub minimum: String,
    pub maximum: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoinbasePayoutResult {
    pub status: Status,
    pub amount: Option<Quantity>,
    pub evidence: Value,
}

#[async_trait]
pub trait CoinbaseRecipientVerifier: Send + Sync {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        destination_name: Option<&str>,
    ) -> Result<Option<CoinbaseDestination>>;
    async fn verify(&self, request: CoinbasePayoutRequest) -> Result<CoinbasePayoutResult>;
}
