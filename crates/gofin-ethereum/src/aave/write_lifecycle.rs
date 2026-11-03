//! Pure financial decisions over typed Aave observations.
//!
//! These functions have no adapter, RPC, clock, signer or persistence access.
use super::super::*;
use crate::collection::{ReadCall, ReadCollector};
use gofin_evm::alloy::sol_types::SolCall;

fn decode<C: SolCall>(value: Option<Result<Vec<u8>>>) -> Result<C::Return> {
    let bytes = value.ok_or_else(|| {
        error(
            "MULTICALL_INCOMPLETE",
            "read",
            "Required Aave observation is missing.",
        )
    })??;
    C::abi_decode_returns(&bytes).map_err(|_| {
        error(
            "CONTRACT_DATA_INVALID",
            "read",
            "Contract return does not match the reviewed ABI.",
        )
    })
}

pub(crate) struct SupplyEvidence {
    pub reserve: reads::Reserve,
    pub amount: U256,
    pub allowance: U256,
}

pub(crate) fn evaluate_supply(e: &SupplyEvidence) -> Result<()> {
    require(
        !e.reserve.frozen,
        "RESERVE_FROZEN",
        "Frozen reserves cannot accept supply.",
    )?;
    let cap = mul(
        u256(&e.reserve.supply_cap)?,
        pow10(e.reserve.token.decimals)?,
    )?;
    require(
        cap.is_zero() || add(u256(&e.reserve.total_supply)?, e.amount)? <= cap,
        "SUPPLY_CAP",
        "Supply exceeds the observed reserve cap.",
    )?;
    require(
        e.allowance >= e.amount && e.allowance < U256::MAX,
        "ALLOWANCE_INSUFFICIENT",
        "Finite Pool allowance must cover exact supply.",
    )
}

pub(crate) struct RepayEvidence {
    pub debt: U256,
    pub allowance: U256,
    pub amount: U256,
    pub all: bool,
}

pub(crate) fn evaluate_repay(e: &RepayEvidence) -> Result<()> {
    require(
        e.debt > U256::ZERO,
        "NO_VARIABLE_DEBT",
        "No variable debt exists for this reserve.",
    )?;
    require(
        e.allowance < U256::MAX && e.allowance >= if e.all { e.debt } else { e.amount },
        "ALLOWANCE_INSUFFICIENT",
        "Repayment requires finite sufficient Pool allowance.",
    )?;
    if e.all {
        require(
            e.debt <= e.amount && e.allowance <= e.amount,
            "REPAY_MAXIMUM",
            "Repay all debt/allowance exceeds explicit max_amount.",
        )?;
    }
    Ok(())
}

pub(crate) struct WithdrawEvidence {
    pub claim: U256,
    pub liquidity: U256,
    pub amount: U256,
    pub all: bool,
    pub approved_minimum: Option<U256>,
}

pub(crate) fn evaluate_withdraw(e: &WithdrawEvidence) -> Result<U256> {
    require(
        if e.all {
            e.claim > U256::ZERO && e.claim <= e.amount && e.claim <= e.liquidity
        } else {
            e.amount <= e.claim && e.amount <= e.liquidity
        },
        "WITHDRAW_LIMIT",
        "Withdrawal must fit its exact claim, finite cap and available liquidity.",
    )?;
    let minimum = if e.all {
        e.approved_minimum.unwrap_or(e.claim)
    } else {
        e.amount
    };
    require(
        e.claim >= minimum,
        "WITHDRAW_MINIMUM",
        "Current claim decreased below the frozen return minimum.",
    )?;
    Ok(minimum)
}

pub(crate) struct CollateralEvidence {
    pub reserve: reads::Reserve,
    pub user: reads::UserReserve,
    pub enabled: bool,
}

pub(crate) fn evaluate_collateral(e: &CollateralEvidence) -> Result<()> {
    require(
        !e.user.supplied.is_zero(),
        "NO_SUPPLIED_POSITION",
        "Collateral toggles require an existing supplied position.",
    )?;
    require(
        !e.enabled || (e.reserve.collateral_eligible && !e.reserve.ltv.is_zero()),
        "COLLATERAL_UNAVAILABLE",
        "Reserve is ineligible for collateral use.",
    )
}

pub(crate) struct BorrowEvidence {
    pub reserve: reads::Reserve,
    pub amount: U256,
    pub collateral_base: U256,
    pub debt: U256,
    pub projected_health: U256,
    pub previous_debt: U256,
    pub available_borrow: U256,
    pub request: request::Request,
}

pub(crate) fn evaluate_borrow_reserve(reserve: &reads::Reserve, amount: U256) -> Result<()> {
    require(
        !reserve.frozen && reserve.borrowing,
        "BORROW_UNAVAILABLE",
        "Reserve must permit variable borrowing and be unfrozen.",
    )?;
    let cap = mul(u256(&reserve.borrow_cap)?, pow10(reserve.token.decimals)?)?;
    require(
        cap.is_zero() || add(u256(&reserve.total_debt)?, amount)? <= cap,
        "BORROW_CAP",
        "Borrow exceeds the observed reserve cap.",
    )?;
    require(
        u256(&reserve.liquidity)? >= amount,
        "LIQUIDITY_INSUFFICIENT",
        "Reserve lacks exact underlying liquidity.",
    )?;
    Ok(())
}

pub(crate) fn evaluate_borrow(e: &BorrowEvidence) -> Result<()> {
    evaluate_borrow_reserve(&e.reserve, e.amount)?;
    if let Some(maximum) = &e.request.max_debt_usd {
        require(
            e.debt <= exact(maximum, 8)?,
            "AAVE_DEBT_LIMIT",
            "Borrow exceeds the explicit maximum outstanding protocol USD debt.",
        )?;
    }
    if let Some(bps) = e.request.max_debt_to_collateral_bps {
        require(
            bps > 0
                && bps <= 10000
                && mul(e.debt, U256::from(10000))? <= mul(e.collateral_base, U256::from(bps))?,
            "AAVE_DEBT_COLLATERAL_LIMIT",
            "Borrow exceeds the explicit debt-to-collateral ratio.",
        )?;
    }
    let minimum = exact(e.request.min_health_factor.as_deref().expect("schema"), 18)?;
    require(
        minimum > pow10(18)?
            && e.projected_health >= minimum
            && e.debt.saturating_sub(e.previous_debt) <= e.available_borrow,
        "HEALTH_FACTOR_LIMIT",
        "Borrow exceeds collateral capacity or explicit minimum health factor.",
    )
}

