//! Reviewed Aave V3 deployment identities. Dynamic reads never add authority.
use super::super::*;

pub const ADDRESS_BOOK_COMMIT: &str = "6a83d11893d6687bf6b9fd091a2a5320b3e1966e";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketProfile {
    pub chain_id: u64,
    pub name: String,
    pub pool: String,
    pub oracle: String,
    pub addresses_provider: String,
}

pub fn reviewed_markets(chain_id: u64) -> Vec<MarketProfile> {
    let profiles = [
        (
            1,
            "Ethereum Core",
            "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2",
            "0x54586be62e3c3580375ae3723c145253060ca0c2",
            "0x2f39d218133afab8f2b819b1066c7e434ad94e9e",
        ),
        (
            1,
            "Ethereum Lido",
            "0x4e033931ad43597d96d6bcc25c280717730b58b1",
            "0xe3c061981870c0c7b1f3c4f4bb36b95f1f260be6",
            "0xcfbf336fe147d643b9cb705648500e101504b16d",
        ),
        (
            1,
            "Ethereum EtherFi",
            "0x0aa97c284e98396202b6a04024f5e2c65026f3c0",
            "0x43b64f28a678944e0655404b0b98e443851cc34f",
            "0xeba440b438ad808101d1c451c1c5322c90befcda",
        ),
        (
            1,
            "Ethereum Horizon",
            "0xae05cd22df81871bc7cc2a04becfb516bfe332c8",
            "0x985bcfab7e0f4ef2606cc5b64fc1a16311880442",
            "0x5d39e06b825c1f2b80bf2756a73e28efaa128ba0",
        ),
        (
            8453,
            "Base",
            "0xa238dd80c259a72e81d7e4664a9801593f98d1c5",
            "0x2cc0fc26ed4563a5ce5e8bdcfe1a2878676ae156",
            "0xe20fcbdbffc4dd138ce8b2e6fbb6cb49777ad64d",
        ),
        (
            42161,
            "Arbitrum One",
            "0x794a61358d6845594f94dc1db02a252b5b4814ad",
            "0xb56c2f0b653b2e0b10c9b928c8580ac5df02c7c7",
            "0xa97684ead0e402dc232d5a977953df7ecbab3cdb",
        ),
    ];
    profiles
        .into_iter()
        .filter(|(chain, ..)| *chain == chain_id)
        .map(
            |(chain_id, name, pool, oracle, addresses_provider)| MarketProfile {
                chain_id,
                name: name.into(),
                pool: pool.into(),
                oracle: oracle.into(),
                addresses_provider: addresses_provider.into(),
            },
        )
        .collect()
}

impl EthereumAdapter {
    /// Select only protected configured market identities; preserve the default
    /// and every transport/signing dependency. No agent-supplied RPC or oracle.
    pub(crate) fn selected_market(&self, input: &Value) -> Result<Option<Self>> {
        let Some(pool) = input.get("market") else {
            return Ok(None);
        };
        let pool = pool
            .as_str()
            .ok_or_else(|| FinError::validation("Market must be an exact pool address."))?;
        let pool = lower_address(address(pool)?);
        if self.config.aave.as_ref().is_some_and(|m| m.pool == pool) {
            return Ok(None);
        }
        let market = self
            .config
            .aave_markets
            .iter()
            .find(|m| m.pool == pool)
            .ok_or_else(|| {
                error(
                    "AAVE_MARKET_UNAVAILABLE",
                    "validation",
                    "This pool is not an enabled Aave market on the selected network.",
                )
            })?;
        let mut config = self.config.clone();
        config.aave = Some(market.clone());
        Ok(Some(Self {
            config,
            wallets: self.wallets.clone(),
            runtime: self.runtime.clone(),
            clock: self.clock.clone(),
            oracle_clock: self.oracle_clock.clone(),
            wallet_valuation: self.wallet_valuation.clone(),
            token_discovery: self.token_discovery.clone(),
        }))
    }
    pub(crate) fn compatible_pool_revision(&self, revision: U256) -> bool {
        if self.horizon_math() {
            revision == U256::from(7)
        } else {
            [U256::from(10), U256::from(11)].contains(&revision)
        }
    }
    pub(crate) async fn verify_receipt_identity(
        &self,
        token: &reads::Token,
        reserve: &reads::Reserve,
        block: Option<u64>,
    ) -> Result<()> {
        let pool_revision = self
            .runtime
            .call(
                address(&self.aave()?.pool)?,
                &abi::Pool::POOL_REVISIONCall {},
                block,
            )
            .await?;
        require(
            self.compatible_pool_revision(pool_revision),
            "AAVE_POOL_REVISION_UNSUPPORTED",
            "Selected pool revision has no reviewed exact risk kernel.",
        )?;
        for receipt in [&reserve.a_token, &reserve.variable_debt_token] {
            let receipt = address(receipt)?;
            require(
                self.runtime
                    .call(receipt, &abi::AToken::POOLCall {}, block)
                    .await?
                    == address(&self.aave()?.pool)?
                    && self
                        .runtime
                        .call(
                            receipt,
                            &abi::AToken::UNDERLYING_ASSET_ADDRESSCall {},
                            block,
                        )
                        .await?
                        == address(&token.reference)?,
                "AAVE_RECEIPT_IDENTITY_MISMATCH",
                "Receipt token does not bind the selected pool and underlying asset.",
            )?;
        }
        Ok(())
    }
    pub(crate) fn market_configs(&self) -> Vec<AaveConfig> {
        let mut configs: Vec<AaveConfig> = self.config.aave.iter().cloned().collect();
        for market in &self.config.aave_markets {
            if !configs.iter().any(|m| m.pool == market.pool) {
                configs.push(market.clone());
            }
        }
        configs
    }
}
