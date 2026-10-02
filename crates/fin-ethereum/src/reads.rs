use super::*;
use abi::*;
use fin_evm::alloy::primitives::aliases::U24;
use fin_evm::{quantity, rpc_u64, u256};

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
    pub fee_tier: u32,
}

impl EthereumAdapter {
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
        let pool = address(&self.config.aave.pool)?;
        let oracle = address(&self.config.aave.oracle)?;
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
                address(&self.config.aave.pool)?,
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
        self.runtime.assert_chain().await?;
        let token = self.token(reference, None).await?;
        let oracle = address(&self.config.aave.oracle)?;
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
            address(&self.config.weth)?
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
        let heartbeat = if reference == address(&self.config.weth)? {
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
                self.config.aave.oracle,
                lower_address(feed),
                round.updatedAt
            ),
        ))
    }
    pub(crate) async fn quote(
        &self,
        input: &str,
        output: &str,
        qty: &str,
        tier: Option<u32>,
        block: Option<u64>,
    ) -> Result<SwapQuote> {
        let ti = self.token(input, block).await?;
        let to = self.token(output, block).await?;
        require(
            ti.reference != "native" && to.reference != "native" && ti.reference != to.reference,
            "SWAP_ASSET_INVALID",
            "Single-pool swaps require two distinct ERC-20 contracts.",
        )?;
        let units = exact(qty, ti.decimals)?;
        require(
            units > U256::ZERO,
            "ZERO_AMOUNT",
            "Swap input must be positive.",
        )?;
        self.runtime
            .code(address(&self.config.uniswap.factory)?)
            .await?;
        self.runtime
            .code(address(&self.config.uniswap.quoter)?)
            .await?;
        let tiers = if let Some(t) = tier {
            vec![t]
        } else {
            vec![100, 500, 3000, 10000]
        };
        let mut best = None;
        for fee in tiers {
            let pool = self
                .runtime
                .call(
                    address(&self.config.uniswap.factory)?,
                    &Factory::getPoolCall {
                        tokenA: address(&ti.reference)?,
                        tokenB: address(&to.reference)?,
                        fee: U24::from(fee),
                    },
                    block,
                )
                .await?;
            if pool == Address::ZERO {
                continue;
            }
            self.runtime.code(pool).await?;
            let quoted = self
                .runtime
                .call(
                    address(&self.config.uniswap.quoter)?,
                    &Quoter::quoteExactInputSingleCall {
                        params: Quoter::QuoteParams {
                            tokenIn: address(&ti.reference)?,
                            tokenOut: address(&to.reference)?,
                            amountIn: units,
                            fee: U24::from(fee),
                            sqrtPriceLimitX96: Default::default(),
                        },
                    },
                    block,
                )
                .await;
            if let Ok(q) = quoted
                && q.amountOut > U256::ZERO
                && best
                    .as_ref()
                    .is_none_or(|b: &SwapQuote| u256(&b.output).is_ok_and(|n| q.amountOut > n))
            {
                best = Some(SwapQuote {
                    token_in: ti.clone(),
                    token_out: to.clone(),
                    input: quantity(units),
                    output: quantity(q.amountOut),
                    pool: lower_address(pool),
                    fee_tier: fee,
                })
            }
        }
        best.ok_or_else(|| {
            error(
                "QUOTE_UNAVAILABLE",
                "preview",
                "No reviewed single-pool fee tier returned a positive quote.",
            )
        })
    }
    pub(crate) async fn read_operation(&self, operation: &str, input: &Value) -> Result<Value> {
        require(
            request::is_read(operation),
            "READ_OPERATION_REQUIRED",
            "Use prepare and the protected execution path for writes.",
        )?;
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
        let data = match operation {
            "wallet.token" => json!(
                self.token(r.token.as_deref().expect("schema"), Some(block))
                    .await?
            ),
            "wallet.allowance" => {
                let token = self
                    .token(r.token.as_deref().expect("schema"), Some(block))
                    .await?;
                let spender = address(r.spender.as_deref().expect("schema"))?;
                json!({"token":token,"spender":lower_address(spender),"units":quantity(self.allowance(&token,owner,spender,Some(block)).await?)})
            }
            "wallet.balances" => {
                let ts = r
                    .tokens
                    .unwrap_or_else(|| self.config.snapshot_tokens.clone());
                let mut seen = BTreeSet::new();
                let mut balances = vec![ResourceAmount {
                    resource: resource(self.config.chain_id, owner),
                    amount: amount(
                        &self.native_asset(),
                        self.runtime.balance(owner, Some(block)).await?,
                    ),
                }];
                for t in ts {
                    let token = self.token(&t, Some(block)).await?;
                    require(
                        token.reference != "native",
                        "DUPLICATE_TOKEN",
                        "Native ETH is already included in holdings.",
                    )?;
                    require(
                        seen.insert(token.reference.clone()),
                        "DUPLICATE_TOKEN",
                        "Requested holdings duplicate an immutable ERC-20 identity.",
                    )?;
                    balances.push(ResourceAmount {
                        resource: resource(self.config.chain_id, owner),
                        amount: amount(
                            &token.asset(self),
                            self.balance_of(&token, owner, Some(block)).await?,
                        ),
                    })
                }
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
            "uniswap.quote" => json!(
                self.quote(
                    r.token_in.as_deref().expect("schema"),
                    r.token_out.as_deref().expect("schema"),
                    r.amount.as_deref().expect("schema"),
                    r.fee_tier,
                    Some(block)
                )
                .await?
            ),
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
                        address(&self.config.aave.pool)?,
                        &Pool::getUserAccountDataCall { user: owner },
                        Some(block),
                    )
                    .await?;
                json!({"positions":positions,"account":{"collateral_base":quantity(account.totalCollateralBase),"debt_base":quantity(account.totalDebtBase),"available_borrow_base":quantity(account.availableBorrowsBase),"liquidation_threshold_bps":quantity(account.currentLiquidationThreshold),"health_factor":quantity(account.healthFactor)},"all_reserves":r.tokens.is_none()})
            }
            _ => return Err(FinError::validation("Unsupported read.")),
        };
        Ok(
            json!({"operation":operation,"resource":resource(self.config.chain_id,owner),"chain_id":self.config.chain_id,"observed_at":now(),"block":block,"data":data}),
        )
    }
}
