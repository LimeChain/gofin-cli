//! Native order ownership comes from the original protected journal, never a reusable client label.
use crate::{
    KrakenEndpoint as E,
    observation::result,
    profile::{KrakenProfile, error},
};
use fin_domain::{
    Result,
    amendments::{AmendmentContext, AmendmentScope},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

impl KrakenProfile {
    pub(crate) async fn pinned_original_scope(
        &self,
        context: &AmendmentContext,
    ) -> Result<Option<AmendmentScope>> {
        let original = context.original.as_ref().map(|p| AmendmentScope {
            intent: p.intent.clone(),
            binding: p.binding.clone(),
            valuation: p.valuation.clone(),
        });
        let Some(mut scope) = context.effective_scope().or(original.as_ref()).cloned() else {
            return Ok(None);
        };
        let Some(evidence) = context.original_evidence.as_ref() else {
            return Ok(Some(scope));
        };
        let account = self.account(scope.binding["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original account identity missing.",
            )
        })?)?;
        let receipt = evidence
            .dispatch_evidence
            .as_ref()
            .map(|r| &r["response"])
            .unwrap_or(&Value::Null);
        let receipt = if self.product == crate::KrakenProduct::Spot {
            if receipt
                .get("error")
                .is_some_and(|error| error.as_array().is_none_or(|errors| !errors.is_empty()))
            {
                &Value::Null
            } else {
                receipt
            }
        } else if receipt["result"] == "success" {
            receipt
        } else {
            &Value::Null
        };
        let events = evidence
            .verified_effects
            .as_ref()
            .and_then(|e| e.evidence["proofEvents"].as_array())
            .cloned()
            .unwrap_or_default();
        let at = evidence
            .submission
            .as_ref()
            .and_then(|s| s.data["submittedAt"].as_str())
            .unwrap_or(&evidence.created_at);
        let cutoff = chrono::DateTime::parse_from_rfc3339(at).map_err(|_| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original durable submission time invalid.",
            )
        })?;
        let margin = scope.binding["margin"].is_object();
        if scope.binding["orders"].is_array() || margin {
            let orders = if margin {
                &scope.binding["margin"]["orders"]
            } else {
                &scope.binding["orders"]
            }
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Original native orders missing.",
                )
            })?
            .clone();
            let generated = scope.binding["conditionalClose"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r["childIndex"].as_u64())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let roots = orders
                .iter()
                .enumerate()
                .filter(|(index, order)| {
                    !generated.contains(&(*index as u64)) && order["conditionalParent"].is_null()
                })
                .count();
            let mut pinned = Vec::new();
            for (index, mut order) in orders.clone().into_iter().enumerate() {
                let child =
                    generated.contains(&(index as u64)) || !order["conditionalParent"].is_null();
                let mut ids = order["orderIds"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                let prior_ids = ids.clone();
                let mut receipt_ids = BTreeSet::new();
                for event in &events {
                    if event["clientId"] == order["clientId"] && event["orderId"].is_string() {
                        ids.insert(event["orderId"].as_str().unwrap().to_owned());
                    }
                }
                if roots == 1
                    && !child
                    && let Some(ack) = result(receipt)["txid"].as_array()
                {
                    receipt_ids.extend(ack.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                // Batch receipts do not carry the client ID. Resolve their exact txids through native order metadata.
                let batch_ids = result(receipt)["orders"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r["txid"].as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !batch_ids.is_empty() && !child {
                    let queried = self
                        .json(
                            E::KrakenPrivatePostQueryOrders,
                            Some(account),
                            json!({"txid":batch_ids.join(","),"trades":true}),
                        )
                        .await?;
                    if let Some(rows) = result(&queried).as_object() {
                        for (id, row) in rows {
                            if spot_matches(&order, row) {
                                receipt_ids.insert(id.clone());
                            }
                        }
                    }
                }
                if !receipt_ids.is_empty() {
                    let allowed = receipt_ids
                        .union(&prior_ids)
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    if !ids.is_subset(&allowed) {
                        return Err(error(
                            "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS",
                            "Native original acknowledgment contradicts its immutable fill identity.",
                        ));
                    }
                    ids.extend(receipt_ids);
                }
                if ids.len() > 1 && prior_ids.is_empty() {
                    return Err(error(
                        "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS",
                        "Multiple native identities require a proven original successor lineage.",
                    ));
                }
                if ids.is_empty() && child {
                    // An unissued automatic close has no native identity yet. It cannot be selected for a mutation; its parent remains pinned.
                    pinned.push(order);
                    continue;
                }
                if ids.is_empty() {
                    let mut query = if order["userReference"].is_null() {
                        json!({"trades":true,"cl_ord_id":order["clientId"]})
                    } else {
                        json!({"trades":true,"userref":order["userReference"]})
                    };
                    let open = self
                        .json(E::KrakenPrivatePostOpenOrders, Some(account), query.clone())
                        .await?;
                    let mut candidates = std::collections::BTreeMap::new();
                    if let Some(rows) = result(&open)["open"].as_object() {
                        candidates.extend(rows.iter().map(|(id, v)| (id.clone(), v.clone())));
                    }
                    let mut complete = false;
                    let mut previous_count = None;
                    let mut closed_ids = BTreeSet::new();
                    for page in 0..64 {
                        query["ofs"] = json!(page * 50);
                        query["start"] = json!(cutoff.timestamp());
                        let closed = self
                            .json(
                                E::KrakenPrivatePostClosedOrders,
                                Some(account),
                                query.clone(),
                            )
                            .await?;
                        let count = result(&closed)["count"].as_u64().ok_or_else(|| {
                            error(
                                "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                                "Closed order recovery omitted its exhaustive count.",
                            )
                        })?;
                        let rows = result(&closed)["closed"].as_object().ok_or_else(|| {
                            error(
                                "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                                "Closed order recovery omitted its identity map.",
                            )
                        })?;
                        if previous_count.is_some_and(|prior| prior != count)
                            || rows.len() as u64 != count.saturating_sub(page * 50).min(50)
                            || rows.keys().any(|id| !closed_ids.insert(id.clone()))
                        {
                            return Err(error(
                                "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                                "Closed order recovery changed count, repeated identities or omitted expected page records.",
                            ));
                        }
                        previous_count = Some(count);
                        candidates.extend(rows.iter().map(|(id, v)| (id.clone(), v.clone())));
                        if (page + 1) * 50 >= count {
                            complete = true;
                            break;
                        }
                    }
                    if !complete {
                        return Err(error(
                            "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                            "Original order history exceeded the bounded recovery limit.",
                        ));
                    }
                    for (id, row) in candidates {
                        if spot_matches(&order, &row)
                            && crate::trading::opened_at_or_after(&row, &cutoff)
                        {
                            ids.insert(id);
                        }
                    }
                    if ids.len() != 1 {
                        return Err(error(
                            "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS",
                            "Lost-response recovery requires exactly one original native order across complete open and closed history. Nothing was amended.",
                        ));
                    }
                }
                // Proven legacy replacements are appended by the profile's immutable successor lineage.
                if let Some(prior) = order["orderIds"].as_array() {
                    ids.extend(prior.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                if ids.is_empty() {
                    return Err(error(
                        "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS",
                        "Exact original venue identity remains unresolved.",
                    ));
                }
                order["orderIds"] = json!(ids);
                pinned.push(order);
            }
            if margin {
                scope.binding["margin"]["orders"] = json!(pinned);
            } else {
                scope.binding["orders"] = json!(pinned);
            }
        } else if scope.binding["derivative"].is_object()
            || scope.binding["derivativeBatch"].is_array()
        {
            let batch = scope.binding["derivativeBatch"].is_array();
            let mut members = if batch {
                scope.binding["derivativeBatch"].as_array().unwrap().clone()
            } else {
                vec![scope.binding.clone()]
            };
            for member in &mut members {
                let d = &member["derivative"];
                if !d.is_object() {
                    continue;
                }
                let mut ids = BTreeSet::new();
                if let Some(id) = d["orderId"].as_str() {
                    ids.insert(id.to_owned());
                }
                for event in &events {
                    let order = &event["execution"]["order"];
                    if order["clientId"] == d["clientId"]
                        && order["tradeable"] == d["symbol"]
                        && let Some(id) = order["uid"].as_str()
                    {
                        ids.insert(id.to_owned());
                    }
                }
                if !batch && let Some(id) = receipt["sendStatus"]["order_id"].as_str() {
                    ids.insert(id.to_owned());
                }
                if batch && let Some(rows) = receipt["batchStatus"].as_array() {
                    for row in rows {
                        if scope.binding["parameters"]["batchOrder"]
                            .as_array()
                            .is_some_and(|native| {
                                native.iter().any(|n| {
                                    n["cliOrdId"] == d["clientId"]
                                        && n["order_tag"] == row["order_tag"]
                                })
                            })
                            && let Some(id) = row["order_id"].as_str()
                        {
                            ids.insert(id.to_owned());
                        }
                    }
                }
                if ids.is_empty() {
                    let history=self.derivatives_history(E::KrakenfuturesHistoryGetOrders,json!({"__account":account.name,"since":cutoff.timestamp_millis(),"sort":"asc","count":1000,"tradeable":d["symbol"]})).await?;
                    for row in history {
                        if let Some(event) = row["event"].as_object() {
                            for (kind, detail) in event {
                                let o = if kind == "OrderUpdated" {
                                    &detail["newOrder"]
                                } else {
                                    &detail["order"]
                                };
                                if o["clientId"] == d["clientId"]
                                    && o["tradeable"] == d["symbol"]
                                    && o["accountUid"] == d["accountUid"]
                                    && o["timestamp"].as_i64().is_some_and(|t| {
                                        i128::from(t) * 1_000_000
                                            >= i128::from(cutoff.timestamp()) * 1_000_000_000
                                                + i128::from(cutoff.timestamp_subsec_nanos())
                                    })
                                    && let Some(id) = o["uid"].as_str()
                                {
                                    ids.insert(id.to_owned());
                                }
                            }
                        }
                    }
                }
                if ids.len() != 1 {
                    return Err(error(
                        "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS",
                        "Original derivative identity is missing or reused across native history. Nothing was amended.",
                    ));
                }
                member["derivative"]["orderId"] = json!(ids.first().unwrap());
            }
            if batch {
                scope.binding["derivativeBatch"] = json!(members);
            } else {
                scope.binding = members.remove(0);
            }
        }
        Ok(Some(scope))
    }
}
fn spot_matches(order: &Value, row: &Value) -> bool {
    if !order["userReference"].is_null() {
        row["userref"] == order["userReference"]
    } else {
        row["cl_ord_id"] == order["clientId"]
    }
}
