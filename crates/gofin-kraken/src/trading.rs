//! Exact order identities and proof-backed spot trading. No transport acknowledgment settles cash.
use crate::{
    KrakenEndpoint, contracts,
    observation::{atoms, result, spot_credit_used},
    profile::{KrakenAccount, KrakenProfile, error},
};
use chrono::{Duration, Utc};
use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, EffectConstraint,
    FinancialEffect, FinancialIntent, OperationRecord, PreparedOperation, Quantity, Reconciliation,
    ResourceAmount, Result, Status, Submission, Valuation, VerifiedEffects,
};
use gofin_exchange::ExchangeProfile;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Native validation and timer configuration have no principal, exposure or fee
/// authority. Attribute the exact zero without introducing invented asset quotes.
pub(crate) fn zero_authority_valuation(endpoint: KrakenEndpoint) -> Valuation {
    Valuation {
        movement_usd_micros: Quantity::zero(),
        maximum_fee_usd_micros: Quantity::zero(),
        additional_allocation_usd_micros: Quantity::zero(),
        observed_at: gofin_domain::now(),
        source: format!(
            "Kraken zero-principal/no-cost native authority: {}",
            endpoint.method()
        ),
        prices: vec![],
    }
}

pub(crate) fn assert_cash_trade(trade: &Value) -> Result<()> {
    let margin = trade
        .get("margin")
        .map(atoms)
        .transpose()?
        .unwrap_or_default();
    let leverage = match trade.get("leverage") {
        None => false,
        Some(Value::String(value)) => !matches!(value.as_str(), "none" | "0" | "1"),
        Some(_) => {
            return Err(error(
                "KRAKEN_EVIDENCE_INVALID",
                "Kraken trade leverage is not an exact native string.",
            ));
        }
    };
    let closing = match trade.get("misc") {
        None => false,
        Some(Value::String(flags)) => flags.split(',').any(|flag| flag.trim() == "closing"),
        Some(_) => {
            return Err(error(
                "KRAKEN_EVIDENCE_INVALID",
                "Kraken trade flags are not a native string.",
            ));
        }
    };
    if !margin.is_zero() || leverage || closing {
        return Err(error(
            "KRAKEN_UNEXPECTED_MARGIN_EFFECT",
            "Spot trade evidence contains margin or position closure; cash conversion settlement is refused.",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TradingControls {
    pub request_id: String,
    #[serde(default)]
    pub maximum_fees: BTreeMap<String, String>,
    pub maximum_spend: Option<String>,
    pub minimum_receive: Option<String>,
    pub maximum_loss: Option<String>,
    pub maximum_profit: Option<String>,
    pub maximum_funding_loss: Option<String>,
    pub maximum_funding_profit: Option<String>,
    pub operation_id: Option<String>,
    pub original_submission: Option<Submission>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settlement_limits: BTreeMap<String, SettlementLimits>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SettlementLimits {
    pub maximum_loss: String,
    pub maximum_profit: String,
    pub maximum_funding_loss: String,
    pub maximum_funding_profit: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SpotOrder {
    pub(crate) client_id: String,
    pub(crate) pair: String,
    pub(crate) base: AssetId,
    pub(crate) quote: AssetId,
    pub(crate) buy: bool,
    pub(crate) volume: Quantity,
    pub(crate) maximum_spend: Quantity,
    pub(crate) quoted_volume: bool,
    pub(crate) display_pair: String,
    #[serde(default)]
    pub(crate) order_ids: Vec<String>,
    #[serde(default)]
    pub(crate) quoted_fee_notional: Quantity,
    #[serde(default)]
    pub(crate) user_reference: Option<Value>,
    #[serde(default)]
    pub(crate) uncreated_child: bool,
}
fn reject_conditional_parent_revision(
    binding: &Value,
    parameters: &Value,
    orders: &[SpotOrder],
) -> Result<()> {
    for conditional in binding["conditionalClose"].as_array().into_iter().flatten() {
        let Some(parent) = conditional["parentIndex"]
            .as_u64()
            .and_then(|i| orders.get(i as usize))
        else {
            continue;
        };
        let selected = parameters["cl_ord_id"]
            .as_str()
            .is_some_and(|id| id == parent.client_id)
            || parameters["txid"]
                .as_str()
                .is_some_and(|id| parent.order_ids.iter().any(|old| old == id));
        if selected {
            return Err(error(
                "KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED",
                "Kraken excludes orders with attached conditional-close terms from both atomic amendments and legacy edits. Cancel the original through its GoFin operation, then place a separately approved replacement; generated child orders require their own exact linked status.",
            ));
        }
    }
    Ok(())
}
fn exact_spot_ledger_asset<'a>(
    code: &str,
    base: &'a AssetId,
    quote: &'a AssetId,
    resolved_alias: Option<&AssetId>,
) -> Result<&'a AssetId> {
    if code == base.reference || resolved_alias == Some(base) {
        Ok(base)
    } else if code == quote.reference || resolved_alias == Some(quote) {
        Ok(quote)
    } else {
        Err(error(
            "KRAKEN_UNEXPECTED_TRADE_ASSET",
            "A spot trade ledger debits an undeclared asset identity.",
        ))
    }
}
fn quote_price_collar(market: &Value, cap: &str, volume: &Quantity) -> Result<Quantity> {
    let cap = Quantity::from_decimal(cap, 36)?;
    let precision = market["pair_decimals"]
        .as_u64()
        .filter(|n| *n <= 36)
        .ok_or_else(|| {
            error(
                "KRAKEN_MARKET_PRECISION_UNKNOWN",
                "An exact price collar needs the venue's published pair price precision.",
            )
        })?;
    let quantum = Quantity::new(format!("1{}", "0".repeat(36 - precision as usize)))?.integer();
    let scale = Quantity::from_decimal("1", 36)?.integer();
    let raw = cap.integer() * scale / volume.integer();
    let price = Quantity::new(((&raw / &quantum) * &quantum).to_string())?;
    if price.is_zero() {
        return Err(error(
            "KRAKEN_SPEND_BOUND_EXCEEDED",
            "The selected quote ceiling cannot express a positive native price tick for this base quantity.",
        ));
    }
    Ok(price)
}
pub(crate) fn is_trading(endpoint: KrakenEndpoint) -> bool {
    matches!(
        endpoint,
        KrakenEndpoint::KrakenPrivatePostAddOrder
            | KrakenEndpoint::KrakenPrivatePostAddOrderBatch
            | KrakenEndpoint::KrakenPrivatePostAmendOrder
            | KrakenEndpoint::KrakenPrivatePostEditOrder
            | KrakenEndpoint::KrakenPrivatePostCancelOrder
            | KrakenEndpoint::KrakenPrivatePostCancelAll
            | KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter
            | KrakenEndpoint::KrakenPrivatePostCancelOrderBatch
            | KrakenEndpoint::KrakenfuturesPrivatePostSendorder
            | KrakenEndpoint::KrakenfuturesPrivatePostEditorder
            | KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
            | KrakenEndpoint::KrakenfuturesPrivatePostCancelorder
            | KrakenEndpoint::KrakenfuturesPrivatePostCancelallorders
            | KrakenEndpoint::KrakenfuturesPrivatePostCancelallordersafter
    )
}
pub(crate) fn controls(input: &Value) -> Result<TradingControls> {
    let mut control = serde_json::Map::new();
    for key in [
        "requestId",
        "maximumFees",
        "maximumSpend",
        "minimumReceive",
        "maximumLoss",
        "maximumProfit",
        "maximumFundingLoss",
        "maximumFundingProfit",
        "operationId",
        "originalSubmission",
        "settlementLimits",
    ] {
        if let Some(value) = input.get(key) {
            control.insert(key.into(), value.clone());
        }
    }
    let decoded: TradingControls = serde_json::from_value(Value::Object(control))
        .map_err(|_| error("INVALID_INPUT", "Kraken execution controls are invalid."))?;
    if decoded.request_id.is_empty()
        || decoded.request_id.len() > 128
        || !decoded
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
    {
        return Err(error(
            "INVALID_INPUT",
            "Every Kraken write requires an explicit stable requestId (1–128 letters, digits, hyphens, underscores or colons).",
        ));
    }
    Ok(decoded)
}
// Preserve the original external client ID and its 18-character venue limit.
fn client_id(account: &KrakenAccount, request_id: &str, index: usize) -> String {
    format!(
        "fin{}",
        &hex::encode(Sha256::digest(format!(
            "fin-kraken-order-v1:{}:{request_id}:{index}",
            account.physical_identity
        )))[..15]
    )
}
fn nonzero(v: &Value) -> Result<Quantity> {
    let q = atoms(v)?;
    if q.is_zero() {
        return Err(error(
            "INVALID_INPUT",
            "Order quantities and prices must be positive exact decimals.",
        ));
    }
    Ok(q)
}
fn decimal_product(a: &Quantity, b: &Quantity) -> Result<Quantity> {
    a.mul_div_ceil(b, &Quantity::new(format!("1{}", "0".repeat(36)))?)
}
pub(crate) fn normalize_proof_numbers(value: &mut Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for v in fields.values_mut() {
                normalize_proof_numbers(v)?;
            }
        }
        Value::Array(values) => {
            for v in values {
                normalize_proof_numbers(v)?;
            }
        }
        Value::Number(n) if n.as_i64().is_none() && n.as_u64().is_none() => {
            *value = json!(crate::observation::exact(&Value::Number(n.clone()))?);
        }
        _ => (),
    };
    Ok(())
}
pub(crate) fn proof(
    submission: &Submission,
    status: Status,
    mut effects: Vec<FinancialEffect>,
    mut evidence: Value,
) -> Result<Reconciliation> {
    normalize_proof_numbers(&mut evidence)?;
    for effect in &mut effects {
        match effect {
            FinancialEffect::PositionChange {
                anchor: Some(anchor),
                ..
            }
            | FinancialEffect::DebtChange {
                anchor: Some(anchor),
                ..
            } => normalize_proof_numbers(&mut anchor.evidence)?,
            _ => (),
        };
    }
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: evidence.clone(),
        observed_at: gofin_domain::now(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = gofin_domain::verified_effect_digest(&bundle)?;
    Ok(Reconciliation {
        status,
        evidence,
        verified: Some(bundle),
    })
}
pub(crate) fn unresolved(status: Status, reason: &str, evidence: Value) -> Reconciliation {
    Reconciliation {
        status,
        evidence: json!({"reason":reason,"observations":evidence}),
        verified: None,
    }
}
impl KrakenProfile {
    pub(crate) async fn prepare_spot_legacy_edit(
        &self,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        control: &TradingControls,
        previous: Option<&gofin_domain::amendments::AmendmentScope>,
    ) -> Result<PreparedOperation> {
        contracts::validate_fin_controls(
            KrakenEndpoint::KrakenPrivatePostEditOrder,
            input,
            previous.map(|scope| &scope.binding).or_else(|| {
                control
                    .original_submission
                    .as_ref()
                    .map(|submission| &submission.data["binding"])
            }),
        )?;
        let original = control.original_submission.as_ref().ok_or_else(|| {
            error(
                "INVALID_INPUT",
                "Legacy edit requires exact originalSubmission and operationId.",
            )
        })?;
        let b = previous
            .map(|p| &p.binding)
            .unwrap_or(&original.data["binding"]);
        let orders: Vec<SpotOrder> = serde_json::from_value(b["orders"].clone()).map_err(|_| {
            error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original order scope missing.",
            )
        })?;
        reject_conditional_parent_revision(b, &parameters, &orders)?;
        if orders.first().is_some_and(|o| o.quoted_volume) {
            return Err(error(
                "KRAKEN_QUOTE_VOLUME_REVISION_UNBOUNDED",
                "Legacy edit volume is a base-asset quantity and cannot preserve this market buy's quote-volume cash ceiling. Cancel the original and place a separately approved quote-volume replacement.",
            ));
        }
        if orders.len() != 1 {
            return Err(error(
                "KRAKEN_LEGACY_EDIT_SCOPE_PENDING",
                "Legacy edits require one exact base-volume GoFin order.",
            ));
        }
        let old = &orders[0];
        if !matches!(parameters["pair"].as_str(),Some(p)if p==old.pair||p==old.display_pair) {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Legacy pair must retain the original exact venue market.",
            ));
        }
        if parameters["oflags"]
            .as_str()
            .is_some_and(|flags| flags.split(',').any(|f| !f.is_empty() && f != "post"))
        {
            return Err(error(
                "INVALID_INPUT",
                "Legacy edits can change only the documented post flag.",
            ));
        }
        let open = self
            .json(
                KrakenEndpoint::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let matches = result(&open)["open"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_PROOF_PENDING",
                    "Original open order evidence missing.",
                )
            })?
            .iter()
            .filter(|(id, o)| {
                if old.order_ids.is_empty() {
                    o["cl_ord_id"] == old.client_id
                } else {
                    old.order_ids.contains(id)
                }
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(error(
                "KRAKEN_ORDER_NOT_OPEN",
                "Exactly one original order remainder must be open.",
            ));
        }
        let (id, venue) = matches[0];
        check_legacy_native_trigger(venue)?;
        if parameters["txid"].as_str().is_some_and(|v| v != id)
            || parameters["txid"]
                .as_i64()
                .is_some_and(|n| venue["userref"].as_i64() != Some(n))
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Legacy selector does not identify original open order.",
            ));
        }
        let mut native = parameters;
        native["txid"] = json!(id);
        let volume = native
            .get("volume")
            .map(atoms)
            .transpose()?
            .unwrap_or(old.volume.clone());
        let filled = atoms(&venue["vol_exec"])?;
        if volume < filled {
            return Err(error(
                "KRAKEN_AMENDMENT_BELOW_PROVED_FILL",
                "Kraken rejects legacy quantity below already executed volume.",
            ));
        }
        // Legacy executions remain on the canceled predecessor. Reserve the
        // entire requested successor quantity in addition to proved old fills;
        // no assumption about the venue copying or subtracting executions.
        let mut mapped = json!({"txid":id,"order_qty":volume.checked_add(&filled)?.to_decimal(36)});
        for (from, to) in [
            ("displayvol", "display_qty"),
            (
                "price",
                if matches!(
                    b["parameters"]["ordertype"].as_str(),
                    Some("limit" | "iceberg")
                ) {
                    "limit_price"
                } else {
                    "trigger_price"
                },
            ),
            ("price2", "limit_price"),
            ("deadline", "deadline"),
        ] {
            if let Some(v) = native.get(from) {
                mapped[to] = v.clone();
            }
        }
        let mut prepared = self
            .prepare_spot_amendment(input, account, mapped, control, previous)
            .await?;
        for expected in &mut prepared.intent.expected {
            if let EffectConstraint::SubmissionAmendment { revision, .. } = expected {
                let original_flags = b["parameters"]["oflags"].as_str().unwrap_or("");
                let mut flags = original_flags
                    .split(',')
                    .filter(|f| !f.is_empty() && *f != "post")
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if native["oflags"]
                    .as_str()
                    .is_some_and(|f| f.split(',').any(|v| v == "post"))
                {
                    flags.push("post".into());
                }
                if flags.is_empty() {
                    revision.binding["parameters"]
                        .as_object_mut()
                        .unwrap()
                        .remove("oflags");
                } else {
                    revision.binding["parameters"]["oflags"] = json!(flags.join(","));
                }
            }
        }
        prepared.operation = KrakenEndpoint::KrakenPrivatePostEditOrder
            .operation()
            .into();
        prepared.binding["endpoint"] = json!(KrakenEndpoint::KrakenPrivatePostEditOrder.method());
        prepared.binding["parameters"] = native;
        prepared.binding["legacyEdit"] = json!(true);
        prepared.summary = format!(
            "Legacy edit original Kraken order {id}; reserve predecessor fills plus the full requested successor quantity and declared fees. The venue returns a new order ID."
        );
        prepared.warnings.push("Legacy edits lose queue priority and do not copy existing executions to the new order.".into());
        Ok(prepared)
    }
    async fn reconcile_spot_legacy_edit(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        b: &Value,
    ) -> Result<Reconciliation> {
        let account =
            self.account(b["account"].as_str().ok_or_else(|| {
                error("KRAKEN_SUBMISSION_INVALID", "Legacy edit account missing.")
            })?)?;
        let receipt = record
            .dispatch_evidence
            .as_ref()
            .and_then(|r| r.get("response"))
            .map(result);
        let Some(receipt) = receipt else {
            return Ok(unresolved(
                Status::Unknown,
                "Legacy edit response was lost; successor identity is ambiguous and no edit is replayed.",
                json!({}),
            ));
        };
        let Some(successor) = receipt["txid"].as_str() else {
            return Ok(unresolved(
                Status::Unknown,
                "Legacy edit lacks exact successor transaction ID.",
                receipt.clone(),
            ));
        };
        let predecessor = b["amendment"]["orderId"].as_str().unwrap();
        if receipt["originaltxid"] != predecessor
            || receipt
                .get("status")
                .is_some_and(|s| !matches!(s.as_str(), Some("ok" | "Ok")))
            || receipt
                .get("orders_cancelled")
                .is_some_and(|n| n.as_u64() != Some(1))
        {
            return Ok(unresolved(
                Status::Unknown,
                "Legacy edit acknowledgment does not prove the exact canceled predecessor and successor relationship.",
                receipt.clone(),
            ));
        }
        let query = self
            .json(
                KrakenEndpoint::KrakenPrivatePostQueryOrders,
                Some(account),
                json!({"txid":format!("{predecessor},{successor}"),"trades":true}),
            )
            .await?;
        let map = result(&query);
        if map[predecessor]["status"] != "canceled" || !map[successor].is_object() {
            return Ok(unresolved(
                Status::Pending,
                "Original canceled order and newly created successor are not both visible.",
                query,
            ));
        }
        let old = &map[predecessor];
        let new = &map[successor];
        if old["descr"]["pair"] != new["descr"]["pair"]
            || old["descr"]["type"] != new["descr"]["type"]
        {
            return Err(error(
                "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                "Legacy successor changed original market or direction.",
            ));
        }
        for field in ["price", "price2"] {
            if let Some(v) = b["parameters"].get(field)
                && atoms(v)? != atoms(&new["descr"][field])?
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                    "Legacy successor price differs from exact requested edit.",
                ));
            }
        }
        if let Some(v) = b["parameters"].get("volume")
            && atoms(&new["vol"])? > atoms(v)?
        {
            return Err(error(
                "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                "Legacy successor quantity exceeds the full requested quantity ceiling.",
            ));
        }
        let original: Submission =
            serde_json::from_value(b["amendment"]["originalSubmission"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original legacy edit submission invalid.",
                )
            })?;
        let successor_submission = Submission {
            kind: original.kind.clone(),
            reference: format!("{}:legacy:{successor}", original.reference),
            data: json!({"venueOrderId":successor,"predecessorOrderId":predecessor,"clientId":new["cl_ord_id"],"batchOrderIndex":b["amendment"]["batchOrderIndex"]}),
        };
        proof(
            submission,
            Status::Completed,
            vec![FinancialEffect::SubmissionAmendment {
                operation_id: b["amendment"]["operationId"].as_str().unwrap().into(),
                submission: original,
                successor: Some(successor_submission),
            }],
            json!({"predecessor":old,"successor":new,"acknowledgment":receipt}),
        )
    }
    pub(crate) async fn prepare_spot_amendment(
        &self,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        control: &TradingControls,
        previous: Option<&gofin_domain::amendments::AmendmentScope>,
    ) -> Result<PreparedOperation> {
        contracts::validate_fin_controls(
            KrakenEndpoint::KrakenPrivatePostAmendOrder,
            input,
            previous.map(|scope| &scope.binding).or_else(|| {
                control
                    .original_submission
                    .as_ref()
                    .map(|submission| &submission.data["binding"])
            }),
        )?;
        let operation_id = control
            .operation_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                error(
                    "INVALID_INPUT",
                    "An amendment requires the original operationId.",
                )
            })?;
        let original = control.original_submission.as_ref().ok_or_else(|| {
            error(
                "INVALID_INPUT",
                "An amendment requires exact originalSubmission.",
            )
        })?;
        let b = previous
            .map(|s| &s.binding)
            .unwrap_or(&original.data["binding"]);
        if original.kind != "kraken-rest-v1"
            || b["account"] != account.name
            || b["physicalIdentity"] != account.physical_identity
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original submission must identify this exact spot account.",
            ));
        }
        let orders: Vec<SpotOrder> = serde_json::from_value(b["orders"].clone()).map_err(|_| {
            error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original order scope missing.",
            )
        })?;
        reject_conditional_parent_revision(b, &parameters, &orders)?;
        if orders.first().is_some_and(|o| o.quoted_volume) {
            return Err(error(
                "KRAKEN_QUOTE_VOLUME_REVISION_UNBOUNDED",
                "Atomic order_qty is a base-asset quantity and cannot preserve this market buy's quote-volume cash ceiling; the market order type cannot be changed in-place. Cancel the original and place a separately approved quote-volume replacement.",
            ));
        }
        if orders.len() != 1 {
            return Err(error(
                "KRAKEN_AMENDMENT_SCOPE_PENDING",
                "Atomic amendments currently require one base-volume GoFin order; batch and quote-volume revision need per-order scope accounting.",
            ));
        }
        let old = &orders[0];
        let old_controls: TradingControls =
            serde_json::from_value(b["controls"].clone()).map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Original execution controls missing.",
                )
            })?;
        let all = self
            .json(
                KrakenEndpoint::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let matched = result(&all)["open"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_PROOF_PENDING",
                    "Open order evidence unavailable.",
                )
            })?
            .iter()
            .filter(|(id, o)| {
                if old.order_ids.is_empty() {
                    o["cl_ord_id"] == old.client_id
                } else {
                    old.order_ids.contains(id)
                }
            })
            .collect::<Vec<_>>();
        if matched.len() != 1 {
            return Err(error(
                "KRAKEN_ORDER_NOT_OPEN",
                "One exact original order must remain open to amend.",
            ));
        }
        let (id, venue) = matched[0];
        if parameters["txid"].as_str().is_some_and(|v| v != id)
            || parameters["cl_ord_id"]
                .as_str()
                .is_some_and(|v| v != old.client_id)
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Amendment selectors do not match the original GoFin order.",
            ));
        }
        let mut wire = parameters;
        wire["txid"] = json!(id);
        wire.as_object_mut().unwrap().remove("cl_ord_id");
        let mut replacement = b["parameters"].clone();
        for (from, to) in [
            ("order_qty", "volume"),
            ("display_qty", "displayvol"),
            (
                "limit_price",
                if matches!(replacement["ordertype"].as_str(), Some("limit" | "iceberg")) {
                    "price"
                } else {
                    "price2"
                },
            ),
            ("trigger_price", "price"),
        ] {
            if let Some(v) = wire.get(from) {
                replacement[to] = v.clone();
            }
        }
        // post_only controls whether this exact price amendment may cross the
        // book; it does not grant additional principal or change fee ceilings.
        if wire["post_only"] == true && wire.get("limit_price").is_none() {
            return Err(error(
                "INVALID_INPUT",
                "post_only requires an exact limit_price amendment.",
            ));
        }
        let filled = atoms(&venue["vol_exec"])?;
        let requested_quantity = wire.get("order_qty").map(atoms).transpose()?;
        let cancel_remainder = requested_quantity.as_ref().is_some_and(|q| *q <= filled);
        if cancel_remainder {
            // Kraken accepts quantities below existing fills and restates the
            // quantity to the filled amount while cancelling the remainder.
            // Keep the exact human quantity on the wire; financial scope uses
            // the already proved quantity, never a negative remaining amount.
            replacement["volume"] = json!(filled.to_decimal(36));
        }
        let mut revised = self
            .prepare_spot_order(account, &mut replacement, control, 0, cancel_remainder)
            .await?;
        let mut historical_volume = Quantity::zero();
        let mut historical_cost = Quantity::zero();
        if old.order_ids.len() > 1 {
            let prior = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostQueryOrders,
                    Some(account),
                    json!({"txid":old.order_ids.join(","),"trades":true}),
                )
                .await?;
            for previous_id in &old.order_ids {
                if previous_id == id {
                    continue;
                }
                let o = &result(&prior)[previous_id];
                if !matches!(
                    o["status"].as_str(),
                    Some("canceled" | "closed" | "expired")
                ) {
                    return Err(error(
                        "KRAKEN_AMENDMENT_LINEAGE_PENDING",
                        "A predecessor lacks terminal evidence.",
                    ));
                }
                historical_volume = historical_volume.checked_add(&atoms(&o["vol_exec"])?)?;
                historical_cost = historical_cost.checked_add(&atoms(&o["cost"])?)?;
            }
        }
        if revised.volume < filled {
            return Err(error(
                "KRAKEN_AMENDMENT_BELOW_PROVED_FILL",
                "Revised total quantity cannot fall below already executed original fills.",
            ));
        }
        if revised.client_id != old.client_id || revised.pair != old.pair || revised.buy != old.buy
        {
            return Err(error(
                "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                "Atomic amendment must preserve original identity, exact market and direction.",
            ));
        }
        if revised.buy {
            let price = nonzero(
                &replacement[if matches!(
                    replacement["ordertype"].as_str(),
                    Some("limit" | "iceberg")
                ) {
                    "price"
                } else {
                    "price2"
                }],
            )?;
            revised.maximum_spend = historical_cost
                .checked_add(&atoms(&venue["cost"])?)?
                .checked_add(&decimal_product(
                    &revised.volume.checked_sub(&filled)?,
                    &price,
                )?)?;
        } else {
            revised.maximum_spend = revised.maximum_spend.checked_add(&historical_volume)?;
        }
        if revised.buy {
            revised.quoted_fee_notional = revised.maximum_spend.clone();
        }
        revised.volume = revised.volume.checked_add(&historical_volume)?;
        revised.order_ids = old.order_ids.clone();
        if control
            .maximum_spend
            .as_deref()
            .map(|v| Quantity::from_decimal(v, 36))
            .transpose()?
            .is_some_and(|cap| revised.maximum_spend > cap)
        {
            return Err(error(
                "KRAKEN_AMENDMENT_PRINCIPAL_BOUND",
                "Full predecessor and successor principal exceeds declared maximumSpend.",
            ));
        }
        let make_intent = |order: &SpotOrder, c: &TradingControls| -> Result<FinancialIntent> {
            let source = if order.buy {
                order.quote.clone()
            } else {
                order.base.clone()
            };
            let output = if order.buy {
                order.base.clone()
            } else {
                order.quote.clone()
            };
            let mut i = FinancialIntent {
                locks: vec![account.resource.clone()],
                debits: vec![ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset: source.clone(),
                        atoms: order.maximum_spend.clone(),
                    },
                }],
                ..Default::default()
            };
            i.expected.push(EffectConstraint::Conversion {
                resource: account.resource.clone(),
                input: source.clone(),
                maximum_input: order.maximum_spend.clone(),
                output: output.clone(),
                minimum_output: c
                    .minimum_receive
                    .as_deref()
                    .map(|v| Quantity::from_decimal(v, 36))
                    .transpose()?
                    .unwrap_or_default(),
            });
            for a in [&order.base, &order.quote] {
                let cap = c.maximum_fees.get(&a.reference).ok_or_else(|| {
                    error(
                        "KRAKEN_FEE_BOUNDS_REQUIRED",
                        "Both exact fee currencies must remain declared in amended scope.",
                    )
                })?;
                i.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: account.resource.clone(),
                    },
                    amount: Amount {
                        asset: a.clone(),
                        atoms: Quantity::from_decimal(cap, 36)?,
                    },
                    treatment: if *a == source {
                        CostTreatment::AdditionalDebit
                    } else {
                        CostTreatment::DeductedFromCredit
                    },
                });
            }
            if c.maximum_fees.len() != 2 {
                return Err(error(
                    "INVALID_INPUT",
                    "Amendment fees must name exactly the original traded assets.",
                ));
            }
            Ok(i)
        };
        let prior = previous
            .map(|s| Ok(s.intent.clone()))
            .unwrap_or_else(|| make_intent(old, &old_controls))?;
        let full = make_intent(&revised, control)?;
        let mut intent = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for (new, old) in full.debits.iter().zip(&prior.debits) {
            if new.amount.atoms > old.amount.atoms {
                let mut d = new.clone();
                d.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                intent.debits.push(d);
            }
        }
        for new in &full.maximum_costs {
            let old = prior
                .maximum_costs
                .iter()
                .find(|c| {
                    c.location == new.location
                        && c.amount.asset == new.amount.asset
                        && c.treatment == new.treatment
                })
                .unwrap();
            if new.amount.atoms > old.amount.atoms {
                let mut c = new.clone();
                c.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                intent.maximum_costs.push(c);
            }
        }
        self.verify_spot_fee_admission(account, std::slice::from_ref(&revised), control)
            .await?;
        let balance = self
            .json(
                KrakenEndpoint::KrakenPrivatePostBalanceEx,
                Some(account),
                json!({}),
            )
            .await?;
        let mut increases: BTreeMap<AssetId, Quantity> = BTreeMap::new();
        for d in &intent.debits {
            let total = increases.entry(d.amount.asset.clone()).or_default();
            *total = total.checked_add(&d.amount.atoms)?;
        }
        for c in intent
            .maximum_costs
            .iter()
            .filter(|c| c.treatment == CostTreatment::AdditionalDebit)
        {
            let total = increases.entry(c.amount.asset.clone()).or_default();
            *total = total.checked_add(&c.amount.atoms)?;
        }
        for (asset, amount) in increases {
            let entry = &result(&balance)[&asset.reference];
            if !spot_credit_used(entry)?.is_zero()
                || atoms(&entry["balance"])?.checked_sub(&atoms(&entry["hold_trade"])?)? < amount
            {
                return Err(error(
                    "INSUFFICIENT_BALANCE",
                    "Free unborrowed spot cash cannot cover the additional amended principal and fee reservations.",
                ));
            }
        }
        let full_valuation = self
            .financial_valuation(&full.debits, &full.maximum_costs)
            .await?;
        let mut revised_binding = b.clone();
        revised_binding["orders"] = json!([revised]);
        revised_binding["parameters"] = replacement;
        revised_binding["controls"] = json!(control);
        revised_binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::value_bounds::KEY);
        let scope = gofin_domain::amendments::AmendmentScope {
            intent: full,
            binding: revised_binding,
            valuation: Some(full_valuation),
        };
        intent.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: operation_id.into(),
            submission: original.clone(),
            revision: Box::new(scope),
        });
        let valuation = self
            .financial_valuation(&intent.debits, &intent.maximum_costs)
            .await?;
        let mut prepared = PreparedOperation {
            operation: KrakenEndpoint::KrakenPrivatePostAmendOrder
                .operation()
                .into(),
            input: input.clone(),
            binding: json!({"endpoint":KrakenEndpoint::KrakenPrivatePostAmendOrder.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":wire,"requestId":control.request_id,"amendmentPreviousScope":previous,"amendment":{"operationId":operation_id,"originalSubmission":original,"orderId":id,"cancelRemainder":cancel_remainder,"filledQuantityAtApproval":filled}}),
            summary: format!(
                "Amend the exact original Kraken order {id} on {}. Existing fills retain their original accounting; reserve only increased principal and fees.",
                account.name
            ),
            checks: vec![],
            warnings: vec![],
            observed_at: gofin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            action_deadline: None,
            operation_version: 1,
        };
        if cancel_remainder {
            prepared.summary = format!(
                "Amend original Kraken order {id} below or to its filled quantity; Kraken cancels its remaining quantity. Existing fills retain their original accounting."
            );
        }
        if prepared
            .valuation
            .as_ref()
            .is_some_and(|v| !v.prices.is_empty())
        {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }
    async fn reconcile_spot_amendment(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        b: &Value,
    ) -> Result<Reconciliation> {
        let account =
            self.account(b["account"].as_str().ok_or_else(|| {
                error("KRAKEN_SUBMISSION_INVALID", "Amendment account missing.")
            })?)?;
        let receipt = record
            .dispatch_evidence
            .as_ref()
            .and_then(|v| v.get("response"));
        let Some(amend_id) = receipt.and_then(|r| result(r)["amend_id"].as_str()) else {
            return Ok(unresolved(
                Status::Unknown,
                "Atomic amend response was lost or lacks an amend identity; no mutation is replayed.",
                json!({}),
            ));
        };
        let audit = self
            .json(
                KrakenEndpoint::KrakenPrivatePostOrderAmends,
                Some(account),
                json!({"order_id":b["amendment"]["orderId"]}),
            )
            .await?;
        let matches = result(&audit)["amends"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_PROOF_PENDING",
                    "Exact amendment audit trail unavailable.",
                )
            })?
            .iter()
            .filter(|a| a["amend_id"] == amend_id)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Ok(unresolved(
                Status::Pending,
                "Acknowledged amend identity is not uniquely visible in the original order audit trail.",
                audit,
            ));
        }
        let observed = matches[0];
        let mut cancellation_readback = Value::Null;
        if let Some(requested) = b["parameters"].get("order_qty")
            && (b["amendment"]["cancelRemainder"] == true
                || b["amendment"]["clampToFilled"].is_object()
                || atoms(requested)? != atoms(&observed["order_qty"])?)
        {
            let query = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostQueryOrders,
                    Some(account),
                    json!({"txid":b["amendment"]["orderId"],"trades":true}),
                )
                .await?;
            let state = &result(&query)[b["amendment"]["orderId"].as_str().unwrap()];
            if !matches!(state["status"].as_str(), Some("canceled" | "closed")) {
                return Ok(unresolved(
                    Status::Pending,
                    "Below-fill amendment requires exact cancelled remainder evidence; acknowledgment alone cannot release the original order.",
                    json!({"amendId":amend_id}),
                ));
            }
            let actual = atoms(&state["vol"])?;
            let executed = atoms(&state["vol_exec"])?;
            let approved_fill: Quantity = serde_json::from_value(
                b["amendment"]
                    .get("filledQuantityAtApproval")
                    .unwrap_or(&b["amendment"]["clampToFilled"]["observedFilledQuantity"])
                    .clone(),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original fill quantity missing.",
                )
            })?;
            if actual != executed
                || executed < approved_fill
                || executed < atoms(requested)?
                || (atoms(&observed["order_qty"])? != atoms(requested)?
                    && atoms(&observed["order_qty"])? != executed)
                || observed
                    .get("remaining_qty")
                    .map(atoms)
                    .transpose()?
                    .is_some_and(|q| !q.is_zero())
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                    "Native below-fill restatement does not prove the exact filled quantity and zero remaining order.",
                ));
            }
            cancellation_readback = state.clone();
        }
        for field in ["order_qty", "display_qty", "limit_price", "trigger_price"] {
            if let Some(requested) = b["parameters"].get(field)
                && !(field == "order_qty" && cancellation_readback.is_object())
                && atoms(requested)? != atoms(&observed[field])?
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_PROOF_MISMATCH",
                    "Original order audit does not match an exact approved amendment field.",
                ));
            }
        }
        proof(
            submission,
            Status::Completed,
            vec![FinancialEffect::SubmissionAmendment {
                operation_id: b["amendment"]["operationId"].as_str().unwrap().into(),
                submission: serde_json::from_value(b["amendment"]["originalSubmission"].clone())
                    .map_err(|_| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Original amendment submission invalid.",
                        )
                    })?,
                successor: None,
            }],
            json!({"amendId":amend_id,"orderId":b["amendment"]["orderId"],"audit":observed,"cancelledRemainder":cancellation_readback}),
        )
    }
    pub(crate) async fn verify_spot_fee_admission(
        &self,
        account: &KrakenAccount,
        orders: &[SpotOrder],
        controls: &TradingControls,
    ) -> Result<()> {
        let mut needed: BTreeMap<String, Quantity> = BTreeMap::new();
        let mut components: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for order in orders {
            let response = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostTradeVolume,
                    Some(account),
                    json!({"pair":order.pair,"fee_schedule":true}),
                )
                .await?;
            let fees = result(&response);
            let mut rate = Quantity::zero();
            for field in ["fees", "fees_maker"] {
                if let Some(value) = fees[field][&order.pair].get("fee") {
                    let exact = crate::observation::exact(value)?;
                    if !exact.starts_with('-') {
                        rate = rate.max(Quantity::from_decimal(&exact, 36)?);
                    }
                }
            }
            if fees["fees"][&order.pair]["fee"].is_null() {
                return Err(error(
                    "KRAKEN_FEE_PROOF_PENDING",
                    "Current protected fee service omitted the exact traded market taker rate.",
                ));
            }
            let divisor = Quantity::new(format!("100{}", "0".repeat(36)))?;
            // Additional source charges are finite under the native principal
            // bound. Output deductions are reconciled from actual ledger legs.
            for (code, principal) in [
                (!order.quoted_volume).then_some((&order.base.reference, &order.volume)),
                Some((&order.quote.reference, &order.quoted_fee_notional)),
            ]
            .into_iter()
            .flatten()
            {
                let amount = principal.mul_div_ceil(&rate, &divisor)?;
                let total = needed.entry(code.clone()).or_default();
                *total = total.checked_add(&amount)?;
                components.entry(code.clone()).or_default().push(json!({
                    "pair":order.pair,
                    "rate_percent":rate.to_decimal(36),
                    "basis_amount":principal.to_decimal(36),
                    "required_fee":amount.to_decimal(36)
                }));
            }
        }
        for (code, amount) in needed {
            let cap = controls.maximum_fees.get(&code).ok_or_else(|| {
                error(
                    "KRAKEN_FEE_BOUNDS_REQUIRED",
                    "Exact traded fee asset ceiling missing.",
                )
            })?;
            if amount > Quantity::from_decimal(cap, 36)? {
                let required = amount.to_decimal(36);
                let fee_components = &components[&code];
                let rates = fee_components
                    .iter()
                    .map(|c| {
                        format!(
                            "{}% on {}",
                            c["rate_percent"].as_str().unwrap(),
                            c["pair"].as_str().unwrap()
                        )
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(gofin_domain::GoFinError::new(
                    "KRAKEN_FEE_BOUND_EXCEEDED",
                    "kraken",
                    format!("Kraken requires a fee cap of at least {required} {code} at current rates ({rates}); the supplied cap is {cap} {code}."),
                    "Review maximumFees for the failing asset and any affected net-receipt target before approval. Kraken fee-currency flags are preferences, so both traded assets need sufficient caps. Existing authority cannot be widened automatically.",
                ).with_details(json!({
                    "asset":code,
                    "required_fee":required,
                    "supplied_cap":cap,
                    "fee_components":fee_components,
                    "fee_currency_preference_is_guarantee":false
                })));
            }
        }
        Ok(())
    }
    async fn spot_market(
        &self,
        account: &KrakenAccount,
        pair: &str,
    ) -> Result<(String, Value, AssetId, AssetId)> {
        let markets = self
            .json(
                KrakenEndpoint::KrakenPublicGetAssetPairs,
                None,
                json!({"pair":pair}),
            )
            .await?;
        let records = result(&markets).as_object().ok_or_else(|| {
            error(
                "KRAKEN_MARKET_UNKNOWN",
                "Kraken pair metadata is unavailable.",
            )
        })?;
        if records.len() != 1 {
            return Err(error(
                "KRAKEN_MARKET_AMBIGUOUS",
                "Select one exact Kraken pair identity.",
            ));
        }
        let (key, market) = records.iter().next().unwrap();
        if key != pair && market["altname"] != pair && market["wsname"] != pair {
            return Err(error(
                "KRAKEN_MARKET_IDENTITY_CHANGED",
                "Kraken returned a different pair identity.",
            ));
        }
        if market["status"] != "online" {
            return Err(error(
                "KRAKEN_MARKET_UNAVAILABLE",
                "This exact Kraken pair is not online.",
            ));
        }
        let base = market["base"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_MARKET_UNKNOWN",
                "Kraken omitted the exact base identity.",
            )
        })?;
        let quote = market["quote"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_MARKET_UNKNOWN",
                "Kraken omitted the exact quote identity.",
            )
        })?;
        Ok((
            key.clone(),
            market.clone(),
            self.spot_asset(account, base).await?,
            self.spot_asset(account, quote).await?,
        ))
    }
    pub(crate) async fn prepare_spot_order(
        &self,
        account: &KrakenAccount,
        parameters: &mut Value,
        controls: &TradingControls,
        index: usize,
        cancelling_remainder: bool,
    ) -> Result<SpotOrder> {
        if parameters
            .get("leverage")
            .is_some_and(|v| v.as_str().is_none_or(|s| s != "none" && s != "1"))
        {
            return Err(error(
                "KRAKEN_MARGIN_EVIDENCE_PENDING",
                "Margin orders require verified borrowing, rollover-cost and liability settlement; the spot cash path cannot authorize margin exposure.",
            ));
        }
        let pair = parameters["pair"]
            .as_str()
            .ok_or_else(|| error("INVALID_INPUT", "Exact pair missing."))?;
        let (pair, market, base, quote) = self.spot_market(account, pair).await?;
        let buy = match parameters["type"].as_str() {
            Some("buy") => true,
            Some("sell") => false,
            _ => return Err(error("INVALID_INPUT", "Order side must be buy or sell.")),
        };
        let volume = if cancelling_remainder {
            atoms(&parameters["volume"])?
        } else {
            nonzero(&parameters["volume"])?
        };
        let flags = parameters["oflags"].as_str().unwrap_or("");
        let quoted_volume = flags.split(',').any(|f| f == "viqc");
        if quoted_volume && (!buy || parameters["ordertype"] != "market") {
            return Err(error(
                "INVALID_INPUT",
                "Quote-volume viqc is permitted only for a market buy.",
            ));
        }
        let precision = market[if quoted_volume {
            "cost_decimals"
        } else {
            "lot_decimals"
        }]
        .as_u64()
        .filter(|n| *n <= 36)
        .ok_or_else(|| {
            error(
                "KRAKEN_MARKET_PRECISION_UNKNOWN",
                "Exact lot/cost precision is missing.",
            )
        })?;
        let quantum = Quantity::new(format!("1{}", "0".repeat(36 - precision as usize)))?;
        if &volume.integer() % quantum.integer() != num_bigint::BigUint::from(0u8) {
            return Err(error(
                "INVALID_INPUT",
                "Order volume exceeds the observed exact Kraken market precision.",
            ));
        }
        if !cancelling_remainder
            && !quoted_volume
            && market.get("ordermin").is_some()
            && volume < nonzero(&market["ordermin"])?
        {
            return Err(error(
                "INVALID_INPUT",
                "Order volume is below Kraken's observed minimum.",
            ));
        }
        if buy
            && !quoted_volume
            && parameters["ordertype"] == "market"
            && let Some(cap) = &controls.maximum_spend
        {
            let price = quote_price_collar(&market, cap, &volume)?;
            if !matches!(
                parameters["timeinforce"].as_str(),
                None | Some("IOC" | "FOK")
            ) {
                return Err(error(
                    "KRAKEN_MARKET_COLLAR_CONFLICT",
                    "A capped market buy uses IOC or FOK to avoid leaving a resting order. Review the explicit time-in-force and quote ceiling.",
                ));
            }
            parameters["ordertype"] = json!("limit");
            parameters["price"] = json!(price.to_decimal(36));
            if parameters["timeinforce"].is_null() {
                parameters["timeinforce"] = json!("IOC");
            }
        }
        if buy
            && !quoted_volume
            && matches!(
                parameters["ordertype"].as_str(),
                Some("stop-loss" | "take-profit" | "trailing-stop")
            )
            && let Some(cap) = &controls.maximum_spend
        {
            let price = quote_price_collar(&market, cap, &volume)?;
            let kind = parameters["ordertype"].as_str().unwrap();
            parameters["ordertype"] = json!(format!("{kind}-limit"));
            parameters["price2"] = json!(price.to_decimal(36));
        }
        let maximum_spend = if !buy || quoted_volume {
            volume.clone()
        } else {
            let price_field = match parameters["ordertype"].as_str() {
                Some("limit" | "iceberg") => "price",
                Some("stop-loss-limit" | "take-profit-limit" | "trailing-stop-limit") => "price2",
                _ => {
                    return Err(error(
                        "KRAKEN_UNBOUNDED_BUY",
                        "This base-volume buy has no enforceable absolute quote-price ceiling. Use an absolute limit or an explicit market quote-volume (viqc) request.",
                    ));
                }
            };
            let price = nonzero(&parameters[price_field])?;
            decimal_product(&volume, &price)?
        };
        if let Some(cap) = &controls.maximum_spend
            && maximum_spend > Quantity::from_decimal(cap, 36)?
        {
            return Err(error(
                "KRAKEN_SPEND_BOUND_EXCEEDED",
                "The exact order principal exceeds maximumSpend.",
            ));
        }
        parameters["leverage"] = json!("none");
        let quoted_fee_notional = if buy {
            maximum_spend.clone()
        } else {
            let ticker = self
                .json(
                    KrakenEndpoint::KrakenPublicGetTicker,
                    None,
                    json!({"pair":pair}),
                )
                .await?;
            let record = result(&ticker).get(&pair).ok_or_else(|| {
                error(
                    "KRAKEN_FEE_PRICE_PENDING",
                    "Current quote fee estimate requires the exact reviewed market ticker.",
                )
            })?;
            let mut reference = nonzero(&record["b"][0])?;
            for key in ["price", "price2"] {
                if let Some(v) = parameters.get(key)
                    && let Ok(limit) = atoms(v)
                {
                    reference = reference.max(limit);
                }
            }
            decimal_product(&volume, &reference)?
        };
        let id = parameters["cl_ord_id"].as_str().unwrap_or("");
        let id = if id.is_empty() {
            client_id(account, &controls.request_id, index)
        } else {
            id.into()
        };
        let uuid = (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            || (id.len() == 36
                && id.split('-').map(|s| s.len()).eq([8, 4, 4, 4, 12])
                && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
        if !uuid && (id.len() > 18 || !id.bytes().all(|b| (32..=126).contains(&b))) {
            return Err(error(
                "INVALID_INPUT",
                "Use a Kraken UUID client order ID or at most 18 letters, digits, hyphens or underscores.",
            ));
        }
        let user_reference = parameters.get("userref").cloned();
        if user_reference.is_some() && parameters.get("cl_ord_id").is_some() {
            return Err(error(
                "INVALID_INPUT",
                "Kraken userref and cl_ord_id are mutually exclusive.",
            ));
        }
        if user_reference.is_none() {
            parameters["cl_ord_id"] = json!(id);
        }
        parameters["pair"] = json!(pair);
        Ok(SpotOrder {
            client_id: id,
            pair: pair.clone(),
            base,
            quote,
            buy,
            volume,
            maximum_spend,
            quoted_volume,
            display_pair: market["altname"].as_str().unwrap_or(&pair).into(),
            order_ids: vec![],
            quoted_fee_notional,
            user_reference,
            uncreated_child: false,
        })
    }
    pub(crate) async fn prepare_trading(
        &self,
        endpoint: KrakenEndpoint,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let controls = controls(input)?;
        let (selection, mut parameters) = contracts::decode(endpoint, input)?;
        let account =
            self.account(selection.account.as_deref().ok_or_else(|| {
                error("INVALID_INPUT", "Kraken writes require an exact account.")
            })?)?;
        if endpoint.futures() {
            return self.prepare_derivatives(endpoint, input).await;
        }
        self.spot_permissions(
            account,
            if matches!(
                endpoint,
                KrakenEndpoint::KrakenPrivatePostAddOrder
                    | KrakenEndpoint::KrakenPrivatePostAddOrderBatch
                    | KrakenEndpoint::KrakenPrivatePostAmendOrder
                    | KrakenEndpoint::KrakenPrivatePostEditOrder
            ) {
                &[
                    "query-funds",
                    "query-open-trades",
                    "query-closed-trades",
                    "query-ledger",
                    "modify-trades",
                ]
            } else {
                &["query-open-trades", "query-closed-trades", "close-trades"]
            },
        )
        .await?;
        if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
            self.spot_permissions(account, &["close-trades"]).await?;
        }
        if parameters["validate"] == true {
            return Ok(PreparedOperation {
                operation: endpoint.operation().into(),
                input: input.clone(),
                binding: json!({"endpoint":endpoint.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":parameters,"requestId":controls.request_id,"validationOnly":true}),
                summary: format!(
                    "Ask Kraken to validate {} without placing or editing an order.",
                    endpoint.operation()
                ),
                checks: vec![],
                warnings: vec![],
                observed_at: gofin_domain::now(),
                expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
                valuation: Some(zero_authority_valuation(endpoint)),
                intent: FinancialIntent {
                    expected: vec![EffectConstraint::Authority {
                        resource: account.resource.clone(),
                        authority: format!("order:validation:{}", controls.request_id),
                        before: "unvalidated".into(),
                        after: "valid".into(),
                    }],
                    ..Default::default()
                },
                action_deadline: None,
                operation_version: 1,
            });
        }
        if !matches!(
            endpoint,
            KrakenEndpoint::KrakenPrivatePostAddOrder
                | KrakenEndpoint::KrakenPrivatePostAddOrderBatch
        ) {
            return self
                .prepare_order_control(endpoint, input, account, parameters, &controls)
                .await;
        }
        if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrderBatch
            && crate::mixed_spot_margin::mixed(&parameters)
        {
            return Box::pin(self.prepare_mixed_spot_margin(input, account, parameters, &controls))
                .await;
        }
        let margin = |p: &Value| {
            p["leverage"]
                .as_str()
                .is_some_and(|v| v != "none" && v != "1")
                || p["ordertype"] == "settle-position"
        };
        if margin(&parameters)
            || parameters["orders"]
                .as_array()
                .is_some_and(|orders| orders.iter().any(margin))
        {
            return self
                .prepare_margin(endpoint, input, account, parameters, &controls)
                .await;
        }
        let mut orders = vec![];
        if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrderBatch {
            let pair = parameters["pair"].clone();
            let batch = parameters["orders"]
                .as_array_mut()
                .ok_or_else(|| error("INVALID_INPUT", "Order batch missing."))?;
            if controls.maximum_spend.is_some()
                && batch
                    .iter()
                    .map(|item| item["type"].as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
                    != 1
            {
                return Err(contracts::unsupported_control(
                    endpoint,
                    vec!["/maximumSpend".into()],
                ));
            }
            let collar = |item: &Value| {
                item["type"] == "buy"
                    && !item["oflags"]
                        .as_str()
                        .unwrap_or("")
                        .split(',')
                        .any(|flag| flag == "viqc")
                    && matches!(
                        item["ordertype"].as_str(),
                        Some("market" | "stop-loss" | "take-profit" | "trailing-stop")
                    )
            };
            let collared = batch.iter().filter(|item| collar(item)).count();
            let mut prepared = vec![None; batch.len()];
            let mut fixed_spend = Quantity::zero();
            for (i, item) in batch
                .iter_mut()
                .enumerate()
                .filter(|(_, item)| collared == 0 || !collar(item))
            {
                item["pair"] = pair.clone();
                let order = self
                    .prepare_spot_order(account, item, &controls, i, false)
                    .await?;
                fixed_spend = fixed_spend.checked_add(&order.maximum_spend)?;
                prepared[i] = Some(order);
                item.as_object_mut().unwrap().remove("pair");
            }
            if collared > 0 {
                let cap=controls.maximum_spend.as_deref().ok_or_else(||error("KRAKEN_SPEND_BOUND_REQUIRED","Base-volume market and trigger buys require a whole-batch maximumSpend quote ceiling."))?;
                let left = Quantity::from_decimal(cap, 36)?
                    .checked_sub(&fixed_spend)
                    .map_err(|_| {
                        error(
                            "KRAKEN_SPEND_BOUND_EXCEEDED",
                            "Fixed native orders already exceed the whole-batch maximumSpend.",
                        )
                    })?;
                let share = left
                    .mul_div_floor(&Quantity::from_u64(1), &Quantity::from_u64(collared as u64))?;
                let mut allocated = controls.clone();
                allocated.maximum_spend = Some(share.to_decimal(36));
                for (i, item) in batch
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, item)| collar(item))
                {
                    item["pair"] = pair.clone();
                    prepared[i] = Some(
                        self.prepare_spot_order(account, item, &allocated, i, false)
                            .await?,
                    );
                    item.as_object_mut().unwrap().remove("pair");
                }
            }
            orders = prepared
                .into_iter()
                .map(|order| order.expect("every native batch member prepared"))
                .collect();
        } else {
            orders.push(
                self.prepare_spot_order(account, &mut parameters, &controls, 0, false)
                    .await?,
            );
        }
        let root_count = orders.len();
        if let Some(cap) = controls.maximum_spend.as_deref() {
            let mut sources = BTreeMap::<AssetId, Quantity>::new();
            for order in &orders {
                let asset = if order.buy { &order.quote } else { &order.base };
                let total = sources.entry(asset.clone()).or_default();
                *total = total.checked_add(&order.maximum_spend)?;
            }
            if sources.len() != 1 {
                return Err(contracts::unsupported_control(
                    endpoint,
                    vec!["/maximumSpend".into()],
                ));
            }
            if sources.into_values().next().unwrap() > Quantity::from_decimal(cap, 36)? {
                return Err(error(
                    "KRAKEN_SPEND_BOUND_EXCEEDED",
                    "The whole cash batch exceeds maximumSpend in its exact source asset.",
                ));
            }
        }
        let mut conditionals = vec![];
        for index in 0..root_count {
            let native = if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrderBatch {
                parameters["orders"][index].clone()
            } else {
                parameters.clone()
            };
            let close = if native["close"].is_object() {
                Some(native["close"].clone())
            } else {
                native.get("close[ordertype]").map(|kind|json!({"ordertype":kind,"price":native["close[price]"],"price2":native["close[price2]"]}))
            };
            if let Some(mut child) = close {
                if orders[index].quoted_volume {
                    return Err(error(
                        "KRAKEN_CONDITIONAL_QUOTE_VOLUME_POLICY",
                        "Quote-volume entries do not bound generated child base quantity; select an exact base-volume entry for a finite child scope.",
                    ));
                }
                child.as_object_mut().unwrap().retain(|_, v| !v.is_null());
                child["pair"] = json!(orders[index].pair);
                child["type"] = json!(if orders[index].buy { "sell" } else { "buy" });
                child["volume"] = json!(orders[index].volume.to_decimal(36));
                child["leverage"] = json!("none");
                let mut child_controls = controls.clone();
                child_controls.maximum_spend = None;
                child_controls.minimum_receive = None;
                if !orders[index].buy {
                    let price_field = match native["ordertype"].as_str() {
                        Some("limit" | "iceberg") => "price",
                        Some("stop-loss-limit" | "take-profit-limit" | "trailing-stop-limit") => {
                            "price2"
                        }
                        _ => {
                            return Err(error(
                                "KRAKEN_CONDITIONAL_SOURCE_CREDIT_UNBOUNDED",
                                "A generated cash buy needs an absolute native entry sell price floor. Use a limit or trigger-limit entry; existing quote holdings cannot finance its principal.",
                            ));
                        }
                    };
                    let floor = nonzero(&native[price_field])?;
                    let cap = decimal_product(&orders[index].volume, &floor)?;
                    child_controls.maximum_spend = Some(cap.to_decimal(36));
                    match child["ordertype"].as_str() {
                        Some("market") => {
                            child["ordertype"] = json!("limit");
                            child["price"] = json!(floor.to_decimal(36));
                        }
                        Some("stop-loss" | "take-profit" | "trailing-stop") => {
                            child["ordertype"] =
                                json!(format!("{}-limit", child["ordertype"].as_str().unwrap()));
                            child["price2"] = json!(floor.to_decimal(36));
                        }
                        _ => (),
                    }
                    let execution_field =
                        if matches!(child["ordertype"].as_str(), Some("limit" | "iceberg")) {
                            "price"
                        } else {
                            "price2"
                        };
                    if nonzero(&child[execution_field])? > floor {
                        return Err(error(
                            "KRAKEN_CONDITIONAL_SOURCE_CREDIT_EXCEEDED",
                            "The generated cash buy execution ceiling exceeds the entry sell's guaranteed per-unit credit. Lower the close limit or raise the native entry price floor.",
                        ));
                    }
                    let target = if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrderBatch {
                        &mut parameters["orders"][index]
                    } else {
                        &mut parameters
                    };
                    if target["close"].is_object() {
                        target["close"] = json!({"ordertype":child["ordertype"]});
                        for field in ["price", "price2"] {
                            if let Some(v) = child.get(field) {
                                target["close"][field] = v.clone();
                            }
                        }
                    } else {
                        target["close[ordertype]"] = child["ordertype"].clone();
                        for field in ["price", "price2"] {
                            if let Some(v) = child.get(field) {
                                target[format!("close[{field}]")] = v.clone();
                            }
                        }
                    }
                }
                let template = self
                    .prepare_spot_order(
                        account,
                        &mut child,
                        &child_controls,
                        root_count + index,
                        false,
                    )
                    .await?;
                conditionals.push(
                    json!({"parentIndex":index,"childIndex":orders.len(),"parameters":child}),
                );
                orders.push(template);
            }
        }
        if root_count > 1 && controls.minimum_receive.is_some() {
            return Err(error(
                "INVALID_INPUT",
                "Batch minimumReceive must be represented by per-order target controls; the single-order receipt control is not silently applied to a batch.",
            ));
        }
        let mut ids = BTreeSet::new();
        if !orders.iter().all(|o| ids.insert(o.client_id.clone())) {
            return Err(error(
                "INVALID_INPUT",
                "Every order in a batch requires a distinct client identity.",
            ));
        }
        let mut intent = FinancialIntent {
            locks: vec![account.resource.clone()],
            ..Default::default()
        };
        for (index, order) in orders.iter().enumerate() {
            let input_asset = if order.buy {
                order.quote.clone()
            } else {
                order.base.clone()
            };
            let output_asset = if order.buy {
                order.base.clone()
            } else {
                order.quote.clone()
            };
            intent.debits.push(ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: input_asset.clone(),
                    atoms: order.maximum_spend.clone(),
                },
            });
            intent.expected.push(EffectConstraint::Conversion {
                resource: account.resource.clone(),
                input: input_asset,
                maximum_input: order.maximum_spend.clone(),
                output: output_asset,
                minimum_output: if root_count == 1 && index == 0 {
                    controls
                        .minimum_receive
                        .as_deref()
                        .map(|s| Quantity::from_decimal(s, 36))
                        .transpose()?
                        .unwrap_or_default()
                } else {
                    Quantity::zero()
                },
            });
        }
        for (code, cap) in &controls.maximum_fees {
            let asset = self.spot_asset(account, code).await?;
            let additional = orders.iter().any(|o| {
                if o.buy {
                    asset == o.quote
                } else {
                    asset == o.base
                }
            });
            let deducted = orders.iter().any(|o| {
                if o.buy {
                    asset == o.base
                } else {
                    asset == o.quote
                }
            });
            for treatment in [
                additional.then_some(CostTreatment::AdditionalDebit),
                deducted.then_some(CostTreatment::DeductedFromCredit),
            ]
            .into_iter()
            .flatten()
            {
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: account.resource.clone(),
                    },
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: Quantity::from_decimal(cap, 36)?,
                    },
                    treatment,
                });
            }
            if !additional && !deducted {
                return Err(error(
                    "INVALID_INPUT",
                    "maximumFees names an asset outside the exact traded pairs.",
                ));
            }
        }
        if intent.maximum_costs.is_empty() {
            return Err(error(
                "KRAKEN_FEE_BOUNDS_REQUIRED",
                "Declare maximumFees for all possible Kraken fee assets.",
            ));
        }
        for o in &orders {
            for code in [&o.base.reference, &o.quote.reference] {
                if !controls.maximum_fees.contains_key(code) {
                    return Err(error(
                        "KRAKEN_FEE_BOUNDS_REQUIRED",
                        "Kraken fee-currency preferences are not guarantees; declare both traded assets in maximumFees.",
                    ));
                }
            }
        }
        let gross_principal = intent.debits.clone();
        let sequences = crate::conditional_sequences::prepare(
            &account.resource,
            &orders,
            &conditionals,
            &controls,
            &mut intent,
        )?;
        self.verify_spot_fee_admission(account, &orders, &controls)
            .await?;
        let balances = self
            .json(
                KrakenEndpoint::KrakenPrivatePostBalanceEx,
                Some(account),
                json!({}),
            )
            .await?;
        let mut required: BTreeMap<AssetId, Quantity> = BTreeMap::new();
        for d in &intent.debits {
            let total = required.entry(d.amount.asset.clone()).or_default();
            *total = total.checked_add(&d.amount.atoms)?;
        }
        for c in intent
            .maximum_costs
            .iter()
            .filter(|c| c.treatment == CostTreatment::AdditionalDebit)
        {
            let total = required.entry(c.amount.asset.clone()).or_default();
            *total = total.checked_add(&c.amount.atoms)?;
        }
        for (asset, q) in required {
            let entry = &result(&balances)[&asset.reference];
            let free = atoms(&entry["balance"])?.checked_sub(&atoms(&entry["hold_trade"])?)?;
            if spot_credit_used(entry)? > Quantity::zero() {
                return Err(error(
                    "KRAKEN_MARGIN_LIABILITY_PRESENT",
                    "Spot cash execution requires separately verified existing credit liabilities.",
                ));
            }
            if q > free {
                return Err(error(
                    "INSUFFICIENT_BALANCE",
                    "Existing unborrowed Kraken custody does not cover principal and all additional fee reservations.",
                ));
            }
        }
        let valuation = self
            .financial_valuation(&gross_principal, &intent.maximum_costs)
            .await?;
        let mut prepared=PreparedOperation{operation:endpoint.operation().into(),input:input.clone(),binding:json!({"endpoint":endpoint.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":parameters,"requestId":controls.request_id,"orders":orders,"controls":controls,"conditionalClose":conditionals,"rootOrderCount":root_count,"conversionSequences":sequences}),summary:format!("Place {} exact Kraken spot order(s) on {}. Reserve full principal and declared fees until matching terminal trade evidence.",root_count,account.name),checks:vec![],warnings:vec!["Standing orders remain live at Kraken until filled, canceled or expired; revoking GoFin authorization does not cancel them.".into(),"Kraken does not offer a hard trading-fee ceiling. GoFin checks current venue rates and reserves your exact fee caps; an unexpected excess retains receipts and unresolved reservations for recovery.".into()],observed_at:gofin_domain::now(),expires_at:(Utc::now()+Duration::seconds(90)).to_rfc3339(),valuation:Some(valuation),intent,action_deadline: None, operation_version:1};
        if !conditionals.is_empty() {
            prepared.summary.push_str(" Generated close orders are included in this approval and retain the entry's settlement owner.");
            if orders.iter().take(root_count).any(|o| !o.buy) {
                prepared.warnings.push("Generated cash buys are bounded by the entry sell native price floor. Requested market or trigger-market closes become standing limit or trigger-limit closes at that exact ceiling; they may remain open or receive no fill. Only correlated entry credit funds their principal.".into());
            }
            prepared.warnings.push("Conditional close orders persist at Kraken and may activate after an entry fill. Child principal uses independently proved entry credit. Exact fee caps in both traded assets are reserved separately from existing custody; no unrelated holdings can finance child principal. All generated child identities and terminal trade receipts are required before completion.".into());
        }
        let collared = if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder {
            input["ordertype"] == "market" && prepared.binding["parameters"]["ordertype"] == "limit"
        } else {
            input["orders"].as_array().is_some_and(|rows| {
                rows.iter()
                    .zip(prepared.binding["parameters"]["orders"].as_array().unwrap())
                    .any(|(requested, wire)| {
                        requested["ordertype"] == "market" && wire["ordertype"] == "limit"
                    })
            })
        };
        if collared {
            prepared.warnings.push("The requested base-volume market buy is submitted as a native IOC/FOK limit with a rounded-down quote price collar derived from maximumSpend. It may fill partially or receive no fill; it leaves no resting limit order.".into());
        }
        let triggered_collar = |requested: &Value, wire: &Value| {
            requested["type"] == "buy"
                && matches!(
                    requested["ordertype"].as_str(),
                    Some("stop-loss" | "take-profit" | "trailing-stop")
                )
                && wire["ordertype"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with("-limit"))
        };
        let triggered = if endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder {
            triggered_collar(input, &prepared.binding["parameters"])
        } else {
            input["orders"].as_array().is_some_and(|rows| {
                rows.iter()
                    .zip(prepared.binding["parameters"]["orders"].as_array().unwrap())
                    .any(|(requested, wire)| triggered_collar(requested, wire))
            })
        };
        if triggered {
            prepared.warnings.push("The requested triggered base-volume buy becomes the corresponding native trigger-limit order with an absolute rounded-down price2 collar from maximumSpend. Trigger/trailing parameters remain exact; after activation it may remain open, fill partially or receive no fill.".into());
        }
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn prepare_order_control(
        &self,
        endpoint: KrakenEndpoint,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
    ) -> Result<PreparedOperation> {
        self.prepare_order_control_with_scope(endpoint, input, account, parameters, controls, None)
            .await
    }
    pub(crate) async fn prepare_order_control_with_scope(
        &self,
        endpoint: KrakenEndpoint,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        approved_scope: Option<&Value>,
    ) -> Result<PreparedOperation> {
        contracts::validate_fin_controls(endpoint, input, approved_scope)?;
        if endpoint == KrakenEndpoint::KrakenPrivatePostAmendOrder {
            return self
                .prepare_spot_amendment(input, account, parameters, controls, None)
                .await;
        }
        if endpoint == KrakenEndpoint::KrakenPrivatePostEditOrder {
            return self
                .prepare_spot_legacy_edit(input, account, parameters, controls, None)
                .await;
        }
        let deadman = endpoint == KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter;
        let mut intent = FinancialIntent {
            locks: vec![account.resource.clone()],
            ..Default::default()
        };
        let original = match (&controls.operation_id, &controls.original_submission) {
            (Some(id), Some(submission)) if !id.is_empty() => {
                Some((id.clone(), submission.clone()))
            }
            (None, None) => None,
            _ => {
                return Err(error(
                    "INVALID_INPUT",
                    "GoFin order cancellation requires both operationId and the exact originalSubmission.",
                ));
            }
        };
        if let Some((_, submission)) = &original
            && (submission.kind != self.submission_kind()
                || submission.data["binding"]["account"] != account.name
                || submission.data["binding"]["physicalIdentity"] != account.physical_identity
                || (!submission.data["binding"]["orders"].is_array()
                    && !submission.data["binding"]["margin"]["orders"].is_array()))
        {
            return Err(error(
                "KRAKEN_CANCELLATION_TARGET_INVALID",
                "Original Submission does not identify this exact account/product order scope.",
            ));
        }
        let mut targets = vec![];
        if !deadman {
            let all = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostOpenOrders,
                    Some(account),
                    json!({"trades":true}),
                )
                .await?;
            let map = result(&all)["open"].as_object().ok_or_else(|| {
                error(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Open orders unavailable for cancellation scope.",
                )
            })?;
            for (id, order) in map {
                let selected = match endpoint {
                    KrakenEndpoint::KrakenPrivatePostCancelAll => true,
                    KrakenEndpoint::KrakenPrivatePostCancelOrder => {
                        parameters["txid"].as_str() == Some(id)
                            || parameters["cl_ord_id"]
                                .as_str()
                                .is_some_and(|c| order["cl_ord_id"] == c)
                            || parameters["txid"]
                                .as_i64()
                                .is_some_and(|n| order["userref"].as_i64() == Some(n))
                    }
                    KrakenEndpoint::KrakenPrivatePostCancelOrderBatch => {
                        parameters["orders"].as_array().is_some_and(|v| {
                            v.iter().any(|o| {
                                o["txid"] == *id
                                    || o["txid"]
                                        .as_i64()
                                        .is_some_and(|n| order["userref"].as_i64() == Some(n))
                            })
                        }) || parameters["cl_ord_ids"]
                            .as_array()
                            .is_some_and(|v| v.iter().any(|o| o["cl_ord_id"] == order["cl_ord_id"]))
                    }
                    _ => false,
                };
                if selected {
                    targets.push(json!({"orderId":id,"clientId":order["cl_ord_id"]}));
                    intent.expected.push(EffectConstraint::Authority {
                        resource: account.resource.clone(),
                        authority: format!("order:{id}:status"),
                        before: "open".into(),
                        after: "canceled".into(),
                    });
                }
            }
            if targets.is_empty() {
                return Err(error(
                    "KRAKEN_ORDER_NOT_OPEN",
                    "No exact currently open order matches the requested cancellation.",
                ));
            }
            if let Some((operation_id, submission)) = &original {
                let scope = approved_scope.unwrap_or(&submission.data["binding"]);
                let original_orders = scope["orders"]
                    .as_array()
                    .or_else(|| scope["margin"]["orders"].as_array())
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_CANCELLATION_TARGET_INVALID",
                            "Protected original order scope is unavailable.",
                        )
                    })?;
                let submitted = chrono::DateTime::parse_from_rfc3339(
                    submission.data["submittedAt"].as_str().unwrap_or(""),
                )
                .map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Original order submission time invalid.",
                    )
                })?
                .timestamp()
                    - 1;
                let owned = |id: &str, o: &Value| {
                    original_orders.iter().any(|owner| {
                        if owner["orderIds"]
                            .as_array()
                            .is_some_and(|ids| ids.iter().any(|v| v == id))
                        {
                            return true;
                        }
                        if owner["userReference"].is_null() {
                            return o["cl_ord_id"] == owner["clientId"];
                        }
                        o["userref"] == owner["userReference"]
                            && crate::observation::exact(&o["opentm"])
                                .ok()
                                .and_then(|v| {
                                    v.split('.').next().and_then(|s| s.parse::<i64>().ok())
                                })
                                .is_some_and(|t| t >= submitted)
                    })
                };
                if targets.iter().any(|t| {
                    t["orderId"]
                        .as_str()
                        .is_none_or(|id| map.get(id).is_none_or(|o| !owned(id, o)))
                }) {
                    return Err(error(
                        "KRAKEN_CANCELLATION_TARGET_MISMATCH",
                        "Selected cancellation includes a venue order outside the exact original GoFin submission or approved successor lineage.",
                    ));
                }
                if map
                    .iter()
                    .any(|(id, o)| owned(id, o) && !targets.iter().any(|t| t["orderId"] == *id))
                {
                    return Err(error(
                        "KRAKEN_CANCELLATION_TARGET_MISMATCH",
                        "Cancel the whole original submission remainder; partial cancellation of a batched GoFin submission needs a separately bound target scope.",
                    ));
                }
                intent.expected = vec![EffectConstraint::SubmissionCancellation {
                    operation_id: operation_id.clone(),
                    submission: submission.clone(),
                }];
            }
            // CancelAll has no identity filter at dispatch. Freeze exact target identities by
            // substituting the closed batch endpoint, preventing newly created orders being canceled.
        } else {
            intent.locks.clear();
            intent.expected.push(EffectConstraint::Authority {
                resource: account.resource.clone(),
                authority: "orders:deadman:timeout".into(),
                before: "unspecified".into(),
                after: parameters["timeout"].to_string(),
            });
        }
        Ok(PreparedOperation {
            operation: endpoint.operation().into(),
            input: input.clone(),
            binding: json!({"endpoint":endpoint.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":parameters,"requestId":controls.request_id,"targets":targets,"deadman":deadman,"originalCancellation":original,"cancellationOriginalScope":approved_scope}),
            summary: if deadman {
                format!(
                    "Set Kraken order cancellation timer to {} seconds on {}. GoFin will not renew it automatically.",
                    parameters["timeout"], account.name
                )
            } else {
                format!(
                    "Cancel {} bound open Kraken order(s) on {}. Concurrent fills retain their original order accounting.",
                    targets.len(),
                    account.name
                )
            },
            checks: vec![],
            warnings: vec![],
            observed_at: gofin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            valuation: deadman.then(|| zero_authority_valuation(endpoint)),
            intent,
            action_deadline: None,
            operation_version: 1,
        })
    }
    pub(crate) async fn reconcile_trading(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation> {
        if contracts::endpoint(&record.operation)?.futures() {
            return self.reconcile_derivatives(record, submission).await;
        }
        self.reconcile_trading_binding(record, submission, &submission.data["binding"])
            .await
    }
    pub(crate) async fn reconcile_trading_binding(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        if contracts::endpoint(&record.operation)?.futures() {
            return self
                .reconcile_derivatives_binding(record, submission, binding)
                .await;
        }
        if binding["mixedSpotMargin"].is_object() {
            return Box::pin(self.reconcile_mixed_spot_margin(record, submission, binding)).await;
        }
        if binding["margin"].is_object() {
            return self.reconcile_margin(record, submission, binding).await;
        }
        if binding["conditionalClose"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            return Box::pin(self.reconcile_conditional_close(record, submission, binding)).await;
        }
        if binding["legacyEdit"] == true {
            return self
                .reconcile_spot_legacy_edit(record, submission, binding)
                .await;
        }
        if binding["amendment"].is_object() {
            return self
                .reconcile_spot_amendment(record, submission, binding)
                .await;
        }
        let account =
            self.account(binding["account"].as_str().ok_or_else(|| {
                error("KRAKEN_SUBMISSION_INVALID", "Persisted account missing.")
            })?)?;
        if binding["physicalIdentity"] != account.physical_identity {
            return Err(error(
                "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                "Original physical Kraken account identity no longer matches.",
            ));
        }
        if binding["validationOnly"] == true {
            let Some(receipt) = record
                .dispatch_evidence
                .as_ref()
                .and_then(|v| v.get("response"))
            else {
                return Ok(unresolved(
                    Status::Unknown,
                    "Validation-only response was lost. No financial order was requested and validation is not replayed automatically.",
                    json!({}),
                ));
            };
            if receipt["error"].as_array().is_some_and(|e| !e.is_empty()) {
                return proof(
                    submission,
                    Status::Failed,
                    vec![],
                    json!({"validationOnly":true,"validation":receipt}),
                );
            }
            let r = result(receipt);
            if r.get("txid")
                .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
            {
                return Err(error(
                    "KRAKEN_VALIDATION_PROOF_MISMATCH",
                    "Validation-only response unexpectedly contains a placed order identity.",
                ));
            }
            if !r["descr"].is_object() && !r["orders"].is_array() {
                return Ok(unresolved(
                    Status::Unknown,
                    "Validation-only response lacks the native order validation description.",
                    receipt.clone(),
                ));
            }
            return proof(
                submission,
                Status::Completed,
                vec![FinancialEffect::AuthorityChange {
                    resource: account.resource.clone(),
                    authority: format!(
                        "order:validation:{}",
                        binding["requestId"].as_str().unwrap()
                    ),
                    before: "unvalidated".into(),
                    after: "valid".into(),
                }],
                json!({"validationOnly":true,"financialOrderPlaced":false,"validation":r}),
            );
        }
        if binding["deadman"] == true {
            let receipt = record
                .dispatch_evidence
                .as_ref()
                .and_then(|d| d.get("response"));
            let Some(receipt) = receipt else {
                return Ok(unresolved(
                    Status::Unknown,
                    "Cancellation timer receipt was lost; GoFin never renews it automatically.",
                    json!({}),
                ));
            };
            let observed = result(receipt);
            if !timer_matches_timeout(observed, &binding["parameters"]["timeout"]) {
                return Ok(unresolved(
                    Status::Unknown,
                    "Kraken timer acknowledgment lacks exact current/trigger time.",
                    receipt.clone(),
                ));
            }
            return proof(
                submission,
                Status::Completed,
                vec![FinancialEffect::AuthorityChange {
                    resource: account.resource.clone(),
                    authority: "orders:deadman:timeout".into(),
                    before: "unspecified".into(),
                    after: binding["parameters"]["timeout"].to_string(),
                }],
                json!({"timerAcknowledgment":observed,"automaticRenewal":false}),
            );
        }
        if let Some(targets) = binding["targets"].as_array() {
            let ids: Vec<_> = targets
                .iter()
                .filter_map(|v| v["orderId"].as_str())
                .collect();
            let response = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostQueryOrders,
                    Some(account),
                    json!({"txid":ids.join(","),"trades":true}),
                )
                .await?;
            let mut effects = vec![];
            for id in ids {
                if result(&response)[id]["status"] != "canceled"
                    && !(binding["originalCancellation"].is_array()
                        && result(&response)[id]["status"] == "closed")
                {
                    return Ok(unresolved(
                        Status::Pending,
                        "A bound cancellation target remains open, filled, or ambiguous; its original fills require reconciliation.",
                        response,
                    ));
                }
                effects.push(FinancialEffect::AuthorityChange {
                    resource: account.resource.clone(),
                    authority: format!("order:{id}:status"),
                    before: "open".into(),
                    after: "canceled".into(),
                });
            }
            if let Some(original) = binding["originalCancellation"].as_array() {
                let operation_id = original[0].as_str().ok_or_else(|| {
                    error(
                        "KRAKEN_CANCELLATION_TARGET_INVALID",
                        "Original GoFin operation identity is missing.",
                    )
                })?;
                let original_submission: Submission = serde_json::from_value(original[1].clone())
                    .map_err(|_| {
                    error(
                        "KRAKEN_CANCELLATION_TARGET_INVALID",
                        "Original GoFin submission identity is invalid.",
                    )
                })?;
                effects = vec![FinancialEffect::SubmissionCancellation {
                    operation_id: operation_id.into(),
                    submission: original_submission,
                }];
            }
            return proof(
                submission,
                Status::Completed,
                effects,
                json!({"targets":targets,"orders":response}),
            );
        }
        let orders: Vec<SpotOrder> =
            serde_json::from_value(binding["orders"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original exact order bindings are unavailable.",
                )
            })?;
        let mut effects = vec![];
        let mut evidence = vec![];
        let mut any_fill = false;
        let mut all_terminal = true;
        let mut proof_events = vec![];
        let mut order_receipts = vec![Quantity::zero(); orders.len()];
        for (order_index, order) in orders.iter().enumerate() {
            if order.uncreated_child {
                effects.push(FinancialEffect::Conversion {
                    resource: account.resource.clone(),
                    spent: Amount {
                        asset: if order.buy {
                            order.quote.clone()
                        } else {
                            order.base.clone()
                        },
                        atoms: Quantity::zero(),
                    },
                    received: Amount {
                        asset: if order.buy {
                            order.base.clone()
                        } else {
                            order.quote.clone()
                        },
                        atoms: Quantity::zero(),
                    },
                });
                continue;
            }
            let mut query = if let Some(reference) = &order.user_reference {
                json!({"userref":reference,"trades":true})
            } else {
                json!({"cl_ord_id":order.client_id,"trades":true})
            };
            if let Some(receipt) = record
                .dispatch_evidence
                .as_ref()
                .and_then(|d| d.get("response"))
            {
                let r = result(receipt);
                if orders.len() == 1
                    && let Some(id) = r["txid"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                {
                    query["txid"] = json!(id);
                } else if let Some(rows) = r["orders"].as_array() {
                    let ids = rows
                        .iter()
                        .filter_map(|r| r["txid"].as_str())
                        .filter(|id| !id.is_empty())
                        .collect::<Vec<_>>();
                    if !ids.is_empty() {
                        query["txid"] = json!(ids.join(","));
                    }
                }
            }
            // Legacy QueryOrders requires txid; client-ID search is supported through
            // exact open/closed order histories after a lost response.
            let order_response = if !order.order_ids.is_empty() {
                self.json(
                    KrakenEndpoint::KrakenPrivatePostQueryOrders,
                    Some(account),
                    json!({"txid":order.order_ids.join(","),"trades":true}),
                )
                .await?
            } else if query.get("txid").is_some() {
                self.json(
                    KrakenEndpoint::KrakenPrivatePostQueryOrders,
                    Some(account),
                    query.clone(),
                )
                .await?
            } else {
                let open = self
                    .json(
                        KrakenEndpoint::KrakenPrivatePostOpenOrders,
                        Some(account),
                        query.clone(),
                    )
                    .await?;
                let closed = self
                    .json(
                        KrakenEndpoint::KrakenPrivatePostClosedOrders,
                        Some(account),
                        query.clone(),
                    )
                    .await?;
                let mut combined = serde_json::Map::new();
                for (r, k) in [(&open, "open"), (&closed, "closed")] {
                    if let Some(map) = result(r)[k].as_object() {
                        for (id, v) in map {
                            if if let Some(reference) = &order.user_reference {
                                v["userref"] == *reference
                            } else {
                                v["cl_ord_id"] == order.client_id
                            } {
                                combined.insert(id.clone(), v.clone());
                            }
                        }
                    }
                }
                json!({"result":combined})
            };
            let map = result(&order_response).as_object().ok_or_else(|| {
                error(
                    "KRAKEN_EVIDENCE_INVALID",
                    "Exact order query returned no identity map.",
                )
            })?;
            let submitted_at = chrono::DateTime::parse_from_rfc3339(
                submission.data["submittedAt"].as_str().unwrap_or(""),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original submission timestamp invalid.",
                )
            })?;
            let matching: Vec<_> = map
                .iter()
                .filter(|(id, v)| {
                    if !order.order_ids.is_empty() {
                        order.order_ids.contains(id)
                    } else {
                        opened_at_or_after(v, &submitted_at)
                            && (if let Some(reference) = &order.user_reference {
                                v["userref"] == *reference
                            } else {
                                v["cl_ord_id"] == order.client_id
                            })
                    }
                })
                .collect();
            if matching.is_empty()
                || (!order.order_ids.is_empty() && matching.len() != order.order_ids.len())
                || (order.order_ids.is_empty() && matching.len() != 1)
            {
                return Ok(unresolved(
                    Status::Unknown,
                    "Original order or successor identities are missing or ambiguous; no write was replayed.",
                    order_response,
                ));
            }
            let order_id = matching[0].0;
            let mut merged = matching[0].1.clone();
            let mut merged_vol = Quantity::zero();
            let mut merged_cost = Quantity::zero();
            let mut merged_fee = Quantity::zero();
            let mut merged_trades = vec![];
            let mut merged_terminal = true;
            for (_, row) in &matching {
                if (row["descr"]["pair"] != order.pair
                    && row["descr"]["pair"] != order.display_pair)
                    || row["descr"]["type"] != if order.buy { "buy" } else { "sell" }
                {
                    return Err(error(
                        "KRAKEN_ORDER_IDENTITY_MISMATCH",
                        "Order lineage changed exact market or direction.",
                    ));
                }
                merged_vol = merged_vol.checked_add(&atoms(&row["vol_exec"])?)?;
                merged_cost = merged_cost.checked_add(&atoms(&row["cost"])?)?;
                merged_fee = merged_fee.checked_add(&atoms(&row["fee"])?)?;
                if let Some(t) = row["trades"].as_array() {
                    merged_trades.extend(t.iter().cloned());
                } else if !atoms(&row["vol_exec"])?.is_zero() {
                    return Err(error(
                        "KRAKEN_TRADE_PROOF_PENDING",
                        "A filled successor omitted its trade identities.",
                    ));
                }
                merged_terminal &= matches!(
                    row["status"].as_str(),
                    Some("closed" | "canceled" | "expired")
                );
            }
            merged["vol_exec"] = json!(merged_vol.to_decimal(36));
            merged["cost"] = json!(merged_cost.to_decimal(36));
            merged["fee"] = json!(merged_fee.to_decimal(36));
            merged["trades"] = json!(merged_trades);
            merged["status"] = json!(if merged_terminal { "closed" } else { "open" });
            let observation = &merged;
            if (observation["descr"]["pair"] != order.pair
                && observation["descr"]["pair"] != order.display_pair)
                || observation["descr"]["type"] != if order.buy { "buy" } else { "sell" }
            {
                return Err(error(
                    "KRAKEN_ORDER_IDENTITY_MISMATCH",
                    "Order evidence changed its exact pair or side.",
                ));
            }
            let status = observation["status"].as_str().unwrap_or("");
            if !matches!(status, "closed" | "canceled" | "expired") {
                all_terminal = false;
            }
            let volume = atoms(&observation["vol_exec"])?;
            let cost = atoms(&observation["cost"])?;
            if volume.is_zero() {
                if !cost.is_zero() || !atoms(&observation["fee"])?.is_zero() {
                    return Err(error(
                        "KRAKEN_EVIDENCE_INVALID",
                        "No-fill order reported cost or fees.",
                    ));
                }
                effects.push(FinancialEffect::Conversion {
                    resource: account.resource.clone(),
                    spent: Amount {
                        asset: if order.buy {
                            order.quote.clone()
                        } else {
                            order.base.clone()
                        },
                        atoms: Quantity::zero(),
                    },
                    received: Amount {
                        asset: if order.buy {
                            order.base.clone()
                        } else {
                            order.quote.clone()
                        },
                        atoms: Quantity::zero(),
                    },
                });
                evidence.push(json!({"orderId":order_id,"order":observation,"trades":[]}));
                continue;
            }
            let ids = observation["trades"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_TRADE_PROOF_PENDING",
                    "Filled order omitted matching trade identities.",
                )
            })?;
            let mut unique = BTreeSet::new();
            let mut trade_ids = vec![];
            for id in ids {
                let id = id
                    .as_str()
                    .ok_or_else(|| error("KRAKEN_EVIDENCE_INVALID", "Trade ID is invalid."))?;
                if !unique.insert(id) {
                    return Err(error(
                        "KRAKEN_EVIDENCE_INVALID",
                        "Order repeats a trade identity.",
                    ));
                }
                trade_ids.push(id);
            }
            let mut trade_map = serde_json::Map::new();
            for chunk in trade_ids.chunks(20) {
                let response = self
                    .json(
                        KrakenEndpoint::KrakenPrivatePostQueryTrades,
                        Some(account),
                        json!({"txid":chunk.join(","),"ledgers":true}),
                    )
                    .await?;
                for id in chunk {
                    let value = result(&response).get(*id).ok_or_else(|| {
                        error(
                            "KRAKEN_TRADE_PROOF_PENDING",
                            "An exact requested trade is not yet visible.",
                        )
                    })?;
                    trade_map.insert((*id).into(), value.clone());
                }
            }
            let trades = Value::Object(trade_map);
            let mut filled = Quantity::zero();
            let mut spent = Quantity::zero();
            let mut fees: BTreeMap<AssetId, Quantity> = BTreeMap::new();
            let mut quote_equivalent_fees = Quantity::zero();
            for id in trade_ids {
                let t = &trades[id];
                if !matching.iter().any(|(id, _)| t["ordertxid"] == id.as_str())
                    || t["pair"] != order.pair
                    || t["type"] != if order.buy { "buy" } else { "sell" }
                {
                    return Err(error(
                        "KRAKEN_TRADE_IDENTITY_MISMATCH",
                        "Trade proof is not bound to this exact order, market and side.",
                    ));
                }
                assert_cash_trade(t)?;
                quote_equivalent_fees = quote_equivalent_fees.checked_add(&atoms(&t["fee"])?)?;
                let trade_volume = atoms(&t["vol"])?;
                let trade_cost = atoms(&t["cost"])?;
                let ledger_ids=t["ledgers"].as_array().ok_or_else(||error("KRAKEN_LEDGER_PROOF_PENDING","Actual fee currency and custody legs require the trade's exact ledger identities."))?;
                let mut unique_ledgers = BTreeSet::new();
                let mut ledger_map = serde_json::Map::new();
                for value in ledger_ids {
                    let key = value.as_str().ok_or_else(|| {
                        error("KRAKEN_EVIDENCE_INVALID", "Trade ledger identity invalid.")
                    })?;
                    if !unique_ledgers.insert(key) {
                        return Err(error(
                            "KRAKEN_EVIDENCE_INVALID",
                            "Trade repeats a ledger identity.",
                        ));
                    }
                }
                for chunk in unique_ledgers.into_iter().collect::<Vec<_>>().chunks(20) {
                    let response = self
                        .json(
                            KrakenEndpoint::KrakenPrivatePostQueryLedgers,
                            Some(account),
                            json!({"id":chunk.join(",")}),
                        )
                        .await?;
                    for key in chunk {
                        let value = result(&response).get(*key).ok_or_else(|| {
                            error(
                                "KRAKEN_LEDGER_PROOF_PENDING",
                                "An exact trade ledger entry is unavailable.",
                            )
                        })?;
                        ledger_map.insert((*key).into(), value.clone());
                    }
                }
                let mut base_change = gofin_domain::SignedQuantity::new("0")?.integer();
                let mut quote_change = base_change.clone();
                let mut event_ledgers = vec![];
                for (ledger_id, l) in &ledger_map {
                    if l["refid"] != id || l["type"] != "trade" {
                        return Err(error(
                            "KRAKEN_LEDGER_IDENTITY_MISMATCH",
                            "Ledger entry is not a trade leg bound to this exact fill.",
                        ));
                    }
                    let code = l["asset"].as_str().ok_or_else(|| {
                        error("KRAKEN_EVIDENCE_INVALID", "Trade ledger asset omitted.")
                    })?;
                    let resolved_alias =
                        if code == order.base.reference || code == order.quote.reference {
                            None
                        } else {
                            Some(self.spot_asset(account, code).await?)
                        };
                    let asset = exact_spot_ledger_asset(
                        code,
                        &order.base,
                        &order.quote,
                        resolved_alias.as_ref(),
                    )?;
                    let decimal = crate::observation::exact(&l["amount"])?;
                    let magnitude = Quantity::from_decimal(decimal.trim_start_matches('-'), 36)?;
                    let signed = if decimal.starts_with('-') {
                        gofin_domain::SignedQuantity::negative(&magnitude)
                    } else {
                        gofin_domain::SignedQuantity::positive(&magnitude)
                    };
                    if asset == &order.base {
                        base_change += signed.integer();
                    } else {
                        quote_change += signed.integer();
                    }
                    let fee = atoms(&l["fee"])?;
                    let total = fees.entry(asset.clone()).or_default();
                    *total = total.checked_add(&fee)?;
                    event_ledgers.push(json!({"ledgerId":ledger_id,"refid":l["refid"],"type":l["type"],"asset":code,"amount":decimal,"fee":fee}));
                }
                let expected_base = if order.buy {
                    gofin_domain::SignedQuantity::positive(&trade_volume)
                } else {
                    gofin_domain::SignedQuantity::negative(&trade_volume)
                };
                let expected_quote = if order.buy {
                    gofin_domain::SignedQuantity::negative(&trade_cost)
                } else {
                    gofin_domain::SignedQuantity::positive(&trade_cost)
                };
                if base_change != expected_base.integer()
                    || quote_change != expected_quote.integer()
                {
                    return Err(error(
                        "KRAKEN_LEDGER_PROOF_PENDING",
                        "Matching ledger entries do not prove the exact gross cash legs of the fill.",
                    ));
                }
                filled = filled.checked_add(&trade_volume)?;
                spent = spent.checked_add(&trade_cost)?;
                proof_events.push(json!({"eventId":format!("kraken:trade:{id}"),"orderId":t["ordertxid"],"clientId":order.client_id,"pair":order.pair,"side":t["type"],"volumeAtoms":trade_volume,"costAtoms":trade_cost,"ledgers":event_ledgers}));
            }
            if filled != volume
                || spent != cost
                || quote_equivalent_fees != atoms(&observation["fee"])?
            {
                return Err(error(
                    "KRAKEN_TRADE_PROOF_PENDING",
                    "Matching trades do not prove the order's complete executed quantity and cost.",
                ));
            }
            let (input, output, principal, receipt) = if order.buy {
                (
                    order.quote.clone(),
                    order.base.clone(),
                    cost.clone(),
                    volume.clone(),
                )
            } else {
                (
                    order.base.clone(),
                    order.quote.clone(),
                    volume.clone(),
                    cost.clone(),
                )
            };
            if principal > order.maximum_spend || (!order.quoted_volume && volume > order.volume) {
                return Err(error(
                    "KRAKEN_ORDER_BOUND_EXCEEDED",
                    "Proved fill exceeds the originally authorized principal or base quantity.",
                ));
            }
            any_fill = true;
            order_receipts[order_index] = receipt
                .checked_sub(fees.get(&output).unwrap_or(&Quantity::zero()))
                .unwrap_or_default();
            effects.push(FinancialEffect::Conversion {
                resource: account.resource.clone(),
                spent: Amount {
                    asset: input,
                    atoms: principal,
                },
                received: Amount {
                    asset: output,
                    atoms: receipt,
                },
            });
            for (asset, fee) in fees {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: account.resource.clone(),
                    },
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: fee,
                    },
                    category: "kraken_trade_fee".into(),
                    treatment: if (order.buy && asset == order.base)
                        || (!order.buy && asset == order.quote)
                    {
                        CostTreatment::DeductedFromCredit
                    } else {
                        CostTreatment::AdditionalDebit
                    },
                });
            }
            evidence.push(json!({"orderId":order_id,"order":observation,"trades":result(&trades)}));
        }
        let controls: TradingControls = serde_json::from_value(binding["controls"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original execution controls are unavailable.",
                )
            })?;
        let mut receipt_satisfied = true;
        if let Some(floors) = binding
            .get("completionOrderMinimumReceive")
            .or_else(|| binding.get("orderMinimumReceive"))
            .and_then(Value::as_array)
        {
            if floors.len() != orders.len() {
                return Err(error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Per-member receipt bounds do not match original batch identities.",
                ));
            }
            for (receipt, floor) in order_receipts.iter().zip(floors) {
                let floor = Quantity::from_decimal(
                    floor.as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Exact member receipt bound unavailable.",
                        )
                    })?,
                    36,
                )?;
                receipt_satisfied &= receipt >= &floor;
            }
        }
        if let Some(minimum) = binding["completionMinimumReceive"]
            .as_str()
            .or(controls.minimum_receive.as_deref())
        {
            let floor = Quantity::from_decimal(minimum, 36)?;
            let received = effects.iter().find_map(|e| match e {
                FinancialEffect::Conversion { received, .. } => Some(received.clone()),
                _ => None,
            });
            let gross = received
                .as_ref()
                .map(|v| v.atoms.clone())
                .unwrap_or_default();
            let fees = effects
                .iter()
                .filter_map(|e| match e {
                    FinancialEffect::Cost {
                        amount,
                        treatment: CostTreatment::DeductedFromCredit,
                        ..
                    } if received
                        .as_ref()
                        .is_some_and(|credit| credit.asset == amount.asset) =>
                    {
                        Some(amount.atoms.clone())
                    }
                    _ => None,
                })
                .try_fold(Quantity::zero(), |total, q| total.checked_add(&q))?;
            receipt_satisfied &= gross.checked_sub(&fees).is_ok_and(|net| net >= floor);
        }
        let mut total_fees: BTreeMap<String, Quantity> = BTreeMap::new();
        for effect in &effects {
            if let FinancialEffect::Cost { amount, .. } = effect {
                let total = total_fees
                    .entry(amount.asset.reference.clone())
                    .or_default();
                *total = total.checked_add(&amount.atoms)?;
            }
        }
        for (code, total) in total_fees {
            if binding["_finBatchCostScope"] != true
                && total
                    > Quantity::from_decimal(
                        controls.maximum_fees.get(&code).ok_or_else(|| {
                            error(
                                "KRAKEN_FEE_PROOF_MISMATCH",
                                "Actual fee has no declared global asset ceiling.",
                            )
                        })?,
                        36,
                    )?
            {
                return crate::unresolved_consumption::cost_overrun(
                    submission,
                    &effects,
                    json!({"code":"KRAKEN_FEE_BOUND_EXCEEDED","asset":code,"actualFeeAtoms":total,"orders":evidence,"proofEvents":proof_events}),
                );
            }
        }
        if binding["conditionalChildrenPending"] == true {
            all_terminal = false;
        }
        if !all_terminal && !any_fill {
            return Ok(unresolved(
                Status::Pending,
                "The native orders remain open with no verified fills; principal and fee reservations remain held.",
                json!({"orders":evidence}),
            ));
        }
        if all_terminal && !any_fill {
            effects.clear();
        }
        proof(
            submission,
            if !all_terminal {
                Status::Pending
            } else if any_fill && receipt_satisfied {
                Status::Completed
            } else {
                Status::Failed
            },
            effects,
            json!({"orders":evidence,"proofEvents":proof_events,"coverage":"Exact order identities and all referenced custody ledger trade legs; no balance-difference settlement."}),
        )
    }
}

