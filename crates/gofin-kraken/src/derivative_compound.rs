//! Native batch mutations retain each original order's independent fill ownership.
use crate::{
    KrakenEndpoint as E, contracts,
    profile::{KrakenProfile, error},
    trading::controls,
};
use gofin_domain::amendments::AmendmentScope;
use gofin_domain::compound_mutations::{ChildMutation, CompoundAmendmentContext, MutationChild};
use gofin_domain::{EffectConstraint, FinancialIntent, PreparedOperation, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_compound(
        &self,
        input: &Value,
        linked: &CompoundAmendmentContext,
    ) -> Result<PreparedOperation> {
        let unique = linked
            .children
            .values()
            .map(|c| &c.operation_id)
            .collect::<BTreeSet<_>>();
        if unique.len() < linked.children.len() {
            let pure = unique.len() == 1
                && input["batchOrder"]
                    .as_array()
                    .is_some_and(|rows| rows.len() == linked.children.len());
            if pure {
                return Box::pin(self.prepare_derivative_grouped_edits(input, linked)).await;
            }
            return Box::pin(self.prepare_derivative_unified(input, linked)).await;
        }
        let (_, mut native) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, input)?;
        let rows = native["batchOrder"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Native batch rows missing."))?
            .clone();
        let c = controls(input)?;
        let mut owners = BTreeSet::new();
        let mut tags = BTreeSet::new();
        let mut children = Vec::new();
        let mut prepared = Vec::new();
        let mut remaining = Vec::new();
        let mut wire = BTreeMap::new();
        for row in &rows {
            let tag = row["order_tag"]
                .as_str()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    error(
                        "INVALID_INPUT",
                        "Each native mutation needs an exact order_tag.",
                    )
                })?;
            if !tags.insert(tag.to_owned()) {
                return Err(error(
                    "INVALID_INPUT",
                    "Native batch tags must be distinct.",
                ));
            }
            let Some(owner) = linked.children.get(tag) else {
                if row["order"] == "edit" {
                    return Err(error(
                        "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                        "Every edited native tag needs a protected original operation.",
                    ));
                }
                remaining.push(row.clone());
                continue;
            };
            if !owners.insert(owner.operation_id.clone()) {
                return Err(error(
                    "KRAKEN_BATCH_OWNER_REPEATED",
                    "Combine repeated edits of the same original batch into one cumulative revision; independent child revisions cannot race.",
                ));
            }
            let original = owner.context.original.as_ref().ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                    "Protected original preview missing.",
                )
            })?;
            let submission = input["amendments"]
                .as_array()
                .and_then(|a| a.iter().find(|a| a["orderTag"] == tag))
                .and_then(|a| serde_json::from_value(a["originalSubmission"].clone()).ok())
                .ok_or_else(|| {
                    error(
                        "INVALID_INPUT",
                        "Exact original submission for the tagged mutation missing.",
                    )
                })?;
            let original_scope = self
                .pinned_original_scope(&owner.context)
                .await?
                .unwrap_or_else(|| AmendmentScope {
                    intent: original.intent.clone(),
                    binding: original.binding.clone(),
                    valuation: original.valuation.clone(),
                });
            let previous = &original_scope;
            let mut child = input.clone();
            child.as_object_mut().unwrap().remove("amendments");
            child["batchOrder"] = json!([row]);
            child["operationId"] = json!(owner.operation_id);
            child["originalSubmission"] = json!(submission);
            let p = if row["order"] == "edit" {
                let member = if previous.binding["derivativeBatch"].is_array() {
                    self.derivative_original_member(
                        &previous.binding,
                        row,
                        self.account(input["account"].as_str().unwrap())?,
                    )
                    .await?
                    .1
                } else {
                    previous.binding.clone()
                };
                let currency = member["derivative"]["collateral"]["reference"]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_AMENDMENT_TARGET_INVALID",
                            "Exact original settlement currency missing.",
                        )
                    })?;
                child["maximumFees"] = json!({currency:c.maximum_fees.get(currency).ok_or_else(||error("KRAKEN_FEE_BOUNDS_REQUIRED","Each original currency needs an exact cumulative fee cap."))?});
                let wallet = member["derivative"]["wallet"].as_str().ok_or_else(|| {
                    error(
                        "KRAKEN_AMENDMENT_TARGET_INVALID",
                        "The protected original must identify an exact native derivative order.",
                    )
                })?;
                if !c.settlement_limits.is_empty() {
                    child["settlementLimits"] = json!({wallet:c.settlement_limits.get(wallet).ok_or_else(||error("KRAKEN_BATCH_SETTLEMENT_LIMITS_REQUIRED","Each edited native wallet needs exact settlement limits."))?});
                }
                if let Some(limit) = c.settlement_limits.get(wallet) {
                    for (key, value) in serde_json::to_value(limit)
                        .map_err(|_| error("INVALID_INPUT", "Native wallet limits invalid."))?
                        .as_object()
                        .unwrap()
                    {
                        child[key] = value.clone();
                    }
                }
                child.as_object_mut().unwrap().remove("settlementLimits");
                if let Some(limits) = input["amendments"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|r| r["orderTag"] == tag))
                    .and_then(|r| r.get("limits"))
                {
                    for (key, value) in limits.as_object().ok_or_else(|| {
                        error("INVALID_INPUT", "Exact child limits must be an object.")
                    })? {
                        child[key] = value.clone();
                    }
                }
                self.prepare_derivative_edit_batch(&child, Some(previous))
                    .await?
            } else if row["order"] == "cancel" {
                let mut cancel = row.clone();
                cancel.as_object_mut().unwrap().remove("order");
                cancel.as_object_mut().unwrap().remove("order_tag");
                for key in ["account", "requestId", "operationId", "originalSubmission"] {
                    cancel[key] = child[key].clone();
                }
                let (_, parameters) =
                    contracts::decode(E::KrakenfuturesPrivatePostCancelorder, &cancel)?;
                self.prepare_derivative_control_with_scope(
                    E::KrakenfuturesPrivatePostCancelorder,
                    &cancel,
                    parameters,
                    Some(&previous.binding),
                )
                .await?
            } else {
                return Err(error(
                    "INVALID_INPUT",
                    "An original-owner mutation must be native edit or cancel.",
                ));
            };
            let mutation = if row["order"] == "edit" {
                p.intent
                    .expected
                    .iter()
                    .find_map(|e| match e {
                        EffectConstraint::SubmissionAmendment { revision, .. } => {
                            Some(ChildMutation::Amend {
                                revision: revision.clone(),
                            })
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_PREVIEW_INVALID",
                            "Revised cumulative financial scope missing.",
                        )
                    })?
            } else {
                ChildMutation::Cancel
            };
            let native_row = if row["order"] == "edit" {
                p.binding["parameters"]["batchOrder"][0].clone()
            } else {
                let target = &p.binding["targets"][0];
                if !target.is_object() {
                    return Err(error(
                        "KRAKEN_ORDER_PROOF_PENDING",
                        "Exact native cancellation target missing.",
                    ));
                }
                json!({"order":"cancel","order_tag":tag,"order_id":target["orderId"]})
            };
            wire.insert(tag.to_owned(), native_row);
            let mut p = p;
            p.binding["compoundChildTag"] = json!(tag);
            children.push(MutationChild {
                child_tag: tag.into(),
                operation_id: owner.operation_id.clone(),
                submission,
                mutation,
                incremental_valuation: p.valuation.clone(),
            });
            prepared.push(p);
        }
        if children.len() != linked.children.len() || children.is_empty() {
            return Err(error(
                "INVALID_INPUT",
                "Protected mutation tags must map exactly to native edited/canceled rows.",
            ));
        }
        let mut current = if remaining.is_empty() {
            let mut p = prepared[0].clone();
            p.intent = FinancialIntent::default();
            p.valuation = Some(self.financial_valuation(&[], &[]).await?);
            p.binding["derivativeBatch"] = json!([]);
            p
        } else {
            let mut subset = input.clone();
            subset.as_object_mut().unwrap().remove("amendments");
            subset.as_object_mut().unwrap().remove("operationId");
            subset.as_object_mut().unwrap().remove("originalSubmission");
            subset["batchOrder"] = json!(remaining);
            if remaining.iter().all(|row| row["order"] == "cancel") {
                crate::contracts::remove_monetary_controls(&mut subset);
            }
            let mut available = c.maximum_fees.clone();
            for p in &prepared {
                for cost in &p.intent.maximum_costs {
                    if cost.treatment == gofin_domain::CostTreatment::AdditionalDebit {
                        let code = &cost.amount.asset.reference;
                        let cap = available.get(code).ok_or_else(|| {
                            error(
                                "KRAKEN_FEE_BOUNDS_REQUIRED",
                                "Every incremental fee currency needs its exact global cap.",
                            )
                        })?;
                        let left=gofin_domain::Quantity::from_decimal(cap,36)?.checked_sub(&cost.amount.atoms).map_err(|_|error("KRAKEN_BATCH_FEE_ALLOCATION_REQUIRED","The original-owner fee increases exceed this batch's currency cap."))?;
                        available.insert(code.clone(), left.to_decimal(36));
                    }
                }
            }
            subset["maximumFees"] = json!(available);
            if remaining.iter().all(|row| row["order"] == "cancel") {
                crate::contracts::remove_monetary_controls(&mut subset);
            }
            if !c.settlement_limits.is_empty() {
                let a = self.account(input["account"].as_str().ok_or_else(|| {
                    error("INVALID_INPUT", "Exact derivative account missing.")
                })?)?;
                let mut limits = serde_json::Map::new();
                for r in &remaining {
                    if r["order"] == "send" {
                        let (wallet, _) = self
                            .derivative_batch_wallet(a, r["symbol"].as_str().unwrap_or(""))
                            .await?;
                        limits.insert(
                            wallet.clone(),
                            json!(c.settlement_limits.get(&wallet).ok_or_else(|| error(
                                "KRAKEN_BATCH_SETTLEMENT_LIMITS_REQUIRED",
                                "Every new native wallet needs settlement limits."
                            ))?),
                        );
                    }
                }
                if limits.is_empty() {
                    subset.as_object_mut().unwrap().remove("settlementLimits");
                } else {
                    subset["settlementLimits"] = Value::Object(limits);
                }
            }
            let (_, params) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, &subset)?;
            self.prepare_derivative_batch(&subset, params).await?
        };
        if !remaining.is_empty() {
            current.binding["mutationNewWriteScope"] = json!(AmendmentScope {
                intent: current.intent.clone(),
                binding: current.binding.clone(),
                valuation: current.valuation.clone()
            });
        }
        for r in current.binding["parameters"]["batchOrder"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(tag) = r["order_tag"].as_str() {
                wire.entry(tag.into()).or_insert_with(|| r.clone());
            }
        }
        let mut bindings = current.binding["derivativeBatch"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if current.valuation.is_none() {
            current.valuation = Some(
                self.financial_valuation(&current.intent.debits, &current.intent.maximum_costs)
                    .await?,
            );
        }
        for p in prepared {
            current.intent.debits.extend(p.intent.debits);
            current.intent.maximum_costs.extend(p.intent.maximum_costs);
            current.intent.locks.extend(p.intent.locks);
            let v = p.valuation.ok_or_else(|| {
                error(
                    "KRAKEN_VALUATION_UNAVAILABLE",
                    "Mutation delta valuation missing.",
                )
            })?;
            let total = current.valuation.as_mut().unwrap();
            total.movement_usd_micros = total
                .movement_usd_micros
                .checked_add(&v.movement_usd_micros)?;
            total.maximum_fee_usd_micros = total
                .maximum_fee_usd_micros
                .checked_add(&v.maximum_fee_usd_micros)?;
            total.additional_allocation_usd_micros = total
                .additional_allocation_usd_micros
                .checked_add(&v.additional_allocation_usd_micros)?;
            for price in v.prices {
                if let Some(prior) = total.prices.iter().find(|p| p.asset == price.asset) {
                    if prior.usd_micros_per_unit != price.usd_micros_per_unit
                        || prior.unit_atoms != price.unit_atoms
                        || prior.source != price.source
                    {
                        return Err(error(
                            "KRAKEN_PRICE_CHANGED",
                            "Protected native batch price changed between owners.",
                        ));
                    }
                } else {
                    total.prices.push(price);
                }
            }
            bindings.push(p.binding);
        }
        current.intent.locks.sort();
        current.intent.locks.dedup();
        let mut incremental_fees = BTreeMap::<String, gofin_domain::Quantity>::new();
        for cost in &current.intent.maximum_costs {
            if cost.treatment == gofin_domain::CostTreatment::AdditionalDebit {
                let total = incremental_fees
                    .entry(cost.amount.asset.reference.clone())
                    .or_default();
                *total = total.checked_add(&cost.amount.atoms)?;
            }
        }
        for (currency, amount) in incremental_fees {
            let cap = c.maximum_fees.get(&currency).ok_or_else(|| {
                error(
                    "KRAKEN_FEE_BOUNDS_REQUIRED",
                    "Native incremental fee currency cap missing.",
                )
            })?;
            if amount > gofin_domain::Quantity::from_decimal(cap, 36)? {
                return Err(error(
                    "KRAKEN_BATCH_FEE_ALLOCATION_REQUIRED",
                    "The sum of additional original-owner and new-order fee ceilings exceeds this exact batch currency cap. Select a bounded per-owner fee allocation before dispatch.",
                ));
            }
        }
        current.binding["compoundChildren"] = json!(children);
        current
            .intent
            .expected
            .push(EffectConstraint::CompoundSubmissionMutations { children });
        let mut native_targets = BTreeSet::new();
        for row in wire.values() {
            if let Some(id) = row
                .get("orderId")
                .or_else(|| row.get("order_id"))
                .and_then(Value::as_str)
                && !native_targets.insert(id.to_owned())
            {
                return Err(error(
                    "KRAKEN_BATCH_TARGET_REPEATED",
                    "The same native order cannot be edited or canceled twice in one batch.",
                ));
            }
        }
        native["batchOrder"] = json!(
            rows.iter()
                .map(|r| wire
                    .get(r["order_tag"].as_str().unwrap())
                    .cloned()
                    .ok_or_else(|| error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Native mutation wire mapping missing."
                    )))
                .collect::<Result<Vec<_>>>()?
        );
        for key in [
            "derivativeEdit",
            "amendment",
            "amendmentPreviousScope",
            "derivativeBatchEditTag",
            "derivativeEditParameters",
            "derivativeControl",
            "targets",
        ] {
            current.binding.as_object_mut().unwrap().remove(key);
        }
        current.binding["requestId"] = json!(c.request_id);
        current.binding["endpoint"] = json!(E::KrakenfuturesPrivatePostBatchorder.method());
        current.binding["parameters"] = native;
        current.binding["derivativeBatch"] = json!(bindings);
        current.binding["compoundContext"] = json!(linked);
        current.binding["controls"] = json!(c);
        current.operation = E::KrakenfuturesPrivatePostBatchorder.operation().into();
        current.input = input.clone();
        current.summary = format!(
            "Execute {} exact native commands while preserving each original financial owner and cumulative amendment authority.",
            rows.len()
        );
        // Mutations carry delta principal only; native contract exposure stays in each full revision.
        self.compound_margin_admission(&current).await?;
        gofin_domain::value_bounds::bind(&mut current);
        Ok(current)
    }
}

