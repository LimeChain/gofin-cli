//! Reviewed EVM networks supported by GoFin's wallet layer.
//!
//! This registry describes network identity and wallet settlement metadata.
//! Protocol and provider applicability remain owned by their adapters.

/// Settlement classification used for canonical wallet receipt policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementModel {
    /// The network uses the configured RPC finalized tag when requested.
    RpcFinalized,
    /// Canonical rollup sequencer inclusion is the reviewed settlement signal.
    SequencerInclusion,
}

/// Static metadata for one reviewed EVM wallet network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvmNetwork {
    pub chain_id: u64,
    /// Canonical GoFin resource network identifier, for example `ethereum:1`.
    pub network_id: &'static str,
    pub display_name: &'static str,
    /// Ordered public endpoints: primary first, followed by read fallbacks.
    pub rpc_urls: &'static [&'static str],
    /// Previously shipped default list eligible for a runtime-only refresh.
    /// Empty means no refresh; custom lists and order remain authoritative.
    pub previous_rpc_urls: &'static [&'static str],
    pub minimum_confirmations: u64,
    pub settlement_model: SettlementModel,
}

impl EvmNetwork {
    pub const fn native_symbol(self) -> &'static str {
        match self.chain_id {
            137 => "POL",
            43114 => "AVAX",
            5042 => "USDC",
            999 => "HYPE",
            _ => "ETH",
        }
    }

    pub const fn uses_sequencer_inclusion(self) -> bool {
        matches!(self.settlement_model, SettlementModel::SequencerInclusion)
    }
}

const NETWORKS: &[EvmNetwork] = &[
    EvmNetwork {
        chain_id: 1,
        network_id: "ethereum:1",
        display_name: "Ethereum Mainnet",
        rpc_urls: &[
            "https://ethereum-rpc.publicnode.com",
            "https://eth.drpc.org",
        ],
        previous_rpc_urls: &[],
        minimum_confirmations: 2,
        settlement_model: SettlementModel::RpcFinalized,
    },
    EvmNetwork {
        chain_id: 8453,
        network_id: "ethereum:8453",
        display_name: "Base",
        // Standard RPC uses canonical full blocks, not pending preconfirmations.
        rpc_urls: &["https://mainnet.base.org", "https://base.drpc.org"],
        previous_rpc_urls: &["https://mainnet.base.org"],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::SequencerInclusion,
    },
    EvmNetwork {
        chain_id: 42161,
        network_id: "ethereum:42161",
        display_name: "Arbitrum One",
        rpc_urls: &[
            "https://arb1.arbitrum.io/rpc",
            "https://arb-one.api.pocket.network",
            "https://arb-mainnet.g.alchemy.com/public",
        ],
        previous_rpc_urls: &["https://arb1.arbitrum.io/rpc"],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::SequencerInclusion,
    },
    EvmNetwork {
        chain_id: 4663,
        network_id: "ethereum:4663",
        display_name: "Robinhood Chain",
        rpc_urls: &[
            "https://rpc.mainnet.chain.robinhood.com",
            "https://robinhood-rpc.publicnode.com",
        ],
        previous_rpc_urls: &["https://rpc.mainnet.chain.robinhood.com"],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::SequencerInclusion,
    },
    EvmNetwork {
        chain_id: 10,
        network_id: "ethereum:10",
        display_name: "Optimism",
        rpc_urls: &[
            "https://mainnet.optimism.io",
            "https://optimism-rpc.publicnode.com",
        ],
        previous_rpc_urls: &[],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::SequencerInclusion,
    },
    EvmNetwork {
        chain_id: 137,
        network_id: "ethereum:137",
        display_name: "Polygon",
        rpc_urls: &["https://polygon.drpc.org", "https://polygon.publicnode.com"],
        previous_rpc_urls: &[],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::RpcFinalized,
    },
    EvmNetwork {
        chain_id: 43114,
        network_id: "ethereum:43114",
        display_name: "Avalanche C-Chain",
        rpc_urls: &["https://api.avax.network/ext/bc/C/rpc"],
        previous_rpc_urls: &[],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::RpcFinalized,
    },
    EvmNetwork {
        chain_id: 5042,
        network_id: "ethereum:5042",
        display_name: "Arc",
        rpc_urls: &["https://rpc.mainnet.arc.io"],
        previous_rpc_urls: &[],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::RpcFinalized,
    },
    EvmNetwork {
        chain_id: 999,
        network_id: "ethereum:999",
        display_name: "HyperEVM",
        rpc_urls: &["https://rpc.hyperliquid.xyz/evm"],
        previous_rpc_urls: &[],
        minimum_confirmations: 1,
        settlement_model: SettlementModel::RpcFinalized,
    },
];

/// All reviewed EVM networks, in stable user-facing order.
pub const fn all() -> &'static [EvmNetwork] {
    NETWORKS
}

/// Resolve a chain ID without adding protocol-specific assumptions.
pub const fn by_chain_id(chain_id: u64) -> Option<&'static EvmNetwork> {
    let mut index = 0;
    while index < NETWORKS.len() {
        if NETWORKS[index].chain_id == chain_id {
            return Some(&NETWORKS[index]);
        }
        index += 1;
    }
    None
}

