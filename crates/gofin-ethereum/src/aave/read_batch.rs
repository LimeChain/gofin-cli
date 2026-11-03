//! Pinned, staged Aave read facts. All financial classification happens in evaluate.
use super::*;
use crate::{
    abi::{AToken, Bytes32Symbol, DataProvider, Erc20, Pool, PoolReserveData},
    collection::{ReadCall, ReadCollector},
    reads::Token,
};
use gofin_evm::alloy::sol_types::SolCall;
use tokio::task::JoinSet;

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
    user: Option<User>,
    wallet: Option<U256>,
    allowance: Option<U256>,
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
async fn code_checks(
    adapter: &EthereumAdapter,
    block: u64,
    targets: BTreeSet<Address>,
) -> Result<()> {
    let mut jobs = JoinSet::new();
    let mut remaining = targets.into_iter();
    loop {
        while jobs.len() < snapshot::SNAPSHOT_CONCURRENCY {
            let Some(target) = remaining.next() else {
                break;
            };
            let adapter = adapter.clone();
            jobs.spawn(async move { adapter.runtime.code_at(target, Some(block)).await });
        }
        match jobs.join_next().await {
            None => return Ok(()),
            Some(Ok(result)) => result?,
            Some(Err(_)) => {
                return Err(error(
                    "AAVE_READ_TASK_FAILED",
                    "read",
                    "A required code observation did not finish.",
                ));
            }
        }
    }
}
pub(super) async fn collect(
    adapter: &EthereumAdapter,
    capture: &Capture,
    explicit: Option<&Vec<String>>,
    positions: bool,
) -> Result<RowsFacts> {
    let mut collector = ReadCollector::new(adapter, capture.block).await;
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
        adapter.data_provider(Some(block)),
        adapter.all_tokens(explicit, Some(block))
    )?;
    let assets = references
        .iter()
        .map(|v| address(v))
        .collect::<Result<Vec<_>>>()?;
    for asset in &assets {
        require(
            *asset != Address::ZERO,
            "ZERO_ADDRESS",
            "Token identity must be a deployed nonzero address.",
        )?;
    }
    code_checks(adapter, block, assets.iter().copied().collect()).await?;
    let mut calls = Vec::new();
    for &asset in &assets {
        calls.extend([
            ReadCall::new(asset, &Erc20::decimalsCall {}),
            ReadCall::new(asset, &Erc20::symbolCall {}),
            ReadCall::new(
                provider,
                &DataProvider::getReserveConfigurationDataCall { asset },
            ),
            ReadCall::new(
                provider,
                &DataProvider::getReserveTokensAddressesCall { asset },
            ),
            ReadCall::new(provider, &DataProvider::getReserveCapsCall { asset }),
            ReadCall::new(provider, &DataProvider::getPausedCall { asset }),
            ReadCall::new(pool, &Pool::getConfigurationCall { asset }),
            ReadCall::new(pool, &Pool::getReserveNormalizedIncomeCall { asset }),
            ReadCall::new(pool, &Pool::getReserveNormalizedVariableDebtCall { asset }),
            ReadCall::new(pool, &Pool::getReserveDataCall { asset }),
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
    let mut receipt_code = BTreeSet::new();
    for asset in assets {
        let decimals = next::<Erc20::decimalsCall>(&mut values)?;
        let symbol = symbol(values.next())?;
        let config = next::<DataProvider::getReserveConfigurationDataCall>(&mut values)?;
        let receipts = next::<DataProvider::getReserveTokensAddressesCall>(&mut values)?;
        require(
            receipts.aTokenAddress != Address::ZERO
                && receipts.variableDebtTokenAddress != Address::ZERO,
            "RESERVE_UNAVAILABLE",
            "Aave reserve has no supplied/debt representation.",
        )?;
        receipt_code.insert(receipts.aTokenAddress);
        receipt_code.insert(receipts.variableDebtTokenAddress);
        if receipts.stableDebtTokenAddress != Address::ZERO {
            receipt_code.insert(receipts.stableDebtTokenAddress);
        }
        let caps = next::<DataProvider::getReserveCapsCall>(&mut values)?;
        let paused = next::<DataProvider::getPausedCall>(&mut values)?;
        let configuration = next::<Pool::getConfigurationCall>(&mut values)?;
        let income = next::<Pool::getReserveNormalizedIncomeCall>(&mut values)?;
        let debt_index = next::<Pool::getReserveNormalizedVariableDebtCall>(&mut values)?;
        let deployed = next::<Pool::getReserveDataCall>(&mut values)?;
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
            decimals,
            symbol,
            config,
            receipts,
            caps,
            paused,
            configuration,
            income,
            debt_index,
            deployed,
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
    code_checks(adapter, block, receipt_code).await?;
    let mut calls = Vec::new();
    for row in &first {
        let r = &row.receipts;
        calls.extend([
            ReadCall::new(r.aTokenAddress, &Erc20::totalSupplyCall {}),
            ReadCall::new(r.variableDebtTokenAddress, &Erc20::totalSupplyCall {}),
        ]);
        if r.stableDebtTokenAddress != Address::ZERO {
            calls.push(ReadCall::new(
                r.stableDebtTokenAddress,
                &Erc20::totalSupplyCall {},
            ));
        }
        calls.extend([
            ReadCall::new(
                row.asset,
                &Erc20::balanceOfCall {
                    owner: r.aTokenAddress,
                },
            ),
            ReadCall::new(r.aTokenAddress, &AToken::ATOKEN_REVISIONCall {}),
            ReadCall::new(
                r.variableDebtTokenAddress,
                &AToken::DEBT_TOKEN_REVISIONCall {},
            ),
        ]);
        if positions {
            calls.extend([
                ReadCall::new(
                    r.aTokenAddress,
                    &AToken::scaledBalanceOfCall {
                        user: capture.owner,
                    },
                ),
                ReadCall::new(
                    r.aTokenAddress,
                    &AToken::getPreviousIndexCall {
                        user: capture.owner,
                    },
                ),
                ReadCall::new(
                    r.variableDebtTokenAddress,
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
        let supplied_total = next::<Erc20::totalSupplyCall>(&mut values)?;
        let variable_total = next::<Erc20::totalSupplyCall>(&mut values)?;
        let stable_total = if f.receipts.stableDebtTokenAddress == Address::ZERO {
            U256::ZERO
        } else {
            next::<Erc20::totalSupplyCall>(&mut values)?
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
            decimals: f.decimals,
            symbol: f.symbol,
            config: f.config,
            receipts: f.receipts,
            caps: f.caps,
            paused: f.paused,
            configuration: f.configuration,
            income: f.income,
            debt_index: f.debt_index,
            deployed: f.deployed,
            supplied_total,
            variable_total,
            stable_total,
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
