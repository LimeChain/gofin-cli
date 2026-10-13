//! Derivative contracts are exposure; only independently correlated collateral bookings are cash.
use crate::{
    KrakenEndpoint as E, contracts,
    observation::{atoms, exact},
    profile::{KrakenProduct, KrakenProfile, error},
    trading::{controls, proof, unresolved},
};
use chrono::{Duration, Utc};
use fin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, EffectConstraint,
    FinancialEffect, FinancialIntent, OperationRecord, PositionAnchor, PositionKind,
    PreparedOperation, Quantity, Reconciliation, ResourceAmount, ResourceId, Result,
    SettlementCategory, SignedQuantity, Status, Submission,
};
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub(crate) fn asset(code: &str) -> AssetId {
    AssetId {
        network: "krakenfutures".into(),
        reference: code.into(),
        decimals: 36,
    }
}
pub(crate) fn contract(symbol: &str) -> AssetId {
    AssetId {
        network: "krakenfutures:contracts".into(),
        reference: symbol.into(),
        decimals: 36,
    }
}
fn signed(v: &Value) -> Result<BigInt> {
    let text = exact(v)?;
    let magnitude = Quantity::from_decimal(text.trim_start_matches('-'), 36)?;
    Ok(if text.starts_with('-') {
        SignedQuantity::negative(&magnitude).integer()
    } else {
        SignedQuantity::positive(&magnitude).integer()
    })
}
fn sum(a: &Quantity, b: &Quantity) -> Result<Quantity> {
    a.checked_add(b)
}
fn positive(v: &Value) -> Result<Quantity> {
    let q = atoms(v)?;
    if q.is_zero() {
        return Err(error(
            "INVALID_INPUT",
            "Derivative quantities and bound prices must be positive.",
        ));
    }
    Ok(q)
}
fn delta(before: &BigInt, after: &BigInt, short: bool) -> Result<SignedQuantity> {
    let magnitude = |v: &BigInt| {
        if short && v.is_negative() {
            -v
        } else if !short && v.is_positive() {
            v.clone()
        } else {
            BigInt::zero()
        }
    };
    SignedQuantity::new((magnitude(after) - magnitude(before)).to_string())
}
// Use every applicable published customer/platform schedule when the venue
// does not disclose the credential's classification. This is conservative and
// never assumes professional leverage from a public instrument default.
fn initial_margin_rate(
    instrument: &Value,
    contracts: &Quantity,
    quote_units: &Quantity,
) -> Result<Quantity> {
    let mut arrays = Vec::new();
    for key in ["marginLevels", "retailMarginLevels"] {
        if let Some(rows) = instrument[key].as_array() {
            arrays.push(rows);
        }
    }
    if let Some(schedules) = instrument["marginSchedules"].as_object() {
        for schedule in schedules.values() {
            for key in ["retail", "professional"] {
                if let Some(rows) = schedule[key].as_array() {
                    arrays.push(rows);
                }
            }
        }
    }
    let mut rate = Quantity::zero();
    let mut applicable = false;
    for rows in arrays {
        for row in rows {
            let contracts_apply = row
                .get("contracts")
                .filter(|v| !v.is_null())
                .map(atoms)
                .transpose()?
                .is_none_or(|threshold| *contracts >= threshold);
            let quote_apply = row
                .get("numNonContractUnits")
                .filter(|v| !v.is_null())
                .map(atoms)
                .transpose()?
                .is_none_or(|threshold| *quote_units >= threshold);
            if contracts_apply && quote_apply {
                applicable = true;
                rate = rate.max(positive(&row["initialMargin"])?);
            }
        }
    }
    if !applicable || rate > Quantity::from_decimal("1", 36)? {
        return Err(error(
            "KRAKEN_INITIAL_MARGIN_UNKNOWN",
            "Published applicable derivative margin requirements are unavailable or invalid; no professional-leverage assumption is used.",
        ));
    }
    Ok(rate)
}
impl KrakenProfile {
    pub(crate) async fn prepare_derivative_control_with_scope(
        &self,
        endpoint: E,
        input: &Value,
        parameters: Value,
        previous: Option<&Value>,
    ) -> Result<PreparedOperation> {
        crate::contracts::validate_fin_controls(endpoint, input, previous)?;
        let control = controls(input)?;
        let account = self.account(
            input["account"]
                .as_str()
                .ok_or_else(|| error("INVALID_INPUT", "Exact derivative account missing."))?,
        )?;
        let deadman = endpoint == E::KrakenfuturesPrivatePostCancelallordersafter;
        let original = match (&control.operation_id, &control.original_submission) {
            (Some(id), Some(s)) if !id.is_empty() => Some((id.clone(), s.clone())),
            (None, None) => None,
            _ => {
                return Err(error(
                    "INVALID_INPUT",
                    "Linked cancellation requires operationId and originalSubmission together.",
                ));
            }
        };
        if let Some((_, s)) = &original
            && (s.kind != "krakenfutures-rest-v1"
                || s.data["binding"]["account"] != account.name
                || s.data["binding"]["physicalIdentity"] != account.physical_identity
                || (!s.data["binding"]["derivative"].is_object()
                    && !s.data["binding"]["derivativeBatch"].is_array()))
        {
            return Err(error(
                "KRAKEN_CANCELLATION_TARGET_INVALID",
                "Original derivative submission does not identify this exact account and product.",
            ));
        }
        let mut targets = vec![];
        let mut intent = FinancialIntent::default();
        if deadman {
            let seconds=parameters["timeout"].as_str().and_then(|v|v.parse::<u32>().ok()).ok_or_else(||error("INVALID_INPUT","Set an explicit whole-second derivative cancellation timeout, including zero to disable."))?;
            if original.is_some() {
                return Err(error(
                    "INVALID_INPUT",
                    "A dead-man switch configures the account timer, not one original submission.",
                ));
            }
            intent.expected.push(EffectConstraint::Authority {
                resource: account.resource.clone(),
                authority: "orders:deadman:timeout".into(),
                before: "unspecified".into(),
                after: seconds.to_string(),
            });
        } else {
            intent.locks.push(account.resource.clone());
            let open = self
                .json(
                    E::KrakenfuturesPrivateGetOpenorders,
                    Some(account),
                    json!({}),
                )
                .await?;
            for order in open["openOrders"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_PROOF_PENDING",
                    "Exact open derivative order scope unavailable.",
                )
            })? {
                let selected = if endpoint == E::KrakenfuturesPrivatePostCancelallorders {
                    parameters["symbol"].is_null() || parameters["symbol"] == order["symbol"]
                } else {
                    parameters["order_id"]
                        .as_str()
                        .is_some_and(|id| order["order_id"] == id)
                        || parameters["cliOrdId"]
                            .as_str()
                            .is_some_and(|id| order["cliOrdId"] == id)
                };
                if selected {
                    if endpoint == E::KrakenfuturesPrivatePostCancelallorders
                        && let Some((_, submission)) = &original
                    {
                        let original = previous.unwrap_or(&submission.data["binding"]);
                        let owned = if let Some(rows) = original["derivativeBatch"].as_array() {
                            rows.iter().any(|b| {
                                b["derivative"]["clientId"] == order["cliOrdId"]
                                    && b["derivative"]["symbol"] == order["symbol"]
                                    && (b["derivative"]["orderId"].is_null()
                                        || b["derivative"]["orderId"] == order["order_id"])
                            })
                        } else {
                            original["derivative"]["clientId"] == order["cliOrdId"]
                                && original["derivative"]["symbol"] == order["symbol"]
                                && (original["derivative"]["orderId"].is_null()
                                    || original["derivative"]["orderId"] == order["order_id"])
                        };
                        if !owned {
                            continue;
                        }
                    }
                    let id = order["order_id"].as_str().ok_or_else(|| {
                        error("KRAKEN_ORDER_PROOF_PENDING", "Derivative order id missing.")
                    })?;
                    targets.push(
                        json!({"orderId":id,"clientId":order["cliOrdId"],"symbol":order["symbol"]}),
                    );
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
                    "No exact open derivative order matches cancellation.",
                ));
            }
            if let Some((id, s)) = &original {
                let binding = previous.unwrap_or(&s.data["binding"]);
                let owned = targets.iter().all(|target| {
                    if let Some(rows) = binding["derivativeBatch"].as_array() {
                        rows.iter().any(|b| {
                            b["derivative"]["clientId"] == target["clientId"]
                                && b["derivative"]["symbol"] == target["symbol"]
                                && (b["derivative"]["orderId"].is_null()
                                    || b["derivative"]["orderId"] == target["orderId"])
                        })
                    } else {
                        target["clientId"] == binding["derivative"]["clientId"]
                            && target["symbol"] == binding["derivative"]["symbol"]
                            && (binding["derivative"]["orderId"].is_null()
                                || binding["derivative"]["orderId"] == target["orderId"])
                    }
                });
                if !owned {
                    return Err(error(
                        "KRAKEN_CANCELLATION_TARGET_MISMATCH",
                        "Selected cancellation is outside the original Fin derivative submission.",
                    ));
                }
                intent.expected = vec![EffectConstraint::SubmissionCancellation {
                    operation_id: id.clone(),
                    submission: s.clone(),
                }];
            }
        }
        Ok(PreparedOperation {
            operation: endpoint.operation().into(),
            input: input.clone(),
            binding: json!({"endpoint":endpoint.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":parameters,"requestId":control.request_id,"targets":targets,"deadman":deadman,"derivativeControl":true,"originalCancellation":original,"cancellationOriginalScope":previous}),
            summary: if deadman {
                format!(
                    "Set derivative cancellation timer on {} to {} seconds. Fin will not renew it.",
                    account.name, parameters["timeout"]
                )
            } else {
                format!(
                    "Cancel {} exact derivative order(s) on {}; original fills retain their accounting.",
                    targets.len(),
                    account.name
                )
            },
            checks: vec![],
            warnings: vec!["The venue timer persists independently of Fin authorization.".into()],
            observed_at: fin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            valuation: deadman.then(|| crate::trading::zero_authority_valuation(endpoint)),
            intent,
            action_deadline: None,
            operation_version: 1,
        })
    }
    async fn reconcile_derivative_control(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        b: &Value,
    ) -> Result<Reconciliation> {
        let account = self.account(b["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original derivative account missing.",
            )
        })?)?;
        if b["physicalIdentity"] != account.physical_identity {
            return Err(error(
                "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                "Original derivative account identity changed.",
            ));
        }
        if b["deadman"] == true {
            let receipt = record
                .dispatch_evidence
                .as_ref()
                .and_then(|v| v.get("response"));
            let Some(r) = receipt else {
                return Ok(unresolved(
                    Status::Unknown,
                    "Derivative timer acknowledgment was lost; Fin never renews it automatically.",
                    json!({}),
                ));
            };
            if r["result"] != "success"
                || !crate::trading::timer_matches_timeout(&r["status"], &b["parameters"]["timeout"])
            {
                return Ok(unresolved(
                    Status::Unknown,
                    "Derivative timer receipt lacks exact acknowledged times.",
                    r.clone(),
                ));
            }
            return proof(
                submission,
                Status::Completed,
                vec![FinancialEffect::AuthorityChange {
                    resource: account.resource.clone(),
                    authority: "orders:deadman:timeout".into(),
                    before: "unspecified".into(),
                    after: b["parameters"]["timeout"].as_str().unwrap().into(),
                }],
                json!({"timerAcknowledgment":r,"automaticRenewal":false}),
            );
        }
        let since = chrono::DateTime::parse_from_rfc3339(
            submission.data["submittedAt"]
                .as_str()
                .ok_or_else(|| error("KRAKEN_SUBMISSION_INVALID", "Submission time missing."))?,
        )
        .map_err(|_| error("KRAKEN_SUBMISSION_INVALID", "Submission time invalid."))?
        .timestamp_millis()
            - 30_000;
        let history = self
            .derivatives_history(
                E::KrakenfuturesHistoryGetOrders,
                json!({"__account":account.name,"since":since,"sort":"asc","count":1000}),
            )
            .await?;
        let targets = b["targets"].as_array().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Exact cancellation targets missing.",
            )
        })?;
        let mut effects = vec![];
        let mut evidence = vec![];
        for target in targets {
            let matched = history
                .iter()
                .filter_map(|row| row["event"].as_object().map(|events| (row, events)))
                .flat_map(|(row, events)| {
                    events.iter().map(move |(kind, detail)| (row, kind, detail))
                })
                .filter(|(_, _, d)| {
                    d["order"]["uid"] == target["orderId"]
                        && d["order"]["tradeable"] == target["symbol"]
                })
                .collect::<Vec<_>>();
            let terminal = matched.iter().find(|(_, kind, detail)| {
                kind.as_str() == "OrderCancelled"
                    || (b["originalCancellation"].is_array()
                        && kind.as_str() != "OrderRejected"
                        && detail["order"]["filled"] == detail["order"]["quantity"])
            });
            let Some((row, _, _)) = terminal else {
                return Ok(unresolved(
                    Status::Pending,
                    "Cancellation target lacks a matching durable terminal order event; an empty page never authorizes replay.",
                    json!({"target":target}),
                ));
            };
            evidence.push((*row).clone());
            effects.push(FinancialEffect::AuthorityChange {
                resource: account.resource.clone(),
                authority: format!("order:{}:status", target["orderId"].as_str().unwrap()),
                before: "open".into(),
                after: "canceled".into(),
            });
        }
        if let Some(original) = b["originalCancellation"].as_array() {
            effects = vec![FinancialEffect::SubmissionCancellation {
                operation_id: original[0]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Original operation id missing.",
                        )
                    })?
                    .into(),
                submission: serde_json::from_value(original[1].clone()).map_err(|_| {
                    error("KRAKEN_SUBMISSION_INVALID", "Original submission invalid.")
                })?,
            }];
        }
        proof(
            submission,
            Status::Completed,
            effects,
            json!({"targets":targets,"terminalOrders":evidence}),
        )
    }
    pub(crate) async fn derivatives_history(
        &self,
        endpoint: E,
        parameters: Value,
    ) -> Result<Vec<Value>> {
        let mut parameters = parameters;
        let mut records = vec![];
        let mut tokens = BTreeSet::new();
        for _ in 0..64 {
            let response = self
                .json(
                    endpoint,
                    Some(
                        self.account(parameters["__account"].as_str().ok_or_else(|| {
                            error("KRAKEN_HISTORY_ACCOUNT_MISSING", "History account missing.")
                        })?)?,
                    ),
                    {
                        let mut p = parameters.clone();
                        p.as_object_mut().unwrap().remove("__account");
                        p
                    },
                )
                .await?;
            let rows = response["elements"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_HISTORY_PROOF_PENDING",
                    "Derivative history omitted exact event records.",
                )
            })?;
            records.extend(rows.iter().cloned());
            let Some(token) = response["continuationToken"]
                .as_str()
                .filter(|s| !s.is_empty())
            else {
                return Ok(records);
            };
            if !tokens.insert(token.to_owned()) {
                return Err(error(
                    "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                    "Derivative history repeated a continuation token.",
                ));
            }
            parameters["continuation_token"] = json!(token);
        }
        Err(error(
            "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
            "Derivative history exceeded its bounded page limit; no exhaustive proof is claimed.",
        ))
    }
    pub(crate) async fn prepare_derivatives(
        &self,
        endpoint: E,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let control = controls(input)?;
        let (selection, mut parameters) = contracts::decode(endpoint, input)?;
        let account = self.account(
            selection
                .account
                .as_deref()
                .ok_or_else(|| error("INVALID_INPUT", "Derivative writes require an account."))?,
        )?;
        let derivative_permissions = self.derivative_permissions(account).await?;
        if matches!(
            endpoint,
            E::KrakenfuturesPrivatePostCancelorder
                | E::KrakenfuturesPrivatePostCancelallorders
                | E::KrakenfuturesPrivatePostCancelallordersafter
        ) {
            return self
                .prepare_derivative_control_with_scope(endpoint, input, parameters, None)
                .await;
        }
        if endpoint == E::KrakenfuturesPrivatePostEditorder {
            return Box::pin(self.prepare_derivative_edit(input, None)).await;
        }
        if endpoint == E::KrakenfuturesPrivatePostBatchorder {
            return Box::pin(self.prepare_derivative_batch(input, parameters)).await;
        }
        if endpoint != E::KrakenfuturesPrivatePostSendorder {
            return Err(error(
                "KRAKEN_DERIVATIVE_CONTROL_PROOF_PENDING",
                "Derivative edit and mixed batch require original order-history identity and cumulative exposure proof; no replacement was dispatched.",
            ));
        }
        let symbol = parameters["symbol"]
            .as_str()
            .ok_or_else(|| error("INVALID_INPUT", "Derivative exact symbol missing."))?
            .to_owned();
        let instruments = self
            .json(E::KrakenfuturesPublicGetInstruments, None, json!({}))
            .await?;
        let instrument = instruments["instruments"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["symbol"] == symbol))
            .ok_or_else(|| {
                error(
                    "KRAKEN_INSTRUMENT_UNKNOWN",
                    "Exact derivative instrument is unavailable.",
                )
            })?;
        if instrument["tradeable"] != true || instrument["isExpired"] == true {
            return Err(error(
                "KRAKEN_INSTRUMENT_UNAVAILABLE",
                "The exact derivative contract is not tradeable.",
            ));
        }
        let size = positive(&parameters["size"])?;
        let decimals = instrument["contractValueTradePrecision"]
            .as_u64()
            .filter(|n| *n <= 36)
            .ok_or_else(|| {
                error(
                    "KRAKEN_CONTRACT_PRECISION_UNKNOWN",
                    "Contract precision is unavailable.",
                )
            })?;
        let quantum = Quantity::new(format!("1{}", "0".repeat(36 - decimals as usize)))?;
        if &size.integer() % quantum.integer() != num_bigint::BigUint::from(0u8) {
            return Err(error(
                "INVALID_INPUT",
                "Exact contract size violates observed trade precision.",
            ));
        }
        if size > positive(&instrument["maxPositionSize"])? {
            return Err(error(
                "KRAKEN_EXPOSURE_LIMIT",
                "Exact contract size exceeds the venue market exposure limit.",
            ));
        }
        let buy = match parameters["side"].as_str() {
            Some("buy") => true,
            Some("sell") => false,
            _ => {
                return Err(error(
                    "INVALID_INPUT",
                    "Derivative side must be buy or sell.",
                ));
            }
        };
        let order_type = parameters["orderType"]
            .as_str()
            .ok_or_else(|| error("INVALID_INPUT", "Derivative order type missing."))?;
        if !matches!(
            order_type,
            "lmt" | "post" | "ioc" | "fok" | "mkt" | "stp" | "take_profit" | "trailing_stop"
        ) {
            return Err(error("INVALID_INPUT", "Unsupported derivative order type."));
        }
        let tick = positive(&instrument["tickSize"])?;
        let mut observed_fee_price = None;
        let price = if parameters["limitPrice"].is_null() {
            if matches!(order_type, "lmt" | "post" | "ioc" | "fok") {
                return Err(error(
                    "INVALID_INPUT",
                    "This derivative order requires an exact limitPrice.",
                ));
            }
            let ticker = self
                .json(
                    E::KrakenfuturesPublicGetTickersSymbol,
                    None,
                    json!({"symbol":symbol}),
                )
                .await?;
            let ticker = &ticker["ticker"];
            if ticker["symbol"] != symbol || ticker["suspended"] == true {
                return Err(error(
                    "KRAKEN_INSTRUMENT_UNAVAILABLE",
                    "Exact derivative book is unavailable or suspended.",
                ));
            }
            let bid = positive(&ticker["bid"])?;
            let ask = positive(&ticker["ask"])?;
            if ask < bid {
                return Err(error(
                    "KRAKEN_MARKET_EVIDENCE_INVALID",
                    "Derivative book has reversed bid and ask prices.",
                ));
            }
            // This is an independent admission estimate, never a guarantee or
            // a fabricated native collar. Actual collateral fees require receipts.
            let mut quote = if instrument["type"] == "futures_inverse" {
                bid
            } else {
                ask
            };
            if let Some(trigger) = parameters.get("stopPrice") {
                let trigger = positive(trigger)?;
                quote = if instrument["type"] == "futures_inverse" {
                    quote.min(trigger)
                } else {
                    quote.max(trigger)
                };
            }
            observed_fee_price = Some(quote.clone());
            quote
        } else {
            let price = positive(&parameters["limitPrice"])?;
            if &price.integer() % tick.integer() != num_bigint::BigUint::from(0u8) {
                return Err(error(
                    "INVALID_INPUT",
                    "Derivative limit price violates the exact instrument tick.",
                ));
            }
            price
        };
        if matches!(order_type, "stp" | "take_profit") {
            let trigger = positive(&parameters["stopPrice"])?;
            if &trigger.integer() % tick.integer() != num_bigint::BigUint::from(0u8) {
                return Err(error(
                    "INVALID_INPUT",
                    "Derivative trigger price violates exact contract tick.",
                ));
            }
        }
        if order_type == "trailing_stop" {
            let deviation = positive(&parameters["trailingStopMaxDeviation"])?;
            let unit = parameters["trailingStopDeviationUnit"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "INVALID_INPUT",
                        "Trailing orders require an explicit deviation unit.",
                    )
                })?;
            if !matches!(unit, "PERCENT" | "QUOTE_CURRENCY") {
                return Err(error(
                    "INVALID_INPUT",
                    "Trailing deviation unit must be PERCENT or QUOTE_CURRENCY.",
                ));
            }
            if unit == "PERCENT" && deviation >= Quantity::from_decimal("100", 36)? {
                return Err(error(
                    "INVALID_INPUT",
                    "Trailing percentage must be below 100.",
                ));
            }
        }
        let wallets = self
            .json(E::KrakenfuturesPrivateGetAccounts, Some(account), json!({}))
            .await?;
        let map = wallets["accounts"].as_object().ok_or_else(|| {
            error(
                "KRAKEN_COLLATERAL_UNKNOWN",
                "Derivative collateral wallets are unavailable.",
            )
        })?;
        let contract_type = instrument["type"].as_str().unwrap_or("");
        let (wallet, collateral_code, notional_asset, notional) = match contract_type {
            "futures_vanilla" | "flexible_futures" => {
                let base = instrument["base"].as_str().ok_or_else(|| {
                    error(
                        "KRAKEN_CONTRACT_IDENTITY_UNKNOWN",
                        "Linear contract base denomination missing.",
                    )
                })?;
                let units = size.mul_div_ceil(
                    &positive(&instrument["contractSize"])?,
                    &Quantity::new(format!("1{}", "0".repeat(36)))?,
                )?;
                ("flex".to_owned(), "USD".to_owned(), asset(base), units)
            }
            "futures_inverse" => {
                let prefix = symbol
                    .split('_')
                    .take(2)
                    .collect::<Vec<_>>()
                    .join("_")
                    .to_ascii_lowercase();
                let wallets = map
                    .iter()
                    .filter(|(key, record)| {
                        **key == prefix || record["balances"].get(&symbol).is_some()
                    })
                    .collect::<Vec<_>>();
                if wallets.len() != 1 {
                    return Err(error(
                        "KRAKEN_COLLATERAL_UNKNOWN",
                        "Inverse contract settlement wallet is missing or ambiguous.",
                    ));
                }
                let wallet = wallets[0].0.clone();
                let currency = map[&wallet]["currency"]
                    .as_str()
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_COLLATERAL_UNKNOWN",
                            "Inverse settlement currency is unavailable.",
                        )
                    })?
                    .to_owned();
                let face = size.mul_div_ceil(
                    &positive(&instrument["contractSize"])?,
                    &Quantity::new(format!("1{}", "0".repeat(36)))?,
                )?;
                (wallet, currency, asset("USD"), face)
            }
            _ => {
                return Err(error(
                    "KRAKEN_CONTRACT_SEMANTICS_PENDING",
                    "This instrument's contract notional and settlement denomination is not reviewed.",
                ));
            }
        };
        let wallet_record = map.get(&wallet).ok_or_else(|| {
            error(
                "KRAKEN_COLLATERAL_UNKNOWN",
                "Exact derivative settlement wallet is missing.",
            )
        })?;
        if wallet == "flex" {
            crate::derivative_fee_units::require_supported_preferred_fee_unit(wallet_record)?;
            crate::derivative_fee_units::nonnegative_spendable(
                &wallet_record["currencies"][&collateral_code]["available"],
            )?;
        }
        let collateral = asset(&collateral_code);
        let resource = ResourceId(format!("{}:wallet:{wallet}", account.resource.0));
        let positions = self
            .json(
                E::KrakenfuturesPrivateGetOpenpositions,
                Some(account),
                json!({}),
            )
            .await?;
        let matches = positions["openPositions"]
            .as_array()
            .ok_or_else(|| {
                error(
                    "KRAKEN_POSITION_UNKNOWN",
                    "Derivative position state unavailable.",
                )
            })?
            .iter()
            .filter(|p| p["symbol"] == symbol)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(error(
                "KRAKEN_POSITION_AMBIGUOUS",
                "Derivative net position identity is ambiguous.",
            ));
        }
        let before = if let Some(p) = matches.first() {
            let q = atoms(&p["size"])?;
            match p["side"].as_str() {
                Some("long") => SignedQuantity::positive(&q).integer(),
                Some("short") => SignedQuantity::negative(&q).integer(),
                _ => {
                    return Err(error(
                        "KRAKEN_POSITION_UNKNOWN",
                        "Derivative direction unavailable.",
                    ));
                }
            }
        } else {
            BigInt::zero()
        };
        let after = before.clone()
            + if buy {
                BigInt::from(size.integer())
            } else {
                -BigInt::from(size.integer())
            };
        if after.magnitude() > &positive(&instrument["maxPositionSize"])?.integer() {
            return Err(error(
                "KRAKEN_EXPOSURE_LIMIT",
                "The observed existing position plus this order exceeds the exact venue exposure limit.",
            ));
        }
        let reduce = parameters["reduceOnly"] == true;
        if reduce
            && ((buy && !before.is_negative())
                || (!buy && !before.is_positive())
                || size.integer() > before.magnitude().clone())
        {
            return Err(error(
                "KRAKEN_REDUCE_ONLY_BOUND",
                "A reduce-only order must reduce the observed opposite position without reversing it.",
            ));
        }
        let negative = |text: Option<&str>| -> Result<Quantity> {
            Quantity::from_decimal(text.ok_or_else(||error("KRAKEN_SETTLEMENT_BOUNDS_REQUIRED","Derivative orders require explicit maximumLoss, maximumProfit, maximumFundingLoss and maximumFundingProfit in the exact settlement currency."))?,36)
        };
        let loss = negative(control.maximum_loss.as_deref())?;
        let profit = negative(control.maximum_profit.as_deref())?;
        let funding_loss = negative(control.maximum_funding_loss.as_deref())?;
        let funding_profit = negative(control.maximum_funding_profit.as_deref())?;
        let mut intent = FinancialIntent {
            locks: vec![account.resource.clone(), resource.clone()],
            ..Default::default()
        };
        for kind in [PositionKind::DerivativeLong, PositionKind::DerivativeShort] {
            intent.expected.push(EffectConstraint::DerivativePosition {
                resource: account.resource.clone(),
                position: symbol.clone(),
                asset: contract(&symbol),
                position_kind: kind,
                minimum_delta: SignedQuantity::negative(&size),
                maximum_delta: SignedQuantity::positive(&size),
            });
        }
        for (category, low, high) in [
            (SettlementCategory::RealizedPnl, loss.clone(), profit),
            (
                SettlementCategory::Funding,
                funding_loss.clone(),
                funding_profit,
            ),
        ] {
            intent.expected.push(EffectConstraint::Settlement {
                resource: resource.clone(),
                asset: collateral.clone(),
                category,
                minimum_delta: SignedQuantity::negative(&low),
                maximum_delta: SignedQuantity::positive(&high),
            });
        }
        intent.expected.push(EffectConstraint::Credit {
            to: resource.clone(),
            asset: collateral.clone(),
            minimum_credit: Quantity::zero(),
        });
        let losses = sum(&loss, &funding_loss)?;
        intent.debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: collateral.clone(),
                atoms: losses.clone(),
            },
        });
        for (code, cap) in &control.maximum_fees {
            if code != &collateral_code {
                return Err(error(
                    "KRAKEN_DERIVATIVE_FEE_ASSET_PENDING",
                    "Derivative fee conversion requires independent collateral conversion bookings; select the exact current settlement fee asset.",
                ));
            }
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset(code),
                    atoms: Quantity::from_decimal(cap, 36)?,
                },
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        if intent.maximum_costs.is_empty() {
            return Err(error(
                "KRAKEN_FEE_BOUNDS_REQUIRED",
                "Declare maximumFees in the exact derivative settlement asset.",
            ));
        }
        if !losses.is_zero() {
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: collateral.clone(),
                    atoms: losses.clone(),
                },
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        // New fee service requires a separately enrolled Spot credential, not an old public schedule.
        let mut fee_accounts = Vec::new();
        for candidate in self
            .related_accounts
            .iter()
            .filter(|a| a.product == KrakenProduct::Spot)
        {
            let permission = self.spot_permissions(candidate, &["query-funds"]).await?;
            let venue_verified = derivative_permissions["iiban"]
                .as_str()
                .is_some_and(|id| !id.is_empty() && permission["iiban"] == id);
            let unavailable = derivative_permissions["iiban"]
                .as_str()
                .is_none_or(str::is_empty)
                || permission["iiban"].as_str().is_none_or(str::is_empty);
            if venue_verified
                || (unavailable && candidate.physical_identity == account.physical_identity)
            {
                fee_accounts.push((candidate, venue_verified));
            }
        }
        if fee_accounts.len() != 1 {
            return Err(error(
                "KRAKEN_FUTURES_FEE_ACCOUNT_REQUIRED",
                "Enroll one Spot account whose authenticated IIBAN matches this derivative account, or explicitly confirm their relationship when the venue omits it.",
            ));
        }
        let (fee_account, venue_verified) = fee_accounts[0];
        let rates = self
            .json(
                E::KrakenPrivatePostTradeVolume,
                Some(fee_account),
                json!({"pair":[{"asset":symbol,"aclass":"futures_contract"}],"fee_schedule":true}),
            )
            .await?;
        let rates = crate::observation::result(&rates);
        let maker =
            crate::derivative_fee_units::positive_fee_rate(&rates["fees_maker"][&symbol]["fee"])?;
        let taker = crate::derivative_fee_units::positive_fee_rate(&rates["fees"][&symbol]["fee"])?;
        let rate = maker.max(taker);
        let fee_notional = if contract_type == "futures_inverse" {
            notional.mul_div_ceil(&Quantity::new(format!("1{}", "0".repeat(36)))?, &price)?
        } else {
            notional.mul_div_ceil(&price, &Quantity::new(format!("1{}", "0".repeat(36)))?)?
        };
        let required_fee =
            fee_notional.mul_div_ceil(&rate, &Quantity::new(format!("100{}", "0".repeat(36)))?)?;
        let cap = Quantity::from_decimal(
            control.maximum_fees.get(&collateral_code).ok_or_else(|| {
                error(
                    "KRAKEN_FEE_BOUNDS_REQUIRED",
                    "Exact derivative commission asset fee bound missing.",
                )
            })?,
            36,
        )?;
        if cap < required_fee {
            return Err(error(
                "KRAKEN_FEE_BOUND_EXCEEDED",
                "Declared derivative commission cap is below current protected maker/taker rates at the bounded notional.",
            ));
        }
        let unit = Quantity::new(format!("1{}", "0".repeat(36)))?;
        let position_size = Quantity::new(after.magnitude().to_string())?
            .max(Quantity::new(before.magnitude().to_string())?);
        let face = position_size.mul_div_ceil(&positive(&instrument["contractSize"])?, &unit)?;
        let quote_units = if contract_type == "futures_inverse" {
            face
        } else {
            face.mul_div_ceil(&price, &unit)?
        };
        let mut margin_rate = initial_margin_rate(instrument, &position_size, &quote_units)?;
        let mut leverage_preference = Value::Null;
        if wallet == "flex" {
            let preferences = self
                .json(
                    E::KrakenfuturesPrivateGetLeveragepreferences,
                    Some(account),
                    json!({}),
                )
                .await?;
            let matches = preferences["leveragePreferences"]
                .as_array()
                .ok_or_else(|| {
                    error(
                        "KRAKEN_INITIAL_MARGIN_UNKNOWN",
                        "Protected leverage preferences are unavailable.",
                    )
                })?
                .iter()
                .filter(|p| p["symbol"] == symbol)
                .collect::<Vec<_>>();
            if matches.len() > 1 {
                return Err(error(
                    "KRAKEN_INITIAL_MARGIN_UNKNOWN",
                    "Exact contract leverage preference is ambiguous.",
                ));
            }
            if let Some(preference) = matches.first() {
                leverage_preference = (*preference).clone();
                if !preference["maxLeverage"].is_null() {
                    let leverage = positive(&preference["maxLeverage"])?;
                    margin_rate = margin_rate.max(unit.mul_div_ceil(&unit, &leverage)?);
                }
            }
        }
        let initial_margin = if reduce {
            Quantity::zero()
        } else {
            fee_notional.mul_div_ceil(&margin_rate, &unit)?
        };
        let available_margin = if wallet == "flex" {
            atoms(&wallet_record["availableMargin"])?
        } else {
            atoms(&wallet_record["auxiliary"]["af"])?
        };
        if available_margin < initial_margin.checked_add(&losses)?.checked_add(&cap)? {
            return Err(error(
                "INSUFFICIENT_MARGIN",
                "Observed available margin cannot cover the conservative applicable initial requirement plus declared loss, funding and commission reserves.",
            ));
        }
        let balance = if wallet == "flex" {
            crate::derivative_fee_units::nonnegative_spendable(
                &wallet_record["currencies"][&collateral_code]["available"],
            )?
        } else {
            atoms(&wallet_record["auxiliary"]["af"])?
        };
        if balance < losses.checked_add(&cap)? {
            return Err(error(
                "INSUFFICIENT_COLLATERAL",
                "Existing settlement collateral cannot reserve declared realized losses, funding and commission.",
            ));
        }
        let id = parameters["cliOrdId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "fin{}",
                    &hex::encode(Sha256::digest(format!(
                        "fin-krakenfutures-v1:{}:{}",
                        account.physical_identity, control.request_id
                    )))[..28]
                )
            });
        parameters["cliOrdId"] = json!(id);
        let mut valuation = self
            .financial_valuation(
                &[ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset: notional_asset,
                        atoms: notional,
                    },
                }],
                &intent.maximum_costs,
            )
            .await?;
        // Settlement loss reserves are policy costs as well as cash debits.
        valuation.additional_allocation_usd_micros = Quantity::zero();
        let mut prepared=PreparedOperation{operation:endpoint.operation().into(),input:input.clone(),binding:json!({"endpoint":endpoint.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":parameters,"requestId":control.request_id,"derivative":{"clientId":id,"symbol":symbol,"wallet":wallet,"collateral":collateral,"size":size,"positionBefore":before.to_string(),"positionLimit":atoms(&instrument["maxPositionSize"])? ,"minimumRequiredFeeAtoms":required_fee,"observedFeePrice":observed_fee_price,"initialMarginRate":margin_rate,"initialMargin":initial_margin,"leveragePreference":leverage_preference,"accountUid":derivative_permissions["accountUid"],"feeSourceAccount":fee_account.name,"feeRelationship":if venue_verified{"venue_verified"}else{"human_confirmed"},"rebateCreditPermitted":true}}),summary:format!("Place exact Kraken derivative {symbol} order on {}. Contract exposure, realized collateral and funding are accounted separately.",account.name),checks:vec![],warnings:vec!["Initial margin is checked against the maximum applicable published customer/platform schedule and protected isolated leverage preferences.".into(),"Standing derivative positions can accrue funding and be liquidated independently of Fin; revoking Fin consent does not cancel the venue order. Kraken offers no hard native commission ceiling; Fin reserves exact fee caps and retains unresolved receipts if a better price or conversion causes an unexpected excess.".into()],observed_at:fin_domain::now(),expires_at:(Utc::now()+Duration::seconds(90)).to_rfc3339(),valuation:Some(valuation),intent,action_deadline: None, operation_version:1};
        let baseline=[(PositionKind::DerivativeLong,if before.is_positive(){before.clone()}else{BigInt::zero()}),(PositionKind::DerivativeShort,if before.is_negative(){-before.clone()}else{BigInt::zero()})].into_iter().map(|(kind,value)|->Result<PositionAnchor>{Ok(PositionAnchor{resource:account.resource.clone(),position:symbol.clone(),position_kind:kind,principal:Amount{asset:contract(&symbol),atoms:Quantity::new(value.to_string())?},evidence_version:1,scaled_receipt:None,evidence:json!({"endpoint":"private_get_openpositions","account":account.name,"physicalIdentity":account.physical_identity,"symbol":symbol,"netPositionAtoms":before.to_string()}),observed_at:prepared.observed_at.clone()})}).collect::<Result<Vec<_>>>()?;
        prepared.binding[fin_domain::BASELINE_ANCHORS_KEY] = json!(baseline);
        if observed_fee_price.is_some() {
            prepared.warnings.push("This native market or trigger order has no limit price. Fee admission uses the exact observed venue book and trigger; execution may differ, and unexpected fee excess remains unresolved with its receipts.".into());
        }
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(crate) async fn reconcile_derivatives(
        &self,
        _record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation> {
        self.reconcile_derivatives_binding(_record, submission, &submission.data["binding"])
            .await
    }
    pub(crate) async fn reconcile_derivatives_binding(
        &self,
        _record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
    ) -> Result<Reconciliation> {
        if binding["unifiedNativePartitions"].is_array() {
            return Box::pin(self.reconcile_derivative_unified(_record, submission, binding)).await;
        }
        if binding["derivativeEdit"] == true {
            return self
                .reconcile_derivative_edit(_record, submission, binding)
                .await;
        }
        if binding["groupedAmendments"].is_object() {
            return Box::pin(self.reconcile_derivative_grouped_edits(_record, submission, binding))
                .await;
        }
        if binding["derivativeBatch"].is_array() {
            return Box::pin(self.reconcile_derivative_batch(_record, submission, binding)).await;
        }
        if binding["derivativeControl"] == true {
            return self
                .reconcile_derivative_control(_record, submission, binding)
                .await;
        }
        self.reconcile_derivative_order(_record, submission, binding, false)
            .await
    }
    pub(crate) async fn reconcile_derivative_order(
        &self,
        _record: &OperationRecord,
        submission: &Submission,
        binding: &Value,
        grouped: bool,
    ) -> Result<Reconciliation> {
        let bound = &binding["derivative"];
        let account = self.account(binding["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original derivative account missing.",
            )
        })?)?;
        if binding["physicalIdentity"] != account.physical_identity {
            return Err(error(
                "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                "Original derivative physical identity changed.",
            ));
        }
        let client = bound["clientId"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original derivative client identity missing.",
            )
        })?;
        let symbol = bound["symbol"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original derivative symbol missing.",
            )
        })?;
        let since = chrono::DateTime::parse_from_rfc3339(
            submission.data["submittedAt"].as_str().ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original submission time missing.",
                )
            })?,
        )
        .map_err(|_| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original submission time invalid.",
            )
        })?
        .timestamp_millis()
            - 30_000;
        let history=self.derivatives_history(E::KrakenfuturesHistoryGetOrders,json!({"__account":account.name,"since":since,"sort":"asc","count":1000,"tradeable":symbol})).await?;
        let mut order_id = None;
        let mut filled = None;
        let mut terminal = false;
        let mut order_proof = vec![];
        for row in &history {
            let Some(event) = row["event"].as_object() else {
                continue;
            };
            for (kind, detail) in event {
                let order = if kind == "OrderUpdated" {
                    &detail["newOrder"]
                } else {
                    &detail["order"]
                };
                if order["clientId"] != client
                    || (bound["orderId"].is_string() && bound["orderId"] != order["uid"])
                {
                    continue;
                }
                let created = order["timestamp"].as_i64().ok_or_else(|| {
                    error(
                        "KRAKEN_ORDER_PROOF_PENDING",
                        "Native derivative order creation timestamp is unavailable.",
                    )
                })?;
                let original_time = chrono::DateTime::parse_from_rfc3339(
                    submission.data["submittedAt"].as_str().unwrap(),
                )
                .map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Original derivative timestamp unavailable.",
                    )
                })?;
                if i128::from(created) * 1_000_000
                    < i128::from(original_time.timestamp()) * 1_000_000_000
                        + i128::from(original_time.timestamp_subsec_nanos())
                {
                    continue;
                }
                if order["accountUid"] != bound["accountUid"]
                    || bound["accountUid"].as_str().is_none_or(str::is_empty)
                {
                    return Err(error(
                        "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                        "Derivative history account UID differs from the protected authenticated UID frozen at approval.",
                    ));
                }
                if order["tradeable"] != symbol {
                    return Err(error(
                        "KRAKEN_ORDER_IDENTITY_MISMATCH",
                        "Derivative order history changed the exact market.",
                    ));
                }
                let id = order["uid"].as_str().ok_or_else(|| {
                    error(
                        "KRAKEN_ORDER_PROOF_PENDING",
                        "Order history unique identity unavailable.",
                    )
                })?;
                if order_id.as_deref().is_some_and(|old| old != id) {
                    return Err(error(
                        "KRAKEN_ORDER_IDENTITY_AMBIGUOUS",
                        "Client identity refers to multiple derivative orders.",
                    ));
                }
                order_id = Some(id.to_owned());
                filled = Some(atoms(&order["filled"])?);
                terminal = matches!(kind.as_str(), "OrderCancelled" | "OrderRejected")
                    || filled.as_ref() == Some(&atoms(&order["quantity"])?);
                order_proof.push(row.clone());
            }
        }
        let Some(order_id) = order_id else {
            return Ok(unresolved(
                Status::Unknown,
                "Original derivative order is not yet visible in durable history; no replacement was submitted.",
                json!({}),
            ));
        };
        let filled = filled.unwrap_or_default();
        let executions=self.derivatives_history(E::KrakenfuturesHistoryGetExecutions,json!({"__account":account.name,"since":since,"sort":"asc","count":1000,"tradeable":symbol})).await?;
        let positions=self.derivatives_history(E::KrakenfuturesHistoryGetPositions,json!({"__account":account.name,"since":since,"sort":"asc","count":1000,"tradeable":symbol})).await?;
        let collateral: AssetId =
            serde_json::from_value(bound["collateral"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original collateral asset missing.",
                )
            })?;
        let wallet = bound["wallet"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Original collateral wallet missing.",
            )
        })?;
        let resource = ResourceId(format!("{}:wallet:{wallet}", account.resource.0));
        let original_before =
            SignedQuantity::new(bound["positionBefore"].as_str().ok_or_else(|| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original derivative position binding missing.",
                )
            })?)?
            .integer();
        let mut previous_position = original_before;
        let mut events = vec![];
        let mut executed = Quantity::zero();
        let mut long_delta = BigInt::zero();
        let mut short_delta = BigInt::zero();
        let mut commission = crate::derivative_fee_units::GrossCommission::default();
        let mut pnl = BigInt::zero();
        let mut funding = BigInt::zero();
        let mut pnl_loss = Quantity::zero();
        let mut funding_loss = Quantity::zero();
        let mut last_long = BigInt::zero();
        let mut last_short = BigInt::zero();
        let mut ids = BTreeSet::new();
        for row in executions {
            let e = &row["event"]["execution"]["execution"];
            if e["order"]["uid"] != order_id {
                continue;
            }
            let id = e["uid"].as_str().ok_or_else(|| {
                error(
                    "KRAKEN_EXECUTION_PROOF_PENDING",
                    "Derivative execution identity unavailable.",
                )
            })?;
            if !ids.insert(id.to_owned()) {
                return Err(error(
                    "KRAKEN_EXECUTION_DUPLICATE",
                    "Derivative history repeated an execution identity.",
                ));
            }
            if e["order"]["tradeable"] != symbol || e["order"]["clientId"] != client {
                return Err(error(
                    "KRAKEN_EXECUTION_IDENTITY_MISMATCH",
                    "Derivative execution belongs to a different exact market or client.",
                ));
            }
            let matching = positions
                .iter()
                .filter(|p| p["event"]["PositionUpdate"]["executionUid"] == id)
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                return Ok(unresolved(
                    Status::Pending,
                    "One exact execution requires a unique position and collateral realization proof.",
                    json!({"executionId":id}),
                ));
            }
            let p = &matching[0]["event"]["PositionUpdate"];
            if p["tradeable"] != symbol
                || !crate::derivative_fee_units::native_currency_matches(
                    &collateral.reference,
                    &p["feeCurrency"],
                )
                || p["tradeType"] != "userExecution"
                || p["updateReason"] != "trade"
            {
                return Err(error(
                    "KRAKEN_POSITION_IDENTITY_MISMATCH",
                    "Position proof is not an ordinary trade in the exact approved collateral currency.",
                ));
            }
            let before = signed(&p["oldPosition"])?;
            let after = signed(&p["newPosition"])?;
            let quantity = atoms(&e["quantity"])?;
            if !grouped && before != previous_position {
                return Err(error(
                    "KRAKEN_POSITION_CHAIN_CHANGED",
                    "Derivative fills do not form a contiguous chain from the approved initial contract position.",
                ));
            }
            let movement = after.clone() - before.clone();
            if movement.is_negative() != (binding["parameters"]["side"] == "sell") {
                return Err(error(
                    "KRAKEN_EXECUTION_DIRECTION_MISMATCH",
                    "Derivative position transition reverses the exact approved order direction.",
                ));
            }
            if binding["parameters"]["reduceOnly"] == true
                && (before.is_zero()
                    || after.magnitude() > before.magnitude()
                    || (!after.is_zero() && after.is_negative() != before.is_negative()))
            {
                return Err(error(
                    "KRAKEN_REDUCE_ONLY_PROOF_MISMATCH",
                    "A supposedly reduce-only fill opened or reversed native exposure; keep the original receipts unresolved.",
                ));
            }
            previous_position = after.clone();
            if (after.clone() - before.clone()).magnitude() != &quantity.integer() {
                return Err(error(
                    "KRAKEN_POSITION_PROOF_PENDING",
                    "Execution size does not match its independently reported contract position transition.",
                ));
            }
            long_delta += delta(&before, &after, false)?.integer();
            short_delta += delta(&before, &after, true)?.integer();
            last_long = if after.is_positive() {
                after.clone()
            } else {
                BigInt::zero()
            };
            last_short = if after.is_negative() {
                -after.clone()
            } else {
                BigInt::zero()
            };
            commission.include(&p["fee"])?;
            let realized = signed(&p["realizedPnL"])?;
            let realized_funding = signed(&p["realizedFunding"])?;
            if realized.is_negative() {
                pnl_loss =
                    pnl_loss.checked_add(&Quantity::new(realized.magnitude().to_string())?)?;
            }
            if realized_funding.is_negative() {
                funding_loss = funding_loss
                    .checked_add(&Quantity::new(realized_funding.magnitude().to_string())?)?;
            }
            pnl += &realized;
            funding += &realized_funding;
            executed = executed.checked_add(&quantity)?;
            events.push(json!({"eventId":format!("krakenfutures:execution:{id}"),"execution":e,"positionUpdate":p}));
        }
        if executed != filled {
            return Ok(unresolved(
                Status::Pending,
                "Execution events do not yet establish the full order cumulative filled quantity.",
                json!({"filled":filled,"proved":executed}),
            ));
        }
        if executed
            > serde_json::from_value::<Quantity>(bound["size"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original derivative size missing.",
                )
            })?
        {
            return Err(error(
                "KRAKEN_EXPOSURE_BOUND_EXCEEDED",
                "Original derivative exposure quantity exceeded its approved size.",
            ));
        }
        if executed.is_zero() {
            if terminal {
                return proof(
                    submission,
                    Status::Failed,
                    vec![],
                    json!({"orders":order_proof,"proofEvents":[]}),
                );
            }
            return Ok(unresolved(
                Status::Pending,
                "Original derivative order remains live without fills.",
                json!({"orders":order_proof}),
            ));
        }
        // Position history proves realization classification. Account log proves the
        // actual collateral booking; both are required before effects are posted.
        let mut entries = vec![];
        let mut parameters = json!({"since":since,"before":Utc::now().timestamp_millis(),"sort":"asc","count":1000,"info":["futures trade"],"conversion_details":true});
        for _ in 0..64 {
            let response = self
                .json(
                    E::KrakenfuturesHistoryGetAccountLog,
                    Some(account),
                    parameters.clone(),
                )
                .await?;
            let rows = response["logs"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_COLLATERAL_PROOF_PENDING",
                    "Exact collateral booking records are unavailable.",
                )
            })?;
            entries.extend(rows.iter().cloned());
            if ids.iter().all(|id| {
                entries.iter().any(|l| {
                    l["execution"] == *id
                        && l["margin_account"] == wallet
                        && crate::derivative_fee_units::native_currency_matches(
                            &collateral.reference,
                            &l["asset"],
                        )
                })
            }) || rows.len() < 1000
            {
                break;
            }
            let last = rows.last().and_then(|l| l["id"].as_u64()).ok_or_else(|| {
                error(
                    "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                    "Collateral ledger page cursor missing.",
                )
            })?;
            parameters["from"] = json!(last.checked_add(1).ok_or_else(|| error(
                "KRAKEN_HISTORY_COVERAGE_INCOMPLETE",
                "Collateral ledger cursor overflow."
            ))?);
        }
        let mut bookings = vec![];
        let mut adopted_booking_ids = BTreeSet::new();
        for id in &ids {
            let matches = entries
                .iter()
                .filter(|l| {
                    l["execution"] == *id
                        && l["margin_account"] == wallet
                        && crate::derivative_fee_units::native_currency_matches(
                            &collateral.reference,
                            &l["asset"],
                        )
                })
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Ok(unresolved(
                    Status::Pending,
                    "One exact derivative execution requires one attributable collateral booking; no balance delta inference is used.",
                    json!({"executionId":id}),
                ));
            }
            let l = matches[0];
            let booking_id = l["booking_uid"].as_str().filter(|id| !id.is_empty());
            if !booking_id.is_some_and(|id| adopted_booking_ids.insert(id.to_owned())) {
                return Ok(unresolved(
                    Status::Unknown,
                    "Each adopted execution requires a distinct immutable native collateral booking identity; no duplicate fee or credit is posted.",
                    json!({"executionId":id,"compatibilityCode":"KRAKEN_COLLATERAL_BOOKING_IDENTITY_INVALID"}),
                ));
            }
            let event = events
                .iter_mut()
                .find(|e| e["eventId"] == format!("krakenfutures:execution:{id}"))
                .ok_or_else(|| {
                    error(
                        "KRAKEN_EXECUTION_PROOF_PENDING",
                        "Correlated realization proof missing.",
                    )
                })?;
            let p = &event["positionUpdate"];
            if signed(&l["realized_pnl"])? != signed(&p["realizedPnL"])?
                || signed(&l["realized_funding"])? != signed(&p["realizedFunding"])?
                || crate::derivative_fee_units::signed_native(&l["fee"])?
                    != crate::derivative_fee_units::signed_native(&p["fee"])?
            {
                return Ok(unresolved(
                    Status::Unknown,
                    "Execution position realization and independent collateral booking disagree; preserve the original immutable proofs and reservations.",
                    json!({"compatibilityCode":"KRAKEN_COLLATERAL_PROOF_MISMATCH","executionId":id,"positionUpdate":p,"collateralBooking":l}),
                ));
            }
            if l["contract"].as_str().is_some_and(|c| {
                !crate::derivative_fee_units::native_log_contract_matches(symbol, c)
            }) {
                return Err(error(
                    "KRAKEN_COLLATERAL_IDENTITY_MISMATCH",
                    "Collateral booking contract differs from the original approved market.",
                ));
            }
            if let Err(reason) =
                crate::derivative_fee_units::require_same_unit_usable_booking(l, &collateral)
            {
                return Ok(unresolved(
                    Status::Pending,
                    &reason.message,
                    json!({"executionId":id,"compatibilityCode":reason.code,"booking":l}),
                ));
            }
            event["collateralBooking"] = l.clone();
            let expected = signed(&l["realized_pnl"])? + signed(&l["realized_funding"])?
                - crate::derivative_fee_units::signed_native(&l["fee"])?;
            if signed(&l["new_balance"])? - signed(&l["old_balance"])? != expected {
                return Err(error(
                    "KRAKEN_COLLATERAL_PROOF_MISMATCH",
                    "Collateral booking balance change does not equal its proved realized PnL, funding and fee.",
                ));
            }
            bookings.push(l.clone());
        }
        let mut effects = vec![];
        for (kind, change, post) in [
            (PositionKind::DerivativeLong, long_delta, last_long),
            (PositionKind::DerivativeShort, short_delta, last_short),
        ] {
            let amount = Amount {
                asset: contract(symbol),
                atoms: Quantity::new(post.to_string())?,
            };
            let anchor = PositionAnchor {
                resource: account.resource.clone(),
                position: symbol.into(),
                position_kind: kind,
                principal: amount,
                evidence_version: 1,
                scaled_receipt: None,
                evidence: json!({"executions":events,"collateralBookings":bookings}),
                observed_at: fin_domain::now(),
            };
            effects.push(FinancialEffect::PositionChange {
                resource: account.resource.clone(),
                position: symbol.into(),
                asset: contract(symbol),
                delta: SignedQuantity::new(change.to_string())?,
                scaled_receipt: None,
                anchor: Some(anchor),
            });
        }
        for (category, total, gross_loss) in [
            (SettlementCategory::RealizedPnl, pnl, pnl_loss),
            (SettlementCategory::Funding, funding, funding_loss),
        ] {
            let change = SignedQuantity::new(total.to_string())?;
            if !gross_loss.is_zero() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: collateral.clone(),
                        atoms: gross_loss,
                    },
                    category: format!("krakenfutures_{category:?}_loss"),
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
            effects.push(FinancialEffect::Settlement {
                resource: resource.clone(),
                asset: collateral.clone(),
                category,
                delta: change,
            });
        }
        if !commission.rebate.is_zero() {
            if bound["rebateCreditPermitted"] != true || !_record.preview.as_ref().is_some_and(|preview|preview.intent.expected.iter().any(|constraint|matches!(constraint,EffectConstraint::Credit{to,asset,..} if to==&resource && asset==&collateral))) {
                return Ok(unresolved(Status::Unknown,"The original pending scope did not authorize this usable native rebate credit; retain its original receipt without adding retroactive authority.",json!({"collateralBookings":bookings,"rebate":commission.rebate})));
            }
            effects.push(FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: collateral.clone(),
                    atoms: commission.rebate,
                },
            });
        }
        if !commission.paid.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset: collateral,
                    atoms: commission.paid,
                },
                category: "krakenfutures_commission".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        if !grouped && binding["_finBatchCostScope"] != true {
            let cost_bounds: Vec<CostBound> =
                if let Some(envelope) = binding.get("_finAccountingCostBounds") {
                    serde_json::from_value(envelope.clone()).map_err(|_| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Protected derivative accounting cost bounds are malformed.",
                        )
                    })?
                } else {
                    _record
                    .preview
                    .as_ref()
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_SUBMISSION_INVALID",
                            "Original protected derivative fee and loss ceilings are unavailable.",
                        )
                    })?
                    .intent
                    .maximum_costs
                    .clone()
                };
            if crate::unresolved_consumption::costs_exceed_bounds(&effects, &cost_bounds)? {
                return crate::unresolved_consumption::cost_overrun(
                    submission,
                    &effects,
                    json!({"code":"KRAKEN_COST_BOUND_EXCEEDED","orders":order_proof,"proofEvents":events,"collateralBookings":bookings}),
                );
            }
        }
        proof(
            submission,
            if terminal {
                Status::Completed
            } else {
                Status::Pending
            },
            effects,
            json!({"orders":order_proof,"proofEvents":events,"collateralBookings":bookings}),
        )
    }
}

