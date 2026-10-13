//! Disjoint native edits share one original batch's immutable fill owner.
use crate::{
    KrakenEndpoint as E, contracts,
    profile::{KrakenProfile, error},
    trading::{controls, proof, unresolved},
};
use fin_domain::amendments::AmendmentScope;
use fin_domain::compound_mutations::{
    ChildMutation, ChildMutationOutcome, CompoundAmendmentContext, MutationChild,
    MutationChildProof,
};
use fin_domain::grouped_amendments::{
    BindingReplacement, GroupedAmendmentChild, GroupedAmendmentGroup, GroupedAmendmentProof,
    GroupedChildMutation, compose_binding, derive_accounting_scope, validate_replacements,
};
use fin_domain::{
    EffectConstraint, FinancialEffect, FinancialIntent, OperationRecord, PreparedOperation,
    Quantity, Reconciliation, Result, Status, Submission,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
fn revisions(p: &PreparedOperation) -> Result<AmendmentScope> {
    p.intent
        .expected
        .iter()
        .find_map(|e| match e {
            EffectConstraint::SubmissionAmendment { revision, .. } => Some((**revision).clone()),
            _ => None,
        })
        .ok_or_else(|| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Exact native member revision missing.",
            )
        })
}
fn differences(
    before: &Value,
    after: &Value,
    pointer: &str,
    out: &mut Vec<BindingReplacement>,
) -> Result<()> {
    if before == after {
        return Ok(());
    }
    match (before, after) {
        (Value::Object(old), Value::Object(new)) if old.keys().eq(new.keys()) => {
            for (key, v) in old {
                differences(
                    v,
                    &new[key],
                    &format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    out,
                )?;
            }
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (i, (a, b)) in old.iter().zip(new).enumerate() {
                differences(a, b, &format!("{pointer}/{i}"), out)?;
            }
        }
        (Value::Object(_), _)
        | (_, Value::Object(_))
        | (Value::Array(_), _)
        | (_, Value::Array(_)) => {
            return Err(error(
                "KRAKEN_GROUPED_EDIT_SHAPE_PENDING",
                "Grouped edits replace existing native member fields. Adding an absent field or changing scope shape requires a reviewed presence-aware revision contract; nothing was dispatched.",
            ));
        }
        _ => out.push(BindingReplacement {
            pointer: pointer.into(),
            before: before.clone(),
            after: after.clone(),
        }),
    }
    Ok(())
}
fn remove_new_pins(before: &Value, after: &mut Value) {
    match (before, after) {
        (Value::Object(old), Value::Object(new)) => {
            if !old.contains_key("orderId") {
                new.remove("orderId");
            }
            for (key, value) in new {
                if let Some(prior) = old.get(key) {
                    remove_new_pins(prior, value);
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for (a, b) in old.iter().zip(new) {
                remove_new_pins(a, b);
            }
        }
        _ => (),
    }
}
fn incremental(old: &FinancialIntent, new: &FinancialIntent) -> Result<FinancialIntent> {
    let mut intent = FinancialIntent {
        locks: new.locks.clone(),
        ..Default::default()
    };
    for row in &new.debits {
        let before = old
            .debits
            .iter()
            .filter(|v| v.resource == row.resource && v.amount.asset == row.amount.asset)
            .try_fold(Quantity::zero(), |n, v| n.checked_add(&v.amount.atoms))?;
        if row.amount.atoms > before {
            let mut delta = row.clone();
            delta.amount.atoms = row.amount.atoms.checked_sub(&before)?;
            intent.debits.push(delta);
        }
    }
    for row in &new.maximum_costs {
        let before = old
            .maximum_costs
            .iter()
            .filter(|v| {
                v.location == row.location
                    && v.amount.asset == row.amount.asset
                    && v.treatment == row.treatment
            })
            .try_fold(Quantity::zero(), |n, v| n.checked_add(&v.amount.atoms))?;
        if row.amount.atoms > before {
            let mut delta = row.clone();
            delta.amount.atoms = row.amount.atoms.checked_sub(&before)?;
            intent.maximum_costs.push(delta);
        }
    }
    Ok(intent)
}
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_grouped_edits(
        &self,
        input: &Value,
        linked: &CompoundAmendmentContext,
    ) -> Result<PreparedOperation> {
        let (_, mut native) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, input)?;
        let rows = native["batchOrder"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Exact grouped native rows missing."))?
            .clone();
        let owner = linked.children.values().next().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                "Protected original owner missing.",
            )
        })?;
        if rows.len() < 2
            || rows.len() != linked.children.len()
            || rows
                .iter()
                .any(|r| r["order"] != "edit" && r["order"] != "cancel")
            || linked
                .children
                .values()
                .any(|c| c.operation_id != owner.operation_id)
        {
            return Err(error(
                "KRAKEN_GROUPED_MIXED_OWNERS_PENDING",
                "Grouped native mutations require disjoint edits or cancellations of one protected original batch.",
            ));
        }
        let original = owner.context.original.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                "Protected original preview missing.",
            )
        })?;
        let raw = owner
            .context
            .effective_scope()
            .cloned()
            .unwrap_or(AmendmentScope {
                intent: original.intent.clone(),
                binding: original.binding.clone(),
                valuation: original.valuation.clone(),
            });
        if !raw.binding["derivativeBatch"].is_array() {
            return Err(error(
                "KRAKEN_GROUPED_TARGET_INVALID",
                "Multiple grouped edits must identify distinct native members of an original batch.",
            ));
        }
        let pinned = self
            .pinned_original_scope(&owner.context)
            .await?
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                    "Protected native original scope missing.",
                )
            })?;
        let mut prepared = vec![];
        let mut scopes = vec![];
        let mut tags = BTreeSet::new();
        let mut indices = BTreeSet::new();
        let global = controls(input)?;
        for row in &rows {
            let tag = row["order_tag"]
                .as_str()
                .ok_or_else(|| error("INVALID_INPUT", "An exact native order tag is required."))?;
            if !tags.insert(tag.to_owned()) || !linked.children.contains_key(tag) {
                return Err(error(
                    "INVALID_INPUT",
                    "Grouped native tags must exactly match protected original ownership.",
                ));
            }
            let mapping = input["amendments"]
                .as_array()
                .and_then(|a| a.iter().find(|v| v["orderTag"] == tag))
                .ok_or_else(|| {
                    error(
                        "INVALID_INPUT",
                        "Exact original submission mapping missing.",
                    )
                })?;
            let mut child = input.clone();
            child.as_object_mut().unwrap().remove("amendments");
            child["batchOrder"] = json!([row]);
            child["operationId"] = json!(owner.operation_id);
            child["originalSubmission"] = mapping["originalSubmission"].clone();
            if let Some(limits) = mapping.get("limits") {
                for (key, value) in limits.as_object().ok_or_else(|| {
                    error(
                        "INVALID_INPUT",
                        "Grouped cumulative limits must be exact typed controls.",
                    )
                })? {
                    child[key] = value.clone();
                }
            }
            child["requestId"] = json!(format!("{}:{tag}", global.request_id));
            let p = if row["order"] == "edit" {
                self.prepare_derivative_edit_batch(&child, Some(&pinned))
                    .await?
            } else {
                let (index, _) = self
                    .derivative_original_member(
                        &pinned.binding,
                        row,
                        self.account(input["account"].as_str().unwrap())?,
                    )
                    .await?;
                let mut cancel = row.clone();
                cancel.as_object_mut().unwrap().remove("order");
                cancel.as_object_mut().unwrap().remove("order_tag");
                for key in ["account", "requestId", "operationId", "originalSubmission"] {
                    cancel[key] = child[key].clone();
                }
                let (_, parameters) =
                    contracts::decode(E::KrakenfuturesPrivatePostCancelorder, &cancel)?;
                let mut p = self
                    .prepare_derivative_control_with_scope(
                        E::KrakenfuturesPrivatePostCancelorder,
                        &cancel,
                        parameters,
                        Some(&pinned.binding),
                    )
                    .await?;
                if p.binding["targets"].as_array().map(Vec::len) != Some(1) {
                    return Err(error(
                        "KRAKEN_GROUPED_TARGET_INVALID",
                        "A grouped cancellation must resolve exactly one original member.",
                    ));
                }
                let wire = json!({"order":"cancel","order_tag":tag,"order_id":p.binding["targets"][0]["orderId"]});
                p.binding["parameters"] = json!({"batchOrder":[wire]});
                p.binding["originalBatchMemberIndex"] = json!(index);
                p
            };
            let index = p.binding["originalBatchMemberIndex"]
                .as_u64()
                .ok_or_else(|| {
                    error(
                        "KRAKEN_GROUPED_TARGET_INVALID",
                        "Exact original member index missing.",
                    )
                })?;
            if !indices.insert(index) {
                return Err(error(
                    "KRAKEN_GROUPED_TARGET_REPEATED",
                    "A grouped native edit may change each original member only once.",
                ));
            }
            let mut scope = if row["order"] == "edit" {
                revisions(&p)?
            } else {
                raw.clone()
            };
            remove_new_pins(&raw.binding, &mut scope.binding);
            scopes.push(scope);
            prepared.push(p);
        }
        // Every child agrees on full-owner fees and settlement bounds. They are
        // one shared authority increase, never duplicated once per edited member.
        let amendment_scopes = scopes
            .iter()
            .zip(&rows)
            .filter(|(_, r)| r["order"] == "edit")
            .map(|(s, _)| s)
            .collect::<Vec<_>>();
        if amendment_scopes
            .windows(2)
            .any(|s| s[0].intent.maximum_costs != s[1].intent.maximum_costs)
        {
            return Err(error(
                "KRAKEN_GROUPED_FEE_CAPS_DIVERGE",
                "Grouped children must declare identical full-original per-asset fee ceilings.",
            ));
        }
        let mut group = GroupedAmendmentGroup {
            operation_id: owner.operation_id.clone(),
            submission: serde_json::from_value(
                input["amendments"][0]["originalSubmission"].clone(),
            )
            .map_err(|_| error("INVALID_INPUT", "Original submission invalid."))?,
            original_scope: Box::new(raw.clone()),
            shared_binding_replacements: vec![],
            children: scopes
                .into_iter()
                .zip(&rows)
                .map(|(scope, row)| GroupedAmendmentChild {
                    child_tag: row["order_tag"].as_str().unwrap().into(),
                    mutation: if row["order"] == "edit" {
                        GroupedChildMutation::Amend
                    } else {
                        GroupedChildMutation::Cancel
                    },
                    scope: Box::new(scope),
                    binding_replacements: vec![],
                })
                .collect(),
            approved_envelope: Box::new(raw.clone()),
            incremental_valuation: None,
        };
        // The shared USD ceiling is a conservative whole-group approval ceiling.
        // Financial quantities come from the independently shared no-net helper.
        group.approved_envelope.valuation = group
            .children
            .iter()
            .find(|c| c.mutation == GroupedChildMutation::Amend)
            .map_or(raw.valuation.clone(), |c| c.scope.valuation.clone());
        let mut valuation = derive_accounting_scope(&group, &tags)?.valuation.unwrap();
        let observed = fin_domain::now();
        valuation.observed_at = observed.clone();
        for child in &mut group.children {
            if child.mutation == GroupedChildMutation::Cancel {
                continue;
            }
            if let Some(v) = child.scope.valuation.as_mut() {
                v.observed_at = observed.clone();
                v.prices = valuation.prices.clone();
            }
            let mut full = prepared[0].clone();
            full.intent = child.scope.intent.clone();
            full.binding = child.scope.binding.clone();
            full.valuation = Some(valuation.clone());
            fin_domain::value_bounds::bind(&mut full);
            child.scope.binding = full.binding;
        }
        let mut patches = Vec::new();
        for child in &group.children {
            let mut diff = vec![];
            if child.mutation == GroupedChildMutation::Amend {
                differences(&raw.binding, &child.scope.binding, "", &mut diff)?;
            }
            patches.push(diff);
        }
        if let Some(first) = group
            .children
            .iter()
            .position(|c| c.mutation == GroupedChildMutation::Amend)
        {
            for patch in &patches[first] {
                if !patch.pointer.starts_with("/derivativeBatch/")
                    && !patch.pointer.starts_with("/parameters/batchOrder/")
                    && group
                        .children
                        .iter()
                        .zip(&patches)
                        .filter(|(c, _)| c.mutation == GroupedChildMutation::Amend)
                        .all(|(_, rows)| rows.contains(patch))
                {
                    group.shared_binding_replacements.push(patch.clone());
                }
            }
        }
        for (child, rows) in group.children.iter_mut().zip(patches) {
            child.binding_replacements = rows
                .into_iter()
                .filter(|p| !group.shared_binding_replacements.contains(p))
                .collect();
        }
        group.approved_envelope.binding = compose_binding(&group, tags.iter().map(String::as_str))?;
        group.approved_envelope.valuation = Some(valuation);
        *group.approved_envelope = derive_accounting_scope(&group, &tags)?;
        validate_replacements(&group)?;
        let mut current = prepared[0].clone();
        current.intent = incremental(&raw.intent, &group.approved_envelope.intent)?;
        let mut value = group.approved_envelope.valuation.clone().unwrap();
        value.observed_at = observed;
        if group
            .children
            .iter()
            .all(|c| c.mutation == GroupedChildMutation::Cancel)
        {
            value.prices.clear();
        }
        let old = raw
            .valuation
            .as_ref()
            .ok_or_else(|| error("KRAKEN_PRICE_UNKNOWN", "Original gross valuation missing."))?;
        for (now, prior) in [
            (&mut value.movement_usd_micros, &old.movement_usd_micros),
            (
                &mut value.maximum_fee_usd_micros,
                &old.maximum_fee_usd_micros,
            ),
            (
                &mut value.additional_allocation_usd_micros,
                &old.additional_allocation_usd_micros,
            ),
        ] {
            *now = if *now > *prior {
                now.checked_sub(prior)?
            } else {
                Quantity::zero()
            };
        }
        // Native quantity increases are priced per member before any original
        // scalar comparison. Whole-original fee authority increases only once.
        let exposure_floor = prepared.iter().try_fold(Quantity::zero(), |sum, p| {
            if p.binding["derivativeControl"] == true {
                return Ok(sum);
            }
            let increment: Quantity = serde_json::from_value(
                p.binding["positiveExposureUsdMicros"].clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Member exposure USD floor missing.",
                )
            })?;
            sum.checked_add(&increment)
        })?;
        let existing_exposure = value
            .movement_usd_micros
            .checked_add(&value.additional_allocation_usd_micros)?;
        if exposure_floor > existing_exposure {
            value.additional_allocation_usd_micros = value
                .additional_allocation_usd_micros
                .checked_add(&exposure_floor.checked_sub(&existing_exposure)?)?;
        }
        group.incremental_valuation = Some(value.clone());
        current.valuation = Some(value);
        current.intent.expected = vec![EffectConstraint::GroupedSubmissionAmendments {
            group: Box::new(group.clone()),
        }];
        native["batchOrder"] = json!(
            prepared
                .iter()
                .map(|p| p.binding["parameters"]["batchOrder"][0].clone())
                .collect::<Vec<_>>()
        );
        let children = prepared
            .iter()
            .zip(&rows)
            .map(|(p, r)| {
                let mut b = p.binding.clone();
                b["groupedChildTag"] = r["order_tag"].clone();
                b
            })
            .collect::<Vec<_>>();
        current.binding = json!({"endpoint":E::KrakenfuturesPrivatePostBatchorder.method(),"account":input["account"],"product":self.product,"physicalIdentity":original.binding["physicalIdentity"],"parameters":native,"requestId":global.request_id,"groupedAmendments":group,"groupedNativeChildren":children,"groupedOriginalContext":owner.context});
        // Reuse combined venue margin admission with the unmodified native full scopes.
        let mut admission = current.clone();
        admission.binding["derivativeBatch"] = json!(
            prepared
                .iter()
                .zip(&rows)
                .map(|(p, r)| {
                    let mut b = p.binding.clone();
                    b["compoundChildTag"] = r["order_tag"].clone();
                    b
                })
                .collect::<Vec<_>>()
        );
        admission.intent.expected = vec![EffectConstraint::CompoundSubmissionMutations {
            children: prepared
                .iter()
                .zip(&rows)
                .map(|(p, r)| {
                    Ok(MutationChild {
                        child_tag: r["order_tag"].as_str().unwrap().into(),
                        operation_id: owner.operation_id.clone(),
                        submission: group.submission.clone(),
                        incremental_valuation: p.valuation.clone(),
                        mutation: if r["order"] == "edit" {
                            ChildMutation::Amend {
                                revision: Box::new(revisions(p)?),
                            }
                        } else {
                            ChildMutation::Cancel
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        }];
        self.compound_margin_admission(&admission).await?;
        current.operation = E::KrakenfuturesPrivatePostBatchorder.operation().into();
        current.input = input.clone();
        current.summary = format!(
            "Mutate {} disjoint native members of original batch {}; original fills and conservative cumulative authority retain one owner.",
            rows.len(),
            owner.operation_id
        );
        if current
            .valuation
            .as_ref()
            .is_some_and(|v| !v.prices.is_empty())
        {
            fin_domain::value_bounds::bind(&mut current);
        }
        Ok(current)
    }
    pub(crate) async fn reconcile_derivative_grouped_edits(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        let group: GroupedAmendmentGroup =
            serde_json::from_value(binding["groupedAmendments"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Protected grouped owner binding invalid.",
                )
            })?;
        let mut outcomes = vec![];
        let mut events = vec![];
        let mut rejected = false;
        for child in &group.children {
            let native = record
                .dispatch_evidence
                .as_ref()
                .filter(|r| r["response"]["result"] == "success")
                .and_then(|r| r["response"]["batchStatus"].as_array())
                .map(|rows| {
                    rows.iter()
                        .filter(|r| r["order_tag"] == child.child_tag)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if native.len() != 1 {
                continue;
            }
            let cancel = child.mutation == GroupedChildMutation::Cancel;
            if (cancel && matches!(native[0]["status"].as_str(), Some("filled" | "notFound")))
                || (!cancel
                    && fin_exchange::kraken::no_fill_futures_rejection(
                        E::KrakenfuturesPrivatePostEditorder,
                        &json!({"result":"success","editStatus":native[0]}),
                    ))
            {
                rejected = true;
                outcomes.push(MutationChildProof {
                    child_tag: child.child_tag.clone(),
                    operation_id: group.operation_id.clone(),
                    submission: group.submission.clone(),
                    outcome: ChildMutationOutcome::Rejected {
                        code: native[0]["status"].as_str().unwrap().into(),
                    },
                    native_evidence: json!({"taggedNativeOutcome":native[0]}),
                });
                continue;
            }
            if cancel && native[0]["status"] != "cancelled" {
                continue;
            }
            let b = binding["groupedNativeChildren"]
                .as_array()
                .and_then(|rows| {
                    rows.iter()
                        .find(|r| r["groupedChildTag"] == child.child_tag)
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Frozen grouped native proof owner missing.",
                    )
                })?;
            let observed =
                Box::pin(self.reconcile_derivatives_binding(record, submission, b)).await?;
            let Some(bundle) = observed
                .verified
                .filter(|_| observed.status == Status::Completed)
            else {
                continue;
            };
            let matched = bundle.effects.iter().any(|e| match e {
                FinancialEffect::SubmissionAmendment {
                    operation_id,
                    submission,
                    ..
                } if !cancel => {
                    operation_id == &group.operation_id && submission == &group.submission
                }
                FinancialEffect::SubmissionCancellation {
                    operation_id,
                    submission,
                } if cancel => {
                    operation_id == &group.operation_id && submission == &group.submission
                }
                _ => false,
            });
            if !matched {
                continue;
            }
            let evidence = json!({"bindingReplacementDigest":fin_domain::binding_hash(&serde_json::to_value(&child.binding_replacements).map_err(|_|error("KRAKEN_EVIDENCE_INVALID","Grouped native replacements invalid."))?)?,"nativeAudit":if cancel {native[0].clone()} else {bundle.evidence["acknowledgedEdit"].clone()},"nativeReadback":if cancel {json!({"terminalOrders":bundle.evidence["terminalOrders"]})} else {bundle.evidence["durableOrderUpdate"].clone()}});
            outcomes.push(MutationChildProof {
                child_tag: child.child_tag.clone(),
                operation_id: group.operation_id.clone(),
                submission: group.submission.clone(),
                outcome: ChildMutationOutcome::Accepted { successor: None },
                native_evidence: evidence.clone(),
            });
            events.push(json!({"eventId":format!("krakenfutures:grouped-edit:{}:{}",submission.reference,child.child_tag),"childTag":child.child_tag,"nativeEvidence":evidence}));
        }
        if outcomes.is_empty() {
            return Ok(unresolved(
                Status::Pending,
                "No exact native grouped edit audit/readback is proven; nothing is replayed.",
                json!({}),
            ));
        }
        let terminal = outcomes.len() == group.children.len();
        // A known negative child is itself an immutable proof event when it is
        // the only proved outcome while siblings remain unresolved. Retain it
        // at terminal reconciliation after a restart as well.
        let negative_events = outcomes.iter().filter(|p| matches!(p.outcome, ChildMutationOutcome::Rejected {..})).map(|p|
            json!({"eventId":format!("krakenfutures:grouped-rejection:{}:{}",submission.reference,p.child_tag),"nativeRejection":p})).collect::<Vec<_>>();
        let had_negative_events = record
            .effects
            .as_ref()
            .and_then(|e| e.evidence["proofEvents"].as_array())
            .is_some_and(|old| {
                old.iter()
                    .any(|v| negative_events.iter().any(|n| n["eventId"] == v["eventId"]))
            });
        if (!terminal && events.is_empty()) || had_negative_events {
            events.extend(negative_events);
        }
        let status = if !terminal {
            Status::Pending
        } else if rejected {
            Status::Failed
        } else {
            Status::Completed
        };
        proof(
            submission,
            status.clone(),
            vec![FinancialEffect::GroupedSubmissionAmendments {
                proof: GroupedAmendmentProof {
                    operation_id: group.operation_id,
                    submission: group.submission,
                    children: outcomes,
                },
            }],
            json!({"proofEvents":events,"partialExecution":rejected,"allNativeChildrenResolved":terminal}),
        )
    }
}
