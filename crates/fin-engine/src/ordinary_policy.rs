//! Durable personal policy for ordinary operations. No test-run authority.
use super::*;

fn budget_error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "budget",
        message,
        "Inspect policy and pending operations. Reconcile the original submission; a new request identity does not release its allocation.",
    )
}

fn charge(prepared: &PreparedOperation) -> Result<BigUint> {
    // A verified cancellation-only operation owns no fills, movement or fees.
    // The original order keeps its own allocation and unresolved reservation.
    if cancellation_target(&prepared.intent).is_some()
        && prepared.intent.debits.is_empty()
        && prepared.intent.maximum_costs.is_empty()
    {
        return Ok(BigUint::zero());
    }
    let value = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "An exact USD valuation is required for durable allocation accounting.",
        )
    })?;
    Ok(value.movement_usd_micros.integer()
        + value.maximum_fee_usd_micros.integer()
        + value.additional_allocation_usd_micros.integer())
}

pub(super) fn record_charge(record: &OperationRecord) -> Result<BigUint> {
    let Some(prepared) = record.preview.as_ref() else {
        return Err(budget_error(
            "VALUATION_UNAVAILABLE",
            "A historical submission has no recorded valuation.",
        ));
    };
    let bounded = prepared
        .binding
        .get(fin_domain::boundaries::BOUNDARY_METADATA_KEY)
        .is_some()
        || prepared
            .binding
            .get(fin_domain::financial_plans::PLAN_METADATA_KEY)
            .is_some();
    if !bounded || !record.status.terminal() {
        return charge(prepared);
    }
    let exact_plan_input = prepared
        .binding
        .get(fin_domain::financial_plans::PLAN_METADATA_KEY)
        .and_then(|metadata| metadata.get("exact_input"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if exact_plan_input {
        // Generic exact scopes can represent position, debt, collateral, and
        // derivative notional effects that are not cash debits. Keep the
        // approved conservative valuation rather than inferring exposure
        // from one subset of typed effects.
        return charge(prepared);
    }
    let Some(verified) = &record.effects else {
        return Ok(BigUint::zero());
    };
    let valuation = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "A bounded attempt has no recorded pricing evidence.",
        )
    })?;
    actual_effect_charge(&verified.effects, &valuation.prices)
}

fn actual_effect_charge(
    effects: &[fin_domain::FinancialEffect],
    prices: &[fin_domain::AssetPrice],
) -> Result<BigUint> {
    let mut total = BigUint::zero();
    for effect in effects {
        match effect {
            fin_domain::FinancialEffect::CashDebit { amount, .. } => {
                total += amount_usd(amount, prices)?;
            }
            fin_domain::FinancialEffect::Cost {
                treatment: CostTreatment::IncludedInDebit,
                ..
            } => {} // Already included in a typed gross debit.
            fin_domain::FinancialEffect::Cost { amount, .. } => {
                total += amount_usd(amount, prices)?;
            }
            fin_domain::FinancialEffect::Transfer { debit, .. } => {
                total += amount_usd(debit, prices)?;
            }
            fin_domain::FinancialEffect::Conversion { spent, .. } => {
                total += amount_usd(spent, prices)?;
            }
            fin_domain::FinancialEffect::DebtChange { asset, delta, .. }
                if !delta.is_negative() && !delta.is_zero() =>
            {
                // Borrowed principal increases the protected allocation even
                // though it is a credit to the wallet, not a cash debit.
                total += amount_usd(
                    &fin_domain::Amount {
                        asset: asset.clone(),
                        atoms: delta.magnitude(),
                    },
                    prices,
                )?;
            }
            _ => {}
        }
    }
    Ok(total)
}

fn amount_usd(amount: &fin_domain::Amount, prices: &[fin_domain::AssetPrice]) -> Result<BigUint> {
    let price = prices
        .iter()
        .find(|price| price.asset == amount.asset)
        .ok_or_else(|| {
            budget_error(
                "VALUATION_UNAVAILABLE",
                "Verified consumption has no matching recorded USD price.",
            )
        })?;
    Ok(amount
        .atoms
        .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?
        .integer())
}

