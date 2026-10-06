//! Conditional plan evidence. These proofs neither authorize nor prepare a
//! transaction; every actual write re-enters the normal simulation/approval path.
use super::*;
use abi::*;
use fin_evm::{quantity, u256};

fn cash(
    context: &ReturnRouteContext,
    owner: &ResourceId,
    asset: &AssetId,
) -> Result<QuantityInterval> {
    let observed = context
        .projected
        .cash
        .iter()
        .filter(|h| &h.resource == owner && &h.asset == asset)
        .collect::<Vec<_>>();
    require(
        observed.len() <= 1,
        "PLAN_CASH_UNPROVED",
        "Projected cash cannot have duplicate physical asset intervals.",
    )?;
    let mut lo = U256::ZERO;
    for selected in context
        .allocations
        .iter()
        .filter(|h| &h.resource == owner && &h.amount.asset == asset)
    {
        lo = add(lo, u256(&selected.amount.atoms)?)?;
    }
    let mut hi = lo;
    for proof in &context.prior_proofs {
        for effect in &proof.effects {
            match effect {
                ProjectedEffect::CashDelta {
                    resource,
                    asset: a,
                    minimum_delta,
                    maximum_delta,
                } if resource == owner && a == asset => {
                    lo = if minimum_delta.is_negative() {
                        sub(lo, u256(&minimum_delta.magnitude())?)?
                    } else {
                        add(lo, u256(&minimum_delta.magnitude())?)?
                    };
                    hi = if maximum_delta.is_negative() {
                        sub(hi, u256(&maximum_delta.magnitude())?)?
                    } else {
                        add(hi, u256(&maximum_delta.magnitude())?)?
                    };
                }
                ProjectedEffect::CashSweep {
                    resource, asset: a, ..
                } if resource == owner && a == asset => {
                    lo = U256::ZERO;
                    hi = U256::ZERO;
                }
                _ => {}
            }
        }
    }
    let Some(observed) = observed.first() else {
        let baseline = context
            .baseline
            .state
            .holdings
            .iter()
            .filter(|holding| &holding.resource == owner && &holding.amount.asset == asset)
            .collect::<Vec<_>>();
        require(
            context.baseline.complete
                && context.baseline.pending.is_empty()
                && context.baseline.coverage.contains(&owner.0)
                && baseline.len() == 1
                && baseline[0].amount.atoms.is_zero()
                && lo.is_zero()
                && hi.is_zero(),
            "PLAN_CASH_UNPROVED",
            "Absent selected cash requires one complete verified zero baseline and zero reconstructed funding.",
        )?;
        return Ok(QuantityInterval {
            minimum: Quantity::zero(),
            maximum: Quantity::zero(),
        });
    };
    require(
        observed.amount.minimum <= observed.amount.maximum
            && lo <= hi
            && quantity(lo) <= observed.amount.minimum
            && quantity(hi) <= observed.amount.maximum,
        "PLAN_ALLOCATION_MISMATCH",
        "Conditional cash must remain financed by selected sources and their proved transformations.",
    )?;
    Ok(QuantityInterval {
        minimum: quantity(lo),
        maximum: quantity(hi),
    })
}
fn source_cash(context: &ReturnRouteContext, owner: &ResourceId, asset: &AssetId) -> Option<u32> {
    context.prior_proofs.iter().rev().find(|p|p.effects.iter().any(|e|matches!(e,
        ProjectedEffect::CashDelta {resource,asset:a,maximum_delta,..} if resource==owner && a==asset && !maximum_delta.is_negative() && !maximum_delta.is_zero()
    ))).map(|p|p.step_index)
}
fn requirement_cash_source(
    context: &ReturnRouteContext,
    owner: &ResourceId,
    asset: &AssetId,
    required: U256,
) -> Result<Option<u32>> {
    let required = quantity(required);
    require(
        cash(context, owner, asset)?.minimum >= required,
        "PLAN_PRINCIPAL_UNFUNDED",
        "The selected cash history must guarantee the requested principal after prior debits and costs.",
    )?;
    if let Some(proof) = context.prior_proofs.iter().rev().find(|proof| {
        proof.effects.iter().any(|effect| {
            matches!(effect,
                ProjectedEffect::CashDelta {
                    resource, asset: candidate, minimum_delta, maximum_delta
                } if resource == owner && candidate == asset
                    && !minimum_delta.is_negative()
                    && !maximum_delta.is_negative() && !maximum_delta.is_zero()
            )
        })
    }) {
        return Ok(Some(proof.step_index));
    }
    let selected = context
        .allocations
        .iter()
        .filter(|a| &a.resource == owner && &a.amount.asset == asset)
        .try_fold(U256::ZERO, |sum, a| add(sum, u256(&a.amount.atoms)?))?;
    require(
        quantity(selected) >= required,
        "PLAN_SOURCE_MISSING",
        "Cash requirements need original selected funding or a matching prior credit chain.",
    )?;
    Ok(None)
}
fn add_cash(
    effects: &mut Vec<ProjectedEffect>,
    owner: &ResourceId,
    asset: &AssetId,
    lower: U256,
    upper: U256,
    credit: bool,
) {
    let (minimum_delta, maximum_delta) = if credit {
        (
            SignedQuantity::positive(&quantity(lower)),
            SignedQuantity::positive(&quantity(upper)),
        )
    } else {
        (
            SignedQuantity::negative(&quantity(upper)),
            SignedQuantity::negative(&quantity(lower)),
        )
    };
    effects.push(ProjectedEffect::CashDelta {
        resource: owner.clone(),
        asset: asset.clone(),
        minimum_delta,
        maximum_delta,
    });
}
fn scoped_sweep(context: &ReturnRouteContext, owner: &ResourceId, asset: &AssetId) -> Result<u32> {
    let observed = context
        .baseline
        .state
        .holdings
        .iter()
        .find(|h| &h.resource == owner && &h.amount.asset == asset)
        .ok_or_else(|| {
            error(
                "PLAN_BASELINE_MISSING",
                "preflight",
                "Sweep asset is absent from the complete baseline.",
            )
        })?;
    require(
        observed.amount.atoms.is_zero()
            || context
                .allocations
                .iter()
                .any(|a| &a.resource == owner && a.amount == observed.amount),
        "PLAN_SWEEP_UNSELECTED",
        "A full sweep cannot include unselected baseline principal.",
    )?;
    source_cash(context, owner, asset).ok_or_else(|| {
        error(
            "PLAN_SOURCE_MISSING",
            "preflight",
            "An intermediate sweep must name a proved earlier funding step.",
        )
    })
}
fn planned_position<'a>(
    context: &'a ReturnRouteContext,
    owner: &ResourceId,
    name: &str,
    kind: &PositionKind,
    asset: &AssetId,
) -> Result<&'a QuantityInterval> {
    let rows = context
        .projected
        .positions
        .iter()
        .filter(|p| {
            &p.resource == owner
                && p.position == name
                && &p.position_kind == kind
                && &p.asset == asset
        })
        .collect::<Vec<_>>();
    require(
        rows.len() == 1 && rows[0].amount.minimum <= rows[0].amount.maximum,
        "PLAN_POSITION_UNPROVED",
        "Projected position must have one bounded physical identity.",
    )?;
    Ok(&rows[0].amount)
}
fn source_position(
    context: &ReturnRouteContext,
    owner: &ResourceId,
    name: &str,
    kind: &PositionKind,
) -> Result<u32> {
    context.prior_proofs.iter().rev().find(|p|p.effects.iter().any(|e|matches!(e,
        ProjectedEffect::PositionDelta {resource,position,position_kind,maximum_delta,..} if resource==owner && position==name && position_kind==kind && !maximum_delta.is_negative() && !maximum_delta.is_zero()
    ))).map(|p|p.step_index).ok_or_else(||error("PLAN_SOURCE_MISSING","preflight","A full position close must follow its broker-proved funding step."))
}
fn finite_allowance(
    context: &ReturnRouteContext,
    owner: &ResourceId,
    token: &str,
    spender: &str,
    minimum: U256,
) -> Result<()> {
    let name = authority(token, spender);
    let observed = context
        .projected
        .authorities
        .iter()
        .find(|a| &a.resource == owner && a.authority == name)
        .ok_or_else(|| {
            error(
                "PLAN_AUTHORITY_UNPROVED",
                "preflight",
                "Projected spender authority is absent.",
            )
        })?;
    let value = U256::from_str_radix(&observed.value, 10).map_err(|_| {
        error(
            "PLAN_AUTHORITY_INVALID",
            "preflight",
            "Projected allowance is not exact uint256.",
        )
    })?;
    require(
        value >= minimum && value < U256::MAX,
        "PLAN_ALLOWANCE_UNFUNDED",
        "Projected finite allowance must cover the complete spend bound.",
    )
}
impl EthereumAdapter {
    pub(crate) async fn preflight_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof> {
        self.preflight_route_target(operation, input, context, None)
            .await
    }
    /// Conditional exact-native funding only, invoked by the protected broker's
    /// Binance deposit composite. Ordinary execution still uses its immutable
    /// inner wallet.transfer preview and canonical original receipt; venue
    /// credit is completed only by matching usable Binance deposit history.
    #[doc(hidden)]
    pub async fn preflight_native_deposit_transfer(
        &self,
        inner_wallet_transfer_input: &Value,
        context: &ReturnRouteContext,
        target: &BrokerVerifiedNativeDepositTarget,
    ) -> Result<ReturnRouteProof> {
        let request = request::parse("wallet.transfer", inner_wallet_transfer_input)?;
        let clock = chrono::Utc::now();
        let observed = chrono::DateTime::parse_from_rfc3339(&target.observed_at).map_err(|_| {
            error(
                "DEPOSIT_TARGET_INVALID",
                "preflight",
                "Deposit observation time is malformed.",
            )
        })?;
        let expires = chrono::DateTime::parse_from_rfc3339(&target.expires_at).map_err(|_| {
            error(
                "DEPOSIT_TARGET_INVALID",
                "preflight",
                "Deposit expiry is malformed.",
            )
        })?;
        require(
            observed <= clock + chrono::Duration::seconds(5)
                && observed >= clock - chrono::Duration::seconds(90)
                && expires > clock
                && expires <= observed + chrono::Duration::seconds(120),
            "DEPOSIT_TARGET_STALE",
            "Deposit target must be freshly observed and unexpired on system UTC.",
        )?;
        require(
            self.config.chain_id == 1
                && target.chain_id == 1
                && target.network == "ETH"
                && target.asset.network == "binance"
                && target.asset.reference == "ETH"
                && target.asset.decimals == 36
                && target.evidence_digest.len() == 64
                && target
                    .evidence_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "DEPOSIT_TARGET_INVALID",
            "Deposit target must bind reviewed Ethereum mainnet native ETH, exact venue scale and canonical evidence digest.",
        )?;
        require(
            request.token.as_deref() == Some("native")
                && request.amount.as_deref() != Some("all")
                && address(request.recipient.as_deref().expect("schema"))?
                    == address(&target.address)?,
            "DEPOSIT_TARGET_MISMATCH",
            "Only exact finite native transfer to the freshly verified deposit address is permitted.",
        )?;
        self.preflight_route_target(
            "wallet.transfer",
            inner_wallet_transfer_input,
            context,
            Some(target),
        )
        .await
    }
    async fn preflight_route_target(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
        external_target: Option<&BrokerVerifiedNativeDepositTarget>,
    ) -> Result<ReturnRouteProof> {
        require(
            !request::is_read(operation),
            "PLAN_WRITE_REQUIRED",
            "A planned route must name a supported write.",
        )?;
        self.require_operation(operation)?;
        let r = request::parse(operation, input)?;
        let wallet = self.resolve(&r, true)?;
        let owner = address(&wallet.address)?;
        let resource = resource(self.config.chain_id, owner);
        require(
            context.baseline.complete
                && context.baseline.pending.is_empty()
                && context.baseline.state.in_flight.is_empty()
                && context.baseline.coverage.contains(&resource.0),
            "PLAN_BASELINE_INCOMPLETE",
            "A conditional route requires the complete broker-captured physical baseline.",
        )?;
        require(
            context
                .prior_proofs
                .iter()
                .all(|p| p.step_index < context.step_index),
            "PLAN_ORDER_INVALID",
            "Funding proofs must precede the dependent step.",
        )?;
        self.runtime.assert_chain().await?;
        let reference = if matches!(operation, "wallet.wrap" | "wallet.unwrap") {
            self.weth()?
        } else if operation == "uniswap.swap" {
            r.token_in.as_deref().expect("schema")
        } else {
            r.token.as_deref().expect("schema")
        };
        let token = self.token(reference, None).await?;
        let principal = if operation == "wallet.wrap" {
            self.native_asset()
        } else {
            token.asset(self)
        };
        let all = r.amount.as_deref() == Some("all");
        let cap = if operation == "aave.collateral" {
            U256::ZERO
        } else {
            exact(
                if all {
                    r.max_amount.as_deref().expect("schema")
                } else {
                    r.amount.as_deref().expect("schema")
                },
                token.decimals,
            )?
        };
        require(
            cap > U256::ZERO || matches!(operation, "wallet.approve" | "aave.collateral"),
            "ZERO_AMOUNT",
            "Planned principal must be positive.",
        )?;
        let maximum_fee = exact(r.max_fee_eth.as_deref().expect("schema"), 18)?;
        require(
            maximum_fee > U256::ZERO,
            "FEE_LIMIT",
            "Conditional routes require finite positive native fee bounds.",
        )?;
        let gas_ceiling = route_gas_ceiling(operation)?;
        let (required_fee, _) = self.runtime.fees().await?;
        let planning_fee = add(
            mul(mul(U256::from(gas_ceiling), required_fee)?, U256::from(3))?,
            U256::from(1),
        )? / U256::from(2);
        require(
            planning_fee <= maximum_fee,
            "PLAN_FEE_TOO_LOW",
            "Current EIP-1559 fee plus 50% headroom and reviewed operation gas envelope exceeds max_fee_eth.",
        )?;
        let (fee_usd, source) = self.quote_usd("native", quantity(maximum_fee)).await?;
        require(
            fee_usd <= context.remaining_cost_budget_usd_micros,
            "PLAN_FEE_UNFUNDED",
            "The remaining full-unwind reserve cannot cover this native fee cap.",
        )?;
        require(
            u256(&cash(context, &resource, &self.native_asset())?.minimum)? >= maximum_fee,
            "PLAN_GAS_UNFUNDED",
            "Broker-projected native cash must fund the complete fee cap.",
        )?;
        let mut intent = FinancialIntent::default();
        intent.locks.push(resource.clone());
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: amount(&self.native_asset(), maximum_fee),
            treatment: CostTreatment::AdditionalDebit,
        });
        // Gas is an earmark of the wallet's original selected native funding.
        // An incoming transfer must not replace that source merely because it
        // is the most recent native credit (it may be smaller than the fee).
        // The independently reconstructed selected cash interval above still
        // has to fund this prefix, including principal and all earlier costs.
        let selected_native = context
            .allocations
            .iter()
            .filter(|a| a.resource == resource && a.amount.asset == self.native_asset())
            .try_fold(U256::ZERO, |sum, a| add(sum, u256(&a.amount.atoms)?))?;
        let gas_source = if selected_native >= maximum_fee {
            None
        } else {
            source_cash(context, &resource, &self.native_asset())
        };
        let mut requires = vec![ProjectedRequirement::CashAtLeast {
            resource: resource.clone(),
            asset: self.native_asset(),
            amount: quantity(maximum_fee),
            source_step: gas_source,
        }];
        let mut effects = Vec::new();
        // AdditionalDebit costs are projected once by the broker engine from
        // maximum_costs; raw adapter effects describe principal transformations.
        let mut evidence = json!({"method":"conditional-ethereum-route-v1","chain_id":self.config.chain_id,"resource":resource,"unmet_execution_dependencies":"Projected cash/authority/positions are broker plan evidence, not current holdings or simulated execution. Actual writes require fresh ordinary preview and authorization.","gas_cap":quantity(maximum_fee),"gas_valuation_source":source});
        evidence["conditional_gas_envelope"] = json!({"gas":gas_ceiling,"current_max_fee_per_gas":quantity(required_fee),"planning_fee_with_headroom":quantity(planning_fee),"semantics":"Reviewed profile planning ceiling; no transaction simulation is claimed. Ordinary fresh execution still estimates and freezes actual bounded gas."});
        match operation {
            "wallet.transfer" => {
                let recipient = address(r.recipient.as_deref().expect("schema"))?;
                require(
                    recipient != Address::ZERO && recipient != owner,
                    "RECIPIENT_INVALID",
                    "A transfer route needs a different nonzero recipient.",
                )?;
                let source = cash(context, &resource, &principal)?;
                let required_cash = if principal.reference == "native" {
                    add(cap, maximum_fee)?
                } else {
                    cap
                };
                require(
                    u256(&source.minimum)? >= required_cash,
                    "PLAN_PRINCIPAL_UNFUNDED",
                    "Transfer principal and native gas must remain financed by selected source cash.",
                )?;
                let (destination, credit_asset, credit_atoms, selected_return_gas) = if let Some(
                    target,
                ) =
                    external_target
                {
                    require(
                        context
                            .baseline
                            .coverage
                            .contains(&target.account_resource.0),
                        "PLAN_BASELINE_INCOMPLETE",
                        "The verified venue account must be covered by the complete original snapshot.",
                    )?;
                    let _ = cash(context, &target.account_resource, &target.asset)?;
                    let credit = mul(cap, pow10(18)?)?;
                    require(
                        quantity(credit) >= target.minimum_deposit,
                        "DEPOSIT_MINIMUM",
                        "Exact projected venue credit must meet the verified usable-deposit minimum.",
                    )?;
                    evidence["broker_verified_deposit_target"] = json!(target);
                    evidence["deposit_completion"] = json!(
                        "Canonical inner Ethereum receipt alone is insufficient: the broker composite must additionally prove matching usable Binance deposit credit. No venue gas source is introduced."
                    );
                    (
                        target.account_resource.clone(),
                        target.asset.clone(),
                        credit,
                        None,
                    )
                } else {
                    require(
                        self.wallets
                            .iter()
                            .any(|w| w.signing_enabled && w.address == lower_address(recipient)),
                        "PLAN_RECIPIENT_UNSUPPORTED",
                        "A restoration transfer route requires a configured return-capable signing wallet on this chain.",
                    )?;
                    let destination = super::resource(self.config.chain_id, recipient);
                    require(
                        context.baseline.coverage.contains(&destination.0),
                        "PLAN_BASELINE_INCOMPLETE",
                        "The return wallet must be covered by the complete original snapshot.",
                    )?;
                    // Incoming principal never acts as an unapproved gas subsidy.
                    let mut selected = U256::ZERO;
                    for allocation in context.allocations.iter().filter(|a| {
                        a.resource == destination && a.amount.asset == self.native_asset()
                    }) {
                        selected = add(selected, u256(&allocation.amount.atoms)?)?;
                    }
                    require(
                        selected >= maximum_fee
                            && u256(&cash(context, &destination, &self.native_asset())?.minimum)?
                                >= maximum_fee,
                        "PLAN_RETURN_GAS_UNFUNDED",
                        "The destination's original selected native allocation must fund the complete bounded return fee.",
                    )?;
                    let _ = cash(context, &destination, &principal)?;
                    (destination, principal.clone(), cap, Some(selected))
                };
                if principal.reference == "native" {
                    let code = fin_evm::rpc_bytes(
                        &self
                            .runtime
                            .rpc
                            .request("eth_getCode", json!([lower_address(recipient), "latest"]))
                            .await?,
                    )?;
                    require(
                        code.is_empty(),
                        "NATIVE_CONTRACT_RECIPIENT_UNSUPPORTED",
                        "This native return route requires an independently observed externally-owned recipient.",
                    )?;
                }
                intent.locks.push(destination.clone());
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal, cap),
                });
                intent.expected.push(EffectConstraint::Transfer {
                    from: resource.clone(),
                    to: destination.clone(),
                    debit_asset: principal.clone(),
                    credit_asset: credit_asset.clone(),
                    maximum_debit: quantity(cap),
                    minimum_credit: quantity(credit_atoms),
                });
                requires.push(ProjectedRequirement::CashAtLeast {
                    resource: resource.clone(),
                    asset: principal.clone(),
                    amount: quantity(cap),
                    source_step: requirement_cash_source(context, &resource, &principal, cap)?,
                });
                if let Some(selected) = selected_return_gas {
                    requires.push(ProjectedRequirement::CashAtLeast {
                        resource: destination.clone(),
                        asset: self.native_asset(),
                        amount: quantity(maximum_fee),
                        source_step: None,
                    });
                    evidence["return_gas_source"] = json!({"original_selected_native":quantity(selected),"minimum_return_fee":quantity(maximum_fee),"semantics":"A separately validated return step and fresh per-write approval are still mandatory."});
                }
                add_cash(&mut effects, &resource, &principal, cap, cap, false);
                add_cash(
                    &mut effects,
                    &destination,
                    &credit_asset,
                    credit_atoms,
                    credit_atoms,
                    true,
                );
                evidence["destination"] = json!(destination);
            }
            "wallet.wrap" | "wallet.unwrap" => {
                require(
                    token.decimals == 18,
                    "WETH_PRECISION_INVALID",
                    "Wrap route requires configured 18-decimal WETH.",
                )?;
                let input_asset = &principal;
                let input_cash = cash(context, &resource, input_asset)?;
                let lo = if all { u256(&input_cash.minimum)? } else { cap };
                let hi = if all { u256(&input_cash.maximum)? } else { cap };
                require(
                    lo > U256::ZERO && hi <= cap && input_cash.minimum >= quantity(lo),
                    "PLAN_PRINCIPAL_UNFUNDED",
                    "Projected conversion input must fit finite selected cash and cap.",
                )?;
                if operation == "wallet.wrap" {
                    require(
                        u256(&input_cash.minimum)? >= add(cap, maximum_fee)?,
                        "PLAN_GAS_UNFUNDED",
                        "Wrap principal and full gas must both fit selected native cash.",
                    )?;
                }
                requires.push(ProjectedRequirement::CashAtLeast {
                    resource: resource.clone(),
                    asset: input_asset.clone(),
                    amount: quantity(lo),
                    source_step: requirement_cash_source(context, &resource, input_asset, lo)?,
                });
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(input_asset, hi),
                });
                let output = if operation == "wallet.wrap" {
                    token.asset(self)
                } else {
                    self.native_asset()
                };
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: input_asset.clone(),
                    maximum_input: quantity(cap),
                    output: output.clone(),
                    minimum_output: quantity(lo),
                });
                if all {
                    effects.push(ProjectedEffect::CashSweep {
                        resource: resource.clone(),
                        asset: input_asset.clone(),
                        maximum_amount: quantity(hi),
                        source_step: scoped_sweep(context, &resource, input_asset)?,
                    });
                } else {
                    add_cash(&mut effects, &resource, input_asset, cap, cap, false);
                }
                add_cash(&mut effects, &resource, &output, lo, hi, true);
            }
            "wallet.approve" => {
                require(
                    token.reference != "native" && cap < U256::MAX,
                    "PLAN_ALLOWANCE_INVALID",
                    "Planned approval must be finite ERC-20 authority.",
                )?;
                let spender = address(r.spender.as_deref().expect("schema"))?;
                require(
                    spender != Address::ZERO
                        && [self.aave()?.pool.as_str(), self.uniswap()?.router.as_str()]
                            .contains(&lower_address(spender).as_str()),
                    "PLAN_SPENDER_UNSUPPORTED",
                    "This restoration profile approves only its reviewed Pool and router.",
                )?;
                self.runtime.code(spender).await?;
                let name = authority(&token.reference, &lower_address(spender));
                let before = context
                    .projected
                    .authorities
                    .iter()
                    .find(|a| a.resource == resource && a.authority == name)
                    .ok_or_else(|| {
                        error(
                            "PLAN_AUTHORITY_UNPROVED",
                            "preflight",
                            "Projected allowance identity is absent.",
                        )
                    })?;
                if cap > U256::ZERO {
                    let projected = cash(context, &resource, &principal)?;
                    if cap > u256(&projected.maximum)? {
                        let zero_baseline = context.baseline.state.holdings.iter().any(|h| {
                            h.resource == resource
                                && h.amount.asset == principal
                                && h.amount.atoms.is_zero()
                        });
                        require(
                            zero_baseline && source_cash(context, &resource, &principal).is_some(),
                            "PLAN_ALLOWANCE_UNFUNDED",
                            "A future cleanup allowance requires proved zero-baseline intermediate funding.",
                        )?;
                        let mut selected_usd = Quantity::zero();
                        for selected in context
                            .allocations
                            .iter()
                            .filter(|a| a.resource == resource)
                        {
                            selected_usd = selected_usd.checked_add(
                                &self
                                    .quote_usd(
                                        &selected.amount.asset.reference,
                                        selected.amount.atoms.clone(),
                                    )
                                    .await?
                                    .0,
                            )?;
                        }
                        require(
                            self.quote_usd(&principal.reference, quantity(cap)).await?.0
                                <= selected_usd,
                            "PLAN_ALLOWANCE_UNFUNDED",
                            "Explicit cleanup allowance cannot exceed the selected origin's bounded economic allocation.",
                        )?;
                        evidence["allowance_cap_semantics"] = json!(
                            "Explicit finite future-cleanup authority within selected funding value; approval has no principal debit. Fresh actual spend still requires tracked cash, exact preview and restoration of original authority."
                        );
                    }
                }
                intent.expected.push(EffectConstraint::Authority {
                    resource: resource.clone(),
                    authority: name.clone(),
                    before: before.value.clone(),
                    after: cap.to_string(),
                });
                effects.push(ProjectedEffect::AuthorityValue {
                    resource: resource.clone(),
                    authority: name,
                    value: cap.to_string(),
                });
                evidence["spender"] = json!(lower_address(spender));
            }
            "uniswap.swap" => {
                let input_cash = cash(context, &resource, &principal)?;
                let lo = if all { u256(&input_cash.minimum)? } else { cap };
                let hi = if all { u256(&input_cash.maximum)? } else { cap };
                require(
                    lo > U256::ZERO && hi <= cap && input_cash.minimum >= quantity(lo),
                    "PLAN_PRINCIPAL_UNFUNDED",
                    "Swap principal must fit finite broker-selected cash.",
                )?;
                finite_allowance(
                    context,
                    &resource,
                    &token.reference,
                    &self.uniswap()?.router,
                    hi,
                )?;
                require(
                    address(r.recipient.as_deref().expect("schema"))? == owner,
                    "SWAP_RECIPIENT_UNSUPPORTED",
                    "The scoped swap returns its output to the selected wallet.",
                )?;
                let quote = self
                    .quote(
                        &token.reference,
                        r.token_out.as_deref().expect("schema"),
                        &quantity(lo).to_decimal(token.decimals),
                        r.fee_tier,
                        None,
                    )
                    .await?;
                self.runtime.code(address(&self.uniswap()?.router)?).await?;
                let output = quote.token_out.asset(self);
                let minimum = exact(r.min_output.as_deref().expect("schema"), output.decimals)?;
                let quoted = u256(&quote.output)?;
                require(
                    minimum > U256::ZERO
                        && minimum <= quoted
                        && minimum
                            >= mul(quoted, U256::from(10000 - r.slippage_bps.expect("schema")))?
                                / U256::from(10000),
                    "SLIPPAGE_LIMIT",
                    "Planned swap minimum must respect the fresh quoted slippage floor.",
                )?;
                let timestamp = chrono::Utc::now().timestamp() as u64;
                require(
                    r.deadline.expect("schema") > timestamp
                        && r.deadline.expect("schema") <= timestamp + 600,
                    "DEADLINE_INVALID",
                    "Conditional swap deadline must remain fresh and finite.",
                )?;
                let (input_usd, _) = self.quote_usd(&token.reference, quantity(hi)).await?;
                let (unit_usd, _) = self
                    .quote_usd(&output.reference, quantity(pow10(output.decimals)?))
                    .await?;
                let conditional_cap = input_usd
                    .mul_div_ceil(&Quantity::from_u64(110), &Quantity::from_u64(100))?
                    .mul_div_floor(&quantity(pow10(output.decimals)?), &unit_usd)?;
                require(
                    conditional_cap >= quote.output,
                    "PLAN_OUTPUT_CAP_UNAVAILABLE",
                    "The current quote exceeds the explicitly conditional finite return cap.",
                )?;
                evidence["pool"] = json!(quote.pool);
                evidence["fee_tier"] = json!(quote.fee_tier);
                evidence["conditional_output_cap"] = json!(conditional_cap);
                evidence["output_cap_semantics"] = json!(
                    "Current oracle-valued input plus 10% planning headroom; NOT an enforced protocol output maximum. True excess credits require cleanup outstanding and fresh funded approved cleanup, never altered receipt evidence."
                );
                requires.push(ProjectedRequirement::CashAtLeast {
                    resource: resource.clone(),
                    asset: principal.clone(),
                    amount: quantity(lo),
                    source_step: requirement_cash_source(context, &resource, &principal, lo)?,
                });
                intent.debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: amount(&principal, hi),
                });
                intent.expected.push(EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: principal.clone(),
                    maximum_input: quantity(cap),
                    output: output.clone(),
                    minimum_output: quantity(minimum),
                });
                if all {
                    effects.push(ProjectedEffect::CashSweep {
                        resource: resource.clone(),
                        asset: principal.clone(),
                        maximum_amount: quantity(hi),
                        source_step: scoped_sweep(context, &resource, &principal)?,
                    });
                } else {
                    add_cash(&mut effects, &resource, &principal, cap, cap, false);
                }
                add_cash(
                    &mut effects,
                    &resource,
                    &output,
                    minimum,
                    u256(&conditional_cap)?,
                    true,
                );
            }
            "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral" => {
                let reserve = self.reserve(&token.reference, None).await?;
                require(
                    reserve.active && !reserve.paused,
                    "RESERVE_UNAVAILABLE",
                    "Conditional Aave reserve must be active and unpaused.",
                )?;
                // This first scoped profile introduces and completely closes all
                // test positions. Existing unrelated exposure remains observable,
                // but cannot be borrowed against or swept as test principal.
                require(
                    context
                        .baseline
                        .state
                        .positions
                        .iter()
                        .filter(|p| p.resource == resource)
                        .all(|p| p.amount.atoms.is_zero()),
                    "PLAN_POSITION_BASELINE_UNSUPPORTED",
                    "This practical full-close route requires zero initial Aave positions and debt.",
                )?;
                let supply_name = position(&self.aave()?.pool, &token.reference, "supply");
                let debt_name = position(&self.aave()?.pool, &token.reference, "variable-debt");
                let (kind, name, receipt) = if matches!(operation, "aave.borrow" | "aave.repay") {
                    (
                        PositionKind::VariableDebt,
                        debt_name.clone(),
                        address(&reserve.variable_debt_token)?,
                    )
                } else {
                    (
                        PositionKind::Supply,
                        supply_name.clone(),
                        address(&reserve.a_token)?,
                    )
                };
                let revision = if kind == PositionKind::Supply {
                    self.runtime
                        .call(receipt, &AToken::ATOKEN_REVISIONCall {}, None)
                        .await?
                } else {
                    self.runtime
                        .call(receipt, &AToken::DEBT_TOKEN_REVISIONCall {}, None)
                        .await?
                };
                require(
                    revision == U256::from(5),
                    "TOKEN_REVISION_UNSUPPORTED",
                    "Selected conditional protocol route requires reviewed revision 5.",
                )?;
                let p = planned_position(context, &resource, &name, &kind, &principal)?;
                let current_min = u256(&p.minimum)?;
                let current_max = u256(&p.maximum)?;
                let pool = address(&self.aave()?.pool)?;
                let index = if kind == PositionKind::Supply {
                    self.runtime
                        .call(
                            pool,
                            &Pool::getReserveNormalizedIncomeCall {
                                asset: address(&token.reference)?,
                            },
                            None,
                        )
                        .await?
                } else {
                    self.runtime
                        .call(
                            pool,
                            &Pool::getReserveNormalizedVariableDebtCall {
                                asset: address(&token.reference)?,
                            },
                            None,
                        )
                        .await?
                };
                require(
                    index >= pow10(27)? && index < (U256::from(1) << 128),
                    "ANCHOR_INDEX_INVALID",
                    "Conditional normalized index lies outside reviewed math bounds.",
                )?;
                let quantum = add(
                    (index + pow10(27)? - U256::from(1)) / pow10(27)?,
                    U256::from(2),
                )?;
                let collateral_name = collateral(&self.aave()?.pool, &token.reference);
                let prior_collateral = context
                    .projected
                    .authorities
                    .iter()
                    .find(|a| a.resource == resource && a.authority == collateral_name)
                    .ok_or_else(|| {
                        error(
                            "PLAN_AUTHORITY_UNPROVED",
                            "preflight",
                            "Projected collateral identity is absent.",
                        )
                    })?;
                let position_location = AccountingLocation::Position {
                    resource: resource.clone(),
                    position: name.clone(),
                    position_kind: kind.clone(),
                };
                let cash_location = AccountingLocation::Cash {
                    resource: resource.clone(),
                };
                evidence["reserve"] = json!(reserve);
                evidence["normalized_index"] = json!(quantity(index));
                if matches!(
                    operation,
                    "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw"
                ) {
                    // Full close operations are bounded by their finite
                    // physical close cap. Interest/yield is part of the total
                    // debt/claim closed and cannot exceed that cap. Other
                    // position operations retain the projected-balance plus
                    // operation-cap bound.
                    let maximum_change = if matches!(operation, "aave.repay" | "aave.withdraw") {
                        cap
                    } else {
                        add(cap, current_max)?
                    };
                    intent.expected.push(EffectConstraint::Accrual {
                        resource: resource.clone(),
                        position: name.clone(),
                        asset: principal.clone(),
                        position_kind: kind.clone(),
                        maximum_change: quantity(maximum_change),
                    });
                }
                if matches!(operation, "aave.supply" | "aave.withdraw") {
                    intent.expected.push(EffectConstraint::AuthorityTransition {
                        resource: resource.clone(),
                        authority: collateral_name.clone(),
                        before: prior_collateral.value.clone(),
                        allowed_after: vec![
                            prior_collateral.value.clone(),
                            if operation == "aave.supply" {
                                "true".into()
                            } else {
                                "false".into()
                            },
                        ],
                    });
                }
                if operation == "aave.withdraw" {
                    intent.maximum_costs.push(CostBound {
                        location: position_location.clone(),
                        amount: amount(&principal, quantum),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                match operation {
                    "aave.supply" => {
                        require(
                            !reserve.frozen,
                            "RESERVE_FROZEN",
                            "Frozen reserve cannot accept planned supply.",
                        )?;
                        let supply_cap = mul(u256(&reserve.supply_cap)?, pow10(token.decimals)?)?;
                        require(
                            supply_cap.is_zero()
                                || add(u256(&reserve.total_supply)?, cap)? <= supply_cap,
                            "SUPPLY_CAP",
                            "Planned supply exceeds current reserve cap.",
                        )?;
                        let funds = cash(context, &resource, &principal)?;
                        require(
                            u256(&funds.minimum)? >= cap && cap > quantum,
                            "PLAN_PRINCIPAL_UNFUNDED",
                            "Selected cash must cover non-dust supply and exact mint rounding.",
                        )?;
                        finite_allowance(
                            context,
                            &resource,
                            &token.reference,
                            &self.aave()?.pool,
                            cap,
                        )?;
                        let minimum = sub(cap, quantum)?;
                        requires.push(ProjectedRequirement::CashAtLeast {
                            resource: resource.clone(),
                            asset: principal.clone(),
                            amount: quantity(cap),
                            source_step: requirement_cash_source(
                                context, &resource, &principal, cap,
                            )?,
                        });
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal, cap),
                        });
                        intent.expected.extend([
                            EffectConstraint::Debit {
                                resource: resource.clone(),
                                asset: principal.clone(),
                                maximum_debit: quantity(cap),
                            },
                            EffectConstraint::Position {
                                resource: resource.clone(),
                                position: name.clone(),
                                asset: principal.clone(),
                                direction: Direction::Increase,
                                maximum_change: quantity(cap),
                            },
                            EffectConstraint::OriginMovement {
                                from: cash_location,
                                to: position_location,
                                debit_asset: principal.clone(),
                                credit_asset: principal.clone(),
                                maximum_debit: quantity(cap),
                                minimum_credit: quantity(minimum),
                            },
                        ]);
                        intent.maximum_costs.push(CostBound {
                            location: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            amount: amount(&principal, quantum),
                            treatment: CostTreatment::IncludedInDebit,
                        });
                        add_cash(&mut effects, &resource, &principal, cap, cap, false);
                        effects.push(ProjectedEffect::PositionDelta {
                            resource: resource.clone(),
                            position: name,
                            asset: principal.clone(),
                            position_kind: kind,
                            minimum_delta: SignedQuantity::positive(&quantity(minimum)),
                            maximum_delta: SignedQuantity::positive(&quantity(cap)),
                        });
                        require(
                            reserve.collateral_eligible && !reserve.ltv.is_zero(),
                            "COLLATERAL_UNAVAILABLE",
                            "This introduced-position profile requires collateral-eligible supply.",
                        )?;
                        effects.push(ProjectedEffect::AuthorityValue {
                            resource: resource.clone(),
                            authority: collateral_name,
                            value: "true".into(),
                        });
                    }
                    "aave.borrow" => {
                        require(
                            !reserve.frozen && reserve.borrowing,
                            "BORROW_UNAVAILABLE",
                            "Conditional reserve must permit variable borrowing.",
                        )?;
                        let added = add(cap, quantum)?;
                        require(
                            u256(&reserve.liquidity)? >= cap,
                            "LIQUIDITY_INSUFFICIENT",
                            "Current reserve cannot fund the planned loan.",
                        )?;
                        let borrow_cap = mul(u256(&reserve.borrow_cap)?, pow10(token.decimals)?)?;
                        require(
                            borrow_cap.is_zero()
                                || add(u256(&reserve.total_debt)?, added)? <= borrow_cap,
                            "BORROW_CAP",
                            "Planned physical debt exceeds the current reserve cap.",
                        )?;
                        let mut borrowing_value = U256::ZERO;
                        let mut liquidation_value = U256::ZERO;
                        let mut debt_value =
                            u256(&self.quote_usd(&token.reference, quantity(added)).await?.0)?;
                        let configured_pool = &self.aave()?.pool;
                        for position in context
                            .projected
                            .positions
                            .iter()
                            .filter(|p| p.resource == resource && !p.amount.maximum.is_zero())
                        {
                            if position.position_kind == PositionKind::Supply
                                && context.projected.authorities.iter().any(|a| {
                                    a.resource == resource
                                        && a.authority
                                            == collateral(
                                                configured_pool,
                                                &position.asset.reference,
                                            )
                                        && a.value == "true"
                                })
                            {
                                let info = self.reserve(&position.asset.reference, None).await?;
                                let usd = u256(
                                    &self
                                        .quote_usd(
                                            &position.asset.reference,
                                            position.amount.minimum.clone(),
                                        )
                                        .await?
                                        .0,
                                )?;
                                borrowing_value = add(
                                    borrowing_value,
                                    mul(usd, u256(&info.ltv)?)? / U256::from(10000),
                                )?;
                                liquidation_value = add(
                                    liquidation_value,
                                    mul(usd, u256(&info.liquidation_threshold)?)?
                                        / U256::from(10000),
                                )?;
                            } else if position.position_kind != PositionKind::Supply {
                                debt_value = add(
                                    debt_value,
                                    u256(
                                        &self
                                            .quote_usd(
                                                &position.asset.reference,
                                                position.amount.maximum.clone(),
                                            )
                                            .await?
                                            .0,
                                    )?,
                                )?;
                            }
                        }
                        let requested_health =
                            exact(r.min_health_factor.as_deref().expect("schema"), 18)?;
                        require(
                            requested_health >= pow10(18)?
                                && debt_value <= borrowing_value
                                && mul(liquidation_value, pow10(18)?)? / debt_value
                                    >= requested_health,
                            "BORROW_HEALTH_LIMIT",
                            "Conservative broker-projected collateral cannot cover the bounded loan and requested health factor.",
                        )?;
                        intent.expected.extend([
                            EffectConstraint::Debt {
                                resource: resource.clone(),
                                debt: name.clone(),
                                asset: principal.clone(),
                                direction: Direction::Increase,
                                maximum_change: quantity(added),
                            },
                            EffectConstraint::Credit {
                                to: resource.clone(),
                                asset: principal.clone(),
                                minimum_credit: quantity(cap),
                            },
                            EffectConstraint::OriginMovement {
                                from: position_location,
                                to: cash_location,
                                debit_asset: principal.clone(),
                                credit_asset: principal.clone(),
                                maximum_debit: quantity(added),
                                minimum_credit: quantity(cap),
                            },
                        ]);
                        effects.push(ProjectedEffect::PositionDelta {
                            resource: resource.clone(),
                            position: name,
                            asset: principal.clone(),
                            position_kind: kind,
                            minimum_delta: SignedQuantity::positive(&quantity(cap)),
                            maximum_delta: SignedQuantity::positive(&quantity(added)),
                        });
                        add_cash(&mut effects, &resource, &principal, cap, cap, true);
                    }
                    "aave.repay" => {
                        require(
                            all && current_min > U256::ZERO && current_max <= cap,
                            "PLAN_CLOSE_UNBOUNDED",
                            "Conditional debt cleanup requires bounded repay all and a funded prior loan.",
                        )?;
                        let source = source_position(context, &resource, &name, &kind)?;
                        require(
                            u256(&cash(context, &resource, &principal)?.minimum)? >= cap,
                            "PLAN_PRINCIPAL_UNFUNDED",
                            "Selected proceeds must cover the complete repayment cap and interest.",
                        )?;
                        finite_allowance(
                            context,
                            &resource,
                            &token.reference,
                            &self.aave()?.pool,
                            cap,
                        )?;
                        requires.push(ProjectedRequirement::CashAtLeast {
                            resource: resource.clone(),
                            asset: principal.clone(),
                            amount: quantity(cap),
                            source_step: requirement_cash_source(
                                context, &resource, &principal, cap,
                            )?,
                        });
                        requires.push(ProjectedRequirement::PositionAtLeast {
                            resource: resource.clone(),
                            position: name.clone(),
                            asset: principal.clone(),
                            position_kind: kind.clone(),
                            amount: quantity(current_min),
                            source_step: Some(source),
                        });
                        intent.debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: amount(&principal, cap),
                        });
                        intent.expected.extend([
                            EffectConstraint::Debit {
                                resource: resource.clone(),
                                asset: principal.clone(),
                                maximum_debit: quantity(cap),
                            },
                            EffectConstraint::Debt {
                                resource: resource.clone(),
                                debt: name.clone(),
                                asset: principal.clone(),
                                direction: Direction::Decrease,
                                maximum_change: quantity(cap),
                            },
                            EffectConstraint::OriginMovement {
                                from: cash_location,
                                to: position_location,
                                debit_asset: principal.clone(),
                                credit_asset: principal.clone(),
                                maximum_debit: quantity(cap),
                                minimum_credit: Quantity::from_u64(1),
                            },
                        ]);
                        // Full ordinary repay previews conservatively bind this
                        // IncludedInDebit cost ceiling; it is not an extra cash debit.
                        intent.maximum_costs.push(CostBound {
                            location: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            amount: amount(&principal, cap),
                            treatment: CostTreatment::IncludedInDebit,
                        });
                        add_cash(&mut effects, &resource, &principal, current_min, cap, false);
                        effects.push(ProjectedEffect::PositionClose {
                            resource: resource.clone(),
                            position: name,
                            asset: principal.clone(),
                            position_kind: kind,
                            maximum_amount: quantity(cap),
                            source_step: source,
                        });
                        evidence["close_condition"] = json!(
                            "At actual fresh preview and receipt, complete debt plus interest must remain within finite max_amount and end at zero scaled/physical debt."
                        );
                    }
                    "aave.withdraw" => {
                        require(
                            all && current_min > U256::ZERO && current_max <= cap,
                            "PLAN_CLOSE_UNBOUNDED",
                            "Conditional supply cleanup requires bounded protocol full withdrawal.",
                        )?;
                        require(
                            u256(&reserve.liquidity)? >= cap,
                            "LIQUIDITY_INSUFFICIENT",
                            "Current reserve lacks the entire finite return cap.",
                        )?;
                        require(
                            address(r.recipient.as_deref().expect("schema"))? == owner,
                            "PLAN_RECIPIENT_UNSUPPORTED",
                            "Full test-position close must return to its selected wallet.",
                        )?;
                        require(
                            context
                                .projected
                                .positions
                                .iter()
                                .filter(|p| {
                                    p.resource == resource
                                        && p.position_kind != PositionKind::Supply
                                })
                                .all(|p| p.amount.maximum.is_zero()),
                            "PLAN_DEBT_UNCLOSED",
                            "Debt must be completely closed before returning collateral.",
                        )?;
                        let source = source_position(context, &resource, &name, &kind)?;
                        requires.push(ProjectedRequirement::PositionAtLeast {
                            resource: resource.clone(),
                            position: name.clone(),
                            asset: principal.clone(),
                            position_kind: kind.clone(),
                            amount: quantity(current_min),
                            source_step: Some(source),
                        });
                        intent.expected.extend([
                            EffectConstraint::Position {
                                resource: resource.clone(),
                                position: name.clone(),
                                asset: principal.clone(),
                                direction: Direction::Decrease,
                                maximum_change: quantity(cap),
                            },
                            EffectConstraint::Credit {
                                to: resource.clone(),
                                asset: principal.clone(),
                                minimum_credit: quantity(current_min),
                            },
                            EffectConstraint::OriginMovement {
                                from: position_location,
                                to: cash_location,
                                debit_asset: principal.clone(),
                                credit_asset: principal.clone(),
                                maximum_debit: quantity(cap),
                                minimum_credit: quantity(current_min),
                            },
                        ]);
                        effects.push(ProjectedEffect::PositionClose {
                            resource: resource.clone(),
                            position: name,
                            asset: principal.clone(),
                            position_kind: kind,
                            maximum_amount: quantity(cap),
                            source_step: source,
                        });
                        add_cash(&mut effects, &resource, &principal, current_min, cap, true);
                        effects.push(ProjectedEffect::AuthorityValue {
                            resource: resource.clone(),
                            authority: collateral_name,
                            value: "false".into(),
                        });
                        evidence["close_condition"] = json!(
                            "At actual fresh preview/receipt, full physical claim including proved yield must fit max_amount, receive exact backing and end at zero scaled claim."
                        );
                    }
                    "aave.collateral" => {
                        require(
                            current_min > U256::ZERO,
                            "NO_SUPPLIED_POSITION",
                            "Conditional collateral authority requires a broker-proved supplied claim.",
                        )?;
                        let enabled = r.enabled.expect("schema");
                        require(
                            !enabled || reserve.collateral_eligible,
                            "COLLATERAL_UNAVAILABLE",
                            "Reserve cannot enable collateral.",
                        )?;
                        require(
                            enabled
                                || context
                                    .projected
                                    .positions
                                    .iter()
                                    .filter(|p| {
                                        p.resource == resource
                                            && p.position_kind != PositionKind::Supply
                                    })
                                    .all(|p| p.amount.maximum.is_zero()),
                            "PLAN_DEBT_UNCLOSED",
                            "This practical route disables collateral only with no projected debt.",
                        )?;
                        intent.expected.push(EffectConstraint::Authority {
                            resource: resource.clone(),
                            authority: collateral_name.clone(),
                            before: prior_collateral.value.clone(),
                            after: enabled.to_string(),
                        });
                        effects.push(ProjectedEffect::AuthorityValue {
                            resource: resource.clone(),
                            authority: collateral_name,
                            value: enabled.to_string(),
                        });
                    }
                    _ => unreachable!(),
                }
            }
            _ => {
                return Err(error(
                    "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
                    "preflight",
                    "This conditional operation has no reviewed bounded route proof.",
                ));
            }
        }
        Ok(ReturnRouteProof {
            step_index: context.step_index,
            operation: operation.into(),
            input_hash: binding_hash(input)?,
            observed_at: now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(90))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            intent,
            effects,
            requires,
            evidence,
        })
    }
}