impl KrakenProfile {
    async fn reconcile_conditional_close(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        let account = self.account(binding["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Conditional close account missing.",
            )
        })?)?;
        let mut orders: Vec<SpotOrder> = serde_json::from_value(binding["orders"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Conditional order scope invalid.",
                )
            })?;
        let open = self
            .json(
                KrakenEndpoint::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let mut inventory = result(&open)["open"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_CHILD_EVIDENCE_PENDING",
                    "Conditional child open inventory unavailable.",
                )
            })?
            .clone();
        let submitted_at = chrono::DateTime::parse_from_rfc3339(
            submission.data["submittedAt"].as_str().unwrap_or(""),
        )
        .map_err(|_| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original conditional submission time invalid.",
            )
        })?;
        let since = submitted_at.timestamp() - 1;
        let mut offset = 0usize;
        let mut complete = false;
        let mut seen_pages = BTreeSet::new();
        for _ in 0..64 {
            let closed = self
                .json(
                    KrakenEndpoint::KrakenPrivatePostClosedOrders,
                    Some(account),
                    json!({"trades":true,"start":since,"ofs":offset}),
                )
                .await?;
            let rows = result(&closed)["closed"].as_object().ok_or_else(|| {
                error(
                    "KRAKEN_CHILD_EVIDENCE_PENDING",
                    "Conditional child terminal inventory unavailable.",
                )
            })?;
            if !rows.is_empty() && !seen_pages.insert(rows.keys().cloned().collect::<Vec<_>>()) {
                return Err(error(
                    "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                    "Conditional close history repeated an order page.",
                ));
            }
            for (id, row) in rows {
                if inventory.insert(id.clone(), row.clone()).is_some() {
                    return Err(error(
                        "KRAKEN_CHILD_ORDER_AMBIGUOUS",
                        "One conditional order appears in both open and closed inventory; reconcile after the transition.",
                    ));
                }
            }
            offset = offset.checked_add(rows.len()).ok_or_else(|| {
                error(
                    "KRAKEN_CHILD_HISTORY_INCOMPLETE",
                    "Conditional history cursor overflow.",
                )
            })?;
            if rows.len() < 50
                || result(&closed)["count"]
                    .as_u64()
                    .is_some_and(|n| offset as u64 >= n)
            {
                complete = true;
                break;
            }
        }
        if !complete {
            return Ok(unresolved(
                Status::Pending,
                "Conditional close history exceeds the bounded complete page scope; preserve the entry and never replay.",
                json!({"boundedPages":64}),
            ));
        }
        let receipt = record
            .dispatch_evidence
            .as_ref()
            .and_then(|r| r.get("response"))
            .map(result);
        let mut pending = false;
        let mut child_origins = vec![];
        for condition in binding["conditionalClose"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Conditional child constraints missing.",
            )
        })? {
            let parent_index = condition["parentIndex"].as_u64().ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Conditional parent index invalid.",
                )
            })? as usize;
            let child_index = condition["childIndex"].as_u64().ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Conditional child index invalid.",
                )
            })? as usize;
            let parent = orders
                .get(parent_index)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Conditional parent scope missing.",
                    )
                })?
                .clone();
            let acknowledged = receipt.and_then(|r| {
                r["txid"]
                    .as_array()
                    .and_then(|ids| ids.get(parent_index))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        r["orders"]
                            .as_array()
                            .and_then(|rows| rows.get(parent_index))
                            .and_then(|r| r["txid"].as_str())
                    })
            });
            let parents = inventory
                .iter()
                .filter(|(id, row)| {
                    if !parent.order_ids.is_empty() {
                        parent.order_ids.contains(id)
                    } else {
                        opened_at_or_after(row, &submitted_at)
                            && (if let Some(exact) = acknowledged {
                                id.as_str() == exact
                            } else if let Some(reference) = &parent.user_reference {
                                row["userref"] == *reference
                            } else {
                                row["cl_ord_id"] == parent.client_id
                            })
                    }
                })
                .collect::<Vec<_>>();
            if parents.len() != 1 {
                return Ok(unresolved(
                    Status::Unknown,
                    "Conditional parent identity is missing or ambiguous; no matching child is adopted and no action is replayed.",
                    json!({"parentClientId":parent.client_id}),
                ));
            }
            let (parent_id, parent_row) = parents[0];
            let filled = atoms(&parent_row["vol_exec"])?;
            let child = orders.get_mut(child_index).ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Conditional child scope missing.",
                )
            })?;
            let mut quantity = Quantity::zero();
            let mut child_ids = vec![];
            let mut approved_reductions = Quantity::zero();
            for (id, row) in inventory.iter().filter(|(id, row)| {
                row["refid"] == *parent_id
                    || (child.order_ids.contains(id)
                        && binding["conditionalChildRevisions"][*id]["parentOrderId"] == *parent_id
                        && binding["conditionalChildRevisions"][*id]["predecessorOrderId"]
                            .as_str()
                            .is_some_and(|previous| row["refid"] == previous))
            }) {
                if (row["descr"]["pair"] != child.pair
                    && row["descr"]["pair"] != child.display_pair)
                    || row["descr"]["type"] != if child.buy { "buy" } else { "sell" }
                    || row["descr"]["ordertype"] != condition["parameters"]["ordertype"]
                {
                    return Err(error(
                        "KRAKEN_CHILD_IDENTITY_MISMATCH",
                        "Generated child changed its approved exact parent, market, direction or order type.",
                    ));
                }
                let approved_child = binding["conditionalChildRevisions"][id]["parameters"]
                    .as_object()
                    .map(|_| &binding["conditionalChildRevisions"][id]["parameters"])
                    .unwrap_or(&condition["parameters"]);
                for key in ["price", "price2"] {
                    if let Some(requested) = approved_child.get(key)
                        && let Ok(absolute) = atoms(requested)
                        && atoms(&row["descr"][key])? != absolute
                    {
                        return Err(error(
                            "KRAKEN_CHILD_PRICE_MISMATCH",
                            "Generated child differs from an approved absolute trigger or price.",
                        ));
                    }
                }
                let revision = &binding["conditionalChildRevisions"][id];
                if revision.is_object() {
                    if revision["parentOrderId"] != *parent_id {
                        return Err(error(
                            "KRAKEN_CHILD_IDENTITY_MISMATCH",
                            "The approved child revision changed its exact original entry relationship.",
                        ));
                    }
                    let approved: Quantity = serde_json::from_value(
                        revision["approvedQuantity"].clone(),
                    )
                    .map_err(|_| {
                        error(
                            "KRAKEN_CHILD_EVIDENCE_PENDING",
                            "The approved issued-child quantity is unavailable.",
                        )
                    })?;
                    let issued: Quantity =
                        serde_json::from_value(revision["issuedQuantityAtApproval"].clone())
                            .map_err(|_| {
                                error(
                                    "KRAKEN_CHILD_EVIDENCE_PENDING",
                                    "The original issued-child quantity is unavailable.",
                                )
                            })?;
                    let retired = revision["retiredBySuccessor"].is_string();
                    let current = atoms(&row[if retired { "vol_exec" } else { "vol" }])?;
                    let clamp = revision["allowNativeFillClamp"] == true
                        && matches!(
                            row["status"].as_str(),
                            Some("closed" | "canceled" | "expired")
                        )
                        && current == atoms(&row["vol_exec"])?
                        && current >= approved;
                    if (!retired && current != approved && !clamp) || current > issued {
                        return Err(error(
                            "KRAKEN_CHILD_QUANTITY_MISMATCH",
                            "Issued-child quantity differs from the independently acknowledged exact revision or proved terminal fill clamp.",
                        ));
                    }
                    if !retired {
                        approved_reductions =
                            approved_reductions.checked_add(&issued.checked_sub(&current)?)?;
                    }
                }
                quantity = quantity.checked_add(&atoms(
                    &row[if binding["conditionalChildRevisions"][id]["retiredBySuccessor"]
                        .is_string()
                    {
                        "vol_exec"
                    } else {
                        "vol"
                    }],
                )?)?;
                child_ids.push(id.clone());
                child_origins.push(json!({"childOrderId":id,"parentOrderId":parent_id,"refid":row["refid"],"orderQuantity":row["vol"]}));
            }
            if quantity > filled {
                return Err(error(
                    "KRAKEN_CHILD_QUANTITY_MISMATCH",
                    "Generated close quantities exceed the independently proved original entry fills.",
                ));
            }
            pending |= quantity.checked_add(&approved_reductions)? < filled;
            child.uncreated_child = child_ids.is_empty();
            child.order_ids = child_ids;
        }
        let mut current = binding.clone();
        current["orders"] = json!(orders);
        current.as_object_mut().unwrap().remove("conditionalClose");
        current["conditionalChildrenPending"] = json!(pending);
        let mut reconciled =
            Box::pin(self.reconcile_trading_binding(record, submission, &current)).await?;
        if let Some(bundle) = &mut reconciled.verified {
            crate::conditional_sequences::convert(binding, &current, bundle)?;
        }
        if let Some(bundle) = &mut reconciled.verified {
            let mut child_goals = Vec::new();
            let mut goals_met = true;
            let mut seen_goals = BTreeSet::new();
            for (id, revision) in binding["conditionalChildRevisions"]
                .as_object()
                .into_iter()
                .flatten()
            {
                let Some(minimum) = revision["minimumReceive"].as_str() else {
                    continue;
                };
                let owner = revision["targetOriginalId"].as_str().unwrap_or(id);
                if !seen_goals.insert(owner.to_owned()) {
                    continue;
                }
                let lineage = binding["conditionalChildRevisions"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(native, scope)| {
                        (scope["targetOriginalId"].as_str().unwrap_or(native) == owner)
                            .then_some(native.as_str())
                    })
                    .collect::<BTreeSet<_>>();
                let floor = Quantity::from_decimal(minimum, 36)?;
                let child_index = revision["childIndex"].as_u64().ok_or_else(|| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "An issued-child receipt goal lacks its original leg identity.",
                    )
                })? as usize;
                let child = orders.get(child_index).ok_or_else(|| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "An issued-child receipt goal identifies an unavailable leg.",
                    )
                })?;
                let output = if child.buy { &child.base } else { &child.quote };
                let mut gross = Quantity::zero();
                let mut fees = Quantity::zero();
                for event in bundle.evidence["proofEvents"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|event| {
                        event["orderId"]
                            .as_str()
                            .is_some_and(|id| lineage.contains(id))
                    })
                {
                    let amount: Quantity = serde_json::from_value(
                        event[if child.buy {
                            "volumeAtoms"
                        } else {
                            "costAtoms"
                        }]
                        .clone(),
                    )
                    .map_err(|_| {
                        error(
                            "KRAKEN_CHILD_RECEIPT_PROOF_PENDING",
                            "The exact issued child's proof omitted its gross output quantity.",
                        )
                    })?;
                    gross = gross.checked_add(&amount)?;
                    for ledger in event["ledgers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|ledger| ledger["asset"] == output.reference)
                    {
                        let fee: Quantity =
                            serde_json::from_value(ledger["fee"].clone()).map_err(|_| {
                                error(
                                    "KRAKEN_CHILD_RECEIPT_PROOF_PENDING",
                                    "An issued-child output fee lacks exact units.",
                                )
                            })?;
                        fees = fees.checked_add(&fee)?;
                    }
                }
                let net = gross.checked_sub(&fees).unwrap_or_default();
                goals_met &= net >= floor;
                child_goals.push(json!({"orderId":id,"minimumReceiveAtoms":floor,"provedNetOutputAtoms":net,"asset":output}));
            }
            if reconciled.status == Status::Completed && !goals_met {
                reconciled.status = Status::Failed;
            }
            bundle.evidence["issuedChildReceiptGoals"] = json!(child_goals);
            bundle.evidence["conditionalChildOrigins"] = json!(child_origins);
            bundle.proof_digest = gofin_domain::verified_effect_digest(bundle)?;
            reconciled.evidence = bundle.evidence.clone();
        }
        Ok(reconciled)
    }
}

