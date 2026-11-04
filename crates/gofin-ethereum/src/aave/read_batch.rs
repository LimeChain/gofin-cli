//! Pinned, staged Aave read facts. All financial classification happens in evaluate.
use super::*;
use crate::{
    abi::{AToken, Bytes32Symbol, DataProvider, Erc20, Pool, PoolReserveData},
    collection::{ReadCall, ReadCollector},
    reads::Token,
};
use gofin_evm::alloy::sol_types::SolCall;

type Config = DataProvider::getReserveConfigurationDataReturn;
type Receipts = DataProvider::getReserveTokensAddressesReturn;
type Caps = DataProvider::getReserveCapsReturn;
type User = DataProvider::getUserReserveDataReturn;
type Account = Pool::getUserAccountDataReturn;

#[derive(Clone)]
pub(crate) struct RowsFacts {
    rows: Vec<RowFacts>,
    pub(super) account: Option<Account>,
    pub(super) mode: Option<U256>,
    horizon: bool,
}
impl RowsFacts {
    pub(crate) fn token(&self) -> Result<Token> {
        let row = self
            .rows
            .first()
            .filter(|_| self.rows.len() == 1)
            .ok_or_else(|| {
                error(
                    "AAVE_READ_INCOMPLETE",
                    "read",
                    "Expected one reserve observation.",
                )
            })?;
        Ok(Token {
            reference: lower_address(row.asset),
            decimals: row.decimals,
            symbol: row.symbol.clone(),
        })
    }
    pub(crate) fn receipt_addresses(&self) -> Result<(Address, Address, Address)> {
        let row = self
            .rows
            .first()
            .filter(|_| self.rows.len() == 1)
            .ok_or_else(|| {
                error(
                    "AAVE_READ_INCOMPLETE",
                    "read",
                    "Expected one reserve observation.",
                )
            })?;
        Ok((
            row.receipts.aTokenAddress,
            row.receipts.stableDebtTokenAddress,
            row.receipts.variableDebtTokenAddress,
        ))
    }
}
#[derive(Clone)]
struct RowFacts {
    asset: Address,
    decimals: u8,
    symbol: String,
    config: Config,
    receipts: Receipts,
    caps: Caps,
    paused: bool,
    configuration: U256,
    income: U256,
    debt_index: U256,
    deployed: PoolReserveData,
    supplied_total: U256,
    variable_total: U256,
    stable_total: U256,
    liquidity: U256,
    supply_revision: U256,
    debt_revision: U256,
    position: Option<PositionFacts>,
}
#[derive(Clone)]
struct PositionFacts {
    user: User,
    scaled: U256,
    supply_previous: U256,
    debt_previous: U256,
    wallet: U256,
    allowance: U256,
}
struct First {
    asset: Address,
    metadata: Option<Token>,
    config: Config,
    receipts: Receipts,
    caps: Caps,
    paused: bool,
    configuration: U256,
    income: U256,
    debt_index: U256,
    deployed: PoolReserveData,
    aggregate: DataProvider::getReserveDataReturn,
    user: Option<User>,
    wallet: Option<U256>,
    allowance: Option<U256>,
}
// Canonical Pool configuration, reviewed against Aave ReserveConfiguration.
// The data-provider's separate configuration/caps/paused views decode this same word.
fn configuration(word: U256) -> (Config, Caps, bool) {
    let field =
        |shift: usize, width: usize| (word >> shift) & ((U256::from(1) << width) - U256::from(1));
    let flag = |bit| !field(bit, 1).is_zero();
    let threshold = field(16, 16);
    (
        Config {
            decimals: field(48, 8),
            ltv: field(0, 16),
            liquidationThreshold: threshold,
            liquidationBonus: field(32, 16),
            reserveFactor: field(64, 16),
            usageAsCollateralEnabled: !threshold.is_zero(),
            borrowingEnabled: flag(58),
            stableBorrowRateEnabled: flag(59),
            isActive: flag(56),
            isFrozen: flag(57),
        },
        Caps {
            borrowCap: field(80, 36),
            supplyCap: field(116, 36),
        },
        flag(60),
    )
}
pub(super) fn next<C: SolCall>(
    values: &mut impl Iterator<Item = Result<Vec<u8>>>,
) -> Result<C::Return> {
    let value = values.next().ok_or_else(|| {
        error(
            "AAVE_READ_INCOMPLETE",
            "read",
            "A required reserve observation is missing.",
        )
    })??;
    C::abi_decode_returns(&value).map_err(|_| {
        error(
            "CONTRACT_DATA_INVALID",
            "read",
            "Contract return does not match the reviewed ABI.",
        )
    })
}
fn symbol(value: Option<Result<Vec<u8>>>) -> Result<String> {
    let bytes = value
        .ok_or_else(|| error("AAVE_READ_INCOMPLETE", "read", "Token symbol is missing."))??;
    Erc20::symbolCall::abi_decode_returns(&bytes)
        .or_else(|_| {
            Bytes32Symbol::symbolCall::abi_decode_returns(&bytes).and_then(|v| {
                String::from_utf8(v.as_slice().to_vec())
                    .map(|v| v.trim_end_matches('\0').to_owned())
                    .map_err(|_| gofin_evm::alloy::sol_types::Error::custom("invalid UTF-8"))
            })
        })
        .map_err(|_| {
            error(
                "TOKEN_METADATA_INVALID",
                "read",
                "Token symbol encoding is invalid.",
            )
        })
}
pub(super) async fn collect(
    adapter: &EthereumAdapter,
    capture: &Capture,
    explicit: Option<&Vec<String>>,
    positions: bool,
) -> Result<RowsFacts> {
    let mut collector = ReadCollector::for_aave(adapter, capture.block);
    collect_with_collector(adapter, capture, explicit, positions, &mut collector).await
}
pub(super) async fn collect_with_collector(
    adapter: &EthereumAdapter,
    capture: &Capture,
    explicit: Option<&Vec<String>>,
    positions: bool,
    collector: &mut ReadCollector,
) -> Result<RowsFacts> {
    let block = capture.block;
    require(
        collector.block() == block,
        "AAVE_READ_CHANGED",
        "Reserve collector block differs from the requested block.",
    )?;
    let pool = address(&adapter.aave()?.pool)?;
    let (provider, references) = tokio::try_join!(
        adapter.data_provider_with_collector(block, collector),
        adapter.all_tokens(explicit, Some(block))
    )?;
    let assets = references
        .iter()
        .map(|v| address(v))
        .collect::<Result<Vec<_>>>()?;
    let hints = references
        .iter()
        .map(|reference| {
            if capture.historical {
                None
            } else {
                adapter.aave_metadata_hint(reference)
            }
        })
        .collect::<Vec<_>>();
    for asset in &assets {
        require(
            *asset != Address::ZERO,
            "ZERO_ADDRESS",
            "Token identity must be a deployed nonzero address.",
        )?;
    }
    // Scoped onchain aggregators: Pool owns configuration/identities and the
    // protocol data provider owns current totals. Neither scans other reserves.
    let mut calls = Vec::new();
    for &asset in &assets {
        calls.extend([
            ReadCall::new(pool, &Pool::getReserveDataCall { asset }),
            ReadCall::new(provider, &DataProvider::getReserveDataCall { asset }),
            ReadCall::new(pool, &Pool::getReserveNormalizedIncomeCall { asset }),
            ReadCall::new(pool, &Pool::getReserveNormalizedVariableDebtCall { asset }),
        ]);
        if positions {
            calls.extend([
                ReadCall::new(
                    provider,
                    &DataProvider::getUserReserveDataCall {
                        asset,
                        user: capture.owner,
                    },
                ),
                ReadCall::new(
                    asset,
                    &Erc20::balanceOfCall {
                        owner: capture.owner,
                    },
                ),
                ReadCall::new(
                    asset,
                    &Erc20::allowanceCall {
                        owner: capture.owner,
                        spender: pool,
                    },
                ),
            ]);
        }
    }
    if positions {
        calls.extend([
            ReadCall::new(
                pool,
                &Pool::getUserAccountDataCall {
                    user: capture.owner,
                },
            ),
            ReadCall::new(
                pool,
                &Pool::getUserEModeCall {
                    user: capture.owner,
                },
            ),
        ]);
    }
    let mut values = collector.collect(calls).await.into_iter();
    let mut first = Vec::new();
    for (asset, hint) in assets.into_iter().zip(hints) {
        let deployed = next::<Pool::getReserveDataCall>(&mut values)?;
        let raw = values.next().ok_or_else(|| {
            error(
                "AAVE_READ_INCOMPLETE",
                "read",
                "Reserve totals are missing.",
            )
        })??;
        require(
            raw.len() == 12 * 32,
            "CONTRACT_DATA_INVALID",
            "Protocol reserve aggregate has an incompatible ABI.",
        )?;
        let aggregate =
            DataProvider::getReserveDataCall::abi_decode_returns(&raw).map_err(|_| {
                error(
                    "CONTRACT_DATA_INVALID",
                    "read",
                    "Protocol reserve aggregate does not match its reviewed ABI.",
                )
            })?;
        require(
            aggregate.liquidityIndex == U256::from(deployed.liquidityIndex)
                && aggregate.variableBorrowIndex == U256::from(deployed.variableBorrowIndex)
                && aggregate.liquidityRate == U256::from(deployed.currentLiquidityRate)
                && aggregate.variableBorrowRate == U256::from(deployed.currentVariableBorrowRate)
                && aggregate.lastUpdateTimestamp == deployed.lastUpdateTimestamp,
            "AAVE_READ_CHANGED",
            "Pool and protocol aggregate disagree at the captured block.",
        )?;
        let (config, caps, paused) = configuration(deployed.configuration);
        let receipts = Receipts {
            aTokenAddress: deployed.aTokenAddress,
            stableDebtTokenAddress: deployed.stableDebtTokenAddress,
            variableDebtTokenAddress: deployed.variableDebtTokenAddress,
        };
        require(
            receipts.aTokenAddress != Address::ZERO
                && receipts.variableDebtTokenAddress != Address::ZERO,
            "RESERVE_UNAVAILABLE",
            "Aave reserve has no supplied/debt representation.",
        )?;
        let reference = lower_address(asset);
        let mut metadata = hint
            .as_ref()
            .and_then(|hint| adapter.aave_metadata_for_decimals(&reference, hint, config.decimals));
        // MCP is a display-metadata fallback, never the risk/precision authority.
        // Historical reads retain their block-specific token labels. Bulk reads
        // fall back in one ERC-20 batch rather than opening a session per row.
        if metadata.is_none()
            && hint.is_none()
            && !capture.historical
            && explicit.is_some_and(|tokens| tokens.len() == 1)
            && let Ok(hint) = adapter
                .aave_mcp
                .reserve_metadata(adapter.config.chain_id, pool, asset)
                .await
        {
            metadata = adapter.aave_metadata_for_decimals(&reference, &hint, config.decimals);
            if let Some(token) = &metadata {
                adapter.aave_store_verified_metadata(token);
            }
        }
        let income = next::<Pool::getReserveNormalizedIncomeCall>(&mut values)?;
        let debt_index = next::<Pool::getReserveNormalizedVariableDebtCall>(&mut values)?;
        let (user, wallet, allowance) = if positions {
            (
                Some(next::<DataProvider::getUserReserveDataCall>(&mut values)?),
                Some(next::<Erc20::balanceOfCall>(&mut values)?),
                Some(next::<Erc20::allowanceCall>(&mut values)?),
            )
        } else {
            (None, None, None)
        };
        first.push(First {
            asset,
            metadata,
            config,
            receipts,
            caps,
            paused,
            configuration: deployed.configuration,
            income,
            debt_index,
            deployed,
            aggregate,
            user,
            wallet,
            allowance,
        });
    }
    let (account, mode) = if positions {
        (
            Some(next::<Pool::getUserAccountDataCall>(&mut values)?),
            Some(next::<Pool::getUserEModeCall>(&mut values)?),
        )
    } else {
        (None, None)
    };
    let mut calls = Vec::new();
    for row in &first {
        if row.metadata.is_none() {
            calls.extend([
                ReadCall::new(row.asset, &Erc20::decimalsCall {}),
                ReadCall::new(row.asset, &Erc20::symbolCall {}),
            ]);
        }
        calls.extend([
            ReadCall::new(
                row.asset,
                &Erc20::balanceOfCall {
                    owner: row.receipts.aTokenAddress,
                },
            ),
            ReadCall::new(row.receipts.aTokenAddress, &AToken::ATOKEN_REVISIONCall {}),
            ReadCall::new(
                row.receipts.variableDebtTokenAddress,
                &AToken::DEBT_TOKEN_REVISIONCall {},
            ),
        ]);
        if positions {
            calls.extend([
                ReadCall::new(
                    row.receipts.aTokenAddress,
                    &AToken::scaledBalanceOfCall {
                        user: capture.owner,
                    },
                ),
                ReadCall::new(
                    row.receipts.aTokenAddress,
                    &AToken::getPreviousIndexCall {
                        user: capture.owner,
                    },
                ),
                ReadCall::new(
                    row.receipts.variableDebtTokenAddress,
                    &AToken::getPreviousIndexCall {
                        user: capture.owner,
                    },
                ),
            ]);
        }
    }
    let mut values = collector.collect(calls).await.into_iter();
    let mut rows = Vec::new();
    for f in first {
        let metadata = if let Some(metadata) = f.metadata {
            metadata
        } else {
            let decimals = next::<Erc20::decimalsCall>(&mut values)?;
            let symbol = symbol(values.next())?;
            require(
                U256::from(decimals) == f.config.decimals,
                "RESERVE_PRECISION_MISMATCH",
                "Aave reserve precision disagrees with token identity.",
            )?;
            let token = Token {
                reference: lower_address(f.asset),
                decimals,
                symbol,
            };
            if !capture.historical {
                adapter.aave_store_verified_metadata(&token);
            }
            token
        };
        let liquidity = next::<Erc20::balanceOfCall>(&mut values)?;
        let supply_revision = next::<AToken::ATOKEN_REVISIONCall>(&mut values)?;
        let debt_revision = next::<AToken::DEBT_TOKEN_REVISIONCall>(&mut values)?;
        let position = if positions {
            Some(PositionFacts {
                user: f.user.expect("user"),
                scaled: next::<AToken::scaledBalanceOfCall>(&mut values)?,
                supply_previous: next::<AToken::getPreviousIndexCall>(&mut values)?,
                debt_previous: next::<AToken::getPreviousIndexCall>(&mut values)?,
                wallet: f.wallet.expect("wallet"),
                allowance: f.allowance.expect("allowance"),
            })
        } else {
            None
        };
        rows.push(RowFacts {
            asset: f.asset,
            decimals: metadata.decimals,
            symbol: metadata.symbol,
            config: f.config,
            receipts: f.receipts,
            caps: f.caps,
            paused: f.paused,
            configuration: f.configuration,
            income: f.income,
            debt_index: f.debt_index,
            deployed: f.deployed,
            supplied_total: f.aggregate.totalAToken,
            variable_total: f.aggregate.totalVariableDebt,
            stable_total: f.aggregate.totalStableDebt,
            liquidity,
            supply_revision,
            debt_revision,
            position,
        });
    }
    Ok(RowsFacts {
        rows,
        account,
        mode,
        horizon: adapter.horizon_math(),
    })
}
pub(super) fn evaluate(facts: &RowsFacts) -> Result<(Vec<Reserve>, Vec<PositionRow>)> {
    let mut reserves = Vec::new();
    let mut positions = Vec::new();
    for row in &facts.rows {
        pow10(row.decimals)?;
        require(
            !row.symbol.is_empty()
                && row.symbol.len() <= 128
                && !row.symbol.chars().any(char::is_control),
            "TOKEN_METADATA_INVALID",
            "Token symbol is not a valid bounded display label.",
        )?;
        require(
            row.config.decimals == U256::from(row.decimals),
            "RESERVE_PRECISION_MISMATCH",
            "Aave reserve precision disagrees with token identity.",
        )?;
        let token = Token {
            reference: lower_address(row.asset),
            decimals: row.decimals,
            symbol: row.symbol.clone(),
        };
        let reserve = Reserve {
            liquidity_rate_ray: quantity(U256::from(row.deployed.currentLiquidityRate)),
            variable_borrow_rate_ray: quantity(U256::from(row.deployed.currentVariableBorrowRate)),
            last_update_timestamp: row.deployed.lastUpdateTimestamp.to::<u64>(),
            access_restrictions: if facts.horizon {
                vec!["Horizon collateral eligibility and issuer transfer permissions are reserve-specific. Writes use the wallet itself as beneficiary and must pass exact simulation; no issuer permission is granted by GoFin.".into()]
            } else {
                vec![]
            },
            borrowable_in_isolation: !((row.configuration >> 61usize) & U256::from(1)).is_zero(),
            siloed_borrowing: !((row.configuration >> 62usize) & U256::from(1)).is_zero(),
            isolation_debt_ceiling: quantity(
                (row.configuration >> 212usize) & ((U256::from(1) << 40usize) - U256::from(1)),
            ),
            isolation_debt_ceiling_decimals: 2,
            a_token_revision: quantity(row.supply_revision),
            variable_debt_revision: quantity(row.debt_revision),
            configuration: quantity(row.configuration),
            normalized_income: quantity(row.income),
            normalized_variable_debt: quantity(row.debt_index),
            token: token.clone(),
            a_token: lower_address(row.receipts.aTokenAddress),
            stable_debt_token: lower_address(row.receipts.stableDebtTokenAddress),
            variable_debt_token: lower_address(row.receipts.variableDebtTokenAddress),
            active: row.config.isActive,
            frozen: row.config.isFrozen,
            paused: row.paused,
            borrowing: row.config.borrowingEnabled,
            collateral_eligible: row.config.usageAsCollateralEnabled,
            ltv: quantity(row.config.ltv),
            liquidation_threshold: quantity(row.config.liquidationThreshold),
            borrow_cap: quantity(row.caps.borrowCap),
            supply_cap: quantity(row.caps.supplyCap),
            total_supply: quantity(row.supplied_total),
            total_debt: quantity(add(row.variable_total, row.stable_total)?),
            liquidity: quantity(row.liquidity),
        };
        if let Some(p) = &row.position {
            let user = UserReserve {
                token,
                supplied: quantity(p.user.currentATokenBalance),
                stable_debt: quantity(p.user.currentStableDebt),
                variable_debt: quantity(p.user.currentVariableDebt),
                scaled_variable_debt: quantity(p.user.scaledVariableDebt),
                collateral_enabled: p.user.usageAsCollateralEnabled,
            };
            positions.push(PositionRow {user,scaled_supply:quantity(p.scaled),supply_previous_index:quantity(p.supply_previous),debt_previous_index:quantity(p.debt_previous),reserve:reserve.clone(),wallet_balance:quantity(p.wallet),pool_allowance:quantity(p.allowance),historical_principal:None,historical_interest:None,interest_attribution:"Current balances include interest. Original external principal requires historical proof; it is not inferred from current balances.".into()});
        }
        reserves.push(reserve);
    }
    Ok((reserves, positions))
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;
    #[test]
    fn packed_configuration_preserves_precision_flags_caps_and_risk_fields() {
        let word = U256::from(7500)
            | (U256::from(8100) << 16)
            | (U256::from(10600) << 32)
            | (U256::from(18) << 48)
            | (U256::from(1) << 56)
            | (U256::from(1) << 57)
            | (U256::from(1) << 58)
            | (U256::from(1) << 60)
            | (U256::from(1200) << 64)
            | (U256::from(123456) << 80)
            | (U256::from(654321) << 116)
            | (U256::from(1) << 252);
        let (config, caps, paused) = configuration(word);
        assert_eq!(config.decimals, U256::from(18));
        assert_eq!(config.ltv, U256::from(7500));
        assert_eq!(config.liquidationThreshold, U256::from(8100));
        assert_eq!(config.liquidationBonus, U256::from(10600));
        assert_eq!(config.reserveFactor, U256::from(1200));
        assert!(config.isActive && config.isFrozen && config.borrowingEnabled && paused);
        assert!(!config.stableBorrowRateEnabled);
        assert_eq!(caps.borrowCap, U256::from(123456));
        assert_eq!(caps.supplyCap, U256::from(654321));
    }
}
