//! Conditional, selected-origin return proofs. These never submit an order.
use super::*;

fn route_error(code: &str, message: &str) -> GoFinError {
    error(code, "preflight", message)
}

fn cash_interval<'a>(
    context: &'a ReturnRouteContext,
    resource: &ResourceId,
    asset: &AssetId,
) -> Result<Option<&'a gofin_domain::QuantityInterval>, GoFinError> {
    let rows: Vec<_> = context
        .projected
        .cash
        .iter()
        .filter(|row| row.resource == *resource && row.asset == *asset)
        .collect();
    match rows.as_slice() {
        [] => Ok(None),
        [row] if row.amount.minimum.integer() <= row.amount.maximum.integer() => {
            Ok(Some(&row.amount))
        }
        _ => Err(route_error(
            "PLAN_STATE_INVALID",
            "Projected cash identity or interval is ambiguous.",
        )),
    }
}

fn cash_requirement(
    context: &ReturnRouteContext,
    resource: &ResourceId,
    asset: &AssetId,
    amount: Quantity,
) -> Result<ProjectedRequirement, GoFinError> {
    let minimum = cash_interval(context, resource, asset)?
        .map(|row| row.minimum.integer())
        .unwrap_or_default();
    if minimum < amount.integer() {
        return Err(route_error(
            "PLAN_FUNDS_UNAVAILABLE",
            "Projected selected cash cannot cover the order, loss and fee reservations.",
        ));
    }
    // Prefer selected starting cash when earlier worst-case debits leave enough.
    // The shared engine independently replays the exact selected provenance.
    let mut baseline_remaining = context
        .allocations
        .iter()
        .filter(|row| row.resource == *resource && row.amount.asset == *asset)
        .fold(BigInt::zero(), |sum, row| {
            sum + BigInt::from(row.amount.atoms.integer())
        });
    for proof in &context.prior_proofs {
        for effect in &proof.effects {
            match effect {
                ProjectedEffect::CashDelta {
                    resource: place,
                    asset: coin,
                    minimum_delta,
                    ..
                } if place == resource && coin == asset && minimum_delta.is_negative() => {
                    baseline_remaining += minimum_delta.integer()
                }
                ProjectedEffect::CashSweep {
                    resource: place,
                    asset: coin,
                    ..
                } if place == resource && coin == asset => baseline_remaining = BigInt::zero(),
                _ => {}
            }
        }
    }
    let source_step = if baseline_remaining >= BigInt::from(amount.integer()) {
        None
    } else {
        Some(context.prior_proofs.iter().rev().find(|proof| proof.step_index < context.step_index && proof.effects.iter().any(|effect| {
            matches!(effect, ProjectedEffect::CashDelta {resource:place, asset:coin, maximum_delta, ..} if place == resource && coin == asset && !maximum_delta.is_negative() && !maximum_delta.is_zero())
        })).map(|proof| proof.step_index).ok_or_else(|| route_error("PLAN_SOURCE_UNATTRIBUTED", "Projected cash is not attributable to selected starting funds or a prior proved credit."))?)
    };
    Ok(ProjectedRequirement::CashAtLeast {
        resource: resource.clone(),
        asset: asset.clone(),
        amount,
        source_step,
    })
}

