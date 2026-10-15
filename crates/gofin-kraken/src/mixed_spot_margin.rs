//! One native cash/margin batch retains distinct custody and extension semantics.
use crate::{
    KrakenEndpoint as E, contracts,
    observation::{atoms, result},
    profile::{KrakenAccount, KrakenProfile, error},
    trading::{TradingControls, proof, unresolved},
};
use gofin_domain::{
    CostTreatment, FinancialIntent, OperationRecord, PreparedOperation, Quantity, Reconciliation,
    Result, Status, Submission, Valuation,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn leveraged(row: &Value) -> bool {
    row["leverage"]
        .as_str()
        .is_some_and(|v| v != "none" && v != "1")
        || row["ordertype"] == "settle-position"
}
pub(crate) fn mixed(parameters: &Value) -> bool {
    parameters["orders"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(leveraged) && rows.iter().any(|r| !leveraged(r)))
}
fn family_input(input: &Value, rows: Vec<Value>, controls: &TradingControls) -> Result<Value> {
    let mut child = input.clone();
    child["orders"] = json!(rows);
    for (key, value) in serde_json::to_value(controls)
        .map_err(|_| error("INVALID_INPUT", "Mixed batch controls are invalid."))?
        .as_object()
        .unwrap()
    {
        if !value.is_null() {
            child[key] = value.clone();
        } else {
            child.as_object_mut().unwrap().remove(key);
        }
    }
    Ok(child)
}
fn combine_valuation(cash: &Valuation, margin: &Valuation) -> Result<Valuation> {
    let mut prices = BTreeMap::new();
    for quote in cash.prices.iter().chain(&margin.prices) {
        if let Some(old) = prices.insert(quote.asset.clone(), quote.clone())
            && (old.usd_micros_per_unit != quote.usd_micros_per_unit
                || old.unit_atoms != quote.unit_atoms
                || old.source != quote.source)
        {
            return Err(error(
                "KRAKEN_BATCH_PRICE_CHANGED",
                "Independent family quotes changed during mixed batch preparation. Preview again.",
            ));
        }
    }
    Ok(Valuation {
        movement_usd_micros: cash
            .movement_usd_micros
            .checked_add(&margin.movement_usd_micros)?,
        maximum_fee_usd_micros: cash
            .maximum_fee_usd_micros
            .checked_add(&margin.maximum_fee_usd_micros)?,
        additional_allocation_usd_micros: cash
            .additional_allocation_usd_micros
            .checked_add(&margin.additional_allocation_usd_micros)?,
        observed_at: gofin_domain::now(),
        source: "Kraken distinct cash principal and full margin extension".into(),
        prices: prices.into_values().collect(),
    })
}
pub(crate) fn allocate_mixed_fee_caps(
    c: &TradingControls,
    cash_weight: &Quantity,
    margin_weight: &Quantity,
) -> Result<(TradingControls, TradingControls)> {
    let total = cash_weight.checked_add(margin_weight)?;
    if total.is_zero() {
        return Err(error(
            "KRAKEN_PRICE_UNKNOWN",
            "Mixed native principal requires positive exact gross valuation.",
        ));
    }
    let mut cash_controls = c.clone();
    cash_controls.maximum_loss = None;
    cash_controls.maximum_profit = None;
    let mut margin_controls = c.clone();
    for (asset, cap) in &c.maximum_fees {
        let global = Quantity::from_decimal(cap, 36)?;
        let cash = global.mul_div_floor(cash_weight, &total)?;
        cash_controls
            .maximum_fees
            .insert(asset.clone(), cash.to_decimal(36));
        margin_controls
            .maximum_fees
            .insert(asset.clone(), global.checked_sub(&cash)?.to_decimal(36));
    }
    Ok((cash_controls, margin_controls))
}
impl KrakenProfile {
    pub(crate) async fn prepare_mixed_spot_margin(
        &self,
        input: &Value,
        a: &KrakenAccount,
        parameters: Value,
        c: &TradingControls,
    ) -> Result<PreparedOperation> {
        if c.minimum_receive.is_some() {
            return Err(error(
                "KRAKEN_BATCH_RECEIPT_SCOPE_REQUIRED",
                "A mixed batch has separate cash credits and leveraged positions. Omit singleton minimumReceive; individual amendments may bind their exact receipt goal.",
            ));
        }
        let rows = parameters["orders"]
            .as_array()
            .ok_or_else(|| error("INVALID_INPUT", "Mixed batch rows are missing."))?;
        let mut cash_rows = vec![];
        let mut margin_rows = vec![];
        let mut members = vec![];
        let mut client_ids = BTreeSet::new();
        for (index, row) in rows.iter().enumerate() {
            if let Some(id) = row["cl_ord_id"].as_str()
                && !client_ids.insert(id.to_owned())
            {
                return Err(error(
                    "INVALID_INPUT",
                    "Every native batch row must have a distinct exact client ID.",
                ));
            }
            let (family, family_index) = if leveraged(row) {
                let n = margin_rows.len();
                margin_rows.push(row.clone());
                ("margin", n)
            } else {
                let n = cash_rows.len();
                cash_rows.push(row.clone());
                ("cash", n)
            };
            members.push(json!({"index":index,"family":family,"familyIndex":family_index}));
        }
        let mut cash_controls = c.clone();
        cash_controls.maximum_loss = None;
        cash_controls.maximum_profit = None;
        let mut margin_controls = c.clone();
        let family_identity = hex::encode(Sha256::digest(c.request_id.as_bytes()));
        cash_controls.request_id = format!("mixed:cash:{family_identity}");
        margin_controls.request_id = format!("mixed:margin:{family_identity}");
        // One exact principal asset is shared across cash and opening loans. Split it
        // before either family derives a native collar; never multiply its authority.
        if let Some(cap) = &c.maximum_spend {
            let cash_buys = cash_rows.len();
            let margin_buys = margin_rows
                .iter()
                .filter(|r| r["reduce_only"] != true && r["ordertype"] != "settle-position")
                .count();
            let count = cash_buys + margin_buys;
            if count > 0 {
                let global = Quantity::from_decimal(cap, 36)?;
                let cash = global.mul_div_floor(
                    &Quantity::from_u64(cash_buys as u64),
                    &Quantity::from_u64(count as u64),
                )?;
                cash_controls.maximum_spend = (cash_buys > 0).then(|| cash.to_decimal(36));
                margin_controls.maximum_spend = (margin_buys > 0).then(|| {
                    global
                        .checked_sub(&cash)
                        .expect("bounded split")
                        .to_decimal(36)
                });
            }
        }
        let prepare_cash =
            |controls: &TradingControls| family_input(input, cash_rows.clone(), controls);
        let prepare_margin =
            |controls: &TradingControls| family_input(input, margin_rows.clone(), controls);
        let initial_cash = prepare_cash(&cash_controls)?;
        let initial_margin = prepare_margin(&margin_controls)?;
        let cash = Box::pin(self.prepare_trading(E::KrakenPrivatePostAddOrderBatch, &initial_cash))
            .await?;
        let (_, margin_native) =
            contracts::decode(E::KrakenPrivatePostAddOrderBatch, &initial_margin)?;
        let margin = self
            .prepare_margin(
                E::KrakenPrivatePostAddOrderBatch,
                &initial_margin,
                a,
                margin_native,
                &margin_controls,
            )
            .await?;
        if let Some(cap) = c.maximum_spend.as_deref() {
            let mut totals = crate::margin::opening_loan_principals(&margin.binding)?;
            let root = cash.binding["rootOrderCount"].as_u64().unwrap_or(0) as usize;
            let orders: Vec<crate::trading::SpotOrder> =
                serde_json::from_value(cash.binding["orders"].clone()).map_err(|_| {
                    error(
                        "KRAKEN_PREVIEW_INVALID",
                        "Protected cash principal scope is missing.",
                    )
                })?;
            for order in orders.iter().take(root) {
                let asset = if order.buy { &order.quote } else { &order.base };
                let sum = totals.entry(asset.clone()).or_default();
                *sum = sum.checked_add(&order.maximum_spend)?;
            }
            if totals.len() != 1 {
                return Err(contracts::unsupported_control(
                    E::KrakenPrivatePostAddOrderBatch,
                    vec!["/maximumSpend".into()],
                ));
            }
            if totals.into_values().next().unwrap() > Quantity::from_decimal(cap, 36)? {
                return Err(error(
                    "KRAKEN_SPEND_BOUND_EXCEEDED",
                    "The full mixed cash principal and margin loan extension exceed maximumSpend in their exact shared asset.",
                ));
            }
        }
        let cash_weight = &cash
            .valuation
            .as_ref()
            .ok_or_else(|| error("KRAKEN_PRICE_UNKNOWN", "Mixed cash valuation is missing."))?
            .movement_usd_micros;
        let margin_weight = &margin
            .valuation
            .as_ref()
            .ok_or_else(|| error("KRAKEN_PRICE_UNKNOWN", "Mixed margin valuation is missing."))?
            .movement_usd_micros;
        let (allocated_cash, allocated_margin) =
            allocate_mixed_fee_caps(c, cash_weight, margin_weight)?;
        cash_controls.maximum_fees = allocated_cash.maximum_fees;
        margin_controls.maximum_fees = allocated_margin.maximum_fees;
        let cash_input = prepare_cash(&cash_controls)?;
        let margin_input = prepare_margin(&margin_controls)?;
        let cash =
            Box::pin(self.prepare_trading(E::KrakenPrivatePostAddOrderBatch, &cash_input)).await?;
        let (_, margin_native) =
            contracts::decode(E::KrakenPrivatePostAddOrderBatch, &margin_input)?;
        let margin = self
            .prepare_margin(
                E::KrakenPrivatePostAddOrderBatch,
                &margin_input,
                a,
                margin_native,
                &margin_controls,
            )
            .await?;
        let mut exact_clients = BTreeSet::new();
        for order in cash.binding["orders"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                margin.binding["margin"]["orders"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            )
        {
            if !exact_clients.insert(order["clientId"].as_str().unwrap_or("").to_owned()) {
                return Err(error(
                    "INVALID_INPUT",
                    "Cash and leveraged native rows must retain globally distinct exact client identities.",
                ));
            }
        }
        let mut native = parameters;
        for member in &members {
            let i = member["index"].as_u64().unwrap() as usize;
            let n = member["familyIndex"].as_u64().unwrap() as usize;
            let family = if member["family"] == "cash" {
                &cash
            } else {
                &margin
            };
            native["orders"][i] = family.binding["parameters"]["orders"][n].clone();
        }
        let mut intent = FinancialIntent {
            debits: cash.intent.debits.clone(),
            maximum_costs: cash.intent.maximum_costs.clone(),
            expected: cash.intent.expected.clone(),
            locks: cash.intent.locks.clone(),
        };
        intent.debits.extend(margin.intent.debits.clone());
        intent
            .maximum_costs
            .extend(margin.intent.maximum_costs.clone());
        intent.expected.extend(margin.intent.expected.clone());
        for lock in &margin.intent.locks {
            if !intent.locks.contains(lock) {
                intent.locks.push(lock.clone());
            }
        }
        // Family checks alone cannot see their combined additional cash reservations.
        let balance = self
            .json(E::KrakenPrivatePostBalanceEx, Some(a), json!({}))
            .await?;
        let mut required = BTreeMap::new();
        for debit in &intent.debits {
            let sum = required
                .entry(debit.amount.asset.clone())
                .or_insert_with(Quantity::zero);
            *sum = sum.checked_add(&debit.amount.atoms)?;
        }
        for cost in &intent.maximum_costs {
            if cost.treatment == CostTreatment::AdditionalDebit {
                let sum = required
                    .entry(cost.amount.asset.clone())
                    .or_insert_with(Quantity::zero);
                *sum = sum.checked_add(&cost.amount.atoms)?;
            }
        }
        for (asset, needed) in required {
            let row = &result(&balance)[&asset.reference];
            let free = atoms(&row["balance"])?.checked_sub(&atoms(&row["hold_trade"])?)?;
            if needed > free {
                return Err(error(
                    "INSUFFICIENT_BALANCE",
                    "Existing custody cannot cover combined mixed-batch cash principal, fees and bounded margin losses.",
                ));
            }
        }
        let valuation = combine_valuation(
            cash.valuation.as_ref().unwrap(),
            margin.valuation.as_ref().unwrap(),
        )?;
        let mut prepared = cash.clone();
        prepared.input = input.clone();
        prepared.intent = intent;
        prepared.valuation = Some(valuation);
        prepared.binding = json!({"endpoint":E::KrakenPrivatePostAddOrderBatch.method(),"product":self.product,"account":a.name,"physicalIdentity":a.physical_identity,"parameters":native,"requestId":c.request_id,"mixedSpotMargin":{"cash":cash.binding,"margin":margin.binding,"cashIntent":cash.intent,"marginIntent":margin.intent,"cashValuation":cash.valuation,"marginValuation":margin.valuation,"members":members,"controls":c}});
        prepared.binding["costBounds"] = json!(prepared.intent.maximum_costs);
        prepared.binding[gofin_domain::BASELINE_ANCHORS_KEY] =
            margin.binding[gofin_domain::BASELINE_ANCHORS_KEY].clone();
        prepared.summary = format!(
            "Place {} Kraken cash order(s) and {} leveraged order(s) in one exact native batch on {}; cash principal and full margin loans remain distinct.",
            cash_rows.len(),
            margin_rows.len(),
            a.name
        );
        prepared.warnings.extend(margin.warnings);
        prepared.warnings.push("The declared per-asset fee ceilings and maximumSpend are shared across this batch. Fee ceilings are allocated by independently priced gross family exposure; explicit cash fee reservations never finance leveraged loan principal.".into());
        prepared.observed_at = gofin_domain::now();
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(crate) async fn reconcile_mixed_spot_margin(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        let mixed = &binding["mixedSpotMargin"];
        let mut observations = vec![];
        let mut effects = vec![];
        let mut events = BTreeMap::new();
        let mut statuses = vec![];
        for family in ["cash", "margin"] {
            let mut family_record = record.clone();
            // Native batch receipt indices refer to original ordering, not family indices.
            if let Some(dispatch) = family_record.dispatch_evidence.as_mut() {
                let receipt = &mut dispatch["response"];
                if let Some(rows) = receipt["result"]["orders"].as_array() {
                    let original = rows.clone();
                    let mut selected = vec![];
                    for member in mixed["members"].as_array().ok_or_else(|| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Mixed native member identities are missing.",
                        )
                    })? {
                        if member["family"] == family {
                            selected.push(
                                original
                                    .get(member["index"].as_u64().unwrap() as usize)
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            );
                        }
                    }
                    if selected.len() == 1
                        && let Some(id) = selected[0]["txid"].as_str()
                    {
                        receipt["result"]["txid"] = json!([id]);
                    }
                    receipt["result"]["orders"] = json!(selected);
                }
            }
            let mut family_binding = mixed[family].clone();
            family_binding["_finBatchCostScope"] = json!(true);
            let observed = Box::pin(self.reconcile_trading_binding(
                &family_record,
                submission,
                &family_binding,
            ))
            .await?;
            statuses.push(observed.status.clone());
            observations.push(
                json!({"family":family,"status":observed.status,"evidence":observed.evidence}),
            );
            if let Some(verified) = observed.verified {
                for event in verified.evidence["proofEvents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let id = event["eventId"].as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_EVIDENCE_INVALID",
                            "Mixed financial events require immutable IDs.",
                        )
                    })?;
                    if let Some(old) = events.insert(id.to_owned(), event.clone())
                        && old != *event
                    {
                        return Ok(unresolved(
                            Status::Unknown,
                            "Cash and leveraged families supplied conflicting native proof identities.",
                            json!({"eventId":id}),
                        ));
                    }
                }
                effects.extend(verified.effects);
            }
        }
        if statuses.contains(&Status::Unknown) {
            return Ok(unresolved(
                Status::Unknown,
                "One native cash/margin family lacks unambiguous evidence; original reservations and prior proofs remain protected.",
                json!({"families":observations}),
            ));
        }
        // Financial families prove their native costs separately; authority and
        // consumption use the one whole-command exact accounting envelope.
        let bounds = binding
            .get("_finAccountingCostBounds")
            .or_else(|| binding.get("costBounds"))
            .map(|v| {
                serde_json::from_value::<Vec<gofin_domain::CostBound>>(v.clone()).map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Original mixed typed cost ceilings are malformed.",
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
        if crate::unresolved_consumption::costs_exceed_bounds(&effects, &bounds)? {
            return crate::unresolved_consumption::cost_overrun(
                submission,
                &effects,
                json!({"code":"KRAKEN_MIXED_COST_BOUND_EXCEEDED","families":observations,"proofEvents":events.into_values().collect::<Vec<_>>()}),
            );
        }
        let pending = statuses.contains(&Status::Pending);
        let status = if pending {
            Status::Pending
        } else if statuses.contains(&Status::Failed) {
            Status::Failed
        } else {
            Status::Completed
        };
        if pending && events.is_empty() {
            return Ok(unresolved(
                Status::Pending,
                "Native mixed cash/margin orders remain open without verified fills.",
                json!({"families":observations}),
            ));
        }
        proof(
            submission,
            status.clone(),
            effects,
            json!({"families":observations,"proofEvents":events.into_values().collect::<Vec<_>>(),"partialExecution":status==Status::Failed,"coverage":"Separate exact custody conversion and full margin extension proofs under one immutable native batch."}),
        )
    }
}
