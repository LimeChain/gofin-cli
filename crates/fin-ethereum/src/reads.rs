use super::*;
use abi::*;
use fin_evm::alloy::{
    primitives::{
        B256,
        aliases::{U24, U160},
    },
    sol_types::{SolCall, SolValue},
};
use fin_evm::{quantity, rpc_bytes, rpc_u64, u256};

fn configured_quote_router(adapter: &EthereumAdapter, protocol: &str) -> Result<String> {
    let deployment = adapter.uniswap()?;
    let router = if protocol == "v4" {
        deployment
            .v4
            .as_ref()
            .ok_or_else(|| {
                error(
                    "OPERATION_UNAVAILABLE",
                    "validation",
                    "Uniswap v4 has no configured router on this network.",
                )
            })?
            .router
            .clone()
    } else {
        super::uniswap::profile::V3DeploymentProfile::uniswap(deployment)
            .router
            .to_owned()
    };
    Ok(lower_address(address(&router)?))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Token {
    pub reference: String,
    pub decimals: u8,
    pub symbol: String,
}
impl Token {
    pub(crate) fn asset(&self, a: &EthereumAdapter) -> AssetId {
        a.asset(&self.reference, self.decimals)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reserve {
    pub token: Token,
    pub a_token: String,
    pub stable_debt_token: String,
    pub variable_debt_token: String,
    pub active: bool,
    pub frozen: bool,
    pub paused: bool,
    pub borrowing: bool,
    pub collateral_eligible: bool,
    pub ltv: Quantity,
    pub liquidation_threshold: Quantity,
    pub borrow_cap: Quantity,
    pub supply_cap: Quantity,
    pub total_supply: Quantity,
    pub total_debt: Quantity,
    pub liquidity: Quantity,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserReserve {
    pub token: Token,
    pub supplied: Quantity,
    pub stable_debt: Quantity,
    pub variable_debt: Quantity,
    pub scaled_variable_debt: Quantity,
    pub collateral_enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapQuote {
    pub token_in: Token,
    pub token_out: Token,
    pub input: Quantity,
    pub output: Quantity,
    pub pool: String,
    #[serde(default)]
    pub dex: String,
    pub fee_tier: u32,
    #[serde(default)]
    pub tick_spacing: Option<i32>,
    #[serde(default)]
    pub swap_fee_ppm: Option<u32>,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub pool_key: Option<super::V4PoolKey>,
    #[serde(default)]
    pub pool_token_in: String,
    #[serde(default)]
    pub pool_token_out: String,
    pub price_impact_bps: u32,
    pub observed_at: String,
    pub block_number: Option<u64>,
    pub block_hash: Option<String>,
    #[serde(default)]
    pub router: Option<String>,
    #[serde(default)]
    pub permit2: Option<String>,
    #[serde(default)]
    pub permission_strategy: Option<String>,
    #[serde(default)]
    pub permission_verification: Option<String>,
    #[serde(default)]
    pub permission_requirement: Option<fin_amm::PermissionRequirement>,
}

pub(crate) struct ProtocolQuoteRequest<'a> {
    pub dex: &'a str,
    pub input: &'a str,
    pub output: &'a str,
    pub amount: &'a str,
    pub fee_tier: Option<u32>,
    pub tick_spacing: Option<i32>,
    pub protocol: Option<&'a str>,
    pub v4_key: Option<&'a super::V4PoolKey>,
    pub block: Option<u64>,
}

impl EthereumAdapter {
    async fn wallet_usd_valuation_metadata(&self, token: &Token) -> Value {
        let asset = token.asset(self);
        let asset_value = json!(asset);
        let Some(provider) = &self.wallet_valuation else {
            return json!({
                "available":false,
                "asset":asset_value,
                "error_code":"VALUATION_UNAVAILABLE"
            });
        };
        let units = match pow10(token.decimals) {
            Ok(units) => quantity(units),
            Err(error) => {
                return json!({
                    "available":false,
                    "asset":asset_value,
                    "error_code":error.code
                });
            }
        };
        let (value, source, observed_at) = match provider.quote_observation(&asset, units).await {
            Ok(observation) => observation,
            Err(error) => {
                return json!({
                    "available":false,
                    "asset":asset_value,
                    "error_code":error.code
                });
            }
        };
        if source.trim().is_empty() || value.is_zero() {
            return json!({
                "available":false,
                "asset":asset_value,
                "error_code":"VALUATION_UNAVAILABLE"
            });
        }
        let Ok(observed) = chrono::DateTime::parse_from_rfc3339(&observed_at) else {
            return json!({
                "available":false,
                "asset":asset_value,
                "error_code":"VALUATION_UNAVAILABLE"
            });
        };
        let current = self.clock.now();
        if observed > current + chrono::Duration::seconds(5)
            || current.signed_duration_since(observed) > chrono::Duration::seconds(120)
        {
            return json!({
                "available":false,
                "asset":asset_value,
                "error_code":"VALUATION_UNAVAILABLE"
            });
        }
        json!({
            "available":true,
            "asset":asset_value,
            "usd_micros_per_unit":value,
            "observed_at":observed_at,
            "source":source
        })
    }

    /// Return an EIP-2612 nonce only when the token's own domain separator
    /// matches the standard chain- and verifying-contract-bound EIP-712 domain.
    pub(crate) async fn eip2612_nonce_at(
        &self,
        token: Address,
        owner: Address,
        block: u64,
    ) -> Option<(U256, B256)> {
        let block_tag = format!("{block:#x}");
        let call = |data: Vec<u8>| json!([{"to":lower_address(token),"data":fin_evm::hex_bytes(&data)},block_tag]);
        let nonce_bytes = rpc_bytes(
            &self
                .runtime
                .rpc
                .request("eth_call", call(Erc20::noncesCall { owner }.abi_encode()))
                .await
                .ok()?,
        )
        .ok()?;
        let nonce = Erc20::noncesCall::abi_decode_returns(&nonce_bytes).ok()?;
        let domain_bytes = rpc_bytes(
            &self
                .runtime
                .rpc
                .request(
                    "eth_call",
                    call(Erc20::DOMAIN_SEPARATORCall {}.abi_encode()),
                )
                .await
                .ok()?,
        )
        .ok()?;
        let domain = Erc20::DOMAIN_SEPARATORCall::abi_decode_returns(&domain_bytes).ok()?;
        let name_bytes = rpc_bytes(
            &self
                .runtime
                .rpc
                .request("eth_call", call(Erc20::nameCall {}.abi_encode()))
                .await
                .ok()?,
        )
        .ok()?;
        let name = Erc20::nameCall::abi_decode_returns(&name_bytes).ok()?;
        // Some deployed EIP-2612 tokens omit `version()`. Probe only the
        // standard default candidate `1` in that case, and accept it only if
        // the independently computed domain exactly matches DOMAIN_SEPARATOR.
        let version = self
            .runtime
            .rpc
            .request("eth_call", call(Erc20::versionCall {}.abi_encode()))
            .await
            .ok()
            .and_then(|value| rpc_bytes(&value).ok())
            .and_then(|bytes| Erc20::versionCall::abi_decode_returns(&bytes).ok())
            .unwrap_or_else(|| "1".to_owned());
        if name.is_empty() || version.is_empty() {
            return None;
        }
        let type_hash = fin_evm::alloy::primitives::keccak256(
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let expected = fin_evm::alloy::primitives::keccak256(
            (
                type_hash,
                fin_evm::alloy::primitives::keccak256(name.as_bytes()),
                fin_evm::alloy::primitives::keccak256(version.as_bytes()),
                U256::from(self.config.chain_id),
                token,
            )
                .abi_encode(),
        );
        (expected == domain).then_some((nonce, domain))
    }

    pub(crate) async fn swap_permission_requirement(
        &self,
        owner: Address,
        quote: &SwapQuote,
    ) -> Result<fin_amm::PermissionRequirement> {
        use fin_amm::PermissionRequirement::{None as NoPermission, Permit, TokenApproval};
        if quote.token_in.reference == "native" {
            return Ok(NoPermission);
        }
        let block = quote.block_number.expect("quote has pinned block");
        let token = address(&quote.token_in.reference)?;
        let owner_code = rpc_bytes(
            &self
                .runtime
                .rpc
                .request(
                    "eth_getCode",
                    json!([lower_address(owner), format!("{block:#x}")]),
                )
                .await?,
        )?;
        if quote.protocol == "v4" {
            let permit2 = address("0x000000000022d473030f116ddee9f6b43ac78ba3")?;
            let underlying = self
                .allowance(&quote.token_in, owner, permit2, Some(block))
                .await?;
            if underlying < u256(&quote.input)? || underlying == U256::MAX {
                return Ok(TokenApproval);
            }
            let router = address(quote.router.as_deref().expect("configured router"))?;
            let call = Permit2::allowanceCall {
                owner,
                token,
                spender: router,
            }
            .abi_encode();
            let response = self
                .runtime
                .rpc
                .request(
                    "eth_call",
                    json!([{"to":lower_address(permit2),"data":fin_evm::hex_bytes(&call)},format!("{block:#x}")]),
                )
                .await;
            if let Ok(response) = response
                && let Ok(bytes) = rpc_bytes(&response)
                && let Ok(state) = Permit2::allowanceCall::abi_decode_returns(&bytes)
                && U256::from(state.amount) >= u256(&quote.input)?
                && state.amount < fin_evm::alloy::primitives::aliases::U160::MAX
                && state.expiration.to::<u64>() > self.clock.now().timestamp().max(0) as u64
            {
                return Ok(NoPermission);
            }
            return Ok(if owner_code.is_empty() {
                Permit
            } else {
                TokenApproval
            });
        }
        let router = address(quote.router.as_deref().expect("configured router"))?;
        let allowance = self
            .allowance(&quote.token_in, owner, router, Some(block))
            .await?;
        if allowance >= u256(&quote.input)? && allowance < U256::MAX {
            return Ok(NoPermission);
        }
        if !owner_code.is_empty() {
            return Ok(TokenApproval);
        }
        Ok(
            if self.eip2612_nonce_at(token, owner, block).await.is_some() {
                Permit
            } else {
                TokenApproval
            },
        )
    }
}

fn spot_impact_bps(
    input: U256,
    output: U256,
    sqrt_price_x96: U160,
    zero_for_one: bool,
    fee_ppm: u32,
) -> u32 {
    use fin_evm::alloy::primitives::U512;
    let sqrt = U512::from(sqrt_price_x96);
    let square = sqrt * sqrt;
    let q192: U512 = U512::from(1u8) << 192;
    let spot: U512 = if zero_for_one {
        U512::from(input) * square / q192
    } else {
        U512::from(input) * q192 / square
    };
    let fee_denominator = U512::from(1_000_000u64);
    let fee_retained = fee_denominator.saturating_sub(U512::from(fee_ppm));
    if fee_retained.is_zero() {
        return 10_000;
    }
    // Remove the pool fee from the observed output so impact describes curve
    // movement; the fee remains independently bound by the selected tier.
    let actual = U512::from(output) * fee_denominator / fee_retained;
    if spot <= actual || spot.is_zero() {
        return 0;
    }
    (((spot - actual) * U512::from(10_000u64)) / spot)
        .try_into()
        .unwrap_or(10_000)
}

/// Decode Uniswap v4's packed directional protocol fee and combine it with
/// the pool's LP fee using ProtocolFeeLibrary.calculateSwapFee semantics.
fn v4_swap_fee_ppm(packed_protocol_fee: u32, lp_fee_ppm: u32, zero_for_one: bool) -> Result<u32> {
    let zero_for_one_fee = packed_protocol_fee & 0x0fff;
    let one_for_zero_fee = (packed_protocol_fee >> 12) & 0x0fff;
    require(
        zero_for_one_fee <= 1_000 && one_for_zero_fee <= 1_000,
        "V4_PROTOCOL_FEE_INVALID",
        "StateView returned an invalid packed Uniswap v4 protocol fee.",
    )?;
    require(
        lp_fee_ppm <= 1_000_000,
        "V4_LP_FEE_INVALID",
        "Uniswap v4 LP fee exceeds the pool fee denominator.",
    )?;
    let protocol_fee_ppm = if zero_for_one {
        zero_for_one_fee
    } else {
        one_for_zero_fee
    };
    let cross_fee = protocol_fee_ppm * lp_fee_ppm / 1_000_000;
    protocol_fee_ppm
        .checked_add(lp_fee_ppm)
        .and_then(|total| total.checked_sub(cross_fee))
        .ok_or_else(|| {
            error(
                "V4_SWAP_FEE_INVALID",
                "quote",
                "Combined Uniswap v4 protocol and LP fee is invalid.",
            )
        })
}

impl EthereumAdapter {
    pub(crate) async fn validate_v3_metadata(
        &self,
        profile: super::uniswap::profile::V3DeploymentProfile<'_>,
        block: Option<u64>,
    ) -> Result<()> {
        // Legacy profiles keep their existing checks and bindings. Additional
        // venues independently bind factory/router/quoter identities.
        if super::uniswap::profile::additional_dex(profile.dex_id).is_none() {
            return Ok(());
        }
        let observe = |target: Address| async move {
            let (_, factory, weth) = tokio::try_join!(
                self.runtime.code_at(target, block),
                self.runtime
                    .call(target, &V3DeploymentInfo::factoryCall {}, block),
                self.runtime
                    .call(target, &V3RouterInfo::WETH9Call {}, block),
            )?;
            require(
                factory == address(profile.factory)?,
                "DEPLOYMENT_MISMATCH",
                "V3 router or quoter factory differs from its reviewed configuration.",
            )?;
            require(
                weth != Address::ZERO
                    && self
                        .config
                        .weth
                        .as_ref()
                        .is_none_or(|expected| address(expected).is_ok_and(|a| a == weth)),
                "DEPLOYMENT_MISMATCH",
                "V3 wrapped-native identity differs from the configured network.",
            )?;
            self.runtime.code_at(weth, block).await?;
            if profile.router_metadata_abi == V3RouterMetadataAbi::PancakeV3 {
                let expected = address(
                    profile
                        .pool_deployer
                        .ok_or_else(|| FinError::validation("Pancake deployer is missing."))?,
                )?;
                let (_, actual) = tokio::try_join!(
                    self.runtime.code_at(expected, block),
                    self.runtime
                        .call(target, &V3DeploymentInfo::deployerCall {}, block)
                )?;
                require(
                    actual == expected,
                    "DEPLOYMENT_MISMATCH",
                    "Pancake router or quoter deployer differs from its reviewed configuration.",
                )?;
            }
            Ok::<_, FinError>(weth)
        };
        let (router_weth, quoter_weth) = tokio::try_join!(
            observe(address(profile.router)?),
            observe(address(profile.quoter)?)
        )?;
        require(
            router_weth == quoter_weth,
            "DEPLOYMENT_MISMATCH",
            "V3 router and quoter wrapped-native identities differ.",
        )?;
        Ok(())
    }
    pub async fn token(&self, reference: &str, block: Option<u64>) -> Result<Token> {
        if reference == "native" {
            return Ok(Token {
                reference: "native".into(),
                decimals: 18,
                symbol: "ETH".into(),
            });
        }
        let a = address(reference)?;
        require(
            a != Address::ZERO,
            "ZERO_ADDRESS",
            "Token identity must be a deployed nonzero address.",
        )?;
        self.runtime.code_at(a, block).await?;
        let decimals = self.runtime.call(a, &Erc20::decimalsCall {}, block).await?;
        pow10(decimals)?;
        let symbol = match self.runtime.call(a, &Erc20::symbolCall {}, block).await {
            Ok(s) => s,
            Err(_) => {
                let s = self
                    .runtime
                    .call(a, &Bytes32Symbol::symbolCall {}, block)
                    .await?;
                String::from_utf8(s.as_slice().to_vec())
                    .map_err(|_| {
                        error(
                            "TOKEN_METADATA_INVALID",
                            "read",
                            "Token symbol bytes are not UTF-8.",
                        )
                    })?
                    .trim_end_matches('\0')
                    .into()
            }
        };
        require(
            !symbol.is_empty() && symbol.len() <= 128 && !symbol.chars().any(char::is_control),
            "TOKEN_METADATA_INVALID",
            "Token symbol is not a valid bounded display label.",
        )?;
        Ok(Token {
            reference: lower_address(a),
            decimals,
            symbol,
        })
    }
    pub(crate) async fn balance_of(
        &self,
        token: &Token,
        owner: Address,
        block: Option<u64>,
    ) -> Result<U256> {
        if token.reference == "native" {
            self.runtime.balance(owner, block).await
        } else {
            self.runtime
                .call(
                    address(&token.reference)?,
                    &Erc20::balanceOfCall { owner },
                    block,
                )
                .await
        }
    }
    pub(crate) async fn allowance(
        &self,
        token: &Token,
        owner: Address,
        spender: Address,
        block: Option<u64>,
    ) -> Result<U256> {
        require(
            token.reference != "native",
            "NATIVE_ALLOWANCE_INVALID",
            "Native ETH has no ERC-20 allowance.",
        )?;
        self.runtime
            .call(
                address(&token.reference)?,
                &Erc20::allowanceCall { owner, spender },
                block,
            )
            .await
    }
    pub(crate) async fn data_provider(&self, block: Option<u64>) -> Result<Address> {
        let pool = address(&self.aave()?.pool)?;
        let oracle = address(&self.aave()?.oracle)?;
        self.runtime.code_at(pool, block).await?;
        self.runtime.code_at(oracle, block).await?;
        let provider = self
            .runtime
            .call(pool, &Pool::ADDRESSES_PROVIDERCall {}, block)
            .await?;
        self.runtime.code_at(provider, block).await?;
        require(
            self.runtime
                .call(provider, &AddressesProvider::getPoolCall {}, block)
                .await?
                == pool
                && self
                    .runtime
                    .call(provider, &AddressesProvider::getPriceOracleCall {}, block)
                    .await?
                    == oracle,
            "DEPLOYMENT_IDENTITY_MISMATCH",
            "Aave provider disagrees with configured pool or oracle.",
        )?;
        let data = self
            .runtime
            .call(
                provider,
                &AddressesProvider::getPoolDataProviderCall {},
                block,
            )
            .await?;
        self.runtime.code_at(data, block).await?;
        Ok(data)
    }
    pub(crate) async fn reserve(&self, reference: &str, block: Option<u64>) -> Result<Reserve> {
        let token = self.token(reference, block).await?;
        require(
            token.reference != "native",
            "AAVE_NATIVE_UNSUPPORTED",
            "Wrap native ETH before using this Aave reserve.",
        )?;
        let asset = address(&token.reference)?;
        let data = self.data_provider(block).await?;
        let config = self
            .runtime
            .call(
                data,
                &DataProvider::getReserveConfigurationDataCall { asset },
                block,
            )
            .await?;
        require(
            config.decimals == U256::from(token.decimals),
            "RESERVE_PRECISION_MISMATCH",
            "Aave reserve precision disagrees with token identity.",
        )?;
        let receipts = self
            .runtime
            .call(
                data,
                &DataProvider::getReserveTokensAddressesCall { asset },
                block,
            )
            .await?;
        require(
            receipts.aTokenAddress != Address::ZERO
                && receipts.variableDebtTokenAddress != Address::ZERO,
            "RESERVE_UNAVAILABLE",
            "Aave reserve has no supplied/debt representation.",
        )?;
        self.runtime.code_at(receipts.aTokenAddress, block).await?;
        self.runtime
            .code_at(receipts.variableDebtTokenAddress, block)
            .await?;
        if receipts.stableDebtTokenAddress != Address::ZERO {
            self.runtime
                .code_at(receipts.stableDebtTokenAddress, block)
                .await?;
        }
        let caps = self
            .runtime
            .call(data, &DataProvider::getReserveCapsCall { asset }, block)
            .await?;
        let paused = self
            .runtime
            .call(data, &DataProvider::getPausedCall { asset }, block)
            .await?;
        let total_supply = self
            .runtime
            .call(receipts.aTokenAddress, &Erc20::totalSupplyCall {}, block)
            .await?;
        let variable = self
            .runtime
            .call(
                receipts.variableDebtTokenAddress,
                &Erc20::totalSupplyCall {},
                block,
            )
            .await?;
        let stable = if receipts.stableDebtTokenAddress == Address::ZERO {
            U256::ZERO
        } else {
            self.runtime
                .call(
                    receipts.stableDebtTokenAddress,
                    &Erc20::totalSupplyCall {},
                    block,
                )
                .await?
        };
        let liquidity = self
            .runtime
            .call(
                asset,
                &Erc20::balanceOfCall {
                    owner: receipts.aTokenAddress,
                },
                block,
            )
            .await?;
        Ok(Reserve {
            token,
            a_token: lower_address(receipts.aTokenAddress),
            stable_debt_token: lower_address(receipts.stableDebtTokenAddress),
            variable_debt_token: lower_address(receipts.variableDebtTokenAddress),
            active: config.isActive,
            frozen: config.isFrozen,
            paused,
            borrowing: config.borrowingEnabled,
            collateral_eligible: config.usageAsCollateralEnabled,
            ltv: quantity(config.ltv),
            liquidation_threshold: quantity(config.liquidationThreshold),
            borrow_cap: quantity(caps.borrowCap),
            supply_cap: quantity(caps.supplyCap),
            total_supply: quantity(total_supply),
            total_debt: quantity(add(variable, stable)?),
            liquidity: quantity(liquidity),
        })
    }
    pub(crate) async fn user_reserve(
        &self,
        token: &Token,
        owner: Address,
        block: Option<u64>,
    ) -> Result<UserReserve> {
        let data = self.data_provider(block).await?;
        let u = self
            .runtime
            .call(
                data,
                &DataProvider::getUserReserveDataCall {
                    asset: address(&token.reference)?,
                    user: owner,
                },
                block,
            )
            .await?;
        Ok(UserReserve {
            token: token.clone(),
            supplied: quantity(u.currentATokenBalance),
            stable_debt: quantity(u.currentStableDebt),
            variable_debt: quantity(u.currentVariableDebt),
            scaled_variable_debt: quantity(u.scaledVariableDebt),
            collateral_enabled: u.usageAsCollateralEnabled,
        })
    }
    pub(crate) async fn wallet_tokens(
        &self,
        owner: &str,
        explicit: Option<&Vec<String>>,
    ) -> Result<TokenDiscoveryResult> {
        let mut result = if let Some(tokens) = explicit {
            TokenDiscoveryResult {
                candidates: tokens.clone(),
                complete: false,
                source: "explicit-token-scope".into(),
                warnings: vec!["Only requested tokens are included.".into()],
            }
        } else if let Some(provider) = &self.token_discovery {
            provider.tokens(self.config.chain_id, owner).await?
        } else {
            TokenDiscoveryResult { candidates: self.config.snapshot_tokens.clone(), complete: false, source: "configured-token-scope".into(), warnings: vec!["Automatic token discovery is unavailable; only configured tokens are included.".into()] }
        };
        require(
            !result.source.trim().is_empty()
                && result.candidates.len() <= 64
                && result.warnings.len() <= 64,
            "TOKEN_DISCOVERY_INVALID",
            "Token discovery must report a bounded scope and source.",
        )?;
        let mut discovered = BTreeSet::new();
        let mut normalized = Vec::new();
        for candidate in result.candidates {
            let token = address(&candidate)?;
            require(
                token != Address::ZERO,
                "TOKEN_DISCOVERY_INVALID",
                "Discovered ERC-20 addresses must be nonzero.",
            )?;
            let reference = lower_address(token);
            let unique = discovered.insert(reference.clone());
            require(
                explicit.is_none() || unique,
                "DUPLICATE_TOKEN",
                "Requested holdings duplicate an immutable ERC-20 identity.",
            )?;
            if unique {
                normalized.push(reference);
            }
        }
        if explicit.is_some() {
            result.candidates = normalized;
        } else {
            // Keep the user's configured scope first; bounded indexer noise
            // must not crowd out explicitly tracked holdings or deny all reads.
            let mut candidates = self.config.snapshot_tokens.clone();
            let mut seen: BTreeSet<String> = candidates.iter().cloned().collect();
            let mut truncated = false;
            for token in discovered {
                if !seen.insert(token.clone()) {
                    continue;
                }
                if candidates.len() < 64 {
                    candidates.push(token);
                } else {
                    truncated = true;
                }
            }
            if truncated {
                result.complete = false;
                result.warnings.push("Token discovery exceeded the 64-token read scope; configured tokens were retained and additional candidates omitted.".into());
            }
            result.candidates = candidates;
        }
        Ok(result)
    }
    pub(crate) async fn all_tokens(
        &self,
        explicit: Option<&Vec<String>>,
        block: Option<u64>,
    ) -> Result<Vec<String>> {
        if let Some(explicit) = explicit {
            let mut seen = BTreeSet::new();
            let mut result = Vec::new();
            for t in explicit {
                let t = lower_address(address(t)?);
                require(
                    seen.insert(t.clone()),
                    "DUPLICATE_TOKEN",
                    "Duplicate reserve identity.",
                )?;
                result.push(t);
            }
            return Ok(result);
        }
        let tokens = self
            .runtime
            .call(
                address(&self.aave()?.pool)?,
                &Pool::getReservesListCall {},
                block,
            )
            .await?;
        require(
            !tokens.is_empty() && tokens.len() <= 128,
            "RESERVE_LIST_INVALID",
            "Aave reserve list exceeds the reviewed read bound.",
        )?;
        let mut seen = BTreeSet::new();
        tokens
            .into_iter()
            .map(|t| {
                require(
                    t != Address::ZERO && seen.insert(t),
                    "RESERVE_LIST_INVALID",
                    "Reserve list contains zero or duplicate identities.",
                )?;
                Ok(lower_address(t))
            })
            .collect()
    }
    pub async fn quote_usd(&self, reference: &str, units: Quantity) -> Result<(Quantity, String)> {
        let (value, source, _) = self.quote_usd_observed(reference, units).await?;
        Ok((value, source))
    }
    pub async fn quote_usd_observed(
        &self,
        reference: &str,
        units: Quantity,
    ) -> Result<(Quantity, String, String)> {
        self.runtime.assert_chain().await?;
        let token = self.token(reference, None).await?;
        if let Some(provider) = &self.wallet_valuation {
            let (value, source, observed_at) = provider
                .quote_observation(&token.asset(self), units.clone())
                .await?;
            require(
                !source.trim().is_empty() && (units.is_zero() || !value.is_zero()),
                "VALUATION_UNAVAILABLE",
                "Wallet valuation requires an explicit source and positive priced value.",
            )?;
            let observed = chrono::DateTime::parse_from_rfc3339(&observed_at).map_err(|_| {
                error(
                    "VALUATION_UNAVAILABLE",
                    "valuation",
                    "Price observation timestamp is malformed.",
                )
            })?;
            let current = self.clock.now();
            require(
                observed <= current + chrono::Duration::seconds(5)
                    && current.signed_duration_since(observed) <= chrono::Duration::seconds(120),
                "VALUATION_UNAVAILABLE",
                "Price observation is stale or future-dated.",
            )?;
            return Ok((value, source, observed_at));
        }
        require(
            self.config.aave.is_some(),
            "VALUATION_UNAVAILABLE",
            "No trusted wallet valuation provider is configured on this network.",
        )?;
        let (value, source) = self.quote_protocol_usd(reference, units).await?;
        Ok((value, source, self.clock.now().to_rfc3339()))
    }
    // Aave collateral/debt risk uses the protocol's own oracle. The same helper
    // preserves isolated legacy policy fixtures when no provider is injected.
    // Broker policy valuation always uses its injected provider, with no fallback.
    pub(crate) async fn quote_protocol_usd(
        &self,
        reference: &str,
        units: Quantity,
    ) -> Result<(Quantity, String)> {
        let token = self.token(reference, None).await?;
        let oracle = address(&self.aave()?.oracle)?;
        self.runtime.code(oracle).await?;
        require(
            self.runtime
                .call(oracle, &Oracle::BASE_CURRENCYCall {}, None)
                .await?
                == Address::ZERO
                && self
                    .runtime
                    .call(oracle, &Oracle::BASE_CURRENCY_UNITCall {}, None)
                    .await?
                    == U256::from(100_000_000),
            "ORACLE_UNIT_UNSUPPORTED",
            "USD valuation requires the reviewed Aave USD base currency and unit.",
        )?;
        let reference = if token.reference == "native" {
            address(&self.resolved_weth(None).await?)?
        } else {
            address(&token.reference)?
        };
        let price = self
            .runtime
            .call(
                oracle,
                &Oracle::getAssetPriceCall { asset: reference },
                None,
            )
            .await?;
        require(
            price > U256::ZERO,
            "ORACLE_PRICE_INVALID",
            "Oracle has no positive asset price.",
        )?;
        let source = self
            .runtime
            .call(
                oracle,
                &Oracle::getSourceOfAssetCall { asset: reference },
                None,
            )
            .await?;
        self.runtime.code(source).await?;
        let (feed, round) = match self
            .runtime
            .call(source, &PriceFeed::latestRoundDataCall {}, None)
            .await
        {
            Ok(round) => (source, round),
            Err(_) => {
                let feed = self
                    .runtime
                    .call(source, &PriceFeed::ASSET_TO_USD_AGGREGATORCall {}, None)
                    .await?;
                self.runtime.code(feed).await?;
                (
                    feed,
                    self.runtime
                        .call(feed, &PriceFeed::latestRoundDataCall {}, None)
                        .await?,
                )
            }
        };
        let current = U256::from(self.oracle_clock.unix_seconds().await?);
        let heartbeat = if reference == address(&self.resolved_weth(None).await?)? {
            7200
        } else {
            86400
        };
        require(
            round.answer > fin_evm::alloy::primitives::I256::ZERO
                && round.updatedAt > U256::ZERO
                && round.updatedAt <= current
                && current - round.updatedAt <= U256::from(heartbeat)
                && round.answeredInRound >= round.roundId,
            "ORACLE_STALE",
            "Underlying USD feed is stale, invalid or incomplete.",
        )?;
        let usd = units.mul_div_ceil(
            &quantity(mul(price, U256::from(1_000_000))?),
            &quantity(mul(pow10(token.decimals)?, U256::from(100_000_000))?),
        )?;
        Ok((
            usd,
            format!(
                "aave-usd:{}:feed:{}:updated:{}",
                self.aave()?.oracle,
                lower_address(feed),
                round.updatedAt
            ),
        ))
    }
    pub(crate) async fn quote_protocol(
        &self,
        request: ProtocolQuoteRequest<'_>,
    ) -> Result<SwapQuote> {
        let ProtocolQuoteRequest {
            dex,
            input,
            output,
            amount: qty,
            fee_tier: tier,
            tick_spacing,
            protocol,
            v4_key,
            block,
        } = request;
        let block_number = match block {
            Some(number) => number,
            None => rpc_u64(
                &self
                    .runtime
                    .rpc
                    .request("eth_blockNumber", json!([]))
                    .await?,
            )?,
        };
        let block = Some(block_number);
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{block_number:#x}"), false]),
            )
            .await?;
        let block_hash = header["hash"].as_str().map(str::to_owned).ok_or_else(|| {
            error(
                "QUOTE_EVIDENCE_INVALID",
                "preview",
                "Pinned quote block has no hash.",
            )
        })?;
        fin_evm::hash(&block_hash)?;
        let observed_at = self.clock.now().to_rfc3339();
        let protocol = super::uniswap::Protocol::parse(protocol)?;
        require(
            dex == "uniswap" || protocol == super::uniswap::Protocol::V3,
            if dex == "ramses" {
                "RAMSES_VERSION_UNSUPPORTED"
            } else {
                "AMM_VERSION_UNSUPPORTED"
            },
            "This venue supports only its reviewed v3 pool and router interface.",
        )?;
        if dex == "uniswap" && protocol == super::uniswap::Protocol::V4 {
            let mut quote = self
                .quote_v4(
                    input,
                    output,
                    qty,
                    v4_key.ok_or_else(|| {
                        error(
                            "V4_POOL_REQUIRED",
                            "validation",
                            "Uniswap v4 requires an explicit pool key.",
                        )
                    })?,
                    block,
                )
                .await?;
            quote.observed_at = observed_at;
            quote.block_number = block;
            quote.block_hash = Some(block_hash);
            quote.router = Some(configured_quote_router(self, "v4")?);
            quote.permit2 = Some("0x000000000022d473030f116ddee9f6b43ac78ba3".into());
            quote.permission_strategy = Some(if quote.token_in.reference == "native" {
                "Native input is settled directly; no token allowance is needed. ERC-20 output is delivered to the wallet.".into()
            } else {
                "ERC-20 input needs a finite token allowance to canonical Permit2 and a finite Permit2 allowance to this v4 router. Finite existing authority is reused; an EOA may use a typed Permit2 permit for the router allowance, otherwise approve the missing finite layer explicitly.".into()
            });
            quote.permission_verification = Some("Router, Permit2, code and exact allowance state are not attested by this quote; preparation verifies the configured deployments and required authority before signing or submission.".into());
            return Ok(quote);
        }
        require(
            v4_key.is_none(),
            "V4_POOL_UNEXPECTED",
            "V3 venues do not accept a v4 pool key.",
        )?;
        let ti = self.token(input, block).await?;
        let to = self.token(output, block).await?;
        require(
            ti.reference != to.reference,
            "SWAP_ASSET_INVALID",
            "Single-pool swaps require two distinct asset identities.",
        )?;
        let units = exact(qty, ti.decimals)?;
        require(
            units > U256::ZERO,
            "ZERO_AMOUNT",
            "Swap input must be positive.",
        )?;
        let profile = self.v3_profile(dex)?;
        let weth = if ti.reference == "native" || to.reference == "native" {
            let router = address(profile.router)?;
            self.runtime.code_at(router, block).await?;
            let weth = self
                .runtime
                .call(router, &V3RouterInfo::WETH9Call {}, block)
                .await?;
            require(
                weth != Address::ZERO,
                "WETH_DEPLOYMENT_INVALID",
                "Configured AMM router returned a zero WETH9 address.",
            )?;
            self.runtime.code_at(weth, block).await?;
            Some(weth)
        } else {
            None
        };
        let pool_in = if ti.reference == "native" {
            weth.expect("resolved WETH")
        } else {
            address(&ti.reference)?
        };
        let pool_out = if to.reference == "native" {
            weth.expect("resolved WETH")
        } else {
            address(&to.reference)?
        };
        require(
            pool_in != pool_out,
            "SWAP_ASSET_INVALID",
            "The v3 route resolves to the same pool currency.",
        )?;
        self.runtime
            .code_at(address(profile.factory)?, block)
            .await?;
        self.runtime
            .code_at(address(profile.quoter)?, block)
            .await?;
        if profile.factory_lookup == super::uniswap::profile::FactoryLookup::TickSpacingI24 {
            let expected_deployer = profile.pool_deployer.expect("Ramses deployer");
            self.runtime
                .code_at(address(profile.router)?, block)
                .await?;
            self.runtime
                .code_at(address(expected_deployer)?, block)
                .await?;
            let actual_deployer = self
                .runtime
                .call(
                    address(profile.factory)?,
                    &RamsesFactory::ramsesV3PoolDeployerCall {},
                    block,
                )
                .await?;
            require(
                actual_deployer == address(expected_deployer)?,
                "DEPLOYMENT_MISMATCH",
                "Ramses factory pool deployer differs from its reviewed configuration.",
            )?;
            let expected = address(expected_deployer)?;
            let router_deployer = self
                .runtime
                .call(
                    address(profile.router)?,
                    &RamsesRouter::deployerCall {},
                    block,
                )
                .await?;
            let quoter_deployer = self
                .runtime
                .call(
                    address(profile.quoter)?,
                    &RamsesQuoter::deployerCall {},
                    block,
                )
                .await?;
            require(
                router_deployer == expected && quoter_deployer == expected,
                "DEPLOYMENT_MISMATCH",
                "Ramses quoter or router does not match the reviewed pool-deployer profile.",
            )?;
        }
        self.validate_v3_metadata(profile, block).await?;
        let tiers: Vec<u32> = if profile.dex_id == "ramses" {
            let spacing = tick_spacing.ok_or_else(|| {
                error(
                    "RAMSES_TICK_SPACING_REQUIRED",
                    "validation",
                    "Select a reviewed Ramses tick spacing.",
                )
            })?;
            require(
                [1, 5, 10, 50, 100, 200].contains(&spacing),
                "RAMSES_TICK_SPACING_UNSUPPORTED",
                "Ramses tick spacing is outside the reviewed candidate set.",
            )?;
            vec![spacing as u32]
        } else if let Some(t) = tier {
            require(
                profile.fee_tiers.contains(&t),
                "FEE_TIER_UNSUPPORTED",
                "Fee tier is outside this reviewed venue profile.",
            )?;
            vec![t]
        } else {
            profile.fee_tiers.to_vec()
        };
        let mut best = None;
        for fee in tiers {
            let pool = match profile.factory_lookup {
                super::uniswap::profile::FactoryLookup::FeeTierU24 => {
                    self.runtime
                        .call(
                            address(profile.factory)?,
                            &Factory::getPoolCall {
                                tokenA: pool_in,
                                tokenB: pool_out,
                                fee: U24::from(fee),
                            },
                            block,
                        )
                        .await?
                }
                super::uniswap::profile::FactoryLookup::TickSpacingI24 => {
                    self.runtime
                        .call(
                            address(profile.factory)?,
                            &RamsesFactory::getPoolCall {
                                tokenA: pool_in,
                                tokenB: pool_out,
                                tickSpacing: fin_evm::alloy::primitives::aliases::I24::try_from(
                                    i64::from(fee),
                                )
                                .expect("reviewed positive spacing"),
                            },
                            block,
                        )
                        .await?
                }
            };
            if pool == Address::ZERO {
                continue;
            }
            self.runtime.code_at(pool, block).await?;
            let (pool_token0, pool_token1, slot, actual_fee) = if profile.dex_id == "ramses" {
                let token0 = self
                    .runtime
                    .call(pool, &RamsesPool::token0Call {}, block)
                    .await?;
                let token1 = self
                    .runtime
                    .call(pool, &RamsesPool::token1Call {}, block)
                    .await?;
                let slot = self
                    .runtime
                    .call(pool, &RamsesPool::slot0Call {}, block)
                    .await?;
                let dynamic_fee = self
                    .runtime
                    .call(pool, &RamsesPool::feeCall {}, block)
                    .await?;
                let observed_spacing = self
                    .runtime
                    .call(pool, &RamsesPool::tickSpacingCall {}, block)
                    .await?;
                require(
                    observed_spacing
                        == fin_evm::alloy::primitives::aliases::I24::try_from(i64::from(fee))
                            .expect("reviewed positive spacing"),
                    "POOL_IDENTITY_MISMATCH",
                    "Ramses pool tick spacing differs from the factory lookup key.",
                )?;
                (token0, token1, slot.sqrtPriceX96, dynamic_fee.to::<u32>())
            } else {
                let token0 = self
                    .runtime
                    .call(pool, &UniswapPool::token0Call {}, block)
                    .await?;
                let token1 = self
                    .runtime
                    .call(pool, &UniswapPool::token1Call {}, block)
                    .await?;
                let sqrt_price = match profile.pool_abi {
                    V3PoolAbi::UniswapV3 => {
                        self.runtime
                            .call(pool, &UniswapPool::slot0Call {}, block)
                            .await?
                            .sqrtPriceX96
                    }
                    V3PoolAbi::PancakeV3 => {
                        self.runtime
                            .call(pool, &PancakePool::slot0Call {}, block)
                            .await?
                            .sqrtPriceX96
                    }
                };
                if super::uniswap::profile::additional_dex(profile.dex_id).is_some() {
                    let factory = self
                        .runtime
                        .call(pool, &UniswapPool::factoryCall {}, block)
                        .await?;
                    require(
                        factory == address(profile.factory)?,
                        "POOL_IDENTITY_MISMATCH",
                        "Pool belongs to a different factory.",
                    )?;
                    let actual_fee = self
                        .runtime
                        .call(pool, &UniswapPool::feeCall {}, block)
                        .await?
                        .to::<u32>();
                    require(
                        actual_fee == fee,
                        "POOL_IDENTITY_MISMATCH",
                        "Pool fee differs from the selected factory key.",
                    )?;
                }
                (token0, token1, sqrt_price, fee)
            };
            if !((pool_token0 == pool_in && pool_token1 == pool_out)
                || (pool_token0 == pool_out && pool_token1 == pool_in))
            {
                continue;
            }
            if slot.is_zero() || actual_fee >= 1_000_000 {
                continue;
            }
            let quoted = match profile.quoter_abi {
                super::uniswap::profile::QuoterAbi::UniswapV3QuoterV2 => self
                    .runtime
                    .call(
                        address(profile.quoter)?,
                        &Quoter::quoteExactInputSingleCall {
                            params: Quoter::QuoteParams {
                                tokenIn: pool_in,
                                tokenOut: pool_out,
                                amountIn: units,
                                fee: U24::from(fee),
                                sqrtPriceLimitX96: Default::default(),
                            },
                        },
                        block,
                    )
                    .await
                    .map(|q| q.amountOut),
                super::uniswap::profile::QuoterAbi::RamsesV3QuoterV2 => self
                    .runtime
                    .call(
                        address(profile.quoter)?,
                        &RamsesQuoter::quoteExactInputSingleCall {
                            params: RamsesQuoter::QuoteParams {
                                tokenIn: pool_in,
                                tokenOut: pool_out,
                                amountIn: units,
                                tickSpacing: fin_evm::alloy::primitives::aliases::I24::try_from(
                                    i64::from(fee),
                                )
                                .expect("reviewed positive spacing"),
                                sqrtPriceLimitX96: Default::default(),
                            },
                        },
                        block,
                    )
                    .await
                    .map(|q| q.amountOut),
            };
            if let Ok(q) = quoted
                && q > U256::ZERO
                && best
                    .as_ref()
                    .is_none_or(|b: &SwapQuote| u256(&b.output).is_ok_and(|n| q > n))
            {
                best = Some(SwapQuote {
                    token_in: ti.clone(),
                    token_out: to.clone(),
                    input: quantity(units),
                    output: quantity(q),
                    pool: lower_address(pool),
                    dex: profile.dex_id.into(),
                    fee_tier: actual_fee,
                    tick_spacing: (profile.dex_id == "ramses").then_some(fee as i32),
                    swap_fee_ppm: Some(match profile.pool_fee_semantics {
                        super::uniswap::profile::PoolFeeSemantics::FixedFeeTier => fee,
                        super::uniswap::profile::PoolFeeSemantics::DynamicPoolFee => actual_fee,
                    }),
                    protocol: "v3".into(),
                    pool_key: None,
                    pool_token_in: lower_address(pool_in),
                    pool_token_out: lower_address(pool_out),
                    price_impact_bps: spot_impact_bps(
                        units,
                        q,
                        slot,
                        pool_in == pool_token0,
                        actual_fee,
                    ),
                    observed_at: observed_at.clone(),
                    block_number: block,
                    block_hash: Some(block_hash.clone()),
                    router: None,
                    permit2: None,
                    permission_strategy: None,
                    permission_verification: None,
                    permission_requirement: None,
                })
            }
        }
        let mut quote = best.ok_or_else(|| {
            error(
                "QUOTE_UNAVAILABLE",
                "preview",
                "No reviewed single-pool fee tier returned a positive quote.",
            )
        })?;
        quote.router = Some(profile.router.to_owned());
        quote.permission_strategy = Some(if quote.token_in.reference == "native" {
            "Native input is wrapped by the reviewed v3 router; no token allowance is needed."
                .into()
        } else if dex == "ramses" {
            "ERC-20 input needs a finite allowance to the configured Ramses router. Finite existing allowance is reused; an EOA may use atomic EIP-2612 when the token supports it, otherwise approve a finite allowance explicitly.".into()
        } else {
            "ERC-20 input needs a finite allowance to the configured v3 router. Finite existing allowance is reused; an EOA may use an atomic EIP-2612 permit when the token supports it, otherwise approve a finite allowance explicitly.".into()
        });
        quote.permission_verification = Some("The configured router, token permit support and exact allowance state are verified during preparation; this quote does not attest them.".into());
        Ok(quote)
    }
    async fn quote_v4(
        &self,
        input: &str,
        output: &str,
        qty: &str,
        key: &super::V4PoolKey,
        block: Option<u64>,
    ) -> Result<SwapQuote> {
        let ti = self.token(input, block).await?;
        let to = self.token(output, block).await?;
        require(
            ti.reference != to.reference,
            "SWAP_ASSET_INVALID",
            "Swap assets must be distinct.",
        )?;
        let pool = super::uniswap::v4::pool_key(key)?;
        let input_currency = if ti.reference == "native" {
            Address::ZERO
        } else {
            address(&ti.reference)?
        };
        let output_currency = if to.reference == "native" {
            Address::ZERO
        } else {
            address(&to.reference)?
        };
        let zero_for_one = if input_currency == pool.currency0 {
            true
        } else if input_currency == pool.currency1 {
            false
        } else {
            return Err(error(
                "V4_POOL_MISMATCH",
                "validation",
                "Input asset is not a currency in the selected v4 pool.",
            ));
        };
        require(
            output_currency
                == if zero_for_one {
                    pool.currency1
                } else {
                    pool.currency0
                },
            "V4_POOL_MISMATCH",
            "Output asset does not match the selected v4 pool direction.",
        )?;
        let units = exact(qty, ti.decimals)?;
        require(
            units > U256::ZERO && units <= U256::from(u128::MAX),
            "V4_AMOUNT_TOO_LARGE",
            "Uniswap v4 amount must be positive and fit uint128.",
        )?;
        let config = self.uniswap()?.v4.as_ref().ok_or_else(|| {
            error(
                "OPERATION_UNAVAILABLE",
                "validation",
                "Uniswap v4 has no configured deployment on this network.",
            )
        })?;
        let quoter = address(&config.quoter)?;
        let manager = address(&config.pool_manager)?;
        let router = address(&config.router)?;
        let state_view = address(&config.state_view)?;
        self.runtime.code_at(quoter, block).await?;
        self.runtime.code_at(manager, block).await?;
        self.runtime.code_at(router, block).await?;
        self.runtime.code_at(state_view, block).await?;
        let pool_id = super::uniswap::v4::pool_id(key)?;
        let pool_hash = fin_evm::hash(&pool_id)?;
        let slot = self
            .runtime
            .call(
                state_view,
                &V4StateView::getSlot0Call { poolId: pool_hash },
                block,
            )
            .await?;
        require(
            !slot.sqrtPriceX96.is_zero() && slot.lpFee == U24::from(key.fee),
            "V4_POOL_UNAVAILABLE",
            "StateView does not confirm the selected v4 pool key and fixed fee.",
        )?;
        let swap_fee_ppm = v4_swap_fee_ppm(
            slot.protocolFee.to::<u32>(),
            slot.lpFee.to::<u32>(),
            zero_for_one,
        )?;
        let quoted = self
            .runtime
            .call(
                quoter,
                &V4Quoter::quoteExactInputSingleCall {
                    params: V4Quoter::QuoteExactSingleParams {
                        poolKey: pool,
                        zeroForOne: zero_for_one,
                        exactAmount: units.to::<u128>(),
                        hookData: fin_evm::alloy::primitives::Bytes::new(),
                    },
                },
                block,
            )
            .await?;
        require(
            quoted.amountOut > U256::ZERO,
            "QUOTE_UNAVAILABLE",
            "The selected v4 pool returned no positive output.",
        )?;
        Ok(SwapQuote {
            token_in: ti,
            token_out: to,
            input: quantity(units),
            output: quantity(quoted.amountOut),
            pool: pool_id,
            dex: "uniswap".into(),
            fee_tier: key.fee,
            tick_spacing: None,
            swap_fee_ppm: Some(swap_fee_ppm),
            protocol: "v4".into(),
            pool_key: Some(key.clone()),
            pool_token_in: lower_address(input_currency),
            pool_token_out: lower_address(output_currency),
            price_impact_bps: spot_impact_bps(
                units,
                quoted.amountOut,
                slot.sqrtPriceX96,
                zero_for_one,
                swap_fee_ppm,
            ),
            observed_at: self.clock.now().to_rfc3339(),
            block_number: block,
            block_hash: None,
            router: None,
            permit2: None,
            permission_strategy: None,
            permission_verification: None,
            permission_requirement: None,
        })
    }
    pub(crate) async fn read_operation(&self, operation: &str, input: &Value) -> Result<Value> {
        require(
            request::is_read(operation),
            "READ_OPERATION_REQUIRED",
            "Use prepare and the protected execution path for writes.",
        )?;
        self.require_operation(operation)?;
        let r = request::parse(operation, input)?;
        let w = self.resolve(&r, false)?;
        self.runtime.assert_chain().await?;
        let owner = address(&w.address)?;
        let block = rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let mut summary: Option<String> = None;
        let mut discovery = None;
        let data = match operation {
            "wallet.token" => {
                let token = self
                    .token(r.token.as_deref().expect("schema"), Some(block))
                    .await?;
                let usd_valuation = self.wallet_usd_valuation_metadata(&token).await;
                let eip2612 = if token.reference == "native" {
                    json!({
                        "supported":false,
                        "verified":false,
                        "reason":"Native assets do not use ERC-20 permits.",
                        "chain_id":self.config.chain_id,
                        "token":token.reference,
                        "owner":lower_address(owner),
                        "observed_block":block
                    })
                } else if let Ok(token_address) = address(&token.reference) {
                    match self.eip2612_nonce_at(token_address, owner, block).await {
                        Some((nonce, domain_separator)) => json!({
                            "supported":true,
                            "verified":true,
                            "chain_id":self.config.chain_id,
                            "token":lower_address(token_address),
                            "owner":lower_address(owner),
                            "nonce":quantity(nonce),
                            "domain_separator":format!("{domain_separator:#x}"),
                            "observed_block":block
                        }),
                        None => json!({
                            "supported":false,
                            "verified":false,
                            "reason":"No standard EIP-2612 domain was verified for this token at the pinned block.",
                            "chain_id":self.config.chain_id,
                            "token":lower_address(token_address),
                            "owner":lower_address(owner),
                            "observed_block":block
                        }),
                    }
                } else {
                    json!({
                        "supported":false,
                        "verified":false,
                        "reason":"Token identity is not a valid EVM address.",
                        "chain_id":self.config.chain_id,
                        "token":token.reference,
                        "owner":lower_address(owner),
                        "observed_block":block
                    })
                };
                json!({
                    "reference":token.reference,
                    "decimals":token.decimals,
                    "symbol":token.symbol,
                    "eip2612":eip2612,
                    "usd_valuation":usd_valuation
                })
            }
            "wallet.allowance" => {
                let token = self
                    .token(r.token.as_deref().expect("schema"), Some(block))
                    .await?;
                let spender = address(r.spender.as_deref().expect("schema"))?;
                json!({"token":token,"spender":lower_address(spender),"units":quantity(self.allowance(&token,owner,spender,Some(block)).await?)})
            }
            "wallet.permit2.allowance" => self.permit2_allowance(&r).await?,
            "wallet.balances" => {
                let mut scope = self.wallet_tokens(&w.address, r.tokens.as_ref()).await?;
                let ts = scope.candidates.clone();
                let mut balances = vec![ResourceAmount {
                    resource: resource(self.config.chain_id, owner),
                    amount: amount(
                        &self.native_asset(),
                        self.runtime.balance(owner, Some(block)).await?,
                    ),
                }];
                for reference in ts {
                    let observed = async {
                        let token = self.token(&reference, Some(block)).await?;
                        let balance = self.balance_of(&token, owner, Some(block)).await?;
                        Ok::<_, FinError>(ResourceAmount {
                            resource: resource(self.config.chain_id, owner),
                            amount: amount(&token.asset(self), balance),
                        })
                    }
                    .await;
                    match observed {
                        Ok(holding) => balances.push(holding),
                        Err(error) if r.tokens.is_some() => return Err(error),
                        Err(error) => {
                            scope.complete = false;
                            scope.warnings.push(format!("Token {reference} could not be verified ({}); its holding is omitted.", error.code));
                        }
                    }
                }
                discovery = Some(scope);
                json!(balances)
            }
            "wallet.receipt" => {
                let h = r.transaction_hash.expect("schema");
                fin_evm::hash(&h)?;
                let receipt = self
                    .runtime
                    .rpc
                    .request("eth_getTransactionReceipt", json!([h]))
                    .await?;
                json!({"receipt":receipt,"completion":"Raw receipt observation alone is not a verified operation."})
            }
            "uniswap.quote" | "ramses.quote" | "sushi.quote" | "pancakeswap.quote" => {
                let mut quote = self
                    .quote_protocol(ProtocolQuoteRequest {
                        dex: super::uniswap::profile::operation_dex(operation)?,
                        input: r.token_in.as_deref().expect("schema"),
                        output: r.token_out.as_deref().expect("schema"),
                        amount: r.amount.as_deref().expect("schema"),
                        fee_tier: r.fee_tier,
                        tick_spacing: r.tick_spacing,
                        protocol: r.protocol.as_deref(),
                        v4_key: r.v4_pool_key.as_ref(),
                        block: Some(block),
                    })
                    .await?;
                let permission = self.swap_permission_requirement(owner, &quote).await?;
                quote.permission_requirement = Some(permission.clone());
                quote.permission_strategy = Some(match permission.clone() {
                    fin_amm::PermissionRequirement::None if quote.token_in.reference == "native" => "No ERC-20 allowance is needed because the swap input is native ETH.".into(),
                    fin_amm::PermissionRequirement::None => "No new token authority is needed; the pinned quote block has sufficient finite allowance.".into(),
                    fin_amm::PermissionRequirement::Permit if quote.protocol == "v4" => "The pinned quote block has sufficient finite token allowance to Permit2; a router-scoped Permit2 permit is available for an EOA wallet.".into(),
                    fin_amm::PermissionRequirement::Permit => "The pinned quote block lacks sufficient finite router allowance; a verified EIP-2612 permit is available for an EOA wallet.".into(),
                    fin_amm::PermissionRequirement::TokenApproval if quote.protocol == "v4" => "The pinned quote block requires a finite token allowance to Permit2 or a finite Permit2 allowance to the router.".into(),
                    fin_amm::PermissionRequirement::TokenApproval => "The pinned quote block requires a finite ERC-20 allowance to the configured router.".into(),
                });
                quote.permission_verification = Some(format!(
                    "Allowance and permit-method classification used pinned block {block}; preparation rechecks deployments, wallet capability and authority immediately before execution."
                ));
                let amm_quote = super::uniswap::as_amm_quote(
                    self,
                    resource(self.config.chain_id, owner),
                    &quote,
                    Quantity::zero(),
                    permission,
                )?;
                summary = Some(fin_amm::format_quote(&amm_quote)?);
                json!(quote)
            }
            "aave.reserves" => {
                let mut reserves = Vec::new();
                for t in self.all_tokens(r.tokens.as_ref(), Some(block)).await? {
                    reserves.push(self.reserve(&t, Some(block)).await?)
                }
                json!(reserves)
            }
            "aave.positions" => {
                let mut positions = Vec::new();
                for t in self.all_tokens(r.tokens.as_ref(), Some(block)).await? {
                    let reserve = self.reserve(&t, Some(block)).await?;
                    positions.push(
                        self.user_reserve(&reserve.token, owner, Some(block))
                            .await?,
                    )
                }
                let account = self
                    .runtime
                    .call(
                        address(&self.aave()?.pool)?,
                        &Pool::getUserAccountDataCall { user: owner },
                        Some(block),
                    )
                    .await?;
                json!({"positions":positions,"account":{"collateral_base":quantity(account.totalCollateralBase),"debt_base":quantity(account.totalDebtBase),"available_borrow_base":quantity(account.availableBorrowsBase),"liquidation_threshold_bps":quantity(account.currentLiquidationThreshold),"health_factor":quantity(account.healthFactor)},"all_reserves":r.tokens.is_none()})
            }
            _ => return Err(FinError::validation("Unsupported read.")),
        };
        let mut result = json!({"operation":operation,"resource":resource(self.config.chain_id,owner),"chain_id":self.config.chain_id,"observed_at":self.clock.now().to_rfc3339(),"block":block,"data":data,"discovery":discovery});
        if let Some(summary) = summary {
            result["summary"] = json!(summary);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod v4_fee_tests {
    use super::{spot_impact_bps, v4_swap_fee_ppm};
    use fin_evm::alloy::primitives::{U160, U256};

    #[test]
    fn directional_protocol_fee_combines_once_with_lp_fee_and_spot_impact() {
        let packed = (900 << 12) | 125;
        let zero_for_one = v4_swap_fee_ppm(packed, 500, true).unwrap();
        let one_for_zero = v4_swap_fee_ppm(packed, 500, false).unwrap();
        assert_eq!(zero_for_one, 625);
        assert_eq!(one_for_zero, 1_400);

        // At a 1:1 spot price, a quote reflecting exactly the 625 ppm
        // combined fee has zero curve impact after that fee is removed.
        // Passing only the 500 ppm LP fee would incorrectly count the
        // 125 ppm protocol fee again as price impact.
        let q96 = U160::from(1u8) << 96;
        assert_eq!(
            spot_impact_bps(
                U256::from(1_000_000u64),
                U256::from(999_375u64),
                q96,
                true,
                zero_for_one,
            ),
            0
        );
    }

    #[test]
    fn invalid_directional_protocol_fee_is_rejected() {
        assert!(v4_swap_fee_ppm((1_001 << 12) | 1, 500, true).is_err());
    }
}