/// Exact post-simulation Aave accounting, derived only from typed evidence.
pub(crate) struct AccountingFacts<'a> {
    pub operation: &'a str,
    pub pool: &'a str,
    pub token_reference: &'a str,
    pub chain_id: u64,
    pub resource: &'a ResourceId,
    pub asset: &'a AssetId,
    pub binding: &'a Binding,
    pub amount: U256,
    pub withdraw_all: bool,
    pub repay_all: bool,
}
pub(crate) fn evaluate_accounting(
    facts: &AccountingFacts<'_>,
    intent: &mut FinancialIntent,
) -> Result<()> {
    let supply_pos = position(facts.pool, facts.token_reference, "supply");
    let debt_pos = position(facts.pool, facts.token_reference, "variable-debt");
    let cash = AccountingLocation::Cash {
        resource: facts.resource.clone(),
    };
    let supply = AccountingLocation::Position {
        resource: facts.resource.clone(),
        position: supply_pos.clone(),
        position_kind: PositionKind::Supply,
    };
    let debt = AccountingLocation::Position {
        resource: facts.resource.clone(),
        position: debt_pos.clone(),
        position_kind: PositionKind::VariableDebt,
    };
    let index: Quantity = serde_json::from_value(
        facts
            .binding
            .supply_anchor
            .as_ref()
            .expect("anchor")
            .evidence["normalized_index"]
            .clone(),
    )
    .map_err(|_| GoFinError::validation("Invalid trusted index."))?;
    let quantum = aave::math::supply_rounding_limit(u256(&index)?)?;
    let ceiling = add(facts.amount, quantum)?;
    if matches!(facts.operation, "aave.supply" | "aave.withdraw") {
        // A complete withdrawal's observed claim, including yield,
        // must fit its finite max_amount. Partial withdrawals keep
        // their prior claim-plus-request envelope.
        let maximum_change = if facts.operation == "aave.withdraw" && facts.withdraw_all {
            facts.amount
        } else {
            add(
                facts.amount,
                u256(facts.binding.supplied_before.as_ref().expect("claim"))?,
            )?
        };
        intent.expected.push(EffectConstraint::Accrual {
            resource: facts.resource.clone(),
            position: supply_pos.clone(),
            asset: facts.asset.clone(),
            position_kind: PositionKind::Supply,
            maximum_change: quantity(maximum_change),
        });
        let before = facts
            .binding
            .collateral_before
            .expect("collateral")
            .to_string();
        intent.expected.push(EffectConstraint::AuthorityTransition {
            resource: facts.resource.clone(),
            authority: collateral(facts.pool, facts.token_reference),
            before: before.clone(),
            allowed_after: vec![
                before,
                if facts.operation == "aave.supply" {
                    "true".into()
                } else {
                    "false".into()
                },
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        });
    }
    if matches!(facts.operation, "aave.borrow" | "aave.repay") {
        // For repay-all the complete paid debt, including accrued
        // interest, must fit this fresh preview's finite max_amount.
        // Partial repayment keeps its distinct debt-plus-installment
        // envelope.
        let finite_full_close = facts.operation == "aave.repay" && facts.repay_all;
        let maximum_change = if finite_full_close {
            facts.amount
        } else {
            add(
                facts.amount,
                u256(facts.binding.debt_before.as_ref().expect("claim"))?,
            )?
        };
        intent.expected.push(EffectConstraint::Accrual {
            resource: facts.resource.clone(),
            position: debt_pos.clone(),
            asset: facts.asset.clone(),
            position_kind: PositionKind::VariableDebt,
            maximum_change: quantity(maximum_change),
        });
    }
    match facts.operation {
        "aave.supply" => {
            intent.expected.push(EffectConstraint::OriginMovement {
                from: cash,
                to: supply,
                debit_asset: facts.asset.clone(),
                credit_asset: facts.asset.clone(),
                maximum_debit: quantity(facts.amount),
                minimum_credit: quantity(facts.amount.saturating_sub(quantum).max(U256::from(1))),
            });
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: facts.resource.clone(),
                },
                amount: amount(facts.asset, quantum),
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        "aave.borrow" => intent.expected.push(EffectConstraint::OriginMovement {
            from: debt,
            to: cash,
            debit_asset: facts.asset.clone(),
            credit_asset: facts.asset.clone(),
            maximum_debit: quantity(facts.amount),
            minimum_credit: quantity(facts.amount),
        }),
        "aave.repay" => {
            intent.expected.push(EffectConstraint::OriginMovement {
                from: cash,
                to: debt,
                debit_asset: facts.asset.clone(),
                credit_asset: facts.asset.clone(),
                maximum_debit: quantity(facts.amount),
                minimum_credit: Quantity::from_u64(1),
            });
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: facts.resource.clone(),
                },
                amount: amount(facts.asset, facts.amount),
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        "aave.withdraw" => {
            for e in &mut intent.expected {
                if let EffectConstraint::Position { maximum_change, .. } = e {
                    *maximum_change = quantity(if facts.withdraw_all {
                        facts.amount
                    } else {
                        ceiling
                    });
                }
            }
            intent.expected.push(EffectConstraint::OriginMovement {
                from: supply,
                to: AccountingLocation::Cash {
                    resource: resource(
                        facts.chain_id,
                        address(facts.binding.recipient.as_deref().expect("recipient"))?,
                    ),
                },
                debit_asset: facts.asset.clone(),
                credit_asset: facts.asset.clone(),
                maximum_debit: quantity(if facts.withdraw_all {
                    facts.amount
                } else {
                    ceiling
                }),
                minimum_credit: if facts.withdraw_all {
                    facts.binding.minimum_output.clone().expect("minimum")
                } else {
                    quantity(facts.amount)
                },
            });
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Position {
                    resource: facts.resource.clone(),
                    position: supply_pos.clone(),
                    position_kind: PositionKind::Supply,
                },
                amount: amount(facts.asset, quantum),
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        _ => {}
    }
    Ok(())
}

/// Action-specific economic effects, independent of transport and signing.
pub(crate) enum ActionEffects {
    Supply,
    Borrow {
        maximum_debt: U256,
    },
    Repay,
    Withdraw {
        recipient: ResourceId,
        minimum_credit: U256,
        maximum_position_change: U256,
    },
    Collateral {
        before: bool,
        after: bool,
    },
}
pub(crate) struct EffectFacts<'a> {
    pub resource: &'a ResourceId,
    pub asset: &'a AssetId,
    pub supply_position: &'a str,
    pub debt_position: &'a str,
    pub collateral_authority: &'a str,
    pub amount: U256,
}
pub(crate) fn evaluate_action_effects(
    f: &EffectFacts<'_>,
    action: ActionEffects,
    intent: &mut FinancialIntent,
) {
    match action {
        ActionEffects::Supply => {
            intent.debits.push(ResourceAmount {
                resource: f.resource.clone(),
                amount: amount(f.asset, f.amount),
            });
            intent.expected.push(EffectConstraint::Debit {
                resource: f.resource.clone(),
                asset: f.asset.clone(),
                maximum_debit: quantity(f.amount),
            });
            intent.expected.push(EffectConstraint::Position {
                resource: f.resource.clone(),
                position: f.supply_position.into(),
                asset: f.asset.clone(),
                direction: Direction::Increase,
                maximum_change: quantity(f.amount),
            });
        }
        ActionEffects::Borrow { maximum_debt } => {
            intent.expected.push(EffectConstraint::Debt {
                resource: f.resource.clone(),
                debt: f.debt_position.into(),
                asset: f.asset.clone(),
                direction: Direction::Increase,
                maximum_change: quantity(maximum_debt),
            });
            intent.expected.push(EffectConstraint::Credit {
                to: f.resource.clone(),
                asset: f.asset.clone(),
                minimum_credit: quantity(f.amount),
            });
        }
        ActionEffects::Repay => {
            intent.debits.push(ResourceAmount {
                resource: f.resource.clone(),
                amount: amount(f.asset, f.amount),
            });
            intent.expected.push(EffectConstraint::Debit {
                resource: f.resource.clone(),
                asset: f.asset.clone(),
                maximum_debit: quantity(f.amount),
            });
            intent.expected.push(EffectConstraint::Debt {
                resource: f.resource.clone(),
                debt: f.debt_position.into(),
                asset: f.asset.clone(),
                direction: Direction::Decrease,
                maximum_change: quantity(f.amount),
            });
        }
        ActionEffects::Withdraw {
            recipient,
            minimum_credit,
            maximum_position_change,
        } => {
            intent.expected.push(EffectConstraint::Position {
                resource: f.resource.clone(),
                position: f.supply_position.into(),
                asset: f.asset.clone(),
                direction: Direction::Decrease,
                maximum_change: quantity(maximum_position_change),
            });
            intent.expected.push(EffectConstraint::Credit {
                to: recipient,
                asset: f.asset.clone(),
                minimum_credit: quantity(minimum_credit),
            });
        }
        ActionEffects::Collateral { before, after } => {
            intent.expected.push(EffectConstraint::Authority {
                resource: f.resource.clone(),
                authority: f.collateral_authority.into(),
                before: before.to_string(),
                after: after.to_string(),
            })
        }
    }
}

/// Durable input is distinct from any one capture's observed financial facts.
#[derive(Clone)]
pub(crate) struct WriteInput {
    pub operation: String,
    pub input: Value,
    pub request: request::Request,
    pub context: PreparationContext,
}

pub(crate) struct ContextAnchorEvidence {
    pub context: crate::proof::aave_observation::AnchorContext,
    pub prior: Option<PositionAnchor>,
    pub facts: crate::proof::aave_observation::AnchorFacts,
    pub reopened: bool,
    pub historical_headers: Option<(Value, Value)>,
    pub observed_at: String,
}

/// Common observations captured at one numeric block; no client or signer is retained.
pub(crate) struct WriteEvidence {
    pub chain_id: u64,
    pub wallet: WalletConnection,
    pub pool: String,
    pub block: u64,
    pub block_hash: String,
    pub proxy_identity: String,
    pub token: reads::Token,
    pub reserve: super::read_lifecycle::WriteReserveFacts,
    pub pool_revision: U256,
    pub receipt_identities: [(Address, Address); 2],
    pub user: reads::UserReserve,
    pub mode: U256,
    pub receipt_revision: Option<U256>,
    pub scaled_supply: U256,
    pub allowance: Option<U256>,
    pub wallet_balance: Option<U256>,
    pub account: Option<abi::Pool::getUserAccountDataReturn>,
    pub debt_index: Option<U256>,
    pub supply_anchor: ContextAnchorEvidence,
    pub debt_anchor: ContextAnchorEvidence,
    pub original: Option<Binding>,
    pub risk: WriteRiskEvidence,
}

impl EthereumAdapter {
    pub(crate) async fn prepare_aave_typed(
        &self,
        operation: &str,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let typed = WriteInput {
            operation: operation.into(),
            input: input.clone(),
            request: request::parse(operation, input)?,
            context: context.clone(),
        };
        let evidence = self.collect_aave_write(&typed, original).await?;
        let candidate = evaluate_write(&typed, &evidence)?;
        let final_evidence = self.collect_aave_final(&candidate).await?;
        finalize_write(&candidate, &final_evidence)
    }
}

impl EthereumAdapter {
    /// Acquire only current protocol and wallet evidence. Financial intent and
    /// bound calculations belong to the synchronous evaluator.
    pub(crate) async fn collect_aave_write(
        &self,
        input: &WriteInput,
        original: Option<&Binding>,
    ) -> Result<WriteEvidence> {
        self.require_operation(&input.operation)?;
        let selected = self.selected_market(&input.input)?;
        let adapter = selected.as_ref().unwrap_or(self);
        let wallet = adapter.resolve(&input.request, true)?.clone();
        let owner = address(&wallet.address)?;
        adapter.runtime.assert_chain().await?;
        let block = gofin_evm::rpc_u64(
            &adapter
                .runtime
                .rpc
                .request("eth_blockNumber", json!([]))
                .await?,
        )?;
        let tag = gofin_evm::block_tag(Some(block));
        let params = json!([tag, false]);
        let header = adapter
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params.clone())
            .await?;
        let hash = header["hash"].as_str().unwrap_or("").to_owned();
        gofin_evm::hash(&hash)?;
        require(
            gofin_evm::rpc_u64(&header["number"])? == block,
            "AAVE_READ_CHANGED",
            "Preparation block identity differs.",
        )?;
        let mut reader = adapter.clone();
        reader.runtime = Arc::new(EvmRuntime::new(
            Arc::new(snapshot::SnapshotRpc::preparation(
                adapter.runtime.rpc.clone(),
                block,
            )),
            adapter.config.chain_id,
            None,
        ));
        let pool = reader.aave()?.pool.clone();
        let pool_address = address(&pool)?;
        let provider = reader.data_provider(Some(block)).await?;
        let proxy_identity = reader.aave_proxy_identity_at(Some(block)).await?;
        let mut collector = ReadCollector::new(&reader, block).await;
        let reserve = super::read_lifecycle::collect_write_reserve(
            &reader,
            block,
            owner,
            input.request.token.as_deref().expect("schema"),
            &mut collector,
        )
        .await?;
        let token = reserve.token()?;
        let asset = address(&token.reference)?;
        let (a_token, _stable_debt, variable_debt) = reserve.receipt_addresses()?;
        let receipt = match input.operation.as_str() {
            "aave.supply" | "aave.withdraw" => Some((a_token, true)),
            "aave.borrow" | "aave.repay" => Some((variable_debt, false)),
            "aave.collateral" => None,
            _ => return Err(GoFinError::validation("Unsupported Aave write operation.")),
        };
        let has_cash = matches!(input.operation.as_str(), "aave.supply" | "aave.repay");
        let borrow = input.operation == "aave.borrow";
        let has_account = borrow
            || input.operation == "aave.withdraw"
            || (input.operation == "aave.collateral" && input.request.enabled == Some(false));
        let mut calls = vec![
            ReadCall::new(
                provider,
                &abi::DataProvider::getUserReserveDataCall { asset, user: owner },
            ),
            ReadCall::new(pool_address, &abi::Pool::getUserEModeCall { user: owner }),
            ReadCall::new(a_token, &abi::AToken::scaledBalanceOfCall { user: owner }),
        ];
        calls.extend([
            ReadCall::new(pool_address, &abi::Pool::POOL_REVISIONCall {}),
            ReadCall::new(a_token, &abi::AToken::POOLCall {}),
            ReadCall::new(a_token, &abi::AToken::UNDERLYING_ASSET_ADDRESSCall {}),
            ReadCall::new(variable_debt, &abi::AToken::POOLCall {}),
            ReadCall::new(variable_debt, &abi::AToken::UNDERLYING_ASSET_ADDRESSCall {}),
        ]);
        if let Some((target, supply)) = receipt {
            calls.push(if supply {
                ReadCall::new(target, &abi::AToken::ATOKEN_REVISIONCall {})
            } else {
                ReadCall::new(target, &abi::AToken::DEBT_TOKEN_REVISIONCall {})
            });
        }
        if has_cash {
            calls.push(ReadCall::new(
                asset,
                &abi::Erc20::allowanceCall {
                    owner,
                    spender: pool_address,
                },
            ));
            calls.push(ReadCall::new(asset, &abi::Erc20::balanceOfCall { owner }));
        }
        if has_account {
            calls.push(ReadCall::new(
                pool_address,
                &abi::Pool::getUserAccountDataCall { user: owner },
            ));
        }
        if borrow {
            calls.push(ReadCall::new(
                pool_address,
                &abi::Pool::getReserveNormalizedVariableDebtCall { asset },
            ));
        }
        let mut results = collector.collect(calls).await.into_iter();
        let raw_user = decode::<abi::DataProvider::getUserReserveDataCall>(results.next())?;
        let user = reads::UserReserve {
            token: token.clone(),
            supplied: quantity(raw_user.currentATokenBalance),
            stable_debt: quantity(raw_user.currentStableDebt),
            variable_debt: quantity(raw_user.currentVariableDebt),
            scaled_variable_debt: quantity(raw_user.scaledVariableDebt),
            collateral_enabled: raw_user.usageAsCollateralEnabled,
        };
        let mode = decode::<abi::Pool::getUserEModeCall>(results.next())?;
        let scaled_supply = decode::<abi::AToken::scaledBalanceOfCall>(results.next())?;
        let pool_revision = decode::<abi::Pool::POOL_REVISIONCall>(results.next())?;
        let receipt_identities = [
            (
                decode::<abi::AToken::POOLCall>(results.next())?,
                decode::<abi::AToken::UNDERLYING_ASSET_ADDRESSCall>(results.next())?,
            ),
            (
                decode::<abi::AToken::POOLCall>(results.next())?,
                decode::<abi::AToken::UNDERLYING_ASSET_ADDRESSCall>(results.next())?,
            ),
        ];
        let receipt_revision = if let Some((_, supply)) = receipt {
            Some(if supply {
                decode::<abi::AToken::ATOKEN_REVISIONCall>(results.next())?
            } else {
                decode::<abi::AToken::DEBT_TOKEN_REVISIONCall>(results.next())?
            })
        } else {
            None
        };
        let (allowance, wallet_balance) = if has_cash {
            (
                Some(decode::<abi::Erc20::allowanceCall>(results.next())?),
                Some(decode::<abi::Erc20::balanceOfCall>(results.next())?),
            )
        } else {
            (None, None)
        };
        let account = if has_account {
            Some(decode::<abi::Pool::getUserAccountDataCall>(results.next())?)
        } else {
            None
        };
        let debt_index = if borrow {
            Some(decode::<abi::Pool::getReserveNormalizedVariableDebtCall>(
                results.next(),
            )?)
        } else {
            None
        };
        let risk = if input.operation == "aave.borrow" {
            // The collector needs only a positive proposed debt to determine which
            // oracle/risk observations to gather; exact rounding is evaluated later.
            WriteRiskEvidence::Borrow(
                reader
                    .collect_borrow_risk(
                        owner,
                        &token,
                        U256::from(1),
                        super::risk::RiskCapture {
                            block,
                            collector: &mut collector,
                            selected_reserve: Some(&reserve),
                            account: account.as_ref(),
                        },
                    )
                    .await?,
            )
        } else if input.operation == "aave.withdraw" && user.collateral_enabled {
            // A partial or full withdrawal both require the scaled receipt
            // observation; the actual amount is interpreted by the evaluator.
            let removed = Some(U256::ZERO);
            WriteRiskEvidence::Removal(
                reader
                    .collect_removal_health_at(
                        owner,
                        &token,
                        removed,
                        super::risk::RiskCapture {
                            block,
                            collector: &mut collector,
                            selected_reserve: Some(&reserve),
                            account: account.as_ref(),
                        },
                    )
                    .await?,
            )
        } else if input.operation == "aave.collateral"
            && input.request.enabled == Some(false)
            && user.collateral_enabled
        {
            WriteRiskEvidence::Removal(
                reader
                    .collect_removal_health_at(
                        owner,
                        &token,
                        None,
                        super::risk::RiskCapture {
                            block,
                            collector: &mut collector,
                            selected_reserve: Some(&reserve),
                            account: account.as_ref(),
                        },
                    )
                    .await?,
            )
        } else {
            WriteRiskEvidence::None
        };
        let anchor_capture = AnchorCapture {
            reader: &reader,
            live: adapter,
            input,
            new_attempt: original.is_none(),
            token: &token,
            owner,
            block,
        };
        let supply_anchor = collect_context_anchor(
            &anchor_capture,
            &mut collector,
            a_token,
            PositionKind::Supply,
        )
        .await?;
        let debt_anchor = collect_context_anchor(
            &anchor_capture,
            &mut collector,
            variable_debt,
            PositionKind::VariableDebt,
        )
        .await?;
        let after = adapter
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params)
            .await?;
        require(
            gofin_evm::rpc_u64(&after["number"])? == block && after["hash"] == header["hash"],
            "AAVE_READ_CHANGED",
            "Preparation block became noncanonical.",
        )?;
        Ok(WriteEvidence {
            chain_id: adapter.config.chain_id,
            wallet,
            pool,
            block,
            block_hash: hash,
            proxy_identity,
            token,
            reserve,
            pool_revision,
            receipt_identities,
            user,
            mode,
            receipt_revision,
            scaled_supply,
            allowance,
            wallet_balance,
            account,
            debt_index,
            supply_anchor,
            debt_anchor,
            original: original.cloned(),
            risk,
        })
    }
}

