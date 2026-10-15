//! Typed recipient verification seam. Coinbase venue proof is delegated to a
//! protected network observer; the profile never trusts an address string as
//! proof that a withdrawal settled.
use async_trait::async_trait;
use fin_domain::{AssetId, Quantity, ResourceId, Result, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
