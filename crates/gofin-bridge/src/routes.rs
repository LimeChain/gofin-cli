//! Indicative route discovery, deliberately separate from executable quotes.
use crate::{BridgeAssetInput, EvmAsset, SolanaAsset, SolanaAssetInput};
use gofin_domain::{GoFinError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BridgeRoutesInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BridgeAssetInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_source: Option<SolanaAssetInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_chain_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_destination: Option<SolanaRouteDestination>,
    #[serde(default)]
    pub destination_asset: Option<BridgeAssetInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SolanaRouteDestination {
    pub network: String,
    pub mint: Option<String>,
}

/// The broker supplies independently observed precision, never API symbols.
#[derive(Clone, Debug)]
pub struct BridgeRouteRequest {
    pub source: Option<EvmAsset>,
    pub solana_source: Option<SolanaAsset>,
    pub destination_chain_id: u64,
    pub solana_destination: Option<SolanaRouteDestination>,
    pub destination_asset: Option<EvmAsset>,
}
impl BridgeRouteRequest {
    pub fn evm_source(&self) -> Result<&EvmAsset> {
        if self.solana_source.is_some() {
            return Err(GoFinError::validation("This route requires an EVM source."));
        }
        self.source
            .as_ref()
            .ok_or_else(|| GoFinError::validation("An exact EVM source is required."))
    }
    pub fn evm_destination_chain_id(&self) -> Result<u64> {
        if self.solana_destination.is_some() {
            return Err(GoFinError::validation(
                "This route requires an EVM destination.",
            ));
        }
        Ok(self.destination_chain_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteAvailability {
    Candidate,
    Unavailable,
    Unimplemented,
    Unknown,
}

/// Provider metadata is a candidate. Only the broker can verify its identity.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BridgeRouteCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<BridgeAssetInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_destination: Option<SolanaAssetInput>,
    #[serde(default)]
    pub reported_decimals: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BridgeRouteDiscovery {
    pub provider: String,
    pub availability: RouteAvailability,
    pub candidates: Vec<BridgeRouteCandidate>,
    pub observed_at: String,
    pub evidence_source: String,
    pub complete: bool,
    pub limitations: Vec<String>,
    #[serde(default)]
    pub error: Option<GoFinError>,
}

impl BridgeRouteDiscovery {
    pub fn unsupported(provider: String) -> Self {
        Self {
            provider,
            availability: RouteAvailability::Unimplemented,
            candidates: vec![],
            observed_at: gofin_domain::now(),
            evidence_source: "GoFin provider capabilities".into(),
            complete: false,
            limitations: vec!["This provider has no route-discovery implementation; exact bridge.quotes may still be available.".into()],
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_does_not_accept_execution_authority_or_ambiguous_symbols() {
        let input = json!({"source":{"chainId":1},"destinationChainId":8453});
        assert!(serde_json::from_value::<BridgeRoutesInput>(input.clone()).is_ok());
        for (field, value) in [
            ("transferId", json!("attempt")),
            ("calldata", json!("0x")),
            ("authorization", json!("grant")),
            ("amount", json!("100")),
        ] {
            let mut altered = input.clone();
            altered[field] = value;
            assert!(serde_json::from_value::<BridgeRoutesInput>(altered).is_err());
        }
        let mut altered = input;
        altered["source"]["symbol"] = json!("USDC");
        assert!(serde_json::from_value::<BridgeRoutesInput>(altered).is_err());
        let result =
            serde_json::to_value(BridgeRouteDiscovery::unsupported("fixture".into())).unwrap();
        assert!(result.get("transfer_id").is_none());
        assert!(result.get("steps").is_none());
        assert_eq!(result["availability"], "unimplemented");
    }
}
