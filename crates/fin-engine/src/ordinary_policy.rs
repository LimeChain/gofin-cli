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
    if cost_attention_pending(record) {
        // An externally incurred overrun is never capped or valued at invented
        // zero for future policy checks. Retain every other approved slice and
        // enlarge only the proved cost slice using the frozen exact price map.
        let unknown = || {
            budget_error(
                "ALLOCATION_COST_UNKNOWN",
                "An unresolved proved cost has no trustworthy exact valuation; writes remain blocked until its original evidence and pricing are restored.",
            )
        };
        let evidence = record
            .last_reconciliation_evidence
            .as_ref()
            .ok_or_else(unknown)?;
        let verified: VerifiedEffects =
            from_value(evidence["verified_effects"].clone()).map_err(|_| unknown())?;
        let outcome: Status =
            from_value(evidence["reported_status"].clone()).map_err(|_| unknown())?;
        if !matches!(outcome, Status::Completed | Status::Failed) {
            return Err(unknown());
        }
        if let Err(rejection) = validate_effects_mode(record, &verified, &outcome, false)
            && !matches!(
                rejection.code.as_str(),
                "COST_EXCEEDS_RESERVATION"
                    | "EFFECT_EXCEEDS_RESERVATION"
                    | "COST_EXCEEDS_USD_RESERVATION"
            )
        {
            return Err(unknown());
        }
        let valuation = prepared.valuation.as_ref().ok_or_else(unknown)?;
        let actual_cost = actual_cost_usd(&verified.effects, valuation).map_err(|_| unknown())?;
        let approved_cost = valuation.maximum_fee_usd_micros.integer();
        return Ok(charge(prepared)? - &approved_cost + approved_cost.max(actual_cost));
    }
    // A terminal proof with no financial effects proves that no principal,
    // exposure or fees were consumed. Keep its immutable submission identity,
    // but do not charge an unused approval ceiling as rolling expenditure.
    if record.status == Status::Failed
        && record
            .effects
            .as_ref()
            .is_some_and(|proof| proof.effects.is_empty())
    {
        return Ok(BigUint::zero());
    }
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

