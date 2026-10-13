//! One native batch can mutate several owners while new orders retain their own fills.
use crate::{
    KrakenEndpoint as E, contracts,
    profile::{KrakenProfile, error},
    trading::{controls, proof, unresolved},
};
use fin_domain::{
    FinancialIntent, OperationRecord, PreparedOperation, Quantity, Reconciliation, Result, Status,
    Submission, Valuation,
};
use fin_domain::{amendments::AmendmentScope, compound_mutations::CompoundAmendmentContext};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn subset(input: &Value, rows: Vec<Value>, tags: &BTreeSet<String>) -> Value {
    let mut sub = input.clone();
    sub["batchOrder"] = json!(rows);
    sub["amendments"] = json!(
        input["amendments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["orderTag"].as_str().is_some_and(|t| tags.contains(t)))
            .cloned()
            .collect::<Vec<_>>()
    );
    sub
}
fn add_valuation(total: &mut Valuation, value: &Valuation) -> Result<()> {
    total.movement_usd_micros = total
        .movement_usd_micros
        .checked_add(&value.movement_usd_micros)?;
    total.maximum_fee_usd_micros = total
        .maximum_fee_usd_micros
        .checked_add(&value.maximum_fee_usd_micros)?;
    total.additional_allocation_usd_micros = total
        .additional_allocation_usd_micros
        .checked_add(&value.additional_allocation_usd_micros)?;
    for p in &value.prices {
        if let Some(old) = total.prices.iter().find(|old| old.asset == p.asset) {
            if old.usd_micros_per_unit != p.usd_micros_per_unit
                || old.unit_atoms != p.unit_atoms
                || old.source != p.source
            {
                return Err(error(
                    "KRAKEN_PRICE_CHANGED",
                    "Native batch price changed between independent owner scopes.",
                ));
            }
        } else {
            total.prices.push(p.clone());
        }
    }
    Ok(())
}
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_unified(
        &self,
        input: &Value,
        linked: &CompoundAmendmentContext,
    ) -> Result<PreparedOperation> {
        let (_, mut native) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, input)?;
        let rows = native["batchOrder"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Native batch rows missing."))?
            .clone();
        let mut by_owner = BTreeMap::<String, Vec<Value>>::new();
        let mut remaining = vec![];
        let mut alltags = BTreeSet::new();
        for row in &rows {
            let tag = row["order_tag"]
                .as_str()
                .ok_or_else(|| error("INVALID_INPUT", "Native rows require exact tags."))?;
            if !alltags.insert(tag.to_owned()) {
                return Err(error(
                    "INVALID_INPUT",
                    "Native batch tags must be distinct.",
                ));
            }
            if let Some(owner) = linked.children.get(tag) {
                by_owner
                    .entry(owner.operation_id.clone())
                    .or_default()
                    .push(row.clone());
            } else {
                if row["order"] == "edit" {
                    return Err(error(
                        "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                        "Every edited row requires its protected original owner.",
                    ));
                }
                remaining.push(row.clone());
            }
        }
        if linked.children.keys().any(|tag| !alltags.contains(tag)) {
            return Err(error(
                "INVALID_INPUT",
                "Every protected owner tag must occur in the native batch.",
            ));
        }
        let mut partitions = vec![];
        let mut distinct = vec![];
        let mut distincttags = BTreeSet::new();
        for ownerrows in by_owner.into_values() {
            let tags = ownerrows
                .iter()
                .map(|r| r["order_tag"].as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>();
            if ownerrows.len() > 1 {
                let context = CompoundAmendmentContext {
                    children: linked
                        .children
                        .iter()
                        .filter(|(tag, _)| tags.contains(*tag))
                        .map(|(t, c)| (t.clone(), c.clone()))
                        .collect(),
                };
                let p =
                    Box::pin(self.prepare_derivative_grouped_edits(
                        &subset(input, ownerrows, &tags),
                        &context,
                    ))
                    .await?;
                partitions.push(p);
            } else {
                distinct.extend(ownerrows);
                distincttags.extend(tags);
            }
        }
        if !distinct.is_empty() {
            let context = CompoundAmendmentContext {
                children: linked
                    .children
                    .iter()
                    .filter(|(tag, _)| distincttags.contains(*tag))
                    .map(|(t, c)| (t.clone(), c.clone()))
                    .collect(),
            };
            partitions.push(
                Box::pin(self.prepare_derivative_compound(
                    &subset(input, distinct, &distincttags),
                    &context,
                ))
                .await?,
            );
        }
        let c = controls(input)?;
        let mut available = c.maximum_fees.clone();
        for p in &partitions {
            for cost in &p.intent.maximum_costs {
                let code = &cost.amount.asset.reference;
                let cap = available.get(code).ok_or_else(|| {
                    error(
                        "KRAKEN_FEE_BOUNDS_REQUIRED",
                        "Each incremental fee needs an exact whole-batch ceiling.",
                    )
                })?;
                let left = Quantity::from_decimal(cap, 36)?
                    .checked_sub(&cost.amount.atoms)
                    .map_err(|_| {
                        error(
                            "KRAKEN_BATCH_FEE_ALLOCATION_REQUIRED",
                            "Independent owner fee increases exceed this batch's exact global cap.",
                        )
                    })?;
                available.insert(code.clone(), left.to_decimal(36));
            }
        }
        let mut new_scope = None;
        if !remaining.is_empty() {
            let mut sub = subset(input, remaining, &BTreeSet::new());
            sub.as_object_mut().unwrap().remove("amendments");
            sub["maximumFees"] = json!(available);
            if sub["batchOrder"]
                .as_array()
                .is_some_and(|rows| rows.iter().all(|row| row["order"] == "cancel"))
            {
                contracts::remove_monetary_controls(&mut sub);
            }
            let (_, params) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, &sub)?;
            let p = Box::pin(self.prepare_derivative_batch(&sub, params)).await?;
            new_scope = Some(AmendmentScope {
                intent: p.intent.clone(),
                binding: p.binding.clone(),
                valuation: p.valuation.clone(),
            });
            partitions.push(p);
        }
        let mut current = partitions
            .first()
            .cloned()
            .ok_or_else(|| error("INVALID_INPUT", "Protected native batch is empty."))?;
        current.intent = FinancialIntent::default();
        current.valuation = Some(self.financial_valuation(&[], &[]).await?);
        let mut wire = BTreeMap::new();
        let mut frames = vec![];
        for p in &partitions {
            current.intent.debits.extend(p.intent.debits.clone());
            current
                .intent
                .maximum_costs
                .extend(p.intent.maximum_costs.clone());
            current.intent.locks.extend(p.intent.locks.clone());
            current.intent.expected.extend(p.intent.expected.clone());
            add_valuation(
                current.valuation.as_mut().unwrap(),
                p.valuation.as_ref().ok_or_else(|| {
                    error(
                        "KRAKEN_VALUATION_UNAVAILABLE",
                        "Every native partition needs exact gross valuation.",
                    )
                })?,
            )?;
            for row in p.binding["parameters"]["batchOrder"]
                .as_array()
                .into_iter()
                .flatten()
            {
                wire.insert(row["order_tag"].as_str().unwrap().to_owned(), row.clone());
            }
            frames.push(p.binding.clone());
        }
        native["batchOrder"] = json!(
            rows.iter()
                .map(|r| wire
                    .get(r["order_tag"].as_str().unwrap())
                    .cloned()
                    .ok_or_else(|| error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Native partition omitted an original tag."
                    )))
                .collect::<Result<Vec<_>>>()?
        );
        current.intent.locks.sort_by(|a, b| a.0.cmp(&b.0));
        current.intent.locks.dedup();
        current.observed_at = fin_domain::now();
        current.operation = E::KrakenfuturesPrivatePostBatchorder.operation().into();
        current.input = input.clone();
        current.binding = json!({"endpoint":E::KrakenfuturesPrivatePostBatchorder.method(),"account":input["account"],"product":self.product,"physicalIdentity":partitions[0].binding["physicalIdentity"],"parameters":native,"requestId":c.request_id,"unifiedNativePartitions":frames,"unifiedOriginalContext":linked});
        if let Some(scope) = new_scope {
            current.binding[fin_domain::BASELINE_ANCHORS_KEY] =
                scope.binding[fin_domain::BASELINE_ANCHORS_KEY].clone();
            current.binding["mutationNewWriteScope"] = json!(scope);
        }
        current.summary = format!(
            "Submit {} native rows across {} independently bound financial partitions; each original retains its fill ownership.",
            rows.len(),
            partitions.len()
        );
        // Venue margin feasibility must be checked for the whole native batch;
        // passing every partition independently cannot authorize their sum.
        let mut admission = current.clone();
        let mut margin_bindings = Vec::new();
        let mut margin_children = Vec::new();
        for partition in &partitions {
            for constraint in &partition.intent.expected {
                match constraint {
                    fin_domain::EffectConstraint::GroupedSubmissionAmendments { group } => {
                        for child in &group.children {
                            let mut frozen = partition.binding["groupedNativeChildren"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|b| b["groupedChildTag"] == child.child_tag)
                                .unwrap()
                                .clone();
                            frozen["compoundChildTag"] = json!(child.child_tag);
                            margin_bindings.push(frozen);
                            margin_children.push(fin_domain::compound_mutations::MutationChild {
                                child_tag: child.child_tag.clone(),
                                operation_id: group.operation_id.clone(),
                                submission: group.submission.clone(),
                                mutation: match child.mutation {
                                    fin_domain::grouped_amendments::GroupedChildMutation::Amend => fin_domain::compound_mutations::ChildMutation::Amend { revision: child.scope.clone() },
                                    fin_domain::grouped_amendments::GroupedChildMutation::Cancel => fin_domain::compound_mutations::ChildMutation::Cancel,
                                },
                                incremental_valuation: None,
                            });
                        }
                    }
                    fin_domain::EffectConstraint::CompoundSubmissionMutations { children } => {
                        margin_children.extend(children.clone())
                    }
                    _ => {}
                }
            }
            margin_bindings.extend(
                partition.binding["derivativeBatch"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        admission.binding["derivativeBatch"] = json!(margin_bindings);
        admission.intent.expected =
            vec![fin_domain::EffectConstraint::CompoundSubmissionMutations {
                children: margin_children,
            }];
        self.compound_margin_admission(&admission).await?;
        if current
            .valuation
            .as_ref()
            .is_some_and(|v| !v.prices.is_empty())
        {
            fin_domain::value_bounds::bind(&mut current);
        }
        Ok(current)
    }
    pub(crate) async fn reconcile_derivative_unified(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        let frames = binding["unifiedNativePartitions"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Frozen native partitions missing.",
                )
            })?;
        let mut effects = vec![];
        let mut events = BTreeMap::<String, Value>::new();
        let mut terminal = true;
        let mut failed = false;
        let mut observations = vec![];
        let mut consumed = None;
        for frame in frames {
            let tags = frame["parameters"]["batchOrder"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["order_tag"].as_str())
                .collect::<BTreeSet<_>>();
            let mut scoped = record.clone();
            if let Some(receipt) = scoped.dispatch_evidence.as_mut()
                && let Some(rows) = receipt["response"]["batchStatus"].as_array()
            {
                receipt["response"]["batchStatus"] = json!(
                    rows.iter()
                        .filter(|r| r["order_tag"].as_str().is_some_and(|t| tags.contains(t)))
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
            let observed = match Box::pin(
                self.reconcile_derivative_batch(&scoped, submission, frame),
            )
            .await
            {
                Ok(observed) => observed,
                Err(error) if *frame == binding["mutationNewWriteScope"]["binding"] => {
                    terminal = false;
                    observations.push(json!({"tags":tags,"status":Status::Unknown,"financialEvidenceError":{"code":error.code,"message":error.message},"financialEffectsUnsettled":true}));
                    continue;
                }
                Err(error) => return Err(error),
            };
            terminal &= observed.status.terminal();
            failed |= observed.status == Status::Failed;
            if let Some(proof) = fin_domain::unresolved_consumption::read(&observed.evidence)? {
                if consumed.is_some() {
                    return Err(error(
                        "KRAKEN_PROOF_INVALID",
                        "Several unknown financial partitions cannot share one consumed-cost scope.",
                    ));
                }
                for event in proof.evidence["proofEvents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let id = event["eventId"].as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_PROOF_INVALID",
                            "Consumed event identity is missing.",
                        )
                    })?;
                    if let Some(old) = events.insert(id.into(), event.clone())
                        && old != *event
                    {
                        return Err(error(
                            "KRAKEN_PROOF_INVALID",
                            "Consumed native event changed across partitions.",
                        ));
                    }
                }
                consumed = Some(proof);
            }
            observations
                .push(json!({"tags":tags,"status":observed.status,"evidence":observed.evidence}));
            if let Some(bundle) = observed.verified {
                // A proved negative child is an immutable receipt event too.
                // It can resolve an owner's mutation while unrelated new orders
                // remain pending without manufacturing a financial fill.
                for effect in &bundle.effects {
                    let outcomes = match effect {
                        fin_domain::FinancialEffect::GroupedSubmissionAmendments { proof } => {
                            Some(&proof.children)
                        }
                        fin_domain::FinancialEffect::CompoundSubmissionMutations { children } => {
                            Some(children)
                        }
                        _ => None,
                    };
                    for child in outcomes.into_iter().flatten() {
                        if matches!(
                            child.outcome,
                            fin_domain::compound_mutations::ChildMutationOutcome::Rejected { .. }
                        ) {
                            let id = format!(
                                "krakenfutures:batch-rejection:{}:{}",
                                submission.reference, child.child_tag
                            );
                            let event = json!({"eventId":id,"nativeRejection":child});
                            if let Some(old) = events.insert(id, event.clone())
                                && old != event
                            {
                                return Err(error(
                                    "KRAKEN_PROOF_INVALID",
                                    "Conflicting native rejection event identity.",
                                ));
                            }
                        }
                    }
                }
                effects.extend(bundle.effects);
                for event in bundle.evidence["proofEvents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let id = event["eventId"].as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_PROOF_INVALID",
                            "Native proof event lacks an immutable identity.",
                        )
                    })?;
                    if let Some(prior) = events.insert(id.into(), event.clone())
                        && prior != *event
                    {
                        return Err(error(
                            "KRAKEN_PROOF_INVALID",
                            "Native partitions produced conflicting immutable event content.",
                        ));
                    }
                }
            }
        }
        if let Some(consumed) = consumed {
            let markers = effects
                .into_iter()
                .filter(crate::unresolved_consumption::is_marker)
                .collect();
            return crate::unresolved_consumption::new_write_overrun_with_progress(
                record,
                submission,
                &consumed.costs,
                markers,
                json!({"partitions":observations,"proofEvents":events.into_values().collect::<Vec<_>>(),"newFinancialEffectsUnsettled":true}),
            );
        }
        if effects.is_empty() && events.is_empty() {
            return Ok(unresolved(
                Status::Pending,
                "Native partition proofs remain incomplete; no commands are replayed.",
                json!({"partitions":observations}),
            ));
        }
        proof(
            submission,
            if terminal {
                if failed {
                    Status::Failed
                } else {
                    Status::Completed
                }
            } else {
                Status::Pending
            },
            effects,
            json!({"partitions":observations,"partialExecution":failed,"proofEvents":events.into_values().collect::<Vec<_>>()}),
        )
    }
}