#[derive(Clone, Copy)]
struct AnchorCapture<'a> {
    reader: &'a EthereumAdapter,
    live: &'a EthereumAdapter,
    input: &'a WriteInput,
    new_attempt: bool,
    token: &'a reads::Token,
    owner: Address,
    block: u64,
}
async fn collect_context_anchor(
    capture: &AnchorCapture<'_>,
    collector: &mut ReadCollector,
    receipt: Address,
    kind: PositionKind,
) -> Result<ContextAnchorEvidence> {
    let AnchorCapture {
        reader,
        live,
        input,
        new_attempt,
        token,
        owner,
        block,
    } = *capture;
    use crate::proof::aave_observation::AnchorContext;
    let context = AnchorContext {
        chain_id: reader.config.chain_id,
        pool: reader.aave()?.pool.clone(),
        owner,
        asset: token.asset(reader),
        receipt_token: receipt,
        kind: kind.clone(),
        horizon: reader.horizon_math(),
    };
    let resource = resource(reader.config.chain_id, owner);
    let name = position(
        &context.pool,
        &token.reference,
        if kind == PositionKind::Supply {
            "supply"
        } else {
            "variable-debt"
        },
    );
    let found: Vec<_> = input
        .context
        .positions
        .iter()
        .filter(|anchor| {
            anchor.resource == resource && anchor.position == name && anchor.position_kind == kind
        })
        .cloned()
        .collect();
    require(
        found.len() <= 1,
        "ANCHOR_DUPLICATE",
        "Trusted preparation context duplicates one physical position.",
    )?;
    let prior = found.into_iter().next();
    let reopened = new_attempt
        && prior.as_ref().is_some_and(|a| {
            a.principal.atoms.is_zero() && a.scaled_receipt.as_ref().is_some_and(Quantity::is_zero)
        });
    let old_block = prior
        .as_ref()
        .map(|a| {
            a.evidence["block"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    error(
                        "ANCHOR_BLOCK_INVALID",
                        "prepare",
                        "Anchor inclusion block is missing.",
                    )
                })
        })
        .transpose()?;
    let old_params = old_block.map(|n| json!([gofin_evm::block_tag(Some(n)), false]));
    let before = if reopened {
        Some(
            live.runtime
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    old_params.clone().expect("prior block"),
                )
                .await?,
        )
    } else {
        None
    };
    let facts_block = if reopened || prior.is_none() {
        block
    } else {
        old_block.expect("prior block")
    };
    let zero = reopened
        || prior
            .as_ref()
            .is_some_and(|a| a.evidence["method"] == json!("aave-zero-observed-backing"));
    let facts = if facts_block == block {
        reader
            .observe_anchor_facts_with_collector(&context, facts_block, zero, collector)
            .await?
    } else {
        reader
            .observe_anchor_facts(&context, facts_block, zero)
            .await?
    };
    let historical_headers = if let Some(before) = before {
        let after = live
            .runtime
            .rpc
            .request("eth_getBlockByNumber", old_params.expect("prior block"))
            .await?;
        Some((before, after))
    } else {
        None
    };
    Ok(ContextAnchorEvidence {
        context,
        prior,
        facts,
        reopened,
        historical_headers,
        observed_at: now(),
    })
}