#[cfg(test)]
mod margin_rate_tests {
    use super::*;
    #[test]
    fn margin_admission_does_not_assume_professional_or_lowest_platform_tier() {
        let i = json!({"marginLevels":[{"contracts":0,"initialMargin":"0.02"}],"retailMarginLevels":[{"contracts":0,"initialMargin":"0.2"}],"marginSchedules":{"other_platform":{"retail":[{"numNonContractUnits":0,"initialMargin":"0.25"},{"numNonContractUnits":10000,"initialMargin":"0.4"}],"professional":[{"initialMargin":"0.01"}]}}});
        assert_eq!(
            initial_margin_rate(
                &i,
                &Quantity::from_decimal("0.01", 36).unwrap(),
                &Quantity::from_decimal("200", 36).unwrap()
            )
            .unwrap(),
            Quantity::from_decimal("0.25", 36).unwrap()
        );
        assert_eq!(
            initial_margin_rate(
                &i,
                &Quantity::from_decimal("1", 36).unwrap(),
                &Quantity::from_decimal("20000", 36).unwrap()
            )
            .unwrap(),
            Quantity::from_decimal("0.4", 36).unwrap()
        );
    }
    #[test]
    fn missing_or_invalid_initial_margin_cannot_assume_leverage() {
        for i in [json!({}), json!({"marginLevels":[{"initialMargin":"1.1"}]})] {
            assert_eq!(
                initial_margin_rate(
                    &i,
                    &Quantity::from_decimal("1", 36).unwrap(),
                    &Quantity::from_decimal("20000", 36).unwrap()
                )
                .unwrap_err()
                .code,
                "KRAKEN_INITIAL_MARGIN_UNKNOWN"
            );
        }
    }
}
