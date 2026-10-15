//! Proved in-operation credits fund later conversions; initial holdings fund only the root.
use super::*;
use gofin_domain::conversion_sequences::{
    ConversionCost, ConversionCostBound, ConversionCostFunding, ConversionLegStatus,
    ConversionSequence, ConversionSequenceConstraint,
};

fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "INVALID_CONVERSION_SEQUENCE",
        "reconciliation",
        message,
        "Keep the original submission and its reservations; recover the ordered native execution and fee evidence.",
    )
}
fn id_valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| !c.is_control())
}
fn fee_treatment(scope: &ConversionSequenceConstraint, cost: &ConversionCost) -> CostTreatment {
    if cost.amount.asset == scope.external_input || cost.funding == ConversionCostFunding::External
    {
        CostTreatment::AdditionalDebit
    } else {
        CostTreatment::DeductedFromCredit
    }
}
fn fee_bound_treatment(
    scope: &ConversionSequenceConstraint,
    cap: &ConversionCostBound,
) -> CostTreatment {
    if cap.amount.asset == scope.external_input || cap.funding == ConversionCostFunding::External {
        CostTreatment::AdditionalDebit
    } else {
        CostTreatment::DeductedFromCredit
    }
}
fn add(
    pool: &mut BTreeMap<gofin_domain::AssetId, BigUint>,
    asset: &gofin_domain::AssetId,
    atoms: BigUint,
) {
    *pool.entry(asset.clone()).or_default() += atoms;
}
fn subtract(
    pool: &mut BTreeMap<gofin_domain::AssetId, BigUint>,
    asset: &gofin_domain::AssetId,
    atoms: &BigUint,
) -> Result<()> {
    let available = pool.entry(asset.clone()).or_default();
    if &*available < atoms {
        return Err(invalid(
            "A later leg or its fee exceeds previously proved usable generated credit.",
        ));
    }
    *available -= atoms;
    Ok(())
}
pub(super) fn validate_prepared(prepared: &PreparedOperation) -> Result<()> {
    let intent = &prepared.intent;
    let mut ids = BTreeSet::new();
    let mut external = BTreeMap::new();
    let mut costs = BTreeMap::new();
    let mut minimum_usd = BigUint::zero();
    let mut initial_usd = BigUint::zero();
    for constraint in &intent.expected {
        let EffectConstraint::ConversionSequence { scope } = constraint else {
            continue;
        };
        if scope.accounting_version != 1
            || !id_valid(&scope.sequence_id)
            || !ids.insert((scope.resource.clone(), scope.sequence_id.clone()))
            || scope.legs.len() < 2
            || scope.legs.len() > 8
            || scope.maximum_external_input.is_zero()
            || !intent.locks.contains(&scope.resource)
        {
            return Err(invalid(
                "An ordered conversion scope needs one locked resource, unique identity and two to eight finite legs.",
            ));
        }
        let mut leg_ids = BTreeSet::new();
        let mut produced = BTreeSet::new();
        for (index, leg) in scope.legs.iter().enumerate() {
            if !id_valid(&leg.id)
                || !leg_ids.insert(&leg.id)
                || leg.maximum_input.is_zero()
                || leg.input == leg.output
                || (index == 0
                    && (leg.input != scope.external_input
                        || leg.maximum_input > scope.maximum_external_input))
                || (index > 0 && !produced.contains(&leg.input))
            {
                return Err(invalid(
                    "Conversion legs must retain unique identities, distinct exact assets and a prior generated input.",
                ));
            }
            produced.insert(leg.output.clone());
            let valuation = prepared.valuation.as_ref().ok_or_else(|| {
                invalid("Each conversion stage requires attributable gross USD valuation.")
            })?;
            minimum_usd += ordinary_policy::amount_usd(
                &Amount {
                    asset: leg.input.clone(),
                    atoms: leg.maximum_input.clone(),
                },
                &valuation.prices,
            )?;
        }
        initial_usd += ordinary_policy::amount_usd(
            &Amount {
                asset: scope.external_input.clone(),
                atoms: scope.maximum_external_input.clone(),
            },
            &prepared
                .valuation
                .as_ref()
                .ok_or_else(|| invalid("A sequence requires valuation."))?
                .prices,
        )?;
        add_big_amount(
            &mut external,
            &scope.resource,
            &Amount {
                asset: scope.external_input.clone(),
                atoms: scope.maximum_external_input.clone(),
            },
            scope.maximum_external_input.integer(),
        )?;
        let mut assets = BTreeSet::new();
        for cap in &scope.maximum_costs {
            if !assets.insert((&cap.amount.asset, format!("{:?}", cap.funding))) {
                return Err(invalid(
                    "Whole-sequence fee ceilings must identify each exact asset once.",
                ));
            }
            let location = AccountingLocation::Cash {
                resource: scope.resource.clone(),
            };
            let treatment = fee_bound_treatment(scope, cap);
            *costs
                .entry(cost_key(&location, &cap.amount.asset, &treatment)?)
                .or_insert_with(BigUint::zero) += cap.amount.atoms.integer();
        }
    }
    let debits = aggregate_resource_amounts(&intent.debits)?;
    let fee_caps = aggregate_cost_bounds(&intent.maximum_costs)?;
    if external
        .iter()
        .any(|(key, value)| debits.get(key).is_none_or(|maximum| value > maximum))
        || costs
            .iter()
            .any(|(key, value)| fee_caps.get(key).is_none_or(|maximum| value > maximum))
    {
        return Err(invalid(
            "The sequence's external principal and fee sources must be backed by the exact approved reservations.",
        ));
    }
    if !ids.is_empty() {
        let prices = &prepared
            .valuation
            .as_ref()
            .ok_or_else(|| invalid("A sequence requires valuation."))?
            .prices;
        let mut all_debits = BigUint::zero();
        for debit in &intent.debits {
            all_debits += ordinary_policy::amount_usd(&debit.amount, prices)?;
        }
        minimum_usd += if all_debits > initial_usd {
            all_debits - initial_usd
        } else {
            BigUint::zero()
        };
    }
    if !ids.is_empty()
        && prepared
            .valuation
            .as_ref()
            .is_none_or(|v| v.movement_usd_micros.integer() < minimum_usd)
    {
        return Err(invalid(
            "Gross movement valuation must include the approved input bound of every stage, including internally funded legs.",
        ));
    }
    Ok(())
}
fn validate_execution(
    scope: &ConversionSequenceConstraint,
    execution: &ConversionSequence,
    relaxed: bool,
    terminal: bool,
) -> Result<Vec<FinancialEffect>> {
    if scope.accounting_version != 1
        || execution.accounting_version != scope.accounting_version
        || scope.sequence_id != execution.sequence_id
        || scope.resource != execution.resource
        || scope.legs.len() != execution.legs.len()
    {
        return Err(invalid(
            "Sequence receipt does not identify the exact approved resource and ordered stages.",
        ));
    }
    let mut pool = BTreeMap::new();
    let mut fee_totals = BTreeMap::new();
    let mut add_back = BTreeMap::new();
    let mut effects = vec![];
    let mut cost_ids = BTreeSet::new();
    for cost in &execution.costs {
        if !id_valid(&cost.category)
            || !scope.legs.iter().any(|leg| leg.id == cost.leg_id)
            || !cost_ids.insert((
                &cost.leg_id,
                &cost.amount.asset,
                &cost.category,
                format!("{:?}{:?}", cost.treatment, cost.funding),
            ))
        {
            return Err(invalid(
                "A sequence cost is unscoped, duplicated or lacks its ordered stage identity.",
            ));
        }
        add(
            fee_totals
                .entry(format!("{:?}", cost.funding))
                .or_insert_with(BTreeMap::new),
            &cost.amount.asset,
            cost.amount.atoms.integer(),
        );
    }
    for (index, (allowed, leg)) in scope.legs.iter().zip(&execution.legs).enumerate() {
        if allowed.id != leg.id
            || allowed.input != leg.spent.asset
            || allowed.output != leg.received.asset
            || leg.spent.atoms > allowed.maximum_input
            || (!relaxed && leg.received.atoms < allowed.minimum_output)
            || (terminal && leg.status != ConversionLegStatus::Terminal)
            || leg.spent.atoms.is_zero() != leg.received.atoms.is_zero()
        {
            return Err(invalid(
                "An execution stage changed identity, exceeded its quantity bound, omitted completion or lacks correlated gross credit.",
            ));
        }
        let costs = execution
            .costs
            .iter()
            .filter(|c| c.leg_id == leg.id)
            .collect::<Vec<_>>();
        for cost in &costs {
            if cost.funding == ConversionCostFunding::External {
                if (cost.treatment == CostTreatment::DeductedFromCredit
                    && cost.amount.asset != leg.received.asset)
                    || (cost.treatment == CostTreatment::IncludedInDebit
                        && (cost.amount.asset != leg.spent.asset
                            || cost.amount.atoms > leg.spent.atoms))
                {
                    return Err(invalid(
                        "An externally funded fee cannot claim generated output or an unrelated included principal.",
                    ));
                }
            } else {
                match cost.treatment {
                    CostTreatment::AdditionalDebit => {
                        subtract(&mut pool, &cost.amount.asset, &cost.amount.atoms.integer())?
                    }
                    CostTreatment::IncludedInDebit => {
                        if index == 0
                            || cost.amount.asset != leg.spent.asset
                            || cost.amount.atoms > leg.spent.atoms
                        {
                            return Err(invalid(
                                "An internally included fee must already belong to this generated gross spend.",
                            ));
                        }
                    }
                    CostTreatment::DeductedFromCredit => {
                        if cost.amount.asset != leg.received.asset {
                            return Err(invalid(
                                "A withheld sequence fee must identify this stage's actual gross output asset.",
                            ));
                        }
                    }
                }
                add(
                    &mut add_back,
                    &cost.amount.asset,
                    cost.amount.atoms.integer(),
                );
            }
        }
        if index == 0 {
            if leg.spent.atoms > scope.maximum_external_input {
                return Err(invalid(
                    "Initial external sequence principal exceeds its reservation.",
                ));
            }
            let included = costs
                .iter()
                .filter(|cost| {
                    cost.funding == ConversionCostFunding::External
                        && cost.treatment == CostTreatment::IncludedInDebit
                })
                .fold(BigUint::zero(), |total, cost| {
                    total + cost.amount.atoms.integer()
                });
            if included > leg.spent.atoms.integer() {
                return Err(invalid(
                    "Combined included source fees exceed the initial gross debit.",
                ));
            }
            effects.push(FinancialEffect::CashDebit {
                resource: scope.resource.clone(),
                amount: Amount {
                    asset: leg.spent.asset.clone(),
                    atoms: gofin_domain::Quantity::new(
                        (leg.spent.atoms.integer() - included).to_string(),
                    )?,
                },
            });
        } else {
            let included = costs
                .iter()
                .filter(|cost| {
                    cost.funding == ConversionCostFunding::External
                        && cost.treatment == CostTreatment::IncludedInDebit
                })
                .fold(BigUint::zero(), |total, cost| {
                    total + cost.amount.atoms.integer()
                });
            if included > leg.spent.atoms.integer() {
                return Err(invalid(
                    "Combined included fees exceed the native gross spend.",
                ));
            }
            subtract(
                &mut pool,
                &leg.spent.asset,
                &(leg.spent.atoms.integer() - included),
            )?;
        }
        add(&mut pool, &leg.received.asset, leg.received.atoms.integer());
        for cost in costs {
            if cost.funding == ConversionCostFunding::GeneratedCredit
                && cost.treatment == CostTreatment::DeductedFromCredit
            {
                subtract(&mut pool, &cost.amount.asset, &cost.amount.atoms.integer())?;
            }
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: scope.resource.clone(),
                },
                amount: cost.amount.clone(),
                category: cost.category.clone(),
                treatment: fee_treatment(scope, cost),
            });
        }
    }
    let maximums = scope
        .maximum_costs
        .iter()
        .map(|cap| {
            (
                (cap.amount.asset.clone(), format!("{:?}", cap.funding)),
                cap.amount.atoms.integer(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if fee_totals.iter().any(|(funding, rows)| {
        rows.iter().any(|(asset, amount)| {
            maximums
                .get(&(asset.clone(), funding.clone()))
                .is_none_or(|cap| amount > cap)
        })
    }) {
        return Err(invalid(
            "Cumulative native fees across all sequence stages exceed the whole-command asset ceiling.",
        ));
    }
    for leg in &scope.legs {
        pool.entry(leg.output.clone()).or_default();
    }
    for (asset, fees) in add_back {
        add(&mut pool, &asset, fees);
    }
    for (asset, atoms) in pool {
        effects.push(FinancialEffect::UsableCredit {
            to: scope.resource.clone(),
            amount: Amount {
                asset,
                atoms: gofin_domain::Quantity::new(atoms.to_string())?,
            },
        });
    }
    Ok(effects)
}
pub(super) fn project_effects(effects: &[FinancialEffect]) -> Result<Vec<FinancialEffect>> {
    let mut projected = vec![];
    for effect in effects {
        if let FinancialEffect::ConversionSequence { execution } = effect {
            let first = execution
                .legs
                .first()
                .ok_or_else(|| invalid("Sequence has no initial stage."))?;
            // Projection has no authority: bounds and exact identities are checked
            // separately against the immutable approved scope before journal application.
            let scope = ConversionSequenceConstraint {
                accounting_version: execution.accounting_version,
                sequence_id: execution.sequence_id.clone(),
                resource: execution.resource.clone(),
                external_input: first.spent.asset.clone(),
                maximum_external_input: first.spent.atoms.clone(),
                legs: execution
                    .legs
                    .iter()
                    .map(
                        |leg| gofin_domain::conversion_sequences::ConversionLegConstraint {
                            id: leg.id.clone(),
                            input: leg.spent.asset.clone(),
                            maximum_input: leg.spent.atoms.clone(),
                            output: leg.received.asset.clone(),
                            minimum_output: gofin_domain::Quantity::zero(),
                        },
                    )
                    .collect(),
                maximum_costs: {
                    let mut caps = BTreeMap::<(gofin_domain::AssetId, String), BigUint>::new();
                    for cost in &execution.costs {
                        *caps
                            .entry((cost.amount.asset.clone(), format!("{:?}", cost.funding)))
                            .or_default() += cost.amount.atoms.integer();
                    }
                    caps.into_iter()
                        .map(|((asset, funding), atoms)| {
                            Ok(ConversionCostBound {
                                amount: Amount {
                                    asset,
                                    atoms: gofin_domain::Quantity::new(atoms.to_string())?,
                                },
                                funding: if funding == "External" {
                                    ConversionCostFunding::External
                                } else {
                                    ConversionCostFunding::GeneratedCredit
                                },
                            })
                        })
                        .collect::<Result<Vec<_>>>()?
                },
            };
            projected.extend(validate_execution(&scope, execution, true, false)?);
        } else {
            projected.push(effect.clone());
        }
    }
    Ok(projected)
}
pub(super) fn validated_projection(
    intent: &FinancialIntent,
    effects: &[FinancialEffect],
    relaxed: bool,
    terminal: bool,
) -> Result<(FinancialIntent, Vec<FinancialEffect>)> {
    let mut projected = intent.clone();
    projected
        .expected
        .retain(|c| !matches!(c, EffectConstraint::ConversionSequence { .. }));
    let mut matched = BTreeSet::new();
    let mut outputs = vec![];
    for constraint in &intent.expected {
        if let EffectConstraint::ConversionSequence { scope } = constraint {
            projected.expected.push(EffectConstraint::Debit {
                resource: scope.resource.clone(),
                asset: scope.external_input.clone(),
                maximum_debit: scope.maximum_external_input.clone(),
            });
            let assets = scope
                .legs
                .iter()
                .map(|leg| leg.output.clone())
                .collect::<BTreeSet<_>>();
            for asset in assets {
                projected.expected.push(EffectConstraint::Credit {
                    to: scope.resource.clone(),
                    asset,
                    minimum_credit: gofin_domain::Quantity::zero(),
                });
            }
        }
    }
    for effect in effects {
        if let FinancialEffect::ConversionSequence { execution } = effect {
            let scope = intent
                .expected
                .iter()
                .find_map(|c| match c {
                    EffectConstraint::ConversionSequence { scope }
                        if scope.sequence_id == execution.sequence_id
                            && scope.resource == execution.resource =>
                    {
                        Some(scope)
                    }
                    _ => None,
                })
                .ok_or_else(|| invalid("An unapproved sequence cannot spend generated credits."))?;
            if !matched.insert((&execution.resource, &execution.sequence_id)) {
                return Err(invalid("A sequence effect is duplicated."));
            }
            outputs.extend(validate_execution(scope, execution, relaxed, terminal)?);
        } else {
            outputs.push(effect.clone());
        }
    }
    // Unfilled failed actions may legitimately contain no effects. All other
    // completion matching still requires every projected exact source/output.
    Ok((projected, outputs))
}
pub(super) fn constraints_within(
    candidate: &ConversionSequenceConstraint,
    approved: &ConversionSequenceConstraint,
) -> bool {
    candidate.accounting_version == approved.accounting_version
        && candidate.sequence_id == approved.sequence_id
        && candidate.resource == approved.resource
        && candidate.external_input == approved.external_input
        && candidate.maximum_external_input <= approved.maximum_external_input
        && candidate.legs.len() == approved.legs.len()
        && candidate.legs.iter().zip(&approved.legs).all(|(a, b)| {
            a.id == b.id
                && a.input == b.input
                && a.output == b.output
                && a.maximum_input <= b.maximum_input
                && a.minimum_output >= b.minimum_output
        })
        && candidate.maximum_costs.iter().all(|a| {
            approved.maximum_costs.iter().any(|b| {
                a.amount.asset == b.amount.asset
                    && a.funding == b.funding
                    && a.amount.atoms <= b.amount.atoms
            })
        })
}

/// Stable stage identities make shrinking residual credit safe: gross fills and
/// fee bookings grow, while a terminal stage can never acquire another fill.
pub(super) fn validate_growth(old: &[FinancialEffect], new: &[FinancialEffect]) -> Result<()> {
    for effect in old {
        let FinancialEffect::ConversionSequence { execution: prior } = effect else {
            continue;
        };
        let next = new
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::ConversionSequence { execution }
                    if execution.sequence_id == prior.sequence_id
                        && execution.resource == prior.resource =>
                {
                    Some(execution)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("A cumulative proof omitted its original sequence."))?;
        if prior.accounting_version != next.accounting_version
            || prior.legs.len() != next.legs.len()
        {
            return Err(invalid("A cumulative sequence changed its ordered stages."));
        }
        for (before, after) in prior.legs.iter().zip(&next.legs) {
            if before.id != after.id
                || before.spent.asset != after.spent.asset
                || before.received.asset != after.received.asset
                || after.spent.atoms < before.spent.atoms
                || after.received.atoms < before.received.atoms
                || (before.status == ConversionLegStatus::Terminal && before != after)
            {
                return Err(invalid(
                    "A cumulative sequence retracted a fill, changed identity or altered a terminal stage.",
                ));
            }
        }
        for cost in &prior.costs {
            let after = next
                .costs
                .iter()
                .find(|c| {
                    c.leg_id == cost.leg_id
                        && c.amount.asset == cost.amount.asset
                        && c.category == cost.category
                        && c.treatment == cost.treatment
                        && c.funding == cost.funding
                })
                .ok_or_else(|| invalid("A cumulative sequence omitted a proved fee."))?;
            let terminal = prior
                .legs
                .iter()
                .any(|leg| leg.id == cost.leg_id && leg.status == ConversionLegStatus::Terminal);
            if after.amount.atoms < cost.amount.atoms || (terminal && after.amount != cost.amount) {
                return Err(invalid(
                    "A cumulative sequence retracted a fee or altered a terminal booking.",
                ));
            }
        }
        for cost in &next.costs {
            if prior
                .legs
                .iter()
                .any(|leg| leg.id == cost.leg_id && leg.status == ConversionLegStatus::Terminal)
                && !prior.costs.iter().any(|c| c == cost)
            {
                return Err(invalid("A terminal stage acquired an unproved later fee."));
            }
        }
    }
    Ok(())
}

pub(super) fn failed_projection_matches(
    intent: &FinancialIntent,
    effect: &FinancialEffect,
) -> Result<bool> {
    for expected in &intent.expected {
        let EffectConstraint::ConversionSequence { scope } = expected else {
            continue;
        };
        let candidate = match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                resource == &scope.resource
                    && amount.asset == scope.external_input
                    && amount.atoms <= scope.maximum_external_input
            }
            FinancialEffect::UsableCredit { to, amount } => {
                to == &scope.resource && scope.legs.iter().any(|leg| leg.output == amount.asset)
            }
            _ => false,
        };
        if candidate {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::conversion_sequences::{ConversionLeg, ConversionLegConstraint};
    fn asset(id: &str) -> gofin_domain::AssetId {
        gofin_domain::AssetId {
            network: "synthetic".into(),
            reference: id.into(),
            decimals: 0,
        }
    }
    fn amount(id: &str, n: u64) -> Amount {
        Amount {
            asset: asset(id),
            atoms: gofin_domain::Quantity::from_u64(n),
        }
    }
    fn scope() -> ConversionSequenceConstraint {
        ConversionSequenceConstraint {
            accounting_version: 1,
            sequence_id: "root-and-child".into(),
            resource: ResourceId("synthetic:account".into()),
            external_input: asset("A"),
            maximum_external_input: gofin_domain::Quantity::from_u64(100),
            legs: vec![
                ConversionLegConstraint {
                    id: "parent".into(),
                    input: asset("A"),
                    maximum_input: gofin_domain::Quantity::from_u64(100),
                    output: asset("B"),
                    minimum_output: gofin_domain::Quantity::zero(),
                },
                ConversionLegConstraint {
                    id: "child".into(),
                    input: asset("B"),
                    maximum_input: gofin_domain::Quantity::from_u64(100),
                    output: asset("A"),
                    minimum_output: gofin_domain::Quantity::zero(),
                },
            ],
            maximum_costs: vec![
                ConversionCostBound {
                    amount: amount("A", 5),
                    funding: ConversionCostFunding::External,
                },
                ConversionCostBound {
                    amount: amount("B", 5),
                    funding: ConversionCostFunding::GeneratedCredit,
                },
            ],
        }
    }
    fn execution(child: u64, terminal: bool) -> ConversionSequence {
        ConversionSequence {
            accounting_version: 1,
            sequence_id: "root-and-child".into(),
            resource: scope().resource,
            legs: vec![
                ConversionLeg {
                    id: "parent".into(),
                    status: ConversionLegStatus::Terminal,
                    spent: amount("A", 100),
                    received: amount("B", 100),
                },
                ConversionLeg {
                    id: "child".into(),
                    status: if terminal {
                        ConversionLegStatus::Terminal
                    } else {
                        ConversionLegStatus::Pending
                    },
                    spent: amount("B", child),
                    received: amount("A", child),
                },
            ],
            costs: vec![
                ConversionCost {
                    leg_id: "parent".into(),
                    amount: amount("B", 2),
                    category: "synthetic_output_fee".into(),
                    treatment: CostTreatment::DeductedFromCredit,
                    funding: ConversionCostFunding::GeneratedCredit,
                },
                ConversionCost {
                    leg_id: "child".into(),
                    amount: amount("B", 1),
                    category: "synthetic_input_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                    funding: ConversionCostFunding::GeneratedCredit,
                },
            ],
        }
    }
    fn wrap(execution: ConversionSequence) -> Vec<FinancialEffect> {
        vec![FinancialEffect::ConversionSequence { execution }]
    }
    fn prepared() -> PreparedOperation {
        let scope = scope();
        let resource = scope.resource.clone();
        let timestamp = now();
        PreparedOperation {
            operation: "synthetic.sequence".into(),
            input: json!({}),
            binding: json!({"sequence":"exact-original"}),
            summary: "synthetic generated-credit sequence".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: timestamp.clone(),
            expires_at: timestamp.clone(),
            action_deadline: None,
            operation_version: 1,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: amount("A", 100),
                }],
                maximum_costs: vec![
                    CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount("A", 5),
                        treatment: CostTreatment::AdditionalDebit,
                    },
                    CostBound {
                        location: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        amount: amount("B", 5),
                        treatment: CostTreatment::DeductedFromCredit,
                    },
                ],
                expected: vec![EffectConstraint::ConversionSequence { scope }],
                locks: vec![resource],
            },
            valuation: Some(gofin_domain::Valuation {
                movement_usd_micros: gofin_domain::Quantity::from_u64(200_000_000),
                maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(10_000_000),
                additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
                observed_at: timestamp.clone(),
                source: "synthetic-exact".into(),
                prices: ["A", "B"]
                    .into_iter()
                    .map(|id| gofin_domain::AssetPrice {
                        asset: asset(id),
                        usd_micros_per_unit: gofin_domain::Quantity::from_u64(1_000_000),
                        unit_atoms: gofin_domain::Quantity::from_u64(1),
                        observed_at: timestamp.clone(),
                        source: "synthetic-exact".into(),
                    })
                    .collect(),
            }),
        }
    }
    fn record() -> OperationRecord {
        let preview = prepared();
        let timestamp = now();
        OperationRecord {
            id: "synthetic-operation".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: preview.operation.clone(),
            input: preview.input.clone(),
            status: Status::Pending,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
            approval: Some(gofin_domain::ApprovalAudit {
                mechanism: "synthetic_exact".into(),
                binding_hash: binding_hash(&authorization_binding(&preview)).unwrap(),
                challenge_nonce: "synthetic-nonce".into(),
                approved_at: timestamp.clone(),
                expires_at: timestamp,
            }),
            submission: Some(Submission {
                kind: "synthetic".into(),
                reference: "original-once".into(),
                data: json!({}),
            }),
            preview: Some(preview),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        }
    }
    fn proof(record: &OperationRecord, execution: ConversionSequence) -> VerifiedEffects {
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: wrap(execution),
            evidence: json!({"proofEvents":[{"eventId":"synthetic-parent-fill","orderId":"parent"},{"eventId":"synthetic-child-fill","orderId":"child"}]}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        proof
    }
    #[test]
    fn immutable_approval_accepts_child_growth_and_failed_native_terminal_without_extra_source() {
        let mut record = record();
        validate_prepared(record.preview.as_ref().unwrap()).unwrap();
        validate_cost_bound_contract(&record.preview.as_ref().unwrap().intent).unwrap();
        let first = proof(&record, execution(50, false));
        partial_proofs::validate_pending(&record, &first).unwrap();
        record.effects = Some(first.clone());
        let final_proof = proof(&record, execution(97, true));
        partial_proofs::validate_growth(Some(&first), &final_proof).unwrap();
        validate_effects(&record, &final_proof, &Status::Completed).unwrap();
        assert_eq!(
            ordinary_policy::actual_effect_charge(
                &final_proof.effects,
                &record
                    .preview
                    .as_ref()
                    .unwrap()
                    .valuation
                    .as_ref()
                    .unwrap()
                    .prices
            )
            .unwrap(),
            BigUint::from(200_000_000u64)
        );
        assert_eq!(
            actual_cost_usd(
                &final_proof.effects,
                record.preview.as_ref().unwrap().valuation.as_ref().unwrap()
            )
            .unwrap(),
            BigUint::from(3_000_000u64)
        );
        let failed = proof(&record, execution(60, true));
        validate_effects(&record, &failed, &Status::Failed).unwrap();
        let mut tampered = final_proof.clone();
        if let FinancialEffect::ConversionSequence { execution } = &mut tampered.effects[0] {
            execution.legs[1].received = amount("A", 98);
        }
        assert_eq!(
            validate_effects(&record, &tampered, &Status::Completed)
                .unwrap_err()
                .code,
            "PROOF_DIGEST_MISMATCH"
        );
        let before = record.approval.clone();
        let preview = record.preview.clone();
        assert_eq!(before, record.approval);
        assert_eq!(preview, record.preview);
    }
    #[test]
    fn stage_gross_valuation_and_source_fee_reserves_cannot_be_omitted() {
        let mut p = prepared();
        validate_prepared(&p).unwrap();
        p.valuation.as_mut().unwrap().movement_usd_micros =
            gofin_domain::Quantity::from_u64(100_000_000);
        assert!(validate_prepared(&p).is_err());
        p = prepared();
        p.intent.maximum_costs[0].amount.atoms = gofin_domain::Quantity::from_u64(4);
        assert!(validate_prepared(&p).is_err());
        p = prepared();
        p.intent.debits[0].amount.atoms = gofin_domain::Quantity::from_u64(99);
        assert!(validate_prepared(&p).is_err());
    }
    #[test]
    fn generated_principal_and_input_output_fees_spend_only_prior_net_credit() {
        let effects = validate_execution(&scope(), &execution(97, true), false, true).unwrap();
        let debits = actual_cash_debits(&effects, true).unwrap();
        assert_eq!(debits.len(), 1);
        assert_eq!(debits.values().next().unwrap(), &BigUint::from(100u64));
        let gross_b = effects
            .iter()
            .find_map(|e| match e {
                FinancialEffect::UsableCredit { amount, .. } if amount.asset == asset("B") => {
                    Some(amount.atoms.integer())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(gross_b, BigUint::from(3u64));
        let fees = aggregate_costs(
            &effects
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        FinancialEffect::Cost {
                            treatment: CostTreatment::DeductedFromCredit,
                            ..
                        }
                    )
                })
                .cloned()
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(fees.values().next().unwrap(), &BigUint::from(3u64));
        assert!(validate_execution(&scope(), &execution(98, true), false, true).is_err());
        let mut externally_hidden = execution(97, true);
        externally_hidden.costs[1].funding = ConversionCostFunding::External;
        assert!(validate_execution(&scope(), &externally_hidden, false, true).is_err());
        let mut exhausted = execution(97, true);
        exhausted.costs[0].amount = amount("B", 6);
        assert!(validate_execution(&scope(), &exhausted, false, true).is_err());
    }
    #[test]
    fn fee_only_external_reserve_funds_native_fixed_child_without_reserving_child_principal() {
        let mut p = prepared();
        p.intent.maximum_costs[1].treatment = CostTreatment::AdditionalDebit;
        if let EffectConstraint::ConversionSequence { scope } = &mut p.intent.expected[0] {
            scope.maximum_costs[1].funding = ConversionCostFunding::External;
        }
        validate_prepared(&p).unwrap();
        assert_eq!(p.intent.debits.len(), 1);
        assert_eq!(p.intent.debits[0].amount.asset, asset("A"));
        let mut r = record();
        r.preview = Some(p.clone());
        r.approval.as_mut().unwrap().binding_hash =
            binding_hash(&authorization_binding(&p)).unwrap();
        let mut receipt = execution(100, true);
        for fee in &mut receipt.costs {
            fee.funding = ConversionCostFunding::External;
        }
        let verified = proof(&r, receipt);
        validate_effects(&r, &verified, &Status::Completed).unwrap();
        let debits = actual_debits_and_costs(&verified.effects).unwrap();
        let b = resource_asset_key(&ResourceAmount {
            resource: scope().resource,
            amount: amount("B", 0),
        })
        .unwrap();
        assert_eq!(debits[&b], BigUint::from(3u64));
        let mut excess = execution(101, true);
        for fee in &mut excess.costs {
            fee.funding = ConversionCostFunding::External;
        }
        if let EffectConstraint::ConversionSequence { scope } = &p.intent.expected[0] {
            assert!(validate_execution(scope, &excess, false, true).is_err());
        }
        let mut missing = p.clone();
        missing.intent.maximum_costs[1].amount.atoms = gofin_domain::Quantity::from_u64(2);
        assert!(validate_prepared(&missing).is_err());
    }
    #[test]
    fn cumulative_child_growth_preserves_terminal_parent_and_fees() {
        let old = wrap(execution(50, false));
        let next = wrap(execution(97, true));
        validate_growth(&old, &next).unwrap();
        let mut altered = execution(97, true);
        altered.legs[0].spent = amount("A", 99);
        assert!(validate_growth(&old, &wrap(altered)).is_err());
        assert!(validate_growth(&next, &old).is_err());
        let mut extra = execution(97, true);
        extra.costs.push(ConversionCost {
            leg_id: "parent".into(),
            amount: amount("A", 1),
            category: "later_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
            funding: ConversionCostFunding::External,
        });
        assert!(validate_growth(&old, &wrap(extra)).is_err());
        let mut reversed = execution(49, false);
        reversed.legs[1].received = amount("A", 51);
        assert!(validate_growth(&old, &wrap(reversed)).is_err());
    }
    #[test]
    fn scope_minima_terminal_status_version_and_exact_identity_are_enforced() {
        let mut cap = scope();
        cap.legs[1].minimum_output = gofin_domain::Quantity::from_u64(98);
        assert!(validate_execution(&cap, &execution(97, true), false, true).is_err());
        validate_execution(&cap, &execution(97, true), true, true).unwrap();
        assert!(validate_execution(&scope(), &execution(50, false), true, true).is_err());
        let mut wrong = execution(97, true);
        wrong.accounting_version = 2;
        assert!(validate_execution(&scope(), &wrong, false, true).is_err());
        wrong = execution(97, true);
        wrong.legs[1].spent.asset = asset("LOOKALIKE");
        assert!(validate_execution(&scope(), &wrong, false, true).is_err());
        assert!(
            project_effects(&[
                wrap(execution(97, true))[0].clone(),
                wrap(execution(98, true))[0].clone()
            ])
            .is_err()
        );
    }
}