/// Reconstruct or validate one lineage from collected facts without I/O.
pub(crate) fn evaluate_context_anchor(e: &ContextAnchorEvidence) -> Result<PositionAnchor> {
    let Some(prior) = &e.prior else {
        return e.context.anchor(
            &e.facts,
            false,
            U256::ZERO,
            U256::ZERO,
            e.observed_at.clone(),
        );
    };
    if !e.reopened {
        e.context.validate_prior(prior, &e.facts)?;
        return Ok(prior.clone());
    }
    let context = &e.context;
    let name = position(
        &context.pool,
        &context.asset.reference,
        if context.kind == PositionKind::Supply {
            "supply"
        } else {
            "variable-debt"
        },
    );
    let method = if context.horizon {
        "aave-horizon-revision-3-1-half-up-backing"
    } else {
        "aave-v3-revision-5-exact-scaled-backing"
    };
    require(
        prior.evidence_version == 1
            && prior.resource == resource(context.chain_id, context.owner)
            && prior.principal.asset == context.asset
            && prior.position_kind == context.kind
            && prior.position == name
            && (prior.evidence["method"] == json!(method)
                || prior.evidence["method"] == json!("aave-zero-observed-backing"))
            && prior.evidence["chain_id"] == json!(context.chain_id)
            && prior.evidence["pool"] == json!(context.pool)
            && prior.evidence["receipt_token"] == json!(lower_address(context.receipt_token)),
        "ANCHOR_IDENTITY_INVALID",
        "Trusted anchor does not bind the same immutable position representation.",
    )?;
    let old_block = prior.evidence["block"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            error(
                "ANCHOR_BLOCK_INVALID",
                "prepare",
                "Closed lineage block is missing.",
            )
        })?;
    let index: Quantity = serde_json::from_value(prior.evidence["normalized_index"].clone())
        .map_err(|_| {
            error(
                "ANCHOR_INDEX_INVALID",
                "prepare",
                "Closed lineage index is invalid.",
            )
        })?;
    let index = u256(&index)?;
    require(
        index >= pow10(27)? && index < (U256::from(1) << 128),
        "ANCHOR_INDEX_INVALID",
        "Closed lineage index exceeds reviewed bounds.",
    )?;
    let expected = json!({"method":prior.evidence["method"],"chain_id":context.chain_id,"pool":context.pool,"underlying":context.asset.reference,"receipt_token":lower_address(context.receipt_token),"block":old_block,"block_hash":prior.evidence["block_hash"],"normalized_index":quantity(index),"scaled_balance":Quantity::zero(),"deferred_unpaid_rounding":Quantity::zero(),"deferred_unpaid_interest":Quantity::zero()});
    require(
        prior.evidence == expected,
        "ANCHOR_PROOF_MISMATCH",
        "Closed lineage must have exact zero backing and expenses with valid evidence.",
    )?;
    let hash = prior.evidence["block_hash"].as_str().unwrap_or("");
    gofin_evm::hash(hash)?;
    let (before, after) = e
        .historical_headers
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Missing closed lineage headers."))?;
    for header in [before, after] {
        require(
            gofin_evm::rpc_u64(&header["number"])? == old_block && header["hash"] == hash,
            "ANCHOR_PROOF_MISMATCH",
            "Closed lineage canonical block no longer matches.",
        )?;
    }
    require(
        e.facts.block >= old_block,
        "ANCHOR_BLOCK_INVALID",
        "Closed lineage cannot lie after the current block.",
    )?;
    e.context.anchor(&e.facts, true, U256::ZERO, U256::ZERO, e.observed_at.clone()).map_err(|failure| if failure.code == "ANCHOR_BACKING_MISMATCH" {
        error("POSITION_PRINCIPAL_CHANGED", "prepare", "Current backing differs from the proved closed position; it cannot be discarded by reopening.")
    } else { failure })
}

