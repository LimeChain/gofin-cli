//! Public indexer metadata. Never an authorization or settlement source.
use async_trait::async_trait;
use gofin_domain::{GoFinError, Quantity, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const MORPHO: &str = "https://api.morpho.org";
const MERKL: &str = "https://api.merkl.xyz";
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum MorphoApiRequest {
    Rest {
        path: String,
        query: Vec<(String, String)>,
    },
    Graphql {
        query: String,
        variables: Value,
    },
    MerklRewards {
        chain_id: u64,
        owner: String,
    },
}

#[async_trait]
pub trait MorphoApiTransport: Send + Sync {
    async fn request(&self, request: MorphoApiRequest) -> Result<Value>;
}

struct HttpTransport;
#[async_trait]
impl MorphoApiTransport for HttpTransport {
    async fn request(&self, request: MorphoApiRequest) -> Result<Value> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unavailable("Public API transport initialization failed."))?;
        let request = match request {
            MorphoApiRequest::Rest { path, query } => {
                if !path.starts_with("/v0/vaults-v2") && !path.starts_with("/v1/vaults-v2") {
                    return Err(invalid("Unsupported Morpho REST resource."));
                }
                client.get(format!("{MORPHO}{path}")).query(&query)
            }
            MorphoApiRequest::Graphql { query, variables } => client
                .post(format!("{MORPHO}/graphql"))
                .json(&json!({"query":query,"variables":variables})),
            MorphoApiRequest::MerklRewards { chain_id, owner } => {
                let owner = normalized_address(&owner)?;
                client
                    .get(format!("{MERKL}/v4/users/{owner}/rewards"))
                    .query(&[("chainId", chain_id.to_string())])
            }
        };
        let mut response = request
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|_| unavailable("Public rewards/vault API request failed."))?;
        if !response.status().is_success() {
            return Err(unavailable(&format!(
                "Public API returned HTTP {}.",
                response.status().as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(invalid("Public API response exceeds its bounded size."));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unavailable("Public API response was interrupted."))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(invalid("Public API response exceeds its bounded size."));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| invalid("Public API returned invalid JSON."))
    }
}

