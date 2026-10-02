//! Conservative interval bounds for `project_lots` share allocation.
//!
//! Each input tuple is one FIFO provenance lot, even when labels repeat.
//! Bounds deliberately prefer uncertainty over endpoint sampling: in
//! particular, the last proportional lot can receive a non-monotone
//! remainder as the output changes.

use fin_domain::{FinError, Result};
use num_bigint::BigUint;
use num_traits::Zero;

/// Enclose each input lot's possible output share for every value in the
/// independent input and output intervals.
///
/// Rows retain input order and repeated labels. Rows whose maximum share is
/// zero are omitted, matching `project_lots`, which drops zero-sized lots.
/// This only encloses shares for feasible nonzero-source combinations. The
/// caller must separately prove that the projected source interval is funded;
/// an input interval with a zero lower bound does not itself prove availability.
pub(super) fn enclose_shares(
    inputs: &[(String, BigUint, BigUint)],
    output_min: &BigUint,
    output_max: &BigUint,
    same_asset: bool,
) -> Result<Vec<(String, BigUint, BigUint)>> {
    if output_min > output_max || inputs.iter().any(|(_, min, max)| min > max) {
        return Err(invalid_provenance());
    }

    let total_min = inputs
        .iter()
        .fold(BigUint::zero(), |sum, (_, min, _)| sum + min);
    let total_max = inputs
        .iter()
        .fold(BigUint::zero(), |sum, (_, _, max)| sum + max);
    if output_max.is_zero() {
        return Ok(Vec::new());
    }
    if total_max.is_zero() {
        return Err(invalid_provenance());
    }

    if total_min == total_max && output_min == output_max {
        return exact_shares(inputs, output_min, same_asset);
    }

    let proportional_possible = !same_asset || output_max >= &total_min;
    let haircut_possible = same_asset && output_min < &total_max;
    let mut result = Vec::with_capacity(inputs.len());
    for (index, (label, _, input_max)) in inputs.iter().enumerate() {
        if input_max.is_zero() {
            continue;
        }

        let mut lower: Option<BigUint> = None;
        let mut upper: Option<BigUint> = None;

        if proportional_possible {
            let (proportional_min, proportional_max) =
                proportional_bounds(inputs, index, output_min, output_max);
            lower = Some(proportional_min);
            upper = Some(proportional_max);
        }
        if haircut_possible {
            let (haircut_min, haircut_max) = haircut_bounds(inputs, index, output_min, output_max);
            lower = Some(lower.map_or(haircut_min.clone(), |current| current.min(haircut_min)));
            upper = Some(upper.map_or(haircut_max.clone(), |current| current.max(haircut_max)));
        }

        // Every concrete point lies in one of the possible branches. An input
        // interval with zero lower bound can still have feasible positive
        // source realizations; funding is checked by the caller.
        let minimum = lower.unwrap_or_else(BigUint::zero);
        let maximum = upper
            .unwrap_or_else(|| output_max.clone())
            .min(output_max.clone());
        if minimum > maximum {
            return Err(invalid_provenance());
        }
        if !maximum.is_zero() {
            result.push((label.clone(), minimum, maximum));
        }
    }
    Ok(result)
}

fn proportional_bounds(
    inputs: &[(String, BigUint, BigUint)],
    index: usize,
    output_min: &BigUint,
    output_max: &BigUint,
) -> (BigUint, BigUint) {
    let (_, input_min, input_max) = &inputs[index];
    if input_max.is_zero() {
        return (BigUint::zero(), BigUint::zero());
    }

    let other_max = inputs
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .fold(BigUint::zero(), |sum, (_, (_, _, max))| sum + max);
    let other_min = inputs
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .fold(BigUint::zero(), |sum, (_, (_, min, _))| sum + min);

    // Every non-final share is floor(x*y/total); the final positive share is
    // the remainder and is at least that floor. The smallest possible ratio
    // uses this lot's lower endpoint and every other lot's upper endpoint.
    let minimum = if input_min.is_zero() {
        BigUint::zero()
    } else {
        (input_min * output_min) / (input_min + other_max)
    };

    // A non-final share cannot exceed this ratio bound. If this lot can become
    // the final positive input (all later lots may be zero), it can receive
    // the entire remainder, so use the safe output ceiling.
    let can_be_final = inputs[index + 1..].iter().all(|(_, min, _)| min.is_zero());
    let maximum = if can_be_final {
        let earlier_positive = inputs[..index]
            .iter()
            .filter(|(_, _, max)| !max.is_zero())
            .count();
        if earlier_positive == 0 {
            output_max.clone()
        } else {
            let denominator = input_max + other_min;
            let proportional_ceiling = ceil_div(&(input_max * output_max), &denominator);
            (proportional_ceiling + BigUint::from(earlier_positive - 1)).min(output_max.clone())
        }
    } else {
        let denominator = input_max + other_min;
        if denominator.is_zero() {
            BigUint::zero()
        } else {
            (input_max * output_max) / denominator
        }
    }
    .min(output_max.clone());
    (minimum, maximum)
}

