//! Point-in-time Aave risk and mode checks, using protocol units and exact math.
use super::super::*;
use crate::abi::*;
use crate::collection::{ReadCall, ReadCollector};
use gofin_evm::alloy::sol_types::SolCall;
use gofin_evm::{EvmRuntime, rpc_u64, u256};
use tokio::task::JoinSet;

fn decode<C: SolCall>(value: Option<Result<Vec<u8>>>) -> Result<C::Return> {
    let bytes = value.ok_or_else(|| {
        error(
            "MULTICALL_INCOMPLETE",
            "read",
            "A risk observation is missing.",
        )
    })??;
    C::abi_decode_returns(&bytes).map_err(|_| {
        error(
            "CONTRACT_DATA_INVALID",
            "read",
            "Risk observation does not match the reviewed ABI.",
        )
    })
}

type RiskUser = (String, U256, DataProvider::getUserReserveDataReturn);

pub(crate) struct BorrowRestrictionEvidence {
    mode: U256,
    config: U256,
    emode: Option<(u16, u128)>,
    users: Vec<(RiskUser, Option<u128>)>,
}

pub(crate) fn evaluate_borrow_restrictions(
    token: &reads::Token,
    qty: U256,
    e: &BorrowRestrictionEvidence,
) -> Result<()> {
    if e.mode > U256::ZERO {
        let (id, bitmap) = e
            .emode
            .ok_or_else(|| GoFinError::validation("Missing eMode observation."))?;
        require(
            id < 128 && (bitmap & (1u128 << id)) != 0,
            "AAVE_EMODE_BORROW_UNAVAILABLE",
            "Debt asset is not borrowable in the current eMode category.",
        )?;
    }
    for ((reference, config, user), isolation_debt) in &e.users {
        if !user.currentVariableDebt.is_zero() || !user.currentStableDebt.is_zero() {
            require(
                reference == &token.reference
                    || ((e.config >> 62usize) & U256::from(1)).is_zero()
                        && ((*config >> 62usize) & U256::from(1)).is_zero(),
                "AAVE_SILOED_BORROW_UNAVAILABLE",
                "Siloed borrowing cannot mix another debt asset.",
            )?;
        }
        let ceiling = (*config >> 212usize) & ((U256::from(1) << 40usize) - U256::from(1));
        if user.usageAsCollateralEnabled
            && !user.currentATokenBalance.is_zero()
            && !ceiling.is_zero()
        {
            require(
                !((e.config >> 61usize) & U256::from(1)).is_zero(),
                "AAVE_ISOLATION_BORROW_UNAVAILABLE",
                "Debt asset is not permitted against isolated collateral.",
            )?;
            let debt = isolation_debt
                .ok_or_else(|| GoFinError::validation("Missing isolation debt observation."))?;
            let added = add(
                mul(qty, U256::from(100))?,
                pow10(token.decimals)? - U256::from(1),
            )? / pow10(token.decimals)?;
            require(
                add(U256::from(debt), added)? <= ceiling,
                "AAVE_ISOLATION_DEBT_CEILING",
                "Borrow exceeds the isolated collateral's remaining debt ceiling.",
            )?;
        }
    }
    Ok(())
}

struct HealthCollateral {
    reference: String,
    units: U256,
    reserve: super::read_lifecycle::WriteReserveFacts,
    scaled: Option<U256>,
    price: U256,
    reserve_id: Option<u16>,
}
pub(crate) struct HealthEvidence {
    horizon: bool,
    collateral_before: U256,
    debt_before: U256,
    debt_price: Option<U256>,
    oracle_units: Option<(Address, U256)>,
    details: Result<HealthDetails>,
}
struct HealthDetails {
    category_threshold: Option<u16>,
    category_bitmap: u128,
    collateral: Vec<HealthCollateral>,
}
fn health_debt(token: &reads::Token, added_debt: U256, e: &HealthEvidence) -> Result<U256> {
    if !added_debt.is_zero() {
        require(
            e.oracle_units == Some((Address::ZERO, pow10(8)?)),
            "AAVE_ORACLE_UNITS_UNSUPPORTED",
            "USD debt limits require the reviewed USD oracle base and eight-decimal units.",
        )?;
    }
    let added = if added_debt.is_zero() {
        U256::ZERO
    } else {
        let price = e
            .debt_price
            .ok_or_else(|| GoFinError::validation("Missing debt price observation."))?;
        super::math::added_debt_base(e.horizon, added_debt, price, token.decimals)?
    };
    add(e.debt_before, added)
}
pub(crate) fn evaluate_health(
    token: &reads::Token,
    removed: Option<U256>,
    apply_removal: bool,
    added_debt: U256,
    e: &HealthEvidence,
) -> Result<(U256, U256, U256)> {
    let debt = health_debt(token, added_debt, e)?;
    if debt.is_zero() {
        return Ok((e.collateral_before, debt, U256::MAX));
    }
    let details = e.details.as_ref().map_err(Clone::clone)?;
    let mut weighted = U256::ZERO;
    let mut collateral = U256::ZERO;
    for entry in &details.collateral {
        let reserve = super::read_lifecycle::evaluate_write_reserve(&entry.reserve)?;
        let mut units = entry.units;
        if apply_removal && entry.reference == token.reference {
            units = if let Some(qty) = removed {
                let index = u256(&reserve.normalized_income)?;
                let burned = super::math::scaled_change(
                    e.horizon,
                    qty,
                    index,
                    &PositionKind::Supply,
                    false,
                )?;
                let scaled = entry
                    .scaled
                    .ok_or_else(|| GoFinError::validation("Missing scaled supply observation."))?;
                let remaining = scaled
                    .checked_sub(burned)
                    .ok_or_else(|| GoFinError::validation("Removal exceeds exact scaled claim."))?;
                super::math::position_backing(e.horizon, remaining, index, &PositionKind::Supply)?
            } else {
                U256::ZERO
            };
        }
        require(
            !entry.price.is_zero(),
            "ORACLE_INVALID",
            "Collateral risk price must be positive.",
        )?;
        let base = mul(units, entry.price)? / pow10(reserve.token.decimals)?;
        let mut threshold = u256(&reserve.liquidation_threshold)?;
        if let (Some(category), Some(id)) = (details.category_threshold, entry.reserve_id)
            && id < 128
            && (details.category_bitmap & (1u128 << id)) != 0
        {
            threshold = U256::from(category);
        }
        if !e.horizon || !threshold.is_zero() {
            collateral = add(collateral, base)?;
            weighted = add(weighted, mul(base, threshold)?)?;
        }
    }
    Ok((
        collateral,
        debt,
        health_factor(e.horizon, collateral, weighted, debt)?,
    ))
}