/// First-stage, transport-free decision. Risk evidence and the exact transaction
/// remain explicit later acquisition/decision stages.
#[derive(Clone)]
pub(crate) struct WriteCandidate {
    pub operation: String,
    pub input: Value,
    pub request: request::Request,
    pub token: reads::Token,
    pub chain_id: u64,
    pub pool: String,
    pub owner: Address,
    pub block: u64,
    pub block_hash: String,
    pub proxy_identity: String,
    pub amount: U256,
    pub maximum_fee: U256,
    pub binding: Binding,
    pub original: Option<Binding>,
    pub intent: FinancialIntent,
    pub warnings: Vec<String>,
}

pub(crate) fn evaluate_write(input: &WriteInput, e: &WriteEvidence) -> Result<WriteCandidate> {
    let op = input.operation.as_str();
    let reserve = super::read_lifecycle::evaluate_write_reserve(&e.reserve)?;
    require(
        reserve.token.reference == e.token.reference
            && reserve.token.decimals == e.token.decimals
            && reserve.token.symbol == e.token.symbol,
        "RESERVE_PRECISION_MISMATCH",
        "Reserve token metadata differs from the selected token observation.",
    )?;
    let horizon = e.chain_id == 1 && e.pool == super::math::HORIZON_POOL;
    require(
        if horizon {
            e.pool_revision == U256::from(7)
        } else {
            [U256::from(10), U256::from(11)].contains(&e.pool_revision)
        },
        "AAVE_POOL_REVISION_UNSUPPORTED",
        "Selected pool revision has no reviewed exact risk kernel.",
    )?;
    let pool = address(&e.pool)?;
    let asset_address = address(&e.token.reference)?;
    require(
        e.receipt_identities
            .iter()
            .all(|(receipt_pool, underlying)| {
                *receipt_pool == pool && *underlying == asset_address
            }),
        "AAVE_RECEIPT_IDENTITY_MISMATCH",
        "Receipt token does not bind the selected pool and underlying asset.",
    )?;
    let owner = address(&e.wallet.address)?;
    let all = input.request.amount.as_deref() == Some("all");
    let repay_all = op == "aave.repay" && all;
    let withdraw_all = op == "aave.withdraw" && all;
    let qty = if op == "aave.collateral" {
        U256::ZERO
    } else {
        exact(
            if all {
                input.request.max_amount.as_deref().expect("schema")
            } else {
                input.request.amount.as_deref().expect("schema")
            },
            e.token.decimals,
        )?
    };
    if repay_all || withdraw_all {
        require(
            qty < U256::MAX,
            "ALL_AMOUNT_CAP_INVALID",
            "Aave full repayment and withdrawal require a finite max_amount below uint256 maximum.",
        )?;
    }
    require(
        qty > U256::ZERO || op == "aave.collateral",
        "ZERO_AMOUNT",
        "Movement amount must be positive.",
    )?;
    require(
        reserve.active && !reserve.paused,
        "RESERVE_UNAVAILABLE",
        "Aave reserve must be active and unpaused.",
    )?;
    if let Some(revision) = e.receipt_revision {
        let expected = U256::from(if horizon {
            if matches!(op, "aave.supply" | "aave.withdraw") {
                3
            } else {
                1
            }
        } else {
            5
        });
        require(
            revision == expected,
            if matches!(op, "aave.supply" | "aave.withdraw") {
                "ATOKEN_REVISION_UNSUPPORTED"
            } else {
                "DEBT_REVISION_UNSUPPORTED"
            },
            "Selected write requires reviewed receipt token backing math.",
        )?;
    }
    let supply_anchor = evaluate_context_anchor(&e.supply_anchor)?;
    let debt_anchor = evaluate_context_anchor(&e.debt_anchor)?;
    require(
        supply_anchor.scaled_receipt.as_ref() == Some(&quantity(e.scaled_supply))
            && debt_anchor.scaled_receipt.as_ref() == Some(&e.user.scaled_variable_debt),
        "POSITION_PRINCIPAL_CHANGED",
        "Current scaled principal differs from broker-proved prior anchor.",
    )?;
    let resource = resource(e.chain_id, owner);
    let asset = AssetId {
        network: format!("ethereum:{}", e.chain_id),
        reference: e.token.reference.clone(),
        decimals: e.token.decimals,
    };
    let placeholder = FrozenTransaction {
        rollup_fee: None,
        chain_id: e.chain_id,
        sender: e.wallet.address.clone(),
        to: e.wallet.address.clone(),
        value: Quantity::zero(),
        calldata: "0x".into(),
        nonce: 0,
        gas: 1,
        max_fee_per_gas: Quantity::zero(),
        max_priority_fee_per_gas: Quantity::zero(),
    };
    let old = e.original.as_ref();
    let mut binding = Binding {
        transaction: placeholder,
        resource: resource.clone(),
        token: asset.clone(),
        units: quantity(qty),
        recipient: None,
        spender: Some(e.pool.clone()),
        output: None,
        minimum_output: None,
        deadline: None,
        pool: Some(e.pool.clone()),
        fee_tier: None,
        dex: None,
        tick_spacing: None,
        pool_deployer: None,
        factory: None,
        quoter: None,
        swap_fee_ppm: None,
        max_pool_fee_ppm: None,
        protocol: None,
        v4_pool_key: None,
        route_token_in: None,
        route_token_out: None,
        quote_output: None,
        price_impact_bps: None,
        permit_template: None,
        permit2_allowance_before: None,
        allowance_before: None,
        supplied_before: Some(e.user.supplied.clone()),
        debt_before: Some(e.user.variable_debt.clone()),
        collateral_before: Some(e.user.collateral_enabled),
        enabled: None,
        a_token: Some(reserve.a_token.clone()),
        debt_token: Some(reserve.variable_debt_token.clone()),
        scaled_before: None,
        repay_all,
        all_amount: all,
        aave_emode: Some(quantity(e.mode)),
        supply_anchor: Some(supply_anchor),
        debt_anchor: Some(debt_anchor),
        debt_rounding_maximum: None,
        aave_proxy_identity: None,
        verified_baseline_anchors: old
            .map(|b| b.verified_baseline_anchors.clone())
            .unwrap_or_default(),
        value_bounds: old.and_then(|b| b.value_bounds.clone()),
        boundary_authorization: old.and_then(|b| b.boundary_authorization.clone()),
        financial_plan: old.and_then(|b| b.financial_plan.clone()),
    };
    if old.is_none() {
        let relevant = match op {
            "aave.borrow" | "aave.repay" => binding.debt_anchor.as_ref(),
            "aave.supply" | "aave.withdraw" => binding.supply_anchor.as_ref(),
            _ => None,
        };
        if let Some(anchor) = relevant
            && !input.context.positions.iter().any(|old| {
                old.resource == anchor.resource
                    && old.position == anchor.position
                    && old.position_kind == anchor.position_kind
                    && old.principal.asset == anchor.principal.asset
            })
        {
            binding.verified_baseline_anchors.push(anchor.clone());
        }
    }
    let mut intent = FinancialIntent::default();
    intent.locks.push(resource.clone());
    let supply_position = position(&e.pool, &e.token.reference, "supply");
    let debt_position = position(&e.pool, &e.token.reference, "variable-debt");
    let authority = collateral(&e.pool, &e.token.reference);
    let effect_facts = EffectFacts {
        resource: &resource,
        asset: &asset,
        supply_position: &supply_position,
        debt_position: &debt_position,
        collateral_authority: &authority,
        amount: qty,
    };
    let mut warnings =
        vec!["An uncertain transaction must be reconciled without resubmission.".into()];
    match op {
        "aave.supply" => {
            evaluate_supply(&SupplyEvidence {
                reserve: reserve.clone(),
                amount: qty,
                allowance: e.allowance.expect("supply allowance"),
            })?;
            binding.scaled_before = Some(quantity(e.scaled_supply));
            evaluate_action_effects(&effect_facts, ActionEffects::Supply, &mut intent);
            warnings.push("Supply may enable collateral automatically. Collateral settings and allowances persist until explicitly changed by another approved write.".into());
        }
        "aave.borrow" => {
            evaluate_borrow_reserve(&reserve, qty)?;
            let rounding = super::math::borrow_rounding_limit(e.debt_index.expect("borrow index"))?;
            binding.debt_rounding_maximum = Some(quantity(rounding));
            let risk = match &e.risk {
                WriteRiskEvidence::Borrow(risk) => risk,
                _ => {
                    return Err(GoFinError::validation(
                        "Borrow risk observations are missing.",
                    ));
                }
            };
            let (collateral_base, debt, projected_health) =
                super::risk::evaluate_borrow_risk(&e.token, add(qty, rounding)?, risk)?;
            let account = e.account.as_ref().expect("borrow account");
            evaluate_borrow(&BorrowEvidence {
                reserve: reserve.clone(),
                amount: qty,
                collateral_base,
                debt,
                projected_health,
                previous_debt: account.totalDebtBase,
                available_borrow: account.availableBorrowsBase,
                request: input.request.clone(),
            })?;
            let maximum_debt = if binding
                .debt_anchor
                .as_ref()
                .expect("anchor")
                .principal
                .atoms
                .is_zero()
                && e.user.scaled_variable_debt.is_zero()
            {
                add(qty, rounding)?
            } else {
                qty
            };
            evaluate_action_effects(
                &effect_facts,
                ActionEffects::Borrow { maximum_debt },
                &mut intent,
            );
            warnings.push("The minimum health factor is a fresh preflight constraint; variable interest continues after execution.".into());
        }
        "aave.repay" => {
            evaluate_repay(&RepayEvidence {
                debt: u256(&e.user.variable_debt)?,
                allowance: e.allowance.expect("repay allowance"),
                amount: qty,
                all: repay_all,
            })?;
            evaluate_action_effects(&effect_facts, ActionEffects::Repay, &mut intent);
        }
        "aave.withdraw" => {
            let claim = u256(&e.user.supplied)?;
            let minimum = evaluate_withdraw(&WithdrawEvidence {
                claim,
                liquidity: u256(&reserve.liquidity)?,
                amount: qty,
                all: withdraw_all,
                approved_minimum: old
                    .and_then(|b| b.minimum_output.as_ref())
                    .map(u256)
                    .transpose()?,
            })?;
            binding.minimum_output = withdraw_all.then(|| quantity(minimum));
            if e.user.collateral_enabled {
                let removed = Some(if withdraw_all { claim } else { qty });
                let risk = match &e.risk {
                    WriteRiskEvidence::Removal(risk) => risk,
                    _ => {
                        return Err(GoFinError::validation(
                            "Removal risk observations are missing.",
                        ));
                    }
                };
                EthereumAdapter::evaluate_removal_risk(
                    &e.token,
                    removed,
                    input.request.min_health_factor.as_deref(),
                    risk,
                )?;
            }
            let recipient = address(input.request.recipient.as_deref().expect("schema"))?;
            require(
                recipient != Address::ZERO,
                "ZERO_ADDRESS",
                "Withdraw recipient must be nonzero.",
            )?;
            require(
                recipient == owner,
                "AAVE_MCP_RECIPIENT_UNSUPPORTED",
                "The official Aave MCP withdrawal action targets the signing wallet; choose that wallet as recipient.",
            )?;
            binding.recipient = Some(lower_address(recipient));
            let maximum_position_change = if withdraw_all {
                qty
            } else {
                add(
                    qty,
                    (u256(&reserve.normalized_income)? + pow10(27)? - U256::from(1)) / pow10(27)?
                        + U256::from(2),
                )?
            };
            evaluate_action_effects(
                &effect_facts,
                ActionEffects::Withdraw {
                    recipient: crate::resource(e.chain_id, recipient),
                    minimum_credit: minimum,
                    maximum_position_change,
                },
                &mut intent,
            );
        }
        "aave.collateral" => {
            let enabled = input.request.enabled.expect("schema");
            evaluate_collateral(&CollateralEvidence {
                reserve: reserve.clone(),
                user: e.user.clone(),
                enabled,
            })?;
            if !enabled && e.user.collateral_enabled {
                let risk = match &e.risk {
                    WriteRiskEvidence::Removal(risk) => risk,
                    _ => {
                        return Err(GoFinError::validation(
                            "Removal risk observations are missing.",
                        ));
                    }
                };
                EthereumAdapter::evaluate_removal_risk(
                    &e.token,
                    None,
                    input.request.min_health_factor.as_deref(),
                    risk,
                )?;
            }
            binding.enabled = Some(enabled);
            evaluate_action_effects(
                &effect_facts,
                ActionEffects::Collateral {
                    before: e.user.collateral_enabled,
                    after: enabled,
                },
                &mut intent,
            );
        }
        _ => return Err(GoFinError::validation("Unsupported Aave write operation.")),
    }
    if let Some(balance) = e.wallet_balance {
        require(
            balance >= qty,
            "INSUFFICIENT_BALANCE",
            "Exact reserved principal exceeds spendable holdings.",
        )?;
    }
    let maximum_fee = exact(input.request.max_fee_eth.as_deref().expect("schema"), 18)?;
    require(
        maximum_fee > U256::ZERO,
        "FEE_LIMIT",
        "Explicit max_fee_eth must be positive.",
    )?;
    Ok(WriteCandidate {
        operation: input.operation.clone(),
        input: input.input.clone(),
        request: input.request.clone(),
        token: e.token.clone(),
        chain_id: e.chain_id,
        pool: e.pool.clone(),
        owner,
        block: e.block,
        block_hash: e.block_hash.clone(),
        proxy_identity: e.proxy_identity.clone(),
        amount: qty,
        maximum_fee,
        binding,
        original: e.original.clone(),
        intent,
        warnings,
    })
}

