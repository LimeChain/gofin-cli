//! Exact source slices for cash reservations and order-preserving refunds.
//!
//! Source indices refer to the immutable pre-selection vector, including
//! repeated labels. Neither a refund nor a partial receipt may reorder lots.

use super::{ProvenanceLot, RunLedger, RunReservation};
use gofin_domain::{
    AccountingLocation, CostTreatment, FinancialIntent, GoFinError, ResourceAmount, Result,
};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct CashReservationSources {
    pub before: Vec<ProvenanceLot>,
    pub principal_indices: Vec<usize>,
    pub additional_cost_indices: Vec<usize>,
}

pub(super) struct CashProvenanceSelection {
    pub remaining: Vec<ProvenanceLot>,
    pub principal: Vec<ProvenanceLot>,
    pub additional_costs: Vec<ProvenanceLot>,
    pub sources: CashReservationSources,
}

pub(super) struct CashProvenanceSettlement {
    pub remaining: Vec<ProvenanceLot>,
    pub principal: Vec<ProvenanceLot>,
    pub additional_costs: Vec<ProvenanceLot>,
}

/// Select from the whole source, preserving slots until both passes finish.
pub(super) fn select(run: &RunLedger, intent: &FinancialIntent) -> Result<CashProvenanceSelection> {
    let mut remaining = run.provenance.clone();
    let selected_origins = run
        .allocations
        .iter()
        .map(|allocation| {
            super::resource_asset_key(&allocation.holding).map(|key| format!("allocation:{key}"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let mut additional_costs = Vec::new();
    let mut additional_cost_indices = Vec::new();
    for (key, requested) in additional_caps(intent)? {
        let (resource, asset) = super::decode_resource_asset_key(&key)?;
        let (slices, indices) = take_indexed(
            &mut remaining,
            &AccountingLocation::Cash {
                resource: resource.clone(),
            },
            &asset,
            &requested,
            |lot| u8::from(!super::cash_origin_is_home(&lot.origin, &resource, &asset)),
            |lot| selected_origins.contains(&lot.origin),
        )?;
        additional_costs.extend(slices);
        additional_cost_indices.extend(indices);
    }
    let mut principal = Vec::new();
    let mut principal_indices = Vec::new();
    for (key, requested) in normalized_amounts(&intent.debits)? {
        let (resource, asset) = super::decode_resource_asset_key(&key)?;
        let preference = super::cash_principal_preference(intent, &resource, &asset)?;
        let (slices, indices) = take_indexed(
            &mut remaining,
            &AccountingLocation::Cash { resource },
            &asset,
            &requested,
            |lot| preference.priority(&lot.origin),
            |_| true,
        )?;
        principal.extend(slices);
        principal_indices.extend(indices);
    }
    remove_empty(&mut remaining)?;
    Ok(CashProvenanceSelection {
        remaining,
        principal,
        additional_costs,
        sources: CashReservationSources {
            before: run.provenance.clone(),
            principal_indices,
            additional_cost_indices,
        },
    })
}

/// Reconstruct original ordering minus the exact principal and fee slices used.
pub(super) fn settle(
    reservation: &RunReservation,
    current: &[ProvenanceLot],
    _intent: &FinancialIntent,
    actual_principal: &BTreeMap<String, BigUint>,
    actual_total: &BTreeMap<String, BigUint>,
) -> Result<CashProvenanceSettlement> {
    let sources = validate(reservation, current)?;
    let mut principal = reservation.provenance.clone();
    let mut fees = reservation.additional_cost_provenance.clone();
    let mut consumed = vec![BigUint::zero(); sources.before.len()];
    let mut used_principal = Vec::new();
    let mut used_fees = Vec::new();
    for (key, requested) in actual_principal {
        if requested > actual_total.get(key).unwrap_or(&BigUint::zero()) {
            return Err(invalid());
        }
        let (resource, asset) = super::decode_resource_asset_key(key).map_err(|_| invalid())?;
        let (slices, indices) = take_indexed(
            &mut principal,
            &AccountingLocation::Cash { resource },
            &asset,
            requested,
            |_| 0,
            |_| true,
        )
        .map_err(|_| invalid())?;
        record_consumed(&mut consumed, &slices, &indices, &sources.principal_indices)?;
        used_principal.extend(slices);
    }
    for (key, total) in actual_total {
        let principal = actual_principal.get(key).cloned().unwrap_or_default();
        if &principal > total {
            return Err(invalid());
        }
        let requested = total - principal;
        let (resource, asset) = super::decode_resource_asset_key(key).map_err(|_| invalid())?;
        let (slices, indices) = take_indexed(
            &mut fees,
            &AccountingLocation::Cash { resource },
            &asset,
            &requested,
            |_| 0,
            |_| true,
        )
        .map_err(|_| invalid())?;
        record_consumed(
            &mut consumed,
            &slices,
            &indices,
            &sources.additional_cost_indices,
        )?;
        used_fees.extend(slices);
    }
    Ok(CashProvenanceSettlement {
        remaining: subtract_indexed(&sources.before, &consumed)?,
        principal: used_principal,
        additional_costs: used_fees,
    })
}

/// An unsubmitted action restores the exact prior vector, including lot order.
pub(super) fn release(
    reservation: &RunReservation,
    current: &[ProvenanceLot],
) -> Result<Vec<ProvenanceLot>> {
    Ok(validate(reservation, current)?.before.clone())
}

fn validate<'a>(
    reservation: &'a RunReservation,
    current: &[ProvenanceLot],
) -> Result<&'a CashReservationSources> {
    let sources = reservation.cash_sources.as_ref().ok_or_else(invalid)?;
    if sources.principal_indices.len() != reservation.provenance.len()
        || sources.additional_cost_indices.len() != reservation.additional_cost_provenance.len()
        || sources
            .principal_indices
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != sources.principal_indices.len()
        || sources
            .additional_cost_indices
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != sources.additional_cost_indices.len()
    {
        return Err(invalid());
    }
    for slice in &reservation.additional_cost_provenance {
        let key = slice
            .origin
            .strip_prefix("allocation:")
            .ok_or_else(invalid)?;
        super::decode_resource_asset_key(key).map_err(|_| invalid())?;
    }
    let mut reserved = vec![BigUint::zero(); sources.before.len()];
    for (slices, indices) in [
        (&reservation.provenance, &sources.principal_indices),
        (
            &reservation.additional_cost_provenance,
            &sources.additional_cost_indices,
        ),
    ] {
        for (slice, index) in slices.iter().zip(indices) {
            let original = sources.before.get(*index).ok_or_else(invalid)?;
            if original.origin != slice.origin
                || original.location != slice.location
                || original.asset != slice.asset
                || !matches!(slice.location, AccountingLocation::Cash { .. })
            {
                return Err(invalid());
            }
            let amount = super::parse_uint(&slice.atoms).map_err(|_| invalid())?;
            if amount.is_zero() {
                return Err(invalid());
            }
            reserved[*index] += amount;
        }
    }
    if slice_amounts(&reservation.provenance)? != normalized_amounts(&reservation.debits)?
        || slice_amounts(&reservation.additional_cost_provenance)?
            != additional_caps(&FinancialIntent {
                maximum_costs: reservation.maximum_costs.clone(),
                ..Default::default()
            })?
        || subtract_indexed(&sources.before, &reserved)? != current
    {
        return Err(invalid());
    }
    Ok(sources)
}

fn record_consumed(
    consumed: &mut [BigUint],
    slices: &[ProvenanceLot],
    slice_indices: &[usize],
    source_indices: &[usize],
) -> Result<()> {
    if slices.len() != slice_indices.len() {
        return Err(invalid());
    }
    for (slice, index) in slices.iter().zip(slice_indices) {
        let original = source_indices.get(*index).ok_or_else(invalid)?;
        let amount = super::parse_uint(&slice.atoms).map_err(|_| invalid())?;
        *consumed.get_mut(*original).ok_or_else(invalid)? += amount;
    }
    Ok(())
}

/// Does not retain/remove slots: indices stay valid across subsequent selections.
fn take_indexed(
    lots: &mut [ProvenanceLot],
    location: &AccountingLocation,
    asset: &gofin_domain::AssetId,
    requested: &BigUint,
    priority: impl Fn(&ProvenanceLot) -> u8,
    eligible: impl Fn(&ProvenanceLot) -> bool,
) -> Result<(Vec<ProvenanceLot>, Vec<usize>)> {
    let mut outstanding = requested.clone();
    let mut slices = Vec::new();
    let mut indices = Vec::new();
    let mut ordered = lots
        .iter()
        .enumerate()
        .filter(|(_, lot)| &lot.location == location && &lot.asset == asset && eligible(lot))
        .map(|(index, lot)| (priority(lot), index))
        .collect::<Vec<_>>();
    ordered.sort_unstable();
    for (_, index) in ordered {
        let lot = &mut lots[index];
        if outstanding.is_zero() {
            break;
        }
        if &lot.location != location || &lot.asset != asset || !eligible(lot) {
            continue;
        }
        let available = super::parse_uint(&lot.atoms)?;
        let amount = available.clone().min(outstanding.clone());
        if amount.is_zero() {
            continue;
        }
        let mut slice = lot.clone();
        slice.atoms = amount.to_string();
        lot.atoms = (available - &amount).to_string();
        outstanding -= amount;
        slices.push(slice);
        indices.push(index);
    }
    if !outstanding.is_zero() {
        return Err(GoFinError::new(
            "PROVENANCE_UNAVAILABLE",
            "budget",
            "Selected source origins cannot fund the exact principal and additional fees.",
            "Reduce the requested bounds or revise the complete funded return path.",
        ));
    }
    Ok((slices, indices))
}

fn subtract_indexed(before: &[ProvenanceLot], amounts: &[BigUint]) -> Result<Vec<ProvenanceLot>> {
    if before.len() != amounts.len() {
        return Err(invalid());
    }
    let mut result = Vec::with_capacity(before.len());
    for (lot, consumed) in before.iter().zip(amounts) {
        let available = super::parse_uint(&lot.atoms).map_err(|_| invalid())?;
        if consumed > &available {
            return Err(invalid());
        }
        if consumed == &available {
            continue;
        }
        let mut remaining = lot.clone();
        if !consumed.is_zero() {
            remaining.atoms = (available - consumed).to_string();
        }
        result.push(remaining);
    }
    Ok(result)
}

fn remove_empty(lots: &mut Vec<ProvenanceLot>) -> Result<()> {
    let mut result = Vec::with_capacity(lots.len());
    for lot in lots.drain(..) {
        if !super::parse_uint(&lot.atoms)?.is_zero() {
            result.push(lot);
        }
    }
    *lots = result;
    Ok(())
}

fn normalized_amounts(rows: &[ResourceAmount]) -> Result<BTreeMap<String, BigUint>> {
    let mut amounts = super::aggregate_resource_amounts(rows)?;
    amounts.retain(|_, amount| !amount.is_zero());
    Ok(amounts)
}

fn slice_amounts(lots: &[ProvenanceLot]) -> Result<BTreeMap<String, BigUint>> {
    let mut amounts = BTreeMap::new();
    for lot in lots {
        let AccountingLocation::Cash { resource } = &lot.location else {
            return Err(invalid());
        };
        let key = super::resource_asset_key(&ResourceAmount {
            resource: resource.clone(),
            amount: gofin_domain::Amount {
                asset: lot.asset.clone(),
                atoms: gofin_domain::Quantity::zero(),
            },
        })?;
        *amounts.entry(key).or_default() += super::parse_uint(&lot.atoms)?;
    }
    Ok(amounts)
}

fn additional_caps(intent: &FinancialIntent) -> Result<BTreeMap<String, BigUint>> {
    let mut amounts = BTreeMap::new();
    for cost in &intent.maximum_costs {
        if cost.treatment != CostTreatment::AdditionalDebit {
            continue;
        }
        let AccountingLocation::Cash { resource } = &cost.location else {
            return Err(invalid());
        };
        let key = super::resource_asset_key(&ResourceAmount {
            resource: resource.clone(),
            amount: cost.amount.clone(),
        })?;
        *amounts.entry(key).or_default() += cost.amount.atoms.integer();
    }
    amounts.retain(|_, amount: &mut BigUint| !amount.is_zero());
    Ok(amounts)
}

fn invalid() -> GoFinError {
    GoFinError::new(
        "RESERVATION_PROVENANCE_INVALID",
        "reconciliation",
        "The reserved source slices do not match their durable ordered source snapshot.",
        "Preserve the original reservation and reconcile its exact source evidence.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{
        Amount, AssetId, CostBound, EffectConstraint, FinancialState, PositionKind, Quantity,
        ResourceId, StateSnapshot,
    };
    use serde_json::json;

    fn asset() -> AssetId {
        AssetId {
            network: "synthetic:isolated".into(),
            reference: "coin".into(),
            decimals: 0,
        }
    }

    fn cash_resource() -> ResourceId {
        ResourceId("synthetic-current-wallet".into())
    }

    fn amount(units: u32) -> ResourceAmount {
        ResourceAmount {
            resource: cash_resource(),
            amount: Amount {
                asset: asset(),
                atoms: Quantity::from_u64(units.into()),
            },
        }
    }

    fn origin(home: &str) -> String {
        format!(
            "allocation:{}",
            super::super::resource_asset_key(&ResourceAmount {
                resource: ResourceId(home.into()),
                amount: Amount {
                    asset: asset(),
                    atoms: Quantity::zero(),
                },
            })
            .unwrap()
        )
    }

    fn lot(home: &str, units: u32) -> ProvenanceLot {
        ProvenanceLot {
            origin: origin(home),
            location: AccountingLocation::Cash {
                resource: cash_resource(),
            },
            asset: asset(),
            atoms: units.to_string(),
        }
    }

    fn run(lots: Vec<ProvenanceLot>) -> RunLedger {
        let selected = ["A", "B"]
            .into_iter()
            .map(|home| super::super::RunAllocation {
                holding: ResourceAmount {
                    resource: ResourceId(home.into()),
                    amount: Amount {
                        asset: asset(),
                        atoms: Quantity::from_u64(100),
                    },
                },
                value_usd_micros: "100".into(),
                observed_at: "2000-01-01T00:00:00Z".into(),
                source: "synthetic allocation fixture".into(),
            })
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "id":"synthetic-cash-slices",
            "status":"active",
            "baseline":StateSnapshot {
                state: FinancialState::default(),
                observed_at:"2000-01-01T00:00:00Z".into(),
                complete:true,
                coverage:vec![cash_resource().0],
                pending:vec![],
            },
            "initial_allocation_usd_micros":"200",
            "borrowed_proceeds_reserve_usd_micros":"100",
            "return_cost_reserve_usd_micros":"100",
            "allocations":selected,
            "available":[],
            "reservations":{},
            "costs":[],
            "spent_cost_usd_micros":"0",
            "borrowed_allocation_used_usd_micros":"0",
            "final_state":null,
            "final_observed_at":null,
            "differences":{},
            "provenance":lots,
        }))
        .unwrap()
    }

    fn intent(principal: u32, fees: &[u32]) -> FinancialIntent {
        FinancialIntent {
            debits: if principal == 0 {
                vec![]
            } else {
                vec![amount(principal)]
            },
            maximum_costs: fees
                .iter()
                .map(|units| CostBound {
                    location: AccountingLocation::Cash {
                        resource: cash_resource(),
                    },
                    amount: amount(*units).amount,
                    treatment: CostTreatment::AdditionalDebit,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn reservation(
        intent: &FinancialIntent,
        selection: &CashProvenanceSelection,
    ) -> RunReservation {
        serde_json::from_value(json!({
            "debits":intent.debits,
            "maximum_costs":intent.maximum_costs,
            "provenance":selection.principal,
            "additional_cost_provenance":selection.additional_costs,
            "cash_sources":selection.sources,
            "cash_provenance_version":3,
            "maximum_fee_usd_micros":"100",
            "additional_allocation_usd_micros":"0",
            "binding_hash":"synthetic-cash-slices-binding",
            "route_step_index":0,
        }))
        .unwrap()
    }

    fn actual(units: u32) -> BTreeMap<String, BigUint> {
        if units == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(
                super::super::resource_asset_key(&amount(units)).unwrap(),
                BigUint::from(units),
            )])
        }
    }

    fn invalid_code<T>(result: Result<T>) {
        assert_eq!(result.err().unwrap().code, "RESERVATION_PROVENANCE_INVALID");
    }

    fn with_local_allocation(lots: Vec<ProvenanceLot>, units: u32) -> RunLedger {
        let mut run = run(lots);
        run.allocations.push(super::super::RunAllocation {
            holding: amount(units),
            value_usd_micros: units.to_string(),
            observed_at: "2000-01-01T00:00:00Z".into(),
            source: "synthetic local fee stock".into(),
        });
        run
    }

    fn transfer_intent(principal: u32, fee: u32, target: &str) -> FinancialIntent {
        let mut intent = intent(principal, &[fee]);
        intent.expected.push(EffectConstraint::Transfer {
            from: cash_resource(),
            to: ResourceId(target.into()),
            debit_asset: asset(),
            credit_asset: asset(),
            maximum_debit: Quantity::from_u64(principal.into()),
            minimum_credit: Quantity::from_u64(principal.into()),
        });
        intent
    }

    #[test]
    fn local_fee_stock_and_foreign_principal_stay_separate_for_every_partial_receipt() {
        let local = cash_resource().0;
        let run = with_local_allocation(vec![lot(&local, 4), lot("B", 10), lot("A", 5)], 4);
        let intent = transfer_intent(12, 2, "external-return-custody");
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.additional_costs, vec![lot(&local, 2)]);
        assert_eq!(selection.sources.additional_cost_indices, vec![0]);
        assert_eq!(selection.principal, vec![lot("B", 10), lot("A", 2)]);
        assert_eq!(selection.sources.principal_indices, vec![1, 2]);
        assert_eq!(selection.remaining, vec![lot(&local, 2), lot("A", 3)]);
        let reservation = reservation(&intent, &selection);
        for principal in 0u32..=12 {
            for fee in 0u32..=2 {
                let settled = super::settle(
                    &reservation,
                    &selection.remaining,
                    &intent,
                    &actual(principal),
                    &actual(principal + fee),
                )
                .unwrap();
                let mut expected = vec![
                    lot(&local, 4 - fee),
                    lot("B", 10 - principal.min(10)),
                    lot("A", 5 - principal.saturating_sub(10)),
                ];
                expected.retain(|lot| lot.atoms != "0");
                assert_eq!(settled.remaining, expected);
                assert!(
                    settled
                        .principal
                        .iter()
                        .all(|lot| lot.origin != origin(&local))
                );
                assert!(
                    settled
                        .additional_costs
                        .iter()
                        .all(|lot| lot.origin == origin(&local))
                );
                assert_eq!(
                    settled
                        .principal
                        .iter()
                        .map(|lot| lot.atoms.parse::<u32>().unwrap())
                        .sum::<u32>(),
                    principal
                );
                assert_eq!(
                    settled
                        .additional_costs
                        .iter()
                        .map(|lot| lot.atoms.parse::<u32>().unwrap())
                        .sum::<u32>(),
                    fee
                );
            }
        }
        assert_eq!(
            release(&reservation, &selection.remaining).unwrap(),
            run.provenance
        );
    }

    #[test]
    fn transfer_uses_recipient_then_foreign_then_local_and_never_promotes_borrowed_cash() {
        let local = cash_resource().0;
        let mut borrowed = lot("A", 8);
        borrowed.origin = "borrowed:synthetic-only".into();
        let run = with_local_allocation(
            vec![borrowed.clone(), lot(&local, 4), lot("B", 3), lot("A", 5)],
            4,
        );
        let intent = transfer_intent(7, 2, "A");
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.additional_costs, vec![lot(&local, 2)]);
        assert_eq!(selection.principal, vec![lot("A", 5), lot("B", 2)]);
        assert_eq!(
            selection.remaining,
            vec![borrowed.clone(), lot(&local, 2), lot("B", 1)]
        );
        let no_recipient = transfer_intent(10, 2, "external-return-custody");
        let selection = select(&run, &no_recipient).unwrap();
        assert_eq!(
            selection.principal,
            vec![lot("B", 3), lot("A", 5), lot(&local, 2)]
        );
        assert_eq!(selection.remaining, vec![borrowed]);
    }

    #[test]
    fn persisted_v3_partial_receipt_consumes_original_slice_order_without_policy_drift() {
        let local = cash_resource().0;
        let run = with_local_allocation(vec![lot(&local, 5), lot("B", 5)], 5);
        let intent = transfer_intent(7, 0, "external-return-custody");
        // An existing v3 record selected its local lot before incoming stock.
        // Its validated slices remain authoritative after a policy update.
        let selection = CashProvenanceSelection {
            remaining: vec![lot(&local, 2), lot("B", 1)],
            principal: vec![lot(&local, 3), lot("B", 4)],
            additional_costs: vec![],
            sources: CashReservationSources {
                before: run.provenance.clone(),
                principal_indices: vec![0, 1],
                additional_cost_indices: vec![],
            },
        };
        let reservation = reservation(&intent, &selection);
        let settled = super::settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(2),
            &actual(2),
        )
        .unwrap();
        assert_eq!(settled.principal, vec![lot(&local, 2)]);
        assert_eq!(settled.remaining, vec![lot(&local, 3), lot("B", 5)]);
        assert!(settled.additional_costs.is_empty());
    }

    #[test]
    fn receipt_principal_cannot_substitute_a_recipient_home_fee_slice() {
        let mut run = run(vec![lot("A", 2), lot("B", 5)]);
        let intent = transfer_intent(5, 2, "A");
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.additional_costs, vec![lot("A", 2)]);
        assert_eq!(selection.principal, vec![lot("B", 5)]);
        let reservation = reservation(&intent, &selection);
        let mut settled = super::settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(5),
            &actual(6),
        )
        .unwrap();
        run.provenance = settled.remaining;
        let effects = vec![
            gofin_domain::FinancialEffect::Transfer {
                from: cash_resource(),
                to: ResourceId("A".into()),
                debit: amount(5).amount,
                credit: amount(5).amount,
            },
            gofin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: cash_resource(),
                },
                amount: amount(1).amount,
                category: "synthetic fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
        ];
        let costs = super::super::apply_effect_provenance(
            &mut run,
            "synthetic-frozen-fee-separation",
            &mut settled.principal,
            &mut settled.additional_costs,
            &effects,
            &gofin_domain::Valuation {
                movement_usd_micros: Quantity::from_u64(5),
                maximum_fee_usd_micros: Quantity::from_u64(2),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: gofin_domain::now(),
                source: "synthetic exact price".into(),
                prices: vec![],
            },
        )
        .unwrap();
        assert!(settled.principal.is_empty());
        assert!(settled.additional_costs.is_empty());
        assert_eq!(
            costs,
            vec![ResourceAmount {
                resource: ResourceId("A".into()),
                amount: amount(1).amount
            }]
        );
        let moved = run
            .provenance
            .iter()
            .find(|lot| {
                lot.location
                    == (AccountingLocation::Cash {
                        resource: ResourceId("A".into()),
                    })
            })
            .unwrap();
        assert_eq!(moved.origin, origin("B"));
        assert_eq!(moved.atoms, "5");
        assert_eq!(run.provenance[0], lot("A", 1));
    }

    // Legacy unit fixtures inspect the combined consumed sequence. Production
    // and the separation regressions below use the typed fields independently.
    fn settle(
        reservation: &RunReservation,
        current: &[ProvenanceLot],
        intent: &FinancialIntent,
        principal: &BTreeMap<String, BigUint>,
        total: &BTreeMap<String, BigUint>,
    ) -> Result<(Vec<ProvenanceLot>, Vec<ProvenanceLot>)> {
        let result = super::settle(reservation, current, intent, principal, total)?;
        let mut used = result.principal;
        used.extend(result.additional_costs);
        Ok((result.remaining, used))
    }

    #[test]
    fn return_principal_prefers_whole_recipient_origin_and_fees_remain_separate() {
        let run = run(vec![lot("B", 10), lot("A", 100)]);
        let mut intent = intent(100, &[2]);
        intent.expected.push(EffectConstraint::Transfer {
            from: cash_resource(),
            to: ResourceId("A".into()),
            debit_asset: asset(),
            credit_asset: asset(),
            maximum_debit: Quantity::from_u64(100),
            minimum_credit: Quantity::from_u64(100),
        });
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.sources.principal_indices, vec![1]);
        assert_eq!(selection.sources.additional_cost_indices, vec![0]);
        assert_eq!(selection.principal, vec![lot("A", 100)]);
        assert_eq!(selection.additional_costs, vec![lot("B", 2)]);
        assert_eq!(selection.remaining, vec![lot("B", 8)]);
        let reservation = reservation(&intent, &selection);
        let (remaining, used) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(80),
            &actual(81),
        )
        .unwrap();
        assert_eq!(remaining, vec![lot("B", 9), lot("A", 20)]);
        assert_eq!(used, vec![lot("A", 80), lot("B", 1)]);
        assert_eq!(
            release(&reservation, &selection.remaining).unwrap(),
            run.provenance
        );
    }

    #[test]
    fn refusal_and_zero_cost_definitive_failure_preserve_exact_original_sequence() {
        let run = run(vec![lot("A", 10), lot("B", 10)]);
        let intent = intent(5, &[2]);
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.remaining, vec![lot("A", 3), lot("B", 10)]);
        let reservation = reservation(&intent, &selection);
        assert_eq!(
            release(&reservation, &selection.remaining).unwrap(),
            run.provenance
        );
        let (remaining, used) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(0),
            &actual(0),
        )
        .unwrap();
        assert_eq!(remaining, run.provenance);
        assert!(used.is_empty());
    }

    #[test]
    fn reverted_receipt_only_consumes_reserved_fee_slice_and_refunds_principal_in_place() {
        let run = run(vec![lot("A", 10), lot("B", 10)]);
        let intent = intent(5, &[2]);
        let selection = select(&run, &intent).unwrap();
        let reservation = reservation(&intent, &selection);
        let (remaining, used) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(0),
            &actual(1),
        )
        .unwrap();
        assert_eq!(remaining, vec![lot("A", 9), lot("B", 10)]);
        assert_eq!(used, vec![lot("A", 1)]);
        let mut restored_run = run;
        restored_run.provenance = remaining;
        let next = select(&restored_run, &self::intent(8, &[])).unwrap();
        assert_eq!(next.principal, vec![lot("A", 8)]);
    }

    #[test]
    fn repeated_origin_labels_and_partial_refunds_do_not_coalesce_or_sort() {
        let run = run(vec![lot("A", 5), lot("B", 6), lot("A", 7)]);
        let intent = intent(6, &[1]);
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.sources.principal_indices, vec![0, 1]);
        assert_eq!(selection.sources.additional_cost_indices, vec![0]);
        let reservation = reservation(&intent, &selection);
        let (remaining, used) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(4),
            &actual(5),
        )
        .unwrap();
        assert_eq!(remaining, vec![lot("B", 6), lot("A", 7)]);
        assert_eq!(used, vec![lot("A", 4), lot("A", 1)]);
        let encoded = serde_json::to_value(&reservation).unwrap();
        let reopened: RunReservation = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            release(&reopened, &selection.remaining).unwrap(),
            run.provenance
        );
    }

    #[test]
    fn repayment_uses_borrowed_principal_first_and_refunds_selected_ceiling_in_place() {
        let mut borrowed = lot("A", 5);
        borrowed.origin = "borrowed:synthetic-loan".into();
        let run = run(vec![lot("A", 8), borrowed.clone(), lot("B", 9)]);
        let mut intent = intent(10, &[2]);
        intent.expected.push(EffectConstraint::OriginMovement {
            from: AccountingLocation::Cash {
                resource: cash_resource(),
            },
            to: AccountingLocation::Position {
                resource: ResourceId("synthetic-market".into()),
                position: "synthetic-debt".into(),
                position_kind: PositionKind::VariableDebt,
            },
            debit_asset: asset(),
            credit_asset: asset(),
            maximum_debit: Quantity::from_u64(10),
            minimum_credit: Quantity::from_u64(1),
        });
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.sources.principal_indices, vec![1, 0]);
        assert_eq!(selection.sources.additional_cost_indices, vec![0]);
        assert_eq!(selection.principal, vec![borrowed.clone(), lot("A", 5)]);
        let reservation = reservation(&intent, &selection);
        let (remaining, used) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(7),
            &actual(8),
        )
        .unwrap();
        assert_eq!(remaining, vec![lot("A", 5), lot("B", 9)]);
        assert_eq!(used, vec![borrowed, lot("A", 2), lot("A", 1)]);
    }

    #[test]
    fn fee_caps_are_aggregated_and_never_fall_back_to_accrual_or_borrowed_money() {
        let mut income = lot("A", 10);
        income.origin = format!("accrual:{}", income.origin);
        let mut borrowed = lot("A", 10);
        borrowed.origin = "borrowed:synthetic-loan".into();
        let run = run(vec![lot("A", 3), income, borrowed]);
        assert!(select(&run, &intent(0, &[2, 2])).is_err());
        let principal_from_income = select(&run, &intent(2, &[2])).unwrap();
        assert_eq!(principal_from_income.additional_costs, vec![lot("A", 2)]);
        assert_eq!(principal_from_income.principal[0], lot("A", 1));
        assert!(
            principal_from_income.principal[1]
                .origin
                .starts_with("accrual:")
        );
        let selection = select(&run, &intent(0, &[1, 2])).unwrap();
        assert_eq!(selection.additional_costs, vec![lot("A", 3)]);
        assert_eq!(selection.sources.additional_cost_indices, vec![0]);
    }

    #[test]
    fn included_cost_does_not_reserve_additional_cash_and_positions_are_untouched() {
        let mut position = lot("A", 20);
        position.location = AccountingLocation::Position {
            resource: ResourceId("synthetic-market".into()),
            position: "synthetic-supply".into(),
            position_kind: PositionKind::Supply,
        };
        let run = run(vec![position.clone(), lot("A", 10)]);
        let mut intent = intent(5, &[]);
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: cash_resource(),
            },
            amount: amount(1).amount,
            treatment: CostTreatment::IncludedInDebit,
        });
        let selection = select(&run, &intent).unwrap();
        assert_eq!(selection.remaining, vec![position.clone(), lot("A", 5)]);
        assert!(selection.additional_costs.is_empty());
        let reservation = reservation(&intent, &selection);
        let (remaining, _) = settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(4),
            &actual(4),
        )
        .unwrap();
        assert_eq!(remaining, vec![position, lot("A", 6)]);
    }

    #[test]
    fn mismatched_source_snapshot_slice_caps_and_current_order_fail_closed() {
        let run = run(vec![lot("A", 10), lot("B", 10)]);
        let intent = intent(5, &[2]);
        let selection = select(&run, &intent).unwrap();
        let original = reservation(&intent, &selection);
        let mut missing = original.clone();
        missing.cash_sources = None;
        invalid_code(release(&missing, &selection.remaining));
        let mut wrong_index = original.clone();
        wrong_index.cash_sources.as_mut().unwrap().principal_indices[0] = 99;
        invalid_code(release(&wrong_index, &selection.remaining));
        let mut wrong_count = original.clone();
        wrong_count
            .cash_sources
            .as_mut()
            .unwrap()
            .principal_indices
            .clear();
        invalid_code(release(&wrong_count, &selection.remaining));
        let mut wrong_identity = original.clone();
        wrong_identity.provenance[0].origin = "PRIVATE_RESERVATION_MARKER".into();
        let error = release(&wrong_identity, &selection.remaining).unwrap_err();
        assert_eq!(error.code, "RESERVATION_PROVENANCE_INVALID");
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("PRIVATE_RESERVATION_MARKER")
        );
        let mut wrong_snapshot = original.clone();
        wrong_snapshot.cash_sources.as_mut().unwrap().before[0].atoms = "11".into();
        invalid_code(release(&wrong_snapshot, &selection.remaining));
        let mut wrong_cap = original.clone();
        wrong_cap.debits[0].amount.atoms = Quantity::from_u64(6);
        invalid_code(release(&wrong_cap, &selection.remaining));
        let mut oversubscribed = original.clone();
        oversubscribed.provenance[0].atoms = "11".into();
        oversubscribed.debits[0].amount.atoms = Quantity::from_u64(11);
        invalid_code(release(&oversubscribed, &selection.remaining));
        let mut reordered = selection.remaining.clone();
        reordered.reverse();
        invalid_code(release(&original, &reordered));
        let mut changed = selection.remaining.clone();
        changed[0].atoms = "2".into();
        invalid_code(release(&original, &changed));
    }

    #[test]
    fn actual_principal_and_fee_may_not_spend_each_others_reserved_slices() {
        let run = run(vec![lot("A", 20)]);
        let intent = intent(5, &[5]);
        let selection = select(&run, &intent).unwrap();
        let reservation = reservation(&intent, &selection);
        invalid_code(settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(8),
            &actual(8),
        ));
        invalid_code(settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(1),
            &actual(7),
        ));
        invalid_code(settle(
            &reservation,
            &selection.remaining,
            &intent,
            &actual(3),
            &actual(2),
        ));
        let unknown_key = super::super::resource_asset_key(&ResourceAmount {
            resource: ResourceId("synthetic-other-wallet".into()),
            amount: amount(1).amount,
        })
        .unwrap();
        invalid_code(settle(
            &reservation,
            &selection.remaining,
            &intent,
            &BTreeMap::new(),
            &BTreeMap::from([(unknown_key, BigUint::from(1u32))]),
        ));
    }

    #[test]
    fn duplicate_indices_and_nonselected_fee_sources_are_not_valid_reservations() {
        let run = run(vec![lot("A", 10), lot("B", 10)]);
        let intent = intent(5, &[2]);
        let selection = select(&run, &intent).unwrap();
        let original = reservation(&intent, &selection);
        // A source may occur in both the principal and fee lists, but each
        // aggregated selection can take only one slice of that source slot.
        assert_eq!(
            original.cash_sources.as_ref().unwrap().principal_indices,
            vec![0]
        );
        assert_eq!(
            original
                .cash_sources
                .as_ref()
                .unwrap()
                .additional_cost_indices,
            vec![0]
        );
        let mut principal_twice = original.clone();
        principal_twice.provenance = vec![lot("A", 2), lot("A", 3)];
        principal_twice
            .cash_sources
            .as_mut()
            .unwrap()
            .principal_indices = vec![0, 0];
        invalid_code(release(&principal_twice, &selection.remaining));
        let mut fee_twice = original.clone();
        fee_twice.additional_cost_provenance = vec![lot("A", 1), lot("A", 1)];
        fee_twice
            .cash_sources
            .as_mut()
            .unwrap()
            .additional_cost_indices = vec![0, 0];
        invalid_code(release(&fee_twice, &selection.remaining));

        let mut borrowed = lot("A", 5);
        borrowed.origin = "borrowed:synthetic-loan".into();
        let mut corrupted = original;
        corrupted.debits = vec![];
        corrupted.provenance = vec![];
        corrupted.additional_cost_provenance = vec![ProvenanceLot {
            atoms: "2".into(),
            ..borrowed.clone()
        }];
        corrupted.cash_sources = Some(CashReservationSources {
            before: vec![borrowed.clone(), lot("A", 10)],
            principal_indices: vec![],
            additional_cost_indices: vec![0],
        });
        borrowed.atoms = "3".into();
        invalid_code(release(&corrupted, &[borrowed, lot("A", 10)]));
    }

    #[test]
    fn every_bounded_partial_receipt_conserves_units_and_fifo_without_appended_refunds() {
        let run = run(vec![lot("A", 5), lot("B", 6), lot("A", 7)]);
        let intent = intent(6, &[2]);
        let selection = select(&run, &intent).unwrap();
        let reservation = reservation(&intent, &selection);
        for principal in 0..=6 {
            for fee in 0..=2 {
                let (remaining, used) = settle(
                    &reservation,
                    &selection.remaining,
                    &intent,
                    &actual(principal),
                    &actual(principal + fee),
                )
                .unwrap();
                let principal_a = principal.min(3);
                let principal_b = principal.saturating_sub(3);
                let mut expected = vec![
                    lot("A", 5 - principal_a - fee),
                    lot("B", 6 - principal_b),
                    lot("A", 7),
                ];
                expected.retain(|lot| lot.atoms != "0");
                assert_eq!(remaining, expected);
                let sum = |lots: &[ProvenanceLot]| {
                    lots.iter()
                        .map(|lot| lot.atoms.parse::<u32>().unwrap())
                        .sum::<u32>()
                };
                assert_eq!(sum(&remaining) + sum(&used), 18);
                assert_eq!(sum(&used), principal + fee);
            }
        }
    }
}
