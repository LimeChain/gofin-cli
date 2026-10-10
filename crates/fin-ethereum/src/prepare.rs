use super::*;
use abi::*;
use fin_evm::{
    alloy::{
        primitives::{Bytes, aliases::U24},
        sol_types::SolCall,
    },
    quantity, rpc_bytes, u256,
};

impl EthereumAdapter {
    pub(crate) async fn prepare_operation(
        &self,
        operation: &str,
        input: &Value,
        original: Option<&Binding>,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        require(
            !request::is_read(operation),
            "WRITE_OPERATION_REQUIRED",
            "Read operations have no financial execution preview.",
        )?;
        self.require_operation(operation)?;
        let r = request::parse(operation, input)?;
        let w = self.resolve(&r, true)?;
        let owner = address(&w.address)?;
        self.runtime.assert_chain().await?;
        let token_ref = if matches!(operation, "wallet.wrap" | "wallet.unwrap") {
            self.weth()?
        } else if operation == "uniswap.swap" {
            r.token_in.as_deref().expect("schema")
        } else {
            r.token.as_deref().expect("schema")
        };
        let token = self.token(token_ref, None).await?;
        let all_amount = r.amount.as_deref() == Some("all");
        let repay_all = operation == "aave.repay" && all_amount;
        let withdraw_all = operation == "aave.withdraw" && all_amount;
        let mut qty = if operation == "aave.collateral" {
            U256::ZERO
        } else {
            exact(
                if all_amount {
                    r.max_amount.as_deref().expect("schema")
                } else {
                    r.amount.as_deref().expect("schema")
                },
                token.decimals,
            )?
        };
        if all_amount && matches!(operation, "wallet.unwrap" | "uniswap.swap") {
            let observed = self.balance_of(&token, owner, None).await?;
            require(
                observed > U256::ZERO && observed <= qty,
                "SWEEP_MAXIMUM",
                "Complete observed token balance must fit finite max_amount.",
            )?;
            qty = observed;
        }
        require(
            qty > U256::ZERO || matches!(operation, "wallet.approve" | "aave.collateral"),
            "ZERO_AMOUNT",
            "Movement amount must be positive.",
        )?;
        let principal_asset = if operation == "wallet.wrap" {
            self.native_asset()
        } else {
            token.asset(self)
        };
        let resource = resource(self.config.chain_id, owner);
        let mut intent = FinancialIntent::default();
        intent.locks.push(resource.clone());
        let placeholder = FrozenTransaction {
            rollup_fee: None,
            chain_id: self.config.chain_id,
            sender: w.address.clone(),
            to: w.address.clone(),
            value: Quantity::zero(),
            calldata: "0x".into(),
            nonce: 0,
            gas: 1,
            max_fee_per_gas: Quantity::zero(),
            max_priority_fee_per_gas: Quantity::zero(),
        };
        let mut b = Binding {
            transaction: placeholder,
            resource: resource.clone(),
            token: principal_asset.clone(),
            units: quantity(qty),
            recipient: None,
            spender: None,
            output: None,
            minimum_output: None,
            deadline: None,
            pool: None,
            fee_tier: None,
            allowance_before: None,
            supplied_before: None,
            debt_before: None,
            collateral_before: None,
            enabled: None,
            a_token: None,
            debt_token: None,
            scaled_before: None,
            repay_all,
            all_amount,
            supply_anchor: None,
            debt_anchor: None,
            debt_rounding_maximum: None,
            value_bounds: original.and_then(|binding| binding.value_bounds.clone()),
        };
        let mut warnings=vec!["Every financial write requires a fresh human authorization. An uncertain hash is reconciled without resubmission.".into()];
        let mut to = address(&token.reference).unwrap_or(owner);
        let mut value = U256::ZERO;
        let calldata: Bytes = match operation {
            "wallet.wrap" => {
                require(
                    token.decimals == 18,
                    "WETH_PRECISION_INVALID",
                    "Configured WETH must have 18 decimals.",
                )?;
                value = qty;
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                let output = token.asset(self);
                b.output = Some(output.clone());
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output,
                    minimum_output: quantity(qty),
                });
                Weth::depositCall {}.abi_encode().into()
            }
            "wallet.unwrap" => {
                require(
                    token.decimals == 18,
                    "WETH_PRECISION_INVALID",
                    "Configured WETH must have 18 decimals.",
                )?;
                b.output = Some(self.native_asset());
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output: self.native_asset(),
                    minimum_output: quantity(qty),
                });
                Weth::withdrawCall { amount: qty }.abi_encode().into()
            }
            "wallet.transfer" => {
                let recipient = address(r.recipient.as_deref().expect("schema"))?;
                require(
                    recipient != Address::ZERO && recipient != owner,
                    "RECIPIENT_INVALID",
                    "Recipient must be a different explicit nonzero address.",
                )?;
                b.recipient = Some(lower_address(recipient));
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                let target = super::resource(self.config.chain_id, recipient);
                intent.locks.push(target.clone());
                intent.expected.push(EffectConstraint::Transfer {
                    from: resource.clone(),
                    to: target,
                    debit_asset: principal_asset.clone(),
                    credit_asset: principal_asset.clone(),
                    maximum_debit: quantity(qty),
                    minimum_credit: quantity(qty),
                });
                if token.reference == "native" {
                    let code = rpc_bytes(
                        &self
                            .runtime
                            .rpc
                            .request("eth_getCode", json!([lower_address(recipient), "latest"]))
                            .await?,
                    )?;
                    require(
                        code.is_empty(),
                        "NATIVE_CONTRACT_RECIPIENT_UNSUPPORTED",
                        "Native transfers support explicitly verified externally-owned recipients in this profile.",
                    )?;
                    to = recipient;
                    value = qty;
                    Bytes::new()
                } else {
                    Erc20::transferCall {
                        to: recipient,
                        amount: qty,
                    }
                    .abi_encode()
                    .into()
                }
            }
            "wallet.approve" => {
                require(
                    token.reference != "native" && qty < U256::MAX,
                    "UNBOUNDED_ALLOWANCE",
                    "Use an explicit finite ERC-20 allowance; maximum uint256 approval is unsupported.",
                )?;
                let spender = address(r.spender.as_deref().expect("schema"))?;
                require(
                    spender != Address::ZERO,
                    "ZERO_ADDRESS",
                    "Spender must be nonzero.",
                )?;
                self.runtime.code(spender).await?;
                let before = self.allowance(&token, owner, spender, None).await?;
                b.spender = Some(lower_address(spender));
                b.allowance_before = Some(quantity(before));
                intent.expected.push(EffectConstraint::Authority {
                    resource: resource.clone(),
                    authority: authority(&token.reference, b.spender.as_deref().expect("spender")),
                    before: before.to_string(),
                    after: qty.to_string(),
                });
                warnings.push(
                    "Allowance persists until another approved write restores or revokes it."
                        .into(),
                );
                Erc20::approveCall {
                    spender,
                    amount: qty,
                }
                .abi_encode()
                .into()
            }
            "uniswap.swap" => {
                let quote = self
                    .quote(
                        &token.reference,
                        r.token_out.as_deref().expect("schema"),
                        &quantity(qty).to_decimal(token.decimals),
                        original.and_then(|b| b.fee_tier).or(r.fee_tier),
                        None,
                    )
                    .await?;
                let recipient = address(r.recipient.as_deref().expect("schema"))?;
                require(
                    recipient != Address::ZERO,
                    "ZERO_ADDRESS",
                    "Swap recipient must be nonzero.",
                )?;
                // Cross-custody swaps require two independent effects and are deliberately outside this beta profile.
                require(
                    recipient == owner,
                    "SWAP_RECIPIENT_UNSUPPORTED",
                    "This bounded swap profile returns output to the signing wallet.",
                )?;
                let minimum = exact(
                    r.min_output.as_deref().expect("schema"),
                    quote.token_out.decimals,
                )?;
                let quote_output = u256(&quote.output)?;
                let floor = mul(
                    quote_output,
                    U256::from(10000 - r.slippage_bps.expect("schema")),
                )? / U256::from(10000);
                require(
                    minimum > U256::ZERO && minimum >= floor && minimum <= quote_output,
                    "SLIPPAGE_LIMIT",
                    "Minimum output must respect the quoted slippage floor and remain achievable.",
                )?;
                let deadline = r.deadline.expect("schema");
                let current = chrono::Utc::now().timestamp() as u64;
                require(
                    deadline > current && deadline <= current + 600,
                    "DEADLINE_INVALID",
                    "Swap deadline must be in the next ten minutes.",
                )?;
                let router = address(&self.uniswap()?.router)?;
                self.runtime.code(router).await?;
                require(
                    {
                        let allowance = self.allowance(&token, owner, router, None).await?;
                        allowance >= qty && allowance < U256::MAX
                    },
                    "ALLOWANCE_INSUFFICIENT",
                    "Exact input must be covered by a finite router allowance.",
                )?;
                to = router;
                b.spender = Some(self.uniswap()?.router.clone());
                b.recipient = Some(lower_address(recipient));
                b.output = Some(quote.token_out.asset(self));
                b.minimum_output = Some(quantity(minimum));
                b.deadline = Some(deadline);
                b.pool = Some(quote.pool);
                b.fee_tier = Some(quote.fee_tier);
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal_asset, qty),
                });
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal_asset.clone(),
                    maximum_input: quantity(qty),
                    output: b.output.clone().expect("output"),
                    minimum_output: quantity(minimum),
                });
                Router::exactInputSingleCall {
                    params: Router::SwapParams {
                        tokenIn: address(&token.reference)?,
                        tokenOut: address(&quote.token_out.reference)?,
                        fee: U24::from(quote.fee_tier),
                        recipient,
                        deadline: U256::from(deadline),
                        amountIn: qty,
                        amountOutMinimum: minimum,
                        sqrtPriceLimitX96: Default::default(),
                    },
                }
                .abi_encode()
                .into()
            }
            _ => {
                let reserve = self.reserve(&token.reference, None).await?;
                require(
                    reserve.active && !reserve.paused,
                    "RESERVE_UNAVAILABLE",
                    "Aave reserve must be active and unpaused.",
                )?;
                let user = self.user_reserve(&token, owner, None).await?;
                let pool = address(&self.aave()?.pool)?;
                to = pool;
                b.pool = Some(self.aave()?.pool.clone());
                b.spender = Some(self.aave()?.pool.clone());
                b.a_token = Some(reserve.a_token.clone());
                b.debt_token = Some(reserve.variable_debt_token.clone());
                b.supplied_before = Some(user.supplied.clone());
                b.debt_before = Some(user.variable_debt.clone());
                b.collateral_before = Some(user.collateral_enabled);
                let asset = address(&token.reference)?;
                if matches!(operation, "aave.supply" | "aave.withdraw") {
                    require(
                        self.runtime
                            .call(
                                address(&reserve.a_token)?,
                                &AToken::ATOKEN_REVISIONCall {},
                                None,
                            )
                            .await?
                            == U256::from(5),
                        "ATOKEN_REVISION_UNSUPPORTED",
                        "Selected supply writes require reviewed aToken revision 5 backing math.",
                    )?;
                }
                if matches!(operation, "aave.borrow" | "aave.repay") {
                    require(
                        self.runtime
                            .call(
                                address(&reserve.variable_debt_token)?,
                                &AToken::DEBT_TOKEN_REVISIONCall {},
                                None,
                            )
                            .await?
                            == U256::from(5),
                        "DEBT_REVISION_UNSUPPORTED",
                        "Selected debt writes require reviewed variable debt token revision 5 backing math.",
                    )?;
                }
                b.supply_anchor = Some(
                    self.context_anchor(
                        context,
                        &token,
                        owner,
                        address(&reserve.a_token)?,
                        PositionKind::Supply,
                    )
                    .await?,
                );
                b.debt_anchor = Some(
                    self.context_anchor(
                        context,
                        &token,
                        owner,
                        address(&reserve.variable_debt_token)?,
                        PositionKind::VariableDebt,
                    )
                    .await?,
                );
                require(
                    b.supply_anchor
                        .as_ref()
                        .expect("anchor")
                        .scaled_receipt
                        .as_ref()
                        == Some(&quantity(
                            self.runtime
                                .call(
                                    address(&reserve.a_token)?,
                                    &AToken::scaledBalanceOfCall { user: owner },
                                    None,
                                )
                                .await?,
                        ))
                        && b.debt_anchor
                            .as_ref()
                            .expect("anchor")
                            .scaled_receipt
                            .as_ref()
                            == Some(&user.scaled_variable_debt),
                    "POSITION_PRINCIPAL_CHANGED",
                    "Current scaled principal differs from broker-proved prior anchor.",
                )?;
                let supply_position = position(&self.aave()?.pool, &token.reference, "supply");
                let debt_position = position(&self.aave()?.pool, &token.reference, "variable-debt");
                match operation {
                    "aave.supply" => {
                        require(
                            !reserve.frozen,
                            "RESERVE_FROZEN",
                            "Frozen reserves cannot accept supply.",
                        )?;
                        let cap = mul(u256(&reserve.supply_cap)?, pow10(token.decimals)?)?;
                        require(
                            cap.is_zero() || add(u256(&reserve.total_supply)?, qty)? <= cap,
                            "SUPPLY_CAP",
                            "Supply exceeds the observed reserve cap.",
                        )?;
                        require(
                            {
                                let allowance = self.allowance(&token, owner, pool, None).await?;
                                allowance >= qty && allowance < U256::MAX
                            },
                            "ALLOWANCE_INSUFFICIENT",
                            "Finite Pool allowance must cover exact supply.",
                        )?;
                        let a_token = address(&reserve.a_token)?;
                        require(
                            self.runtime
                                .call(a_token, &AToken::ATOKEN_REVISIONCall {}, None)
                                .await?
                                == U256::from(5),
                            "ATOKEN_REVISION_UNSUPPORTED",
                            "Exact supply backing proof supports reviewed aToken revision 5 only.",
                        )?;
                        b.scaled_before = Some(quantity(
                            self.runtime
                                .call(a_token, &AToken::scaledBalanceOfCall { user: owner }, None)
                                .await?,
                        ));
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal_asset, qty),
                        });
                        intent.expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: principal_asset.clone(),
                            maximum_debit: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Position {
                            resource: resource.clone(),
                            position: supply_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Increase,
                            maximum_change: quantity(qty),
                        });
                        warnings.push("Supply may enable collateral automatically. Collateral settings and allowances persist until explicitly changed by another approved write.".into());
                        Pool::supplyCall {
                            asset,
                            amount: qty,
                            onBehalfOf: owner,
                            referralCode: 0,
                        }
                        .abi_encode()
                        .into()
                    }
                    "aave.borrow" => {
                        require(
                            !reserve.frozen && reserve.borrowing,
                            "BORROW_UNAVAILABLE",
                            "Reserve must permit variable borrowing and be unfrozen.",
                        )?;
                        let cap = mul(u256(&reserve.borrow_cap)?, pow10(token.decimals)?)?;
                        require(
                            cap.is_zero() || add(u256(&reserve.total_debt)?, qty)? <= cap,
                            "BORROW_CAP",
                            "Borrow exceeds the observed reserve cap.",
                        )?;
                        require(
                            u256(&reserve.liquidity)? >= qty,
                            "LIQUIDITY_INSUFFICIENT",
                            "Reserve lacks exact underlying liquidity.",
                        )?;
                        let account = self
                            .runtime
                            .call(pool, &Pool::getUserAccountDataCall { user: owner }, None)
                            .await?;
                        let debt_index = self
                            .runtime
                            .call(
                                pool,
                                &Pool::getReserveNormalizedVariableDebtCall { asset },
                                None,
                            )
                            .await?;
                        require(
                            debt_index >= pow10(27)? && debt_index < (U256::from(1) << 128),
                            "DEBT_INDEX_INVALID",
                            "Borrow normalized index exceeds reviewed finite bounds.",
                        )?;
                        let rounding_limit = add(
                            add(debt_index, pow10(27)? - U256::from(1))? / pow10(27)?,
                            U256::from(2),
                        )?;
                        b.debt_rounding_maximum = Some(quantity(rounding_limit));
                        let (usd, _) = self
                            .quote_protocol_usd(
                                &token.reference,
                                quantity(add(qty, rounding_limit)?),
                            )
                            .await?;
                        let added = mul(u256(&usd)?, U256::from(100))?;
                        let debt = add(account.totalDebtBase, added)?;
                        let projected = if debt.is_zero() {
                            U256::MAX
                        } else {
                            mul(
                                mul(
                                    account.totalCollateralBase,
                                    account.currentLiquidationThreshold,
                                )?,
                                pow10(18)?,
                            )? / mul(U256::from(10000), debt)?
                        };
                        let minimum = exact(r.min_health_factor.as_deref().expect("schema"), 18)?;
                        require(
                            minimum > pow10(18)?
                                && projected >= minimum
                                && added <= account.availableBorrowsBase,
                            "HEALTH_FACTOR_LIMIT",
                            "Borrow exceeds collateral capacity or explicit minimum health factor.",
                        )?;
                        intent.expected.push(EffectConstraint::Debt {
                            resource: resource.clone(),
                            debt: debt_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Increase,
                            // Opening debt includes its already-priced finite
                            // rounding exposure. With an existing verified debt
                            // baseline, principal and accrual stay separate.
                            maximum_change: quantity(
                                if b.debt_anchor
                                    .as_ref()
                                    .expect("anchor")
                                    .principal
                                    .atoms
                                    .is_zero()
                                    && user.scaled_variable_debt.is_zero()
                                {
                                    add(qty, rounding_limit)?
                                } else {
                                    qty
                                },
                            ),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: resource.clone(),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(qty),
                        });
                        warnings.push("The minimum health factor is a fresh preflight constraint; variable interest continues after execution.".into());
                        Pool::borrowCall {
                            asset,
                            amount: qty,
                            interestRateMode: U256::from(2),
                            referralCode: 0,
                            onBehalfOf: owner,
                        }
                        .abi_encode()
                        .into()
                    }
                    "aave.repay" => {
                        let debt = u256(&user.variable_debt)?;
                        require(
                            debt > U256::ZERO,
                            "NO_VARIABLE_DEBT",
                            "No variable debt exists for this reserve.",
                        )?;
                        let allowance = self.allowance(&token, owner, pool, None).await?;
                        require(
                            allowance < U256::MAX
                                && allowance >= if repay_all { debt } else { qty },
                            "ALLOWANCE_INSUFFICIENT",
                            "Repayment requires finite sufficient Pool allowance.",
                        )?;
                        if repay_all {
                            require(
                                debt <= qty && allowance <= qty,
                                "REPAY_MAXIMUM",
                                "Repay all debt/allowance exceeds explicit max_amount.",
                            )?;
                        }
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal_asset, qty),
                        });
                        intent.expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: principal_asset.clone(),
                            maximum_debit: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Debt {
                            resource: resource.clone(),
                            debt: debt_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Decrease,
                            maximum_change: quantity(qty),
                        });
                        Pool::repayCall {
                            asset,
                            amount: if repay_all { U256::MAX } else { qty },
                            interestRateMode: U256::from(2),
                            onBehalfOf: owner,
                        }
                        .abi_encode()
                        .into()
                    }
                    "aave.withdraw" => {
                        let claim = u256(&user.supplied)?;
                        require(
                            if withdraw_all {
                                claim > U256::ZERO
                                    && claim <= qty
                                    && claim <= u256(&reserve.liquidity)?
                            } else {
                                qty <= claim && qty <= u256(&reserve.liquidity)?
                            },
                            "WITHDRAW_LIMIT",
                            "Withdrawal must fit its exact claim, finite cap and available liquidity.",
                        )?;
                        let minimum = if withdraw_all {
                            original
                                .and_then(|old| old.minimum_output.as_ref())
                                .map(u256)
                                .transpose()?
                                .unwrap_or(claim)
                        } else {
                            qty
                        };
                        require(
                            claim >= minimum,
                            "WITHDRAW_MINIMUM",
                            "Current claim decreased below the frozen return minimum.",
                        )?;
                        b.minimum_output = if withdraw_all {
                            Some(quantity(minimum))
                        } else {
                            None
                        };
                        let recipient = address(r.recipient.as_deref().expect("schema"))?;
                        require(
                            recipient != Address::ZERO,
                            "ZERO_ADDRESS",
                            "Withdraw recipient must be nonzero.",
                        )?;
                        b.recipient = Some(lower_address(recipient));
                        intent.expected.push(EffectConstraint::Position {
                            resource: resource.clone(),
                            position: supply_position.clone(),
                            asset: principal_asset.clone(),
                            direction: Direction::Decrease,
                            maximum_change: quantity(qty),
                        });
                        intent.expected.push(EffectConstraint::Credit {
                            to: super::resource(self.config.chain_id, recipient),
                            asset: principal_asset.clone(),
                            minimum_credit: quantity(minimum),
                        });
                        Pool::withdrawCall {
                            asset,
                            amount: if withdraw_all { U256::MAX } else { qty },
                            to: recipient,
                        }
                        .abi_encode()
                        .into()
                    }
                    "aave.collateral" => {
                        require(
                            !user.supplied.is_zero(),
                            "NO_SUPPLIED_POSITION",
                            "Collateral toggles require an existing supplied position.",
                        )?;
                        let enabled = r.enabled.expect("schema");
                        require(
                            !enabled || (reserve.collateral_eligible && !reserve.ltv.is_zero()),
                            "COLLATERAL_UNAVAILABLE",
                            "Reserve is ineligible for collateral use.",
                        )?;
                        b.enabled = Some(enabled);
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: collateral(&self.aave()?.pool, &token.reference),
                            before: user.collateral_enabled.to_string(),
                            after: enabled.to_string(),
                        });
                        Pool::setUserUseReserveAsCollateralCall {
                            asset,
                            useAsCollateral: enabled,
                        }
                        .abi_encode()
                        .into()
                    }
                    _ => return Err(FinError::validation("Unsupported write.")),
                }
            }
        };
        if matches!(
            operation,
            "wallet.unwrap" | "wallet.transfer" | "uniswap.swap" | "aave.supply" | "aave.repay"
        ) {
            require(
                self.balance_of(&token, owner, None).await? >= qty,
                "INSUFFICIENT_BALANCE",
                "Exact reserved principal exceeds spendable holdings.",
            )?;
        }
        let maximum_fee = exact(r.max_fee_eth.as_deref().expect("schema"), 18)?;
        require(
            maximum_fee > U256::ZERO,
            "FEE_LIMIT",
            "Explicit max_fee_eth must be positive.",
        )?;
        let simulation = if let Some(old) = original {
            let tx = &old.transaction;
            require(
                address(&tx.sender)? == owner
                    && address(&tx.to)? == to
                    && u256(&tx.value)? == value
                    && fin_evm::rpc_bytes(&json!(tx.calldata))? == calldata,
                "BINDING_CHANGED",
                "Current protocol route/calldata differs from approved exact envelope.",
            )?;
            b.transaction = tx.clone();
            self.runtime.revalidate(tx).await?
        } else {
            let prepared = self
                .runtime
                .prepare(owner, to, value, calldata, maximum_fee)
                .await?;
            b.transaction = prepared.transaction;
            prepared.simulation
        };
        if matches!(operation, "wallet.approve" | "wallet.transfer") && token.reference != "native"
        {
            let accepted = if operation == "wallet.approve" {
                Erc20::approveCall::abi_decode_returns(&simulation)
            } else {
                Erc20::transferCall::abi_decode_returns(&simulation)
            }
            .map_err(|_| {
                error(
                    "ERC20_RETURN_INVALID",
                    "preview",
                    "Standard ERC-20 write must return a decoded boolean.",
                )
            })?;
            require(
                accepted,
                "ERC20_RETURN_FALSE",
                "Token simulation returned false.",
            )?;
        }
        if operation == "aave.repay" {
            let paid = Pool::repayCall::abi_decode_returns(&simulation).map_err(|_| {
                error(
                    "SIMULATION_RETURN_INVALID",
                    "preview",
                    "Repay simulation lacks exact underlying amount.",
                )
            })?;
            require(
                paid > U256::ZERO && paid <= qty,
                "REPAY_SIMULATION_LIMIT",
                "Simulated repayment exceeds the approved finite debit or clears no debt.",
            )?;
        }
        if operation == "aave.withdraw" {
            let received = Pool::withdrawCall::abi_decode_returns(&simulation).map_err(|_| {
                error(
                    "SIMULATION_RETURN_INVALID",
                    "preview",
                    "Withdraw simulation lacks exact underlying amount.",
                )
            })?;
            require(
                if withdraw_all {
                    received <= qty
                        && received >= u256(b.minimum_output.as_ref().expect("minimum"))?
                } else {
                    received == qty
                },
                "WITHDRAW_SIMULATION_LIMIT",
                "Simulated withdrawal differs from requested exact cash amount.",
            )?;
        }
        if operation == "uniswap.swap" {
            let received =
                Router::exactInputSingleCall::abi_decode_returns(&simulation).map_err(|_| {
                    error(
                        "SIMULATION_RETURN_INVALID",
                        "preview",
                        "Swap simulation lacks exact output amount.",
                    )
                })?;
            require(
                received >= u256(b.minimum_output.as_ref().expect("minimum"))?,
                "SWAP_SIMULATION_LIMIT",
                "Simulated output is below the approved minimum.",
            )?;
        }
        if let Some(old) = original {
            require(
                old.scaled_before == b.scaled_before
                    && old.allowance_before == b.allowance_before
                    && old.collateral_before == b.collateral_before,
                "STATE_CHANGED",
                "Bound scaled principal or authority changed since approval.",
            )?;
            // Observed claims may accrue but immutable execution fields and approved origin remain frozen.
            b.supplied_before = old.supplied_before.clone();
            b.debt_before = old.debt_before.clone();
            require(
                &b == old,
                "BINDING_CHANGED",
                "Financial binding changed since approval.",
            )?;
        }
        if operation.starts_with("aave.") {
            let supply_pos = position(&self.aave()?.pool, &token.reference, "supply");
            let debt_pos = position(&self.aave()?.pool, &token.reference, "variable-debt");
            let cash = AccountingLocation::Cash {
                resource: resource.clone(),
            };
            let supply = AccountingLocation::Position {
                resource: resource.clone(),
                position: supply_pos.clone(),
                position_kind: PositionKind::Supply,
            };
            let debt = AccountingLocation::Position {
                resource: resource.clone(),
                position: debt_pos.clone(),
                position_kind: PositionKind::VariableDebt,
            };
            let index: Quantity = serde_json::from_value(
                b.supply_anchor.as_ref().expect("anchor").evidence["normalized_index"].clone(),
            )
            .map_err(|_| FinError::validation("Invalid trusted index."))?;
            let quantum = (u256(&index)? + pow10(27)? - U256::from(1)) / pow10(27)? + U256::from(2);
            let ceiling = add(qty, quantum)?;
            if matches!(operation, "aave.supply" | "aave.withdraw") {
                // A complete withdrawal's observed claim, including yield,
                // must fit its finite max_amount. Partial withdrawals keep
                // their prior claim-plus-request envelope.
                let maximum_change = if operation == "aave.withdraw" && withdraw_all {
                    qty
                } else {
                    add(qty, u256(b.supplied_before.as_ref().expect("claim"))?)?
                };
                intent.expected.push(EffectConstraint::Accrual {
                    resource: resource.clone(),
                    position: supply_pos.clone(),
                    asset: principal_asset.clone(),
                    position_kind: PositionKind::Supply,
                    maximum_change: quantity(maximum_change),
                });
                let before = b.collateral_before.expect("collateral").to_string();
                intent.expected.push(EffectConstraint::AuthorityTransition {
                    resource: resource.clone(),
                    authority: collateral(&self.aave()?.pool, &token.reference),
                    before: before.clone(),
                    allowed_after: if operation == "aave.supply" {
                        vec![before, "true".into()]
                    } else {
                        vec![before, "false".into()]
                    },
                });
            }
            if matches!(operation, "aave.borrow" | "aave.repay") {
                // For repay-all the complete paid debt, including accrued
                // interest, must fit this fresh preview's finite max_amount.
                // Partial repayment keeps its distinct debt-plus-installment
                // envelope.
                let finite_full_close = operation == "aave.repay" && repay_all;
                let maximum_change = if finite_full_close {
                    qty
                } else {
                    add(qty, u256(b.debt_before.as_ref().expect("claim"))?)?
                };
                intent.expected.push(EffectConstraint::Accrual {
                    resource: resource.clone(),
                    position: debt_pos.clone(),
                    asset: principal_asset.clone(),
                    position_kind: PositionKind::VariableDebt,
                    maximum_change: quantity(maximum_change),
                });
            }
            match operation {
                "aave.supply" => {
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: cash,
                        to: supply,
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(qty),
                        minimum_credit: quantity(qty.saturating_sub(quantum).max(U256::from(1))),
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount(&principal_asset, quantum),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                "aave.borrow" => intent.expected.push(EffectConstraint::OriginMovement {
                    from: debt,
                    to: cash,
                    debit_asset: principal_asset.clone(),
                    credit_asset: principal_asset.clone(),
                    maximum_debit: quantity(qty),
                    minimum_credit: quantity(qty),
                }),
                "aave.repay" => {
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: cash,
                        to: debt,
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(qty),
                        minimum_credit: Quantity::from_u64(1),
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount(&principal_asset, qty),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                "aave.withdraw" => {
                    for e in &mut intent.expected {
                        if let EffectConstraint::Position { maximum_change, .. } = e {
                            *maximum_change = quantity(if withdraw_all { qty } else { ceiling });
                        }
                    }
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: supply,
                        to: AccountingLocation::Cash {
                            resource: super::resource(
                                self.config.chain_id,
                                address(b.recipient.as_deref().expect("recipient"))?,
                            ),
                        },
                        debit_asset: principal_asset.clone(),
                        credit_asset: principal_asset.clone(),
                        maximum_debit: quantity(if withdraw_all { qty } else { ceiling }),
                        minimum_credit: if withdraw_all {
                            b.minimum_output.clone().expect("minimum")
                        } else {
                            quantity(qty)
                        },
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: supply_pos.clone(),
                            position_kind: PositionKind::Supply,
                        },
                        amount: amount(&principal_asset, quantum),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                _ => {}
            }
        }
        let gas = b.transaction.maximum_cost()?;
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: amount(&self.native_asset(), gas),
            treatment: CostTreatment::AdditionalDebit,
        });
        let movement = if operation == "aave.collateral" {
            b.supplied_before.clone().unwrap_or_default()
        } else {
            quantity(qty)
        };
        let (movement_usd, source, movement_observed) = self
            .quote_usd_observed(&principal_asset.reference, movement)
            .await?;
        let (fee_usd, _, fee_observed) = self.quote_usd_observed("native", quantity(gas)).await?;
        let mut price_observations = vec![movement_observed, fee_observed];
        let mut prices = Vec::new();
        for asset in [&principal_asset, &self.native_asset()]
            .into_iter()
            .chain(b.output.as_ref())
        {
            if prices.iter().any(|p: &AssetPrice| p.asset == *asset) {
                continue;
            }
            let unit = quantity(pow10(asset.decimals)?);
            let (usd, source, observed_at) = self
                .quote_usd_observed(&asset.reference, unit.clone())
                .await?;
            price_observations.push(observed_at.clone());
            prices.push(AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: usd,
                unit_atoms: unit,
                observed_at,
                source,
            });
        }
        let observed = now();
        let additional_allocation = if operation == "aave.borrow" {
            let (value, _, observed_at) = self
                .quote_usd_observed(
                    &principal_asset.reference,
                    quantity(add(
                        qty,
                        u256(b.debt_rounding_maximum.as_ref().expect("borrow rounding"))?,
                    )?),
                )
                .await?;
            price_observations.push(observed_at);
            value
        } else {
            Quantity::zero()
        };
        let valuation_observed = price_observations
            .into_iter()
            .min_by_key(|timestamp| {
                chrono::DateTime::parse_from_rfc3339(timestamp)
                    .expect("validated observation timestamp")
            })
            .expect("principal and fee observations");
        let expires = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: serde_json::to_value(b).expect("binding serializable"),
            summary: format!(
                "{} from {} on EVM chain {} with exact principal {} and maximum native gas {}",
                request::description(operation),
                w.name,
                self.config.chain_id,
                qty,
                gas
            ),
            checks: vec![Check {
                name: "chain, deployment, balance, protocol and exact simulation".into(),
                status: CheckStatus::Passed,
                detail:
                    "Live reads validated finite exact prerequisites and frozen EIP-1559 fields."
                        .into(),
            }],
            warnings,
            observed_at: observed.clone(),
            expires_at: expires,
            valuation: Some(Valuation {
                movement_usd_micros: movement_usd,
                maximum_fee_usd_micros: fee_usd,
                additional_allocation_usd_micros: additional_allocation,
                observed_at: valuation_observed,
                source,
                prices,
            }),
            intent,
            operation_version: 1,
        })
    }
}