pub(crate) enum WriteRiskEvidence {
    None,
    Borrow(super::risk::BorrowRiskEvidence),
    Removal(super::risk::HealthEvidence),
}
pub(crate) struct WriteFinalEvidence {
    pub transaction: FrozenTransaction,
    pub simulation: gofin_evm::alloy::primitives::Bytes,
    pub live_proxy: String,
    pub prices: Option<Vec<AssetPrice>>,
    pub observed_at: String,
    pub expires_at: String,
    pub native_asset: AssetId,
}

impl EthereumAdapter {
    /// Collect risk, the exact transaction and optional presentation prices;
    /// the candidate and all later economic decisions are pure values.
    pub(crate) async fn collect_aave_final(
        &self,
        candidate: &WriteCandidate,
    ) -> Result<WriteFinalEvidence> {
        use crate::aave_mcp::{
            AaveMcpAction, AaveMcpAmount, AaveMcpIntent, validate_transaction_fields,
        };
        let selected = self.selected_market(&candidate.input)?;
        let adapter = selected.as_ref().unwrap_or(self);
        let params = json!([gofin_evm::block_tag(Some(candidate.block)), false]);
        let before = adapter
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params.clone())
            .await?;
        require(
            gofin_evm::rpc_u64(&before["number"])? == candidate.block
                && before["hash"] == candidate.block_hash,
            "AAVE_READ_CHANGED",
            "Preparation block became noncanonical.",
        )?;
        let after = adapter
            .runtime
            .rpc
            .request("eth_getBlockByNumber", params)
            .await?;
        require(
            gofin_evm::rpc_u64(&after["number"])? == candidate.block
                && after["hash"] == candidate.block_hash,
            "AAVE_READ_CHANGED",
            "Preparation block became noncanonical.",
        )?;
        let action = match candidate.operation.as_str() {
            "aave.supply" => AaveMcpAction::Supply,
            "aave.borrow" => AaveMcpAction::Borrow,
            "aave.repay" => AaveMcpAction::Repay,
            "aave.withdraw" => AaveMcpAction::Withdraw,
            "aave.collateral" => AaveMcpAction::Collateral,
            _ => return Err(GoFinError::validation("Unsupported Aave action.")),
        };
        let amount = if action == AaveMcpAction::Collateral {
            None
        } else if candidate.binding.repay_all
            || (candidate.operation == "aave.withdraw" && candidate.binding.all_amount)
        {
            Some(AaveMcpAmount::Max {
                cap: candidate.amount,
            })
        } else {
            Some(AaveMcpAmount::Exact(candidate.amount))
        };
        let mcp_intent = AaveMcpIntent {
            chain_id: candidate.chain_id,
            pool: address(&candidate.pool)?,
            owner: candidate.owner,
            token: address(&candidate.token.reference)?,
            action,
            amount,
            recipient: candidate
                .binding
                .recipient
                .as_deref()
                .map(address)
                .transpose()?,
            enabled: candidate.binding.enabled,
            decimals: candidate.token.decimals,
        };
        let calldata = if let Some(old) = &candidate.original {
            gofin_evm::rpc_bytes(&json!(old.transaction.calldata))?
        } else {
            adapter
                .aave_mcp
                .prepare(&mcp_intent)
                .await
                .map_err(|e| error("AAVE_MCP_REJECTED", "preview", &e.to_string()))?
                .calldata
        };
        validate_transaction_fields(
            &mcp_intent,
            candidate.chain_id,
            address(&candidate.pool)?,
            candidate.owner,
            U256::ZERO,
            &calldata,
        )
        .map_err(|e| error("AAVE_CALLDATA_INVALID", "preview", &e.to_string()))?;
        let live_proxy = adapter.aave_proxy_identity().await?;
        require(
            live_proxy == candidate.proxy_identity,
            "AAVE_IMPLEMENTATION_CHANGED",
            "Pool implementation changed during preparation.",
        )?;
        if let Some(old) = &candidate.original {
            let identity = old.aave_proxy_identity.as_ref().ok_or_else(|| error("AAVE_PREVIEW_REFRESH_REQUIRED", "revalidation", "This legacy Aave preview did not bind the selected Pool proxy and implementation. Create a fresh preview before execution."))?;
            require(
                identity == &live_proxy,
                "AAVE_IMPLEMENTATION_CHANGED",
                "The selected Pool proxy or implementation changed after preview; create a fresh preview.",
            )?;
        }
        let (transaction, simulation) = if let Some(old) = &candidate.original {
            let tx = &old.transaction;
            require(
                address(&tx.sender)? == candidate.owner
                    && address(&tx.to)? == address(&candidate.pool)?
                    && u256(&tx.value)?.is_zero()
                    && gofin_evm::rpc_bytes(&json!(tx.calldata))? == calldata,
                "BINDING_CHANGED",
                "Current protocol route/calldata differs from approved exact envelope.",
            )?;
            (tx.clone(), adapter.runtime.revalidate(tx).await?)
        } else {
            let prepared = adapter
                .runtime
                .prepare(
                    candidate.owner,
                    address(&candidate.pool)?,
                    U256::ZERO,
                    calldata,
                    candidate.maximum_fee,
                )
                .await?;
            (prepared.transaction, prepared.simulation)
        };
        let native_asset = adapter.native_asset();
        let prices = async {
            let mut assets = vec![candidate.binding.token.clone()];
            if native_asset != candidate.binding.token {
                assets.push(native_asset.clone());
            }
            let amounts = assets
                .into_iter()
                .map(|asset| {
                    Ok(Amount {
                        atoms: quantity(pow10(asset.decimals)?),
                        asset,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok::<Vec<AssetPrice>, GoFinError>(
                adapter
                    .quote_verified_assets_usd_observed(&amounts)
                    .await?
                    .into_iter()
                    .zip(amounts)
                    .map(|((usd, source, observed_at), amount)| AssetPrice {
                        asset: amount.asset,
                        usd_micros_per_unit: usd,
                        unit_atoms: amount.atoms,
                        observed_at,
                        source,
                    })
                    .collect(),
            )
        }
        .await
        .ok();
        let observed_at = adapter.clock.now().to_rfc3339();
        let expires_at = (adapter.clock.now() + chrono::Duration::seconds(300))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(WriteFinalEvidence {
            transaction,
            simulation,
            live_proxy,
            prices,
            observed_at,
            expires_at,
            native_asset,
        })
    }
}

pub(crate) fn finalize_write(
    candidate: &WriteCandidate,
    evidence: &WriteFinalEvidence,
) -> Result<PreparedOperation> {
    if let Some(old) = &candidate.original {
        let identity = old.aave_proxy_identity.as_ref().ok_or_else(|| error("AAVE_PREVIEW_REFRESH_REQUIRED", "revalidation", "This legacy Aave preview did not bind the selected Pool proxy and implementation. Create a fresh preview before execution."))?;
        require(
            identity == &evidence.live_proxy,
            "AAVE_IMPLEMENTATION_CHANGED",
            "The selected Pool proxy or implementation changed after preview; create a fresh preview.",
        )?;
    }
    let mut binding = candidate.binding.clone();
    binding.transaction = evidence.transaction.clone();
    binding.aave_proxy_identity = Some(evidence.live_proxy.clone());
    let mut warnings = candidate.warnings.clone();
    warnings.push("The official Aave MCP supplies candidate unsigned transaction data; GoFin validates the selected Pool call locally before approval and signing.".into());
    super::finalization::finalize_aave(super::finalization::AaveFinalFacts {
        operation: &candidate.operation,
        input: &candidate.input,
        request: &candidate.request,
        token: &candidate.token,
        chain_id: candidate.chain_id,
        pool: &candidate.pool,
        amount: candidate.amount,
        native_asset: &evidence.native_asset,
        binding,
        intent: candidate.intent.clone(),
        warnings,
        simulation: &evidence.simulation,
        original: candidate.original.as_ref(),
        prices: evidence.prices.clone(),
        observed_at: evidence.observed_at.clone(),
        expires_at: evidence.expires_at.clone(),
    })
}

// Marker-bound wrappers make phase values for distinct actions non-interchangeable.
// The shared core contains common facts; action-specific optional observations
// are evaluated only by their matching operation.
use std::marker::PhantomData;
pub(crate) struct Supply;
pub(crate) struct Borrow;
pub(crate) struct Repay;
pub(crate) struct Withdraw;
pub(crate) struct Collateral;
pub(crate) trait WriteMarker {
    const OPERATION: &'static str;
}
impl WriteMarker for Supply {
    const OPERATION: &'static str = "aave.supply";
}
impl WriteMarker for Borrow {
    const OPERATION: &'static str = "aave.borrow";
}
impl WriteMarker for Repay {
    const OPERATION: &'static str = "aave.repay";
}
impl WriteMarker for Withdraw {
    const OPERATION: &'static str = "aave.withdraw";
}
impl WriteMarker for Collateral {
    const OPERATION: &'static str = "aave.collateral";
}
macro_rules! phase {
    ($name:ident, $inner:ty) => {
        pub(crate) struct $name<Op> {
            inner: $inner,
            marker: PhantomData<fn() -> Op>,
        }
        impl<Op> $name<Op> {
            fn new(inner: $inner) -> Self {
                Self {
                    inner,
                    marker: PhantomData,
                }
            }
        }
    };
}
phase!(TypedInput, WriteInput);
phase!(TypedEvidence, WriteEvidence);
phase!(TypedCandidate, WriteCandidate);
phase!(TypedFinalEvidence, WriteFinalEvidence);
phase!(TypedFinalCandidate, PreparedOperation);
phase!(TypedFreshEvidence, WriteEvidence);
phase!(TypedValidated, WriteCandidate);
phase!(TypedConfirmation, WriteFinalEvidence);
phase!(TypedFinalValidated, PreparedOperation);
phase!(TypedExecutionRef, Submission);
phase!(
    TypedObservation,
    crate::proof::aave_execution_observation::LifecycleObservation
);
impl<Op: WriteMarker> TypedInput<Op> {
    fn checked(input: WriteInput) -> Result<Self> {
        require(
            input.operation == Op::OPERATION,
            "BINDING_CHANGED",
            "Aave lifecycle action differs from its typed input.",
        )?;
        Ok(Self::new(input))
    }
}
impl EthereumAdapter {
    pub(crate) fn aave_write_lifecycle<'a>(
        &'a self,
        operation: &str,
        input: &Value,
        context: Option<&PreparationContext>,
    ) -> Result<Option<Box<dyn gofin_domain::operation_lifecycle::LifecycleSession + 'a>>> {
        use gofin_domain::operation_lifecycle::TypedSession;
        if !matches!(
            operation,
            "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral"
        ) {
            return Ok(None);
        }
        self.require_operation(operation)?;
        let input = WriteInput {
            operation: operation.into(),
            input: input.clone(),
            request: request::parse(operation, input)?,
            context: context.cloned().unwrap_or_default(),
        };
        let session: Box<dyn gofin_domain::operation_lifecycle::LifecycleSession + '_> =
            match operation {
                "aave.supply" => Box::new(TypedSession::<_, Supply>::new(
                    self,
                    TypedInput::<Supply>::checked(input)?,
                )),
                "aave.borrow" => Box::new(TypedSession::<_, Borrow>::new(
                    self,
                    TypedInput::<Borrow>::checked(input)?,
                )),
                "aave.repay" => Box::new(TypedSession::<_, Repay>::new(
                    self,
                    TypedInput::<Repay>::checked(input)?,
                )),
                "aave.withdraw" => Box::new(TypedSession::<_, Withdraw>::new(
                    self,
                    TypedInput::<Withdraw>::checked(input)?,
                )),
                "aave.collateral" => Box::new(TypedSession::<_, Collateral>::new(
                    self,
                    TypedInput::<Collateral>::checked(input)?,
                )),
                _ => unreachable!(),
            };
        Ok(Some(session))
    }
}
macro_rules! write_lifecycle {
    ($marker:ty) => {
        #[async_trait]
        impl gofin_domain::operation_lifecycle::OperationLifecycle<$marker> for EthereumAdapter {
            type Input = TypedInput<$marker>;
            type Evidence = TypedEvidence<$marker>;
            type Candidate = TypedCandidate<$marker>;
            type FinalEvidence = TypedFinalEvidence<$marker>;
            type FinalCandidate = TypedFinalCandidate<$marker>;
            type FreshEvidence = TypedFreshEvidence<$marker>;
            type Validated = TypedValidated<$marker>;
            type Confirmation = TypedConfirmation<$marker>;
            type FinalValidated = TypedFinalValidated<$marker>;
            type ExecutionRef = TypedExecutionRef<$marker>;
            type Observation = TypedObservation<$marker>;
            type Output = ();
            async fn collect(&self, input: &Self::Input) -> Result<Self::Evidence> {
                Ok(TypedEvidence::new(
                    self.collect_aave_write(&input.inner, None).await?,
                ))
            }
            fn evaluate(input: &Self::Input, evidence: &Self::Evidence) -> Result<Self::Candidate> {
                Ok(TypedCandidate::new(evaluate_write(
                    &input.inner,
                    &evidence.inner,
                )?))
            }
            async fn collect_final(
                &self,
                _: &Self::Input,
                candidate: &Self::Candidate,
            ) -> Result<Self::FinalEvidence> {
                Ok(TypedFinalEvidence::new(
                    self.collect_aave_final(&candidate.inner).await?,
                ))
            }
            fn finalize_candidate(
                _: &Self::Input,
                candidate: &Self::Candidate,
                evidence: &Self::FinalEvidence,
            ) -> Result<Self::FinalCandidate> {
                Ok(TypedFinalCandidate::new(finalize_write(
                    &candidate.inner,
                    &evidence.inner,
                )?))
            }
            fn candidate_result(
                candidate: &Self::FinalCandidate,
            ) -> Result<gofin_domain::operation_lifecycle::CandidateResult<()>> {
                Ok(gofin_domain::operation_lifecycle::CandidateResult::Write(
                    Box::new(candidate.inner.clone()),
                ))
            }
            fn output_value(_: ()) -> Result<Value> {
                Err(GoFinError::validation("A write has no direct read output."))
            }
            async fn refresh(
                &self,
                input: &Self::Input,
                prepared: &PreparedOperation,
            ) -> Result<Self::FreshEvidence> {
                require(
                    prepared.operation == input.inner.operation
                        && prepared.input == input.inner.input,
                    "BINDING_CHANGED",
                    "Approved operation differs from this lifecycle input.",
                )?;
                require(
                    chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
                        .is_ok_and(|expires| expires > self.clock.now()),
                    "PREVIEW_EXPIRED",
                    "Original preview has expired.",
                )?;
                let original: Binding =
                    serde_json::from_value(prepared.binding.clone()).map_err(|_| {
                        error(
                            "BINDING_INVALID",
                            "submission",
                            "Original Ethereum binding is malformed.",
                        )
                    })?;
                let mut refreshed = input.inner.clone();
                refreshed.context = PreparationContext {
                    logical_operation_id: None,
                    positions: [original.supply_anchor.clone(), original.debt_anchor.clone()]
                        .into_iter()
                        .flatten()
                        .collect(),
                };
                Ok(TypedFreshEvidence::new(
                    self.collect_aave_write(&refreshed, Some(&original)).await?,
                ))
            }
            fn revalidate(
                input: &Self::Input,
                _: &PreparedOperation,
                evidence: &Self::FreshEvidence,
            ) -> Result<Self::Validated> {
                Ok(TypedValidated::new(evaluate_write(
                    &input.inner,
                    &evidence.inner,
                )?))
            }
            async fn confirm(
                &self,
                _: &Self::Input,
                _: &PreparedOperation,
                validated: &Self::Validated,
            ) -> Result<Self::Confirmation> {
                Ok(TypedConfirmation::new(
                    self.collect_aave_final(&validated.inner).await?,
                ))
            }
            fn finalize_validation(
                _: &Self::Input,
                prepared: &PreparedOperation,
                validated: &Self::Validated,
                confirmation: &Self::Confirmation,
            ) -> Result<Self::FinalValidated> {
                let mut fresh = finalize_write(&validated.inner, &confirmation.inner)?;
                fresh.action_deadline.clone_from(&prepared.action_deadline);
                gofin_domain::product_names::preserve_approved_spelling(
                    &prepared.binding,
                    &mut fresh.binding,
                )?;
                let observed =
                    chrono::DateTime::parse_from_rfc3339(&confirmation.inner.observed_at)
                        .map_err(|_| GoFinError::validation("Observation time is invalid."))?
                        .with_timezone(&chrono::Utc);
                carry_attempt_valuation(prepared, &mut fresh, observed)?;
                Ok(TypedFinalValidated::new(fresh))
            }
            fn validated_prepared(validated: &Self::FinalValidated) -> Result<PreparedOperation> {
                Ok(validated.inner.clone())
            }
            async fn execute(
                &self,
                validated: &Self::FinalValidated,
                handoff: &RevalidatedOperation,
                journal: Arc<dyn SubmissionJournal>,
            ) -> Result<Self::ExecutionRef> {
                require(
                    handoff.prepared() == &validated.inner,
                    "BINDING_CHANGED",
                    "Execution handoff differs from validated operation.",
                )?;
                Ok(TypedExecutionRef::new(
                    self.submit_revalidated(handoff, journal).await?,
                ))
            }
            fn submission(reference: Self::ExecutionRef) -> Result<Submission> {
                Ok(reference.inner)
            }
            async fn observe(
                &self,
                input: &Self::Input,
                record: &OperationRecord,
            ) -> Result<Self::Observation> {
                require(
                    record.operation == <$marker as WriteMarker>::OPERATION
                        && record.input == input.inner.input,
                    "BINDING_CHANGED",
                    "Observed action differs from its lifecycle type.",
                )?;
                Ok(TypedObservation::new(
                    self.collect_aave_lifecycle_observation(record).await?,
                ))
            }
            fn evaluate_observation(
                input: &Self::Input,
                record: &OperationRecord,
                observation: &Self::Observation,
            ) -> Result<Reconciliation> {
                require(
                    record.operation == <$marker as WriteMarker>::OPERATION
                        && record.input == input.inner.input,
                    "BINDING_CHANGED",
                    "Observed input differs from its typed lifecycle input.",
                )?;
                EthereumAdapter::evaluate_aave_lifecycle_observation(record, &observation.inner)
            }
        }
    };
}
write_lifecycle!(Supply);
write_lifecycle!(Borrow);
write_lifecycle!(Repay);
write_lifecycle!(Withdraw);
write_lifecycle!(Collateral);

#[cfg(test)]
mod typed_action_tests {
    use super::*;
    #[test]
    fn marker_rejects_another_actions_input() {
        let input = json!({"wallet":"synthetic", "network":"ethereum", "chain_id":1, "token":"0x0000000000000000000000000000000000000065", "request_id":"typed-action", "amount":"1", "max_fee_eth":"0.001", "min_health_factor":"1.5"});
        let common = WriteInput {
            operation: "aave.borrow".into(),
            request: request::parse("aave.borrow", &input).unwrap(),
            input,
            context: PreparationContext::default(),
        };
        assert_eq!(
            TypedInput::<Supply>::checked(common).err().unwrap().code,
            "BINDING_CHANGED"
        );
    }
}