#[derive(Clone)]
pub struct MorphoClient {
    transport: Arc<dyn MorphoApiTransport>,
}
impl Default for MorphoClient {
    fn default() -> Self {
        Self::new()
    }
}
impl MorphoClient {
    pub fn new() -> Self {
        Self {
            transport: Arc::new(HttpTransport),
        }
    }
    /// Internal fixture injection; no endpoint or transport is accepted in CLI input.
    pub fn with_transport(transport: Arc<dyn MorphoApiTransport>) -> Self {
        Self { transport }
    }
    /// Internal fixture delegation. This does not expose endpoint selection to CLI input.
    #[doc(hidden)]
    pub async fn request(&self, request: MorphoApiRequest) -> Result<Value> {
        self.transport.request(request).await
    }
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let result = self
            .transport
            .request(MorphoApiRequest::Graphql {
                query: query.into(),
                variables,
            })
            .await?;
        if result
            .get("errors")
            .is_some_and(|e| !e.is_null() && e.as_array().is_none_or(|v| !v.is_empty()))
        {
            return Err(unavailable(
                "Morpho GraphQL returned errors; partial data is not presented as complete.",
            ));
        }
        result
            .get("data")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| invalid("Morpho GraphQL data is missing."))
    }
    pub async fn merkl_rewards(&self, chain_id: u64, owner: &str) -> Result<Value> {
        super::profile::profile(chain_id)?;
        self.transport
            .request(MorphoApiRequest::MerklRewards {
                chain_id,
                owner: normalized_address(owner)?,
            })
            .await
    }
    async fn rest<T: DeserializeOwned>(
        &self,
        path: String,
        query: Vec<(String, String)>,
    ) -> Result<T> {
        let result = self
            .transport
            .request(MorphoApiRequest::Rest { path, query })
            .await?;
        serde_json::from_value(result)
            .map_err(|_| invalid("Morpho REST response does not match the requested V2 resource."))
    }
    pub async fn vaults(
        &self,
        chain_id: u64,
        limit: u32,
        cursor: Option<&str>,
        asset: Option<&str>,
    ) -> Result<ApiPage<VaultListing>> {
        let mut query = page_query(chain_id, limit, cursor)?;
        if let Some(asset) = asset {
            query.push(("asset_addresses".into(), normalized_address(asset)?));
        }
        let page: ApiPage<VaultListing> = self.rest("/v0/vaults-v2".into(), query).await?;
        validate_cursor(page.cursor.as_deref())?;
        for row in &page.data {
            validate_vault(row.chain_id, &row.vault_address, &row.version, chain_id)?;
            if asset.is_some_and(|asset| !asset.eq_ignore_ascii_case(&row.asset_address)) {
                return Err(invalid(
                    "Morpho directory ignored the selected underlying asset filter.",
                ));
            }
        }
        Ok(page)
    }
    pub async fn vault(&self, chain_id: u64, vault: &str) -> Result<VaultDetails> {
        let vault = normalized_address(vault)?;
        let envelope: Envelope<VaultDetails> = self
            .rest(format!("/v0/vaults-v2/{chain_id}:{vault}"), vec![])
            .await?;
        validate_vault(
            envelope.data.chain_id,
            &envelope.data.address,
            &envelope.data.version,
            chain_id,
        )?;
        if !envelope.data.address.eq_ignore_ascii_case(&vault) {
            return Err(invalid("Morpho detail returned another vault."));
        }
        Ok(envelope.data)
    }
    pub async fn state(&self, chain_id: u64, vault: &str) -> Result<VaultIndexedState> {
        let vault = normalized_address(vault)?;
        let envelope: Envelope<VaultIndexedState> = self
            .rest(format!("/v1/vaults-v2/{chain_id}:{vault}/state"), vec![])
            .await?;
        if envelope.data.chain_id != chain_id || !envelope.data.address.eq_ignore_ascii_case(&vault)
        {
            return Err(invalid("Morpho state returned another chain or vault."));
        }
        Ok(envelope.data)
    }
    /// Indexed adapter, market and shared-cap observations. Cap rows describe
    /// overlapping scopes and must never be summed as independent holdings.
    pub async fn allocations(&self, chain_id: u64, vault: &str) -> Result<VaultAllocations> {
        super::profile::profile(chain_id)?;
        let vault = normalized_address(vault)?;
        let envelope: Envelope<VaultAllocations> = self
            .rest(
                format!("/v0/vaults-v2/{chain_id}:{vault}/allocations"),
                vec![],
            )
            .await?;
        let data = envelope.data;
        if data.chain_id != chain_id || !data.vault_address.eq_ignore_ascii_case(&vault) {
            return Err(invalid(
                "Morpho allocations returned another chain or vault.",
            ));
        }
        data.last_indexed_block
            .parse::<u64>()
            .map_err(|_| invalid("Morpho allocation observation block is invalid."))?;
        let mut adapters = std::collections::BTreeSet::new();
        for allocation in &data.allocations {
            let adapter = normalized_address(&allocation.adapter_address)
                .map_err(|_| invalid("Morpho allocation adapter identity is invalid."))?;
            if !adapters.insert(adapter) {
                return Err(invalid("Morpho allocations returned a duplicate adapter."));
            }
            if allocation.adapter_kind.is_empty() || allocation.target.kind.is_empty() {
                return Err(invalid(
                    "Morpho allocation adapter or target kind is missing.",
                ));
            }
            validate_allocation_caps(&allocation.caps)?;
        }
        validate_allocation_caps(&data.unscoped_caps)?;
        Ok(data)
    }
    pub async fn positions(
        &self,
        chain_id: u64,
        owner: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ApiPage<IndexedPosition>> {
        let owner = normalized_address(owner)?;
        let mut query = page_query(chain_id, limit, cursor)?;
        query.push(("active_only".into(), "true".into()));
        let page: ApiPage<IndexedPosition> = self
            .rest(format!("/v1/vaults-v2/users/{owner}/positions"), query)
            .await?;
        validate_cursor(page.cursor.as_deref())?;
        for row in &page.data {
            if row.chain_id != chain_id {
                return Err(invalid("Morpho positions returned another chain."));
            }
            normalized_address(&row.vault_address)?;
            if let Some(user) = &row.user_address
                && !user.eq_ignore_ascii_case(&owner)
            {
                return Err(invalid("Morpho positions returned another owner."));
            }
        }
        Ok(page)
    }
    pub async fn performance(&self, chain_id: u64, vault: &str, owner: &str) -> Result<Value> {
        let vault = normalized_address(vault)?;
        let owner = normalized_address(owner)?;
        let envelope: Envelope<Value> = self
            .rest(
                format!("/v0/vaults-v2/{chain_id}:{vault}/users/{owner}/position/performance"),
                vec![],
            )
            .await?;
        let d = &envelope.data;
        if d["chain_id"].as_u64() != Some(chain_id)
            || !d["vault_address"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(&vault))
            || !d["user_address"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(&owner))
        {
            return Err(invalid(
                "Morpho performance returned another owner, chain or vault.",
            ));
        }
        Ok(envelope.data)
    }
    pub async fn analytics(&self, chain_id: u64, vault: &str) -> Result<Value> {
        super::profile::profile(chain_id)?;
        let vault = normalized_address(vault)?;
        let data = self.graphql("query GoFinVaultV2($address: String!, $chainId: Int!) { vaultV2ByAddress(address: $address, chainId: $chainId) { address name symbol apy netApy netApyExcludingRewards totalAssetsUsd rewards { supplyApr asset { address symbol } } } }",json!({"address":vault,"chainId":chain_id})).await?;
        let row = data
            .get("vaultV2ByAddress")
            .filter(|v| v.is_object())
            .ok_or_else(|| invalid("Morpho analytics did not return a vault object."))?;
        if !row["address"]
            .as_str()
            .is_some_and(|address| address.eq_ignore_ascii_case(&vault))
        {
            return Err(invalid(
                "Morpho analytics returned a different vault address.",
            ));
        }
        for field in ["apy", "netApy", "netApyExcludingRewards", "totalAssetsUsd"] {
            if row
                .get(field)
                .is_some_and(|v| !v.is_null() && !v.is_number())
            {
                return Err(invalid(
                    "Morpho analytics rate or value has an invalid shape.",
                ));
            }
        }
        if let Some(rewards) = row.get("rewards").filter(|v| !v.is_null()) {
            let rewards = rewards
                .as_array()
                .ok_or_else(|| invalid("Morpho analytics rewards must be an array or unknown."))?;
            for reward in rewards {
                if !reward.is_object()
                    || reward
                        .get("supplyApr")
                        .is_some_and(|v| !v.is_null() && !v.is_number())
                {
                    return Err(invalid(
                        "Morpho analytics reward rate has an invalid shape.",
                    ));
                }
                normalized_address(reward["asset"]["address"].as_str().ok_or_else(|| {
                    invalid("Morpho analytics reward asset identity is missing.")
                })?)?;
            }
        }
        Ok(data)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiPage<T> {
    pub data: Vec<T>,
    pub cursor: Option<String>,
}
#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultListing {
    pub chain_id: u64,
    pub vault_address: String,
    pub version: String,
    pub name: String,
    pub symbol: String,
    pub asset_address: String,
    pub factory_address: String,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultDetails {
    pub chain_id: u64,
    pub address: String,
    pub version: String,
    pub name: String,
    pub symbol: String,
    pub asset: IndexedAsset,
    pub factory_address: String,
    pub last_indexed_block: String,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedAsset {
    pub address: String,
    pub decimals: u8,
    pub symbol: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultIndexedState {
    pub chain_id: u64,
    pub address: String,
    pub last_indexed_block: String,
    pub total_assets: Quantity,
    pub total_supply: Quantity,
    pub withdrawable_assets: Quantity,
    pub idle_assets: Quantity,
    pub allocated_assets: Quantity,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultAllocations {
    pub chain_id: u64,
    pub vault_address: String,
    pub last_indexed_block: String,
    pub allocations: Vec<AdapterAllocation>,
    pub unscoped_caps: Vec<AllocationCap>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdapterAllocation {
    pub adapter_address: String,
    pub adapter_kind: String,
    pub target: AllocationTarget,
    pub caps: Vec<AllocationCap>,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AllocationTarget {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AllocationCap {
    pub cap_id: String,
    pub cap_data: String,
    pub cap_type: String,
    pub allocated_assets: Quantity,
    pub absolute_cap: Quantity,
    pub relative_cap_wad: Quantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collateral_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_supply_assets: Option<Quantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_assets: Option<Quantity>,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedPosition {
    pub chain_id: u64,
    pub vault_address: String,
    #[serde(default)]
    pub user_address: Option<String>,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}

fn validate_allocation_caps(caps: &[AllocationCap]) -> Result<()> {
    let mut identities = std::collections::BTreeSet::new();
    for cap in caps {
        let id = gofin_evm::hash(&cap.cap_id)
            .map_err(|_| invalid("Morpho allocation cap identity is invalid."))?;
        let data = gofin_evm::rpc_bytes(&json!(cap.cap_data))
            .map_err(|_| invalid("Morpho allocation cap data is invalid."))?;
        if gofin_evm::alloy::primitives::keccak256(data) != id
            || !identities.insert(id)
            || cap.cap_type.is_empty()
        {
            return Err(invalid(
                "Morpho allocation cap identity is mismatched or repeated.",
            ));
        }
        if let Some(market) = &cap.market_id {
            gofin_evm::hash(market)
                .map_err(|_| invalid("Morpho allocation market identity is invalid."))?;
        }
        if let Some(collateral) = &cap.collateral_address {
            normalized_address(collateral)
                .map_err(|_| invalid("Morpho allocation collateral identity is invalid."))?;
        }
    }
    Ok(())
}

fn page_query(chain: u64, limit: u32, cursor: Option<&str>) -> Result<Vec<(String, String)>> {
    super::profile::profile(chain)?;
    if !(1..=100).contains(&limit) {
        return Err(invalid("Page limit must be between 1 and 100."));
    }
    validate_cursor(cursor)?;
    let mut q = vec![
        ("chain_ids".into(), chain.to_string()),
        ("limit".into(), limit.to_string()),
    ];
    if let Some(cursor) = cursor {
        q.push(("cursor".into(), cursor.into()));
    }
    Ok(q)
}
fn validate_cursor(cursor: Option<&str>) -> Result<()> {
    if cursor.is_some_and(|c| c.is_empty() || c.len() > 8192 || c.chars().any(char::is_control)) {
        return Err(invalid("Invalid pagination cursor."));
    }
    Ok(())
}
fn validate_vault(chain: u64, vault: &str, version: &str, expected_chain: u64) -> Result<()> {
    normalized_address(vault)?;
    if chain != expected_chain || !matches!(version, "2.0" | "2") {
        return Err(invalid(
            "Morpho response is not the selected chain and Vault V2 version.",
        ));
    }
    Ok(())
}
pub(crate) fn normalized_address(value: &str) -> Result<String> {
    let a = gofin_evm::address(value)?;
    if a == gofin_evm::alloy::primitives::Address::ZERO {
        return Err(invalid("A nonzero address is required."));
    }
    Ok(gofin_evm::lower_address(a))
}
fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "MORPHO_API_UNAVAILABLE",
        "read",
        message,
        "Retry the public read. An unavailable indexer is not an empty portfolio.",
    )
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "MORPHO_API_INVALID",
        "read",
        message,
        "Inspect the selected chain, vault and API availability.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const VAULT: &str = "0x1111111111111111111111111111111111111111";
    struct Fixture(Value);
    #[async_trait]
    impl MorphoApiTransport for Fixture {
        async fn request(&self, _: MorphoApiRequest) -> Result<Value> {
            Ok(self.0.clone())
        }
    }
    #[tokio::test]
    async fn graphql_partial_errors_do_not_become_empty_success() {
        let c = MorphoClient::with_transport(Arc::new(Fixture(
            json!({"data":{},"errors":[{"message":"unavailable"}]}),
        )));
        assert!(c.graphql("query {}", json!({})).await.is_err());
    }
    #[tokio::test]
    async fn directory_rejects_wrong_chain_or_v1_and_keeps_cursor() {
        let row = json!({"chain_id":8453,"vault_address":"0x1111111111111111111111111111111111111111","version":"2.0","name":"Vault","symbol":"V","asset_address":"0x2222222222222222222222222222222222222222","factory_address":"0x3333333333333333333333333333333333333333"});
        let c =
            MorphoClient::with_transport(Arc::new(Fixture(json!({"data":[row],"cursor":"next"}))));
        assert_eq!(
            c.vaults(8453, 20, None, None)
                .await
                .unwrap()
                .cursor
                .as_deref(),
            Some("next")
        );
        assert!(c.vaults(1, 20, None, None).await.is_err());
        let mut v1 = row;
        v1["version"] = json!("1.1");
        let c = MorphoClient::with_transport(Arc::new(Fixture(json!({"data":[v1],"cursor":null}))));
        assert!(c.vaults(8453, 20, None, None).await.is_err());
    }
    #[test]
    fn exact_quantities_reject_float_responses() {
        let s = json!({"chain_id":1,"address":"v","last_indexed_block":"1","total_assets":1.5,"total_supply":"1","withdrawable_assets":"1","idle_assets":"1","allocated_assets":"0"});
        assert!(serde_json::from_value::<VaultIndexedState>(s).is_err());
    }
    fn allocation_fixture() -> Value {
        let cap = |byte: u8, kind: &str| json!({"cap_id":gofin_evm::alloy::primitives::keccak256([byte]).to_string(),"cap_data":format!("0x{byte:02x}"),"cap_type":kind,"allocated_assets":"100","absolute_cap":"1000","relative_cap_wad":"1000000000000000000"});
        json!({"data":{"chain_id":1,"vault_address":VAULT,"last_indexed_block":"123","allocations":[{"adapter_address":"0x2222222222222222222222222222222222222222","adapter_kind":"morpho_market_v1_v2","target":{"type":"blue_market"},"caps":[cap(1,"adapter"),cap(2,"market_v1")]}],"unscoped_caps":[cap(3,"collateral")]}})
    }
    #[tokio::test]
    async fn allocation_read_preserves_separate_overlapping_cap_scopes() {
        let client = MorphoClient::with_transport(Arc::new(Fixture(allocation_fixture())));
        let data = client.allocations(1, VAULT).await.unwrap();
        assert_eq!(data.last_indexed_block, "123");
        assert_eq!(data.allocations[0].caps.len(), 2);
        assert_eq!(data.unscoped_caps.len(), 1);
        assert_eq!(data.allocations[0].caps[0].cap_type, "adapter");
        assert_eq!(data.allocations[0].caps[1].cap_type, "market_v1");
        assert_eq!(data.unscoped_caps[0].cap_type, "collateral");
        // Every cap describes the same 100 assets at a different scope.
        assert_eq!(data.allocations[0].caps[0].allocated_assets.as_str(), "100");
        assert!(
            serde_json::to_value(data)
                .unwrap()
                .get("total_assets")
                .is_none()
        );
    }
    #[tokio::test]
    async fn allocation_read_rejects_wrong_identity_and_malformed_observations() {
        for (path, replacement) in [
            ("/data/chain_id", json!(8453)),
            (
                "/data/vault_address",
                json!("0x3333333333333333333333333333333333333333"),
            ),
            ("/data/last_indexed_block", json!("unknown")),
            ("/data/allocations", Value::Null),
            ("/data/unscoped_caps", Value::Null),
            ("/data/allocations/0/adapter_address", json!("invalid")),
            ("/data/allocations/0/target", json!([])),
            ("/data/allocations/0/caps/0/allocated_assets", json!(1.5)),
            ("/data/allocations/0/caps/0/cap_data", json!("0x04")),
        ] {
            let mut fixture = allocation_fixture();
            *fixture.pointer_mut(path).unwrap() = replacement;
            let client = MorphoClient::with_transport(Arc::new(Fixture(fixture)));
            assert_eq!(
                client.allocations(1, VAULT).await.unwrap_err().code,
                "MORPHO_API_INVALID",
                "{path}"
            );
        }
    }
    struct Unavailable;
    #[async_trait]
    impl MorphoApiTransport for Unavailable {
        async fn request(&self, _: MorphoApiRequest) -> Result<Value> {
            Err(unavailable("Synthetic allocation indexer outage."))
        }
    }
    #[tokio::test]
    async fn allocation_outage_does_not_become_an_empty_list() {
        let client = MorphoClient::with_transport(Arc::new(Unavailable));
        assert_eq!(
            client.allocations(1, VAULT).await.unwrap_err().code,
            "MORPHO_API_UNAVAILABLE"
        );
    }
    #[tokio::test]
    async fn analytics_requires_the_requested_nonnull_vault_and_preserves_unknown_rates() {
        for row in [
            Value::Null,
            json!({"address":"0x2222222222222222222222222222222222222222"}),
            json!({"address":VAULT,"apy":"not-a-rate"}),
            json!({"address":VAULT,"rewards":{}}),
        ] {
            let client = MorphoClient::with_transport(Arc::new(Fixture(
                json!({"data":{"vaultV2ByAddress":row}}),
            )));
            assert!(client.analytics(1, VAULT).await.is_err());
        }
        let client = MorphoClient::with_transport(Arc::new(Fixture(
            json!({"data":{"vaultV2ByAddress":{"address":VAULT,"apy":null,"rewards":null}}}),
        )));
        let unknown = client.analytics(1, VAULT).await.unwrap();
        assert_eq!(unknown["vaultV2ByAddress"]["apy"], Value::Null);
        assert_eq!(unknown["vaultV2ByAddress"]["rewards"], Value::Null);
    }
}
