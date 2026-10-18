//! Owned-fork Merkl entitlement fixture. Never included by installed binaries.
//! Production calldata, proxy identities, proof checking and settlement remain
//! unchanged. The runner installs this one synthetic entitlement in fork state.
use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_ethereum::morpho::api::{MorphoApiRequest, MorphoApiTransport, MorphoClient};
use gofin_evm::{address, lower_address};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::time::Duration;

pub struct MorphoForkRewards {
    chain: u64,
    owner: String,
    endpoint: String,
    instance: String,
    anchor: String,
    public: MorphoClient,
}

impl MorphoForkRewards {
    pub fn new(
        chain: u64,
        owner: String,
        endpoint: String,
        instance: String,
        anchor: String,
    ) -> Self {
        Self {
            chain,
            owner,
            endpoint,
            instance,
            anchor,
            public: MorphoClient::new(),
        }
    }

    async fn guard(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.endpoint)
            .map_err(|_| fixture_error("Invalid fork fixture endpoint."))?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(self.chain, 1 | 8453)
        {
            return Err(fixture_error(
                "Morpho reward fixture requires a numeric owned loopback fork on Ethereum or Base.",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| fixture_error("Fork identity transport unavailable."))?;
        let response: Value = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"anvil_metadata","params":[]}))
            .send()
            .await
            .map_err(|_| fixture_error("Owned fork identity unavailable."))?
            .error_for_status()
            .map_err(|_| fixture_error("Owned fork identity request failed."))?
            .json()
            .await
            .map_err(|_| fixture_error("Owned fork identity is malformed."))?;
        let metadata = &response["result"];
        if response.get("error").is_some()
            || metadata["chainId"] != json!(self.chain)
            || metadata["instanceId"] != self.instance
            || metadata["forkedNetwork"]["forkBlockHash"] != self.anchor
        {
            return Err(fixture_error(
                "Owned fork identity changed; no synthetic reward data was returned.",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl MorphoApiTransport for MorphoForkRewards {
    async fn request(&self, request: MorphoApiRequest) -> Result<Value> {
        self.guard().await?;
        match request {
            MorphoApiRequest::MerklRewards { chain_id, owner } => {
                if chain_id != self.chain || address(&owner)? != address(&self.owner)? {
                    return Err(fixture_error(
                        "Synthetic reward query changed the owned fixture's chain or wallet.",
                    ));
                }
                let token = match self.chain {
                    1 => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    8453 => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                    _ => return Err(fixture_error("Unsupported fork reward chain.")),
                };
                Ok(
                    json!([{"chain":{"id":chain_id},"rewards":[{"token":{"address":token,"symbol":"USDC","decimals":6},"amount":"1000000","claimed":"0","pending":"50000","proofs":[],"breakdowns":[{"identifier":"synthetic-morpho-fork-entitlement","amount":"750000"},{"identifier":"synthetic-other-program-entitlement","amount":"250000"}],"fixture":{"owner":lower_address(address(&self.owner)?),"evidence":"Synthetic entitlement on owned deployed Merkl distributor; not live rewards"}}]}]),
                )
            }
            other => self.public.request(other).await,
        }
    }
}

#[async_trait]
impl UsdPriceSource for MorphoForkRewards {
    async fn quote(&self, asset: &gofin_domain::AssetId) -> Result<UsdQuote> {
        self.guard().await?;
        let usdc = match self.chain {
            1 => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            8453 => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            _ => return Err(fixture_error("Unsupported fork valuation chain.")),
        };
        let price = match (
            asset.network.as_str(),
            asset.reference.as_str(),
            asset.decimals,
        ) {
            (network, "native", 18) if network == format!("ethereum:{}", self.chain) => "3000",
            (network, reference, 6)
                if network == format!("ethereum:{}", self.chain) && reference == usdc =>
            {
                "1"
            }
            _ => {
                return Err(fixture_error(
                    "Asset is outside the exact owned-fork valuation fixture.",
                ));
            }
        };
        let observed = gofin_domain::now();
        Ok(UsdQuote {
            usd_per_unit: price.into(),
            updated_at: observed.clone(),
            observed_at: observed,
            source: "Synthetic fixed valuation on independently owned Morpho fork; not public price evidence".into(),
        })
    }
}

fn fixture_error(message: &str) -> GoFinError {
    GoFinError::new(
        "MORPHO_FORK_FIXTURE_INVALID",
        "fixture",
        message,
        "Inspect the exact owned fork identity; never redirect fixture requests to production.",
    )
}