fn haircut_bounds(
    inputs: &[(String, BigUint, BigUint)],
    index: usize,
    output_min: &BigUint,
    output_max: &BigUint,
) -> (BigUint, BigUint) {
    // project_lots removes losses from principal lots first, then accrual lots;
    // within each class it preserves the input order. Therefore the retained
    // share is the input lot's size capped by output remaining after every
    // later loss-priority lot.
    let accrual = inputs[index].0.starts_with("accrual:");
    let later = inputs.iter().enumerate().filter(|(other, (label, _, _))| {
        if *other == index {
            return false;
        }
        let other_accrual = label.starts_with("accrual:");
        if accrual {
            other_accrual && *other > index
        } else {
            (!other_accrual && *other > index) || other_accrual
        }
    });
    let later_min = later
        .clone()
        .fold(BigUint::zero(), |sum, (_, (_, min, _))| sum + min);
    let later_max = later.fold(BigUint::zero(), |sum, (_, (_, _, max))| sum + max);
    let (_, input_min, input_max) = &inputs[index];
    let retained_minimum = if output_min > &later_max {
        output_min - &later_max
    } else {
        BigUint::zero()
    };
    let retained_maximum = if output_max > &later_min {
        output_max - &later_min
    } else {
        BigUint::zero()
    };
    let minimum = retained_minimum.min(input_min.clone());
    let maximum = retained_maximum.min(input_max.clone());
    (minimum, maximum)
}

fn exact_shares(
    inputs: &[(String, BigUint, BigUint)],
    output: &BigUint,
    same_asset: bool,
) -> Result<Vec<(String, BigUint, BigUint)>> {
    let values = inputs
        .iter()
        .map(|(_, min, _)| min.clone())
        .collect::<Vec<_>>();
    let total = values
        .iter()
        .fold(BigUint::zero(), |sum, value| sum + value);
    if total.is_zero() {
        return if output.is_zero() {
            Ok(Vec::new())
        } else {
            Err(invalid_provenance())
        };
    }
    if output.is_zero() {
        return Ok(Vec::new());
    }

    let mut shares = values.clone();
    if same_asset && output < &total {
        let mut loss = &total - output;
        for accrued in [false, true] {
            for (index, (label, _, _)) in inputs.iter().enumerate() {
                if label.starts_with("accrual:") != accrued || loss.is_zero() {
                    continue;
                }
                let removed = values[index].clone().min(loss.clone());
                shares[index] = &values[index] - &removed;
                loss -= removed;
            }
        }
        if !loss.is_zero() {
            return Err(invalid_provenance());
        }
    } else {
        let final_positive = values.iter().rposition(|value| !value.is_zero());
        let Some(final_positive) = final_positive else {
            return Err(invalid_provenance());
        };
        let mut output_left = output.clone();
        for (index, input) in values.iter().enumerate() {
            if input.is_zero() {
                continue;
            }
            let share = if index == final_positive {
                output_left.clone()
            } else {
                (input * output) / &total
            };
            output_left -= &share;
            shares[index] = share;
        }
    }

    Ok(inputs
        .iter()
        .zip(shares)
        .filter(|(_, share)| !share.is_zero())
        .map(|((label, _, _), share)| (label.clone(), share.clone(), share))
        .collect())
}