pub(crate) struct BorrowRiskEvidence {
    restrictions: BorrowRestrictionEvidence,
    health: HealthEvidence,
}
pub(crate) fn evaluate_borrow_risk(
    token: &reads::Token,
    qty: U256,
    e: &BorrowRiskEvidence,
) -> Result<(U256, U256, U256)> {
    evaluate_borrow_restrictions(token, qty, &e.restrictions)?;
    evaluate_health(token, None, false, qty, &e.health)
}

pub(crate) struct RiskCapture<'a> {
    pub block: u64,
    pub collector: &'a mut ReadCollector,
    pub selected_reserve: Option<&'a super::read_lifecycle::WriteReserveFacts>,
    pub account: Option<&'a Pool::getUserAccountDataReturn>,
}

struct RiskObservation<'a> {
    block: u64,
    users: Option<&'a [RiskUser]>,
    collector: &'a mut ReadCollector,
    selected_reserve: Option<(&'a str, &'a super::read_lifecycle::WriteReserveFacts)>,
    account: Option<&'a Pool::getUserAccountDataReturn>,
}

impl EthereumAdapter {
    // One short-lived, read-only capture. All child work shares an eight-request
    // ceiling and exact-request cache; neither cache nor block survives a retry.
    async fn aave_risk_reader(&self) -> Result<(Self, u64, Value)> {
        let block = rpc_u64(
            &self
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(block)), false]),
            )
            .await?;
        gofin_evm::hash(header["hash"].as_str().unwrap_or(""))?;
        require(
            rpc_u64(&header["number"])? == block,
            "AAVE_READ_CHANGED",
            "Risk block identity differs.",
        )?;
        let mut reader = self.clone();
        reader.runtime = Arc::new(EvmRuntime::new(
            Arc::new(snapshot::SnapshotRpc::new(
                self.runtime.rpc.clone(),
                block,
                snapshot::SNAPSHOT_CONCURRENCY,
            )),
            self.config.chain_id,
            None,
        ));
        Ok((reader, block, header))
    }
    async fn aave_risk_block_unchanged(&self, block: u64, before: &Value) -> Result<()> {
        let after = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([gofin_evm::block_tag(Some(block)), false]),
            )
            .await?;
        require(
            rpc_u64(&after["number"])? == block && after["hash"] == before["hash"],
            "AAVE_READ_CHANGED",
            "Risk block identity changed while gathering evidence.",
        )
    }
    async fn aave_risk_users(
        &self,
        owner: Address,
        block: u64,
        configurations: bool,
    ) -> Result<Vec<RiskUser>> {
        let provider = self.data_provider(Some(block)).await?;
        let pool = address(&self.aave()?.pool)?;
        let mut remaining = self.all_tokens(None, Some(block)).await?.into_iter();
        let mut jobs = JoinSet::new();
        let mut users = Vec::new();
        loop {
            while jobs.len() < snapshot::SNAPSHOT_CONCURRENCY {
                let Some(reference) = remaining.next() else {
                    break;
                };
                let reader = self.clone();
                jobs.spawn(async move {
                    let asset = address(&reference)?;
                    let config = if configurations {
                        reader
                            .runtime
                            .call(pool, &Pool::getConfigurationCall { asset }, Some(block))
                            .await?
                    } else {
                        U256::ZERO
                    };
                    let user = reader
                        .runtime
                        .call(
                            provider,
                            &DataProvider::getUserReserveDataCall { asset, user: owner },
                            Some(block),
                        )
                        .await?;
                    Ok::<_, GoFinError>((reference, config, user))
                });
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            match result {
                Ok(Ok(user)) => users.push(user),
                failed => {
                    jobs.abort_all();
                    while jobs.join_next().await.is_some() {}
                    return Err(match failed {
                        Ok(Err(error)) => error,
                        _ => error(
                            "AAVE_READ_TASK_FAILED",
                            "preflight",
                            "Required risk observation did not finish.",
                        ),
                    });
                }
            }
        }
        // Keep deterministic validation order even when RPC completion differs.
        users.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(users)
    }

    pub(crate) async fn aave_mode(&self, owner: Address, block: Option<u64>) -> Result<U256> {
        self.runtime
            .call(
                address(&self.aave()?.pool)?,
                &Pool::getUserEModeCall { user: owner },
                block,
            )
            .await
    }
    #[cfg(test)]
    pub(crate) async fn aave_borrow_restrictions(
        &self,
        owner: Address,
        token: &reads::Token,
        qty: U256,
    ) -> Result<()> {
        let (reader, block, before) = self.aave_risk_reader().await?;
        let result = reader
            .aave_borrow_restrictions_at(owner, token, qty, block, None)
            .await;
        self.aave_risk_block_unchanged(block, &before).await?;
        result
    }
    #[cfg(test)]
    async fn aave_borrow_restrictions_at(
        &self,
        owner: Address,
        token: &reads::Token,
        qty: U256,
        block: u64,
        selected_users: Option<&[RiskUser]>,
    ) -> Result<()> {
        let mut collector = ReadCollector::for_aave(self, block);
        let evidence = self
            .collect_borrow_restrictions(owner, token, block, selected_users, &mut collector)
            .await?;
        evaluate_borrow_restrictions(token, qty, &evidence)
    }
    async fn collect_borrow_restrictions(
        &self,
        owner: Address,
        token: &reads::Token,
        block: u64,
        selected_users: Option<&[RiskUser]>,
        collector: &mut ReadCollector,
    ) -> Result<BorrowRestrictionEvidence> {
        let pool = address(&self.aave()?.pool)?;
        let mut observed = collector
            .collect(vec![
                ReadCall::new(pool, &Pool::getUserEModeCall { user: owner }),
                ReadCall::new(
                    pool,
                    &Pool::getReserveDataCall {
                        asset: address(&token.reference)?,
                    },
                ),
            ])
            .await
            .into_iter();
        let mode = decode::<Pool::getUserEModeCall>(observed.next())?;
        let selected_reserve = decode::<Pool::getReserveDataCall>(observed.next())?;
        let config = selected_reserve.configuration;
        let emode = if mode > U256::ZERO {
            require(
                mode <= U256::from(255),
                "AAVE_MODE_UNSUPPORTED",
                "Unrecognized eMode category.",
            )?;
            let bitmap = collector
                .call(
                    pool,
                    &Pool::getEModeCategoryBorrowableBitmapCall {
                        id: mode.to::<u8>(),
                    },
                )
                .await?;
            Some((selected_reserve.id, bitmap))
        } else {
            None
        };
        let fetched;
        let users = if let Some(users) = selected_users {
            users
        } else {
            fetched = self.aave_risk_users(owner, block, true).await?;
            &fetched
        };
        let isolated_calls = users
            .iter()
            .filter_map(|(reference, config, user)| {
                let ceiling = (*config >> 212usize) & ((U256::from(1) << 40usize) - U256::from(1));
                (user.usageAsCollateralEnabled
                    && !user.currentATokenBalance.is_zero()
                    && !ceiling.is_zero())
                .then(|| {
                    address(reference)
                        .map(|asset| ReadCall::new(pool, &Pool::getReserveDataCall { asset }))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // Preserve each individual error in the pass-local collector; validation
        // below still interprets the same fact at its original point.
        let _ = collector.collect(isolated_calls).await;
        let mut observed = Vec::with_capacity(users.len());
        for entry in users.iter().cloned() {
            let (reference, config, user) = &entry;
            let ceiling = (*config >> 212usize) & ((U256::from(1) << 40usize) - U256::from(1));
            let isolation_debt = if user.usageAsCollateralEnabled
                && !user.currentATokenBalance.is_zero()
                && !ceiling.is_zero()
            {
                Some(
                    collector
                        .call(
                            pool,
                            &Pool::getReserveDataCall {
                                asset: address(reference)?,
                            },
                        )
                        .await?
                        .isolationModeTotalDebt,
                )
            } else {
                None
            };
            observed.push((entry, isolation_debt));
        }
        Ok(BorrowRestrictionEvidence {
            mode,
            config,
            emode,
            users: observed,
        })
    }
    // Borrow reads only bitmap-selected risk exposures. The pool's stable reserve
    // IDs, not its compact list indices, identify each pair of user flags.
    pub(crate) async fn collect_borrow_risk(
        &self,
        owner: Address,
        token: &reads::Token,
        qty: U256,
        capture: RiskCapture<'_>,
    ) -> Result<BorrowRiskEvidence> {
        let RiskCapture {
            block,
            collector,
            selected_reserve,
            account,
        } = capture;
        let users = self
            .collect_bitmap_risk_users(owner, token, block, collector)
            .await?;
        let restrictions = self
            .collect_borrow_restrictions(owner, token, block, Some(&users), collector)
            .await?;
        let health = self
            .collect_health(
                owner,
                token,
                None,
                false,
                qty,
                RiskObservation {
                    block,
                    users: Some(&users),
                    collector,
                    selected_reserve: selected_reserve
                        .map(|reserve| (token.reference.as_str(), reserve)),
                    account,
                },
            )
            .await?;
        Ok(BorrowRiskEvidence {
            restrictions,
            health,
        })
    }
    // Resolve sparse Pool reserve IDs. Only flagged collateral/debt exposures
    // enter health projection; the selected token is checked independently.
    async fn collect_bitmap_risk_users(
        &self,
        owner: Address,
        token: &reads::Token,
        block: u64,
        collector: &mut ReadCollector,
    ) -> Result<Vec<RiskUser>> {
        let pool = address(&self.aave()?.pool)?;
        let provider = self.data_provider_with_collector(block, collector).await?;
        let selected = address(&token.reference)?;
        let mut initial = collector
            .collect(vec![
                ReadCall::new(pool, &Pool::getUserConfigurationCall { user: owner }),
                ReadCall::new(pool, &Pool::getReserveDataCall { asset: selected }),
                ReadCall::new(
                    provider,
                    &DataProvider::getUserReserveDataCall {
                        asset: selected,
                        user: owner,
                    },
                ),
            ])
            .await
            .into_iter();
        let flags = decode::<Pool::getUserConfigurationCall>(initial.next())?;
        let selected_data = decode::<Pool::getReserveDataCall>(initial.next())?;
        let selected_user = decode::<DataProvider::getUserReserveDataCall>(initial.next())?;
        require(
            selected_data.id < 128,
            "AAVE_USER_CONFIGURATION_INVALID",
            "Selected reserve ID is outside the user configuration bitmap.",
        )?;
        let selected_id = selected_data.id;
        let selected_bits = (flags >> (usize::from(selected_id) * 2)) & U256::from(3);
        require(
            selected_user.usageAsCollateralEnabled == !(selected_bits & U256::from(2)).is_zero()
                && (!(selected_bits & U256::from(1)).is_zero()
                    || (selected_user.currentVariableDebt.is_zero()
                        && selected_user.currentStableDebt.is_zero())),
            "AAVE_USER_CONFIGURATION_INVALID",
            "Selected reserve observations disagree with pool user flags.",
        )?;
        let mut ids: Vec<u16> = (0..128u16)
            .filter(|id| !((flags >> (usize::from(*id) * 2)) & U256::from(3)).is_zero())
            .collect();
        if !ids.contains(&selected_id) {
            ids.push(selected_id);
        }
        ids.sort_unstable();
        let mut resolved = collector
            .collect(
                ids.iter()
                    .map(|&id| ReadCall::new(pool, &Pool::getReserveAddressByIdCall { id }))
                    .collect(),
            )
            .await
            .into_iter();
        let mut active = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            let asset = decode::<Pool::getReserveAddressByIdCall>(resolved.next())?;
            let bits = (flags >> (usize::from(id) * 2)) & U256::from(3);
            if id == selected_id {
                require(
                    asset == selected,
                    "AAVE_USER_CONFIGURATION_INVALID",
                    "Selected reserve ID does not resolve to the selected asset.",
                )?;
            }
            if bits.is_zero() {
                continue;
            }
            require(
                asset != Address::ZERO && seen.insert(asset),
                "AAVE_USER_CONFIGURATION_INVALID",
                "Flagged reserve ID has no unique pool asset.",
            )?;
            active.push((id, asset, bits));
        }
        let mut calls = Vec::with_capacity(active.len() * 2);
        for &(_, asset, _) in &active {
            calls.push(ReadCall::new(pool, &Pool::getReserveDataCall { asset }));
            calls.push(ReadCall::new(
                provider,
                &DataProvider::getUserReserveDataCall { asset, user: owner },
            ));
        }
        let mut observed = collector.collect(calls).await.into_iter();
        let mut users = Vec::with_capacity(active.len());
        for (id, asset, bits) in active {
            let data = decode::<Pool::getReserveDataCall>(observed.next())?;
            let config = data.configuration;
            let user = decode::<DataProvider::getUserReserveDataCall>(observed.next())?;
            require(
                data.id == id,
                "AAVE_USER_CONFIGURATION_INVALID",
                "Flagged reserve ID does not match the pool reserve identity.",
            )?;
            require(
                user.usageAsCollateralEnabled == !(bits & U256::from(2)).is_zero()
                    && (!(bits & U256::from(1)).is_zero()
                        || (user.currentVariableDebt.is_zero()
                            && user.currentStableDebt.is_zero())),
                "AAVE_USER_CONFIGURATION_INVALID",
                "Pool user flags disagree with reserve observations.",
            )?;
            users.push((lower_address(asset), config, user));
        }
        users.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(users)
    }
    pub(crate) async fn collect_removal_health_at(
        &self,
        owner: Address,
        token: &reads::Token,
        removed: Option<U256>,
        capture: RiskCapture<'_>,
    ) -> Result<HealthEvidence> {
        let RiskCapture {
            block,
            collector,
            selected_reserve,
            account,
        } = capture;
        // Acquisition planning from an exact zero account-debt fact avoids
        // market discovery; the pure evaluator remains the authority.
        if let Some(account) = account
            && account.totalDebtBase.is_zero()
        {
            return Ok(HealthEvidence {
                horizon: self.horizon_math(),
                collateral_before: account.totalCollateralBase,
                debt_before: account.totalDebtBase,
                debt_price: None,
                oracle_units: None,
                details: Ok(HealthDetails {
                    category_threshold: None,
                    category_bitmap: 0,
                    collateral: Vec::new(),
                }),
            });
        }
        let users = self
            .collect_bitmap_risk_users(owner, token, block, collector)
            .await?;
        self.collect_health(
            owner,
            token,
            removed,
            true,
            U256::ZERO,
            RiskObservation {
                block,
                users: Some(&users),
                collector,
                selected_reserve: selected_reserve
                    .map(|reserve| (token.reference.as_str(), reserve)),
                account,
            },
        )
        .await
    }
    pub(crate) fn evaluate_removal_risk(
        token: &reads::Token,
        removed: Option<U256>,
        minimum: Option<&str>,
        evidence: &HealthEvidence,
    ) -> Result<()> {
        let (_, debt, projected) = evaluate_health(token, removed, true, U256::ZERO, evidence)?;
        let requested = minimum.map(|v| exact(v, 18)).transpose()?;
        if let Some(floor) = requested {
            require(
                floor > pow10(18)?,
                "HEALTH_FACTOR_LIMIT",
                "Minimum health factor must exceed 1.",
            )?;
        }
        if debt.is_zero() {
            return Ok(());
        }
        let minimum = requested.ok_or_else(|| error("AAVE_RISK_BOUNDS_REQUIRED", "preview", "Removing collateral with outstanding debt requires min_health_factor greater than 1."))?;
        require(
            projected >= minimum,
            "HEALTH_FACTOR_LIMIT",
            "Collateral removal violates the approved minimum health factor.",
        )
    }
    pub(crate) async fn aave_health_projection(
        &self,
        owner: Address,
        token: &reads::Token,
        removed: Option<U256>,
        apply_removal: bool,
        added_debt: U256,
    ) -> Result<(U256, U256, U256)> {
        let (reader, block, before) = self.aave_risk_reader().await?;
        let mut collector = ReadCollector::for_aave(&reader, block);
        let result = reader
            .aave_health_projection_at(
                owner,
                token,
                removed,
                apply_removal,
                added_debt,
                RiskObservation {
                    block,
                    users: None,
                    collector: &mut collector,
                    selected_reserve: None,
                    account: None,
                },
            )
            .await;
        self.aave_risk_block_unchanged(block, &before).await?;
        result
    }
    async fn aave_health_projection_at(
        &self,
        owner: Address,
        token: &reads::Token,
        removed: Option<U256>,
        apply_removal: bool,
        added_debt: U256,
        observation: RiskObservation<'_>,
    ) -> Result<(U256, U256, U256)> {
        let evidence = self
            .collect_health(
                owner,
                token,
                removed,
                apply_removal,
                added_debt,
                observation,
            )
            .await?;
        evaluate_health(token, removed, apply_removal, added_debt, &evidence)
    }
    async fn collect_health(
        &self,
        owner: Address,
        token: &reads::Token,
        removed: Option<U256>,
        apply_removal: bool,
        added_debt: U256,
        observation: RiskObservation<'_>,
    ) -> Result<HealthEvidence> {
        let RiskObservation {
            block,
            users: selected_users,
            collector,
            selected_reserve,
            account,
        } = observation;
        let pool = address(&self.aave()?.pool)?;
        let oracle = address(&self.aave()?.oracle)?;
        // These targets are known at the same pinned block. Gather them in one
        // request; the per-call results retain their original error semantics.
        let mut calls = Vec::new();
        if account.is_none() {
            calls.push(ReadCall::new(
                pool,
                &Pool::getUserAccountDataCall { user: owner },
            ));
        }
        if !added_debt.is_zero() {
            calls.push(ReadCall::new(oracle, &Oracle::BASE_CURRENCYCall {}));
            calls.push(ReadCall::new(oracle, &Oracle::BASE_CURRENCY_UNITCall {}));
            calls.push(ReadCall::new(
                oracle,
                &Oracle::getAssetPriceCall {
                    asset: address(&token.reference)?,
                },
            ));
        }
        calls.push(ReadCall::new(pool, &Pool::getUserEModeCall { user: owner }));
        let mut observed = collector.collect(calls).await.into_iter();
        let account = if let Some(account) = account {
            account.clone()
        } else {
            decode::<Pool::getUserAccountDataCall>(observed.next())?
        };
        let (oracle_units, debt_price) = if added_debt.is_zero() {
            (None, None)
        } else {
            (
                Some((
                    decode::<Oracle::BASE_CURRENCYCall>(observed.next())?,
                    decode::<Oracle::BASE_CURRENCY_UNITCall>(observed.next())?,
                )),
                Some(decode::<Oracle::getAssetPriceCall>(observed.next())?),
            )
        };
        // Mode failure was historically part of the optional details result,
        // so keep its interpretation inside that result rather than promoting it.
        let mode_result = observed.next();
        let details: Result<HealthDetails> = async {
            let mut details = HealthDetails {
                category_threshold: None,
                category_bitmap: 0,
                collateral: Vec::new(),
            };
            let mode = decode::<Pool::getUserEModeCall>(mode_result)?;
            require(
                mode <= U256::from(255),
                "AAVE_MODE_UNSUPPORTED",
                "Unrecognized eMode category.",
            )?;
            if !mode.is_zero() {
                let id = mode.to::<u8>();
                let mut observed = collector
                    .collect(vec![
                        ReadCall::new(pool, &Pool::getEModeCategoryCollateralConfigCall { id }),
                        ReadCall::new(pool, &Pool::getEModeCategoryCollateralBitmapCall { id }),
                    ])
                    .await
                    .into_iter();
                details.category_threshold = Some(
                    decode::<Pool::getEModeCategoryCollateralConfigCall>(observed.next())?
                        .liquidationThreshold,
                );
                details.category_bitmap =
                    decode::<Pool::getEModeCategoryCollateralBitmapCall>(observed.next())?;
            }
            let fetched;
            let users = if let Some(users) = selected_users {
                users
            } else {
                fetched = self.aave_risk_users(owner, block, false).await?;
                &fetched
            };
            let price_calls = users
                .iter()
                .filter(|(_, _, user)| {
                    user.usageAsCollateralEnabled && !user.currentATokenBalance.is_zero()
                })
                .map(|(reference, _, _)| {
                    Ok(ReadCall::new(
                        oracle,
                        &Oracle::getAssetPriceCall {
                            asset: address(reference)?,
                        },
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let _ = collector.collect(price_calls).await;
            for (reference, _, user) in users.iter().cloned() {
                if !user.usageAsCollateralEnabled || user.currentATokenBalance.is_zero() {
                    continue;
                }
                let reserve = if let Some((_, facts)) =
                    selected_reserve.filter(|(selected, _)| *selected == reference)
                {
                    facts.clone()
                } else {
                    super::read_lifecycle::collect_write_reserve(
                        self, block, owner, &reference, collector,
                    )
                    .await?
                };
                let (a_token, _, _) = reserve.receipt_addresses()?;
                let needs_scaled =
                    apply_removal && reference == token.reference && removed.is_some();
                let needs_id = details.category_threshold.is_some();
                let mut calls = Vec::new();
                if needs_scaled {
                    calls.push(ReadCall::new(
                        a_token,
                        &AToken::scaledBalanceOfCall { user: owner },
                    ));
                }
                calls.push(ReadCall::new(
                    oracle,
                    &Oracle::getAssetPriceCall {
                        asset: address(&reference)?,
                    },
                ));
                if needs_id {
                    calls.push(ReadCall::new(
                        pool,
                        &Pool::getReserveDataCall {
                            asset: address(&reference)?,
                        },
                    ));
                }
                let mut observed = collector.collect(calls).await.into_iter();
                let scaled = if needs_scaled {
                    Some(decode::<AToken::scaledBalanceOfCall>(observed.next())?)
                } else {
                    None
                };
                let price = decode::<Oracle::getAssetPriceCall>(observed.next())?;
                let reserve_id = if needs_id {
                    Some(decode::<Pool::getReserveDataCall>(observed.next())?.id)
                } else {
                    None
                };
                details.collateral.push(HealthCollateral {
                    reference,
                    units: user.currentATokenBalance,
                    reserve,
                    scaled,
                    price,
                    reserve_id,
                });
            }
            Ok(details)
        }
        .await;
        Ok(HealthEvidence {
            horizon: self.horizon_math(),
            collateral_before: account.totalCollateralBase,
            debt_before: account.totalDebtBase,
            debt_price,
            oracle_units,
            details,
        })
    }
    /// Pin reserve identity and its income index without reading wallet funds.
    pub(crate) async fn aave_supply_plan_observation(
        &self,
        reference: &str,
    ) -> Result<(reads::Token, U256, u64)> {
        self.runtime.assert_chain_from_transport_or_rpc().await?;
        let (reader, block, before) = self.aave_risk_reader().await?;
        let token = reader.aave_token(reference, Some(block)).await?;
        let reserve = reader.reserve(&token.reference, Some(block)).await?;
        reader
            .verify_receipt_identity(&token, &reserve, Some(block))
            .await?;
        self.aave_risk_block_unchanged(block, &before).await?;
        Ok((token, u256(&reserve.normalized_income)?, block))
    }

    /// Observe only protocol index and USD units/price, never current funding or
    /// collateral. Future-funded plan steps can be reviewed before balances exist.
    pub(crate) async fn aave_borrow_plan_observation(
        &self,
        token: &reads::Token,
    ) -> Result<(U256, U256, u64)> {
        let (reader, block, before) = self.aave_risk_reader().await?;
        let pool = address(&self.aave()?.pool)?;
        let oracle = address(&self.aave()?.oracle)?;
        let index_call = Pool::getReserveNormalizedVariableDebtCall {
            asset: address(&token.reference)?,
        };
        let price_call = Oracle::getAssetPriceCall {
            asset: address(&token.reference)?,
        };
        let (index, base, unit, price) = tokio::try_join!(
            reader.runtime.call(pool, &index_call, Some(block)),
            reader
                .runtime
                .call(oracle, &Oracle::BASE_CURRENCYCall {}, Some(block)),
            reader
                .runtime
                .call(oracle, &Oracle::BASE_CURRENCY_UNITCall {}, Some(block)),
            reader.runtime.call(oracle, &price_call, Some(block)),
        )?;
        self.aave_risk_block_unchanged(block, &before).await?;
        require(
            base == Address::ZERO && unit == pow10(8)?,
            "AAVE_ORACLE_UNITS_UNSUPPORTED",
            "USD debt limits require the reviewed USD oracle base and eight-decimal units.",
        )?;
        Ok((index, price, block))
    }

    pub(crate) fn aave_debt_limits(
        &self,
        request: &request::Request,
        collateral_base: U256,
        debt_base: U256,
    ) -> Result<()> {
        if let Some(maximum) = &request.max_debt_usd {
            let max = exact(maximum, 8)?;
            require(
                debt_base <= max,
                "AAVE_DEBT_LIMIT",
                "Borrow exceeds the explicit maximum outstanding protocol USD debt.",
            )?;
        }
        if let Some(bps) = request.max_debt_to_collateral_bps {
            require(
                bps > 0
                    && bps <= 10000
                    && mul(debt_base, U256::from(10000))? <= mul(collateral_base, U256::from(bps))?,
                "AAVE_DEBT_COLLATERAL_LIMIT",
                "Borrow exceeds the explicit debt-to-collateral ratio.",
            )?;
        }
        Ok(())
    }
    /// Advisory only: projects the current position, which earlier plan steps may still change.
    pub(crate) async fn aave_borrow_plan_advisories(&self, input: &Value) -> Result<Vec<String>> {
        let r = request::parse("aave.borrow", input)?;
        let owner = address(&self.resolve(&r, true)?.address)?;
        let token = self
            .aave_token(r.token.as_deref().expect("schema"), None)
            .await?;
        let qty = exact(r.amount.as_deref().expect("schema"), token.decimals)?;
        let (collateral, debt, projected) = self
            .aave_health_projection(owner, &token, None, false, qty)
            .await?;
        let position = format!(
            "projected total debt ${} against collateral ${}, including existing debt",
            quantity(debt).to_decimal(8),
            quantity(collateral).to_decimal(8),
        );
        let mut warnings = Vec::new();
        if let Err(limit) = self.aave_debt_limits(&r, collateral, debt) {
            warnings.push(format!(
                "Against the current position this borrow would fail with {} ({position}). Unless an earlier plan step changes the position, revise the debt limits before approval.",
                limit.code
            ));
        }
        let minimum = exact(r.min_health_factor.as_deref().expect("schema"), 18)?;
        if projected < minimum {
            warnings.push(format!(
                "Against the current position this borrow would breach min_health_factor ({position}). Unless an earlier plan step adds collateral, revise the amount or limit before approval."
            ));
        }
        Ok(warnings)
    }
}

// GenericLogic: origin v3.6.0@5a230ec82fcb10afc7fe7cffa8978752fb17aa2b;
// Origin v3.7.0@cff15de6d1271b0c800fc001f4aea4c263e8a597 has the same risk kernel.
// Horizon@311a59fb1cec23ee254e84b28759fd1faf9c3711. Both use half-up wadDiv.
fn health_factor(legacy: bool, collateral: U256, weighted: U256, debt: U256) -> Result<U256> {
    if debt.is_zero() {
        return Ok(U256::MAX);
    }
    let numerator = if legacy {
        let average = if collateral.is_zero() {
            U256::ZERO
        } else {
            weighted / collateral
        };
        add(mul(collateral, average)?, U256::from(5000))? / U256::from(10000)
    } else {
        weighted
    };
    let quotient = add(mul(numerator, pow10(18)?)?, debt / U256::from(2))? / debt;
    Ok(if legacy {
        quotient
    } else {
        quotient / U256::from(10000)
    })
}
#[cfg(test)]
mod rounding_tests {
    use super::*;
    #[test]
    fn horizon_truncates_average_before_percent_and_wad_rounding() {
        let c = U256::from(3);
        let w = U256::from(25000);
        let d = U256::from(1);
        assert_eq!(
            health_factor(true, c, w, d).unwrap(),
            U256::from(2) * pow10(18).unwrap()
        );
        assert_eq!(
            health_factor(false, c, w, d).unwrap(),
            U256::from(25) * pow10(17).unwrap()
        );
        assert_eq!(health_factor(false, c, w, U256::ZERO).unwrap(), U256::MAX);
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::tests::{Fixture, adapter};
    use gofin_evm::{
        EvmRpc,
        alloy::sol_types::{SolCall, SolValue},
        rpc_bytes,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct SlowRiskRpc {
        inner: Arc<Fixture>,
        active: AtomicUsize,
        peak: AtomicUsize,
        headers: AtomicUsize,
        fail: bool,
        reorg: bool,
    }
    #[async_trait]
    impl EvmRpc for SlowRiskRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_call" {
                assert_eq!(
                    params[1],
                    json!("0x14"),
                    "Every risk call must use the captured block"
                );
                let data = rpc_bytes(&params[0]["data"])?;
                if data.starts_with(&Pool::getReservesListCall::SELECTOR) {
                    let reserves: Vec<Address> = (101..125)
                        .map(|n| {
                            Address::from_word(gofin_evm::alloy::primitives::B256::from(
                                U256::from(n).to_be_bytes::<32>(),
                            ))
                        })
                        .collect();
                    return Ok(json!(gofin_evm::hex_bytes(&reserves.abi_encode())));
                }
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                let _active = Active(&self.active);
                self.peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                if self.fail && data.starts_with(&DataProvider::getUserReserveDataCall::SELECTOR) {
                    return Err(GoFinError::validation(
                        "Required synthetic risk read failed",
                    ));
                }
                return self.inner.request(method, params).await;
            }
            let mut value = self.inner.request(method, params).await?;
            if method == "eth_getBlockByNumber"
                && self.headers.fetch_add(1, Ordering::SeqCst) > 0
                && self.reorg
            {
                value["hash"] = json!(format!("0x{}", "99".repeat(32)));
            }
            Ok(value)
        }
    }
    fn setup(fail: bool, reorg: bool) -> (EthereumAdapter, Arc<SlowRiskRpc>) {
        let inner = Arc::new(Fixture::new());
        let mut a = adapter(inner.clone());
        let rpc = Arc::new(SlowRiskRpc {
            inner,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            headers: AtomicUsize::new(0),
            fail,
            reorg,
        });
        a.runtime = Arc::new(EvmRuntime::new(rpc.clone(), 1, None));
        (a, rpc)
    }
    #[tokio::test]
    async fn complete_risk_capture_is_parallel_bounded_pinned_and_read_only() {
        let (a, rpc) = setup(false, false);
        let (reader, block, before) = a.aave_risk_reader().await.unwrap();
        let rows = reader
            .aave_risk_users(Address::ZERO, block, true)
            .await
            .unwrap();
        a.aave_risk_block_unchanged(block, &before).await.unwrap();
        assert_eq!(rows.len(), 24);
        assert!(rows.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(rpc.peak.load(Ordering::SeqCst) > 1);
        assert!(rpc.peak.load(Ordering::SeqCst) <= snapshot::SNAPSHOT_CONCURRENCY);
        assert_eq!(rpc.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            rpc.headers.load(Ordering::SeqCst),
            2,
            "Final identity check must bypass capture cache"
        );
        assert!(
            !rpc.inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "eth_sendRawTransaction")
        );
    }
    #[tokio::test]
    async fn required_failure_drains_workers_and_risk_reorg_is_rejected() {
        let (a, rpc) = setup(true, false);
        let (reader, block, _) = a.aave_risk_reader().await.unwrap();
        assert!(
            reader
                .aave_risk_users(Address::ZERO, block, true)
                .await
                .is_err()
        );
        assert_eq!(rpc.active.load(Ordering::SeqCst), 0);
        let (a, _) = setup(false, true);
        let (_, block, before) = a.aave_risk_reader().await.unwrap();
        assert_eq!(
            a.aave_risk_block_unchanged(block, &before)
                .await
                .unwrap_err()
                .code,
            "AAVE_READ_CHANGED"
        );
    }
    #[tokio::test]
    async fn preflight_cancellation_aborts_child_risk_reads() {
        let (a, rpc) = setup(false, false);
        let (reader, block, _) = a.aave_risk_reader().await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                reader.aave_risk_users(Address::ZERO, block, true)
            )
            .await
            .is_err()
        );
        for _ in 0..50 {
            if rpc.active.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            rpc.active.load(Ordering::SeqCst),
            0,
            "Timed-out preparation leaves no running reads"
        );
    }
}
