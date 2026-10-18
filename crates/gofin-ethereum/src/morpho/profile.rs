//! Reviewed immutable Morpho Vault V2 deployments. No agent-selected endpoints
//! or deployment addresses are accepted.
use gofin_domain::{GoFinError, Result};
use serde::Serialize;

/// Sources: Morpho's contracts/addresses documentation and the official SDK
/// packages/morpho-ts/src/addresses.ts at
/// 55536376fb39612c5ff0239009b06b63af27fb99 (reviewed 2026-10-18).
/// Factory membership semantics: morpho-org/vault-v2
/// f4fc64ed11cda9e6722fbedf6f55b89606eacd1e/src/VaultV2Factory.sol.
/// Runtime hashes independently observed with eth_getCode from Ethereum
/// publicnode and Base mainnet RPC: factory 23,124 bytes, bundles 6,781 bytes.
/// Supported adapter membership uses src/adapters/MorphoMarketV1AdapterV2Factory.sol
/// and MorphoVaultV1AdapterFactory.sol at the same vault-v2 source revision.
/// Their independently observed runtimes are 13,633 and 5,775 bytes respectively;
/// the market factory contains chain-specific immutable Morpho/IRM addresses.
#[derive(Clone, Debug, Serialize)]
pub struct MorphoProfile {
    pub chain_id: u64,
    pub vault_factory: &'static str,
    pub vault_bundles: &'static str,
    pub factory_code_hash: &'static str,
    pub bundles_code_hash: &'static str,
    pub market_adapter_factory: &'static str,
    pub market_adapter_factory_code_hash: &'static str,
    pub vault_adapter_factory: &'static str,
    pub vault_adapter_factory_code_hash: &'static str,
}

pub fn profile(chain_id: u64) -> Result<MorphoProfile> {
    let (vault_factory, vault_bundles, factory_code_hash, bundles_code_hash) = match chain_id {
        1 => (
            "0xa1d94f746defa1928926b84fb2596c06926c0405",
            "0x02912516d49de997db75b9d7858faae59209650b",
            "0xba55eb73d42937607d7e3c77f68353c2517c4fad4df02f0c9bbd2e4248475503",
            "0xa8ec5960bb20a33ed52e8248aed85ee3148135b185bba8bb74eb572a987fd4a5",
        ),
        8453 => (
            "0x4501125508079a99ebbebce205dec9593c2b5857",
            "0x2b08a911f48de25a7e305d910afb5597abe8ea7b",
            "0xba55eb73d42937607d7e3c77f68353c2517c4fad4df02f0c9bbd2e4248475503",
            "0xa8ec5960bb20a33ed52e8248aed85ee3148135b185bba8bb74eb572a987fd4a5",
        ),
        _ => {
            return Err(GoFinError::new(
                "MORPHO_NETWORK_UNSUPPORTED",
                "validation",
                "Morpho Vault V2 is supported on Ethereum and Base.",
                "Select chain 1 or 8453.",
            ));
        }
    };
    Ok(MorphoProfile {
        chain_id,
        vault_factory,
        vault_bundles,
        factory_code_hash,
        bundles_code_hash,
        market_adapter_factory: if chain_id == 1 {
            "0x32bb1c0d48d8b1b3363e86eeb9a0300bad61ccc1"
        } else {
            "0x9a1b378c43ba535cdb89934230f0d3890c51c0eb"
        },
        market_adapter_factory_code_hash: if chain_id == 1 {
            "0x0add593bff158a5b8cdfe1060056a9d886cd1d447da031e05eb0e6fa9e4fd5b4"
        } else {
            "0x569933aba5190056e40f3119db624aa79a4890352d8c7a965d542e347fa1740d"
        },
        vault_adapter_factory: if chain_id == 1 {
            "0xd1b8e2dee25c2b89dcd2f98448a7ce87d6f63394"
        } else {
            "0xf42d9c36b34c9c2cf3bc30ed2a52a90eeb604642"
        },
        vault_adapter_factory_code_hash: "0x4e1024d5bfd17914a3ca6ad49905eabdfd60122ccd5a577406e7a4a765bd49ad",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deployments_are_explicit_and_network_bound() {
        let ethereum = profile(1).unwrap();
        let base = profile(8453).unwrap();
        assert_ne!(ethereum.vault_factory, base.vault_factory);
        assert_ne!(ethereum.vault_bundles, base.vault_bundles);
        assert!(profile(42161).is_err());
        assert!(profile(0).is_err());
    }
}