fn resources(intent: &FinancialIntent) -> BTreeSet<ResourceId> {
    let mut result: BTreeSet<_> = intent.locks.iter().cloned().collect();
    result.extend(intent.debits.iter().map(|v| v.resource.clone()));
    result.extend(intent.maximum_costs.iter().map(|v| match &v.location {
        AccountingLocation::Cash { resource } | AccountingLocation::Position { resource, .. } => {
            resource.clone()
        }
    }));
    result
}

fn counted(record: &OperationRecord, instant: DateTime<Utc>) -> Result<bool> {
    if !record.status.terminal() {
        return Ok(record.preview.is_some() || record.submission.is_some());
    }
    // Submitted fees and turnover remain charged even for proven failed writes.
    Ok(record.submission.is_some()
        && parse_time(&record.updated_at)? > instant - chrono::Duration::hours(24))
}

fn unpriced(prepared: &PreparedOperation) -> bool {
    prepared
        .valuation
        .as_ref()
        .is_some_and(|value| value.prices.iter().any(fin_domain::unpriced::is_estimate))
}

pub(super) fn reject_unpriced_run(prepared: &PreparedOperation) -> Result<()> {
    if unpriced(prepared) {
        return Err(budget_error(
            "UNPRICED_ASSET",
            "Unpriced exchange estimates cannot satisfy a scoped acceptance run's verified USD budget.",
        ));
    }
    Ok(())
}