fn ceil_div(numerator: &BigUint, denominator: &BigUint) -> BigUint {
    if numerator.is_zero() {
        return BigUint::zero();
    }
    (numerator + denominator - BigUint::from(1u8)) / denominator
}

fn invalid_provenance() -> FinError {
    FinError::new(
        "RETURN_ROUTE_PROVENANCE_INVALID",
        "budget.restoration.provenance",
        "Projected source shares cannot be proven from the selected origins.",
        "Keep the route unresolved and revise it to preserve bounded source provenance.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProvenanceLot, project_lots};
    use fin_domain::{AccountingLocation, AssetId, ResourceId};

    fn n(value: u32) -> BigUint {
        BigUint::from(value)
    }

    fn selected_lots(
        inputs: &[(String, BigUint, BigUint)],
        same_asset: bool,
    ) -> Vec<ProvenanceLot> {
        inputs
            .iter()
            .filter(|(_, minimum, _)| !minimum.is_zero())
            .map(|(origin, minimum, _)| ProvenanceLot {
                origin: origin.clone(),
                location: AccountingLocation::Cash {
                    resource: ResourceId("shares-test".into()),
                },
                asset: AssetId {
                    network: "test".into(),
                    reference: if same_asset { "coin" } else { "input" }.into(),
                    decimals: 0,
                },
                atoms: minimum.to_string(),
            })
            .collect()
    }

    fn exhaustive_enclosure_check(
        inputs: &[(String, BigUint, BigUint)],
        output_min: u32,
        output_max: u32,
        same_asset: bool,
    ) {
        let output_min_big = n(output_min);
        let output_max_big = n(output_max);
        let possible_source = inputs
            .iter()
            .fold(BigUint::zero(), |sum, (_, _, maximum)| sum + maximum);
        let enclosure = match enclose_shares(inputs, &output_min_big, &output_max_big, same_asset) {
            Ok(enclosure) => enclosure,
            Err(_) if possible_source.is_zero() && output_max > 0 => return,
            Err(error) => panic!("valid interval should be enclosed: {error:?}"),
        };

        fn walk(
            inputs: &[(String, BigUint, BigUint)],
            index: usize,
            concrete: &mut Vec<(String, BigUint, BigUint)>,
            output_min: u32,
            output_max: u32,
            same_asset: bool,
            enclosure: &[(String, BigUint, BigUint)],
        ) {
            if index < inputs.len() {
                let (label, min, max) = &inputs[index];
                let min = min.to_u32_digits().first().copied().unwrap_or(0);
                let max = max.to_u32_digits().first().copied().unwrap_or(0);
                for amount in min..=max {
                    concrete.push((label.clone(), n(amount), n(amount)));
                    walk(
                        inputs,
                        index + 1,
                        concrete,
                        output_min,
                        output_max,
                        same_asset,
                        enclosure,
                    );
                    concrete.pop();
                }
                return;
            }
            let total = concrete
                .iter()
                .fold(BigUint::zero(), |sum, (_, amount, _)| sum + amount);
            if total.is_zero() && output_max > 0 {
                // The route source validator rejects minting from an empty
                // source; this concrete input is outside the route domain.
                return;
            }
            for output in output_min..=output_max {
                let actual = project_lots(
                    &selected_lots(concrete, same_asset),
                    &AccountingLocation::Cash {
                        resource: ResourceId("target".into()),
                    },
                    &AssetId {
                        network: "test".into(),
                        reference: "coin".into(),
                        decimals: 0,
                    },
                    &n(output),
                )
                .expect("valid concrete projection");
                let mut previous = 0usize;
                for actual_lot in actual {
                    let actual_amount = actual_lot.atoms.parse::<BigUint>().unwrap();
                    let Some(relative) =
                        enclosure[previous..]
                            .iter()
                            .position(|(label, minimum, maximum)| {
                                label == &actual_lot.origin
                                    && minimum <= &actual_amount
                                    && &actual_amount <= maximum
                            })
                    else {
                        panic!(
                            "actual share {} for {:?} not enclosed by {:?}, inputs={:?}, output={output}, same_asset={same_asset}",
                            actual_amount, actual_lot.origin, enclosure, concrete
                        );
                    };
                    previous += relative + 1;
                }
            }
        }

        walk(
            inputs,
            0,
            &mut Vec::new(),
            output_min,
            output_max,
            same_asset,
            &enclosure,
        );
    }

    #[test]
    fn exhaustively_encloses_small_uncertain_proportional_and_haircut_routes() {
        let intervals = (0..=2)
            .flat_map(|minimum| (minimum..=2).map(move |maximum| (n(minimum), n(maximum))))
            .collect::<Vec<_>>();
        for same_asset in [false, true] {
            for (first_min, first_max) in &intervals {
                for (yield_min, yield_max) in &intervals {
                    for (last_min, last_max) in &intervals {
                        let inputs = vec![
                            ("principal:0".into(), first_min.clone(), first_max.clone()),
                            ("accrual:yield".into(), yield_min.clone(), yield_max.clone()),
                            ("principal:2".into(), last_min.clone(), last_max.clone()),
                        ];
                        for output_min in 0..=4 {
                            for output_max in output_min..=4 {
                                exhaustive_enclosure_check(
                                    &inputs, output_min, output_max, same_asset,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn same_asset_loss_first_handles_uncertain_yield_and_repeated_labels() {
        let inputs = vec![
            ("origin:principal".into(), n(100), n(100)),
            ("accrual:yield".into(), n(0), n(10)),
            ("origin:principal".into(), n(0), n(0)),
        ];
        let shares = enclose_shares(&inputs, &n(100), &n(110), true).unwrap();
        assert_eq!(shares[0], ("origin:principal".into(), n(90), n(110)));
        assert_eq!(shares[1], ("accrual:yield".into(), n(0), n(10)));
        exhaustive_enclosure_check(&inputs, 100, 110, true);

        let exact = vec![
            ("principal".into(), n(100), n(100)),
            ("accrual:yield".into(), n(10), n(10)),
        ];
        let after_loss = enclose_shares(&exact, &n(108), &n(108), true).unwrap();
        assert_eq!(after_loss[0], ("principal".into(), n(98), n(98)));
        assert_eq!(after_loss[1], ("accrual:yield".into(), n(10), n(10)));
    }

    #[test]
    fn proportional_remainder_keeps_order_and_bounds_non_monotone_final_share() {
        let inputs = vec![
            ("same".into(), n(1), n(1)),
            ("same".into(), n(1), n(1)),
            ("same".into(), n(1), n(1)),
        ];
        let shares = enclose_shares(&inputs, &n(2), &n(3), false).unwrap();
        assert_eq!(shares[0].0, "same");
        assert_eq!(shares[1].0, "same");
        assert_eq!(shares[2], ("same".into(), n(0), n(2)));
        exhaustive_enclosure_check(&inputs, 2, 3, false);
    }

    #[test]
    fn cross_asset_projection_supports_distinct_units_and_zero_lower_source() {
        let exact = vec![("origin".into(), n(1), n(1))];
        assert_eq!(
            enclose_shares(&exact, &n(100), &n(100), false).unwrap(),
            vec![("origin".into(), n(100), n(100))]
        );
        let uncertain = vec![("origin".into(), n(0), n(1))];
        assert_eq!(
            enclose_shares(&uncertain, &n(1), &n(100), false).unwrap(),
            vec![("origin".into(), n(0), n(100))]
        );
        exhaustive_enclosure_check(&uncertain, 1, 4, false);
    }

    #[test]
    fn rejects_invalid_intervals_and_positive_outputs_without_sources() {
        assert!(enclose_shares(&[], &n(0), &n(1), false).is_err());
        assert!(enclose_shares(&[("x".into(), n(2), n(1))], &n(0), &n(1), false).is_err());
        assert!(enclose_shares(&[("x".into(), n(0), n(1))], &n(2), &n(1), false).is_err());
        assert!(enclose_shares(&[("x".into(), n(0), n(0))], &n(0), &n(1), true).is_err());
        assert!(
            enclose_shares(&[("x".into(), n(0), n(0))], &n(0), &n(0), true)
                .unwrap()
                .is_empty()
        );
    }
}
