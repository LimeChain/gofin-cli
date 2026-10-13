//! An edited derivative order keeps one immutable fill owner and an approved scope revision.
use crate::{
    KrakenEndpoint as E, contracts,
    observation::atoms,
    profile::{KrakenProfile, error},
    trading::{controls, proof, unresolved},
};
use fin_domain::amendments::AmendmentScope;
use fin_domain::{
    EffectConstraint, FinancialEffect, FinancialIntent, OperationRecord, PreparedOperation,
    Quantity, Reconciliation, Result, Status, Submission,
};
use serde_json::{Value, json};
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_edit_batch(
        &self,
        input: &Value,
        previous: Option<&AmendmentScope>,
    ) -> Result<PreparedOperation> {
        let (_, mut wire) = contracts::decode(E::KrakenfuturesPrivatePostBatchorder, input)?;
        let rows = wire["batchOrder"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Native derivative batch commands missing."))?;
        if rows.len() != 1 || rows[0]["order"] != "edit" {
            return Err(error(
                "KRAKEN_MIXED_BATCH_SCOPE_PENDING",
                "Multiple edited owners or mixed edit/send batches require separately protected cumulative scopes; nothing was dispatched.",
            ));
        }
        if rows[0]["order_id"].as_str().is_some() && rows[0]["cliOrdId"].as_str().is_some() {
            return Err(error(
                "INVALID_INPUT",
                "Select one exact native batch edit order_id or cliOrdId; dual selectors are not silently resolved.",
            ));
        }
        let tag = rows[0]["order_tag"].clone();
        let mut child = rows[0].clone();
        child.as_object_mut().unwrap().remove("order");
        child.as_object_mut().unwrap().remove("order_tag");
        if let Some(id) = child.as_object_mut().unwrap().remove("order_id") {
            child["orderId"] = id;
        }
        child["account"] = input["account"].clone();
        let controls = serde_json::to_value(controls(input)?)
            .map_err(|_| error("INVALID_INPUT", "Derivative batch controls invalid."))?;
        for (key, value) in controls.as_object().unwrap() {
            if !value.is_null() {
                child[key] = value.clone();
            }
        }
        if let Some(limits) = child.get("settlementLimits").and_then(Value::as_object) {
            let member = if let Some(previous) =
                previous.filter(|p| p.binding["derivativeBatch"].is_array())
            {
                self.derivative_original_member(
                    &previous.binding,
                    &child,
                    self.account(input["account"].as_str().unwrap())?,
                )
                .await?
                .1
            } else {
                previous.map(|p| p.binding.clone()).unwrap_or_default()
            };
            let wallet = member["derivative"]["wallet"].as_str().ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_CONTEXT_REQUIRED",
                    "Protected original wallet scope is required.",
                )
            })?;
            if limits.len() != 1 || !limits.contains_key(wallet) {
                return Err(error(
                    "KRAKEN_BATCH_SETTLEMENT_LIMITS_UNUSED",
                    "Edit-only batch limits must identify exactly the original native wallet.",
                ));
            }
            let limits = limits[wallet].clone();
            for (key, value) in limits.as_object().unwrap() {
                child[key] = value.clone();
            }
        }
        child.as_object_mut().unwrap().remove("settlementLimits");
        if child["processBefore"].is_null()
            && let Some(value) = wire.get("processBefore")
        {
            child["processBefore"] = value.clone();
        }
        let mut prepared = self.prepare_derivative_edit(&child, previous).await?;
        let native = prepared.binding["parameters"].clone();
        let mut row = native.clone();
        if let Some(id) = row.as_object_mut().unwrap().remove("orderId") {
            row["order_id"] = id;
        }
        row.as_object_mut().unwrap().remove("processBefore");
        row["order"] = json!("edit");
        row["order_tag"] = tag.clone();
        wire["batchOrder"] = json!([row]);
        prepared.operation = E::KrakenfuturesPrivatePostBatchorder.operation().into();
        prepared.input = input.clone();
        prepared.binding["endpoint"] = json!(E::KrakenfuturesPrivatePostBatchorder.method());
        prepared.binding["parameters"] = wire;
        prepared.binding["derivativeEditParameters"] = native;
        prepared.binding["derivativeBatchEditTag"] = tag;
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(crate) async fn prepare_derivative_edit(
        &self,
        input: &Value,
        previous: Option<&AmendmentScope>,
    ) -> Result<PreparedOperation> {
        let control = controls(input)?;
        if let Some(previous) = previous.filter(|s| s.binding["derivativeBatch"].is_array()) {
            return Box::pin(self.prepare_derivative_batch_member_edit(input, previous)).await;
        }
        let original = control.original_submission.as_ref().ok_or_else(|| {
            error(
                "INVALID_INPUT",
                "Derivative edit requires exact originalSubmission.",
            )
        })?;
        let id = control
            .operation_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                error(
                    "INVALID_INPUT",
                    "Derivative edit requires original operationId.",
                )
            })?;
        let b = previous
            .map(|s| &s.binding)
            .unwrap_or(&original.data["binding"]);
        let d = &b["derivative"];
        let (selection, mut native) =
            contracts::decode(E::KrakenfuturesPrivatePostEditorder, input)?;
        let account = self.account(
            selection
                .account
                .as_deref()
                .ok_or_else(|| error("INVALID_INPUT", "Exact derivative account missing."))?,
        )?;
        if original.kind != "krakenfutures-rest-v1"
            || b["account"] != account.name
            || b["physicalIdentity"] != account.physical_identity
            || !d.is_object()
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original submission must identify this exact derivative account and one order.",
            ));
        }
        self.derivative_permissions(account).await?;
        let open = self
            .json(
                E::KrakenfuturesPrivateGetOpenorders,
                Some(account),
                json!({}),
            )
            .await?;
        let matches = open["openOrders"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_PROOF_PENDING",
                    "Exact derivative order scope unavailable.",
                )
            })?
            .iter()
            .filter(|o| {
                o["cliOrdId"] == d["clientId"]
                    && o["symbol"] == d["symbol"]
                    && (d["orderId"].is_null() || d["orderId"] == o["order_id"])
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(error(
                "KRAKEN_ORDER_NOT_OPEN",
                "One exact original derivative order must remain open.",
            ));
        }
        let venue = matches[0];
        let venue_id = venue["order_id"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_ORDER_PROOF_PENDING",
                "Derivative order identity missing.",
            )
        })?;
        if native["orderId"].as_str().is_some_and(|v| v != venue_id)
            || native["cliOrdId"]
                .as_str()
                .is_some_and(|v| venue["cliOrdId"] != v)
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Derivative edit selector differs from exact original order.",
            ));
        }
        native["orderId"] = json!(venue_id);
        native.as_object_mut().unwrap().remove("cliOrdId");
        let mut modified = b["parameters"].clone();
        let filled = atoms(&venue["filledSize"])?;
        if let Some(value) = native.get("size") {
            let size = atoms(value)?;
            let total = match native["qtyMode"].as_str() {
                None | Some("RELATIVE") => size.checked_add(&filled)?,
                Some("ABSOLUTE") => size,
                _ => {
                    return Err(error(
                        "INVALID_INPUT",
                        "Derivative qtyMode must be ABSOLUTE or RELATIVE.",
                    ));
                }
            };
            if total < filled {
                return Err(error(
                    "KRAKEN_AMENDMENT_BELOW_PROVED_FILL",
                    "Absolute size cannot be lower than already executed quantity.",
                ));
            }
            modified["size"] = json!(total.to_decimal(36));
        }
        for key in [
            "limitPrice",
            "stopPrice",
            "trailingStopMaxDeviation",
            "trailingStopDeviationUnit",
            "processBefore",
        ] {
            if let Some(v) = native.get(key) {
                modified[key] = v.clone();
            }
        }
        modified["account"] = json!(account.name);
        let controls = serde_json::to_value(&control)
            .map_err(|_| error("INVALID_INPUT", "Derivative controls invalid."))?;
        for (key, value) in controls.as_object().unwrap() {
            if !value.is_null() && key != "operationId" && key != "originalSubmission" {
                modified[key] = value.clone();
            }
        }
        let mut full =
            Box::pin(self.prepare_derivatives(E::KrakenfuturesPrivatePostSendorder, &modified))
                .await?;
        // The current position verifies dispatch feasibility; original baseline
        // remains the origin for cumulative fills from this immutable submission.
        full.binding["derivative"]["positionBefore"] = d["positionBefore"].clone();
        full.binding["derivative"]["orderId"] = json!(venue_id);
        full.binding[fin_domain::BASELINE_ANCHORS_KEY] =
            b[fin_domain::BASELINE_ANCHORS_KEY].clone();
        let previous=previous.ok_or_else(||error("KRAKEN_AMENDMENT_CONTEXT_REQUIRED","Derivative edits require the protected original cumulative scope from Fin's journal."))?;
        let old_size: Quantity = serde_json::from_value(d["size"].clone()).map_err(|_| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Original native contract size missing.",
            )
        })?;
        let new_size: Quantity = serde_json::from_value(full.binding["derivative"]["size"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_PREVIEW_INVALID",
                    "Revised native contract size missing.",
                )
            })?;
        let prior_value = previous.valuation.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Original gross USD valuation missing.",
            )
        })?;
        let current_value = full.valuation.as_mut().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Revised gross USD valuation missing.",
            )
        })?;
        // Fresh contract notional is homogeneous in exact native size. Price the
        // positive quantity increment independently; a falling FX/mark cannot
        // erase historically authorized gross exposure or finance new contracts.
        let positive_exposure_usd = if new_size > old_size {
            current_value
                .movement_usd_micros
                .mul_div_ceil(&new_size.checked_sub(&old_size)?, &new_size)?
        } else {
            Quantity::zero()
        };
        current_value.movement_usd_micros = current_value.movement_usd_micros.clone().max(
            prior_value
                .movement_usd_micros
                .checked_add(&positive_exposure_usd)?,
        );
        current_value.maximum_fee_usd_micros = current_value
            .maximum_fee_usd_micros
            .clone()
            .max(prior_value.maximum_fee_usd_micros.clone());
        fin_domain::value_bounds::bind(&mut full);
        let mut delta = FinancialIntent {
            locks: full.intent.locks.clone(),
            ..Default::default()
        };
        for amount in &full.intent.debits {
            let before = previous
                .intent
                .debits
                .iter()
                .filter(|v| v.resource == amount.resource && v.amount.asset == amount.amount.asset)
                .try_fold(Quantity::zero(), |n, v| n.checked_add(&v.amount.atoms))?;
            if amount.amount.atoms > before {
                let mut a = amount.clone();
                a.amount.atoms = amount.amount.atoms.checked_sub(&before)?;
                delta.debits.push(a);
            }
        }
        for cost in &full.intent.maximum_costs {
            let before = previous
                .intent
                .maximum_costs
                .iter()
                .filter(|v| {
                    v.location == cost.location
                        && v.amount.asset == cost.amount.asset
                        && v.treatment == cost.treatment
                })
                .try_fold(Quantity::zero(), |n, v| n.checked_add(&v.amount.atoms))?;
            if cost.amount.atoms > before {
                let mut c = cost.clone();
                c.amount.atoms = cost.amount.atoms.checked_sub(&before)?;
                delta.maximum_costs.push(c);
            }
        }
        let mut valuation = self
            .financial_valuation(&delta.debits, &delta.maximum_costs)
            .await?;
        let incremental_fee = valuation.maximum_fee_usd_micros.clone();
        let old_fee = &previous.valuation.as_ref().unwrap().maximum_fee_usd_micros;
        let full_value = full.valuation.as_mut().unwrap();
        full_value.maximum_fee_usd_micros = full_value
            .maximum_fee_usd_micros
            .clone()
            .max(old_fee.checked_add(&incremental_fee)?);
        fin_domain::value_bounds::bind(&mut full);
        let scope = AmendmentScope {
            intent: full.intent.clone(),
            binding: full.binding.clone(),
            valuation: full.valuation.clone(),
        };
        delta.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: id.into(),
            submission: original.clone(),
            revision: Box::new(scope),
        });
        // Derivative principal is contract exposure, not a collateral cash debit.
        // Reserve its full gross USD increase independently of incremental cash fees.
        let prior = previous.valuation.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Protected original derivative valuation is missing.",
            )
        })?;
        let revised = full.valuation.as_ref().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_VALUATION_REQUIRED",
                "Revised derivative valuation is missing.",
            )
        })?;
        let total = |value: &fin_domain::Valuation| -> Result<Quantity> {
            value
                .movement_usd_micros
                .checked_add(&value.maximum_fee_usd_micros)?
                .checked_add(&value.additional_allocation_usd_micros)
        };
        let previous_total = total(prior)?;
        let revised_total = total(revised)?;
        valuation.additional_allocation_usd_micros = valuation
            .additional_allocation_usd_micros
            .checked_add(&positive_exposure_usd)?;
        let own_total = total(&valuation)?;
        if revised_total > previous_total {
            let increase = revised_total.checked_sub(&previous_total)?;
            if increase > own_total {
                valuation.additional_allocation_usd_micros = valuation
                    .additional_allocation_usd_micros
                    .checked_add(&increase.checked_sub(&own_total)?)?;
            }
        }
        full.operation = E::KrakenfuturesPrivatePostEditorder.operation().into();
        full.input = input.clone();
        full.intent = delta;
        full.valuation = Some(valuation);
        full.binding = json!({"endpoint":E::KrakenfuturesPrivatePostEditorder.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":native,"requestId":control.request_id,"derivativeEdit":true,"positiveExposureUsdMicros":positive_exposure_usd,"positiveFeeUsdMicros":incremental_fee,"amendmentPreviousScope":previous,"amendment":{"operationId":id,"originalSubmission":original,"orderId":venue_id,"clientId":d["clientId"],"symbol":d["symbol"],"totalSize":modified["size"],"desiredParameters":modified}});
        full.summary = format!(
            "Edit original derivative order {venue_id} on {}. Past fills keep their owner; approve the full revised exposure and only increased loss/fee reserves.",
            account.name
        );
        fin_domain::value_bounds::bind(&mut full);
        Ok(full)
    }
    pub(crate) async fn reconcile_derivative_edit(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        b: &Value,
    ) -> Result<Reconciliation> {
        let account = self.account(b["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Derivative edit account missing.",
            )
        })?)?;
        let m = &b["amendment"];
        let Some(receipt) = record
            .dispatch_evidence
            .as_ref()
            .and_then(|r| r.get("response"))
        else {
            return Ok(unresolved(
                Status::Unknown,
                "Derivative edit acknowledgment was lost; no mutation is replayed.",
                json!({}),
            ));
        };
        let normalized;
        let receipt = if let Some(tag) = b.get("derivativeBatchEditTag") {
            let matches = receipt["batchStatus"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter(|row| row["order_tag"] == *tag)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if matches.len() != 1 {
                return Ok(unresolved(
                    Status::Unknown,
                    "Native batch edit requires one exact tagged outcome; it is never replayed.",
                    receipt.clone(),
                ));
            }
            normalized = json!({"editStatus":matches[0]});
            &normalized
        } else {
            receipt
        };
        if receipt["editStatus"]["status"] != "edited"
            || receipt["editStatus"]["orderId"] != m["orderId"]
        {
            return Ok(unresolved(
                Status::Unknown,
                "Derivative edit response does not acknowledge the exact original order identity.",
                receipt.clone(),
            ));
        }
        let acknowledged = receipt["editStatus"]["orderEvents"]
            .as_array()
            .and_then(|rows| {
                rows.iter().find(|r| {
                    r["type"] == "EDIT"
                        && r["old"]["orderId"] == m["orderId"]
                        && r["new"]["orderId"] == m["orderId"]
                })
            });
        let Some(ack) = acknowledged else {
            return Ok(unresolved(
                Status::Unknown,
                "Derivative edit lacks an exact before/after event; acknowledgment alone does not revise financial authority.",
                receipt.clone(),
            ));
        };
        let new = &ack["new"];
        if new["symbol"] != m["symbol"]
            || new["side"] != m["desiredParameters"]["side"]
            || ack["old"]["side"] != m["desiredParameters"]["side"]
            || atoms(&new["quantity"])? != atoms(&m["totalSize"])?
        {
            return Err(error(
                "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                "Derivative edit event changed exact contract or cumulative size.",
            ));
        }
        for key in ["limitPrice", "stopPrice"] {
            if let Some(v) = b
                .get("derivativeEditParameters")
                .unwrap_or(&b["parameters"])
                .get(key)
                && atoms(v)? != atoms(&new[key])?
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                    "Derivative edit event changed an approved exact price.",
                ));
            }
        }
        let since =
            chrono::DateTime::parse_from_rfc3339(submission.data["submittedAt"].as_str().unwrap())
                .map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Derivative edit submission time invalid.",
                    )
                })?
                .timestamp_millis()
                - 30_000;
        let history=self.derivatives_history(E::KrakenfuturesHistoryGetOrders,json!({"__account":account.name,"since":since,"sort":"asc","count":1000,"tradeable":m["symbol"]})).await?;
        let matched = history
            .iter()
            .filter(|row| {
                let order = &row["event"]["OrderUpdated"]["newOrder"];
                order["uid"] == m["orderId"]
                    && order["clientId"] == m["clientId"]
                    && order["tradeable"] == m["symbol"]
                    && order["direction"].as_str().is_some_and(|side| {
                        side.eq_ignore_ascii_case(
                            m["desiredParameters"]["side"].as_str().unwrap_or(""),
                        )
                    })
                    && atoms(&order["quantity"])
                        .is_ok_and(|q| atoms(&m["totalSize"]).is_ok_and(|expected| q == expected))
            })
            .collect::<Vec<_>>();
        if matched.len() != 1 {
            return Ok(unresolved(
                Status::Pending,
                "Acknowledged derivative size revision is not uniquely visible in durable order history.",
                json!({"acknowledgment":ack}),
            ));
        }
        let original: Submission = serde_json::from_value(m["originalSubmission"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original derivative edit submission invalid.",
                )
            })?;
        proof(
            submission,
            Status::Completed,
            vec![FinancialEffect::SubmissionAmendment {
                operation_id: m["operationId"].as_str().unwrap().into(),
                submission: original,
                successor: None,
            }],
            json!({"acknowledgedEdit":ack,"durableOrderUpdate":matched[0]}),
        )
    }
}