fn sweep_source(
    context: &ReturnRouteContext,
    resource: &ResourceId,
    asset: &AssetId,
    cap: &Quantity,
) -> Result<u32, GoFinError> {
    if context.baseline.state.holdings.iter().any(|row| {
        row.resource == *resource && row.amount.asset == *asset && !row.amount.atoms.is_zero()
    }) {
        return Err(route_error(
            "PLAN_UNRELATED_HOLDINGS",
            "Quantity all cannot sell an asset held at baseline; select a separate account or an explicit quantity.",
        ));
    }
    let interval = cash_interval(context, resource, asset)?.ok_or_else(|| {
        route_error(
            "PLAN_SOURCE_UNATTRIBUTED",
            "Cleanup has no projected selected asset inventory.",
        )
    })?;
    if interval.maximum.integer() > cap.integer() {
        return Err(route_error(
            "CLEANUP_BOUND",
            "Projected cleanup inventory exceeds maximum_quantity.",
        ));
    }
    context.prior_proofs.iter().rev().find(|proof| proof.step_index < context.step_index && proof.effects.iter().any(|effect| {
        matches!(effect, ProjectedEffect::CashDelta {resource:place,asset:coin,maximum_delta,..} if place==resource && coin==asset && !maximum_delta.is_negative() && !maximum_delta.is_zero() && maximum_delta.magnitude().integer() >= interval.maximum.integer())
    })).map(|proof| proof.step_index).ok_or_else(|| route_error("PLAN_SOURCE_UNATTRIBUTED", "Cleanup inventory must originate in one prior selected acquisition."))
}

fn position_interval<'a>(
    context: &'a ReturnRouteContext,
    resource: &ResourceId,
    position: &str,
    asset: &AssetId,
    kind: &PositionKind,
) -> Result<Option<&'a gofin_domain::QuantityInterval>, GoFinError> {
    let rows: Vec<_> = context
        .projected
        .positions
        .iter()
        .filter(|row| {
            row.resource == *resource
                && row.position == position
                && row.asset == *asset
                && row.position_kind == *kind
        })
        .collect();
    match rows.as_slice() {
        [] => Ok(None),
        [row] if row.amount.minimum.integer() <= row.amount.maximum.integer() => {
            Ok(Some(&row.amount))
        }
        _ => Err(route_error(
            "PLAN_STATE_INVALID",
            "Projected derivative exposure is ambiguous.",
        )),
    }
}

fn closing_source(
    context: &ReturnRouteContext,
    resource: &ResourceId,
    position: &str,
    asset: &AssetId,
    kind: &PositionKind,
    cap: &Quantity,
) -> Result<u32, GoFinError> {
    if context.baseline.state.positions.iter().any(|row| {
        row.resource == *resource
            && row.position == position
            && row.amount.asset == *asset
            && row.kind == *kind
            && !row.amount.atoms.is_zero()
    }) {
        return Err(route_error(
            "PLAN_UNRELATED_EXPOSURE",
            "Scoped cleanup cannot close a derivative position held at baseline.",
        ));
    }
    let interval =
        position_interval(context, resource, position, asset, kind)?.ok_or_else(|| {
            route_error(
                "PLAN_SOURCE_UNATTRIBUTED",
                "Cleanup has no projected selected derivative exposure.",
            )
        })?;
    if interval.maximum.integer() > cap.integer() {
        return Err(route_error(
            "CLEANUP_BOUND",
            "Projected derivative exposure exceeds maximum_quantity.",
        ));
    }
    context.prior_proofs.iter().rev().find(|proof| proof.step_index < context.step_index && proof.effects.iter().any(|effect| {
        matches!(effect, ProjectedEffect::PositionDelta {resource:place,position:name,asset:coin,position_kind:side,maximum_delta,..} if place==resource && name==position && coin==asset && side==kind && !maximum_delta.is_negative() && !maximum_delta.is_zero() && maximum_delta.magnitude().integer() >= interval.maximum.integer())
    })).map(|proof| proof.step_index).ok_or_else(|| route_error("PLAN_SOURCE_UNATTRIBUTED", "The selected derivative exposure has no prior opening proof."))
}

fn maximum_market_price(binding: &Value) -> Result<ExactDecimal, GoFinError> {
    let filters = binding["marketFilters"].as_array().ok_or_else(|| {
        route_error(
            "MARKET_FILTERS_MISSING",
            "Reviewed market filters are unavailable.",
        )
    })?;
    let filters: Vec<_> = filters
        .iter()
        .filter(|row| row["filterType"] == "PRICE_FILTER")
        .collect();
    let row = match filters.as_slice() {
        [row] => *row,
        _ => {
            return Err(route_error(
                "MARKET_FILTERS_MISSING",
                "The market price ceiling is missing or ambiguous.",
            ));
        }
    };
    positive_text(row, "maxPrice")
}

