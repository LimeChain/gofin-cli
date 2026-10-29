//! Typed recipient verification seam. Coinbase venue proof is delegated to a
//! protected network observer; the profile never trusts an address string as
//! proof that a withdrawal settled.
use async_trait::async_trait;
use gofin_domain::{AssetId, Quantity, ResourceId, Result, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SOLANA_NETWORK: &str = "solana:mainnet-beta";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoinbaseChain {
    Evm(u64),
    Solana,
}

/// Coinbase send names for GoFin-observed chains; Coinbase may still refuse a send.
#[derive(Clone, Copy, Debug)]
pub struct CoinbaseNetwork {
    pub network: &'static str,
    pub chain: CoinbaseChain,
    /// Canonical native USDC: an EVM contract, a Solana mint, or `native`.
    pub usdc: &'static str,
}

pub const COINBASE_NETWORKS: &[CoinbaseNetwork] = &[
    CoinbaseNetwork {
        network: "ethereum",
        chain: CoinbaseChain::Evm(1),
        usdc: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
    },
    CoinbaseNetwork {
        network: "base",
        chain: CoinbaseChain::Evm(8453),
        usdc: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
    },
    CoinbaseNetwork {
        network: "arbitrum",
        chain: CoinbaseChain::Evm(42161),
        usdc: "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
    },
    CoinbaseNetwork {
        network: "optimism",
        chain: CoinbaseChain::Evm(10),
        usdc: "0x0b2c639c533813f4aa9d7837caf62653d097ff85",
    },
    CoinbaseNetwork {
        network: "polygon",
        chain: CoinbaseChain::Evm(137),
        usdc: "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359",
    },
    CoinbaseNetwork {
        network: "avalanche",
        chain: CoinbaseChain::Evm(43114),
        usdc: "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e",
    },
    // Arc's gas asset is USDC; its 18-decimal native balance is the canonical identity.
    CoinbaseNetwork {
        network: "arc",
        chain: CoinbaseChain::Evm(5042),
        usdc: "native",
    },
    CoinbaseNetwork {
        network: "hyperevm",
        chain: CoinbaseChain::Evm(999),
        usdc: "0xb88339cb7199b77e23db6e890353e22632ba630f",
    },
    CoinbaseNetwork {
        network: "solana",
        chain: CoinbaseChain::Solana,
        usdc: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    },
];

/// `gofin-networks` chains Coinbase does not offer sends on.
pub const COINBASE_UNOFFERED_CHAINS: &[u64] = &[4663];

pub fn coinbase_network(network: &str) -> Option<&'static CoinbaseNetwork> {
    COINBASE_NETWORKS
        .iter()
        .find(|route| route.network == network)
}

pub fn coinbase_evm_network(chain_id: u64) -> Option<&'static CoinbaseNetwork> {
    COINBASE_NETWORKS
        .iter()
        .find(|route| route.chain == CoinbaseChain::Evm(chain_id))
}

impl CoinbaseNetwork {
    pub fn asset_network(&self) -> String {
        match self.chain {
            CoinbaseChain::Evm(chain_id) => format!("ethereum:{chain_id}"),
            CoinbaseChain::Solana => SOLANA_NETWORK.into(),
        }
    }

    /// Non-EVM destinations carry chain ID zero.
    pub fn chain_id(&self) -> u64 {
        match self.chain {
            CoinbaseChain::Evm(chain_id) => chain_id,
            CoinbaseChain::Solana => 0,
        }
    }

    pub fn same_identity(&self, left: &str, right: &str) -> bool {
        match self.chain {
            CoinbaseChain::Evm(_) => left.eq_ignore_ascii_case(right),
            CoinbaseChain::Solana => left == right,
        }
    }

    pub fn valid_tx_hash(&self, hash: &str) -> bool {
        match self.chain {
            CoinbaseChain::Evm(_) => {
                hash.len() == 66
                    && hash.starts_with("0x")
                    && hash[2..].bytes().all(|c| c.is_ascii_hexdigit())
            }
            CoinbaseChain::Solana => {
                (64..=88).contains(&hash.len())
                    && hash.bytes().all(|c| {
                        c.is_ascii_alphanumeric() && !matches!(c, b'0' | b'O' | b'I' | b'l')
                    })
            }
        }
    }

    pub fn matches_destination(&self, destination: &CoinbaseDestination, token: &str) -> bool {
        destination.enrolled
            && destination.network == self.network
            && destination.chain_id == self.chain_id()
            && destination.asset.network == self.asset_network()
            && self.same_identity(&destination.asset.reference, token)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_chain_is_offered_or_explicitly_unoffered() {
        for network in gofin_networks::all() {
            let offered = coinbase_evm_network(network.chain_id).is_some();
            let unoffered = COINBASE_UNOFFERED_CHAINS.contains(&network.chain_id);
            assert!(
                offered != unoffered,
                "{} ({}) must be in exactly one of COINBASE_NETWORKS or COINBASE_UNOFFERED_CHAINS",
                network.display_name,
                network.chain_id
            );
        }
        for route in COINBASE_NETWORKS {
            if let CoinbaseChain::Evm(chain_id) = route.chain {
                assert!(
                    gofin_networks::supports_chain(chain_id),
                    "{}",
                    route.network
                );
            }
        }
    }

    #[test]
    fn transaction_hash_shape_follows_the_selected_chain() {
        let evm = coinbase_network("base").unwrap();
        let solana = coinbase_network("solana").unwrap();
        let evm_hash = format!("0x{}", "a".repeat(64));
        let signature = "5".repeat(87);
        assert!(evm.valid_tx_hash(&evm_hash));
        assert!(!evm.valid_tx_hash(&signature));
        assert!(solana.valid_tx_hash(&signature));
        assert!(!solana.valid_tx_hash(&evm_hash));
        assert!(!solana.valid_tx_hash(&"0".repeat(87)));
    }
}