impl KrakenProfile {
    pub(crate) async fn compound_margin_admission(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        use gofin_domain::compound_mutations::ChildMutation;
        let account =
            self.account(prepared.binding["account"].as_str().ok_or_else(|| {
                error("KRAKEN_PREVIEW_INVALID", "Native batch account missing.")
            })?)?;
        let balances = self
            .json(E::KrakenfuturesPrivateGetAccounts, Some(account), json!({}))
            .await?;
        let mut wallets = BTreeMap::<
            String,
            (
                gofin_domain::ResourceId,
                gofin_domain::AssetId,
                gofin_domain::Quantity,
            ),
        >::new();
        for binding in prepared.binding["derivativeBatch"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if !binding["derivative"].is_object() {
                continue;
            }
            let wallet = binding["derivative"]["wallet"].as_str().unwrap();
            let asset: gofin_domain::AssetId = serde_json::from_value(
                binding["derivative"]["collateral"].clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Exact native collateral identity missing.",
                )
            })?;
            let resource = prepared
                .intent
                .debits
                .iter()
                .find(|d| {
                    d.amount.asset == asset && d.resource.0.ends_with(&format!(":wallet:{wallet}"))
                })
                .map(|d| d.resource.clone())
                .ok_or_else(|| {
                    error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Native wallet debit scope missing.",
                    )
                })?;
            let margin: gofin_domain::Quantity = serde_json::from_value(
                binding["derivative"]["initialMargin"].clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Native batch initial margin missing.",
                )
            })?;
            let row = wallets.entry(wallet.into()).or_insert((
                resource,
                asset,
                gofin_domain::Quantity::zero(),
            ));
            row.2 = row.2.checked_add(&margin)?;
        }
        for child in prepared
            .intent
            .expected
            .iter()
            .filter_map(|e| match e {
                EffectConstraint::CompoundSubmissionMutations { children } => Some(children),
                _ => None,
            })
            .flatten()
        {
            let ChildMutation::Amend { revision } = &child.mutation else {
                continue;
            };
            let mutation = prepared.binding["derivativeBatch"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["compoundChildTag"] == child.child_tag)
                .unwrap();
            let member_index = mutation["originalBatchMemberIndex"]
                .as_u64()
                .map(|n| n as usize);
            let d = if let Some(index) = member_index {
                &revision.binding["derivativeBatch"][index]["derivative"]
            } else {
                &revision.binding["derivative"]
            };
            let wallet = d["wallet"].as_str().ok_or_else(|| {
                error("KRAKEN_PREVIEW_INVALID", "Revised wallet identity missing.")
            })?;
            let binding = prepared.binding["derivativeBatch"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["compoundChildTag"] == child.child_tag)
                .unwrap();
            let previous: gofin_domain::Quantity = serde_json::from_value(
                if let Some(index)=member_index{binding["amendmentPreviousScope"]["binding"]["derivativeBatch"][index]["derivative"]["initialMargin"].clone()}else{binding["amendmentPreviousScope"]["binding"]["derivative"]["initialMargin"].clone()},
            )
            .map_err(|_| error("KRAKEN_PREVIEW_INVALID", "Original margin bound missing."))?;
            let current: gofin_domain::Quantity =
                serde_json::from_value(d["initialMargin"].clone()).map_err(|_| {
                    error("KRAKEN_PREVIEW_INVALID", "Revised margin bound missing.")
                })?;
            let amount = revision
                .intent
                .debits
                .iter()
                .find(|a| a.resource.0.ends_with(&format!(":wallet:{wallet}")))
                .ok_or_else(|| {
                    error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Exact revised native wallet debit scope missing.",
                    )
                })?;
            let row = wallets.entry(wallet.into()).or_insert((
                amount.resource.clone(),
                amount.amount.asset.clone(),
                gofin_domain::Quantity::zero(),
            ));
            if current > previous {
                row.2 = row.2.checked_add(&current.checked_sub(&previous)?)?;
            }
        }
        for (wallet, (resource, asset, margin)) in wallets {
            let debit = prepared
                .intent
                .debits
                .iter()
                .filter(|d| d.resource == resource && d.amount.asset == asset)
                .try_fold(gofin_domain::Quantity::zero(), |n, d| {
                    n.checked_add(&d.amount.atoms)
                })?;
            let fees = prepared
                .intent
                .maximum_costs
                .iter()
                .filter(|c| {
                    c.location
                        == (gofin_domain::AccountingLocation::Cash {
                            resource: resource.clone(),
                        })
                        && c.amount.asset == asset
                        && c.treatment == gofin_domain::CostTreatment::AdditionalDebit
                })
                .try_fold(gofin_domain::Quantity::zero(), |n, c| {
                    n.checked_add(&c.amount.atoms)
                })?;
            let native = &balances["accounts"][&wallet];
            let available = crate::observation::atoms(if wallet == "flex" {
                &native["availableMargin"]
            } else {
                &native["auxiliary"]["af"]
            })?;
            if available < margin.checked_add(&debit)?.checked_add(&fees)? {
                return Err(error(
                    "INSUFFICIENT_MARGIN",
                    "A native wallet cannot cover combined new-order and amendment margin increases plus exact incremental loss and fee reserves.",
                ));
            }
        }
        Ok(())
    }
}
