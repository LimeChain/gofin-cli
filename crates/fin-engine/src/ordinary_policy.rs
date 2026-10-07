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

impl Engine {
    pub fn allocation_used_usd_micros(&self) -> Result<String> {
        let mut total = BigUint::zero();
        for record in self.operations()? {
            if counted(&record, Utc::now())? {
                total += charge(record.preview.as_ref().ok_or_else(|| budget_error("VALUATION_UNAVAILABLE", "A historical submission has no recorded valuation; preserve and reconcile it."))?)?;
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
        // Zero removes personal ceilings, not the exact valuation/accounting contract.
        validate_cost_bound_contract(&prepared.intent)?;
        validate_cost_prices(prepared, &prepared_cost_assets(&prepared.intent), instant)?;
        let own_charge = charge(prepared)?;
        let valuation = prepared.valuation.as_ref().unwrap();
        validate_observation(&valuation.observed_at, instant, "VALUATION_UNAVAILABLE")?;
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
                total += charge(other.preview.as_ref().ok_or_else(|| {
                    budget_error(
                        "VALUATION_UNAVAILABLE",
                        "A historical submission has no recorded valuation.",
                    )
                })?)?;
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
