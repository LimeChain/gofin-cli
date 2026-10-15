//! One request may revise several original owners while placing new orders.
//! Validate each immutable owner separately, then require their summed reserves.
use super::*;
use gofin_domain::compound_mutations::ChildMutation;

fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "MUTATION_BATCH_INVALID",
        "validation",
        message,
        "Preserve every original owner and obtain fresh exact approval for the complete native batch.",
    )
}
fn positive(
    before: &BTreeMap<String, BigUint>,
    after: &BTreeMap<String, BigUint>,
) -> BTreeMap<String, BigUint> {
    after
        .iter()
        .filter_map(|(key, quantity)| {
            let old = before.get(key).cloned().unwrap_or_default();
            (quantity > &old).then(|| (key.clone(), quantity - old))
        })
        .collect()
}
fn add(target: &mut BTreeMap<String, BigUint>, values: BTreeMap<String, BigUint>) {
    for (key, value) in values {
        *target.entry(key).or_default() += value;
    }
}
fn total(value: &gofin_domain::Valuation) -> BigUint {
    value.movement_usd_micros.integer()
        + value.maximum_fee_usd_micros.integer()
        + value.additional_allocation_usd_micros.integer()
}
fn restrict_tags(
    prepared: &PreparedOperation,
    tags: &BTreeSet<String>,
) -> Result<PreparedOperation> {
    let mut isolated = prepared.clone();
    // The virtual partition contains authority markers only. The complete
    // preparation retains and independently validates new-write baselines.
    if let Some(binding) = isolated.binding.as_object_mut() {
        binding.remove(gofin_domain::BASELINE_ANCHORS_KEY);
    }
    let rows = prepared.input["amendments"]
        .as_array()
        .ok_or_else(|| invalid("The complete native owner map is missing."))?;
    isolated.input["amendments"] = to_value(
        &rows
            .iter()
            .filter(|row| {
                row["orderTag"]
                    .as_str()
                    .is_some_and(|tag| tags.contains(tag))
            })
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    Ok(isolated)
}
impl Engine {
    pub(super) fn validate_mutation_batch(
        &self,
        prepared: &PreparedOperation,
        run_id: Option<&str>,
        bounded: bool,
        attempt_id: Option<&str>,
    ) -> Result<Option<String>> {
        let groups = grouped_amendments::groups(&prepared.intent)?;
        let compound = compound_mutations::children(&prepared.intent)?;
        let composed = groups.len() > 1
            || (compound.is_some() && prepared.intent.expected.len() != 1)
            || (!groups.is_empty() && (compound.is_some() || prepared.intent.expected.len() != 1));
        if !composed {
            return Ok(None);
        }
        if run_id.is_some() || bounded {
            return Err(invalid(
                "Composed authority revisions require fresh exact approval outside plans.",
            ));
        }
        if prepared.intent.expected.iter().any(|effect| {
            matches!(
                effect,
                EffectConstraint::SubmissionAmendment { .. }
                    | EffectConstraint::SubmissionCancellation { .. }
            )
        }) {
            return Err(invalid(
                "Standalone and batch ownership contracts cannot coexist.",
            ));
        }
        let mut owners = BTreeSet::new();
        let mut tags = BTreeSet::new();
        let mut debit_requirements = BTreeMap::new();
        let mut cost_requirements = BTreeMap::new();
        let mut locks = BTreeSet::new();
        let mut required_usd = BigUint::zero();
        for group in &groups {
            if !owners.insert(group.operation_id.clone()) {
                return Err(invalid("A composed request repeats an original owner."));
            }
            let local = group
                .children
                .iter()
                .map(|child| child.child_tag.clone())
                .collect::<BTreeSet<_>>();
            if local.len() != group.children.len()
                || local.iter().any(|tag| !tags.insert(tag.clone()))
            {
                return Err(invalid(
                    "Native child tags must be unique across all owners.",
                ));
            }
            let old = &group.original_scope.intent;
            let new = &group.approved_envelope.intent;
            let debits = positive(
                &aggregate_resource_amounts(&old.debits)?,
                &aggregate_resource_amounts(&new.debits)?,
            );
            let costs = positive(
                &aggregate_cost_bounds(&old.maximum_costs)?,
                &aggregate_cost_bounds(&new.maximum_costs)?,
            );
            let incremental=group.incremental_valuation.as_ref().ok_or_else(||invalid("Each composed group requires its own freshly priced positive incremental valuation."))?;
            let mut isolated = restrict_tags(prepared, &local)?;
            isolated.intent = grouped_amendments::incremental_intent(old, new, &debits, &costs)?;
            isolated.intent.expected = vec![EffectConstraint::GroupedSubmissionAmendments {
                group: Box::new((*group).clone()),
            }];
            isolated.valuation = Some(incremental.clone());
            self.validate_grouped_amendments(&isolated, None, false, attempt_id)
                .map_err(|mut error| {
                    error.message = format!("Grouped partition: {}", error.message);
                    error
                })?;
            let old_usd = total(
                group
                    .original_scope
                    .valuation
                    .as_ref()
                    .ok_or_else(|| invalid("Original group valuation is missing."))?,
            );
            let new_usd = total(
                group
                    .approved_envelope
                    .valuation
                    .as_ref()
                    .ok_or_else(|| invalid("Revised group valuation is missing."))?,
            );
            let scalar = if new_usd > old_usd {
                new_usd - old_usd
            } else {
                BigUint::zero()
            };
            required_usd += std::cmp::max(total(incremental), scalar);
            add(&mut debit_requirements, debits);
            add(&mut cost_requirements, costs);
            locks.extend(old.locks.iter().cloned());
        }
        if let Some(children) = compound {
            let context = self
                .compound_preparation_context(&prepared.input)?
                .ok_or_else(|| invalid("Protected compound original contexts are missing."))?;
            let local = children
                .iter()
                .map(|child| child.child_tag.clone())
                .collect::<BTreeSet<_>>();
            if local.len() != children.len() || local.iter().any(|tag| !tags.insert(tag.clone())) {
                return Err(invalid("Compound tags conflict with another owner group."));
            }
            let mut isolated = restrict_tags(prepared, &local)?;
            isolated.intent.expected = vec![EffectConstraint::CompoundSubmissionMutations {
                children: children.to_vec(),
            }];
            self.validate_compound_mutations(&isolated, None, false, attempt_id)
                .map_err(|mut error| {
                    error.message = format!("Independent mutation partition: {}", error.message);
                    error
                })?;
            for child in children {
                if !owners.insert(child.operation_id.clone()) {
                    return Err(invalid(
                        "One owner cannot appear in both grouped and independent mutation contracts.",
                    ));
                }
                let linked = context
                    .children
                    .get(&child.child_tag)
                    .ok_or_else(|| invalid("Compound child original context is unavailable."))?;
                let original = linked
                    .context
                    .original
                    .as_ref()
                    .ok_or_else(|| invalid("Original child scope is missing."))?;
                locks.extend(original.intent.locks.iter().cloned());
                if let ChildMutation::Amend { revision } = &child.mutation {
                    let old = linked
                        .context
                        .effective_scope()
                        .map(|scope| &scope.intent)
                        .unwrap_or(&original.intent);
                    add(
                        &mut debit_requirements,
                        positive(
                            &aggregate_resource_amounts(&old.debits)?,
                            &aggregate_resource_amounts(&revision.intent.debits)?,
                        ),
                    );
                    add(
                        &mut cost_requirements,
                        positive(
                            &aggregate_cost_bounds(&old.maximum_costs)?,
                            &aggregate_cost_bounds(&revision.intent.maximum_costs)?,
                        ),
                    );
                    let incremental=child.incremental_valuation.as_ref().ok_or_else(||invalid("Each composed independent amendment requires a fresh positive incremental valuation."))?;
                    let mut priced = isolated.clone();
                    priced.valuation = Some(incremental.clone());
                    // Its price rows describe this one positive authority delta,
                    // not the complete parent/new-write asset set. The root
                    // ceiling and each full revision are validated separately.
                    if incremental.prices.is_empty() {
                        // Older synthetic/unpriced scopes had no typed ceiling.
                        // Do not copy a different partition's price identities.
                        if let Some(binding) = priced.binding.as_object_mut() {
                            binding.remove(gofin_domain::value_bounds::KEY);
                        }
                    } else {
                        gofin_domain::value_bounds::bind(&mut priced);
                    }
                    let old_value = linked
                        .context
                        .effective_scope()
                        .and_then(|scope| scope.valuation.as_ref())
                        .or(original.valuation.as_ref())
                        .ok_or_else(|| invalid("Original child valuation is missing."))?;
                    let new_value = revision
                        .valuation
                        .as_ref()
                        .ok_or_else(|| invalid("Revised child valuation is missing."))?;
                    let old_usd = total(old_value);
                    let new_usd = total(new_value);
                    required_usd += std::cmp::max(
                        total(incremental),
                        if new_usd > old_usd {
                            new_usd - old_usd
                        } else {
                            BigUint::zero()
                        },
                    );
                    validate_prepared(&priced, &prepared.operation, Utc::now())?;
                }
            }
        }
        let ordinary = prepared
            .intent
            .expected
            .iter()
            .filter(|constraint| {
                !matches!(
                    constraint,
                    EffectConstraint::GroupedSubmissionAmendments { .. }
                        | EffectConstraint::CompoundSubmissionMutations { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if !ordinary.is_empty() {
            let scope: gofin_domain::amendments::AmendmentScope = from_value(
                prepared.binding.get("mutationNewWriteScope").cloned()
                    .ok_or_else(|| invalid("A composed batch must retain the new financial actions' separate exact scope."))?
            )?;
            if scope.intent.expected != ordinary {
                return Err(invalid(
                    "The new financial partition differs from the parent batch constraints.",
                ));
            }
            let value = scope.valuation.as_ref().ok_or_else(|| {
                invalid("New financial actions require their own full valuation.")
            })?;
            let mut isolated = prepared.clone();
            isolated.intent = scope.intent.clone();
            isolated.binding = scope.binding;
            isolated.valuation = scope.valuation.clone();
            validate_prepared(&isolated, &prepared.operation, Utc::now()).map_err(
                |mut error| {
                    error.message = format!("New-write partition: {}", error.message);
                    error
                },
            )?;
            validate_cost_bound_contract(&isolated.intent)?;
            validate_cost_prices(
                &isolated,
                &prepared_cost_assets(&isolated.intent),
                Utc::now(),
            )?;
            add(
                &mut debit_requirements,
                aggregate_resource_amounts(&isolated.intent.debits)?,
            );
            add(
                &mut cost_requirements,
                aggregate_cost_bounds(&isolated.intent.maximum_costs)?,
            );
            locks.extend(isolated.intent.locks.iter().cloned());
            required_usd += total(value);
        }
        let context = self
            .compound_preparation_context(&prepared.input)?
            .ok_or_else(|| invalid("Native owner tags are missing."))?;
        if tags.len() != context.children.len()
            || context.children.keys().any(|tag| !tags.contains(tag))
        {
            return Err(invalid(
                "Every declared native owner must appear exactly once in the prepared contracts.",
            ));
        }
        let covers = |actual: BTreeMap<String, BigUint>, required: &BTreeMap<String, BigUint>| {
            required
                .iter()
                .all(|(key, value)| actual.get(key).is_some_and(|held| held >= value))
        };
        if !covers(
            aggregate_resource_amounts(&prepared.intent.debits)?,
            &debit_requirements,
        ) || !covers(
            aggregate_cost_bounds(&prepared.intent.maximum_costs)?,
            &cost_requirements,
        ) || !locks.is_subset(&prepared.intent.locks.iter().cloned().collect())
        {
            return Err(invalid(
                "The composed reservation must cover the sum of every positive original-owner increase.",
            ));
        }
        if total(
            prepared
                .valuation
                .as_ref()
                .ok_or_else(|| invalid("Composed incremental valuation is missing."))?,
        ) < required_usd
        {
            return Err(invalid(
                "Composed USD allocation cannot reuse one owner's budget for a sibling.",
            ));
        }
        Ok(Some(binding_hash(&to_value(&(groups, compound))?)?))
    }
}
