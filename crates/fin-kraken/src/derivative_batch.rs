//! A batch remains one approved submission with cumulative per-contract proofs.
use crate::{
    KrakenEndpoint as E,
    profile::{KrakenProfile, error},
    trading::{controls, proof, unresolved},
};
use chrono::{Duration, Utc};
use fin_domain::{
    EffectConstraint, FinancialEffect, FinancialIntent, OperationRecord, PreparedOperation,
    Quantity, Reconciliation, Result, SignedQuantity, Status, Submission,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
impl KrakenProfile {
    pub(crate) async fn derivative_batch_wallet(
        &self,
        account: &crate::KrakenAccount,
        symbol: &str,
    ) -> Result<(String, String)> {
        let instruments = self
            .json(E::KrakenfuturesPublicGetInstruments, None, json!({}))
            .await?;
        let instrument = instruments["instruments"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["symbol"] == symbol))
            .ok_or_else(|| {
                error(
                    "KRAKEN_INSTRUMENT_UNKNOWN",
                    "Exact batch contract unavailable.",
                )
            })?;
        if matches!(
            instrument["type"].as_str(),
            Some("futures_vanilla" | "flexible_futures")
        ) {
            return Ok(("flex".into(), "USD".into()));
        }
        if instrument["type"] != "futures_inverse" {
            return Err(error(
                "KRAKEN_CONTRACT_SEMANTICS_PENDING",
                "Contract settlement denomination is unreviewed.",
            ));
        }
        let wallets = self
            .json(E::KrakenfuturesPrivateGetAccounts, Some(account), json!({}))
            .await?;
        let prefix = symbol
            .split('_')
            .take(2)
            .collect::<Vec<_>>()
            .join("_")
            .to_ascii_lowercase();
        let matches = wallets["accounts"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_COLLATERAL_UNKNOWN",
                    "Native wallet inventory missing.",
                )
            })?
            .iter()
            .filter(|(key, record)| **key == prefix || record["balances"].get(symbol).is_some())
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(error(
                "KRAKEN_COLLATERAL_UNKNOWN",
                "Inverse contract settlement wallet is missing or ambiguous.",
            ));
        }
        let (wallet, details) = matches[0];
        let currency = details["currency"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                error(
                    "KRAKEN_COLLATERAL_UNKNOWN",
                    "Inverse settlement currency unavailable.",
                )
            })?;
        Ok((wallet.clone(), currency.into()))
    }

    pub(crate) async fn prepare_derivative_batch(
        &self,
        input: &Value,
        mut parameters: Value,
    ) -> Result<PreparedOperation> {
        let controls = controls(input)?;
        let rows = parameters["batchOrder"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Derivative batch orders missing."))?;
        if rows.is_empty() || rows.len() > 50 {
            return Err(error(
                "INVALID_INPUT",
                "Derivative batches require 1–50 exact order commands.",
            ));
        }
        if rows.iter().any(|r| r["order"] == "edit") {
            return Err(error(
                "KRAKEN_MIXED_BATCH_SCOPE_PENDING",
                "Each edited batch element requires a protected original owner and amendment scope; independent multi-owner edit contexts are not yet available.",
            ));
        }
        let mut prepared = vec![];
        let mut native = vec![];
        let mut symbols = BTreeSet::new();
        let mut clients = BTreeSet::new();
        let mut tags = BTreeSet::new();
        for (index, row) in rows.iter().enumerate() {
            if !tags.insert(row["order_tag"].clone().as_str().unwrap_or("").to_owned()) {
                return Err(error(
                    "INVALID_INPUT",
                    "Every derivative batch command requires a distinct order_tag.",
                ));
            }
            if row["order"] == "cancel"
                && row["order_id"].as_str().is_some()
                && row["cliOrdId"].as_str().is_some()
            {
                return Err(error(
                    "INVALID_INPUT",
                    "Select one exact native batch cancellation order_id or cliOrdId.",
                ));
            }
            let mut item = row.clone();
            item.as_object_mut().unwrap().remove("order");
            item.as_object_mut().unwrap().remove("order_tag");
            let mut child = item.clone();
            child["account"] = input["account"].clone();
            let c = serde_json::to_value(&controls)
                .map_err(|_| error("INVALID_INPUT", "Execution controls invalid."))?;
            for (key, value) in c.as_object().unwrap() {
                if !value.is_null() {
                    child[key] = value.clone();
                }
            }
            child["requestId"] = json!(format!("{}:{index}", controls.request_id));
            let send = row["order"] == "send";
            if !matches!(row["order"].as_str(), Some("send" | "cancel")) {
                return Err(error(
                    "INVALID_INPUT",
                    "Batch commands must select a native send, edit or cancel variant.",
                ));
            }
            let mut expected_wallet = None;
            if send {
                let account = self.account(
                    input["account"]
                        .as_str()
                        .ok_or_else(|| error("INVALID_INPUT", "Exact batch account required."))?,
                )?;
                let (wallet, currency) = self
                    .derivative_batch_wallet(
                        account,
                        child["symbol"].as_str().ok_or_else(|| {
                            error("INVALID_INPUT", "Exact contract symbol required.")
                        })?,
                    )
                    .await?;
                if !controls.settlement_limits.is_empty() {
                    let limits = controls.settlement_limits.get(&wallet).ok_or_else(|| {
                        error(
                            "KRAKEN_BATCH_SETTLEMENT_LIMITS_REQUIRED",
                            "Provide exact settlementLimits for every native wallet in this batch.",
                        )
                    })?;
                    for (key, value) in serde_json::to_value(limits)
                        .map_err(|_| error("INVALID_INPUT", "Settlement limits invalid."))?
                        .as_object()
                        .unwrap()
                    {
                        child[key] = value.clone();
                    }
                }
                child.as_object_mut().unwrap().remove("settlementLimits");
                let cap = controls.maximum_fees.get(&currency).ok_or_else(|| {
                    error(
                        "KRAKEN_FEE_BOUND_REQUIRED",
                        "Provide maximumFees for each exact native settlement currency.",
                    )
                })?;
                child["maximumFees"] = json!({currency.clone():cap});
                expected_wallet = Some((wallet, currency));
            }
            if send {
                child.as_object_mut().unwrap().remove("operationId");
                child.as_object_mut().unwrap().remove("originalSubmission");
                if child["processBefore"].is_null()
                    && let Some(v) = parameters.get("processBefore")
                {
                    child["processBefore"] = v.clone();
                }
                if let Some(v) = parameters.get("broker") {
                    child["broker"] = v.clone();
                }
            }
            child.as_object_mut().unwrap().remove("settlementLimits");
            if !send {
                crate::contracts::remove_monetary_controls(&mut child);
            }
            let current = Box::pin(self.prepare_derivatives(
                if send {
                    E::KrakenfuturesPrivatePostSendorder
                } else {
                    E::KrakenfuturesPrivatePostCancelorder
                },
                &child,
            ))
            .await?;
            if send {
                symbols.insert(
                    current.binding["derivative"]["symbol"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
                if !clients.insert(
                    current.binding["derivative"]["clientId"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                ) {
                    return Err(error(
                        "INVALID_INPUT",
                        "Every newly submitted derivative batch order needs a distinct exact client identity.",
                    ));
                }
            }
            if let Some((wallet, currency)) = &expected_wallet
                && (current.binding["derivative"]["wallet"] != *wallet
                    || current.binding["derivative"]["collateral"]["reference"] != *currency)
            {
                return Err(error(
                    "KRAKEN_BATCH_BASELINE_CHANGED",
                    "Native batch settlement wallet changed during preparation.",
                ));
            }
            let mut wire = if send {
                current.binding["parameters"].clone()
            } else {
                let targets = current.binding["targets"].as_array().ok_or_else(|| {
                    error(
                        "KRAKEN_ORDER_PROOF_PENDING",
                        "Exact cancellation target unavailable.",
                    )
                })?;
                if targets.len() != 1 {
                    return Err(error(
                        "KRAKEN_CANCELLATION_TARGET_INVALID",
                        "One batch cancellation command must freeze one native order.",
                    ));
                }
                json!({"order_id":targets[0]["orderId"]})
            };
            if send {
                for key in ["processBefore", "broker", "selfTradePrevention"] {
                    wire.as_object_mut().unwrap().remove(key);
                }
            }
            wire["order"] = row["order"].clone();
            wire["order_tag"] = row["order_tag"].clone();
            native.push(wire);
            prepared.push(current);
        }
        let primary = prepared
            .iter()
            .position(|p| p.binding["derivative"].is_object())
            .unwrap_or(0);
        let mut current = prepared[primary].clone();
        let senders = prepared
            .iter()
            .filter(|p| p.binding["derivative"].is_object())
            .collect::<Vec<_>>();
        if !senders.is_empty() {
            let mut groups: BTreeMap<String, Vec<&PreparedOperation>> = BTreeMap::new();
            for sender in &senders {
                groups
                    .entry(
                        sender.binding["derivative"]["wallet"]
                            .as_str()
                            .unwrap()
                            .into(),
                    )
                    .or_default()
                    .push(sender);
            }
            if groups.len() > 1 && controls.settlement_limits.is_empty() {
                return Err(error(
                    "KRAKEN_BATCH_SETTLEMENT_LIMITS_REQUIRED",
                    "Multiple native wallets need exact wallet-specific settlementLimits; scalar limits cannot silently change currency.",
                ));
            }
            if controls
                .settlement_limits
                .keys()
                .any(|wallet| !groups.contains_key(wallet))
            {
                return Err(error(
                    "KRAKEN_BATCH_SETTLEMENT_LIMITS_UNUSED",
                    "Settlement limits must identify only native wallets used by this batch.",
                ));
            }
            let mut financial = FinancialIntent::default();
            let mut movement = Quantity::zero();
            let mut fees = Quantity::zero();
            let mut prices = BTreeMap::new();
            let mut required_by_asset: BTreeMap<String, Quantity> = BTreeMap::new();
            let balances = self
                .json(
                    E::KrakenfuturesPrivateGetAccounts,
                    Some(self.account(current.binding["account"].as_str().unwrap())?),
                    json!({}),
                )
                .await?;
            for (wallet, children) in groups {
                let first = children[0];
                let collateral = &first.binding["derivative"]["collateral"];
                let code = collateral["reference"].as_str().unwrap();
                let cap = Quantity::from_decimal(&controls.maximum_fees[code], 36)?;
                let mut initial_margin = Quantity::zero();
                let mut required_fee = Quantity::zero();
                for child in &children {
                    if child.binding["derivative"]["collateral"] != *collateral {
                        return Err(error(
                            "KRAKEN_BATCH_BASELINE_CHANGED",
                            "Native wallet settlement currency changed during batch preparation.",
                        ));
                    }
                    initial_margin = initial_margin.checked_add(
                        &serde_json::from_value::<Quantity>(
                            child.binding["derivative"]["initialMargin"].clone(),
                        )
                        .map_err(|_| {
                            error(
                                "KRAKEN_INITIAL_MARGIN_UNKNOWN",
                                "Independent native margin missing.",
                            )
                        })?,
                    )?;
                    required_fee = required_fee.checked_add(
                        &serde_json::from_value::<Quantity>(
                            child.binding["derivative"]["minimumRequiredFeeAtoms"].clone(),
                        )
                        .map_err(|_| {
                            error(
                                "KRAKEN_FEE_PROOF_PENDING",
                                "Independent native fee bound missing.",
                            )
                        })?,
                    )?;
                    movement = movement
                        .checked_add(&child.valuation.as_ref().unwrap().movement_usd_micros)?;
                    for price in &child.valuation.as_ref().unwrap().prices {
                        prices.insert(price.asset.clone(), price.clone());
                    }
                }
                let required = required_by_asset.entry(code.into()).or_default();
                *required = required.checked_add(&required_fee)?;
                let record = &balances["accounts"][&wallet];
                let available = if wallet == "flex" {
                    crate::observation::atoms(&record["availableMargin"])?
                } else {
                    crate::observation::atoms(&record["auxiliary"]["af"])?
                };
                let loss = first
                    .intent
                    .debits
                    .iter()
                    .try_fold(Quantity::zero(), |n, d| n.checked_add(&d.amount.atoms))?;
                if available < initial_margin.checked_add(&loss)?.checked_add(&cap)? {
                    return Err(error(
                        "INSUFFICIENT_MARGIN",
                        "A native wallet cannot cover its full batch initial margin and cumulative loss/funding/fee reserves.",
                    ));
                }
                financial.locks.extend(first.intent.locks.clone());
                financial.debits.extend(first.intent.debits.clone());
                financial
                    .maximum_costs
                    .extend(first.intent.maximum_costs.clone());
                financial.expected.extend(
                    first
                        .intent
                        .expected
                        .iter()
                        .filter(|e| matches!(e, EffectConstraint::Settlement { .. }))
                        .cloned(),
                );
                fees =
                    fees.checked_add(&first.valuation.as_ref().unwrap().maximum_fee_usd_micros)?;
            }
            for (code, required) in required_by_asset {
                if required > Quantity::from_decimal(&controls.maximum_fees[&code], 36)? {
                    return Err(error(
                        "KRAKEN_FEE_BOUND_EXCEEDED",
                        "Cumulative batch commission admission exceeds the exact global currency fee ceiling.",
                    ));
                }
            }
            financial.locks.sort();
            financial.locks.dedup();
            current.intent = financial;
            current.valuation.as_mut().unwrap().movement_usd_micros = movement;
            current.valuation.as_mut().unwrap().maximum_fee_usd_micros = fees;
            current.valuation.as_mut().unwrap().prices = prices.into_values().collect();
        }
        let mut cancellation_ids = BTreeSet::new();
        for (index, child) in prepared.iter().enumerate() {
            if let Some(targets) = child.binding["targets"].as_array() {
                for target in targets {
                    if !cancellation_ids.insert(target["orderId"].as_str().unwrap().to_owned()) {
                        return Err(error(
                            "INVALID_INPUT",
                            "A batch cannot cancel the same frozen native order twice.",
                        ));
                    }
                }
            }
            if index != primary || !senders.is_empty() {
                current.intent.expected.extend(
                    child
                        .intent
                        .expected
                        .iter()
                        .filter(|e| {
                            matches!(
                                e,
                                EffectConstraint::DerivativePosition { .. }
                                    | EffectConstraint::Authority { .. }
                                    | EffectConstraint::SubmissionCancellation { .. }
                            )
                        })
                        .cloned(),
                );
            }
        }
        let baselines = crate::derivative_groups::collate(&prepared, &mut current.intent)?;
        parameters["batchOrder"] = json!(native);
        current.operation = E::KrakenfuturesPrivatePostBatchorder.operation().into();
        current.input = input.clone();
        current.binding = json!({"endpoint":E::KrakenfuturesPrivatePostBatchorder.method(),"product":self.product,"account":current.binding["account"],"physicalIdentity":current.binding["physicalIdentity"],"parameters":parameters,"requestId":controls.request_id,"controls":controls,"derivativeBatch":prepared.iter().map(|p|p.binding.clone()).collect::<Vec<_>>()});
        current.binding["costBounds"] = json!(current.intent.maximum_costs);
        current.binding[fin_domain::BASELINE_ANCHORS_KEY] = json!(baselines);
        current.summary = format!(
            "Execute {} exact derivative send/cancel commands with cumulative collateral, settlement and commission limits.",
            prepared.len()
        );
        current.observed_at = fin_domain::now();
        current.expires_at = (Utc::now() + Duration::seconds(90)).to_rfc3339();
        fin_domain::value_bounds::bind(&mut current);
        Ok(current)
    }
    pub(crate) async fn reconcile_derivative_batch(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        if binding["unifiedNativePartitions"].is_array() {
            return Box::pin(self.reconcile_derivative_unified(record, submission, binding)).await;
        }
        if binding["groupedAmendments"].is_object() {
            return Box::pin(self.reconcile_derivative_grouped_edits(record, submission, binding))
                .await;
        }
        let mut effects = vec![];
        let mut events = vec![];
        let mut observations = vec![];
        let mut terminal = true;
        let mut any_fill = false;
        let mut any_rejected_mutation = false;
        let mut mutation_proofs = vec![];
        let mut unknown_children = vec![];
        let mutation_children: Vec<fin_domain::compound_mutations::MutationChild> = binding
            .get("compoundChildren")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Frozen compound native owners invalid.",
                )
            })?
            .unwrap_or_default();
        for child in binding["derivativeBatch"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original batch scopes missing.",
            )
        })? {
            let mutation = child["compoundChildTag"]
                .as_str()
                .map(|tag| {
                    mutation_children
                        .iter()
                        .find(|m| m.child_tag == tag)
                        .ok_or_else(|| {
                            error(
                                "KRAKEN_SUBMISSION_INVALID",
                                "Tagged native proof has no frozen owner.",
                            )
                        })
                })
                .transpose()?;
            if let Some(mutation) = mutation {
                let native = record
                    .dispatch_evidence
                    .as_ref()
                    .filter(|r| r["response"]["result"] == "success")
                    .and_then(|r| r["response"]["batchStatus"].as_array())
                    .map(|rows| {
                        rows.iter()
                            .filter(|r| r["order_tag"] == mutation.child_tag)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if native.len() != 1 {
                    terminal = false;
                    unknown_children.push(
                        json!({"tag":mutation.child_tag,"reason":"exact_tagged_outcome_missing"}),
                    );
                    continue;
                }
                let row = native[0];
                if fin_exchange::kraken::no_fill_futures_rejection(
                    E::KrakenfuturesPrivatePostEditorder,
                    &json!({"result":"success","editStatus":row}),
                ) {
                    any_rejected_mutation = true;
                    mutation_proofs.push(fin_domain::compound_mutations::MutationChildProof {
                        child_tag: mutation.child_tag.clone(),
                        operation_id: mutation.operation_id.clone(),
                        submission: mutation.submission.clone(),
                        outcome: fin_domain::compound_mutations::ChildMutationOutcome::Rejected {
                            code: row["status"].as_str().unwrap().into(),
                        },
                        native_evidence: json!({"taggedNativeOutcome":row}),
                    });
                    continue;
                }
            }
            if mutation.is_none() && child["derivative"].is_object() {
                let tag = binding["parameters"]["batchOrder"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter().find(|row| {
                            row["order"] == "send"
                                && row["cliOrdId"] == child["derivative"]["clientId"]
                        })
                    })
                    .and_then(|row| row["order_tag"].as_str());
                let native = record
                    .dispatch_evidence
                    .as_ref()
                    .filter(|r| r["response"]["result"] == "success")
                    .and_then(|r| r["response"]["batchStatus"].as_array())
                    .map(|rows| {
                        rows.iter()
                            .filter(|row| tag.is_some_and(|tag| row["order_tag"] == tag))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if native.len() == 1
                    && fin_exchange::kraken::no_fill_futures_rejection(
                        E::KrakenfuturesPrivatePostSendorder,
                        &json!({"result":"success","sendStatus":native[0]}),
                    )
                {
                    any_rejected_mutation = true;
                    observations.push(json!({"status":Status::Failed,"clientId":child["derivative"]["clientId"],"taggedNoMutationEvidence":native[0]}));
                    continue;
                }
            }
            let grouped = child["derivative"].is_object()
                && binding["derivativeBatch"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|other| other["derivative"]["symbol"] == child["derivative"]["symbol"])
                    .count()
                    > 1;
            let mut scoped_child = child.clone();
            scoped_child["_finBatchCostScope"] = json!(true);
            let observed = match if grouped {
                self.reconcile_derivative_order(record, submission, &scoped_child, true)
                    .await
            } else {
                Box::pin(self.reconcile_derivatives_binding(record, submission, &scoped_child))
                    .await
            } {
                Ok(observed) => observed,
                Err(error) if mutation.is_none() && !mutation_children.is_empty() => {
                    terminal = false;
                    unknown_children.push(json!({"newClientId":child["derivative"]["clientId"],"financialEvidenceError":{"code":error.code,"message":error.message}}));
                    continue;
                }
                Err(error) => return Err(error),
            };
            terminal &= observed.status.terminal();
            any_fill |= observed.status == Status::Completed;
            let Some(bundle) = observed.verified else {
                if !mutation_children.is_empty() {
                    terminal = false;
                    unknown_children.push(
                        json!({"tag":child["compoundChildTag"],"evidence":observed.evidence}),
                    );
                    continue;
                }
                return Ok(unresolved(
                    Status::Pending,
                    "One derivative batch element lacks full cumulative evidence; no commands are replayed.",
                    observed.evidence,
                ));
            };
            if let Some(mutation) = mutation {
                let accepted = bundle.effects.iter().find_map(|e| match e {
                    FinancialEffect::SubmissionAmendment {
                        operation_id,
                        submission,
                        successor,
                    } if operation_id == &mutation.operation_id
                        && submission == &mutation.submission =>
                    {
                        Some(successor.clone())
                    }
                    FinancialEffect::SubmissionCancellation {
                        operation_id,
                        submission,
                    } if operation_id == &mutation.operation_id
                        && submission == &mutation.submission =>
                    {
                        Some(None)
                    }
                    _ => None,
                });
                if let Some(successor) = accepted {
                    mutation_proofs.push(fin_domain::compound_mutations::MutationChildProof {
                        child_tag: mutation.child_tag.clone(),
                        operation_id: mutation.operation_id.clone(),
                        submission: mutation.submission.clone(),
                        outcome: fin_domain::compound_mutations::ChildMutationOutcome::Accepted {
                            successor,
                        },
                        native_evidence: bundle.evidence.clone(),
                    });
                } else {
                    terminal = false;
                    unknown_children
                        .push(json!({"tag":mutation.child_tag,"evidence":bundle.evidence}));
                }
            } else {
                effects.extend(bundle.effects);
            }
            if let Some(rows) = bundle.evidence["proofEvents"].as_array() {
                events.extend(rows.iter().cloned());
            }
            observations.push(json!({"status":observed.status,"evidence":bundle.evidence}));
        }
        if !mutation_children.is_empty() {
            terminal &= mutation_proofs.len() == mutation_children.len();
            if mutation_proofs.is_empty() && events.is_empty() && effects.is_empty() {
                return Ok(unresolved(
                    Status::Pending,
                    "Native compound children remain unresolved; no commands are replayed.",
                    json!({"unknownChildren":unknown_children}),
                ));
            }
            effects.push(FinancialEffect::CompoundSubmissionMutations {
                children: mutation_proofs,
            });
        }
        effects = crate::derivative_groups::verify_and_aggregate(binding, &events, effects)?;
        // Commission and settlement share one cumulative wallet cap even when
        // different contracts execute partially or fail independently.
        let mut costs: BTreeMap<String, FinancialEffect> = BTreeMap::new();
        let mut settlements: BTreeMap<String, FinancialEffect> = BTreeMap::new();
        let mut other = vec![];
        for effect in effects {
            match effect {
                FinancialEffect::Cost {
                    ref location,
                    ref amount,
                    ref treatment,
                    ref category,
                } => {
                    let key = fin_domain::binding_hash(&json!([
                        location,
                        amount.asset,
                        treatment,
                        category
                    ]))?;
                    if let Some(FinancialEffect::Cost { amount: old, .. }) = costs.get_mut(&key) {
                        old.atoms = old.atoms.checked_add(&amount.atoms)?;
                    } else {
                        costs.insert(key, effect);
                    }
                }
                FinancialEffect::Settlement {
                    ref resource,
                    ref asset,
                    ref category,
                    ref delta,
                } => {
                    let key = fin_domain::binding_hash(&json!([resource, asset, category]))?;
                    if let Some(FinancialEffect::Settlement { delta: old, .. }) =
                        settlements.get_mut(&key)
                    {
                        *old = SignedQuantity::new((old.integer() + delta.integer()).to_string())?;
                    } else {
                        settlements.insert(key, effect);
                    }
                }
                other_effect => other.push(other_effect),
            }
        }
        other.extend(costs.into_values());
        other.extend(settlements.into_values());
        events.sort_by(|a, b| a["eventId"].as_str().cmp(&b["eventId"].as_str()));
        let bounds = binding
            .get("_finAccountingCostBounds")
            .or_else(|| binding.get("costBounds"))
            .map(|v| {
                serde_json::from_value::<Vec<fin_domain::CostBound>>(v.clone()).map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Original batch typed cost ceilings are malformed.",
                    )
                })
            })
            .transpose()?
            .unwrap_or_else(|| {
                record
                    .preview
                    .as_ref()
                    .map(|p| p.intent.maximum_costs.clone())
                    .unwrap_or_default()
            });
        if binding["mutationNewWriteScope"].is_object() {
            let scope: fin_domain::amendments::AmendmentScope =
                serde_json::from_value(binding["mutationNewWriteScope"].clone()).map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Protected new-write cost scope is malformed.",
                    )
                })?;
            if crate::unresolved_consumption::costs_exceed_bounds(
                &other,
                &scope.intent.maximum_costs,
            )? {
                let markers = other
                    .iter()
                    .filter(|effect| crate::unresolved_consumption::is_marker(effect))
                    .cloned()
                    .collect();
                return crate::unresolved_consumption::new_write_overrun_with_progress(
                    record,
                    submission,
                    &other,
                    markers,
                    json!({"code":"KRAKEN_BATCH_NEW_WRITE_COST_BOUND_EXCEEDED","batch":observations,"proofEvents":events,"unknownChildren":unknown_children,"newFinancialEffectsUnsettled":true}),
                );
            }
        }
        if crate::unresolved_consumption::costs_exceed_bounds(&other, &bounds)? {
            return crate::unresolved_consumption::cost_overrun(
                submission,
                &other,
                json!({"code":"KRAKEN_BATCH_COST_BOUND_EXCEEDED","batch":observations,"proofEvents":events,"unknownChildren":unknown_children}),
            );
        }
        proof(
            submission,
            if !terminal {
                Status::Pending
            } else if any_fill && !any_rejected_mutation {
                Status::Completed
            } else {
                Status::Failed
            },
            other,
            json!({"batch":observations,"proofEvents":events,"unknownChildren":unknown_children,"partialExecution":any_rejected_mutation}),
        )
    }
}