impl Engine {
    pub fn allocation_unpriced_operations(&self) -> Result<u64> {
        let instant = Utc::now();
        let mut count = 0;
        for record in self.operations()? {
            if counted(&record, instant)? {
                let preview = record.preview.as_ref().ok_or_else(|| budget_error("VALUATION_UNAVAILABLE", "A historical submission has no recorded valuation; preserve and reconcile it."))?;
                if unpriced(preview) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
    pub fn allocation_used_usd_micros(&self) -> Result<String> {
        let mut total = BigUint::zero();
        let instant = Utc::now();
        let (owners, owner_operations, transfers, operations) = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            let operation_rows = store.list_records().map_err(map_store)?;
            let operations = operation_rows
                .into_iter()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?;
            let owners = store.bridge_allocation_owners().map_err(map_store)?;
            let operation_ids = owners
                .iter()
                .map(|row| row.0.clone())
                .collect::<BTreeSet<_>>();
            let transfers = owners
                .iter()
                .map(|row| store.bridge_transfer(&row.1).map_err(map_store))
                .collect::<Result<Vec<_>>>()?;
            (owners, operation_ids, transfers, operations)
        };
        for record in operations {
            // The row's single allocation charge moved atomically to its bridge
            // transfer owner after source proof; never count both identities.
            if owner_operations.contains(&record.id) {
                continue;
            }
            if counted(&record, instant)? {
                total += record_charge(&record)?;
            }
        }
        for (owner, transfer) in owners.iter().zip(transfers.iter()) {
            let unresolved = transfer.as_ref().is_some_and(|record| {
                !matches!(
                    record.state,
                    fin_domain::BridgeTransferState::Completed
                        | fin_domain::BridgeTransferState::Refunded
                        | fin_domain::BridgeTransferState::Failed
                )
            });
            let within_window = owner.4.as_deref().is_some_and(|timestamp| {
                parse_time(timestamp)
                    .map(|settled| settled > instant - chrono::Duration::hours(24))
                    .unwrap_or(true)
            });
            if unresolved || within_window {
                total += parse_uint(&owner.2)?;
            }
        }
        Ok(total.to_string())
    }

    pub(super) fn reserve_ordinary_operation(
        &self,
        record: &mut OperationRecord,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let instant = Utc::now();
        let movement_limit =
            fin_domain::policy::limit(&self.options.max_movement_and_cost_usd_micros)?;
        let allocation_limit =
            fin_domain::policy::limit(&self.options.max_run_allocation_usd_micros)?;
        if unpriced(prepared) && !self.options.allow_unpriced_exchange_assets {
            return Err(FinError::new(
                "UNPRICED_EXCHANGE_ASSET_DISABLED",
                "budget",
                "This operation includes an unpriced exchange asset. The protected policy exception is disabled.",
                "The human may explicitly enable --allow-unpriced-exchange-assets true through fin policy set in their own Terminal after reviewing the USD coverage warning. Then obtain a fresh preview and per-write Touch ID approval.",
            ));
        }
        if let Some(valuation) = &prepared.valuation {
            let assets = valuation
                .prices
                .iter()
                .map(|price| price.asset.clone())
                .collect();
            validate_price_rows(&valuation.prices, &assets, instant)?;
        }
        // Zero removes personal ceilings, not the exact valuation/accounting contract.
        validate_cost_bound_contract(&prepared.intent)?;
        validate_cost_prices(prepared, &prepared_cost_assets(&prepared.intent), instant)?;
        let own_charge = charge(prepared)?;
        let valuation = prepared.valuation.as_ref().unwrap();
        validate_price_observation(
            &valuation.observed_at,
            &valuation.source,
            instant,
            "VALUATION_UNAVAILABLE",
        )?;
        if valuation.source.trim().is_empty() {
            return Err(budget_error(
                "VALUATION_UNAVAILABLE",
                "An attributable USD valuation is required.",
            ));
        }
        if movement_limit.is_some_and(|limit| {
            valuation.movement_usd_micros.integer() + valuation.maximum_fee_usd_micros.integer()
                > BigUint::from(limit)
        }) {
            return Err(budget_error(
                "MOVEMENT_LIMIT",
                "Movement and maximum execution costs exceed the personal per-movement limit.",
            ));
        }
        let requested_resources = resources(&prepared.intent);
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let snapshot = store.list_records().map_err(map_store)?;
        let mut total = own_charge;
        for value in &snapshot {
            let other: OperationRecord = from_value(value.clone())?;
            if other.id == record.id {
                continue;
            }
            if !other.status.terminal() {
                let Some(preview) = other.preview.as_ref() else {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation has no inspectable resource reservation.",
                    ));
                };
                if !requested_resources.is_disjoint(&resources(&preview.intent)) {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation reserves a resource required by this write.",
                    ));
                }
            }
            if counted(&other, instant)? {
                total += record_charge(&other)?;
            }
        }
        if allocation_limit.is_some_and(|limit| total > BigUint::from(limit)) {
            return Err(budget_error(
                "ALLOCATION_LIMIT",
                "This write exceeds the installation-wide rolling 24-hour allocation, including unresolved submissions.",
            ));
        }
        let mut rows: Vec<_> =
            cash_reservation_amounts(&prepared.intent.debits, &prepared.intent.maximum_costs)?
                .iter()
                .map(|amount| {
                    Ok(ReservationRow {
                        resource: resource_asset_key(amount)?,
                        units: amount.amount.atoms.to_string(),
                        fee_units: "0".into(),
                    })
                })
                .collect::<Result<_>>()?;
        // Aggregate debit and fee entries for the same asset before storing.
        let mut amounts = BTreeMap::<String, BigUint>::new();
        for row in rows.drain(..) {
            *amounts.entry(row.resource).or_default() += parse_uint(&row.units)?;
        }
        rows.extend(amounts.into_iter().map(|(resource, units)| ReservationRow {
            resource,
            units: units.to_string(),
            fee_units: "0".into(),
        }));
        rows.extend(
            requested_resources
                .into_iter()
                .map(|resource| ReservationRow {
                    resource: format!("lock:{}", resource.0),
                    units: "0".into(),
                    fee_units: "0".into(),
                }),
        );
        record.preview = Some(prepared.clone());
        record.updated_at = now();
        store
            .persist_operation_reservation(
                &record.id,
                &rows,
                &to_value(record)?,
                &snapshot,
                &record.updated_at,
            )
            .map_err(map_store)
    }
}

