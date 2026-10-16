//! Conservative origin replay for preflighted return paths.
//!
//! This validates the projected route independently of aggregate balance
//! equality. Selected cash is the only seed; typed principal links move its
//! origin labels, costs consume them once, and unresolved/foreign/borrowed lots
//! cannot disappear into an aggregate. Mixed interval conversions use a
//! deliberately broad finite enclosure rather than endpoint sampling.

use super::{ProvenanceLot, RunAllocation};
use gofin_domain::{
    AccountingLocation, AssetId, AssetPrice, CostTreatment, Direction, EffectConstraint,
    PositionKind, ProjectedEffect, ProjectedState, Quantity, Result, ReturnRouteProof,
    StateSnapshot,
};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Zero;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Place {
    location: AccountingLocation,
    asset: AssetId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IntervalLot {
    origin: String,
    location: AccountingLocation,
    asset: AssetId,
    minimum: BigUint,
    maximum: BigUint,
}

impl IntervalLot {
    fn place(&self) -> Place {
        Place {
            location: self.location.clone(),
            asset: self.asset.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Interval {
    minimum: BigUint,
    maximum: BigUint,
}

struct IntervalSlice {
    source_index: usize,
    origin: String,
    amount: Interval,
}

impl Interval {
    fn exact(value: BigUint) -> Self {
        Self {
            minimum: value.clone(),
            maximum: value,
        }
    }

    fn zero() -> Self {
        Self::exact(BigUint::zero())
    }
}

#[derive(Clone, Debug)]
struct Flow {
    from: Place,
    to: Place,
    maximum_debit: BigUint,
    minimum_credit: BigUint,
    conversion: bool,
}

#[derive(Clone, Debug, Default)]
struct StepDeltas {
    cash: BTreeMap<Place, (BigInt, BigInt)>,
    positions: BTreeMap<Place, (BigInt, BigInt)>,
    cash_sweeps: BTreeMap<Place, BigUint>,
    position_closes: BTreeMap<Place, BigUint>,
}

#[derive(Clone, Debug)]
struct SelectedOrigin {
    origin: String,
    home: Place,
    start: BigUint,
}

/// Initial full-run replay. The selected allocations, not the whole observed
/// baseline, are the only cash lots available to the route.
pub(super) fn validate_projected_origins(
    baseline: &StateSnapshot,
    allocations: &[RunAllocation],
    proofs: &[ReturnRouteProof],
    prices: &[AssetPrice],
    reserve_usd_micros: &BigUint,
) -> Result<()> {
    if !baseline.complete || !baseline.pending.is_empty() {
        return Err(provenance_error());
    }
    let selected = selected_origins(baseline, allocations)?;
    let mut lots = selected
        .iter()
        .map(|origin| IntervalLot {
            origin: origin.origin.clone(),
            location: origin.home.location.clone(),
            asset: origin.home.asset.clone(),
            minimum: origin.start.clone(),
            maximum: origin.start.clone(),
        })
        .filter(|lot| !lot.maximum.is_zero())
        .collect::<Vec<_>>();
    let mut state = selected_state(allocations)?;
    replay_steps(&mut lots, &mut state, proofs, prices, 0)?;
    check_terminal(&lots, &state, &selected, proofs, prices, reserve_usd_micros)
}

/// Seed a revised suffix from exact journal provenance. The current aggregate
/// state is only a consistency check; it never creates an origin lot.
pub(super) fn replay_seeded_suffix(
    allocations: &[RunAllocation],
    seed: &[ProvenanceLot],
    current: &ProjectedState,
    suffix: &[ReturnRouteProof],
    prices: &[AssetPrice],
    total_reserve_usd_micros: &BigUint,
    start_index: usize,
) -> Result<()> {
    let selected = selected_origins_without_baseline(allocations)?;
    let mut lots = seed
        .iter()
        .map(|lot| {
            let amount = super::parse_uint(&lot.atoms)?;
            Ok(IntervalLot {
                origin: lot.origin.clone(),
                location: lot.location.clone(),
                asset: lot.asset.clone(),
                minimum: amount.clone(),
                maximum: amount,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate_seed_projection(&lots, current)?;
    let mut state = current.clone();
    replay_steps(&mut lots, &mut state, suffix, prices, start_index)?;
    check_terminal(
        &lots,
        &state,
        &selected,
        suffix,
        prices,
        total_reserve_usd_micros,
    )
}

fn selected_origins(
    baseline: &StateSnapshot,
    allocations: &[RunAllocation],
) -> Result<Vec<SelectedOrigin>> {
    let mut observed = BTreeMap::<Place, BigUint>::new();
    for holding in &baseline.state.holdings {
        let place = Place {
            location: AccountingLocation::Cash {
                resource: holding.resource.clone(),
            },
            asset: holding.amount.asset.clone(),
        };
        *observed.entry(place).or_default() += holding.amount.atoms.integer();
    }
    let selected = selected_origins_without_baseline(allocations)?;
    if selected
        .iter()
        .any(|origin| observed.get(&origin.home).cloned().unwrap_or_default() < origin.start)
    {
        return Err(provenance_error());
    }
    Ok(selected)
}

fn selected_origins_without_baseline(allocations: &[RunAllocation]) -> Result<Vec<SelectedOrigin>> {
    let mut starts = BTreeMap::<Place, BigUint>::new();
    for allocation in allocations {
        let place = Place {
            location: AccountingLocation::Cash {
                resource: allocation.holding.resource.clone(),
            },
            asset: allocation.holding.amount.asset.clone(),
        };
        *starts.entry(place).or_default() += allocation.holding.amount.atoms.integer();
    }
    starts
        .into_iter()
        .map(|(home, start)| {
            let key =
                serde_json::to_string(&(home.location.resource().0.clone(), home.asset.clone()))
                    .map_err(|_| provenance_error())?;
            Ok(SelectedOrigin {
                origin: format!("allocation:{key}"),
                home,
                start,
            })
        })
        .collect()
}

fn selected_state(allocations: &[RunAllocation]) -> Result<ProjectedState> {
    let mut sums = BTreeMap::<Place, BigUint>::new();
    for allocation in allocations {
        let place = Place {
            location: AccountingLocation::Cash {
                resource: allocation.holding.resource.clone(),
            },
            asset: allocation.holding.amount.asset.clone(),
        };
        *sums.entry(place).or_default() += allocation.holding.amount.atoms.integer();
    }
    let mut state = ProjectedState::default();
    for (place, amount) in sums {
        let AccountingLocation::Cash { resource } = place.location else {
            return Err(provenance_error());
        };
        state.cash.push(gofin_domain::ProjectedCash {
            resource,
            asset: place.asset,
            amount: gofin_domain::QuantityInterval {
                minimum: Quantity::new(amount.to_string())?,
                maximum: Quantity::new(amount.to_string())?,
            },
        });
    }
    Ok(state)
}

fn validate_seed_projection(lots: &[IntervalLot], state: &ProjectedState) -> Result<()> {
    let mut sum = BTreeMap::<Place, (BigUint, BigUint)>::new();
    for lot in lots {
        if lot.minimum > lot.maximum {
            return Err(provenance_error());
        }
        let entry = sum.entry(lot.place()).or_default();
        entry.0 += &lot.minimum;
        entry.1 += &lot.maximum;
    }
    let places = sum
        .keys()
        .cloned()
        .chain(state.cash.iter().map(|row| Place {
            location: AccountingLocation::Cash {
                resource: row.resource.clone(),
            },
            asset: row.asset.clone(),
        }))
        .collect::<BTreeSet<_>>();
    for place in places {
        if !matches!(place.location, AccountingLocation::Cash { .. }) {
            continue;
        }
        let (minimum, maximum) = sum.get(&place).cloned().unwrap_or_default();
        let current = projected_interval(state, &place)?;
        if minimum != current.minimum || maximum != current.maximum {
            return Err(provenance_error());
        }
    }
    Ok(())
}

fn replay_steps(
    lots: &mut Vec<IntervalLot>,
    state: &mut ProjectedState,
    proofs: &[ReturnRouteProof],
    prices: &[AssetPrice],
    start_index: usize,
) -> Result<()> {
    for (offset, proof) in proofs.iter().enumerate() {
        let step = start_index + offset;
        if proof.step_index as usize != step {
            return Err(provenance_error());
        }
        replay_step(lots, state, proof, prices, step)?;
    }
    Ok(())
}

fn replay_step(
    lots: &mut Vec<IntervalLot>,
    state: &mut ProjectedState,
    proof: &ReturnRouteProof,
    _prices: &[AssetPrice],
    step: usize,
) -> Result<()> {
    validate_cost_funding(lots, proof)?;
    let deltas = step_deltas(proof)?;
    let flows = flows(&proof.intent.expected)?;
    add_accruals(lots, state, proof)?;
    let mut used_sources = BTreeSet::<Place>::new();
    let mut used_destinations = BTreeSet::<Place>::new();
    let mut cost_sources = BTreeSet::<Place>::new();
    let mut fee_refunds = Vec::new();

    // Earmark the full exact selected fee cap before principal. Refunds stay
    // outside principal selection and return to their original slots only
    // after the principal flow, matching durable source-index reservations.
    for (place, amount) in additional_cost_caps(proof) {
        if !matches!(place.location, AccountingLocation::Cash { .. }) {
            return Err(provenance_error());
        }
        if amount.is_zero() {
            continue;
        }
        let slices =
            consume_lots_indexed(lots, &place, Interval::exact(amount), None, true, false)?;
        fee_refunds.push((place.clone(), slices));
        cost_sources.insert(place);
    }

    // Borrowed cash is a distinct, temporary origin. The liability is checked
    // in projected position state; it is not a second cash-origin lot.
    for flow in &flows {
        if !is_new_borrow(flow, proof, &deltas) {
            continue;
        }
        if !used_sources.insert(flow.from.clone()) || !used_destinations.insert(flow.to.clone()) {
            return Err(provenance_error());
        }
        let (_, debt_max) = positive_position_delta(&deltas, &flow.from)?;
        let credit = gross_credit(&deltas, proof, &flow.to, flow.minimum_credit.clone())?;
        if debt_max > flow.maximum_debit || credit.minimum < flow.minimum_credit {
            return Err(provenance_error());
        }
        let origin = format!("borrowed:projected:{step}");
        push_lot(lots, &flow.to, &origin, credit);
    }

    // Repayments spend borrowed cash lots first, then selected lots. The debt
    // position is reduced by its own projected principal delta, not relabeled
    // as a credit or a source of cash.
    for flow in &flows {
        if is_new_borrow(flow, proof, &deltas) || !is_repayment(flow, &deltas) {
            continue;
        }
        if !used_sources.insert(flow.from.clone()) || !used_destinations.insert(flow.to.clone()) {
            return Err(provenance_error());
        }
        let paid = movement_debit(lots, &deltas, proof, flow)?;
        if paid.maximum > flow.maximum_debit {
            return Err(provenance_error());
        }
        let debt_decrease = negative_position_delta(&deltas, &flow.to)?;
        let debt_min = debt_decrease
            .map(|range| range.0)
            .or_else(|| deltas.position_closes.get(&flow.to).cloned())
            .ok_or_else(provenance_error)?;
        if debt_min < flow.minimum_credit {
            return Err(provenance_error());
        }
        // Debt is checked as physical position state, never as cash provenance.
        consume_flow_source(lots, state, &deltas, &flow.from, paid, None, true)?;
        used_sources.insert(flow.from.clone());
        used_destinations.insert(flow.to.clone());
    }

    // Remaining principal flows preserve their source origin. Transfer source
    // selection prefers an existing lot whose allocation home is the recipient,
    // exactly as settlement does; it never renames a source lot.
    for flow in &flows {
        if is_new_borrow(flow, proof, &deltas) || is_repayment(flow, &deltas) {
            continue;
        }
        if !used_sources.insert(flow.from.clone()) || !used_destinations.insert(flow.to.clone()) {
            return Err(provenance_error());
        }
        let debit = movement_debit(lots, &deltas, proof, flow)?;
        let credit = destination_credit(&deltas, proof, &flow.to, flow.minimum_credit.clone())?;
        if credit.minimum < flow.minimum_credit {
            return Err(provenance_error());
        }
        let recipient_origin = transfer_recipient_origin(flow);
        let consumed = consume_flow_source(
            lots,
            state,
            &deltas,
            &flow.from,
            debit,
            recipient_origin.as_deref(),
            false,
        )?;
        let output = credit;
        let projected =
            project_origin_intervals(&consumed, &output, flow.from.asset == flow.to.asset)?;
        for (origin, amount) in projected {
            push_lot(lots, &flow.to, &origin, amount);
        }
    }

    let mut settlements = BTreeMap::<Place, (BigInt, BigInt)>::new();
    for expected in &proof.intent.expected {
        if let EffectConstraint::Settlement {
            resource,
            asset,
            minimum_delta,
            maximum_delta,
            ..
        } = expected
        {
            let place = Place {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                asset: asset.clone(),
            };
            let range = settlements.entry(place).or_default();
            range.0 += minimum_delta.integer();
            range.1 += maximum_delta.integer();
        }
    }
    for (place, (minimum, maximum)) in settlements {
        if minimum > maximum {
            return Err(provenance_error());
        }
        let parents = lots
            .iter()
            .filter(|lot| {
                lot.place() == place
                    && lot.origin.starts_with("allocation:")
                    && !lot.maximum.is_zero()
            })
            .cloned()
            .collect::<Vec<_>>();
        let loss_max = if minimum.sign() == Sign::Minus {
            minimum.magnitude().clone()
        } else {
            BigUint::zero()
        };
        let loss_min = if maximum.sign() == Sign::Minus {
            maximum.magnitude().clone()
        } else {
            BigUint::zero()
        };
        if !loss_max.is_zero() {
            consume_lots(
                lots,
                &place,
                Interval {
                    minimum: loss_min,
                    maximum: loss_max,
                },
                None,
            )?;
        }
        if maximum.sign() == Sign::Plus {
            let parent = parents.first().ok_or_else(provenance_error)?;
            let gain_min = if minimum.sign() == Sign::Plus {
                minimum.magnitude().clone()
            } else {
                BigUint::zero()
            };
            push_lot(
                lots,
                &place,
                &format!("accrual:{}", parent.origin),
                Interval {
                    minimum: gain_min,
                    maximum: maximum.magnitude().clone(),
                },
            );
        }
        used_sources.insert(place.clone());
        used_destinations.insert(place);
    }

    used_sources.extend(cost_sources);

    // No typed principal edge may be silently omitted, and no cash delta may
    // mint a provenance label. Cost-only and explicit Debit are the only
    // unpaired cash outflows.
    for (place, (minimum, maximum)) in &deltas.cash {
        if used_sources.contains(place) && used_destinations.contains(place) {
            continue;
        }
        match (minimum.sign(), maximum.sign()) {
            (Sign::Plus | Sign::NoSign, Sign::Plus | Sign::NoSign) => {
                if !used_destinations.contains(place) {
                    return Err(provenance_error());
                }
            }
            (Sign::Minus, Sign::Minus | Sign::NoSign) => {
                if !used_sources.contains(place) {
                    let debit = delta_depletion(&(minimum.clone(), maximum.clone()))?
                        .map(|(minimum, maximum)| Interval { minimum, maximum })
                        .ok_or_else(provenance_error)?;
                    let cost_cap = additional_cost_cap(proof, place);
                    let cap = &cost_cap + explicit_debit_cap(proof, place);
                    if debit.maximum > cap {
                        return Err(provenance_error());
                    }
                    if !cost_cap.is_zero() {
                        continue;
                    }
                    consume_lots(lots, place, debit, None)?;
                    used_sources.insert(place.clone());
                }
            }
            _ => return Err(provenance_error()),
        }
    }
    for (place, (minimum, maximum)) in &deltas.positions {
        if matches!(&place.location, AccountingLocation::Position { position_kind, .. } if position_kind.is_derivative())
        {
            continue;
        }
        if minimum.sign() == Sign::Plus || maximum.sign() == Sign::Plus {
            if !used_sources.contains(place)
                && !used_destinations.contains(place)
                && !has_accrual(proof, place)
            {
                return Err(provenance_error());
            }
        } else if (minimum.sign() == Sign::Minus || maximum.sign() == Sign::Minus)
            && !used_sources.contains(place)
            && !used_destinations.contains(place)
        {
            return Err(provenance_error());
        }
    }
    for expected in &proof.intent.expected {
        if let EffectConstraint::Credit {
            to,
            asset,
            minimum_credit,
        } = expected
        {
            let paired_movement = proof.intent.expected.iter().any(|candidate| matches!(candidate,
                EffectConstraint::OriginMovement {
                    to: AccountingLocation::Cash { resource },
                    credit_asset,
                    minimum_credit: movement_minimum,
                    ..
                } if resource == to && credit_asset == asset && movement_minimum >= minimum_credit
            ));
            if !paired_movement
                || !used_destinations.contains(&Place {
                    location: AccountingLocation::Cash {
                        resource: to.clone(),
                    },
                    asset: asset.clone(),
                })
            {
                return Err(provenance_error());
            }
        }
    }

    // Sweeps and closes consume every tracked source lot, not the caller's cap.
    for (place, cap) in &deltas.cash_sweeps {
        if used_sources.contains(place) {
            continue;
        }
        if projected_interval(state, place)?.maximum > *cap {
            return Err(provenance_error());
        }
        let _ = take_all(lots, place);
        used_sources.insert(place.clone());
    }
    for (place, cap) in &deltas.position_closes {
        if used_sources.contains(place) {
            continue;
        }
        if projected_interval(state, place)?.maximum > *cap {
            return Err(provenance_error());
        }
        let _ = take_all(lots, place);
        used_sources.insert(place.clone());
    }

    // Each actual fee is in [0, cap]: a full charge leaves no reserved slice,
    // while a zero charge refunds every slice. Keeping those bounds separate
    // prevents unused fee stock from becoming projected transfer principal.
    for (place, slices) in fee_refunds {
        for slice in slices {
            let lot = lots
                .get_mut(slice.source_index)
                .ok_or_else(provenance_error)?;
            if lot.origin != slice.origin || lot.place() != place {
                return Err(provenance_error());
            }
            lot.maximum += slice.amount.maximum;
        }
    }
    lots.retain(|lot| !lot.maximum.is_zero());
    apply_state_deltas(state, &deltas)?;
    validate_lots_fit_state(lots, state, &deltas)?;
    Ok(())
}

fn validate_cost_funding(lots: &[IntervalLot], proof: &ReturnRouteProof) -> Result<()> {
    for (place, cap) in additional_cost_caps(proof) {
        let selected_minimum = lots
            .iter()
            .filter(|lot| lot.place() == place && lot.origin.starts_with("allocation:"))
            .fold(BigUint::zero(), |sum, lot| sum + &lot.minimum);
        if selected_minimum < cap {
            return Err(provenance_error());
        }
    }
    Ok(())
}

fn additional_cost_caps(proof: &ReturnRouteProof) -> BTreeMap<Place, BigUint> {
    let mut caps = BTreeMap::<Place, BigUint>::new();
    for cost in &proof.intent.maximum_costs {
        if cost.treatment != CostTreatment::AdditionalDebit {
            continue;
        }
        let place = Place {
            location: cost.location.clone(),
            asset: cost.amount.asset.clone(),
        };
        let cap = cost.amount.atoms.integer();
        *caps.entry(place).or_default() += cap;
    }
    caps
}

fn step_deltas(proof: &ReturnRouteProof) -> Result<StepDeltas> {
    let mut result = StepDeltas::default();
    for effect in &proof.effects {
        match effect {
            ProjectedEffect::CashDelta {
                resource,
                asset,
                minimum_delta,
                maximum_delta,
            } => add_delta(
                &mut result.cash,
                Place {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    asset: asset.clone(),
                },
                minimum_delta.integer(),
                maximum_delta.integer(),
            ),
            ProjectedEffect::PositionDelta {
                resource,
                position,
                asset,
                position_kind,
                minimum_delta,
                maximum_delta,
            } => add_delta(
                &mut result.positions,
                Place {
                    location: AccountingLocation::Position {
                        resource: resource.clone(),
                        position: position.clone(),
                        position_kind: position_kind.clone(),
                    },
                    asset: asset.clone(),
                },
                minimum_delta.integer(),
                maximum_delta.integer(),
            ),
            ProjectedEffect::CashSweep {
                resource,
                asset,
                maximum_amount,
                ..
            } => {
                result.cash_sweeps.insert(
                    Place {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        asset: asset.clone(),
                    },
                    maximum_amount.integer(),
                );
            }
            ProjectedEffect::PositionClose {
                resource,
                position,
                asset,
                position_kind,
                maximum_amount,
                ..
            } => {
                result.position_closes.insert(
                    Place {
                        location: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: position.clone(),
                            position_kind: position_kind.clone(),
                        },
                        asset: asset.clone(),
                    },
                    maximum_amount.integer(),
                );
            }
            ProjectedEffect::AuthorityValue { .. } => {}
        }
    }
    Ok(result)
}

fn add_delta(
    deltas: &mut BTreeMap<Place, (BigInt, BigInt)>,
    place: Place,
    minimum: BigInt,
    maximum: BigInt,
) {
    let entry = deltas
        .entry(place)
        .or_insert_with(|| (BigInt::zero(), BigInt::zero()));
    entry.0 += minimum;
    entry.1 += maximum;
}

fn flows(expected: &[EffectConstraint]) -> Result<Vec<Flow>> {
    let mut result = BTreeMap::<(Place, Place), Flow>::new();
    for constraint in expected {
        let flow = match constraint {
            EffectConstraint::OriginMovement {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => Some(Flow {
                from: Place {
                    location: from.clone(),
                    asset: debit_asset.clone(),
                },
                to: Place {
                    location: to.clone(),
                    asset: credit_asset.clone(),
                },
                maximum_debit: maximum_debit.integer(),
                minimum_credit: minimum_credit.integer(),
                conversion: false,
            }),
            EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => Some(Flow {
                from: Place {
                    location: AccountingLocation::Cash {
                        resource: from.clone(),
                    },
                    asset: debit_asset.clone(),
                },
                to: Place {
                    location: AccountingLocation::Cash {
                        resource: to.clone(),
                    },
                    asset: credit_asset.clone(),
                },
                maximum_debit: maximum_debit.integer(),
                minimum_credit: minimum_credit.integer(),
                conversion: false,
            }),
            EffectConstraint::Conversion {
                resource,
                input,
                maximum_input,
                output,
                minimum_output,
            } => Some(Flow {
                from: Place {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    asset: input.clone(),
                },
                to: Place {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    asset: output.clone(),
                },
                maximum_debit: maximum_input.integer(),
                minimum_credit: minimum_output.integer(),
                conversion: true,
            }),
            _ => None,
        };
        if let Some(flow) = flow {
            let key = (flow.from.clone(), flow.to.clone());
            result
                .entry(key)
                .and_modify(|current| {
                    current.maximum_debit = current
                        .maximum_debit
                        .clone()
                        .min(flow.maximum_debit.clone());
                    current.minimum_credit = current
                        .minimum_credit
                        .clone()
                        .max(flow.minimum_credit.clone());
                    current.conversion |= flow.conversion;
                })
                .or_insert(flow);
        }
    }
    Ok(result.into_values().collect())
}

fn is_new_borrow(flow: &Flow, proof: &ReturnRouteProof, deltas: &StepDeltas) -> bool {
    let AccountingLocation::Position {
        resource,
        position,
        position_kind,
    } = &flow.from.location
    else {
        return false;
    };
    matches!(position_kind, PositionKind::StableDebt | PositionKind::VariableDebt)
        && matches!(flow.to.location, AccountingLocation::Cash { .. })
        && proof.intent.expected.iter().any(|expected| matches!(expected,
            EffectConstraint::Debt {resource: actual_resource,debt,asset,direction:Direction::Increase,..}
                if actual_resource == resource && debt == position && asset == &flow.from.asset
        ))
        && positive_position_delta(deltas, &flow.from).is_ok()
        && gross_credit(deltas, proof, &flow.to, flow.minimum_credit.clone()).is_ok()
}

fn is_repayment(flow: &Flow, deltas: &StepDeltas) -> bool {
    matches!(flow.from.location, AccountingLocation::Cash { .. })
        && matches!(
            flow.to.location,
            AccountingLocation::Position {
                position_kind: PositionKind::StableDebt | PositionKind::VariableDebt,
                ..
            }
        )
        && negative_cash_delta(deltas, &flow.from).is_some()
        && (negative_position_delta(deltas, &flow.to)
            .ok()
            .flatten()
            .is_some()
            || deltas.position_closes.contains_key(&flow.to))
}

fn add_accruals(
    lots: &mut Vec<IntervalLot>,
    _state: &mut ProjectedState,
    proof: &ReturnRouteProof,
) -> Result<()> {
    for expected in &proof.intent.expected {
        let EffectConstraint::Accrual {
            resource,
            position,
            asset,
            position_kind,
            maximum_change,
        } = expected
        else {
            continue;
        };
        let maximum = maximum_change.integer();
        if maximum.is_zero() {
            continue;
        }
        let place = Place {
            location: AccountingLocation::Position {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: position_kind.clone(),
            },
            asset: asset.clone(),
        };
        if matches!(
            position_kind,
            PositionKind::StableDebt | PositionKind::VariableDebt
        ) {
            // Debt accrual is a liability bound, not a cash-origin lot or an
            // additional projected position delta. The typed close cap and
            // fresh receipt bound the eventual payment.
            continue;
        }
        let Some(close_cap) = proof.effects.iter().find_map(|effect| match effect {
            ProjectedEffect::PositionClose {
                resource: close_resource,
                position: close_position,
                asset: close_asset,
                position_kind: close_kind,
                maximum_amount,
                ..
            } if close_resource == resource
                && close_position == position
                && close_asset == asset
                && close_kind == position_kind =>
            {
                Some(maximum_amount.integer())
            }
            _ => None,
        }) else {
            // The Accrual maximum is not itself new principal. In particular,
            // adding it to every supply step would double the approved stock.
            continue;
        };
        let parents = lots
            .iter()
            .filter(|lot| lot.place() == place && !lot.origin.starts_with("accrual:"))
            .cloned()
            .collect::<Vec<_>>();
        if parents.is_empty() {
            return Err(provenance_error());
        }
        let principal_min = parents
            .iter()
            .fold(BigUint::zero(), |sum, lot| sum + &lot.minimum);
        let existing_accrual_min = lots
            .iter()
            .filter(|lot| lot.place() == place && lot.origin.starts_with("accrual:"))
            .fold(BigUint::zero(), |sum, lot| sum + &lot.minimum);
        let capacity = sub_floor(&close_cap, &(&principal_min + &existing_accrual_min));
        let gain_maximum = capacity.min(maximum);
        if gain_maximum.is_zero() {
            continue;
        }
        // Supply yield is introduced only when a typed close and its cash
        // output bound can carry it. Accrual is linked to existing principal,
        // never emitted as an independent credit.
        let parent_intervals = parents
            .iter()
            .map(|parent| {
                (
                    parent.origin.clone(),
                    parent.minimum.clone(),
                    parent.maximum.clone(),
                )
            })
            .collect::<Vec<_>>();
        let shares = super::projected_shares::enclose_shares(
            &parent_intervals,
            &BigUint::zero(),
            &gain_maximum,
            true,
        )?;
        for (origin, minimum, maximum) in shares {
            push_lot(
                lots,
                &place,
                &format!("accrual:{origin}"),
                Interval { minimum, maximum },
            );
        }
    }
    Ok(())
}

fn transfer_recipient_origin(flow: &Flow) -> Option<String> {
    if flow.conversion {
        return None;
    }
    if !matches!(flow.from.location, AccountingLocation::Cash { .. })
        || !matches!(flow.to.location, AccountingLocation::Cash { .. })
    {
        return None;
    }
    let AccountingLocation::Cash { resource } = &flow.to.location else {
        return None;
    };
    let key = serde_json::to_string(&(resource.0.clone(), flow.to.asset.clone())).ok()?;
    Some(format!("allocation:{key}"))
}

fn consume_lots(
    lots: &mut [IntervalLot],
    place: &Place,
    requested: Interval,
    preferred_origin: Option<&str>,
) -> Result<Vec<(String, Interval)>> {
    consume_lots_filtered(lots, place, requested, preferred_origin, false, false)
}

fn consume_borrowed_first_lots(
    lots: &mut [IntervalLot],
    place: &Place,
    requested: Interval,
) -> Result<Vec<(String, Interval)>> {
    consume_lots_filtered(lots, place, requested, None, false, true)
}

fn consume_lots_filtered(
    lots: &mut [IntervalLot],
    place: &Place,
    requested: Interval,
    preferred_origin: Option<&str>,
    selected_only: bool,
    borrowed_first: bool,
) -> Result<Vec<(String, Interval)>> {
    consume_lots_indexed(
        lots,
        place,
        requested,
        preferred_origin,
        selected_only,
        borrowed_first,
    )
    .map(|slices| {
        slices
            .into_iter()
            .map(|slice| (slice.origin, slice.amount))
            .collect()
    })
}

fn consume_lots_indexed(
    lots: &mut [IntervalLot],
    place: &Place,
    requested: Interval,
    preferred_origin: Option<&str>,
    selected_only: bool,
    borrowed_first: bool,
) -> Result<Vec<IntervalSlice>> {
    if requested.minimum > requested.maximum {
        return Err(provenance_error());
    }
    let mut indices = lots
        .iter()
        .enumerate()
        .filter(|(_, lot)| {
            lot.place() == *place
                && !lot.maximum.is_zero()
                && (!selected_only || lot.origin.starts_with("allocation:"))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if selected_only {
        indices.sort_by_key(|index| {
            (
                !super::cash_origin_is_home(
                    &lots[*index].origin,
                    place.location.resource(),
                    &place.asset,
                ),
                *index,
            )
        });
    } else if let Some(preferred) = preferred_origin {
        indices.sort_by_key(|index| {
            (
                super::cash_transfer_priority(
                    &lots[*index].origin,
                    place.location.resource(),
                    &place.asset,
                    preferred,
                ),
                *index,
            )
        });
    } else {
        indices.sort_by_key(|index| {
            (
                lots[*index].origin.starts_with("borrowed:") != borrowed_first,
                *index,
            )
        });
    }
    let available_min = indices
        .iter()
        .fold(BigUint::zero(), |sum, index| sum + &lots[*index].minimum);
    let available_max = indices
        .iter()
        .fold(BigUint::zero(), |sum, index| sum + &lots[*index].maximum);
    if requested.maximum > available_min || requested.minimum > available_max {
        return Err(provenance_error());
    }
    let mut consumed = Vec::new();
    let mut prior_min = BigUint::zero();
    let mut prior_max = BigUint::zero();
    for index in indices {
        let lot = &mut lots[index];
        let original_min = lot.minimum.clone();
        let original_max = lot.maximum.clone();
        let take_min = sub_floor(&requested.minimum, &prior_max).min(original_min.clone());
        let take_max = sub_floor(&requested.maximum, &prior_min).min(original_max.clone());
        if take_min > take_max {
            return Err(provenance_error());
        }
        // Depletion is saturating: a lot's lower bound can be fully consumed
        // even when the chosen debit upper exceeds that lower endpoint.
        lot.minimum = sub_floor(&original_min, &take_max);
        lot.maximum = sub_floor(&original_max, &take_min);
        if !take_max.is_zero() {
            consumed.push(IntervalSlice {
                source_index: index,
                origin: lot.origin.clone(),
                amount: Interval {
                    minimum: take_min,
                    maximum: take_max,
                },
            });
        }
        prior_min += original_min;
        prior_max += original_max;
    }
    Ok(consumed)
}

fn take_all(lots: &mut [IntervalLot], place: &Place) -> Vec<(String, Interval)> {
    let mut taken = Vec::new();
    for lot in lots.iter_mut() {
        if lot.place() == *place && !lot.maximum.is_zero() {
            taken.push((
                lot.origin.clone(),
                Interval {
                    minimum: std::mem::take(&mut lot.minimum),
                    maximum: std::mem::take(&mut lot.maximum),
                },
            ));
        }
    }
    taken
}

fn project_origin_intervals(
    sources: &[(String, Interval)],
    output: &Interval,
    same_asset: bool,
) -> Result<Vec<(String, Interval)>> {
    let inputs = sources
        .iter()
        .map(|(origin, amount)| {
            (
                origin.clone(),
                amount.minimum.clone(),
                amount.maximum.clone(),
            )
        })
        .collect::<Vec<_>>();
    super::projected_shares::enclose_shares(&inputs, &output.minimum, &output.maximum, same_asset)
        .map(|shares| {
            shares
                .into_iter()
                .map(|(origin, minimum, maximum)| (origin, Interval { minimum, maximum }))
                .collect()
        })
}

fn negative_cash_delta(deltas: &StepDeltas, place: &Place) -> Option<(BigUint, BigUint)> {
    delta_depletion(deltas.cash.get(place)?).ok().flatten()
}

fn negative_position_delta(
    deltas: &StepDeltas,
    place: &Place,
) -> Result<Option<(BigUint, BigUint)>> {
    match deltas.positions.get(place) {
        Some(delta) => delta_depletion(delta),
        None => Ok(None),
    }
}

fn positive_position_delta(deltas: &StepDeltas, place: &Place) -> Result<(BigUint, BigUint)> {
    positive_delta(deltas.positions.get(place))
}

fn positive_delta(value: Option<&(BigInt, BigInt)>) -> Result<(BigUint, BigUint)> {
    let Some((minimum, maximum)) = value else {
        return Err(provenance_error());
    };
    if (minimum.sign() != Sign::Plus && !minimum.is_zero())
        || maximum.sign() != Sign::Plus
        || maximum < minimum
    {
        return Err(provenance_error());
    }
    Ok((minimum.magnitude().clone(), maximum.magnitude().clone()))
}

fn delta_depletion(value: &(BigInt, BigInt)) -> Result<Option<(BigUint, BigUint)>> {
    let (minimum, maximum) = value;
    if minimum.is_zero() && maximum.is_zero() {
        return Ok(None);
    }
    if minimum.sign() != Sign::Minus
        || (maximum.sign() != Sign::Minus && !maximum.is_zero())
        || minimum > maximum
    {
        return Err(provenance_error());
    }
    Ok(Some((
        maximum.magnitude().clone(),
        minimum.magnitude().clone(),
    )))
}

fn additional_cost_cap(proof: &ReturnRouteProof, place: &Place) -> BigUint {
    proof
        .intent
        .maximum_costs
        .iter()
        .filter(|cost| cost.treatment == CostTreatment::AdditionalDebit)
        .filter(|cost| cost.location == place.location && cost.amount.asset == place.asset)
        .fold(BigUint::zero(), |sum, cost| {
            sum + cost.amount.atoms.integer()
        })
}

fn movement_debit(
    lots: &[IntervalLot],
    deltas: &StepDeltas,
    proof: &ReturnRouteProof,
    flow: &Flow,
) -> Result<Interval> {
    if let Some((minimum, maximum)) = negative_cash_delta(deltas, &flow.from) {
        let total = Interval { minimum, maximum };
        let cost = additional_cost_cap(proof, &flow.from);
        let principal_maximum = sub_floor(&total.maximum, &cost).min(flow.maximum_debit.clone());
        if total.minimum > flow.maximum_debit
            || total.maximum > &flow.maximum_debit + &cost
            || total.minimum > principal_maximum
        {
            return Err(provenance_error());
        }
        return Ok(Interval {
            minimum: total.minimum,
            maximum: principal_maximum,
        });
    }
    if let Some((minimum, maximum)) = negative_position_delta(deltas, &flow.from)? {
        return Ok(Interval { minimum, maximum });
    }
    if deltas.cash_sweeps.contains_key(&flow.from)
        || deltas.position_closes.contains_key(&flow.from)
    {
        return Ok(sum_at(lots, &flow.from));
    }
    Err(provenance_error())
}

fn gross_credit(
    deltas: &StepDeltas,
    _proof: &ReturnRouteProof,
    place: &Place,
    minimum_credit: BigUint,
) -> Result<Interval> {
    match &place.location {
        AccountingLocation::Cash { .. } => {
            let (minimum_delta, maximum_delta) =
                deltas.cash.get(place).ok_or_else(provenance_error)?;
            let withheld = _proof
                .intent
                .maximum_costs
                .iter()
                .filter(|bound| {
                    bound.treatment == CostTreatment::DeductedFromCredit
                        && bound.location == place.location
                        && bound.amount.asset == place.asset
                })
                .fold(BigUint::zero(), |sum, bound| {
                    sum + bound.amount.atoms.integer()
                });
            let gross_minimum =
                minimum_delta + BigInt::from(additional_cost_cap(_proof, place) + withheld);
            if gross_minimum.sign() == Sign::Minus || maximum_delta.sign() == Sign::Minus {
                return Err(provenance_error());
            }
            let minimum = gross_minimum.magnitude().clone();
            let maximum = maximum_delta.magnitude().clone();
            if minimum < minimum_credit || minimum > maximum {
                return Err(provenance_error());
            }
            Ok(Interval { minimum, maximum })
        }
        AccountingLocation::Position { .. } => {
            let (minimum, maximum) = positive_position_delta(deltas, place)?;
            Ok(Interval { minimum, maximum })
        }
    }
}

fn destination_credit(
    deltas: &StepDeltas,
    proof: &ReturnRouteProof,
    place: &Place,
    minimum_credit: BigUint,
) -> Result<Interval> {
    let mut credit = gross_credit(deltas, proof, place, minimum_credit)?;
    let withheld = proof
        .intent
        .maximum_costs
        .iter()
        .filter(|bound| {
            bound.treatment == CostTreatment::DeductedFromCredit
                && bound.location == place.location
                && bound.amount.asset == place.asset
        })
        .fold(BigUint::zero(), |sum, bound| {
            sum + bound.amount.atoms.integer()
        });
    if credit.minimum < withheld {
        return Err(provenance_error());
    }
    credit.minimum -= withheld;
    Ok(credit)
}

fn consume_flow_source(
    lots: &mut [IntervalLot],
    state: &ProjectedState,
    deltas: &StepDeltas,
    place: &Place,
    requested: Interval,
    preferred_origin: Option<&str>,
    borrowed_first: bool,
) -> Result<Vec<(String, Interval)>> {
    if let Some(cap) = deltas.cash_sweeps.get(place) {
        if projected_interval(state, place)?.maximum > *cap {
            return Err(provenance_error());
        }
        return Ok(take_all(lots, place));
    }
    if let Some(cap) = deltas.position_closes.get(place) {
        if projected_interval(state, place)?.maximum > *cap {
            return Err(provenance_error());
        }
        return Ok(take_all(lots, place));
    }
    if borrowed_first {
        consume_borrowed_first_lots(lots, place, requested)
    } else {
        consume_lots(lots, place, requested, preferred_origin)
    }
}

fn explicit_debit_cap(proof: &ReturnRouteProof, place: &Place) -> BigUint {
    proof
        .intent
        .expected
        .iter()
        .filter_map(|expected| match expected {
            EffectConstraint::Debit {
                resource,
                asset,
                maximum_debit,
            } if place.location
                == AccountingLocation::Cash {
                    resource: resource.clone(),
                }
                && &place.asset == asset =>
            {
                Some(maximum_debit.integer())
            }
            _ => None,
        })
        .fold(BigUint::zero(), |sum, amount| sum + amount)
}

fn has_accrual(proof: &ReturnRouteProof, place: &Place) -> bool {
    proof.intent.expected.iter().any(|expected| match expected {
        EffectConstraint::Accrual {
            resource,
            position,
            asset,
            position_kind,
            ..
        } => {
            place.location
                == (AccountingLocation::Position {
                    resource: resource.clone(),
                    position: position.clone(),
                    position_kind: position_kind.clone(),
                })
                && &place.asset == asset
        }
        _ => false,
    })
}

fn sum_at(lots: &[IntervalLot], place: &Place) -> Interval {
    lots.iter()
        .filter(|lot| lot.place() == *place)
        .fold(Interval::zero(), |mut sum, lot| {
            sum.minimum += &lot.minimum;
            sum.maximum += &lot.maximum;
            sum
        })
}

fn push_lot(lots: &mut Vec<IntervalLot>, place: &Place, origin: &str, amount: Interval) {
    if amount.maximum.is_zero() {
        return;
    }
    // Preserve FIFO lot order exactly. Merging A1,B1,A1 into A2,B1 changes
    // which origin a later partial debit consumes.
    lots.push(IntervalLot {
        origin: origin.to_owned(),
        location: place.location.clone(),
        asset: place.asset.clone(),
        minimum: amount.minimum,
        maximum: amount.maximum,
    });
}

fn check_terminal(
    lots: &[IntervalLot],
    state: &ProjectedState,
    selected: &[SelectedOrigin],
    proofs: &[ReturnRouteProof],
    prices: &[AssetPrice],
    reserve: &BigUint,
) -> Result<()> {
    let selected_by_origin = selected
        .iter()
        .map(|origin| (origin.origin.clone(), origin))
        .collect::<BTreeMap<_, _>>();
    let mut lots_by_place = BTreeMap::<Place, (BigUint, BigUint)>::new();
    for lot in lots {
        if lot.maximum.is_zero() {
            continue;
        }
        let root_origin = strip_accrual(&lot.origin);
        let Some(origin) = selected_by_origin.get(root_origin) else {
            return Err(provenance_error());
        };
        if lot.place() != origin.home || !matches!(lot.location, AccountingLocation::Cash { .. }) {
            return Err(provenance_error());
        }
        let entry = lots_by_place.entry(lot.place()).or_default();
        entry.0 += &lot.minimum;
        entry.1 += &lot.maximum;
    }
    let mut returned = BTreeMap::<String, BigUint>::new();
    let places = lots_by_place
        .keys()
        .cloned()
        .chain(state.cash.iter().map(|row| Place {
            location: AccountingLocation::Cash {
                resource: row.resource.clone(),
            },
            asset: row.asset.clone(),
        }))
        .collect::<BTreeSet<_>>();
    for place in places {
        let projected = projected_interval(state, &place)?;
        let (minimum, maximum) = lots_by_place.get(&place).cloned().unwrap_or_default();
        if minimum > projected.minimum || maximum < projected.maximum {
            return Err(provenance_error());
        }
        if projected.minimum.is_zero() && projected.maximum.is_zero() {
            continue;
        }
        let origin = selected_by_origin
            .values()
            .find(|origin| origin.home == place)
            .ok_or_else(provenance_error)?;
        returned.insert(origin.origin.clone(), projected.minimum);
    }
    let mut shortfall_usd = BigUint::zero();
    for origin in selected {
        let minimum = returned.get(&origin.origin).cloned().unwrap_or_default();
        if minimum < origin.start {
            shortfall_usd +=
                value_usd_micros(&origin.home.asset, &(&origin.start - &minimum), prices)?;
        }
    }
    if shortfall_usd > *reserve {
        return Err(provenance_error());
    }
    let _ = proofs; // Per-step finite cost ceilings are validated by the run planner.
    Ok(())
}

fn strip_accrual(origin: &str) -> &str {
    origin
        .strip_prefix("accrual:")
        .map(strip_accrual)
        .unwrap_or(origin)
}

fn value_usd_micros(asset: &AssetId, amount: &BigUint, prices: &[AssetPrice]) -> Result<BigUint> {
    super::value_usd_micros_ceil(asset, amount, prices)
}

fn sub_floor(left: &BigUint, right: &BigUint) -> BigUint {
    if left > right {
        left - right
    } else {
        BigUint::zero()
    }
}

fn projected_interval(state: &ProjectedState, place: &Place) -> Result<Interval> {
    match &place.location {
        AccountingLocation::Cash { resource } => {
            let rows = state
                .cash
                .iter()
                .filter(|row| row.resource == *resource && row.asset == place.asset)
                .collect::<Vec<_>>();
            if rows.len() > 1 {
                return Err(provenance_error());
            }
            Ok(rows
                .first()
                .map(|row| Interval {
                    minimum: row.amount.minimum.integer(),
                    maximum: row.amount.maximum.integer(),
                })
                .unwrap_or_else(Interval::zero))
        }
        AccountingLocation::Position {
            resource,
            position,
            position_kind,
        } => {
            let rows = state
                .positions
                .iter()
                .filter(|row| {
                    row.resource == *resource
                        && row.position == *position
                        && row.asset == place.asset
                        && row.position_kind == *position_kind
                })
                .collect::<Vec<_>>();
            if rows.len() > 1 {
                return Err(provenance_error());
            }
            Ok(rows
                .first()
                .map(|row| Interval {
                    minimum: row.amount.minimum.integer(),
                    maximum: row.amount.maximum.integer(),
                })
                .unwrap_or_else(Interval::zero))
        }
    }
}

fn apply_state_deltas(state: &mut ProjectedState, deltas: &StepDeltas) -> Result<()> {
    for (place, (minimum, maximum)) in &deltas.cash {
        let current = projected_interval(state, place)?;
        set_projected_interval(state, place, apply_delta(&current, minimum, maximum)?)?;
    }
    for (place, (minimum, maximum)) in &deltas.positions {
        let current = projected_interval(state, place)?;
        set_projected_interval(state, place, apply_delta(&current, minimum, maximum)?)?;
    }
    for place in deltas.cash_sweeps.keys() {
        set_projected_interval(state, place, Interval::zero())?;
    }
    for place in deltas.position_closes.keys() {
        set_projected_interval(state, place, Interval::zero())?;
    }
    Ok(())
}

fn apply_delta(current: &Interval, minimum: &BigInt, maximum: &BigInt) -> Result<Interval> {
    let next_minimum = BigInt::from(current.minimum.clone()) + minimum;
    let next_maximum = BigInt::from(current.maximum.clone()) + maximum;
    if next_minimum.sign() == Sign::Minus || next_maximum < next_minimum {
        return Err(provenance_error());
    }
    Ok(Interval {
        minimum: next_minimum.magnitude().clone(),
        maximum: next_maximum.magnitude().clone(),
    })
}

fn set_projected_interval(
    state: &mut ProjectedState,
    place: &Place,
    amount: Interval,
) -> Result<()> {
    match &place.location {
        AccountingLocation::Cash { resource } => {
            state
                .cash
                .retain(|row| row.resource != *resource || row.asset != place.asset);
            state.cash.push(gofin_domain::ProjectedCash {
                resource: resource.clone(),
                asset: place.asset.clone(),
                amount: gofin_domain::QuantityInterval {
                    minimum: Quantity::new(amount.minimum.to_string())?,
                    maximum: Quantity::new(amount.maximum.to_string())?,
                },
            });
        }
        AccountingLocation::Position {
            resource,
            position,
            position_kind,
        } => {
            state.positions.retain(|row| {
                row.resource != *resource
                    || row.position != *position
                    || row.asset != place.asset
                    || row.position_kind != *position_kind
            });
            state.positions.push(gofin_domain::ProjectedPosition {
                resource: resource.clone(),
                position: position.clone(),
                asset: place.asset.clone(),
                position_kind: position_kind.clone(),
                amount: gofin_domain::QuantityInterval {
                    minimum: Quantity::new(amount.minimum.to_string())?,
                    maximum: Quantity::new(amount.maximum.to_string())?,
                },
                backing_valuation: None,
            });
        }
    }
    Ok(())
}

fn validate_lots_fit_state(
    lots: &[IntervalLot],
    state: &ProjectedState,
    deltas: &StepDeltas,
) -> Result<()> {
    let touched = deltas
        .cash
        .keys()
        .chain(deltas.positions.keys())
        .chain(deltas.cash_sweeps.keys())
        .chain(deltas.position_closes.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for place in touched {
        if matches!(
            place.location,
            AccountingLocation::Position {
                position_kind: PositionKind::StableDebt
                    | PositionKind::VariableDebt
                    | PositionKind::DerivativeLong
                    | PositionKind::DerivativeShort,
                ..
            }
        ) {
            continue;
        }
        let total = sum_at(lots, &place);
        let projected = projected_interval(state, &place)?;
        if total.minimum > projected.minimum || total.maximum < projected.maximum {
            return Err(provenance_error());
        }
    }
    Ok(())
}

fn provenance_error() -> gofin_domain::GoFinError {
    gofin_domain::GoFinError::new(
        "RETURN_ROUTE_PROVENANCE_INVALID",
        "budget.restoration.provenance",
        "The projected route cannot prove that every selected origin returns within its finite cost reserve.",
        "Revise the route to preserve selected origins and close all borrowed cash and positions.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{
        AssetId, CostBound, EffectConstraint, FinancialIntent, ProjectedEffect, QuantityInterval,
        ResourceId,
    };
    use serde_json::json;

    fn asset() -> AssetId {
        AssetId {
            network: "synthetic".into(),
            reference: "USD".into(),
            decimals: 0,
        }
    }

    fn place(name: &str) -> Place {
        Place {
            location: AccountingLocation::Cash {
                resource: ResourceId(name.into()),
            },
            asset: asset(),
        }
    }

    fn allocation_origin(name: &str) -> String {
        format!(
            "allocation:{}",
            serde_json::to_string(&(name.to_owned(), asset())).unwrap()
        )
    }

    fn q(value: &str) -> Quantity {
        Quantity::new(value).unwrap()
    }

    fn concrete_shares(
        sources: &[(String, Interval)],
        amounts: &[BigUint],
        output: &BigUint,
        same_asset: bool,
    ) -> Vec<(String, BigUint)> {
        let location = AccountingLocation::Cash {
            resource: ResourceId("share-test".into()),
        };
        let output_asset = if same_asset {
            asset()
        } else {
            AssetId {
                network: "synthetic".into(),
                reference: "OUT".into(),
                decimals: 0,
            }
        };
        let lots = sources
            .iter()
            .zip(amounts)
            .map(|((origin, _), amount)| ProvenanceLot {
                origin: origin.clone(),
                location: location.clone(),
                asset: asset(),
                atoms: amount.to_string(),
            })
            .collect::<Vec<_>>();
        super::super::project_lots(&lots, &location, &output_asset, output)
            .unwrap()
            .into_iter()
            .map(|lot| (lot.origin, super::super::parse_uint(&lot.atoms).unwrap()))
            .collect()
    }

    fn cash_state(name: &str, amount: &str) -> ProjectedState {
        ProjectedState {
            cash: vec![gofin_domain::ProjectedCash {
                resource: ResourceId(name.into()),
                asset: asset(),
                amount: QuantityInterval {
                    minimum: q(amount),
                    maximum: q(amount),
                },
            }],
            ..Default::default()
        }
    }

    fn proof(
        step_index: u32,
        intent: FinancialIntent,
        effects: Vec<ProjectedEffect>,
    ) -> ReturnRouteProof {
        ReturnRouteProof {
            step_index,
            operation: "synthetic.test".into(),
            input_hash: "synthetic-hash".into(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:01:00Z".into(),
            intent,
            effects,
            requires: vec![],
            evidence: json!({"synthetic": true}),
        }
    }

    #[test]
    fn custody_cycle_preserves_child_principal_and_local_fee_stocks_across_36_to_18_atoms() {
        let venue = AssetId {
            network: "binance".into(),
            reference: "ETH".into(),
            decimals: 36,
        };
        let native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let quantity =
            |value: &str, asset: &AssetId| Quantity::from_decimal(value, asset.decimals).unwrap();
        let allocation = |name: &str, asset: &AssetId, value: &str| RunAllocation {
            holding: gofin_domain::ResourceAmount {
                resource: ResourceId(name.into()),
                amount: gofin_domain::Amount {
                    asset: asset.clone(),
                    atoms: quantity(value, asset),
                },
            },
            value_usd_micros: "1000000".into(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic exact stock".into(),
        };
        let allocations = vec![
            allocation("main", &venue, "0.001"),
            allocation("child-a", &venue, "0.005"),
            allocation("child-b", &venue, "0.010"),
            allocation("wallet", &native, "0.020"),
        ];
        let baseline = StateSnapshot {
            state: gofin_domain::FinancialState {
                holdings: allocations.iter().map(|row| row.holding.clone()).collect(),
                ..Default::default()
            },
            observed_at: "2026-01-01T00:00:00Z".into(),
            complete: true,
            coverage: allocations
                .iter()
                .map(|row| row.holding.resource.0.clone())
                .collect(),
            pending: vec![],
        };
        let transfer = |step,
                        from: &str,
                        to: &str,
                        debit_asset: &AssetId,
                        credit_asset: &AssetId,
                        amount: &str,
                        fee: &str| {
            let from = gofin_domain::ResourceId(from.into());
            let to = gofin_domain::ResourceId(to.into());
            let debit = quantity(amount, debit_asset);
            let credit = quantity(amount, credit_asset);
            let mut intent = FinancialIntent {
                debits: vec![gofin_domain::ResourceAmount {
                    resource: from.clone(),
                    amount: gofin_domain::Amount {
                        asset: debit_asset.clone(),
                        atoms: debit.clone(),
                    },
                }],
                expected: vec![EffectConstraint::Transfer {
                    from: from.clone(),
                    to: to.clone(),
                    debit_asset: debit_asset.clone(),
                    credit_asset: credit_asset.clone(),
                    maximum_debit: debit.clone(),
                    minimum_credit: credit.clone(),
                }],
                ..Default::default()
            };
            if fee != "0" {
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: from.clone(),
                    },
                    amount: gofin_domain::Amount {
                        asset: debit_asset.clone(),
                        atoms: quantity(fee, debit_asset),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            let mut proof = proof(
                step,
                intent,
                vec![
                    ProjectedEffect::CashDelta {
                        resource: from,
                        asset: debit_asset.clone(),
                        minimum_delta: gofin_domain::SignedQuantity::negative(&debit),
                        maximum_delta: gofin_domain::SignedQuantity::negative(&debit),
                    },
                    ProjectedEffect::CashDelta {
                        resource: to,
                        asset: credit_asset.clone(),
                        minimum_delta: gofin_domain::SignedQuantity::positive(&credit),
                        maximum_delta: gofin_domain::SignedQuantity::positive(&credit),
                    },
                ],
            );
            super::super::include_costs_in_projected_cash(&mut proof).unwrap();
            proof
        };
        let proofs = vec![
            transfer(0, "child-a", "main", &venue, &venue, "0.005", "0"),
            transfer(1, "child-b", "main", &venue, &venue, "0.010", "0"),
            transfer(2, "main", "wallet", &venue, &native, "0.015", "0.001"),
            transfer(3, "wallet", "main", &native, &venue, "0.015", "0.001"),
            transfer(4, "main", "child-a", &venue, &venue, "0.005", "0"),
            transfer(5, "main", "child-b", &venue, &venue, "0.010", "0"),
        ];
        let prices = [&venue, &native]
            .into_iter()
            .map(|asset| AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: q("2000000000"),
                unit_atoms: quantity("1", asset),
                observed_at: "2026-01-01T00:00:00Z".into(),
                source: "synthetic exact ETH price".into(),
            })
            .collect::<Vec<_>>();
        validate_projected_origins(
            &baseline,
            &allocations,
            &proofs,
            &prices,
            &BigUint::from(5_000_000u32),
        )
        .unwrap();
        let selected = selected_origins(&baseline, &allocations).unwrap();
        let mut lots = selected
            .iter()
            .map(|origin| IntervalLot {
                origin: origin.origin.clone(),
                location: origin.home.location.clone(),
                asset: origin.home.asset.clone(),
                minimum: origin.start.clone(),
                maximum: origin.start.clone(),
            })
            .collect::<Vec<_>>();
        let mut state = selected_state(&allocations).unwrap();
        replay_steps(&mut lots, &mut state, &proofs, &prices, 0).unwrap();
        for origin in &selected {
            assert!(
                lots.iter()
                    .filter(|lot| lot.origin == origin.origin)
                    .all(|lot| lot.place() == origin.home)
            );
            if origin.home.location.resource().0.starts_with("child-") {
                assert_eq!(
                    sum_at(&lots, &origin.home),
                    Interval::exact(origin.start.clone())
                );
            }
        }
    }

    #[test]
    fn mixed_origin_remainder_intervals_enclose_all_small_outputs() {
        let sources = ["a", "b", "c"]
            .into_iter()
            .map(|name| (allocation_origin(name), Interval::exact(BigUint::from(1u8))))
            .collect::<Vec<_>>();
        let projected = project_origin_intervals(
            &sources,
            &Interval {
                minimum: BigUint::from(2u8),
                maximum: BigUint::from(3u8),
            },
            false,
        )
        .unwrap();
        for output in 2u8..=3 {
            let concrete = concrete_shares(
                &sources,
                &[BigUint::from(1u8), BigUint::from(1u8), BigUint::from(1u8)],
                &BigUint::from(output),
                false,
            );
            for (origin, actual) in concrete {
                if let Some((_, range)) = projected.iter().find(|(label, _)| label == &origin) {
                    assert!(
                        range.minimum <= actual && actual <= range.maximum,
                        "{origin}"
                    );
                } else {
                    assert!(actual.is_zero(), "{origin}");
                }
            }
        }
        for count in 2..=3 {
            let choices = 3usize.pow(count as u32);
            for mut encoded_bounds in 0..choices {
                let mut bounds = Vec::with_capacity(count);
                for _index in 0..count {
                    let low = (encoded_bounds % 3) as u8;
                    encoded_bounds /= 3;
                    bounds.push((low, low + 1));
                }
                let sources = bounds
                    .iter()
                    .enumerate()
                    .map(|(index, (low, high))| {
                        (
                            format!("uncertain-{index}"),
                            Interval {
                                minimum: BigUint::from(*low),
                                maximum: BigUint::from(*high),
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let output = Interval {
                    minimum: BigUint::from(0u8),
                    maximum: BigUint::from(6u8),
                };
                let enclosure = project_origin_intervals(&sources, &output, false).unwrap();
                let concrete_inputs =
                    bounds
                        .iter()
                        .fold(vec![Vec::<u8>::new()], |states, (low, high)| {
                            states
                                .into_iter()
                                .flat_map(|prefix| {
                                    (*low..=*high).map(move |value| {
                                        let mut next = prefix.clone();
                                        next.push(value);
                                        next
                                    })
                                })
                                .collect()
                        });
                for inputs in concrete_inputs {
                    let total_input = inputs.iter().copied().sum::<u8>();
                    for output_value in 0u8..=6 {
                        if total_input == 0 && output_value != 0 {
                            continue;
                        }
                        let concrete = concrete_shares(
                            &sources,
                            &inputs
                                .iter()
                                .map(|value| BigUint::from(*value))
                                .collect::<Vec<_>>(),
                            &BigUint::from(output_value),
                            false,
                        );
                        for (origin, actual) in concrete {
                            if let Some((_, range)) =
                                enclosure.iter().find(|(label, _)| label == &origin)
                            {
                                assert!(
                                    range.minimum <= actual && actual <= range.maximum,
                                    "{origin}, inputs={inputs:?}, output={output_value}"
                                );
                            } else {
                                assert!(
                                    actual.is_zero(),
                                    "{origin}, inputs={inputs:?}, output={output_value}"
                                );
                            }
                        }
                    }
                }
            }
        }
        for count in 2..=4 {
            let possibilities = 3usize.pow(count as u32);
            for mut encoding in 0..possibilities {
                let mut concrete_sources = Vec::with_capacity(count);
                for index in 0..count {
                    let amount = (encoding % 3 + 1) as u8;
                    encoding /= 3;
                    concrete_sources.push((
                        format!("origin-{index}"),
                        Interval::exact(BigUint::from(amount)),
                    ));
                }
                let total = concrete_sources
                    .iter()
                    .fold(BigUint::zero(), |sum, (_, amount)| sum + &amount.minimum);
                let max_output = &total + BigUint::from(2u8);
                let enclosure = project_origin_intervals(
                    &concrete_sources,
                    &Interval {
                        minimum: BigUint::from(1u8),
                        maximum: max_output.clone(),
                    },
                    false,
                )
                .unwrap();
                let max_output_small = concrete_sources.iter().fold(2u8, |sum, (_, amount)| {
                    sum + amount.minimum.to_string().parse::<u8>().unwrap()
                });
                for output in 1u8..=max_output_small {
                    let inputs = concrete_sources
                        .iter()
                        .map(|(_, amount)| amount.minimum.clone())
                        .collect::<Vec<_>>();
                    let actual =
                        concrete_shares(&concrete_sources, &inputs, &BigUint::from(output), false);
                    for (origin, quantity) in actual {
                        if let Some((_, range)) =
                            enclosure.iter().find(|(label, _)| label == &origin)
                        {
                            assert!(
                                range.minimum <= quantity && quantity <= range.maximum,
                                "{origin}"
                            );
                        } else {
                            assert!(quantity.is_zero(), "{origin}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn provenance_lots_keep_fifo_identity_across_repeated_origins() {
        let source = place("wallet");
        let a = allocation_origin("a");
        let b = allocation_origin("b");
        let mut lots = vec![];
        for origin in [&a, &b, &a] {
            push_lot(
                &mut lots,
                &source,
                origin,
                Interval::exact(BigUint::from(1u8)),
            );
        }
        let taken = consume_lots(
            &mut lots,
            &source,
            Interval::exact(BigUint::from(2u8)),
            None,
        )
        .unwrap();
        assert_eq!(
            taken.iter().map(|lot| lot.0.as_str()).collect::<Vec<_>>(),
            [&a, &b]
        );
        assert_eq!(lots.len(), 3);
        assert!(lots[0].maximum.is_zero());
        assert!(lots[1].maximum.is_zero());
        lots.retain(|lot| !lot.maximum.is_zero());
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].origin, a);
        assert_eq!(lots[0].minimum, BigUint::from(1u8));
    }

    #[test]
    fn repayment_borrow_preference_does_not_leak_into_later_fifo_selection() {
        let source = place("source");
        let elsewhere = place("elsewhere");
        let mut lots = vec![
            IntervalLot {
                origin: "borrowed:unrelated".into(),
                location: elsewhere.location.clone(),
                asset: asset(),
                minimum: BigUint::from(1u8),
                maximum: BigUint::from(1u8),
            },
            IntervalLot {
                origin: "borrowed:loan".into(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(2u8),
                maximum: BigUint::from(2u8),
            },
            IntervalLot {
                origin: "allocation:Z".into(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
            IntervalLot {
                origin: "allocation:elsewhere".into(),
                location: elsewhere.location.clone(),
                asset: asset(),
                minimum: BigUint::from(3u8),
                maximum: BigUint::from(3u8),
            },
            IntervalLot {
                origin: "allocation:A".into(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
        ];
        let elsewhere_before = lots[0].clone();
        let repaid =
            consume_borrowed_first_lots(&mut lots, &source, Interval::exact(BigUint::from(1u8)))
                .unwrap();
        assert_eq!(repaid[0].0, "borrowed:loan");
        assert_eq!(lots[0], elsewhere_before);
        assert_eq!(lots[1].origin, "borrowed:loan");
        assert_eq!(lots[2].origin, "allocation:Z");
        assert_eq!(lots[3].origin, "allocation:elsewhere");
        assert_eq!(lots[4].origin, "allocation:A");
        let ordinary = consume_lots(
            &mut lots,
            &source,
            Interval::exact(BigUint::from(1u8)),
            None,
        )
        .unwrap();
        assert_eq!(ordinary[0].0, "allocation:Z");
        assert_eq!(lots[1].origin, "borrowed:loan");
    }

    #[test]
    fn transfer_recipient_preference_includes_nested_accrual_descendants() {
        let source = place("intermediate");
        let recipient = ResourceId("recipient".into());
        let destination = Place {
            location: AccountingLocation::Cash {
                resource: recipient.clone(),
            },
            asset: asset(),
        };
        let origin = allocation_origin("recipient");
        let flow = Flow {
            from: source.clone(),
            to: destination.clone(),
            maximum_debit: BigUint::from(5u8),
            minimum_credit: BigUint::from(5u8),
            conversion: false,
        };
        let preferred = transfer_recipient_origin(&flow).unwrap();
        let descendant = format!("accrual:accrual:{origin}");
        let unrelated = allocation_origin("other");
        let mut lots = vec![
            IntervalLot {
                origin: unrelated.clone(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
            IntervalLot {
                origin: descendant.clone(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
        ];
        let moved = consume_lots(
            &mut lots,
            &source,
            Interval::exact(BigUint::from(5u8)),
            Some(&preferred),
        )
        .unwrap();
        assert_eq!(moved[0].0, descendant);
        assert_eq!(lots.len(), 2);
        assert!(lots[1].maximum.is_zero());
        lots.retain(|lot| !lot.maximum.is_zero());
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].origin, unrelated);
    }

    #[test]
    fn final_deficit_is_priced_per_origin_and_tiny_reserve_rejects_large_loss() {
        let home = place("selected");
        let origin = SelectedOrigin {
            origin: allocation_origin("selected"),
            home: home.clone(),
            start: BigUint::from(100u8),
        };
        let lot = IntervalLot {
            origin: origin.origin.clone(),
            location: home.location.clone(),
            asset: home.asset.clone(),
            minimum: BigUint::from(1u8),
            maximum: BigUint::from(1u8),
        };
        let prices = vec![AssetPrice {
            asset: asset(),
            usd_micros_per_unit: q("100"),
            unit_atoms: q("100"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic".into(),
        }];
        assert!(
            check_terminal(
                std::slice::from_ref(&lot),
                &cash_state("selected", "1"),
                std::slice::from_ref(&origin),
                &[],
                &prices,
                &BigUint::from(98u8)
            )
            .is_err()
        );
        assert!(
            check_terminal(
                &[lot],
                &cash_state("selected", "1"),
                &[origin],
                &[],
                &prices,
                &BigUint::from(99u8)
            )
            .is_ok()
        );
    }

    #[test]
    fn equal_aggregate_cash_at_wrong_selected_home_does_not_restore_origins() {
        let home_a = place("account-a");
        let home_b = place("account-b");
        let selected = vec![
            SelectedOrigin {
                origin: allocation_origin("account-a"),
                home: home_a.clone(),
                start: BigUint::from(5u8),
            },
            SelectedOrigin {
                origin: allocation_origin("account-b"),
                home: home_b.clone(),
                start: BigUint::from(5u8),
            },
        ];
        let lots = vec![
            IntervalLot {
                origin: selected[0].origin.clone(),
                location: home_b.location.clone(),
                asset: home_b.asset.clone(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
            IntervalLot {
                origin: selected[1].origin.clone(),
                location: home_b.location.clone(),
                asset: home_b.asset.clone(),
                minimum: BigUint::from(5u8),
                maximum: BigUint::from(5u8),
            },
        ];
        let state = ProjectedState {
            cash: vec![gofin_domain::ProjectedCash {
                resource: ResourceId("account-b".into()),
                asset: asset(),
                amount: QuantityInterval {
                    minimum: q("10"),
                    maximum: q("10"),
                },
            }],
            ..Default::default()
        };
        let prices = vec![AssetPrice {
            asset: asset(),
            usd_micros_per_unit: q("100"),
            unit_atoms: q("100"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic".into(),
        }];
        assert!(check_terminal(&lots, &state, &selected, &[], &prices, &BigUint::zero()).is_err());
    }

    #[test]
    fn cost_only_gas_consumes_selected_cash_once() {
        let source = place("selected");
        let mut lots = vec![IntervalLot {
            origin: allocation_origin("selected"),
            location: source.location.clone(),
            asset: source.asset.clone(),
            minimum: BigUint::from(10u8),
            maximum: BigUint::from(10u8),
        }];
        let cost = CostBound {
            location: source.location.clone(),
            amount: gofin_domain::Amount {
                asset: source.asset.clone(),
                atoms: q("2"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut proof = proof(
            0,
            FinancialIntent {
                maximum_costs: vec![cost],
                ..Default::default()
            },
            vec![],
        );
        super::super::include_costs_in_projected_cash(&mut proof).unwrap();
        let mut state = cash_state("selected", "10");
        replay_step(&mut lots, &mut state, &proof, &[], 0).unwrap();
        assert_eq!(
            sum_at(&lots, &source),
            Interval {
                minimum: 8u8.into(),
                maximum: 10u8.into()
            }
        );
    }

    #[test]
    fn normalized_transfer_earmarks_local_fee_before_principal() {
        let from = ResourceId("source".into());
        let to = ResourceId("destination".into());
        let source = Place {
            location: AccountingLocation::Cash {
                resource: from.clone(),
            },
            asset: asset(),
        };
        let destination = Place {
            location: AccountingLocation::Cash {
                resource: to.clone(),
            },
            asset: asset(),
        };
        let fee = CostBound {
            location: source.location.clone(),
            amount: gofin_domain::Amount {
                asset: asset(),
                atoms: q("2"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut transfer = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::Transfer {
                    from: from.clone(),
                    to: to.clone(),
                    debit_asset: asset(),
                    credit_asset: asset(),
                    maximum_debit: q("100"),
                    minimum_credit: q("100"),
                }],
                maximum_costs: vec![fee],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: from.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: to.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                },
            ],
        );
        super::super::include_costs_in_projected_cash(&mut transfer).unwrap();
        let a = format!(
            "allocation:{}",
            serde_json::to_string(&("source-a".to_owned(), asset())).unwrap()
        );
        let b = format!(
            "allocation:{}",
            serde_json::to_string(&("source".to_owned(), asset())).unwrap()
        );
        let mut lots = vec![
            IntervalLot {
                origin: a.clone(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(100u8),
                maximum: BigUint::from(100u8),
            },
            IntervalLot {
                origin: b.clone(),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(2u8),
                maximum: BigUint::from(2u8),
            },
        ];
        let mut state = ProjectedState {
            cash: vec![
                gofin_domain::ProjectedCash {
                    resource: from,
                    asset: asset(),
                    amount: QuantityInterval {
                        minimum: q("102"),
                        maximum: q("102"),
                    },
                },
                gofin_domain::ProjectedCash {
                    resource: to,
                    asset: asset(),
                    amount: QuantityInterval {
                        minimum: q("0"),
                        maximum: q("0"),
                    },
                },
            ],
            ..Default::default()
        };
        replay_step(&mut lots, &mut state, &transfer, &[], 0).unwrap();
        assert_eq!(
            sum_at(&lots, &source),
            Interval {
                minimum: BigUint::zero(),
                maximum: BigUint::from(2u8)
            }
        );
        assert_eq!(
            sum_at(&lots, &destination),
            Interval::exact(BigUint::from(100u8))
        );
        assert!(
            lots.iter()
                .any(|lot| lot.origin == a && lot.place() == destination)
        );
        assert!(
            lots.iter()
                .filter(|lot| lot.origin == b)
                .all(|lot| lot.place() == source)
        );
        assert!(
            lots.iter()
                .filter(|lot| lot.origin == b)
                .map(|lot| lot.maximum.clone())
                .fold(BigUint::zero(), |sum, amount| sum + amount)
                <= BigUint::from(2u8)
        );
    }

    #[test]
    fn normalized_destination_delta_reconstructs_gross_credit_and_consumes_selected_fee() {
        let source_resource = ResourceId("input".into());
        let destination_resource = ResourceId("wallet".into());
        let source = place("input");
        let destination = place("wallet");
        let fee = CostBound {
            location: destination.location.clone(),
            amount: gofin_domain::Amount {
                asset: asset(),
                atoms: q("2"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut proof = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::OriginMovement {
                    from: source.location.clone(),
                    to: destination.location.clone(),
                    debit_asset: asset(),
                    credit_asset: asset(),
                    maximum_debit: q("10"),
                    minimum_credit: q("10"),
                }],
                maximum_costs: vec![fee],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: source_resource.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-10").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-10").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: destination_resource.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("10").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("10").unwrap(),
                },
            ],
        );
        super::super::include_costs_in_projected_cash(&mut proof).unwrap();
        let mut lots = vec![
            IntervalLot {
                origin: allocation_origin("input"),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(10u8),
                maximum: BigUint::from(10u8),
            },
            IntervalLot {
                origin: allocation_origin("wallet"),
                location: destination.location.clone(),
                asset: asset(),
                minimum: BigUint::from(2u8),
                maximum: BigUint::from(2u8),
            },
        ];
        let mut state = ProjectedState {
            cash: vec![
                gofin_domain::ProjectedCash {
                    resource: source_resource,
                    asset: asset(),
                    amount: QuantityInterval {
                        minimum: q("10"),
                        maximum: q("10"),
                    },
                },
                gofin_domain::ProjectedCash {
                    resource: destination_resource,
                    asset: asset(),
                    amount: QuantityInterval {
                        minimum: q("2"),
                        maximum: q("2"),
                    },
                },
            ],
            ..Default::default()
        };
        replay_step(&mut lots, &mut state, &proof, &[], 0).unwrap();
        assert_eq!(sum_at(&lots, &source), Interval::zero());
        assert_eq!(
            sum_at(&lots, &destination),
            Interval {
                minimum: BigUint::from(10u8),
                maximum: BigUint::from(12u8)
            }
        );
        assert_eq!(
            projected_interval(&state, &destination).unwrap(),
            Interval {
                minimum: BigUint::from(10u8),
                maximum: BigUint::from(12u8)
            }
        );
    }

    #[test]
    fn gross_credit_reconstruction_can_cross_zero_after_native_fee_normalization() {
        let native = asset();
        let wrapped = AssetId {
            network: "synthetic".into(),
            reference: "WETH".into(),
            decimals: 18,
        };
        let resource = ResourceId("wallet".into());
        let destination = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: native.clone(),
        };
        let mut proof = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: wrapped,
                    maximum_input: q("1"),
                    output: native.clone(),
                    minimum_output: q("1"),
                }],
                maximum_costs: vec![CostBound {
                    location: destination.location.clone(),
                    amount: gofin_domain::Amount {
                        asset: native.clone(),
                        atoms: q("2"),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                ..Default::default()
            },
            vec![ProjectedEffect::CashDelta {
                resource: resource.clone(),
                asset: native,
                minimum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
                maximum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
            }],
        );
        super::super::include_costs_in_projected_cash(&mut proof).unwrap();
        let deltas = step_deltas(&proof).unwrap();
        assert_eq!(
            deltas.cash[&destination],
            (BigInt::from(-1), BigInt::from(1))
        );
        assert_eq!(
            gross_credit(&deltas, &proof, &destination, BigUint::from(1u8)).unwrap(),
            Interval::exact(BigUint::from(1u8))
        );
    }

    #[test]
    fn additional_fee_cannot_be_funded_only_by_borrowed_or_accrual_lots() {
        let source = place("selected");
        let cost = CostBound {
            location: source.location.clone(),
            amount: gofin_domain::Amount {
                asset: source.asset.clone(),
                atoms: q("2"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let accrual_proof = proof(
            0,
            FinancialIntent {
                maximum_costs: vec![cost],
                ..Default::default()
            },
            vec![],
        );
        for origin in [
            "borrowed:projected:0".to_owned(),
            format!("accrual:{}", allocation_origin("selected")),
        ] {
            let lots = vec![IntervalLot {
                origin,
                location: source.location.clone(),
                asset: source.asset.clone(),
                minimum: BigUint::from(2u8),
                maximum: BigUint::from(2u8),
            }];
            assert!(validate_cost_funding(&lots, &accrual_proof).is_err());
        }
        let selected_lots = vec![IntervalLot {
            origin: allocation_origin("selected"),
            location: source.location,
            asset: asset(),
            minimum: BigUint::from(2u8),
            maximum: BigUint::from(2u8),
        }];
        assert!(validate_cost_funding(&selected_lots, &accrual_proof).is_ok());
    }

    #[test]
    fn cost_caps_at_one_place_are_aggregated_before_borrowed_funds_are_considered() {
        let source = place("selected");
        let cost = || CostBound {
            location: source.location.clone(),
            amount: gofin_domain::Amount {
                asset: asset(),
                atoms: q("8"),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let proof = proof(
            0,
            FinancialIntent {
                maximum_costs: vec![cost(), cost()],
                ..Default::default()
            },
            vec![],
        );
        let lots = vec![
            IntervalLot {
                origin: allocation_origin("selected"),
                location: source.location.clone(),
                asset: asset(),
                minimum: BigUint::from(10u8),
                maximum: BigUint::from(10u8),
            },
            IntervalLot {
                origin: "borrowed:projected:1".into(),
                location: source.location,
                asset: asset(),
                minimum: BigUint::from(10u8),
                maximum: BigUint::from(10u8),
            },
        ];
        assert!(validate_cost_funding(&lots, &proof).is_err());
    }

    #[test]
    fn borrowed_proceeds_are_cash_origin_and_repayment_consumes_them_first() {
        let selected = place("selected");
        let debt = Place {
            location: AccountingLocation::Position {
                resource: ResourceId("lender".into()),
                position: "debt".into(),
                position_kind: PositionKind::VariableDebt,
            },
            asset: asset(),
        };
        let original = IntervalLot {
            origin: allocation_origin("selected"),
            location: selected.location.clone(),
            asset: selected.asset.clone(),
            minimum: BigUint::from(10u8),
            maximum: BigUint::from(10u8),
        };
        let borrow = proof(
            0,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::OriginMovement {
                        from: debt.location.clone(),
                        to: selected.location.clone(),
                        debit_asset: asset(),
                        credit_asset: asset(),
                        maximum_debit: q("1"),
                        minimum_credit: q("1"),
                    },
                    EffectConstraint::Debt {
                        resource: ResourceId("lender".into()),
                        debt: "debt".into(),
                        asset: asset(),
                        direction: Direction::Increase,
                        maximum_change: q("1"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: ResourceId("selected".into()),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
                },
                ProjectedEffect::PositionDelta {
                    resource: ResourceId("lender".into()),
                    position: "debt".into(),
                    asset: asset(),
                    position_kind: PositionKind::VariableDebt,
                    minimum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("1").unwrap(),
                },
            ],
        );
        let repay = proof(
            1,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::OriginMovement {
                        from: selected.location.clone(),
                        to: debt.location.clone(),
                        debit_asset: asset(),
                        credit_asset: asset(),
                        maximum_debit: q("1"),
                        minimum_credit: q("1"),
                    },
                    EffectConstraint::Debt {
                        resource: ResourceId("lender".into()),
                        debt: "debt".into(),
                        asset: asset(),
                        direction: Direction::Decrease,
                        maximum_change: q("1"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: ResourceId("selected".into()),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-1").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-1").unwrap(),
                },
                ProjectedEffect::PositionDelta {
                    resource: ResourceId("lender".into()),
                    position: "debt".into(),
                    asset: asset(),
                    position_kind: PositionKind::VariableDebt,
                    minimum_delta: gofin_domain::SignedQuantity::new("-1").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-1").unwrap(),
                },
            ],
        );
        let selected_origin = SelectedOrigin {
            origin: original.origin.clone(),
            home: selected.clone(),
            start: BigUint::from(10u8),
        };
        let mut lots = vec![original];
        let mut state = cash_state("selected", "10");
        replay_step(&mut lots, &mut state, &borrow, &[], 0).unwrap();
        replay_step(&mut lots, &mut state, &repay, &[], 1).unwrap();
        assert!(
            check_terminal(
                &lots,
                &state,
                &[selected_origin],
                &[borrow, repay],
                &[],
                &BigUint::zero()
            )
            .is_ok()
        );
    }

    #[test]
    fn seed_cash_must_match_every_receipt_derived_lot() {
        let selected = place("selected");
        let current = cash_state("selected", "10");
        let lot = IntervalLot {
            origin: allocation_origin("selected"),
            location: selected.location,
            asset: selected.asset,
            minimum: BigUint::from(9u8),
            maximum: BigUint::from(9u8),
        };
        assert!(validate_seed_projection(&[lot], &current).is_err());
    }

    #[test]
    fn unrelated_credit_cannot_hide_inside_a_valid_conversion_cycle_step() {
        let output_asset = AssetId {
            network: "synthetic".into(),
            reference: "WETH".into(),
            decimals: 18,
        };
        let resource = ResourceId("wallet".into());
        let input = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: asset(),
        };
        let conversion = proof(
            0,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::Conversion {
                        resource: resource.clone(),
                        input: asset(),
                        maximum_input: q("100"),
                        output: output_asset.clone(),
                        minimum_output: q("100"),
                    },
                    // This claimed credit is not linked to an origin movement;
                    // it cannot borrow the neighbouring conversion's flow.
                    EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: output_asset.clone(),
                        minimum_credit: q("100"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: output_asset,
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("200").unwrap(),
                },
            ],
        );
        let mut lots = vec![IntervalLot {
            origin: allocation_origin("wallet"),
            location: input.location,
            asset: asset(),
            minimum: BigUint::from(100u8),
            maximum: BigUint::from(100u8),
        }];
        let mut state = cash_state("wallet", "100");
        assert!(replay_step(&mut lots, &mut state, &conversion, &[], 0).is_err());
    }

    #[test]
    fn finite_conversion_cycle_returns_its_origin_with_bounded_loss_or_gain() {
        let token = AssetId {
            network: "synthetic".into(),
            reference: "WETH".into(),
            decimals: 18,
        };
        let resource = ResourceId("wallet".into());
        let native = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: asset(),
        };
        let wrapped = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: token.clone(),
        };
        let forward = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: asset(),
                    maximum_input: q("100"),
                    output: token.clone(),
                    minimum_output: q("90"),
                }],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: token.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("90").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("110").unwrap(),
                },
            ],
        );
        let reverse = proof(
            1,
            FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: token.clone(),
                    maximum_input: q("110"),
                    output: asset(),
                    minimum_output: q("90"),
                }],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashSweep {
                    resource: resource.clone(),
                    asset: token,
                    maximum_amount: q("110"),
                    source_step: 0,
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("90").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("110").unwrap(),
                },
            ],
        );
        let mut state = cash_state("wallet", "100");
        let mut lots = vec![IntervalLot {
            origin: allocation_origin("wallet"),
            location: native.location.clone(),
            asset: native.asset.clone(),
            minimum: BigUint::from(100u8),
            maximum: BigUint::from(100u8),
        }];
        replay_step(&mut lots, &mut state, &forward, &[], 0).unwrap();
        replay_step(&mut lots, &mut state, &reverse, &[], 1).unwrap();
        let prices = vec![AssetPrice {
            asset: asset(),
            usd_micros_per_unit: q("100"),
            unit_atoms: q("100"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic".into(),
        }];
        let selected = SelectedOrigin {
            origin: allocation_origin("wallet"),
            home: native,
            start: BigUint::from(100u8),
        };
        assert!(
            check_terminal(
                &lots,
                &state,
                &[selected],
                &[forward, reverse],
                &prices,
                &BigUint::from(10u8),
            )
            .is_ok()
        );
        assert_eq!(
            projected_interval(&state, &wrapped).unwrap(),
            Interval::zero()
        );
    }

    #[test]
    fn receipt_backed_supply_yield_and_full_return_preserve_origin() {
        let cash = place("selected");
        let position = Place {
            location: AccountingLocation::Position {
                resource: ResourceId("lender".into()),
                position: "supply".into(),
                position_kind: PositionKind::Supply,
            },
            asset: asset(),
        };
        let supply = proof(
            0,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::OriginMovement {
                        from: cash.location.clone(),
                        to: position.location.clone(),
                        debit_asset: asset(),
                        credit_asset: asset(),
                        maximum_debit: q("100"),
                        minimum_credit: q("99"),
                    },
                    EffectConstraint::Accrual {
                        resource: ResourceId("lender".into()),
                        position: "supply".into(),
                        asset: asset(),
                        position_kind: PositionKind::Supply,
                        maximum_change: q("2"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: ResourceId("selected".into()),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::PositionDelta {
                    resource: ResourceId("lender".into()),
                    position: "supply".into(),
                    asset: asset(),
                    position_kind: PositionKind::Supply,
                    minimum_delta: gofin_domain::SignedQuantity::new("99").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("99").unwrap(),
                },
            ],
        );
        let withdraw = proof(
            1,
            FinancialIntent {
                expected: vec![EffectConstraint::OriginMovement {
                    from: position.location.clone(),
                    to: cash.location.clone(),
                    debit_asset: asset(),
                    credit_asset: asset(),
                    maximum_debit: q("101"),
                    minimum_credit: q("99"),
                }],
                ..Default::default()
            },
            vec![
                ProjectedEffect::PositionClose {
                    resource: ResourceId("lender".into()),
                    position: "supply".into(),
                    asset: asset(),
                    position_kind: PositionKind::Supply,
                    maximum_amount: q("101"),
                    source_step: 0,
                },
                ProjectedEffect::CashDelta {
                    resource: ResourceId("selected".into()),
                    asset: asset(),
                    minimum_delta: gofin_domain::SignedQuantity::new("99").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("101").unwrap(),
                },
            ],
        );
        let mut state = cash_state("selected", "100");
        let mut lots = vec![IntervalLot {
            origin: allocation_origin("selected"),
            location: cash.location.clone(),
            asset: asset(),
            minimum: BigUint::from(100u8),
            maximum: BigUint::from(100u8),
        }];
        replay_step(&mut lots, &mut state, &supply, &[], 0).unwrap();
        replay_step(&mut lots, &mut state, &withdraw, &[], 1).unwrap();
        let returned = sum_at(&lots, &cash);
        assert_eq!(returned.minimum, BigUint::from(99u8));
        assert_eq!(returned.maximum, BigUint::from(101u8));
        assert!(
            lots.iter()
                .all(|lot| lot.origin == allocation_origin("selected")
                    || lot.origin == format!("accrual:{}", allocation_origin("selected")))
        );
        let selected_origin = SelectedOrigin {
            origin: allocation_origin("selected"),
            home: cash.clone(),
            start: BigUint::from(100u8),
        };
        let prices = vec![AssetPrice {
            asset: asset(),
            usd_micros_per_unit: q("100"),
            unit_atoms: q("100"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic".into(),
        }];
        assert!(
            check_terminal(
                &lots,
                &state,
                std::slice::from_ref(&selected_origin),
                &[supply, withdraw],
                &prices,
                &BigUint::from(1u8),
            )
            .is_ok()
        );
        assert!(
            check_terminal(
                &lots,
                &state,
                &[selected_origin],
                &[],
                &prices,
                &BigUint::zero(),
            )
            .is_err()
        );
    }

    #[test]
    fn adapter_shaped_supply_accrual_close_and_unwrap_stays_within_output_capacity() {
        let native = asset();
        let wrapped = AssetId {
            network: "synthetic".into(),
            reference: "WETH".into(),
            decimals: 18,
        };
        let resource = ResourceId("selected".into());
        let cash_native = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: native.clone(),
        };
        let cash_wrapped = Place {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            asset: wrapped.clone(),
        };
        let supply_position = Place {
            location: AccountingLocation::Position {
                resource: ResourceId("lender".into()),
                position: "supply".into(),
                position_kind: PositionKind::Supply,
            },
            asset: wrapped.clone(),
        };
        let forward = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: native.clone(),
                    maximum_input: q("100"),
                    output: wrapped.clone(),
                    minimum_output: q("100"),
                }],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: native.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: wrapped.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                },
            ],
        );
        let supply = proof(
            1,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::OriginMovement {
                        from: cash_wrapped.location.clone(),
                        to: supply_position.location.clone(),
                        debit_asset: wrapped.clone(),
                        credit_asset: wrapped.clone(),
                        maximum_debit: q("100"),
                        minimum_credit: q("100"),
                    },
                    EffectConstraint::Accrual {
                        resource: ResourceId("lender".into()),
                        position: "supply".into(),
                        asset: wrapped.clone(),
                        position_kind: PositionKind::Supply,
                        maximum_change: q("100"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: wrapped.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                },
                ProjectedEffect::PositionDelta {
                    resource: ResourceId("lender".into()),
                    position: "supply".into(),
                    asset: wrapped.clone(),
                    position_kind: PositionKind::Supply,
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                },
            ],
        );
        let withdraw = proof(
            2,
            FinancialIntent {
                expected: vec![
                    EffectConstraint::OriginMovement {
                        from: supply_position.location.clone(),
                        to: cash_wrapped.location.clone(),
                        debit_asset: wrapped.clone(),
                        credit_asset: wrapped.clone(),
                        maximum_debit: q("122"),
                        minimum_credit: q("100"),
                    },
                    EffectConstraint::Accrual {
                        resource: ResourceId("lender".into()),
                        position: "supply".into(),
                        asset: wrapped.clone(),
                        position_kind: PositionKind::Supply,
                        maximum_change: q("122"),
                    },
                ],
                ..Default::default()
            },
            vec![
                ProjectedEffect::PositionClose {
                    resource: ResourceId("lender".into()),
                    position: "supply".into(),
                    asset: wrapped.clone(),
                    position_kind: PositionKind::Supply,
                    maximum_amount: q("122"),
                    source_step: 1,
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: wrapped.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("122").unwrap(),
                },
            ],
        );
        let reverse = proof(
            3,
            FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: wrapped.clone(),
                    maximum_input: q("122"),
                    output: native.clone(),
                    minimum_output: q("100"),
                }],
                ..Default::default()
            },
            vec![
                ProjectedEffect::CashSweep {
                    resource: resource.clone(),
                    asset: wrapped.clone(),
                    maximum_amount: q("122"),
                    source_step: 2,
                },
                ProjectedEffect::CashDelta {
                    resource: resource.clone(),
                    asset: native.clone(),
                    minimum_delta: gofin_domain::SignedQuantity::new("100").unwrap(),
                    maximum_delta: gofin_domain::SignedQuantity::new("122").unwrap(),
                },
            ],
        );
        let mut lots = vec![IntervalLot {
            origin: allocation_origin("selected"),
            location: cash_native.location.clone(),
            asset: native.clone(),
            minimum: BigUint::from(100u8),
            maximum: BigUint::from(100u8),
        }];
        let mut state = cash_state("selected", "100");
        for (index, step) in [&forward, &supply, &withdraw, &reverse]
            .into_iter()
            .enumerate()
        {
            replay_step(&mut lots, &mut state, step, &[], index).unwrap();
        }
        let selected = SelectedOrigin {
            origin: allocation_origin("selected"),
            home: cash_native,
            start: BigUint::from(100u8),
        };
        let prices = vec![AssetPrice {
            asset: native,
            usd_micros_per_unit: q("100"),
            unit_atoms: q("100"),
            observed_at: "2026-01-01T00:00:00Z".into(),
            source: "synthetic".into(),
        }];
        assert!(
            check_terminal(
                &lots,
                &state,
                &[selected],
                &[forward, supply, withdraw, reverse],
                &prices,
                &BigUint::zero(),
            )
            .is_ok()
        );
        assert_eq!(
            projected_interval(&state, &cash_wrapped).unwrap(),
            Interval::zero()
        );
    }

    #[test]
    fn accrual_ceiling_without_close_does_not_mint_or_inflate_position() {
        let position = Place {
            location: AccountingLocation::Position {
                resource: ResourceId("lender".into()),
                position: "supply".into(),
                position_kind: PositionKind::Supply,
            },
            asset: asset(),
        };
        let accrual_proof = proof(
            0,
            FinancialIntent {
                expected: vec![EffectConstraint::Accrual {
                    resource: ResourceId("lender".into()),
                    position: "supply".into(),
                    asset: asset(),
                    position_kind: PositionKind::Supply,
                    maximum_change: q("10"),
                }],
                ..Default::default()
            },
            vec![],
        );
        let mut state = ProjectedState::default();
        let mut lots = vec![];
        assert!(add_accruals(&mut lots, &mut state, &accrual_proof).is_ok());
        assert!(lots.is_empty());
        assert!(
            projected_interval(&state, &position)
                .unwrap()
                .maximum
                .is_zero()
        );

        let close_proof = proof(
            1,
            FinancialIntent {
                expected: accrual_proof.intent.expected.clone(),
                ..Default::default()
            },
            vec![ProjectedEffect::PositionClose {
                resource: ResourceId("lender".into()),
                position: "supply".into(),
                asset: asset(),
                position_kind: PositionKind::Supply,
                maximum_amount: q("10"),
                source_step: 0,
            }],
        );
        assert!(add_accruals(&mut lots, &mut state, &close_proof).is_err());
    }
}