// A reused client reference is not a request identity. Newly submitted orders
// must also have a native creation time inside the original attempt window.
pub(crate) fn opened_at_or_after(
    order: &Value,
    cutoff: &chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    let cutoff = Quantity::from_decimal(
        &format!(
            "{}.{:09}",
            cutoff.timestamp(),
            cutoff.timestamp_subsec_nanos()
        ),
        36,
    );
    let observed =
        crate::observation::exact(&order["opentm"]).and_then(|v| Quantity::from_decimal(&v, 36));
    matches!((observed,cutoff),(Ok(observed),Ok(cutoff))if observed>=cutoff)
}

pub(crate) fn timer_matches_timeout(receipt: &Value, timeout: &Value) -> bool {
    let Some(seconds) = timeout
        .as_u64()
        .or_else(|| timeout.as_str().and_then(|s| s.parse().ok()))
    else {
        return false;
    };
    let Some(current) = receipt["currentTime"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    if seconds == 0 {
        return receipt["triggerTime"] == "0";
    }
    let Some(trigger) = receipt["triggerTime"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    i64::try_from(seconds)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .is_some_and(|approved| trigger.signed_duration_since(current) == approved)
}

/// `misc` is the official native triggered-state evidence; the order type alone
/// says nothing about whether its stop/touch has actually triggered.
pub(crate) fn check_legacy_native_trigger(order: &Value) -> Result<()> {
    if matches!(
        order["descr"]["ordertype"].as_str(),
        Some("stop-loss" | "stop-loss-limit" | "take-profit" | "take-profit-limit")
    ) && order["misc"].as_str().is_some_and(|flags| {
        flags
            .split(',')
            .any(|flag| matches!(flag, "stopped" | "touched"))
    }) {
        return Err(error(
            "KRAKEN_LEGACY_TRIGGERED_ORDER_UNSUPPORTED",
            "Kraken's legacy EditOrder excludes an independently observed triggered stop-loss or take-profit. Use atomic amendment or cancel the exact original order before a separately approved replacement.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod spot_ledger_asset_tests {
    use super::*;
    use crate::KrakenProduct;
    use gofin_domain::ResourceId;

    #[test]
    fn legacy_client_id_survives_the_product_rename() {
        let account = KrakenAccount {
            name: "spot".into(),
            resource: ResourceId("kraken:synthetic:spot".into()),
            credential_ref: "synthetic-only".into(),
            physical_identity: "synthetic-uid".into(),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product: KrakenProduct::Spot,
        };
        assert_eq!(
            client_id(&account, "request-000001", 0),
            "finf6acba3b0bd4712"
        );
    }

    #[test]
    fn native_ledger_asset_altname_resolves_to_the_exact_pair_asset() {
        let base = AssetId {
            network: "kraken".into(),
            reference: "XETH".into(),
            decimals: 36,
        };
        let quote = AssetId {
            network: "kraken".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let resolved = AssetId {
            reference: "XETH".into(),
            ..base.clone()
        };
        assert_eq!(
            exact_spot_ledger_asset("ETH", &base, &quote, Some(&resolved)).unwrap(),
            &base
        );
        assert_eq!(
            exact_spot_ledger_asset("USDC", &base, &quote, None).unwrap(),
            &quote
        );
        assert_eq!(
            exact_spot_ledger_asset("XETH", &base, &quote, None).unwrap(),
            &base
        );
        assert_eq!(
            exact_spot_ledger_asset("OTHER", &base, &quote, None)
                .unwrap_err()
                .code,
            "KRAKEN_UNEXPECTED_TRADE_ASSET"
        );
    }
}