impl BinanceProfile {
    pub(in crate::adapter) async fn trading_preflight(
        &self,
        operation: &str,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, GoFinError> {
        if !is_write(operation) {
            return Err(route_error(
                "RETURN_ROUTE_UNSUPPORTED",
                "The operation is not an implemented Binance trading write.",
            ));
        }
        let input: OrderInput = self.parse_input(raw).await?;
        let futures = operation == order_name(true);
        if !futures && !matches!(input.time_in_force, ImmediateTime::Fok) {
            return Err(route_error(
                "RETURN_ROUTE_PARTIAL_FILL_UNSUPPORTED",
                "Scoped SPOT return plans currently require FOK; partial-fill IOC conversion provenance is not implemented.",
            ));
        }
        let account = self.account(&input.account)?;
        let resource = if futures {
            futures_resource(account)
        } else {
            account.resource.clone()
        };
        if !context.baseline.complete || !context.baseline.coverage.contains(&resource.0) {
            return Err(route_error(
                "PLAN_BASELINE_INCOMPLETE",
                "Capture complete baseline coverage for the selected trading account and wallet before planning.",
            ));
        }
        let prepared = self.trading_prepare(operation, raw, Some(context)).await?;
        let base: AssetId =
            serde_json::from_value(prepared.binding["base"].clone()).map_err(|_| {
                route_error(
                    "PREVIEW_INVALID",
                    "Prepared market base identity is invalid.",
                )
            })?;
        let quote: AssetId =
            serde_json::from_value(prepared.binding["quote"].clone()).map_err(|_| {
                route_error(
                    "PREVIEW_INVALID",
                    "Prepared market quote identity is invalid.",
                )
            })?;
        let quantity = amount(prepared.binding["quantity"].as_str().ok_or_else(|| {
            route_error("PREVIEW_INVALID", "Prepared order quantity is invalid.")
        })?)?;
        let price = amount(&input.limit_price)?;
        let quantity_atoms = decimal_to_atoms(&quantity, base.decimals)?;
        let notional = decimal_to_atoms(&product(&quantity, &price)?, quote.decimals)?;
        let cleanup = input.quantity == "all";
        let mut requirements = BTreeMap::<AssetId, BigUint>::new();
        for debit in &prepared.intent.debits {
            if debit.resource != resource {
                return Err(route_error(
                    "PREVIEW_INVALID",
                    "Trading debit targets another physical account.",
                ));
            }
            // A sweep consumes the actual prior selected inventory rather than
            // requiring its maximum to be guaranteed in every projected branch.
            if !(cleanup && !futures && debit.amount.asset == base) {
                *requirements.entry(debit.amount.asset.clone()).or_default() +=
                    debit.amount.atoms.integer();
            }
        }
        for cost in &prepared.intent.maximum_costs {
            if cost.location
                != (AccountingLocation::Cash {
                    resource: resource.clone(),
                })
            {
                return Err(route_error(
                    "PREVIEW_INVALID",
                    "Trading costs target another account.",
                ));
            }
            if cost.treatment == CostTreatment::AdditionalDebit {
                *requirements.entry(cost.amount.asset.clone()).or_default() +=
                    cost.amount.atoms.integer();
            }
        }
        let zero = SignedQuantity::positive(&Quantity::zero());
        let mut effects = Vec::new();
        let mut requires = Vec::new();
        if futures {
            let kind: PositionKind =
                serde_json::from_value(prepared.binding["positionKind"].clone()).map_err(|_| {
                    route_error("PREVIEW_INVALID", "Prepared derivative side is invalid.")
                })?;
            let position = position_name(&input.symbol, &kind);
            if !input.reduce_only {
                if context.projected.positions.iter().any(|row| {
                    row.resource == resource
                        && matches!(
                            row.position_kind,
                            PositionKind::DerivativeLong | PositionKind::DerivativeShort
                        )
                        && !row.amount.maximum.is_zero()
                }) {
                    return Err(route_error(
                        "EXISTING_EXPOSURE",
                        "The beta opening path allows one projected futures position per account; close it before opening another.",
                    ));
                }
                let risk_notional = prepared.binding["riskNotional"].as_str().ok_or_else(|| {
                    route_error("PREVIEW_INVALID", "Prepared risk notional is unavailable.")
                })?;
                *requirements.entry(quote.clone()).or_default() +=
                    decimal_to_atoms(&amount(risk_notional)?, quote.decimals)?.integer();
                effects.push(ProjectedEffect::PositionDelta {
                    resource: resource.clone(),
                    position,
                    asset: base.clone(),
                    position_kind: kind,
                    minimum_delta: zero.clone(),
                    maximum_delta: SignedQuantity::positive(&quantity_atoms),
                });
            } else if cleanup {
                let source_step =
                    closing_source(context, &resource, &position, &base, &kind, &quantity_atoms)?;
                effects.push(ProjectedEffect::PositionClose {
                    resource: resource.clone(),
                    position,
                    asset: base.clone(),
                    position_kind: kind,
                    maximum_amount: quantity_atoms.clone(),
                    source_step,
                });
            } else {
                let exposure = position_interval(context, &resource, &position, &base, &kind)?
                    .ok_or_else(|| {
                        route_error(
                            "PLAN_EXPOSURE_UNAVAILABLE",
                            "An explicit closing quantity has no projected exposure.",
                        )
                    })?;
                let source_step = closing_source(
                    context,
                    &resource,
                    &position,
                    &base,
                    &kind,
                    &exposure.maximum,
                )?;
                let minimum = exposure.minimum.integer();
                if minimum < quantity_atoms.integer() {
                    return Err(route_error(
                        "PLAN_EXPOSURE_UNAVAILABLE",
                        "An explicit closing quantity exceeds guaranteed projected exposure; use a bounded all cleanup.",
                    ));
                }
                requires.push(ProjectedRequirement::PositionAtLeast {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: base.clone(),
                    position_kind: kind.clone(),
                    amount: quantity_atoms.clone(),
                    source_step: Some(source_step),
                });
                effects.push(ProjectedEffect::PositionDelta {
                    resource: resource.clone(),
                    position,
                    asset: base.clone(),
                    position_kind: kind,
                    minimum_delta: SignedQuantity::negative(&quantity_atoms),
                    maximum_delta: zero.clone(),
                });
            }
            let mut minimum = BigInt::zero();
            let mut maximum = BigInt::zero();
            for constraint in &prepared.intent.expected {
                if let EffectConstraint::Settlement {
                    resource: place,
                    asset,
                    minimum_delta,
                    maximum_delta,
                    ..
                } = constraint
                {
                    if place != &resource || asset != &quote {
                        return Err(route_error(
                            "PREVIEW_INVALID",
                            "Trading settlement targets another account or collateral asset.",
                        ));
                    }
                    minimum += minimum_delta.integer();
                    maximum += maximum_delta.integer();
                }
            }
            effects.push(ProjectedEffect::CashDelta {
                resource: resource.clone(),
                asset: quote.clone(),
                minimum_delta: SignedQuantity::new(minimum.to_string()).map_err(domain_error)?,
                maximum_delta: SignedQuantity::new(maximum.to_string()).map_err(domain_error)?,
            });
        } else {
            let (spent, received, minimum_received, maximum_received) =
                if matches!(input.side, Side::Buy) {
                    (
                        quote.clone(),
                        base.clone(),
                        quantity_atoms.clone(),
                        quantity_atoms.clone(),
                    )
                } else {
                    let (minimum_quantity, maximum_quantity) = if cleanup {
                        let source_step = sweep_source(context, &resource, &base, &quantity_atoms)?;
                        let interval = cash_interval(context, &resource, &base)?
                            .expect("sweep_source checked projected inventory");
                        effects.push(ProjectedEffect::CashSweep {
                            resource: resource.clone(),
                            asset: base.clone(),
                            maximum_amount: quantity_atoms.clone(),
                            source_step,
                        });
                        (
                            observed_decimal(&interval.minimum.to_decimal(base.decimals))?,
                            observed_decimal(&interval.maximum.to_decimal(base.decimals))?,
                        )
                    } else {
                        (quantity.clone(), quantity.clone())
                    };
                    (
                        base.clone(),
                        quote.clone(),
                        decimal_to_atoms(&product(&minimum_quantity, &price)?, quote.decimals)?,
                        decimal_to_atoms(
                            &product(&maximum_quantity, &maximum_market_price(&prepared.binding)?)?,
                            quote.decimals,
                        )?,
                    )
                };
            if !cleanup {
                let debit = if matches!(input.side, Side::Buy) {
                    &notional
                } else {
                    &quantity_atoms
                };
                effects.push(ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: spent,
                    minimum_delta: SignedQuantity::negative(debit),
                    maximum_delta: SignedQuantity::negative(debit),
                });
            }
            let deducted = prepared
                .intent
                .maximum_costs
                .iter()
                .filter(|cost| {
                    cost.treatment == CostTreatment::DeductedFromCredit
                        && cost.amount.asset == received
                })
                .fold(BigUint::zero(), |sum, cost| {
                    sum + cost.amount.atoms.integer()
                });
            if minimum_received.integer() <= deducted {
                return Err(route_error(
                    "PLAN_RECEIPT_UNFUNDED",
                    "The successful FOK branch must leave a positive exact output after maximum commission.",
                ));
            }
            effects.push(ProjectedEffect::CashDelta {
                resource: resource.clone(),
                asset: received,
                minimum_delta: SignedQuantity::positive(&minimum_received),
                maximum_delta: SignedQuantity::positive(&maximum_received),
            });
        }
        for (asset, atoms) in requirements {
            if !atoms.is_zero() {
                requires.push(cash_requirement(
                    context,
                    &resource,
                    &asset,
                    Quantity::new(atoms.to_string()).map_err(domain_error)?,
                )?);
            }
        }
        if prepared.valuation.as_ref().is_some_and(|valuation| {
            valuation.maximum_fee_usd_micros.integer()
                > context.remaining_cost_budget_usd_micros.integer()
        }) {
            return Err(route_error(
                "RETURN_ROUTE_COST_BUDGET",
                "Order loss and fee ceilings exceed the remaining test cost reserve.",
            ));
        }
        Ok(ReturnRouteProof {
            step_index: context.step_index,
            operation: operation.into(),
            input_hash: binding_hash(raw).map_err(domain_error)?,
            observed_at: prepared.observed_at,
            expires_at: prepared.expires_at,
            intent: prepared.intent,
            effects,
            requires,
            evidence: json!({"method":"binance-conditional-immediate-order-v1","resource":resource,"symbol":input.symbol,"scope":if futures {"USD-M perpetual"} else {"SPOT FOK"},"quantity":quantity.canonical(),"limitPrice":price.canonical(),"cleanup":cleanup,
                "projection":if futures {"Exposure 0 through maximum; realized collateral settlement bounded independently. Cleanup uses the prior selected position identity."} else {"Conditional successful FOK branch at the worst approved input price; no-fill leaves the original balance, and price improvement can require route refresh."},
                "fees":"Shared engine subtracts each exact maximum cost once from these gross projected effects.","execution":"Every actual order requires a fresh preview, Touch ID, durable client identity and verified fills; this proof does not submit or approve an order."}),
        })
    }
}
