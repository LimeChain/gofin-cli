//! Point-in-time Aave risk and mode checks, using protocol units and exact math.
use super::super::*;
use crate::abi::*;
use gofin_evm::{EvmRuntime, rpc_u64, u256};
use tokio::task::JoinSet;

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
    ) -> Result<Vec<(String, U256, DataProvider::getUserReserveDataReturn)>> {
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
    pub(crate) async fn aave_borrow_restrictions(
        &self,
        owner: Address,
        token: &reads::Token,
        qty: U256,
    ) -> Result<()> {
        let (reader, block, before) = self.aave_risk_reader().await?;
        let result = reader
            .aave_borrow_restrictions_at(owner, token, qty, block)
            .await;
        self.aave_risk_block_unchanged(block, &before).await?;
        result
    }
    async fn aave_borrow_restrictions_at(
        &self,
        owner: Address,
        token: &reads::Token,
        qty: U256,
        block: u64,
    ) -> Result<()> {
        let pool = address(&self.aave()?.pool)?;
        let mode = self.aave_mode(owner, Some(block)).await?;
        let config = self
            .runtime
            .call(
                pool,
                &Pool::getConfigurationCall {
                    asset: address(&token.reference)?,
                },
                Some(block),
            )
            .await?;
        if mode > U256::ZERO {
            require(
                mode <= U256::from(255),
                "AAVE_MODE_UNSUPPORTED",
                "Unrecognized eMode category.",
            )?;
            let id = mode.to::<u8>();
            let bitmap = self
                .runtime
                .call(
                    pool,
                    &Pool::getEModeCategoryBorrowableBitmapCall { id },
                    Some(block),
                )
                .await?;
            let reserve = self
                .runtime
                .call(
                    pool,
                    &Pool::getReserveDataCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?;
            require(
                reserve.id < 128 && (bitmap & (1u128 << reserve.id)) != 0,
                "AAVE_EMODE_BORROW_UNAVAILABLE",
                "Debt asset is not borrowable in the current eMode category.",
            )?;
        }
        // Eligibility needs only current configuration and user flags/debt,
        // not every reserve's token metadata, caps, indices and total supply.
        for (reference, other_config, user) in self.aave_risk_users(owner, block, true).await? {
            if !user.currentVariableDebt.is_zero() || !user.currentStableDebt.is_zero() {
                require(
                    reference == token.reference
                        || ((config >> 62usize) & U256::from(1)).is_zero()
                            && ((other_config >> 62usize) & U256::from(1)).is_zero(),
                    "AAVE_SILOED_BORROW_UNAVAILABLE",
                    "Siloed borrowing cannot mix another debt asset.",
                )?;
            }
            let ceiling = (other_config >> 212usize) & ((U256::from(1) << 40usize) - U256::from(1));
            if user.usageAsCollateralEnabled
                && !user.currentATokenBalance.is_zero()
                && !ceiling.is_zero()
            {
                require(
                    !((config >> 61usize) & U256::from(1)).is_zero(),
                    "AAVE_ISOLATION_BORROW_UNAVAILABLE",
                    "Debt asset is not permitted against isolated collateral.",
                )?;
                let data = self
                    .runtime
                    .call(
                        pool,
                        &Pool::getReserveDataCall {
                            asset: address(&reference)?,
                        },
                        Some(block),
                    )
                    .await?;
                // Isolation debt ceilings use two decimal borrowing units in
                // reviewed legacy V3 revisions, independently of oracle USD.
                let added = add(
                    mul(qty, U256::from(100))?,
                    pow10(token.decimals)? - U256::from(1),
                )? / pow10(token.decimals)?;
                require(
                    add(U256::from(data.isolationModeTotalDebt), added)? <= ceiling,
                    "AAVE_ISOLATION_DEBT_CEILING",
                    "Borrow exceeds the isolated collateral's remaining debt ceiling.",
                )?;
            }
        }
        Ok(())
    }
    pub(crate) async fn aave_removal_risk(
        &self,
        owner: Address,
        token: &reads::Token,
        removed: Option<U256>,
        minimum: Option<&str>,
    ) -> Result<()> {
        let (_, debt, projected) = self
            .aave_health_projection(owner, token, removed, true, U256::ZERO)
            .await?;
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
        let result = reader
            .aave_health_projection_at(owner, token, removed, apply_removal, added_debt, block)
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
        block: u64,
    ) -> Result<(U256, U256, U256)> {
        let pool = address(&self.aave()?.pool)?;
        let account = self
            .runtime
            .call(
                pool,
                &Pool::getUserAccountDataCall { user: owner },
                Some(block),
            )
            .await?;
        let oracle = address(&self.aave()?.oracle)?;
        if !added_debt.is_zero() {
            require(
                self.runtime
                    .call(oracle, &Oracle::BASE_CURRENCYCall {}, Some(block))
                    .await?
                    == Address::ZERO
                    && self
                        .runtime
                        .call(oracle, &Oracle::BASE_CURRENCY_UNITCall {}, Some(block))
                        .await?
                        == pow10(8)?,
                "AAVE_ORACLE_UNITS_UNSUPPORTED",
                "USD debt limits require the reviewed USD oracle base and eight-decimal units.",
            )?;
        }
        let added_base = if added_debt.is_zero() {
            U256::ZERO
        } else {
            let price = self
                .runtime
                .call(
                    oracle,
                    &Oracle::getAssetPriceCall {
                        asset: address(&token.reference)?,
                    },
                    Some(block),
                )
                .await?;
            require(
                !price.is_zero(),
                "ORACLE_INVALID",
                "Debt risk price must be positive.",
            )?;
            let product = mul(added_debt, price)?;
            let unit = pow10(token.decimals)?;
            if self.horizon_math() {
                product / unit
            } else {
                add(product, unit - U256::from(1))? / unit
            }
        };
        let debt = add(account.totalDebtBase, added_base)?;
        if debt.is_zero() {
            return Ok((account.totalCollateralBase, debt, U256::MAX));
        }
        let mode = self.aave_mode(owner, Some(block)).await?;
        require(
            mode <= U256::from(255),
            "AAVE_MODE_UNSUPPORTED",
            "Unrecognized eMode category.",
        )?;
        let category = if mode.is_zero() {
            None
        } else {
            Some(
                self.runtime
                    .call(
                        pool,
                        &Pool::getEModeCategoryCollateralConfigCall {
                            id: mode.to::<u8>(),
                        },
                        Some(block),
                    )
                    .await?,
            )
        };
        let bitmap = if mode.is_zero() {
            0
        } else {
            self.runtime
                .call(
                    pool,
                    &Pool::getEModeCategoryCollateralBitmapCall {
                        id: mode.to::<u8>(),
                    },
                    Some(block),
                )
                .await?
        };
        let mut weighted = U256::ZERO;
        let mut collateral = U256::ZERO;
        for (reference, _, user) in self.aave_risk_users(owner, block, false).await? {
            if !user.usageAsCollateralEnabled || user.currentATokenBalance.is_zero() {
                continue;
            }
            let reserve = self.reserve(&reference, Some(block)).await?;
            let mut units = user.currentATokenBalance;
            if apply_removal && reference == token.reference {
                units = match removed {
                    Some(qty) => {
                        let index = u256(&reserve.normalized_income)?;
                        let scaled = self
                            .runtime
                            .call(
                                address(&reserve.a_token)?,
                                &AToken::scaledBalanceOfCall { user: owner },
                                Some(block),
                            )
                            .await?;
                        let burned =
                            self.scaled_change(qty, index, &PositionKind::Supply, false)?;
                        let remaining = scaled.checked_sub(burned).ok_or_else(|| {
                            GoFinError::validation("Removal exceeds exact scaled claim.")
                        })?;
                        self.position_backing(remaining, index, &PositionKind::Supply)?
                    }
                    None => U256::ZERO,
                };
            }
            let price = self
                .runtime
                .call(
                    oracle,
                    &Oracle::getAssetPriceCall {
                        asset: address(&reference)?,
                    },
                    Some(block),
                )
                .await?;
            require(
                !price.is_zero(),
                "ORACLE_INVALID",
                "Collateral risk price must be positive.",
            )?;
            let base = mul(units, price)? / pow10(reserve.token.decimals)?;
            let mut threshold = u256(&reserve.liquidation_threshold)?;
            if let Some(category) = &category {
                let data = self
                    .runtime
                    .call(
                        pool,
                        &Pool::getReserveDataCall {
                            asset: address(&reference)?,
                        },
                        Some(block),
                    )
                    .await?;
                if data.id < 128 && (bitmap & (1u128 << data.id)) != 0 {
                    threshold = U256::from(category.liquidationThreshold);
                }
            }
            // Legacy Horizon excludes zero-threshold collateral entirely.
            if !self.horizon_math() || !threshold.is_zero() {
                collateral = add(collateral, base)?;
                weighted = add(weighted, mul(base, threshold)?)?;
            }
        }
        Ok((
            collateral,
            debt,
            health_factor(self.horizon_math(), collateral, weighted, debt)?,
        ))
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