/// Resolve GoFin's canonical network identifier.
pub fn by_network_id(network_id: &str) -> Option<&'static EvmNetwork> {
    NETWORKS
        .iter()
        .find(|network| network.network_id == network_id)
}

/// Whether an EVM chain is in GoFin's reviewed wallet registry.
pub fn supports_chain(chain_id: u64) -> bool {
    by_chain_id(chain_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_wallet_metadata_preserves_current_networks_and_defaults() {
        assert_eq!(
            all()
                .iter()
                .map(|network| network.chain_id)
                .collect::<Vec<_>>(),
            [1, 8453, 42161, 4663, 10, 137, 43114, 5042, 999]
        );
        let ethereum = by_chain_id(1).unwrap();
        assert_eq!(ethereum.network_id, "ethereum:1");
        assert_eq!(ethereum.display_name, "Ethereum Mainnet");
        assert_eq!(ethereum.minimum_confirmations, 2);
        assert_eq!(
            ethereum.rpc_urls,
            [
                "https://ethereum-rpc.publicnode.com",
                "https://eth.drpc.org"
            ]
        );
        assert!(!ethereum.uses_sequencer_inclusion());

        let base = by_chain_id(8453).unwrap();
        assert_eq!(base.network_id, "ethereum:8453");
        assert_eq!(base.display_name, "Base");
        assert_eq!(base.minimum_confirmations, 1);
        assert_eq!(
            base.rpc_urls,
            ["https://mainnet.base.org", "https://base.drpc.org"]
        );
        assert!(base.uses_sequencer_inclusion());

        let arbitrum = by_network_id("ethereum:42161").unwrap();
        assert_eq!(arbitrum.display_name, "Arbitrum One");
        assert_eq!(arbitrum.minimum_confirmations, 1);
        assert_eq!(
            arbitrum.rpc_urls,
            [
                "https://arb1.arbitrum.io/rpc",
                "https://arb-one.api.pocket.network",
                "https://arb-mainnet.g.alchemy.com/public"
            ]
        );
        assert!(arbitrum.uses_sequencer_inclusion());

        let robinhood = by_chain_id(4663).unwrap();
        assert_eq!(robinhood.display_name, "Robinhood Chain");
        assert_eq!(robinhood.minimum_confirmations, 1);
        assert_eq!(
            robinhood.rpc_urls,
            [
                "https://rpc.mainnet.chain.robinhood.com",
                "https://robinhood-rpc.publicnode.com"
            ]
        );
        assert_eq!(robinhood.previous_rpc_urls, [robinhood.rpc_urls[0]]);
        assert!(robinhood.uses_sequencer_inclusion());
        let optimism = by_chain_id(10).unwrap();
        assert_eq!(optimism.network_id, "ethereum:10");
        assert_eq!(optimism.display_name, "Optimism");
        assert_eq!(optimism.minimum_confirmations, 1);
        assert_eq!(
            optimism.rpc_urls,
            [
                "https://mainnet.optimism.io",
                "https://optimism-rpc.publicnode.com"
            ]
        );
        assert!(optimism.previous_rpc_urls.is_empty());
        assert!(optimism.uses_sequencer_inclusion());
        assert!(supports_chain(10));
        assert!(by_network_id("ethereum:1").is_some());
        assert_eq!(by_network_id("ethereum:10"), Some(optimism));

        let polygon = by_chain_id(137).unwrap();
        assert_eq!(polygon.network_id, "ethereum:137");
        assert_eq!(polygon.display_name, "Polygon");
        assert_eq!(
            polygon.rpc_urls,
            ["https://polygon.drpc.org", "https://polygon.publicnode.com"]
        );
        assert!(polygon.previous_rpc_urls.is_empty());
        assert!(!polygon.uses_sequencer_inclusion());

        let avalanche = by_chain_id(43114).unwrap();
        assert_eq!(avalanche.network_id, "ethereum:43114");
        assert_eq!(avalanche.display_name, "Avalanche C-Chain");
        assert_eq!(
            avalanche.rpc_urls,
            ["https://api.avax.network/ext/bc/C/rpc"]
        );
        assert!(avalanche.previous_rpc_urls.is_empty());
        assert!(!avalanche.uses_sequencer_inclusion());

        let arc = by_chain_id(5042).unwrap();
        assert_eq!(arc.network_id, "ethereum:5042");
        assert_eq!(arc.display_name, "Arc");
        assert_eq!(arc.rpc_urls, ["https://rpc.mainnet.arc.io"]);
        assert!(arc.previous_rpc_urls.is_empty());
        assert!(!arc.uses_sequencer_inclusion());

        let hyperevm = by_chain_id(999).unwrap();
        assert_eq!(hyperevm.network_id, "ethereum:999");
        assert_eq!(hyperevm.display_name, "HyperEVM");
        assert_eq!(hyperevm.rpc_urls, ["https://rpc.hyperliquid.xyz/evm"]);
        assert!(hyperevm.previous_rpc_urls.is_empty());
        assert!(!hyperevm.uses_sequencer_inclusion());
    }
}
