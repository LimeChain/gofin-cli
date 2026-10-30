//! Bundled candidate catalogs; metadata values are hints, never balance evidence.
//! The TokenDiscovery seam can later supply a remote list with local fallback.

use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_ethereum::{TokenDiscovery, TokenDiscoveryResult};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    chain_id: u64,
    network: String,
    generated_at: String,
    tokens: Vec<CatalogToken>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogToken {
    address: String,
    name: String,
    symbol: String,
    decimals: u8,
    metadata_expiry_seconds: i64,
}

pub(crate) struct BundledTokenDiscovery {
    catalogs: BTreeMap<u64, Catalog>,
}

impl BundledTokenDiscovery {
    pub(crate) fn new() -> Result<Self> {
        let mut catalogs = BTreeMap::new();
        for (chain_id, json) in [
            (1, include_str!("../token-lists/1.json")),
            (8453, include_str!("../token-lists/8453.json")),
            (42161, include_str!("../token-lists/42161.json")),
            (4663, include_str!("../token-lists/4663.json")),
        ] {
            let catalog = Self::parse(chain_id, json)?;
            catalogs.insert(chain_id, catalog);
        }
        Ok(Self { catalogs })
    }

    fn parse(chain_id: u64, json: &str) -> Result<Catalog> {
        let invalid = || GoFinError::validation("Bundled EVM token catalog is invalid.");
        let catalog: Catalog = serde_json::from_str(json).map_err(|_| invalid())?;
        if catalog.schema_version != 1
            || catalog.chain_id != chain_id
            || catalog.network.is_empty()
            || chrono::DateTime::parse_from_rfc3339(&catalog.generated_at).is_err()
            || !(100..=128).contains(&catalog.tokens.len())
        {
            return Err(invalid());
        }
        let mut seen = std::collections::BTreeSet::new();
        for token in &catalog.tokens {
            let address = gofin_evm::address(&token.address).map_err(|_| invalid())?;
            if address == gofin_evm::alloy::primitives::Address::ZERO
                || token.address == "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                || token.address != token.address.to_ascii_lowercase()
                || !seen.insert(token.address.clone())
                || token.name.is_empty()
                || token.symbol.is_empty()
                || token.name.chars().any(char::is_control)
                || token.symbol.chars().any(char::is_control)
                || (token.metadata_expiry_seconds != -1 && token.metadata_expiry_seconds <= 0)
            {
                return Err(invalid());
            }
            // The catalog's declared decimals are never used for financial amounts.
            let _ = token.decimals;
        }
        Ok(catalog)
    }
}

#[async_trait]
impl TokenDiscovery for BundledTokenDiscovery {
    async fn tokens(&self, chain_id: u64, _address: &str) -> Result<TokenDiscoveryResult> {
        let catalog = self.catalogs.get(&chain_id);
        Ok(TokenDiscoveryResult {
            candidates: catalog.map(|c| c.tokens.iter().map(|t| t.address.clone()).collect()).unwrap_or_default(),
            complete: false,
            source: format!("bundled-token-catalog:v1:{chain_id}"),
            warnings: vec![match catalog {
                Some(_) => "Bundled token coverage is finite; unlisted holdings may be absent. Supply an explicit contract address for an unlisted token.",
                None => "This network has no bundled token catalog; only configured, remembered or explicitly requested tokens are included.",
            }.into()],
        })
    }

    fn metadata_expiry_seconds(&self, chain_id: u64, address: &str) -> i64 {
        self.catalogs
            .get(&chain_id)
            .and_then(|catalog| {
                catalog
                    .tokens
                    .iter()
                    .find(|token| token.address.eq_ignore_ascii_case(address))
            })
            .map_or(86400, |token| token.metadata_expiry_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bundled_catalogs_are_bounded_and_do_not_claim_exhaustive_discovery() {
        let provider = BundledTokenDiscovery::new().unwrap();
        for chain in [1, 8453, 42161, 4663] {
            let result = provider.tokens(chain, "unused").await.unwrap();
            assert_eq!(result.candidates.len(), 128);
            assert!(!result.complete);
            assert_eq!(
                provider.metadata_expiry_seconds(chain, &result.candidates[0]),
                86400
            );
        }
        let unsupported = provider.tokens(999, "unused").await.unwrap();
        assert!(unsupported.candidates.is_empty());
        assert!(!unsupported.complete);
        assert_eq!(provider.metadata_expiry_seconds(999, "unknown"), 86400);
    }

    #[test]
    fn policy_accepts_forever_and_rejects_invalid_scope_or_expiry() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../token-lists/1.json")).unwrap();
        let mut value = original.clone();
        value["tokens"][0]["metadataExpirySeconds"] = serde_json::json!(-1);
        assert_eq!(
            BundledTokenDiscovery::parse(1, &value.to_string())
                .unwrap()
                .tokens[0]
                .metadata_expiry_seconds,
            -1
        );
        for expiry in [0, -2] {
            value["tokens"][0]["metadataExpirySeconds"] = serde_json::json!(expiry);
            assert!(BundledTokenDiscovery::parse(1, &value.to_string()).is_err());
        }
        assert!(BundledTokenDiscovery::parse(8453, &original.to_string()).is_err());
        value = original.clone();
        value["tokens"][1] = value["tokens"][0].clone();
        assert!(BundledTokenDiscovery::parse(1, &value.to_string()).is_err());
    }
}