pub(super) fn validate_boundary_candidate_policy(
    prepared: &PreparedOperation,
    operation_id: &str,
    options: &EngineOptions,
    store: &Store,
) -> Result<()> {
    let instant = Utc::now();
    if unpriced(prepared) && !options.allow_unpriced_exchange_assets {
        return Err(budget_error(
            "UNPRICED_EXCHANGE_ASSET_DISABLED",
            "This operation includes an unpriced exchange asset and the protected exception is disabled.",
        ));
    }
    let valuation = prepared.valuation.as_ref().ok_or_else(|| {
        budget_error(
            "VALUATION_UNAVAILABLE",
            "Fresh USD valuation is required for every boundary attempt.",
        )
    })?;
    validate_price_observation(
        &valuation.observed_at,
        &valuation.source,
        instant,
        "VALUATION_UNAVAILABLE",
    )?;
    validate_cost_bound_contract(&prepared.intent)?;
    validate_cost_prices(prepared, &prepared_cost_assets(&prepared.intent), instant)?;
    let movement_and_fee =
        valuation.movement_usd_micros.integer() + valuation.maximum_fee_usd_micros.integer();
    if fin_domain::policy::limit(&options.max_movement_and_cost_usd_micros)?
        .is_some_and(|limit| movement_and_fee > BigUint::from(limit))
    {
        return Err(budget_error(
            "MOVEMENT_LIMIT",
            "Fresh movement and cost valuation exceeds the current protected per-write limit.",
        ));
    }
    let allocation_limit = fin_domain::policy::limit(&options.max_run_allocation_usd_micros)?;
    if allocation_limit.is_some() {
        let requested_resources = resources(&prepared.intent);
        let mut total = charge(prepared)?;
        for value in store.list_records().map_err(map_store)? {
            let other: OperationRecord = from_value(value)?;
            if other.id == operation_id {
                continue;
            }
            if !other.status.terminal() {
                let Some(previous) = other.preview.as_ref() else {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation has no inspectable resource reservation.",
                    ));
                };
                if !requested_resources.is_disjoint(&resources(&previous.intent)) {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation reserves a resource required by this write.",
                    ));
                }
            }
            if counted(&other, instant)? {
                total += record_charge(&other)?;
            }
        }
        if allocation_limit.is_some_and(|limit| total > BigUint::from(limit)) {
            return Err(budget_error(
                "ALLOCATION_LIMIT",
                "Fresh valuation exceeds the current rolling 24-hour allocation.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::{AccountingLocation, Amount, AssetId, Quantity, ResourceId, SignedQuantity};

    fn asset() -> AssetId {
        AssetId {
            network: "synthetic".into(),
            reference: "USD".into(),
            decimals: 0,
        }
    }

    fn amount(value: &str) -> Amount {
        Amount {
            asset: asset(),
            atoms: Quantity::new(value).unwrap(),
        }
    }

    fn prices() -> Vec<fin_domain::AssetPrice> {
        vec![fin_domain::AssetPrice {
            asset: asset(),
            usd_micros_per_unit: Quantity::new("1").unwrap(),
            unit_atoms: Quantity::new("1").unwrap(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic test price".into(),
        }]
    }

    #[test]
    fn protected_allocation_counts_cash_once_and_keeps_borrowed_principal() {
        let cash = ResourceId("synthetic:wallet".into());
        let market = ResourceId("synthetic:market".into());
        let transfer_provenance = fin_domain::FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: cash.clone(),
            },
            to: AccountingLocation::Position {
                resource: market.clone(),
                position: "supply".into(),
                position_kind: fin_domain::PositionKind::Supply,
            },
            debit: amount("100"),
            credit: amount("100"),
        };
        let cash_effects = vec![
            fin_domain::FinancialEffect::CashDebit {
                resource: cash.clone(),
                amount: amount("100"),
            },
            transfer_provenance,
        ];
        assert_eq!(
            actual_effect_charge(&cash_effects, &prices()).unwrap(),
            BigUint::from(100u32)
        );

        let borrowed = vec![
            fin_domain::FinancialEffect::DebtChange {
                resource: market,
                debt: "variableDebtUSD".into(),
                asset: asset(),
                delta: SignedQuantity::new("75").unwrap(),
                anchor: None,
            },
            fin_domain::FinancialEffect::UsableCredit {
                to: cash,
                amount: amount("75"),
            },
        ];
        assert_eq!(
            actual_effect_charge(&borrowed, &prices()).unwrap(),
            BigUint::from(75u32)
        );
    }

    #[test]
    fn included_repayment_fee_is_not_charged_twice() {
        let resource = ResourceId("synthetic:wallet".into());
        let effects = vec![
            fin_domain::FinancialEffect::CashDebit {
                resource: resource.clone(),
                amount: amount("100"),
            },
            fin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: amount("5"),
                category: "included repayment fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ];
        assert_eq!(
            actual_effect_charge(&effects, &prices()).unwrap(),
            BigUint::from(100u32)
        );
    }
}
