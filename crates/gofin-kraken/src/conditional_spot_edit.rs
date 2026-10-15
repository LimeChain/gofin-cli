//! Exact issued-child revisions retain the entry's generated-credit accounting owner.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProfile,
    observation::{atoms, result},
    profile::error,
    trading::{SpotOrder, TradingControls},
};
use gofin_domain::{
    Amount, CostTreatment, EffectConstraint, FinancialEffect, FinancialIntent, PreparedOperation,
    Quantity, ResourceAmount, Result, Submission,
    amendments::{AmendmentScope, OriginalSubmissionEvidence},
    conversion_sequences::ConversionSequence,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

pub(crate) async fn conditional_inventory(
    profile: &KrakenProfile,
    account: &KrakenAccount,
    submission: &Submission,
) -> Result<Map<String, Value>> {
    let open = profile
        .json(
            E::KrakenPrivatePostOpenOrders,
            Some(account),
            json!({"trades":true}),
        )
        .await?;
    let mut inventory = result(&open)["open"]
        .as_object()
        .ok_or_else(|| {
            error(
                "KRAKEN_CHILD_EVIDENCE_PENDING",
                "Complete native open order inventory is unavailable.",
            )
        })?
        .clone();
    let submitted =
        chrono::DateTime::parse_from_rfc3339(submission.data["submittedAt"].as_str().unwrap_or(""))
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "The original submission time is unavailable.",
                )
            })?;
    let mut offset = 0usize;
    let mut seen = BTreeSet::new();
    for _ in 0..64 {
        let closed = profile
            .json(
                E::KrakenPrivatePostClosedOrders,
                Some(account),
                json!({"trades":true,"start":submitted.timestamp(),"ofs":offset}),
            )
            .await?;
        let rows = result(&closed)["closed"].as_object().ok_or_else(|| {
            error(
                "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                "Complete native closed order inventory is unavailable.",
            )
        })?;
        if !rows.is_empty() && !seen.insert(rows.keys().cloned().collect::<Vec<_>>()) {
            return Err(error(
                "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                "Native child history repeated an order page.",
            ));
        }
        for (id, row) in rows {
            if inventory.insert(id.clone(), row.clone()).is_some() {
                return Err(error(
                    "KRAKEN_CHILD_ORDER_AMBIGUOUS",
                    "One native order appears in both open and closed history; retry after its transition is resolved.",
                ));
            }
        }
        offset = offset.checked_add(rows.len()).ok_or_else(|| {
            error(
                "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                "Native order pagination overflow.",
            )
        })?;
        if inventory.len() > 16_384 {
            return Err(error(
                "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                "The complete conditional order inventory exceeds the bounded recovery scope.",
            ));
        }
        if rows.len() < 50
            || result(&closed)["count"]
                .as_u64()
                .is_some_and(|count| offset as u64 >= count)
        {
            return Ok(inventory);
        }
    }
    Err(error(
        "KRAKEN_CHILD_HISTORY_INCOMPLETE",
        "Native child history exceeds 64 complete pages; no amendment is dispatched.",
    ))
}
fn product(order: &SpotOrder) -> (&gofin_domain::AssetId, &gofin_domain::AssetId) {
    if order.buy {
        (&order.quote, &order.base)
    } else {
        (&order.base, &order.quote)
    }
}
fn native_child_parameters(template: &Value, order: &SpotOrder, row: &Value) -> Result<Value> {
    let mut parameters = template.clone();
    if !parameters.is_object() {
        return Err(error(
            "KRAKEN_CHILD_EVIDENCE_PENDING",
            "The immutable child template is unavailable.",
        ));
    }
    parameters["pair"] = json!(order.pair);
    parameters["type"] = json!(if order.buy { "buy" } else { "sell" });
    parameters["volume"] = row["vol"].clone();
    parameters["cl_ord_id"] = json!(order.client_id);
    parameters.as_object_mut().unwrap().remove("userref");
    for field in ["price", "price2"] {
        if template.get(field).is_some() {
            parameters[field] = row["descr"][field].clone();
        }
    }
    Ok(parameters)
}
fn child_input(order: &SpotOrder, row: &Value, parameters: &Value) -> Result<Quantity> {
    if !order.buy {
        return atoms(&row["vol"]);
    }
    let price = atoms(
        &parameters[if matches!(parameters["ordertype"].as_str(), Some("limit" | "iceberg")) {
            "price"
        } else {
            "price2"
        }],
    )?;
    let remainder = atoms(&row["vol"])?.checked_sub(&atoms(&row["vol_exec"])?)?;
    atoms(&row["cost"])?.checked_add(
        &remainder.mul_div_ceil(&price, &Quantity::new(format!("1{}", "0".repeat(36)))?)?,
    )
}
impl KrakenProfile {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_issued_spot_child_revision(
        &self,
        endpoint: E,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: &AmendmentScope,
        original_evidence: Option<&OriginalSubmissionEvidence>,
    ) -> Result<PreparedOperation> {
        for condition in previous.binding["conditionalClose"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(parent) = condition["parentIndex"].as_u64().and_then(|i| {
                previous.binding["orders"]
                    .as_array()
                    .and_then(|rows| rows.get(i as usize))
            }) && (parameters["txid"].as_str().is_some_and(|id| {
                parent["orderIds"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|known| known == id))
            }) || parameters["cl_ord_id"] == parent["clientId"])
            {
                return Err(error("KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED", "Kraken does not support atomic amendment or legacy edit of a parent with attached conditional-close terms. Issued native children require their exact original relationship and generated-credit proof.").with_details(json!({"venueUnsupported":true,"source":"https://docs.kraken.com/exchange/guides/general/amends","legacySource":"https://docs.kraken.com/api-reference/trading/edit-order"})));
            }
        }
        let evidence=original_evidence.ok_or_else(||error("KRAKEN_CHILD_ORIGINAL_PROOF_REQUIRED","Recover the original conditional operation first so its protected journal contains the entry's correlated generated-credit proof."))?;
        let original = controls.original_submission.as_ref().ok_or_else(|| {
            error(
                "INVALID_INPUT",
                "An issued-child amendment requires its exact originalSubmission.",
            )
        })?;
        if evidence.submission.as_ref() != Some(original) {
            return Err(error(
                "KRAKEN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "The protected original journal does not identify this exact submission.",
            ));
        }
        let bundle=evidence.verified_effects.as_ref().ok_or_else(||error("KRAKEN_CHILD_ORIGINAL_PROOF_REQUIRED","Recover the original entry's exact trades and ledgers before revising its issued child."))?;
        let mut orders: Vec<SpotOrder> = serde_json::from_value(previous.binding["orders"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Conditional order bindings are unavailable.",
                )
            })?;
        let inventory = conditional_inventory(self, account, original).await?;
        let target=parameters["txid"].as_str().ok_or_else(||error("KRAKEN_CHILD_EXACT_ID_REQUIRED","An issued conditional child requires its exact native txid; a template client ID cannot identify a generated order."))?.to_owned();
        let row = inventory
            .get(&target)
            .filter(|row| row["status"] == "open")
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_NOT_OPEN",
                    "The exact issued child is not open; preserve its original recovery operation.",
                )
            })?;
        let conditions = previous.binding["conditionalClose"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "The immutable conditional-close relationship is unavailable.",
                )
            })?;
        let mut selection = None;
        for condition in conditions {
            let parent_index = condition["parentIndex"].as_u64().unwrap_or(u64::MAX) as usize;
            let child_index = condition["childIndex"].as_u64().unwrap_or(u64::MAX) as usize;
            let Some(parent) = orders.get(parent_index) else {
                continue;
            };
            let native_parents = inventory
                .iter()
                .filter(|(id, _)| parent.order_ids.contains(id))
                .collect::<Vec<_>>();
            if native_parents.len() != 1 {
                continue;
            }
            if row["refid"] == *native_parents[0].0 {
                if selection.is_some() {
                    return Err(error(
                        "KRAKEN_CHILD_ORDER_AMBIGUOUS",
                        "The issued child belongs to more than one original conditional scope.",
                    ));
                }
                selection = Some((
                    condition,
                    parent_index,
                    child_index,
                    native_parents[0].0.clone(),
                    native_parents[0].1,
                ));
            }
        }
        let (condition,parent_index,child_index,parent_id,parent_row)=selection.ok_or_else(||error("KRAKEN_CHILD_IDENTITY_MISMATCH","The selected native order lacks an exact refid relationship to a protected original entry."))?;
        let parent = orders.get(parent_index).ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "The original entry scope is unavailable.",
            )
        })?;
        let child = orders
            .get(child_index)
            .ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "The original generated-close scope is unavailable.",
                )
            })?
            .clone();
        let sequence: ConversionSequence = bundle
            .effects
            .iter()
            .find_map(|effect| match effect {
                FinancialEffect::ConversionSequence { execution }
                    if execution
                        .legs
                        .first()
                        .is_some_and(|leg| leg.id == parent.client_id) =>
                {
                    Some(execution.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_CHILD_ORIGINAL_PROOF_REQUIRED",
                    "The protected original journal lacks this exact entry's ordered cash proof.",
                )
            })?;
        if sequence.resource != account.resource
            || sequence.legs.len() != 2
            || sequence.legs[1].id != child.client_id
        {
            return Err(error(
                "KRAKEN_CHILD_ORIGINAL_PROOF_REQUIRED",
                "The protected generated-credit proof changed account or ordered leg identity.",
            ));
        }
        let root = &sequence.legs[0];
        let proved_volume = if parent.buy {
            &root.received.atoms
        } else {
            &root.spent.atoms
        };
        if atoms(&parent_row["vol_exec"])? != *proved_volume
            || atoms(&parent_row["cost"])?
                != if parent.buy {
                    root.spent.atoms.clone()
                } else {
                    root.received.atoms.clone()
                }
        {
            return Err(error(
                "KRAKEN_CHILD_ENTRY_PROOF_CHANGED",
                "Entry fills changed since the protected original proof. Recover the original operation, then request a fresh child preview.",
            ));
        }
        let fresh_parent = self
            .json(
                E::KrakenPrivatePostQueryOrders,
                Some(account),
                json!({"txid":parent_id,"trades":true}),
            )
            .await?;
        if result(&fresh_parent)[&parent_id]["vol_exec"] != parent_row["vol_exec"]
            || result(&fresh_parent)[&parent_id]["cost"] != parent_row["cost"]
        {
            return Err(error(
                "KRAKEN_CHILD_ENTRY_PROOF_CHANGED",
                "The pinned entry changed while its paginated child inventory was being read. Recover its original fills before a fresh child approval.",
            ));
        }
        for origin in bundle.evidence["conditionalChildOrigins"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|origin| origin["parentOrderId"] == parent_id)
        {
            if origin["childOrderId"]
                .as_str()
                .is_none_or(|id| !inventory.contains_key(id))
            {
                return Err(error(
                    "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                    "An immutable previously proved child is absent from the complete native inventory. Absence cannot free its generated-credit allocation.",
                ));
            }
        }
        let mut issued_volume = Quantity::zero();
        let mut issued_input = Quantity::zero();
        let mut proved_released_volume = Quantity::zero();
        let mut child_ids = vec![];
        let mut selected_native = Value::Null;
        for (id, current) in inventory
            .iter()
            .filter(|(_, row)| row["refid"] == parent_id)
        {
            if (current["descr"]["pair"] != child.pair
                && current["descr"]["pair"] != child.display_pair)
                || current["descr"]["type"] != if child.buy { "buy" } else { "sell" }
                || current["descr"]["ordertype"] != condition["parameters"]["ordertype"]
            {
                return Err(error(
                    "KRAKEN_CHILD_IDENTITY_MISMATCH",
                    "Generated-close market, direction or order type differs from its immutable original template.",
                ));
            }
            let approved = previous.binding["conditionalChildRevisions"][id]["parameters"]
                .as_object()
                .map(|_| &previous.binding["conditionalChildRevisions"][id]["parameters"])
                .unwrap_or(&condition["parameters"]);
            for key in ["price", "price2"] {
                if let Some(value) = approved.get(key)
                    && atoms(value).is_ok()
                    && atoms(value)? != atoms(&current["descr"][key])?
                {
                    return Err(error(
                        "KRAKEN_CHILD_PRICE_MISMATCH",
                        "An issued child differs from its exact approved native execution parameters.",
                    ));
                }
            }
            let native = native_child_parameters(approved, &child, current)?;
            let terminal = matches!(
                current["status"].as_str(),
                Some("closed" | "canceled" | "expired")
            );
            issued_volume = issued_volume
                .checked_add(&atoms(&current[if terminal { "vol_exec" } else { "vol" }])?)?;
            issued_input = issued_input.checked_add(&if terminal {
                atoms(&current[if child.buy { "cost" } else { "vol_exec" }])?
            } else {
                child_input(&child, current, &native)?
            })?;
            if terminal && current["status"] == "canceled" {
                proved_released_volume = proved_released_volume.checked_add(
                    &atoms(&current["vol"])?.checked_sub(&atoms(&current["vol_exec"])?)?,
                )?;
            }
            if let Some(issued) =
                previous.binding["conditionalChildRevisions"][id].get("issuedQuantityAtApproval")
            {
                let issued: Quantity = serde_json::from_value(issued.clone()).map_err(|_| {
                    error(
                        "KRAKEN_CHILD_EVIDENCE_PENDING",
                        "The protected prior issued quantity is unavailable.",
                    )
                })?;
                if issued > atoms(&current["vol"])? {
                    proved_released_volume = proved_released_volume
                        .checked_add(&issued.checked_sub(&atoms(&current["vol"])?)?)?;
                }
            }
            if id == &target {
                selected_native = native;
            }
            child_ids.push(id.clone());
        }
        if issued_volume > *proved_volume
            || issued_input > root.received.atoms
            || child_ids.is_empty()
        {
            return Err(error(
                "KRAKEN_CHILD_GENERATED_CREDIT_BOUND",
                "Existing children exceed the exact entry's proved generated principal. Recover the original relationship; no initial child principal is borrowed or independently reserved.",
            ));
        }
        let mut isolated_order = child.clone();
        isolated_order.volume = atoms(&row["vol"])?;
        isolated_order.maximum_spend = child_input(&child, row, &selected_native)?;
        isolated_order.order_ids = vec![target.clone()];
        isolated_order.uncreated_child = false;
        let (source, output) = product(&isolated_order);
        let mut virtual_binding = previous.binding.clone();
        virtual_binding["orders"] = json!([isolated_order]);
        virtual_binding["parameters"] = selected_native.clone();
        virtual_binding["conditionalClose"] = json!([]);
        virtual_binding["conversionSequences"] = json!([]);
        let virtual_intent = FinancialIntent {
            locks: previous.intent.locks.clone(),
            debits: vec![ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: source.clone(),
                    atoms: isolated_order.maximum_spend.clone(),
                },
            }],
            maximum_costs: previous
                .intent
                .maximum_costs
                .iter()
                .filter(|cost| cost.amount.asset == *source || cost.amount.asset == *output)
                .map(|cost| {
                    let mut cost = cost.clone();
                    cost.treatment = if cost.amount.asset == *source {
                        CostTreatment::AdditionalDebit
                    } else {
                        CostTreatment::DeductedFromCredit
                    };
                    cost
                })
                .collect(),
            expected: vec![EffectConstraint::Conversion {
                resource: account.resource.clone(),
                input: source.clone(),
                maximum_input: isolated_order.maximum_spend.clone(),
                output: output.clone(),
                minimum_output: Quantity::zero(),
            }],
        };
        let virtual_scope = AmendmentScope {
            intent: virtual_intent,
            binding: virtual_binding,
            valuation: previous.valuation.clone(),
        };
        if child.buy {
            for value in [
                selected_native.get(
                    if matches!(
                        selected_native["ordertype"].as_str(),
                        Some("limit" | "iceberg")
                    ) {
                        "price"
                    } else {
                        "price2"
                    },
                ),
                parameters.get(if endpoint == E::KrakenPrivatePostEditOrder {
                    if matches!(
                        selected_native["ordertype"].as_str(),
                        Some("limit" | "iceberg")
                    ) {
                        "price"
                    } else {
                        "price2"
                    }
                } else {
                    "limit_price"
                }),
            ]
            .into_iter()
            .flatten()
            {
                let exact = crate::observation::exact(value)?;
                if exact.starts_with(['+', '-', '#'])
                    || exact.contains('%')
                    || Quantity::from_decimal(&exact, 36)?.is_zero()
                {
                    return Err(error(
                        "KRAKEN_CHILD_EXECUTION_LIMIT_UNBOUNDED",
                        "An issued buy child needs an absolute positive execution limit to preserve its exact generated quote-credit ceiling.",
                    ));
                }
            }
        }
        let mut prepared = if endpoint == E::KrakenPrivatePostEditOrder {
            self.prepare_spot_legacy_edit(
                input,
                account,
                parameters,
                controls,
                Some(&virtual_scope),
            )
            .await?
        } else {
            self.prepare_spot_amendment(input, account, parameters, controls, Some(&virtual_scope))
                .await?
        };
        let isolated = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment { revision, .. } => {
                    Some((**revision).clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_CHILD_REVISION_INVALID",
                    "The exact native child revision is unavailable.",
                )
            })?;
        let revised: SpotOrder = serde_json::from_value(isolated.binding["orders"][0].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_CHILD_REVISION_INVALID",
                    "The revised child quantity is unavailable.",
                )
            })?;
        let revised_volume = issued_volume
            .checked_sub(&isolated_order.volume)?
            .checked_add(&revised.volume)?;
        let revised_input = issued_input
            .checked_sub(&isolated_order.maximum_spend)?
            .checked_add(&revised.maximum_spend)?;
        if revised.volume > isolated_order.volume
            && issued_volume.checked_add(&proved_released_volume)? != *proved_volume
        {
            return Err(error(
                "KRAKEN_CHILD_GENERATED_CREDIT_BOUND",
                "A quantity increase requires complete existing child allocation or an independently proved released remainder. Missing history or an unissued child never supplies usable credit.",
            ));
        }
        if revised_volume > *proved_volume
            || revised_input > root.received.atoms
            || revised_input > child.maximum_spend
        {
            return Err(error(
                "KRAKEN_CHILD_GENERATED_CREDIT_BOUND",
                "This child revision exceeds the original child principal ceiling or its independently proved generated-credit pool. Recover the original entry before requesting additional quantity.",
            ));
        }
        orders[child_index].order_ids = child_ids;
        let mut full = previous.intent.clone();
        for cost in &mut full.maximum_costs {
            cost.amount.atoms=Quantity::from_decimal(controls.maximum_fees.get(&cost.amount.asset.reference).ok_or_else(||error("KRAKEN_FEE_BOUNDS_REQUIRED","Every original sequence fee asset requires its exact cumulative ceiling."))?,36)?;
        }
        for expected in &mut full.expected {
            if let EffectConstraint::ConversionSequence { scope } = expected {
                for cost in &mut scope.maximum_costs {
                    cost.amount.atoms=Quantity::from_decimal(controls.maximum_fees.get(&cost.amount.asset.reference).ok_or_else(||error("KRAKEN_FEE_BOUNDS_REQUIRED","Every original sequence fee currency requires its exact cumulative ceiling."))?,36)?;
                }
            }
        }
        self.verify_spot_fee_admission(account, &orders, controls)
            .await?;
        let mut delta = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for cost in &full.maximum_costs {
            let before = previous
                .intent
                .maximum_costs
                .iter()
                .find(|old| {
                    old.location == cost.location
                        && old.amount.asset == cost.amount.asset
                        && old.treatment == cost.treatment
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_CHILD_REVISION_INVALID",
                        "A child revision cannot introduce another financial fee identity.",
                    )
                })?;
            if cost.amount.atoms > before.amount.atoms {
                let mut increase = cost.clone();
                increase.amount.atoms = cost.amount.atoms.checked_sub(&before.amount.atoms)?;
                delta.maximum_costs.push(increase);
            }
        }
        let mut binding = previous.binding.clone();
        binding["orders"] = json!(orders);
        binding["controls"]["maximumFees"] = json!(controls.maximum_fees);
        binding["conditionalChildRevisions"][&target] = json!({"parentOrderId":parent_id,"childIndex":child_index,"parameters":isolated.binding["parameters"],"minimumReceive":controls.minimum_receive,"issuedQuantityAtApproval":isolated_order.volume.max(revised.volume.clone()),"approvedQuantity":revised.volume,"allowNativeFillClamp":prepared.binding["amendment"]["cancelRemainder"] == true});
        if let Some(old) =
            previous.binding["conditionalChildRevisions"][&target].get("issuedQuantityAtApproval")
        {
            let old: Quantity = serde_json::from_value(old.clone()).map_err(|_| {
                error(
                    "KRAKEN_CHILD_EVIDENCE_PENDING",
                    "The protected prior issued quantity is unavailable.",
                )
            })?;
            let current: Quantity = serde_json::from_value(
                binding["conditionalChildRevisions"][&target]["issuedQuantityAtApproval"].clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_CHILD_EVIDENCE_PENDING",
                    "The current issued quantity is unavailable.",
                )
            })?;
            binding["conditionalChildRevisions"][&target]["issuedQuantityAtApproval"] =
                json!(old.max(current));
        }
        if controls.minimum_receive.is_none() {
            binding["conditionalChildRevisions"][&target]["minimumReceive"] =
                previous.binding["conditionalChildRevisions"][&target]["minimumReceive"].clone();
        }
        binding["conditionalChildRevisions"][&target]["targetOriginalId"] = json!(target);
        if endpoint == E::KrakenPrivatePostEditOrder {
            binding["conditionalChildRevisions"][&target]["legacyRevision"] = json!(true);
            binding["conditionalChildRevisions"][&target]["predecessorParameters"] =
                selected_native.clone();
            binding["conditionalChildRevisions"][&target]["nativeSuccessorQuantity"] =
                prepared.binding["parameters"]["volume"].clone();
            binding["conditionalChildRevisions"][&target]["approvedCumulativeVolume"] =
                json!(revised.volume);
        }
        binding["conversionSequences"] = json!(
            full.expected
                .iter()
                .filter_map(|effect| match effect {
                    EffectConstraint::ConversionSequence { scope } => Some(scope),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
        binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::value_bounds::KEY);
        let mut gross = full.debits.clone();
        for expected in &full.expected {
            if let EffectConstraint::ConversionSequence { scope } = expected {
                for leg in scope.legs.iter().skip(1) {
                    gross.push(ResourceAmount {
                        resource: scope.resource.clone(),
                        amount: Amount {
                            asset: leg.input.clone(),
                            atoms: leg.maximum_input.clone(),
                        },
                    });
                }
            }
        }
        let mut valuation = self
            .financial_valuation(&gross, &full.maximum_costs)
            .await?;
        if let Some(old) = &previous.valuation {
            valuation.movement_usd_micros = valuation
                .movement_usd_micros
                .max(old.movement_usd_micros.clone());
        }
        let revised_scope = AmendmentScope {
            intent: full,
            binding,
            valuation: Some(valuation),
        };
        delta.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: controls
                .operation_id
                .clone()
                .ok_or_else(|| error("INVALID_INPUT", "The original operationId is required."))?,
            submission: original.clone(),
            revision: Box::new(revised_scope),
        });
        prepared.intent = delta;
        prepared.valuation = Some(
            self.financial_valuation(&[], &prepared.intent.maximum_costs)
                .await?,
        );
        prepared.binding["amendmentPreviousScope"] = json!(previous);
        prepared.binding["conditionalOriginalEvidence"] = json!(evidence);
        prepared.binding["conditionalChildRevision"] = json!(true);
        prepared.binding["amendment"]["conditionalChildIndex"] = json!(child_index);
        prepared.binding["amendment"]["batchOrderIndex"] = json!(child_index);
        let wire = &prepared.binding["parameters"];
        let requested_quantity = wire
            .get("order_qty")
            .or_else(|| wire.get("volume"))
            .and_then(Value::as_str)
            .unwrap_or("unchanged");
        let requested_limit = wire
            .get("limit_price")
            .or_else(|| {
                wire.get(
                    if matches!(
                        selected_native["ordertype"].as_str(),
                        Some("limit" | "iceberg")
                    ) {
                        "price"
                    } else {
                        "price2"
                    },
                )
            })
            .and_then(Value::as_str)
            .unwrap_or("unchanged");
        let requested_trigger = wire
            .get("trigger_price")
            .or_else(|| {
                (!matches!(
                    selected_native["ordertype"].as_str(),
                    Some("limit" | "iceberg")
                ))
                .then(|| wire.get("price"))
                .flatten()
            })
            .and_then(Value::as_str)
            .unwrap_or("unchanged");
        let goal = prepared
            .intent
            .expected
            .iter()
            .find_map(|effect| match effect {
                EffectConstraint::SubmissionAmendment { revision, .. } => {
                    revision.binding["conditionalChildRevisions"][&target]["minimumReceive"]
                        .as_str()
                        .map(|amount| {
                            format!(
                                "{amount} {}",
                                if child.buy {
                                    &child.base.reference
                                } else {
                                    &child.quote.reference
                                }
                            )
                        })
                }
                _ => None,
            });
        prepared.summary = format!(
            "Revise issued Kraken child {target} on {}: quantity {requested_quantity}, limit {requested_limit}, trigger {requested_trigger}, minimum receive {}. Entry {parent_id} and its original generated-credit owner remain unchanged.",
            account.name,
            goal.unwrap_or_else(|| "unchanged".into())
        );
        prepared.checks.push(gofin_domain::Check{name:"issued child relationship and generated-credit pool".into(),status:gofin_domain::CheckStatus::Passed,detail:format!("Exact child {target} is linked to entry {parent_id}; proved generated credit is {} {}. Original external principal remains unchanged.",root.received.atoms.to_decimal(36),root.received.asset.reference)});
        prepared.warnings.push("The original entry and conditional template remain unchanged. This exact child remains within the original generated-credit ceiling and the independently proved entry credit; every native fill and fee remains owned by the original operation.".into());
        if !prepared.valuation.as_ref().unwrap().prices.is_empty() {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
}
