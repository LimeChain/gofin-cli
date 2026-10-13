//! Ordered bounded reads at one block, sharing a capture-local read cache.
use super::super::*;
use fin_evm::{EvmRuntime, quantity};
use tokio::task::JoinSet;
impl EthereumAdapter {
    pub(crate) async fn aave_rows(
        &self,
        explicit: Option<&Vec<String>>,
        owner: Address,
        block: u64,
        positions: bool,
    ) -> Result<Vec<Value>> {
        let tag = fin_evm::block_tag(Some(block));
        let before = self
            .runtime
            .rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        fin_evm::hash(before["hash"].as_str().unwrap_or(""))?;
        require(
            fin_evm::rpc_u64(&before["number"])? == block,
            "AAVE_READ_CHANGED",
            "Aave read block identity differs.",
        )?;
        let a = Arc::new(Self {
            config: self.config.clone(),
            wallets: self.wallets.clone(),
            clock: self.clock.clone(),
            oracle_clock: self.oracle_clock.clone(),
            wallet_valuation: self.wallet_valuation.clone(),
            token_discovery: self.token_discovery.clone(),
            runtime: Arc::new(EvmRuntime::new(
                Arc::new(snapshot::SnapshotRpc::new(
                    self.runtime.rpc.clone(),
                    block,
                    snapshot::SNAPSHOT_CONCURRENCY,
                )),
                self.config.chain_id,
                None,
            )),
        });
        let tokens = a.all_tokens(explicit, Some(block)).await?;
        let mut values: Vec<Option<Value>> = vec![None; tokens.len()];
        let mut next = tokens.into_iter().enumerate();
        let mut jobs = JoinSet::new();
        loop {
            while jobs.len() < snapshot::SNAPSHOT_CONCURRENCY {
                let Some((index, token)) = next.next() else {
                    break;
                };
                let a = a.clone();
                jobs.spawn(async move {
                    let row:Result<Value>=async {
                        let reserve=a.reserve(&token,Some(block)).await?;
                        if !positions {return Ok(json!(reserve));}
                        let user=a.user_reserve(&reserve.token,owner,Some(block)).await?;
                        let scaled_supply=a.runtime.call(address(&reserve.a_token)?, &abi::AToken::scaledBalanceOfCall {user:owner}, Some(block)).await?;
                        let supply_previous_index=a.runtime.call(address(&reserve.a_token)?, &abi::AToken::getPreviousIndexCall {user:owner}, Some(block)).await?;
                        let debt_previous_index=a.runtime.call(address(&reserve.variable_debt_token)?, &abi::AToken::getPreviousIndexCall {user:owner}, Some(block)).await?;
                        Ok(json!({"token":user.token,"supplied":user.supplied,"scaled_supply":quantity(scaled_supply),"supply_previous_index":quantity(supply_previous_index),"debt_previous_index":quantity(debt_previous_index),"stable_debt":user.stable_debt,"variable_debt":user.variable_debt,"scaled_variable_debt":user.scaled_variable_debt,"collateral_enabled":user.collateral_enabled,"reserve":reserve,"wallet_balance":quantity(a.balance_of(&reserve.token,owner,Some(block)).await?),"pool_allowance":quantity(a.allowance(&reserve.token,owner,address(&a.aave()?.pool)?,Some(block)).await?),"historical_principal":null,"historical_interest":null,"interest_attribution":"Current balances include interest. Original external principal requires historical proof; it is not inferred from current balances."}))
                    }.await;
                    (index,row)
                });
            }
            let Some(result) = jobs.join_next().await else {
                break;
            };
            match result {
                Ok((index, Ok(row))) => values[index] = Some(row),
                failure => {
                    jobs.abort_all();
                    while jobs.join_next().await.is_some() {}
                    return Err(match failure {
                        Ok((_, Err(error))) => error,
                        _ => error(
                            "AAVE_READ_TASK_FAILED",
                            "read",
                            "A required Aave read did not finish.",
                        ),
                    });
                }
            }
        }
        let header = self
            .runtime
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([fin_evm::block_tag(Some(block)), false]),
            )
            .await?;
        require(
            fin_evm::rpc_u64(&header["number"])? == block && header["hash"] == before["hash"],
            "AAVE_READ_CHANGED",
            "Aave read block identity changed.",
        )?;
        values
            .into_iter()
            .map(|row| {
                row.ok_or_else(|| {
                    error(
                        "AAVE_READ_INCOMPLETE",
                        "read",
                        "A required reserve observation is missing.",
                    )
                })
            })
            .collect()
    }
}

pub(crate) fn position_summary(rows: &[Value], debt_base: &Quantity, health: &Quantity) -> String {
    let mut entries = Vec::new();
    for row in rows {
        let decimals = row["token"]["decimals"]
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .unwrap_or(0);
        let label = crate::amm_permissions::safe_display_label(row["token"]["symbol"].as_str());
        let units = |key: &str| {
            row[key]
                .as_str()
                .and_then(|v| Quantity::new(v).ok())
                .unwrap_or_default()
        };
        let supplied = units("supplied");
        let debt = units("variable_debt");
        let stable = units("stable_debt");
        if !supplied.is_zero() || !debt.is_zero() || !stable.is_zero() {
            entries.push(format!(
                "{label}: supplied {}, variable debt {}, stable debt {}, collateral {}",
                supplied.to_decimal(decimals),
                debt.to_decimal(decimals),
                stable.to_decimal(decimals),
                if row["collateral_enabled"] == true {
                    "on"
                } else {
                    "off"
                }
            ));
        }
    }
    let risk = if debt_base.is_zero() {
        "no debt".into()
    } else {
        format!("health factor {}", health.to_decimal(18))
    };
    let count = entries.len();
    entries.truncate(6);
    let positions = if entries.is_empty() {
        "No supplied or borrowed positions".into()
    } else {
        entries.join("; ")
    };
    format!(
        "{positions}; {risk}. {} reserves checked{}. Supplied assets and debt are separate from wallet cash. Next: inspect aave.reserves before previewing a write.",
        rows.len(),
        if count > 6 {
            format!("; {} more positions in --json", count - 6)
        } else {
            String::new()
        }
    )
}
