//! Native mutations and new financial effects share one durable proof transaction.
use super::*;
fn invalid(message: &str) -> FinError {
    FinError::new(
        "MUTATION_BATCH_INVALID",
        "reconciliation",
        message,
        "Preserve every original submission and reconcile this batch without replay.",
    )
}
fn has_mutations(intent: &FinancialIntent) -> Result<bool> {
    Ok(!grouped_amendments::groups(intent)?.is_empty()
        || compound_mutations::children(intent)?.is_some())
}
fn difference(
    before: &BTreeMap<String, BigUint>,
    after: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    after
        .iter()
        .filter_map(|(k, n)| {
            let old = before.get(k).cloned().unwrap_or_default();
            (n > &old).then(|| (k.clone(), n - old))
        })
        .collect()
}
impl Engine {
    pub(super) fn remaining_mutation_reservations(
        &self,
        record: &OperationRecord,
        settlements: &[fin_store::grouped_amendments::GroupedSettlement],
    ) -> Result<Vec<ReservationRow>> {
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| invalid("Mutation preparation missing."))?;
        let mut amounts = aggregate_resource_amounts(&cash_reservation_amounts(
            &prepared.intent.debits,
            &prepared.intent.maximum_costs,
        )?)?;
        for group in grouped_amendments::groups(&prepared.intent)? {
            let Some(settled) = settlements
                .iter()
                .find(|s| s.original_id == group.operation_id)
            else {
                continue;
            };
            let old = aggregate_resource_amounts(&group.original_scope.intent.debits)?;
            let new = aggregate_resource_amounts(&group.approved_envelope.intent.debits)?;
            let old_costs = aggregate_cost_bounds(&group.original_scope.intent.maximum_costs)?;
            let new_costs = aggregate_cost_bounds(&group.approved_envelope.intent.maximum_costs)?;
            let own = grouped_amendments::incremental_intent(
                &group.original_scope.intent,
                &group.approved_envelope.intent,
                &difference(&old, &new),
                &difference(&old_costs, &new_costs),
            )?;
            for (k, n) in aggregate_resource_amounts(&cash_reservation_amounts(
                &own.debits,
                &own.maximum_costs,
            )?)? {
                let total = amounts
                    .get_mut(&k)
                    .ok_or_else(|| invalid("The parent reservation omits a grouped increment."))?;
                if *total < n {
                    return Err(invalid(
                        "Grouped transfer exceeds the approved parent reservation.",
                    ));
                }
                *total -= n;
            }
            for row in &settled.remaining_reservations {
                if !row.resource.starts_with("lock:") {
                    *amounts.entry(row.resource.clone()).or_default() += parse_uint(&row.units)?;
                }
            }
        }
        let mut rows = amounts
            .into_iter()
            .filter(|(_, n)| !n.is_zero())
            .map(|(resource, n)| ReservationRow {
                resource,
                units: n.to_string(),
                fee_units: "0".into(),
            })
            .collect::<Vec<_>>();
        rows.extend(
            ordinary_policy::resources(&prepared.intent)
                .iter()
                .map(|r| ReservationRow {
                    resource: format!("lock:{}", r.0),
                    units: "0".into(),
                    fee_units: "0".into(),
                }),
        );
        Ok(rows)
    }
    pub(super) fn commit_mutation_batch_progress(
        &self,
        record: &OperationRecord,
        expected: &Value,
    ) -> Result<bool> {
        let Some(prepared) = &record.preview else {
            return Ok(false);
        };
        if !has_mutations(&prepared.intent)? {
            return Ok(false);
        }
        let groups = self.grouped_settlements(record)?;
        let children = self.compound_child_settlements(record)?.unwrap_or_default();
        let remaining = self.remaining_mutation_reservations(record, &groups)?;
        let verified = record
            .effects
            .as_ref()
            .ok_or_else(|| invalid("Mutation progress evidence missing."))?;
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .commit_mutation_batch_progress(
                &record.id,
                expected,
                &to_value(record)?,
                &to_value(verified)?,
                &to_value(&verified.effects)?,
                &groups,
                &children,
                &remaining,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(true)
    }
    pub(super) fn commit_mutation_batch_terminal(
        &self,
        record: &OperationRecord,
        settlement: &Settlement,
    ) -> Result<bool> {
        if record.submission.is_none() {
            return Ok(false);
        }
        let Some(prepared) = &record.preview else {
            return Ok(false);
        };
        if !has_mutations(&prepared.intent)? {
            return Ok(false);
        }
        let groups = self.grouped_settlements(record)?;
        let children = self.compound_child_settlements(record)?.unwrap_or_default();
        let expected = self.load_record(&record.id)?;
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .commit_mutation_batch_settlement(
                settlement,
                &to_value(&expected)?,
                &groups,
                &children,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(true)
    }
}