pub(super) fn actual_effect_charge(
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
                location,
                amount,
                treatment: CostTreatment::IncludedInDebit,
                ..
            } => {
                // Signed net settlements contain gross losses which later gains
                // may offset. Charge those losses unless another typed gross
                // origin already contains them; rebates/positive net settlement
                // cannot erase consumed expenditure.
                let settlement = matches!(location, AccountingLocation::Cash { resource }
                    if effects.iter().any(|effect| matches!(effect, fin_domain::FinancialEffect::Settlement { resource:r, asset, .. } if r==resource && asset==&amount.asset)));
                let gross_origin = effects.iter().any(|effect| match (location, effect) {
                    (
                        AccountingLocation::Cash { resource },
                        fin_domain::FinancialEffect::CashDebit {
                            resource: r,
                            amount: debit,
                        },
                    ) => r == resource && debit.asset == amount.asset,
                    (
                        AccountingLocation::Cash { resource },
                        fin_domain::FinancialEffect::Transfer { from, debit, .. },
                    ) => from == resource && debit.asset == amount.asset,
                    (
                        AccountingLocation::Cash { resource },
                        fin_domain::FinancialEffect::Conversion {
                            resource: r, spent, ..
                        },
                    ) => r == resource && spent.asset == amount.asset,
                    (_, fin_domain::FinancialEffect::OriginMovement { from, debit, .. }) => {
                        from == location && debit.asset == amount.asset
                    }
                    _ => false,
                });
                if settlement && !gross_origin {
                    total += amount_usd(amount, prices)?;
                }
            }
            fin_domain::FinancialEffect::Cost { amount, .. } => {
                total += amount_usd(amount, prices)?;
            }
            fin_domain::FinancialEffect::Transfer { debit, .. } => {
                total += amount_usd(debit, prices)?;
            }
            fin_domain::FinancialEffect::Conversion { spent, .. } => {
                total += amount_usd(spent, prices)?;
            }
            fin_domain::FinancialEffect::ConversionSequence { execution } => {
                for leg in &execution.legs {
                    total += amount_usd(&leg.spent, prices)?;
                }
                for cost in &execution.costs {
                    if cost.treatment != CostTreatment::IncludedInDebit {
                        total += amount_usd(&cost.amount, prices)?;
                    }
                }
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

pub(super) fn amount_usd(
    amount: &fin_domain::Amount,
    prices: &[fin_domain::AssetPrice],
) -> Result<BigUint> {
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

pub(super) fn resources(intent: &FinancialIntent) -> BTreeSet<ResourceId> {
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

fn counted_with_amendment_owner(
    record: &OperationRecord,
    records: &BTreeMap<String, OperationRecord>,
    instant: DateTime<Utc>,
) -> Result<bool> {
    if record.status.terminal()
        && record.submission.is_some()
        && let Some(prepared) = &record.preview
        && let Some(children) = compound_mutations::children(&prepared.intent)?
    {
        for child in children {
            let original = records.get(&child.operation_id).ok_or_else(|| {
                budget_error(
                    "AMENDMENT_OWNER_MISSING",
                    "A compound mutation's original accounting owner is missing.",
                )
            })?;
            if counted(original, instant)? {
                return Ok(true);
            }
        }
    }
    if record.status.terminal()
        && record.submission.is_some()
        && let Some(prepared) = &record.preview
    {
        for group in grouped_amendments::groups(&prepared.intent)? {
            let owner = records.get(&group.operation_id).ok_or_else(|| {
                budget_error(
                    "AMENDMENT_OWNER_MISSING",
                    "The grouped original owner is missing.",
                )
            })?;
            if counted(owner, instant)? {
                return Ok(true);
            }
        }
    }
    if record.status == Status::Completed
        && let Some((original_id, _, _)) = record
            .preview
            .as_ref()
            .and_then(|p| amendments::target(&p.intent))
    {
        let original=records.get(original_id).ok_or_else(||budget_error("AMENDMENT_OWNER_MISSING","A proved amendment's original accounting owner is missing; its allocation cannot expire safely."))?;
        // Positive amendment reserves transfer to the original unresolved order.
        // Preserve the delta beyond24h and through the original final settlement window.
        return counted(original, instant);
    }
    counted(record, instant)
}
fn operation_index(records: Vec<OperationRecord>) -> BTreeMap<String, OperationRecord> {
    records
        .into_iter()
        .map(|record| (record.id.clone(), record))
        .collect()
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
        let instant = self.clock.now();
        let mut count = 0;
        let records = operation_index(self.operations()?);
        for record in records.values() {
            if counted_with_amendment_owner(record, &records, instant)? {
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
        let instant = self.clock.now();
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
        let operation_index = operation_index(operations);
        for record in operation_index.values() {
            // The row's single allocation charge moved atomically to its bridge
            // transfer owner after source proof; never count both identities.
            if owner_operations.contains(&record.id) {
                continue;
            }
            if counted_with_amendment_owner(record, &operation_index, instant)? {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                total += unresolved_consumption::record_charge(&store, record)?;
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
        replacement_parent: Option<&str>,
        recovery_parent: Option<&str>,
    ) -> Result<()> {
        let instant = self.clock.now();
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
        let linked_original = amendments::target(&prepared.intent).map(|(id, _, _)| id);
        let compound_children = compound_mutations::children(&prepared.intent)?;
        let grouped_owners = grouped_amendments::groups(&prepared.intent)?
            .iter()
            .map(|g| g.operation_id.as_str())
            .collect::<Vec<_>>();

        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let snapshot = store.list_records().map_err(map_store)?;
        let mut total = own_charge;
        let records = operation_index(
            snapshot
                .iter()
                .cloned()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?,
        );
        for other in records.values() {
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
                if Some(other.id.as_str()) != replacement_parent
                    && linked_original != Some(other.id.as_str())
                    && !grouped_owners.contains(&other.id.as_str())
                    && !compound_children.is_some_and(|children| {
                        children.iter().any(|child| child.operation_id == other.id)
                    })
                    && !requested_resources.is_disjoint(&resources(&preview.intent))
                {
                    return Err(budget_error(
                        "RESOURCE_RESERVED",
                        "An unresolved operation reserves a resource required by this write.",
                    ));
                }
            }
            if counted_with_amendment_owner(other, &records, instant)? {
                total += unresolved_consumption::record_charge(&store, other)?;
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
        if !grouped_owners.is_empty() || compound_children.is_some() {
            store
                .persist_mutation_batch_reservation(
                    &record.id,
                    &rows,
                    &to_value(record)?,
                    &snapshot,
                    &record.updated_at,
                )
                .map_err(map_store)
        } else if let Some(original_id) = linked_original {
            store
                .persist_amendment_reservation(
                    &record.id,
                    original_id,
                    &rows,
                    &to_value(record)?,
                    &snapshot,
                    &record.updated_at,
                )
                .map_err(map_store)
        } else {
            let persist = if let Some(parent_id) = recovery_parent {
                store.persist_operation_reservation_with_recovery_parent(
                    &record.id,
                    &rows,
                    &to_value(record)?,
                    &snapshot,
                    &record.updated_at,
                    parent_id,
                )
            } else {
                store.persist_operation_reservation_with_parent(
                    &record.id,
                    &rows,
                    &to_value(record)?,
                    &snapshot,
                    &record.updated_at,
                    replacement_parent,
                )
            };
            persist.map_err(map_store)
        }
    }
}

pub(super) fn validate_boundary_candidate_policy(
    prepared: &PreparedOperation,
    operation_id: &str,
    options: &EngineOptions,
    store: &Store,
    instant: DateTime<Utc>,
) -> Result<()> {
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
        let records = operation_index(
            store
                .list_records()
                .map_err(map_store)?
                .into_iter()
                .map(from_value::<OperationRecord>)
                .collect::<Result<Vec<_>>>()?,
        );
        for other in records.values() {
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
            if counted_with_amendment_owner(other, &records, instant)? {
                total += unresolved_consumption::record_charge(store, other)?;
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
