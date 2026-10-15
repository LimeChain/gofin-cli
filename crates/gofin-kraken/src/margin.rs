//! Spot margin is a full venue extension secured by collateral, not a cash buy.
//! Native trade/position identities prove liability; attributed ledgers prove costs.
#[path = "margin_batch_edit.rs"]
mod batch_edit;
#[path = "conditional_margin_revision.rs"]
mod conditional_revision;
use crate::{
    KrakenEndpoint as E, contracts,
    observation::{atoms, exact, result},
    profile::{KrakenAccount, KrakenProfile, error},
    trading::{TradingControls, proof, unresolved},
};
use chrono::{Duration, Utc};
use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, Direction, EffectConstraint,
    FinancialEffect, FinancialIntent, OperationRecord, PositionAnchor, PositionKind, PositionState,
    PreparedOperation, Quantity, Reconciliation, ResourceAmount, Result, SettlementCategory,
    SignedQuantity, Status, Submission,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn warnings_from_fee_evidence(prepared: &mut PreparedOperation, evidence: &Value) {
    prepared.binding["marginFeeAdmission"] = evidence.clone();
    if let Some(disclosure) = evidence["disclosure"].as_str() {
        prepared.warnings.push(disclosure.into());
    }
}
fn scale() -> Quantity {
    Quantity::new(format!("1{}", "0".repeat(36))).expect("fixed scale")
}
fn product(a: &Quantity, b: &Quantity) -> Result<Quantity> {
    a.mul_div_ceil(b, &scale())
}
fn positive(v: &Value) -> Result<Quantity> {
    let q = atoms(v)?;
    if q.is_zero() {
        Err(error(
            "INVALID_INPUT",
            "Margin size and price must be positive.",
        ))
    } else {
        Ok(q)
    }
}
fn absolute_execution_price(value: &Value) -> Result<Quantity> {
    if value
        .as_str()
        .is_some_and(|text| text.starts_with(['+', '-', '#']) || text.ends_with('%'))
    {
        return Err(error(
            "KRAKEN_MARGIN_ABSOLUTE_LIMIT_REQUIRED",
            "A margin execution limit must be an absolute positive price; relative offsets cannot bound borrowing or loss.",
        ));
    }
    positive(value)
}
fn check_margin_execution_limit(parameters: &Value) -> Result<()> {
    let field = match parameters["ordertype"].as_str() {
        Some("limit") => Some("price"),
        Some("stop-loss-limit" | "take-profit-limit" | "trailing-stop-limit") => Some("price2"),
        _ => None,
    };
    if let Some(field) = field {
        absolute_execution_price(&parameters[field])?;
    } else {
        for field in ["price", "price2"] {
            if let Some(value) = parameters.get(field) {
                absolute_execution_price(value)?;
            }
        }
    }
    Ok(())
}
fn check_conditional_margin_revision_target(
    binding: &MarginBinding,
    parameters: &Value,
) -> Result<()> {
    if !binding
        .orders
        .iter()
        .any(|order| order.conditional_parent.is_some())
    {
        return Ok(());
    }
    let selected = |order: &MarginOrder| {
        parameters["txid"]
            .as_str()
            .is_some_and(|id| order.order_ids.iter().any(|known| known == id))
            || parameters["cl_ord_id"]
                .as_str()
                .is_some_and(|id| id == order.client_id)
            || parameters["txid"].as_i64().is_some_and(|reference| {
                order.user_reference.as_ref().and_then(Value::as_i64) == Some(reference)
            })
    };
    let parent = binding.orders.iter().any(|order| {
        order.conditional_parent.is_none()
            && selected(order)
            && binding
                .orders
                .iter()
                .any(|child| child.conditional_parent.as_ref() == Some(&order.client_id))
    });
    if parent {
        return Err(error("KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED","Kraken atomic amendment and legacy edit do not support a parent order with conditional-close terms attached. Preserve its original recovery operation; any issued child needs separate exact native ownership and funding proof.").with_details(json!({"source":"https://docs.kraken.com/exchange/guides/general/amends","legacySource":"https://docs.kraken.com/api-reference/trading/edit-order","venueUnsupported":true})));
    }
    Err(error(
        "KRAKEN_CONDITIONAL_CHILD_REVISION_PENDING",
        "An issued generated margin child requires independently correlated parent refid, native loan/position and remaining-credit revision proof before its separate amendment.",
    ))
}
fn signed(v: &Value) -> Result<SignedQuantity> {
    let value = exact(v)?;
    let negative = value.starts_with('-');
    let q = Quantity::from_decimal(value.trim_start_matches(['-', '+']), 36)?;
    Ok(if negative {
        SignedQuantity::negative(&q)
    } else {
        SignedQuantity::positive(&q)
    })
}
fn normalized(value: &Value) -> Result<Value> {
    let mut result = value.clone();
    fn walk(v: &mut Value) -> Result<()> {
        match v {
            Value::Object(m) => {
                for v in m.values_mut() {
                    walk(v)?
                }
            }
            Value::Array(a) => {
                for v in a {
                    walk(v)?
                }
            }
            Value::Number(n) if !n.is_i64() && !n.is_u64() => {
                *v = json!(exact(&Value::Number(n.clone()))?)
            }
            _ => {}
        }
        Ok(())
    }
    walk(&mut result)?;
    Ok(result)
}
fn position_key(pair: &str, buy: bool) -> String {
    format!("margin:{pair}:{}", if buy { "buy" } else { "sell" })
}
fn debt_key(pair: &str, buy: bool) -> String {
    format!("margin-loan:{pair}:{}", if buy { "buy" } else { "sell" })
}
fn kind(buy: bool) -> PositionKind {
    if buy {
        PositionKind::MarginLong
    } else {
        PositionKind::MarginShort
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct MarginTotals {
    pub(crate) volume: Quantity,
    pub(crate) loan: Quantity,
    records: BTreeMap<String, Value>,
}
impl Default for MarginTotals {
    fn default() -> Self {
        Self {
            volume: Quantity::zero(),
            loan: Quantity::zero(),
            records: BTreeMap::new(),
        }
    }
}
/// Kraken extends the full quote opening cost for a long and full base size for a
/// short. `margin` is collateral consumed, never subtracted from that extension.
pub(crate) fn position_totals(records: &Value, pair: &str, buy: bool) -> Result<MarginTotals> {
    let rows = records.as_object().ok_or_else(|| {
        error(
            "KRAKEN_MARGIN_EVIDENCE_INVALID",
            "Unconsolidated native margin positions are required.",
        )
    })?;
    let mut totals = MarginTotals::default();
    for (id, row) in rows {
        if row["pair"] != pair || row["type"] != if buy { "buy" } else { "sell" } {
            continue;
        }
        if row["posstatus"] != "open" {
            return Err(error(
                "KRAKEN_MARGIN_EVIDENCE_INVALID",
                "OpenPositions returned an inconsistent margin status.",
            ));
        }
        let volume = positive(&row["vol"])?;
        let closed = atoms(&row["vol_closed"])?;
        if closed > volume {
            return Err(error(
                "KRAKEN_MARGIN_EVIDENCE_INVALID",
                "Closed native position size exceeds its original volume.",
            ));
        }
        let remaining = volume.checked_sub(&closed)?;
        let loan = if buy {
            positive(&row["cost"])?.mul_div_ceil(&remaining, &volume)?
        } else {
            remaining.clone()
        };
        totals.volume = totals.volume.checked_add(&remaining)?;
        totals.loan = totals.loan.checked_add(&loan)?;
        totals.records.insert(id.clone(), normalized(row)?);
    }
    Ok(totals)
}
/// Closing may select native positions with different opening prices. Bound the
/// largest possible loan reduction without assuming FIFO or an average price.
fn maximum_close_loan(prior: &MarginTotals, volume: &Quantity, buy: bool) -> Result<Quantity> {
    if !buy {
        return Ok(volume.clone());
    }
    let mut positions = prior
        .records
        .values()
        .map(|row| {
            Ok((
                positive(&row["cost"])?,
                positive(&row["vol"])?,
                atoms(&row["vol_closed"])?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    positions.sort_by(|a, b| (b.0.integer() * a.1.integer()).cmp(&(a.0.integer() * b.1.integer())));
    let mut remaining = volume.clone();
    let mut maximum = Quantity::zero();
    for (cost, total, closed) in positions {
        if remaining.is_zero() {
            break;
        }
        let open = total.checked_sub(&closed)?;
        let taken = remaining.clone().min(open.clone());
        // Exact ceiling bounds every possible native allocation; actual repayment
        // is subsequently derived once from cumulative original-position reductions.
        maximum = maximum.checked_add(&cost.mul_div_ceil(&taken, &total)?)?;
        remaining = remaining.checked_sub(&taken)?;
    }
    if !remaining.is_zero() {
        return Err(error(
            "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED",
            "Closing size exceeds the native baseline.",
        ));
    }
    Ok(maximum.min(prior.loan.clone()))
}
fn anchor(
    a: &KrakenAccount,
    pair: &str,
    buy: bool,
    principal: Amount,
    debt: bool,
    records: &BTreeMap<String, Value>,
    observed: &str,
) -> PositionAnchor {
    PositionAnchor {
        resource: a.resource.clone(),
        position: if debt {
            debt_key(pair, buy)
        } else {
            position_key(pair, buy)
        },
        position_kind: if debt {
            PositionKind::VariableDebt
        } else {
            kind(buy)
        },
        principal,
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"provider":"kraken","account":a.name,"physicalIdentity":a.physical_identity,"pair":pair,"side":if buy{"buy"}else{"sell"},"positions":records,"loanMeaning":"full native extension; collateral is not deducted"}),
        observed_at: observed.into(),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MarginOrder {
    client_id: String,
    #[serde(default)]
    order_ids: Vec<String>,
    #[serde(default)]
    conditional_parent: Option<String>,
    #[serde(default)]
    conditional_parameters: Option<Value>,
    #[serde(default)]
    generated_positions: BTreeMap<String, Value>,
    pair: String,
    base: AssetId,
    quote: AssetId,
    buy: bool,
    volume: Quantity,
    maximum_loan: Quantity,
    leverage: u64,
    #[serde(default)]
    collateral_required: Quantity,
    #[serde(default)]
    user_reference: Option<Value>,
    closing: bool,
    settling: bool,
    prior: MarginTotals,
    prior_anchor: PositionAnchor,
    prior_debt_anchor: PositionAnchor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MarginBinding {
    orders: Vec<MarginOrder>,
    controls: TradingControls,
    validation_only: bool,
}

fn opening_principals(orders: &[MarginOrder]) -> Result<BTreeMap<AssetId, Quantity>> {
    let mut totals = BTreeMap::new();
    for order in orders
        .iter()
        .filter(|order| !order.closing && order.conditional_parent.is_none())
    {
        let asset = if order.buy { &order.quote } else { &order.base };
        let total = totals.entry(asset.clone()).or_insert_with(Quantity::zero);
        *total = total.checked_add(&order.maximum_loan)?;
    }
    Ok(totals)
}
pub(crate) fn opening_loan_principals(binding: &Value) -> Result<BTreeMap<AssetId, Quantity>> {
    let margin: MarginBinding =
        serde_json::from_value(binding["margin"].clone()).map_err(|_| {
            error(
                "KRAKEN_PREVIEW_INVALID",
                "Protected margin principal scope is missing.",
            )
        })?;
    opening_principals(&margin.orders)
}
fn check_principal_cap(
    endpoint: E,
    orders: &[MarginOrder],
    cap: Option<&str>,
) -> Result<Option<AssetId>> {
    let Some(cap) = cap else { return Ok(None) };
    let totals = opening_principals(orders)?;
    if totals.len() != 1 {
        return Err(contracts::unsupported_control(
            endpoint,
            vec!["/maximumSpend".into()],
        ));
    }
    let (asset, total) = totals.into_iter().next().unwrap();
    if total > Quantity::from_decimal(cap, 36)? {
        return Err(error(
            "KRAKEN_SPEND_BOUND_EXCEEDED",
            "The entire margin scope exceeds maximumSpend in its exact borrowed asset.",
        ));
    }
    Ok(Some(asset))
}

/// Collapse repeated native exposure scopes without collapsing request identities.
fn grouped_orders(orders: &[MarginOrder]) -> Result<Vec<MarginOrder>> {
    let mut groups: BTreeMap<(String, bool, bool, bool), MarginOrder> = BTreeMap::new();
    for order in orders {
        let key = (order.pair.clone(), order.buy, order.closing, order.settling);
        if let Some(group) = groups.get_mut(&key) {
            if group.base != order.base
                || group.quote != order.quote
                || group.prior.records != order.prior.records
            {
                return Err(error(
                    "KRAKEN_MARGIN_BATCH_SCOPE_AMBIGUOUS",
                    "A margin batch must retain one exact asset and immutable native baseline per exposure.",
                ));
            }
            group.volume = group.volume.checked_add(&order.volume)?;
            group.maximum_loan = group.maximum_loan.checked_add(&order.maximum_loan)?;
            for (id, row) in &order.generated_positions {
                if group
                    .generated_positions
                    .insert(id.clone(), row.clone())
                    .is_some()
                {
                    return Err(error(
                        "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                        "Generated parent position identity is duplicated.",
                    ));
                }
            }
            group.conditional_parent = group
                .conditional_parent
                .clone()
                .or(order.conditional_parent.clone());
            group.collateral_required = group
                .collateral_required
                .checked_add(&order.collateral_required)?;
        } else {
            groups.insert(key, order.clone());
        }
    }
    let mut close_volumes = BTreeMap::<(String, bool), Quantity>::new();
    for order in orders {
        if let Some(other) = orders.iter().find(|other| {
            other.pair == order.pair
                && other.buy == order.buy
                && (other.prior.records != order.prior.records
                    || other.base != order.base
                    || other.quote != order.quote)
        }) {
            let _ = other;
            return Err(error(
                "KRAKEN_MARGIN_BATCH_SCOPE_AMBIGUOUS",
                "Native exposure has conflicting asset or baseline identities.",
            ));
        }
        if order.closing && order.conditional_parent.is_none() {
            let total = close_volumes
                .entry((order.pair.clone(), order.buy))
                .or_default();
            *total = total.checked_add(&order.volume)?;
            if *total > order.prior.volume {
                return Err(error(
                    "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED",
                    "All closing and settling children together exceed the frozen native position.",
                ));
            }
        }
    }
    for group in groups.values_mut() {
        if group.closing && group.conditional_parent.is_none() {
            if group.volume > group.prior.volume {
                return Err(error(
                    "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED",
                    "The full batch closing quantity exceeds the frozen remaining native position.",
                ));
            }
            group.maximum_loan = maximum_close_loan(&group.prior, &group.volume, group.buy)?;
        }
    }
    Ok(groups.into_values().collect())
}
#[derive(Clone, Default)]
struct MarginProgress {
    volume: Quantity,
    loan: Quantity,
    reductions: BTreeMap<String, Quantity>,
    opening_ids: BTreeSet<String>,
    trades: serde_json::Map<String, Value>,
}

fn combined_exposure_progress(
    orders: &[MarginOrder],
    progress: &BTreeMap<(String, bool, bool, bool), MarginProgress>,
    pair: &str,
    buy: bool,
) -> Result<(Quantity, Quantity, MarginProgress)> {
    let prior = orders
        .iter()
        .find(|o| o.pair == pair && o.buy == buy)
        .ok_or_else(|| error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Native baseline missing."))?;
    let mut combined = MarginProgress::default();
    let mut opening_volume = Quantity::zero();
    let mut opening_loan = Quantity::zero();
    let mut closing_volume = Quantity::zero();
    for ((p, side, closing, _), entry) in progress {
        if p != pair || *side != buy {
            continue;
        }
        if *closing {
            closing_volume = closing_volume.checked_add(&entry.volume)?;
        } else {
            opening_volume = opening_volume.checked_add(&entry.volume)?;
            opening_loan = opening_loan.checked_add(&entry.loan)?;
        }
        for (id, reduction) in &entry.reductions {
            let total = combined.reductions.entry(id.clone()).or_default();
            *total = total.checked_add(reduction)?;
        }
        combined.opening_ids.extend(entry.opening_ids.clone());
        combined.trades.extend(entry.trades.clone());
    }
    let mut closing_loan = Quantity::zero();
    for (id, reduction) in &combined.reductions {
        let old = prior
            .prior
            .records
            .get(id)
            .or_else(|| {
                orders
                    .iter()
                    .find_map(|order| order.generated_positions.get(id))
            })
            .ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                    "Closing native loan has no original identity.",
                )
            })?;
        let volume = positive(&old["vol"])?;
        let before = volume.checked_sub(&atoms(&old["vol_closed"])?)?;
        let after = before.checked_sub(reduction)?;
        closing_loan = closing_loan.checked_add(&if buy {
            let cost = positive(&old["cost"])?;
            cost.mul_div_ceil(&before, &volume)?
                .checked_sub(&cost.mul_div_ceil(&after, &volume)?)?
        } else {
            reduction.clone()
        })?;
    }
    Ok((
        prior
            .prior
            .volume
            .checked_add(&opening_volume)?
            .checked_sub(&closing_volume)?,
        prior
            .prior
            .loan
            .checked_add(&opening_loan)?
            .checked_sub(&closing_loan)?,
        combined,
    ))
}

fn coalesce_cash_effects(effects: Vec<FinancialEffect>) -> Result<Vec<FinancialEffect>> {
    let mut merged: Vec<FinancialEffect> = vec![];
    for effect in effects.into_iter().filter(|e| !matches!(e,FinancialEffect::Cost{category,..} if category=="kraken_margin_realized_loss")) {
        let existing=merged.iter_mut().find(|old|match (&effect,&**old) {
            (FinancialEffect::Settlement{resource,asset,category,delta},FinancialEffect::Settlement{resource:r,asset:a,category:c,delta:d})=>resource==r&&asset==a&&category==c&&delta.is_negative()==d.is_negative(),
            (FinancialEffect::CashDebit{resource,amount},FinancialEffect::CashDebit{resource:r,amount:a})=>resource==r&&amount.asset==a.asset,
            (FinancialEffect::UsableCredit{to,amount},FinancialEffect::UsableCredit{to:r,amount:a})=>to==r&&amount.asset==a.asset,
            _=>false,
        });
        if let Some(existing)=existing {match(existing,effect) {
            (FinancialEffect::Settlement{delta,..},FinancialEffect::Settlement{delta:new,..})=>*delta=SignedQuantity::new((delta.integer()+new.integer()).to_string())?,
            (FinancialEffect::CashDebit{amount,..},FinancialEffect::CashDebit{amount:new,..})|(FinancialEffect::UsableCredit{amount,..},FinancialEffect::UsableCredit{amount:new,..})=>amount.atoms=amount.atoms.checked_add(&new.atoms)?,
            _=>unreachable!("typed matching merge"),
        }}else{merged.push(effect);}
    }
    // Preserve gross loss and profit independently: later profit cannot erase
    // already proved costs or replenish the approved whole-command loss budget.
    let losses = merged
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::Settlement {
                resource,
                asset,
                delta,
                ..
            } if delta.is_negative() => Some(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset.clone(),
                    atoms: delta.magnitude(),
                },
                category: "kraken_margin_realized_loss".into(),
                treatment: CostTreatment::IncludedInDebit,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    merged.extend(losses);
    let settlement_keys = merged
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::Settlement {
                resource, asset, ..
            } => Some((resource.clone(), asset.clone())),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for (resource, asset) in settlement_keys {
        let category = SettlementCategory::RealizedPnl;
        let count=merged.iter().filter(|e|matches!(e,FinancialEffect::Settlement{resource:r,asset:a,category:c,..} if r==&resource&&a==&asset&&c==&category)).count();
        if count == 1 {
            merged.push(FinancialEffect::Settlement {
                resource,
                asset,
                category,
                delta: SignedQuantity::positive(&Quantity::zero()),
            });
        }
    }
    // Nonzero signs must claim their bucket before indistinguishable zero placeholders.
    merged.sort_by_key(|e| matches!(e,FinancialEffect::Settlement{delta,..} if delta.is_zero()));
    Ok(merged)
}
impl KrakenProfile {
    pub(crate) async fn margin_snapshot_positions(
        &self,
        a: &KrakenAccount,
    ) -> Result<Vec<PositionState>> {
        let response = self
            .json(
                E::KrakenPrivatePostOpenPositions,
                Some(a),
                json!({"docalcs":false}),
            )
            .await?;
        let rows = result(&response).as_object().ok_or_else(|| {
            error(
                "KRAKEN_MARGIN_EVIDENCE_INVALID",
                "Margin position map is unavailable.",
            )
        })?;
        let mut grouped = BTreeSet::new();
        for row in rows.values() {
            let pair = row["pair"].as_str().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_EVIDENCE_INVALID",
                    "Margin position lacks its market identity.",
                )
            })?;
            let buy = match row["type"].as_str() {
                Some("buy") => true,
                Some("sell") => false,
                _ => {
                    return Err(error(
                        "KRAKEN_MARGIN_EVIDENCE_INVALID",
                        "Margin position lacks direction.",
                    ));
                }
            };
            grouped.insert((pair.to_string(), buy));
        }
        let mut positions = vec![];
        let observed = gofin_domain::now();
        for (pair, buy) in grouped {
            let metadata = self
                .json(E::KrakenPublicGetAssetPairs, None, json!({"pair":pair}))
                .await?;
            let records = result(&metadata).as_object().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_EVIDENCE_INVALID",
                    "Margin market identities missing.",
                )
            })?;
            if records.len() != 1 {
                return Err(error(
                    "KRAKEN_MARGIN_EVIDENCE_INVALID",
                    "Margin market must resolve uniquely.",
                ));
            }
            let market = records.values().next().unwrap();
            let base = self
                .spot_asset(
                    a,
                    market["base"].as_str().ok_or_else(|| {
                        error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Margin base missing.")
                    })?,
                )
                .await?;
            let quote = self
                .spot_asset(
                    a,
                    market["quote"].as_str().ok_or_else(|| {
                        error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Margin quote missing.")
                    })?,
                )
                .await?;
            let totals = position_totals(result(&response), &pair, buy)?;
            let loan_asset = if buy { &quote } else { &base };
            let position_anchor = anchor(
                a,
                &pair,
                buy,
                Amount {
                    asset: base.clone(),
                    atoms: totals.volume.clone(),
                },
                false,
                &totals.records,
                &observed,
            );
            let debt_anchor = anchor(
                a,
                &pair,
                buy,
                Amount {
                    asset: loan_asset.clone(),
                    atoms: totals.loan.clone(),
                },
                true,
                &totals.records,
                &observed,
            );
            positions.push(PositionState {
                resource: a.resource.clone(),
                position: position_key(&pair, buy),
                kind: kind(buy),
                amount: position_anchor.principal.clone(),
                scaled_receipt: None,
                anchor: Some(position_anchor),
            });
            positions.push(PositionState {
                resource: a.resource.clone(),
                position: debt_key(&pair, buy),
                kind: PositionKind::VariableDebt,
                amount: debt_anchor.principal.clone(),
                scaled_receipt: None,
                anchor: Some(debt_anchor),
            });
        }
        Ok(positions)
    }
    async fn prepare_margin_order(
        &self,
        a: &KrakenAccount,
        p: &mut Value,
        c: &TradingControls,
        index: usize,
        positions: &Value,
    ) -> Result<(MarginOrder, Vec<String>)> {
        let requested = p["pair"]
            .as_str()
            .ok_or_else(|| error("INVALID_INPUT", "Margin requires an exact pair."))?;
        let metadata = self
            .json(
                E::KrakenPublicGetAssetPairs,
                None,
                json!({"pair":requested}),
            )
            .await?;
        let rows = result(&metadata)
            .as_object()
            .ok_or_else(|| error("KRAKEN_MARKET_UNKNOWN", "Margin market metadata missing."))?;
        if rows.len() != 1 {
            return Err(error(
                "KRAKEN_MARKET_AMBIGUOUS",
                "Choose one native margin pair.",
            ));
        }
        let (pair, m) = rows.iter().next().unwrap();
        if m["status"] != "online" {
            return Err(error(
                "KRAKEN_MARKET_UNAVAILABLE",
                "Margin market is not online.",
            ));
        }
        let base = self
            .spot_asset(
                a,
                m["base"]
                    .as_str()
                    .ok_or_else(|| error("KRAKEN_MARKET_UNKNOWN", "Margin base missing."))?,
            )
            .await?;
        let quote = self
            .spot_asset(
                a,
                m["quote"]
                    .as_str()
                    .ok_or_else(|| error("KRAKEN_MARKET_UNKNOWN", "Margin quote missing."))?,
            )
            .await?;
        let buy = match p["type"].as_str() {
            Some("buy") => true,
            Some("sell") => false,
            _ => {
                return Err(error(
                    "INVALID_INPUT",
                    "Margin direction must be buy or sell.",
                ));
            }
        };
        let leverage_text = p["leverage"]
            .as_str()
            .ok_or_else(|| error("INVALID_INPUT", "Margin requires explicit leverage."))?;
        let leverage = leverage_text
            .strip_suffix(":1")
            .unwrap_or(leverage_text)
            .parse::<u64>()
            .map_err(|_| error("INVALID_INPUT", "Use supported integer margin leverage."))?;
        if leverage < 2
            || !m[if buy { "leverage_buy" } else { "leverage_sell" }]
                .as_array()
                .is_some_and(|values| values.iter().any(|v| v.as_u64() == Some(leverage)))
        {
            return Err(error(
                "KRAKEN_LEVERAGE_UNAVAILABLE",
                "Selected leverage is not supported for this exact market and direction.",
            ));
        }
        if p["oflags"]
            .as_str()
            .is_some_and(|v| v.split(',').any(|flag| flag == "viqc"))
        {
            return Err(error(
                "KRAKEN_MARGIN_QUOTE_VOLUME_UNSUPPORTED",
                "Kraken quote-volume market buys do not apply to margin.",
            ));
        }
        check_margin_execution_limit(p)?;
        let settling = p["ordertype"] == "settle-position";
        let opposite = position_totals(result(positions), pair, !buy)?;
        let closing = settling || p["reduce_only"] == true || !opposite.volume.is_zero();
        let position_side = if closing { !buy } else { buy };
        let prior = position_totals(result(positions), pair, position_side)?;
        let mut volume = atoms(&p["volume"])?;
        if volume.is_zero() {
            if settling && !prior.volume.is_zero() {
                volume = prior.volume.clone();
                p["volume"] = json!(volume.to_decimal(36));
            } else {
                return Err(error(
                    "INVALID_INPUT",
                    "Margin volume must be positive; zero is only accepted for an exactly frozen full settlement.",
                ));
            }
        }
        if closing && volume > prior.volume {
            return Err(error(
                "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED",
                "An order that closes existing positions must not silently create a reversed residual exposure. Split the close and new opening into ordered steps.",
            ));
        }
        let precision = m["lot_decimals"]
            .as_u64()
            .ok_or_else(|| error("KRAKEN_MARKET_UNKNOWN", "Margin lot precision missing."))?;
        if volume
            .to_decimal(36)
            .split('.')
            .nth(1)
            .is_some_and(|f| f.len() > precision as usize)
        {
            return Err(error(
                "INVALID_INPUT",
                "Margin size exceeds the venue lot precision.",
            ));
        }
        let mut warnings=vec!["Kraken extends the entire loan principal. Collateral secures the position and is not deducted from the loan. Venue margin liquidation and future rollover charges persist after this command; GoFin does not silently renew or close the position.".into()];
        let maximum_loan = if closing {
            maximum_close_loan(&prior, &volume, position_side)?
        } else if !buy {
            volume.clone()
        } else {
            let price_field = match p["ordertype"].as_str() {
                Some("limit") => "price",
                Some("stop-loss-limit" | "take-profit-limit" | "trailing-stop-limit") => "price2",
                Some("market") => {
                    let maximum=c.maximum_spend.as_deref().ok_or_else(||error("KRAKEN_MARGIN_SPEND_CAP_REQUIRED","A margin market buy requires maximumSpend in exact quote units for its disclosed IOC price collar."))?;
                    let maximum = Quantity::from_decimal(maximum, 36)?;
                    let collar = maximum.mul_div_floor(&scale(), &volume)?;
                    if collar.is_zero() {
                        return Err(error("INVALID_INPUT", "Margin price collar is zero."));
                    }
                    p["ordertype"] = json!("limit");
                    p["timeinforce"] = json!("IOC");
                    p["price"] = json!(collar.to_decimal(36));
                    warnings.push("The requested margin market buy is submitted as an immediate-or-cancel limit with the exact approved absolute quote-price collar; it may partially fill or expire unfilled.".into());
                    "price"
                }
                _ => {
                    return Err(error(
                        "KRAKEN_MARGIN_SPEND_CAP_REQUIRED",
                        "A margin buy needs an absolute venue limit; a trigger alone does not bound quote borrowing.",
                    ));
                }
            };
            product(&volume, &absolute_execution_price(&p[price_field])?)?
        };
        if let Some(maximum) = &c.maximum_spend
            && !closing
            && maximum_loan > Quantity::from_decimal(maximum, 36)?
        {
            return Err(error(
                "KRAKEN_SPEND_BOUND_EXCEEDED",
                "The exact margin loan principal exceeds maximumSpend in its borrowed asset.",
            ));
        }
        let mut collateral_required = Quantity::zero();
        if !closing {
            let mut collateral = if buy {
                maximum_loan.clone()
            } else {
                let ticker = self
                    .json(E::KrakenPublicGetTicker, None, json!({"pair":pair}))
                    .await?;
                let ask = result(&ticker)
                    .get(pair)
                    .and_then(|v| v["a"].as_array())
                    .and_then(|a| a.first())
                    .ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_COLLATERAL_UNAVAILABLE",
                            "Margin short collateral quote is missing.",
                        )
                    })?;
                product(&volume, &positive(ask)?)?
            };
            collateral =
                collateral.mul_div_ceil(&Quantity::from_u64(1), &Quantity::from_u64(leverage))?;
            collateral_required = collateral.clone();
            let balance = self
                .json(
                    E::KrakenPrivatePostTradeBalance,
                    Some(a),
                    json!({"asset":quote.reference}),
                )
                .await?;
            if atoms(&result(&balance)["mf"])? < collateral {
                return Err(error(
                    "KRAKEN_INSUFFICIENT_MARGIN",
                    "Exact free margin is below the reviewed collateral requirement.",
                ));
            }
        }
        let id = p["cl_ord_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "fin{}",
                    &hex::encode(Sha256::digest(format!(
                        "fin-kraken-order-v1:{}:{}:{index}",
                        a.physical_identity, c.request_id
                    )))[..15]
                )
            });
        if id.is_empty()
            || !(id.len() <= 18 && id.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
                || (id.len() == 36 || id.len() == 32) && uuid::Uuid::parse_str(&id).is_ok())
        {
            return Err(error(
                "INVALID_INPUT",
                "Use a native UUID or a client order ID of at most 18 characters.",
            ));
        }
        let user_reference = p.get("userref").cloned();
        if user_reference.is_some() && p.get("cl_ord_id").is_some() {
            return Err(error(
                "INVALID_INPUT",
                "userref and cl_ord_id are mutually exclusive.",
            ));
        }
        if user_reference.is_none() {
            p["cl_ord_id"] = json!(id);
        }
        p["pair"] = json!(pair);
        let observed = gofin_domain::now();
        let loan_asset = if position_side { &quote } else { &base };
        let prior_anchor = anchor(
            a,
            pair,
            position_side,
            Amount {
                asset: base.clone(),
                atoms: prior.volume.clone(),
            },
            false,
            &prior.records,
            &observed,
        );
        let prior_debt_anchor = anchor(
            a,
            pair,
            position_side,
            Amount {
                asset: loan_asset.clone(),
                atoms: prior.loan.clone(),
            },
            true,
            &prior.records,
            &observed,
        );
        Ok((
            MarginOrder {
                client_id: id,
                order_ids: vec![],
                conditional_parent: None,
                conditional_parameters: None,
                generated_positions: BTreeMap::new(),
                pair: pair.clone(),
                base,
                quote,
                buy: position_side,
                volume,
                maximum_loan,
                leverage,
                collateral_required,
                user_reference,
                closing,
                settling,
                prior,
                prior_anchor,
                prior_debt_anchor,
            },
            warnings,
        ))
    }
    pub(crate) async fn prepare_margin(
        &self,
        endpoint: E,
        input: &Value,
        a: &KrakenAccount,
        mut parameters: Value,
        c: &TradingControls,
    ) -> Result<PreparedOperation> {
        let mut orders = vec![];
        let mut warnings = vec![];
        let positions = self
            .json(
                E::KrakenPrivatePostOpenPositions,
                Some(a),
                json!({"docalcs":false}),
            )
            .await?;
        if endpoint == E::KrakenPrivatePostAddOrderBatch {
            let pair = parameters["pair"].clone();
            let values = parameters["orders"]
                .as_array_mut()
                .ok_or_else(|| error("INVALID_INPUT", "Margin batch orders missing."))?;
            for (index, p) in values.iter_mut().enumerate() {
                p["pair"] = pair.clone();
                let (o, w) = self
                    .prepare_margin_order(a, p, c, index, &positions)
                    .await?;
                p.as_object_mut().unwrap().remove("pair");
                orders.push(o);
                warnings.extend(w);
            }
        } else {
            let (o, w) = self
                .prepare_margin_order(a, &mut parameters, c, 0, &positions)
                .await?;
            orders.push(o);
            warnings.extend(w);
        }
        let root_count = orders.len();
        if let Some(asset) = check_principal_cap(endpoint, &orders, c.maximum_spend.as_deref())? {
            warnings.push(format!("maximumSpend caps the full borrowed {} principal across this command. It does not cap or represent collateral; fees and realized-loss limits are separate.",asset.reference));
        }
        let conditional_count = (0..root_count)
            .filter(|index| {
                let native = if endpoint == E::KrakenPrivatePostAddOrderBatch {
                    &parameters["orders"][*index]
                } else {
                    &parameters
                };
                native["close"].is_object() || native.get("close[ordertype]").is_some()
            })
            .count();
        if conditional_count == 0 && orders.iter().all(|order| !order.closing || order.settling) {
            let mut unused = vec![];
            if c.maximum_loss.is_some() {
                unused.push("/maximumLoss".into());
            }
            if c.maximum_profit.is_some() {
                unused.push("/maximumProfit".into());
            }
            if !unused.is_empty() {
                return Err(contracts::unsupported_control(endpoint, unused));
            }
        }
        let conditional_loss_share = if conditional_count > 0 {
            Quantity::from_decimal(
                c.maximum_loss.as_deref().ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                        "A conditional margin close requires a whole-command maximumLoss.",
                    )
                })?,
                36,
            )?
            .mul_div_floor(
                &Quantity::from_u64(1),
                &Quantity::from_u64(conditional_count as u64),
            )?
        } else {
            Quantity::zero()
        };
        for index in 0..root_count {
            let native = if endpoint == E::KrakenPrivatePostAddOrderBatch {
                &parameters["orders"][index]
            } else {
                &parameters
            };
            let close = if native["close"].is_object() {
                Some(native["close"].clone())
            } else {
                native.get("close[ordertype]").map(|kind|json!({"ordertype":kind,"price":native["close[price]"],"price2":native["close[price2]"]}))
            };
            if let Some(mut close) = close {
                if orders[index].closing {
                    return Err(error(
                        "KRAKEN_MARGIN_CONDITIONAL_REVERSE_SCOPE",
                        "A conditional child of a closing margin order requires separately approved reversed exposure. Use an opening entry with its bounded close, or ordered fresh approvals.",
                    ));
                }
                close
                    .as_object_mut()
                    .ok_or_else(|| {
                        error("INVALID_INPUT", "Conditional margin parameters invalid.")
                    })?
                    .retain(|_, v| !v.is_null());
                let child_kind = close["ordertype"].as_str().ok_or_else(|| {
                    error("INVALID_INPUT", "Conditional margin order type missing.")
                })?;
                if !matches!(
                    child_kind,
                    "limit" | "stop-loss-limit" | "take-profit-limit" | "trailing-stop-limit"
                ) {
                    return Err(error(
                        "KRAKEN_MARGIN_CONDITIONAL_LIMIT_REQUIRED",
                        "A conditional margin close needs an absolute native limit to bound loss. Select limit or a trigger with its separate execution limit.",
                    ));
                }
                let field = if child_kind == "limit" {
                    "price"
                } else {
                    "price2"
                };
                check_margin_execution_limit(&close)?;
                let limit = absolute_execution_price(&close[field])?;
                let quote_bound = product(&orders[index].volume, &limit)?;
                if orders[index].buy {
                    if orders[index].maximum_loan
                        > quote_bound.checked_add(&conditional_loss_share)?
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_CHILD_LOSS_CAP",
                            "The native child sell limit cannot cover the parent quote extension within its share of maximumLoss.",
                        ));
                    }
                } else {
                    let entry_field = if native["ordertype"] == "limit" {
                        "price"
                    } else {
                        "price2"
                    };
                    let entry_floor = native
                        .get(entry_field)
                        .map(absolute_execution_price)
                        .transpose()?
                        .map(|price| product(&orders[index].volume, &price))
                        .transpose()?
                        .unwrap_or_default();
                    if quote_bound > entry_floor.checked_add(&conditional_loss_share)? {
                        return Err(error(
                            "KRAKEN_MARGIN_CHILD_LOSS_CAP",
                            "The native child buy limit exceeds the bounded entry quote proceeds plus its share of maximumLoss. Select an entry with an absolute quote floor.",
                        ));
                    }
                }
                let mut child = orders[index].clone();
                child.client_id = format!("{}:conditional-close", child.client_id);
                child.conditional_parent = Some(orders[index].client_id.clone());
                child.conditional_parameters = Some(close);
                child.closing = true;
                child.collateral_required = Quantity::zero();
                child.user_reference = None;
                orders.push(child);
                warnings.push("Kraken persists the conditional margin close. GoFin reserves explicit trading fees and bounded realized loss, proves its exact parent/child orders and full loan repayment, and waits for all generated children before completion.".into());
            }
        }
        let mut collateral_by_quote: BTreeMap<AssetId, Quantity> = BTreeMap::new();
        for order in &orders {
            if !order.closing {
                let sum = collateral_by_quote.entry(order.quote.clone()).or_default();
                *sum = sum.checked_add(&order.collateral_required)?;
            }
        }
        for (quote, needed) in collateral_by_quote {
            let balance = self
                .json(
                    E::KrakenPrivatePostTradeBalance,
                    Some(a),
                    json!({"asset":quote.reference}),
                )
                .await?;
            if atoms(&result(&balance)["mf"])? < needed {
                return Err(error(
                    "KRAKEN_INSUFFICIENT_MARGIN",
                    "Free margin cannot secure the combined batch collateral requirement.",
                ));
            }
        }
        let validation_only = parameters["validate"] == true;
        let mut intent = FinancialIntent {
            locks: vec![a.resource.clone()],
            ..Default::default()
        };
        let mut valuation_principal = vec![];
        let mut baselines = vec![];
        let grouped = grouped_orders(&orders)?;
        for order in &grouped {
            let loan_asset = if order.buy {
                order.quote.clone()
            } else {
                order.base.clone()
            };
            valuation_principal.push(ResourceAmount {
                resource: a.resource.clone(),
                amount: Amount {
                    asset: loan_asset.clone(),
                    atoms: order.maximum_loan.clone(),
                },
            });
            if !validation_only {
                let delta = if order.closing {
                    SignedQuantity::negative(&order.volume)
                } else {
                    SignedQuantity::positive(&order.volume)
                };
                intent.expected.push(EffectConstraint::MarginPosition {
                    resource: a.resource.clone(),
                    position: position_key(&order.pair, order.buy),
                    asset: order.base.clone(),
                    position_kind: kind(order.buy),
                    minimum_delta: if order.closing {
                        delta.clone()
                    } else {
                        SignedQuantity::positive(&Quantity::zero())
                    },
                    maximum_delta: if order.closing {
                        SignedQuantity::positive(&Quantity::zero())
                    } else {
                        delta
                    },
                });
                intent.expected.push(EffectConstraint::Debt {
                    resource: a.resource.clone(),
                    debt: debt_key(&order.pair, order.buy),
                    asset: loan_asset,
                    direction: if order.closing {
                        Direction::Decrease
                    } else {
                        Direction::Increase
                    },
                    maximum_change: order.maximum_loan.clone(),
                });
                for baseline in [&order.prior_anchor, &order.prior_debt_anchor] {
                    if !baselines.iter().any(|old: &PositionAnchor| {
                        old.resource == baseline.resource
                            && old.position == baseline.position
                            && old.position_kind == baseline.position_kind
                            && old.principal.asset == baseline.principal.asset
                    }) {
                        baselines.push(baseline.clone());
                    }
                }
                if order.settling {
                    intent.debits.push(ResourceAmount {
                        resource: a.resource.clone(),
                        amount: Amount {
                            asset: if order.buy {
                                order.quote.clone()
                            } else {
                                order.base.clone()
                            },
                            atoms: order.maximum_loan.clone(),
                        },
                    });
                    intent.expected.push(EffectConstraint::Debit {
                        resource: a.resource.clone(),
                        asset: if order.buy {
                            order.quote.clone()
                        } else {
                            order.base.clone()
                        },
                        maximum_debit: order.maximum_loan.clone(),
                    });
                    intent.expected.push(EffectConstraint::Credit {
                        to: a.resource.clone(),
                        asset: if order.buy {
                            order.base.clone()
                        } else {
                            order.quote.clone()
                        },
                        minimum_credit: Quantity::zero(),
                    });
                } else if order.closing {
                    let loss = c.maximum_loss.as_deref().ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                            "Margin closing requires maximumLoss in exact quote units.",
                        )
                    })?;
                    let profit = c.maximum_profit.as_deref().ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_PNL_BOUNDS_REQUIRED",
                            "Margin closing requires maximumProfit in exact quote units.",
                        )
                    })?;
                    intent.expected.push(EffectConstraint::Settlement {
                        resource: a.resource.clone(),
                        asset: order.quote.clone(),
                        category: SettlementCategory::RealizedPnl,
                        minimum_delta: SignedQuantity::negative(&Quantity::from_decimal(loss, 36)?),
                        maximum_delta: SignedQuantity::positive(&Quantity::from_decimal(
                            profit, 36,
                        )?),
                    });
                    intent.debits.push(ResourceAmount {
                        resource: a.resource.clone(),
                        amount: Amount {
                            asset: order.quote.clone(),
                            atoms: Quantity::from_decimal(loss, 36)?,
                        },
                    });
                    intent.maximum_costs.push(CostBound {
                        location: AccountingLocation::Cash {
                            resource: a.resource.clone(),
                        },
                        amount: Amount {
                            asset: order.quote.clone(),
                            atoms: Quantity::from_decimal(loss, 36)?,
                        },
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
            }
        }
        // maximumLoss/maximumProfit are whole-command limits, including batches.
        let mut settlements = BTreeSet::new();
        let mut duplicate_loss = vec![];
        intent.expected.retain(|effect| {
            if let EffectConstraint::Settlement {
                resource, asset, ..
            } = effect
                && !settlements.insert((resource.clone(), asset.clone()))
            {
                duplicate_loss.push((resource.clone(), asset.clone()));
                return false;
            }
            true
        });
        for (resource, asset) in duplicate_loss {
            let loss = Quantity::from_decimal(c.maximum_loss.as_deref().unwrap_or("0"), 36)?;
            let index = intent
                .debits
                .iter()
                .position(|d| {
                    d.resource == resource && d.amount.asset == asset && d.amount.atoms == loss
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_BOUND_INVALID",
                        "Repeated net-loss reservation missing.",
                    )
                })?;
            intent.debits.remove(index);
            let index = intent
                .maximum_costs
                .iter()
                .position(|d| {
                    d.location
                        == AccountingLocation::Cash {
                            resource: resource.clone(),
                        }
                        && d.amount.asset == asset
                        && d.treatment == CostTreatment::IncludedInDebit
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_BOUND_INVALID",
                        "Repeated net-loss cost reservation missing.",
                    )
                })?;
            intent.maximum_costs.remove(index);
        }
        intent.expected = intent
            .expected
            .into_iter()
            .flat_map(|effect| match effect {
                EffectConstraint::Settlement {
                    resource,
                    asset,
                    category,
                    minimum_delta,
                    maximum_delta,
                } => vec![
                    EffectConstraint::Settlement {
                        resource: resource.clone(),
                        asset: asset.clone(),
                        category: category.clone(),
                        minimum_delta,
                        maximum_delta: SignedQuantity::positive(&Quantity::zero()),
                    },
                    EffectConstraint::Settlement {
                        resource,
                        asset,
                        category,
                        minimum_delta: SignedQuantity::positive(&Quantity::zero()),
                        maximum_delta,
                    },
                ],
                other => vec![other],
            })
            .collect();
        if !validation_only {
            if c.maximum_fees.is_empty() {
                return Err(error(
                    "KRAKEN_FEE_BOUNDS_REQUIRED",
                    "Declare whole-command margin fees by exact venue asset ID in maximumFees; standing rollover liability is separately disclosed.",
                ));
            }
            for (code, cap) in &c.maximum_fees {
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: a.resource.clone(),
                    },
                    amount: Amount {
                        asset: self.spot_asset(a, code).await?,
                        atoms: Quantity::from_decimal(cap, 36)?,
                    },
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
        }
        let fee_admission = if validation_only {
            Value::Null
        } else {
            let mut scopes = Vec::with_capacity(orders.len());
            for (index, order) in orders.iter().enumerate() {
                let native = order.conditional_parameters.as_ref().unwrap_or_else(|| {
                    if endpoint == E::KrakenPrivatePostAddOrderBatch {
                        &parameters["orders"][index]
                    } else {
                        &parameters
                    }
                });
                scopes.push(
                    self.margin_fee_scope(
                        crate::margin_fee_admission::MarginFeeScope {
                            pair: order.pair.clone(),
                            base: order.base.clone(),
                            quote: order.quote.clone(),
                            volume: order.volume.clone(),
                            quote_notional: Quantity::zero(),
                            settling: order.settling,
                        },
                        native,
                    )
                    .await?,
                );
            }
            self.verify_margin_fee_admission(a, &scopes, &c.maximum_fees)
                .await?
        };
        let valuation = self
            .financial_valuation(
                if validation_only {
                    &[]
                } else {
                    &valuation_principal
                },
                &intent.maximum_costs,
            )
            .await?;
        let mut prepared = PreparedOperation {
            operation: endpoint.operation().into(),
            input: input.clone(),
            binding: json!({"endpoint":endpoint.method(),"product":self.product,"account":a.name,"physicalIdentity":a.physical_identity,"parameters":parameters,"requestId":c.request_id,"margin":MarginBinding{orders,controls:c.clone(),validation_only}}),
            summary: format!(
                "{} Kraken margin request on {}; full loan principal, explicit fee caps and segregated exposure.",
                if validation_only {
                    "Validate"
                } else {
                    "Execute"
                },
                a.name
            ),
            checks: vec![],
            warnings,
            observed_at: gofin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            action_deadline: None,
            operation_version: 1,
        };
        if !validation_only {
            warnings_from_fee_evidence(&mut prepared, &fee_admission);
        }
        prepared.binding[gofin_domain::BASELINE_ANCHORS_KEY] = serde_json::to_value(baselines)
            .map_err(|_| {
                error(
                    "KRAKEN_MARGIN_EVIDENCE_INVALID",
                    "Margin anchors failed to serialize.",
                )
            })?;
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
}

impl KrakenProfile {
    pub(crate) async fn reconcile_margin(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        b: &Value,
    ) -> Result<Reconciliation> {
        let a = self.account(b["account"].as_str().ok_or_else(|| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Margin account binding is missing.",
            )
        })?)?;
        if b["physicalIdentity"] != a.physical_identity {
            return Err(error(
                "KRAKEN_ACCOUNT_IDENTITY_CHANGED",
                "Original margin custody identity changed.",
            ));
        }
        let mut binding: MarginBinding =
            serde_json::from_value(b["margin"].clone()).map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Exact margin binding is unavailable.",
                )
            })?;
        if binding.validation_only {
            let Some(receipt) = record
                .dispatch_evidence
                .as_ref()
                .and_then(|r| r.get("response"))
            else {
                return Ok(unresolved(
                    Status::Unknown,
                    "Margin validation response was lost; no order is replayed.",
                    json!({}),
                ));
            };
            let r = result(receipt);
            if r.get("txid")
                .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
            {
                return Err(error(
                    "KRAKEN_VALIDATION_PROOF_MISMATCH",
                    "Validation returned a placed order identity.",
                ));
            }
            if !r["descr"].is_object() && !r["orders"].is_array() {
                return Ok(unresolved(
                    Status::Unknown,
                    "Native margin validation description is unavailable.",
                    receipt.clone(),
                ));
            }
            return proof(
                submission,
                Status::Completed,
                vec![],
                json!({"validationOnly":true,"financialOrderPlaced":false,"validation":r}),
            );
        }
        let conditional_pending = self
            .resolve_margin_children(record, submission, a, &mut binding.orders, b)
            .await?;
        let current_response = self
            .json(
                E::KrakenPrivatePostOpenPositions,
                Some(a),
                json!({"docalcs":false}),
            )
            .await?;
        let mut effects = vec![];
        let mut events = vec![];
        let mut evidence = vec![];
        let mut closed_positions = serde_json::Map::new();
        let mut any_fill = false;
        let mut terminal = !conditional_pending;
        let mut fees: BTreeMap<AssetId, Quantity> = BTreeMap::new();
        let mut seen_trades = BTreeSet::new();
        let mut seen_ledgers = BTreeSet::new();
        let grouped = grouped_orders(&binding.orders)?;
        let mut progress: BTreeMap<(String, bool, bool, bool), MarginProgress> = BTreeMap::new();
        let mut stages = vec![];
        for (index, o) in binding.orders.iter().enumerate() {
            if o.conditional_parent.is_some() && o.order_ids.is_empty() {
                continue;
            }
            if o.order_ids.is_empty() {
                stages.push((index, o.clone()));
            } else {
                for id in &o.order_ids {
                    let mut stage = o.clone();
                    stage.order_ids = vec![id.clone()];
                    stages.push((index, stage));
                }
            }
        }
        for (index, o) in &stages {
            let receipt = record
                .dispatch_evidence
                .as_ref()
                .and_then(|r| r.get("response"))
                .map(result);
            let receipt_id = if let Some(id) = o.order_ids.first() {
                Some(id.as_str())
            } else if binding
                .orders
                .iter()
                .filter(|o| o.conditional_parent.is_none())
                .count()
                == 1
            {
                receipt
                    .and_then(|r| r["txid"].as_array())
                    .and_then(|v| v.first())
                    .and_then(Value::as_str)
            } else {
                receipt
                    .and_then(|r| r["orders"].as_array())
                    .and_then(|v| v.get(*index))
                    .and_then(|r| r["txid"].as_str())
            };
            let response = if let Some(id) = receipt_id {
                self.json(
                    E::KrakenPrivatePostQueryOrders,
                    Some(a),
                    json!({"txid":id,"trades":true}),
                )
                .await?
            } else {
                let query = if let Some(reference) = &o.user_reference {
                    json!({"userref":reference,"trades":true})
                } else {
                    json!({"cl_ord_id":o.client_id,"trades":true})
                };
                let open = self
                    .json(E::KrakenPrivatePostOpenOrders, Some(a), query.clone())
                    .await?;
                let closed = self
                    .json(E::KrakenPrivatePostClosedOrders, Some(a), query)
                    .await?;
                let mut rows = serde_json::Map::new();
                for (r, key) in [(&open, "open"), (&closed, "closed")] {
                    if let Some(m) = result(r)[key].as_object() {
                        for (id, row) in m {
                            rows.insert(id.clone(), row.clone());
                        }
                    }
                }
                json!({"result":rows})
            };
            let rows = result(&response).as_object().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_EVIDENCE_INVALID",
                    "Native margin order map missing.",
                )
            })?;
            let submitted = chrono::DateTime::parse_from_rfc3339(
                submission.data["submittedAt"].as_str().unwrap_or(""),
            )
            .map_err(|_| {
                error(
                    "KRAKEN_SUBMISSION_INVALID",
                    "Original margin submission time is unavailable.",
                )
            })?;
            let matching: Vec<_> = rows
                .iter()
                .filter(|(id, row)| {
                    if let Some(expected) = receipt_id {
                        // An immutable acknowledged/proved native identity is safe
                        // even when the venue timestamp has coarser precision.
                        return id.as_str() == expected;
                    }
                    let label = if let Some(reference) = &o.user_reference {
                        row["userref"] == *reference
                    } else {
                        row["cl_ord_id"] == o.client_id
                    };
                    label && crate::trading::opened_at_or_after(row, &submitted)
                })
                .collect();
            if matching.len() != 1 {
                return Ok(unresolved(
                    Status::Unknown,
                    "Original margin order identity is absent or ambiguous; no order is replayed.",
                    response,
                ));
            }
            let (order_id, row) = matching[0];
            let native_side = if o.closing { !o.buy } else { o.buy };
            if row["descr"]["type"] != if native_side { "buy" } else { "sell" }
                || row["descr"]["pair"] != o.pair && row["descr"]["pair"] != b["parameters"]["pair"]
            {
                return Err(error(
                    "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                    "Native margin order changed market or direction.",
                ));
            }
            terminal &= matches!(
                row["status"].as_str(),
                Some("closed" | "canceled" | "expired")
            );
            let executed = atoms(&row["vol_exec"])?;
            if executed > o.volume {
                return Err(error(
                    "KRAKEN_MARGIN_BOUND_EXCEEDED",
                    "Filled margin size exceeds the frozen exact quantity.",
                ));
            }
            if executed.is_zero() {
                if !atoms(&row["cost"])?.is_zero() || !atoms(&row["fee"])?.is_zero() {
                    return Err(error(
                        "KRAKEN_MARGIN_EVIDENCE_INVALID",
                        "No-fill margin order has cost or fees.",
                    ));
                }
                evidence.push(json!({"orderId":order_id,"order":row}));
                continue;
            }
            any_fill = true;
            let ids = row["trades"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_PROOF_PENDING",
                    "Filled margin order omitted native trade identities.",
                )
            })?;
            let mut trades = serde_json::Map::new();
            let mut trade_ids = vec![];
            for id in ids {
                let id = id.as_str().ok_or_else(|| {
                    error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Margin trade ID invalid.")
                })?;
                if !seen_trades.insert(id.to_owned()) {
                    return Err(error(
                        "KRAKEN_MARGIN_EVIDENCE_INVALID",
                        "Margin proof repeats an execution identity.",
                    ));
                }
                trade_ids.push(id);
            }
            for chunk in trade_ids.chunks(20) {
                let response = self
                    .json(
                        E::KrakenPrivatePostQueryTrades,
                        Some(a),
                        json!({"txid":chunk.join(","),"ledgers":true,"trades":true}),
                    )
                    .await?;
                for id in chunk {
                    trades.insert(
                        (*id).to_owned(),
                        result(&response)
                            .get(*id)
                            .ok_or_else(|| {
                                error(
                                    "KRAKEN_MARGIN_PROOF_PENDING",
                                    "Exact margin execution not yet visible.",
                                )
                            })?
                            .clone(),
                    );
                }
            }
            let mut volume = Quantity::zero();
            let mut loan = Quantity::zero();
            let mut cost = Quantity::zero();
            let mut quote_fee = Quantity::zero();
            let mut cash: BTreeMap<AssetId, num_bigint::BigInt> = BTreeMap::new();
            let mut gross_loss = Quantity::zero();
            let mut gross_profit = Quantity::zero();
            let mut reductions: BTreeMap<String, Quantity> = BTreeMap::new();
            let mut opening_ids = BTreeSet::new();
            for (id, t) in &trades {
                if t["ordertxid"] != *order_id
                    || t["pair"] != o.pair
                    || t["type"] != if native_side { "buy" } else { "sell" }
                {
                    return Err(error(
                        "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                        "Margin execution is not correlated to its original order.",
                    ));
                }
                let v = positive(&t["vol"])?;
                let c = atoms(&t["cost"])?;
                volume = volume.checked_add(&v)?;
                cost = cost.checked_add(&c)?;
                quote_fee = quote_fee.checked_add(&atoms(&t["fee"])?)?;
                if o.closing {
                    let parent = t["postxid"].as_str().ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "Closing execution omitted its original position identity.",
                        )
                    })?;
                    let original=o.prior.records.get(parent).or_else(||o.generated_positions.get(parent)).ok_or_else(||error("KRAKEN_MARGIN_IDENTITY_MISMATCH","Closing execution belongs to a position outside the approved baseline."))?;
                    let original_volume = positive(&original["vol"])?;
                    let previous = reductions.entry(parent.into()).or_default();
                    *previous = previous.checked_add(&v)?;
                    if *previous > original_volume.checked_sub(&atoms(&original["vol_closed"])?)? {
                        return Err(error(
                            "KRAKEN_MARGIN_BOUND_EXCEEDED",
                            "Closing executions exceed the original remaining position.",
                        ));
                    }
                    loan = loan.checked_add(&if o.buy {
                        positive(&original["cost"])?.mul_div_ceil(&v, &original_volume)?
                    } else {
                        v.clone()
                    })?;
                } else {
                    if atoms(&t["margin"])?.is_zero() {
                        return Err(error(
                            "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                            "Opening execution lacks native leveraged margin evidence.",
                        ));
                    }
                    opening_ids.insert(id.clone());
                    loan = loan.checked_add(&if o.buy { c } else { v })?;
                }
                let ledger_ids = t["ledgers"].as_array().ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_PROOF_PENDING",
                        "Margin execution omitted its attributed native ledger identities.",
                    )
                })?;
                let mut ledgers = serde_json::Map::new();
                let mut keys = vec![];
                for key in ledger_ids {
                    let key = key.as_str().ok_or_else(|| {
                        error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Ledger identity invalid.")
                    })?;
                    if !seen_ledgers.insert(key.to_owned()) {
                        return Err(error(
                            "KRAKEN_MARGIN_EVIDENCE_INVALID",
                            "Margin executions share an ambiguous custody ledger identity.",
                        ));
                    }
                    keys.push(key);
                }
                if keys.is_empty() {
                    return Err(error(
                        "KRAKEN_MARGIN_PROOF_PENDING",
                        "Actual fee currency requires an attributed margin ledger.",
                    ));
                }
                for chunk in keys.chunks(20) {
                    let response = self
                        .json(
                            E::KrakenPrivatePostQueryLedgers,
                            Some(a),
                            json!({"id":chunk.join(",")}),
                        )
                        .await?;
                    for key in chunk {
                        ledgers.insert(
                            (*key).into(),
                            result(&response)
                                .get(*key)
                                .ok_or_else(|| {
                                    error(
                                        "KRAKEN_MARGIN_PROOF_PENDING",
                                        "Exact margin ledger not yet visible.",
                                    )
                                })?
                                .clone(),
                        );
                    }
                }
                for ledger in ledgers.values() {
                    if ledger["refid"] != *id
                        || !matches!(
                            ledger["type"].as_str(),
                            Some("trade" | "margin" | "settled")
                        )
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                            "Cash ledger is not an exact execution-correlated margin/trade/settlement entry.",
                        ));
                    }
                    let asset = self
                        .spot_asset(
                            a,
                            ledger["asset"].as_str().ok_or_else(|| {
                                error("KRAKEN_MARGIN_EVIDENCE_INVALID", "Ledger asset missing.")
                            })?,
                        )
                        .await?;
                    let amount = signed(&ledger["amount"])?;
                    if !o.closing && !amount.magnitude().is_zero() {
                        return Err(error(
                            "KRAKEN_MARGIN_CASH_MISMATCH",
                            "Opening margin changed cash principal; a leveraged extension cannot be booked as a cash conversion.",
                        ));
                    }
                    if o.closing && !o.settling && asset == o.quote {
                        if amount.is_negative() {
                            gross_loss = gross_loss.checked_add(&amount.magnitude())?;
                        } else {
                            gross_profit = gross_profit.checked_add(&amount.magnitude())?;
                        }
                    }
                    *cash.entry(asset.clone()).or_default() += amount.integer();
                    let fee = atoms(&ledger["fee"])?;
                    let sum = fees.entry(asset).or_default();
                    *sum = sum.checked_add(&fee)?;
                }
                let mut immutable_trade = normalized(t)?;
                if let Some(row) = immutable_trade.as_object_mut() {
                    for key in [
                        "posstatus",
                        "cvol",
                        "ccost",
                        "cfee",
                        "cmargin",
                        "net",
                        "trades",
                    ] {
                        row.remove(key);
                    }
                }
                let event_id = format!("kraken:margin-trade:{id}");
                let mut event = json!({"eventId":event_id,"trade":immutable_trade,"ledgers":normalized(&Value::Object(ledgers))?});
                if let Some(old) = record
                    .effects
                    .as_ref()
                    .and_then(|proof| proof.evidence["proofEvents"].as_array())
                    .and_then(|events| events.iter().find(|event| event["eventId"] == event_id))
                {
                    let mut core = old["trade"].clone();
                    if let Some(row) = core.as_object_mut() {
                        for field in [
                            "posstatus",
                            "cvol",
                            "ccost",
                            "cfee",
                            "cmargin",
                            "net",
                            "trades",
                        ] {
                            row.remove(field);
                        }
                    }
                    if core == event["trade"] && old["ledgers"] == event["ledgers"] {
                        event = old.clone();
                    }
                }
                events.push(event);
            }
            if o.closing {
                loan = Quantity::zero();
                for (id, reduction) in &reductions {
                    let original = o
                        .prior
                        .records
                        .get(id)
                        .or_else(|| o.generated_positions.get(id))
                        .ok_or_else(|| {
                            error(
                                "KRAKEN_MARGIN_IDENTITY_MISMATCH",
                                "Closing loan original position missing.",
                            )
                        })?;
                    let original_volume = positive(&original["vol"])?;
                    let old_remaining =
                        original_volume.checked_sub(&atoms(&original["vol_closed"])?)?;
                    let new_remaining = old_remaining.checked_sub(reduction)?;
                    let decrease = if o.buy {
                        let c = positive(&original["cost"])?;
                        c.mul_div_ceil(&old_remaining, &original_volume)?
                            .checked_sub(&c.mul_div_ceil(&new_remaining, &original_volume)?)?
                    } else {
                        reduction.clone()
                    };
                    loan = loan.checked_add(&decrease)?;
                }
            }
            if volume != executed
                || cost != atoms(&row["cost"])?
                || quote_fee != atoms(&row["fee"])?
                || loan > o.maximum_loan
            {
                return Err(error(
                    "KRAKEN_MARGIN_PROOF_MISMATCH",
                    "Native order totals and attributed executions or loan cap disagree.",
                ));
            }
            let entry = progress
                .entry((o.pair.clone(), o.buy, o.closing, o.settling))
                .or_default();
            entry.volume = entry.volume.checked_add(&volume)?;
            entry.loan = entry.loan.checked_add(&loan)?;
            for (id, reduction) in &reductions {
                let sum = entry.reductions.entry(id.clone()).or_default();
                *sum = sum.checked_add(reduction)?;
            }
            entry.opening_ids.extend(opening_ids.iter().cloned());
            entry
                .trades
                .extend(trades.iter().map(|(id, t)| (id.clone(), t.clone())));
            let loan_asset = if o.buy { &o.quote } else { &o.base };
            if o.settling {
                let credit_asset = if o.buy { &o.base } else { &o.quote };
                let debit = cash.remove(loan_asset).unwrap_or_default();
                let credit = cash.remove(credit_asset).unwrap_or_default();
                if debit != -num_bigint::BigInt::from(loan.integer())
                    || credit <= num_bigint::BigInt::from(0)
                    || cash.values().any(|v| v != &num_bigint::BigInt::from(0))
                {
                    return Err(error(
                        "KRAKEN_MARGIN_SETTLEMENT_PROOF_PENDING",
                        "Settlement requires exact loan repayment and released acquired-asset ledger credit.",
                    ));
                }
                effects.push(FinancialEffect::CashDebit {
                    resource: a.resource.clone(),
                    amount: Amount {
                        asset: loan_asset.clone(),
                        atoms: loan,
                    },
                });
                effects.push(FinancialEffect::UsableCredit {
                    to: a.resource.clone(),
                    amount: Amount {
                        asset: credit_asset.clone(),
                        atoms: Quantity::new(credit.to_string())?,
                    },
                });
            } else if o.closing {
                let pnl = cash.remove(&o.quote).unwrap_or_default();
                if cash.values().any(|v| v != &num_bigint::BigInt::from(0)) {
                    return Err(error(
                        "KRAKEN_MARGIN_CASH_MISMATCH",
                        "Margin close has unexplained non-quote cash principal.",
                    ));
                }
                if SignedQuantity::new(pnl.to_string())?.integer()
                    != SignedQuantity::positive(&gross_profit).integer()
                        - SignedQuantity::positive(&gross_loss).integer()
                {
                    return Err(error(
                        "KRAKEN_MARGIN_CASH_MISMATCH",
                        "Gross native profit and loss do not match correlated quote cash.",
                    ));
                }
                effects.push(FinancialEffect::Settlement {
                    resource: a.resource.clone(),
                    asset: o.quote.clone(),
                    category: SettlementCategory::RealizedPnl,
                    delta: SignedQuantity::negative(&gross_loss),
                });
                effects.push(FinancialEffect::Settlement {
                    resource: a.resource.clone(),
                    asset: o.quote.clone(),
                    category: SettlementCategory::RealizedPnl,
                    delta: SignedQuantity::positive(&gross_profit),
                });
            }
            evidence.push(json!({"orderId":order_id,"order":normalized(row)?,"positionIds":opening_ids,"closingPositionReductions":reductions}));
        }
        let observed = gofin_domain::now();
        let mut assigned_reductions: BTreeMap<(String, bool, String), Quantity> = BTreeMap::new();
        for o in &grouped {
            let combined = combined_exposure_progress(&binding.orders, &progress, &o.pair, o.buy)?;
            let entry = progress
                .get(&(o.pair.clone(), o.buy, o.closing, o.settling))
                .cloned()
                .unwrap_or_default();
            let volume = entry.volume;
            let mut loan = entry.loan;
            let reductions = entry.reductions;
            let trades = entry.trades;
            if volume.is_zero() && loan.is_zero() && trades.is_empty() {
                continue;
            }
            if o.closing {
                loan = Quantity::zero();
                for (id, reduction) in &reductions {
                    let original = o
                        .prior
                        .records
                        .get(id)
                        .or_else(|| o.generated_positions.get(id))
                        .ok_or_else(|| {
                            error(
                                "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                                "A generated closing loan lacks its exact parent position.",
                            )
                        })?;
                    let old_volume = positive(&original["vol"])?;
                    let previous = assigned_reductions
                        .entry((o.pair.clone(), o.buy, id.clone()))
                        .or_default();
                    let old_remaining = old_volume
                        .checked_sub(&atoms(&original["vol_closed"])?)?
                        .checked_sub(previous)?;
                    let remaining = old_remaining.checked_sub(reduction)?;
                    *previous = previous.checked_add(reduction)?;
                    let decrease = if o.buy {
                        let cost = positive(&original["cost"])?;
                        cost.mul_div_ceil(&old_remaining, &old_volume)?
                            .checked_sub(&cost.mul_div_ceil(&remaining, &old_volume)?)?
                    } else {
                        reduction.clone()
                    };
                    loan = loan.checked_add(&decrease)?;
                }
            }
            if volume > o.volume || loan > o.maximum_loan {
                return Err(error(
                    "KRAKEN_MARGIN_BOUND_EXCEEDED",
                    "The complete original and successor lineage exceeds approved cumulative margin quantity or loan principal.",
                ));
            }
            let current = position_totals(result(&current_response), &o.pair, o.buy)?;
            let (expected_volume, expected_loan, combined) = combined;
            let reductions = combined.reductions;
            let opening_ids = combined.opening_ids;
            let trades = combined.trades;
            // Baseline identities remain immutable. A concurrent unrelated position change
            // is not evidence for this operation, even if aggregate balances happen to match.
            for (id, old) in &o.prior.records {
                let reduction = reductions.get(id).cloned().unwrap_or_default();
                let remaining = positive(&old["vol"])?
                    .checked_sub(&atoms(&old["vol_closed"])?)?
                    .checked_sub(&reduction)?;
                if remaining.is_zero() {
                    if current.records.contains_key(id) {
                        return Err(error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "A fully closed original margin position is still active.",
                        ));
                    }
                    if !reduction.is_zero() {
                        let response = self
                            .json(
                                E::KrakenPrivatePostQueryTrades,
                                Some(a),
                                json!({"txid":id,"trades":true,"ledgers":true}),
                            )
                            .await?;
                        let closed = &result(&response)[id];
                        closed_positions.insert(id.clone(), normalized(closed)?);
                        let native_closed = atoms(&closed["cvol"])?;
                        if closed["posstatus"] != "closed"
                            || native_closed != positive(&old["vol"])?
                            || !closed["trades"].as_array().is_some_and(|ids| {
                                trades
                                    .keys()
                                    .filter(|trade| trades[*trade]["postxid"] == *id)
                                    .all(|trade| ids.contains(&json!(trade)))
                            })
                        {
                            return Err(error(
                                "KRAKEN_MARGIN_PROOF_PENDING",
                                "Closed position history does not correlate the exact closing executions.",
                            ));
                        }
                    }
                } else {
                    let now = current.records.get(id).ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "Original remaining margin position missing.",
                        )
                    })?;
                    if now["vol"] != old["vol"]
                        || now["cost"] != old["cost"]
                        || atoms(&now["vol_closed"])?
                            != atoms(&old["vol_closed"])?.checked_add(&reduction)?
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_PROOF_MISMATCH",
                            "Original margin position changed outside the execution-correlated closure.",
                        ));
                    }
                }
            }
            for id in &opening_ids {
                let t = &trades[id];
                let reduction = reductions.get(id).cloned().unwrap_or_default();
                let remaining = atoms(&t["vol"])?.checked_sub(&reduction)?;

                if remaining.is_zero() {
                    if current.records.contains_key(id) {
                        return Err(error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "Closed generated parent position remains active.",
                        ));
                    }
                    let response = self
                        .json(
                            E::KrakenPrivatePostQueryTrades,
                            Some(a),
                            json!({"txid":id,"trades":true,"ledgers":true}),
                        )
                        .await?;
                    let row = &result(&response)[id];
                    closed_positions.insert(id.clone(), normalized(row)?);
                    if row["posstatus"] != "closed"
                        || atoms(&row["cvol"])? != atoms(&t["vol"])?
                        || !row["trades"].as_array().is_some_and(|ids| {
                            trades
                                .keys()
                                .filter(|child| *child != id && trades[*child]["postxid"] == *id)
                                .all(|child| ids.contains(&json!(child)))
                        })
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "Generated parent closure lacks exact native closed-position and child trade evidence.",
                        ));
                    }
                } else {
                    let row = current.records.get(id).ok_or_else(|| {
                        error(
                            "KRAKEN_MARGIN_PROOF_PENDING",
                            "Opening execution lacks its remaining native position.",
                        )
                    })?;
                    if atoms(&row["vol"])? != atoms(&t["vol"])?
                        || atoms(&row["cost"])? != atoms(&t["cost"])?
                        || atoms(&row["vol_closed"])? != reduction
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_PROOF_MISMATCH",
                            "Generated native position differs from its proved opening and closing executions.",
                        ));
                    }
                }
            }
            if current
                .records
                .keys()
                .any(|id| !o.prior.records.contains_key(id) && !opening_ids.contains(id))
            {
                return Err(error(
                    "KRAKEN_MARGIN_PROOF_MISMATCH",
                    "Unrelated native exposure prevents exact current margin anchoring.",
                ));
            }
            if current.volume != expected_volume || current.loan != expected_loan {
                return Err(error(
                    "KRAKEN_MARGIN_PROOF_MISMATCH",
                    "Current native position and full-loan principal do not match attributed executions.",
                ));
            }
            let loan_asset = if o.buy { &o.quote } else { &o.base };
            let delta = if o.closing {
                SignedQuantity::negative(&volume)
            } else {
                SignedQuantity::positive(&volume)
            };
            let loan_delta = if o.closing {
                SignedQuantity::negative(&loan)
            } else {
                SignedQuantity::positive(&loan)
            };
            effects.push(FinancialEffect::PositionChange {
                resource: a.resource.clone(),
                position: position_key(&o.pair, o.buy),
                asset: o.base.clone(),
                delta,
                scaled_receipt: None,
                anchor: Some(anchor(
                    a,
                    &o.pair,
                    o.buy,
                    Amount {
                        asset: o.base.clone(),
                        atoms: current.volume.clone(),
                    },
                    false,
                    &current.records,
                    &observed,
                )),
            });
            effects.push(FinancialEffect::DebtChange {
                resource: a.resource.clone(),
                debt: debt_key(&o.pair, o.buy),
                asset: loan_asset.clone(),
                delta: loan_delta,
                anchor: Some(anchor(
                    a,
                    &o.pair,
                    o.buy,
                    Amount {
                        asset: loan_asset.clone(),
                        atoms: current.loan.clone(),
                    },
                    true,
                    &current.records,
                    &observed,
                )),
            });
            evidence.push(json!({"groupPair":o.pair,"groupSide":o.buy,"nativePositions":current.records,"closingPositionReductions":reductions,"openingPositionIds":opening_ids}));
        }
        for (asset, fee) in fees {
            if !fee.is_zero() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: a.resource.clone(),
                    },
                    amount: Amount { asset, atoms: fee },
                    category: "kraken_margin_execution_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
        }
        if !terminal && !any_fill {
            return Ok(unresolved(
                Status::Pending,
                "Original margin orders remain open without attributed executions; no financial effects are claimed and reservations remain held.",
                json!({"orders":evidence,"closedPositions":closed_positions,"proofEvents":events}),
            ));
        }
        if terminal && !any_fill {
            effects.clear();
        }
        let effects = coalesce_cash_effects(effects)?;
        let cost_bounds = b
            .get("_finAccountingCostBounds")
            .map(|v| {
                serde_json::from_value::<Vec<CostBound>>(v.clone()).map_err(|_| {
                    error(
                        "KRAKEN_SUBMISSION_INVALID",
                        "Protected margin cost scope is unavailable.",
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
        if b["_finBatchCostScope"] != true
            && crate::unresolved_consumption::costs_exceed_bounds(&effects, &cost_bounds)?
        {
            return crate::unresolved_consumption::cost_overrun(
                submission,
                &effects,
                json!({"code":"KRAKEN_MARGIN_COST_BOUND_EXCEEDED","orders":evidence,"closedPositions":closed_positions,"proofEvents":events}),
            );
        }
        proof(
            submission,
            if !terminal {
                Status::Pending
            } else if any_fill {
                Status::Completed
            } else {
                Status::Failed
            },
            effects,
            json!({"orders":evidence,"closedPositions":closed_positions,"proofEvents":events,"coverage":"Exact margin executions, attributed custody ledgers, native position identities and full loan principal; no balance-difference settlement."}),
        )
    }
}

impl KrakenProfile {
    pub(crate) async fn prepare_margin_amendment(
        &self,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: Option<&gofin_domain::amendments::AmendmentScope>,
    ) -> Result<PreparedOperation> {
        let previous=previous.ok_or_else(||error("KRAKEN_AMENDMENT_TARGET_INVALID","Margin amendments require the protected original operation and cumulative authority scope."))?;
        contracts::validate_fin_controls(
            E::KrakenPrivatePostAmendOrder,
            input,
            Some(&previous.binding),
        )?;
        let operation_id = controls
            .operation_id
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                error(
                    "INVALID_INPUT",
                    "Margin amendment needs the original operationId.",
                )
            })?;
        let original = controls.original_submission.as_ref().ok_or_else(|| {
            error(
                "INVALID_INPUT",
                "Margin amendment needs exact originalSubmission.",
            )
        })?;
        let b = &previous.binding;
        if original.kind != "kraken-rest-v1"
            || b["account"] != account.name
            || b["physicalIdentity"] != account.physical_identity
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Original margin account and physical custody identity must remain exact.",
            ));
        }
        let mut margin: MarginBinding =
            serde_json::from_value(b["margin"].clone()).map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Original margin scope unavailable.",
                )
            })?;
        check_conditional_margin_revision_target(&margin, &parameters)?;
        if margin.orders.len() > 1 {
            return Box::pin(self.prepare_margin_batch_revision(
                E::KrakenPrivatePostAmendOrder,
                input,
                account,
                parameters,
                controls,
                previous,
            ))
            .await;
        }
        if margin.validation_only || margin.orders.len() != 1 {
            return Err(error(
                "KRAKEN_MARGIN_AMENDMENT_SCOPE_PENDING",
                "Margin amendment currently requires one exact original order; multi-order authority revisions need independent target scopes.",
            ));
        }
        let old = margin.orders[0].clone();
        let open = self
            .json(
                E::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let rows = result(&open)["open"].as_object().ok_or_else(|| {
            error(
                "KRAKEN_ORDER_PROOF_PENDING",
                "Margin open-order evidence unavailable.",
            )
        })?;
        let matches = rows
            .iter()
            .filter(|(id, row)| {
                if !old.order_ids.is_empty() {
                    old.order_ids.contains(id)
                } else if let Some(reference) = &old.user_reference {
                    row["userref"] == *reference
                } else {
                    row["cl_ord_id"] == old.client_id
                }
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(error(
                "KRAKEN_ORDER_NOT_OPEN",
                "Exactly one original margin order remainder must be open.",
            ));
        }
        let (id, venue) = matches[0];
        let native_buy = if old.closing { !old.buy } else { old.buy };
        if venue["descr"]["pair"] != old.pair
            || venue["descr"]["type"] != if native_buy { "buy" } else { "sell" }
            || parameters["txid"].as_str().is_some_and(|v| v != id)
            || parameters["cl_ord_id"]
                .as_str()
                .is_some_and(|v| v != old.client_id)
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Amendment must retain original native margin market, side and order identity.",
            ));
        }
        let mut wire = parameters;
        wire["txid"] = json!(id);
        wire.as_object_mut().unwrap().remove("cl_ord_id");
        let filled = atoms(&venue["vol_exec"])?;
        let historical = old
            .order_ids
            .iter()
            .filter(|previous| *previous != id)
            .cloned()
            .collect::<Vec<_>>();
        let mut prior_volume = Quantity::zero();
        let mut prior_cost = Quantity::zero();
        if !historical.is_empty() {
            let response = self
                .json(
                    E::KrakenPrivatePostQueryOrders,
                    Some(account),
                    json!({"txid":historical.join(","),"trades":true}),
                )
                .await?;
            for predecessor in historical {
                let row = &result(&response)[predecessor];
                if !matches!(
                    row["status"].as_str(),
                    Some("closed" | "canceled" | "expired")
                ) {
                    return Err(error(
                        "KRAKEN_AMENDMENT_LINEAGE_PENDING",
                        "Margin predecessor execution totals require terminal native evidence.",
                    ));
                }
                prior_volume = prior_volume.checked_add(&atoms(&row["vol_exec"])?)?;
                prior_cost = prior_cost.checked_add(&atoms(&row["cost"])?)?;
            }
        }
        let quantity = wire
            .get("order_qty")
            .map(atoms)
            .transpose()?
            .unwrap_or(old.volume.checked_sub(&prior_volume)?);
        let clamp_to_filled = quantity < filled;
        let requested_quantity = quantity.clone();
        let quantity = std::cmp::max(quantity, filled.clone());
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
        replacement["volume"] = json!(quantity.to_decimal(36));
        check_margin_execution_limit(&replacement)?;
        if wire["post_only"] == true && wire.get("limit_price").is_none() {
            return Err(error(
                "INVALID_INPUT",
                "post_only requires an exact limit_price amendment.",
            ));
        }
        let total_volume = prior_volume.checked_add(&quantity)?;
        let mut revised = old.clone();
        revised.volume = total_volume.clone();
        if revised.order_ids.is_empty() {
            revised.order_ids.push(id.clone());
        }
        revised.maximum_loan = if revised.closing {
            maximum_close_loan(&old.prior, &total_volume, old.buy)?
        } else if !old.buy {
            total_volume.clone()
        } else {
            let field = if matches!(replacement["ordertype"].as_str(), Some("limit" | "iceberg")) {
                "price"
            } else {
                "price2"
            };
            prior_cost
                .checked_add(&atoms(&venue["cost"])?)?
                .checked_add(&product(
                    &quantity.checked_sub(&filled)?,
                    &positive(&replacement[field])?,
                )?)?
        };
        if controls
            .maximum_spend
            .as_deref()
            .map(|v| Quantity::from_decimal(v, 36))
            .transpose()?
            .is_some_and(|cap| !old.closing && revised.maximum_loan > cap)
        {
            return Err(error(
                "KRAKEN_SPEND_BOUND_EXCEEDED",
                "The cumulative margin extension including predecessor fills exceeds maximumSpend.",
            ));
        }
        // Fresh market metadata validates the revised native size/absolute price;
        // already filled exposure is not counted again as new required collateral.
        let remainder = quantity.checked_sub(&filled)?;
        if !remainder.is_zero() {
            let mut admission = replacement.clone();
            admission["volume"] = json!(remainder.to_decimal(36));
            let positions = self
                .json(
                    E::KrakenPrivatePostOpenPositions,
                    Some(account),
                    json!({"docalcs":false}),
                )
                .await?;
            let mut admission_controls = controls.clone();
            admission_controls.maximum_spend = None;
            let (checked, _) = self
                .prepare_margin_order(account, &mut admission, &admission_controls, 0, &positions)
                .await?;
            if checked.buy != old.buy
                || checked.closing != old.closing
                || checked.base != old.base
                || checked.quote != old.quote
                || checked.leverage != old.leverage
            {
                return Err(error(
                    "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                    "Margin amendment cannot change assets, leverage or opening/closing semantics.",
                ));
            }
            revised.collateral_required = checked.collateral_required;
        }
        let mut full = previous.intent.clone();
        let loan_asset = if old.buy { &old.quote } else { &old.base };
        for expected in &mut full.expected {
            match expected {
                EffectConstraint::MarginPosition {
                    position,
                    minimum_delta,
                    maximum_delta,
                    ..
                } if position == &position_key(&old.pair, old.buy) => {
                    if old.closing {
                        *minimum_delta = SignedQuantity::negative(&revised.volume);
                    } else {
                        *maximum_delta = SignedQuantity::positive(&revised.volume);
                    }
                }
                EffectConstraint::Debt {
                    debt,
                    maximum_change,
                    ..
                } if debt == &debt_key(&old.pair, old.buy) => {
                    *maximum_change = revised.maximum_loan.clone()
                }
                EffectConstraint::Debit {
                    asset,
                    maximum_debit,
                    ..
                } if old.settling && asset == loan_asset => {
                    *maximum_debit = revised.maximum_loan.clone()
                }
                EffectConstraint::Settlement {
                    minimum_delta,
                    maximum_delta,
                    ..
                } => {
                    if minimum_delta.is_negative()
                        && let Some(loss) = &controls.maximum_loss
                    {
                        *minimum_delta =
                            SignedQuantity::negative(&Quantity::from_decimal(loss, 36)?);
                    }
                    if !maximum_delta.is_zero()
                        && let Some(profit) = &controls.maximum_profit
                    {
                        *maximum_delta =
                            SignedQuantity::positive(&Quantity::from_decimal(profit, 36)?);
                    }
                }
                _ => {}
            }
        }
        for debit in &mut full.debits {
            if old.settling && &debit.amount.asset == loan_asset {
                debit.amount.atoms = revised.maximum_loan.clone();
            } else if old.closing
                && !old.settling
                && debit.amount.asset == old.quote
                && let Some(loss) = &controls.maximum_loss
            {
                debit.amount.atoms = Quantity::from_decimal(loss, 36)?;
            }
        }
        if !controls.maximum_fees.is_empty()
            && (controls.maximum_fees.len()
                != full
                    .maximum_costs
                    .iter()
                    .filter(|c| c.treatment == CostTreatment::AdditionalDebit)
                    .count()
                || full
                    .maximum_costs
                    .iter()
                    .filter(|c| c.treatment == CostTreatment::AdditionalDebit)
                    .any(|c| {
                        !controls
                            .maximum_fees
                            .contains_key(&c.amount.asset.reference)
                    }))
        {
            return Err(error(
                "KRAKEN_FEE_BOUNDS_REQUIRED",
                "Margin amendment fee caps must preserve exactly the original fee asset identities.",
            ));
        }
        for cost in &mut full.maximum_costs {
            if cost.treatment == CostTreatment::AdditionalDebit {
                if let Some(cap) = controls.maximum_fees.get(&cost.amount.asset.reference) {
                    cost.amount.atoms = Quantity::from_decimal(cap, 36)?;
                }
            } else if cost.treatment == CostTreatment::IncludedInDebit
                && old.closing
                && !old.settling
                && let Some(loss) = &controls.maximum_loss
            {
                cost.amount.atoms = Quantity::from_decimal(loss, 36)?;
            }
        }
        let mut own = FinancialIntent {
            locks: full.locks.clone(),
            ..Default::default()
        };
        for new in &full.debits {
            let old = previous
                .intent
                .debits
                .iter()
                .find(|old| old.resource == new.resource && old.amount.asset == new.amount.asset)
                .ok_or_else(|| {
                    error(
                        "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                        "Margin source reservation identity changed.",
                    )
                })?;
            if new.amount.atoms > old.amount.atoms {
                let mut delta = new.clone();
                delta.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                own.debits.push(delta);
            }
        }
        for new in &full.maximum_costs {
            let old = previous
                .intent
                .maximum_costs
                .iter()
                .find(|old| {
                    old.location == new.location
                        && old.amount.asset == new.amount.asset
                        && old.treatment == new.treatment
                })
                .ok_or_else(|| {
                    error(
                        "KRAKEN_AMENDMENT_ECONOMIC_SHAPE_CHANGED",
                        "Margin fee or PnL reservation identity changed.",
                    )
                })?;
            if new.amount.atoms > old.amount.atoms {
                let mut delta = new.clone();
                delta.amount.atoms = new.amount.atoms.checked_sub(&old.amount.atoms)?;
                own.maximum_costs.push(delta);
            }
        }
        let full_value = self
            .financial_valuation(
                &[ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset: loan_asset.clone(),
                        atoms: revised.maximum_loan.clone(),
                    },
                }],
                &full.maximum_costs,
            )
            .await?;
        let exposure_increase = if revised.maximum_loan > old.maximum_loan {
            revised.maximum_loan.checked_sub(&old.maximum_loan)?
        } else {
            Quantity::zero()
        };
        let value = self
            .financial_valuation(
                &[ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset: loan_asset.clone(),
                        atoms: exposure_increase,
                    },
                }],
                &own.maximum_costs,
            )
            .await?;
        let fee_caps = full
            .maximum_costs
            .iter()
            .filter(|cost| cost.treatment == CostTreatment::AdditionalDebit)
            .map(|cost| {
                (
                    cost.amount.asset.reference.clone(),
                    cost.amount.atoms.to_decimal(36),
                )
            })
            .collect();
        let fee_scope = self
            .margin_fee_scope(
                crate::margin_fee_admission::MarginFeeScope {
                    pair: revised.pair.clone(),
                    base: revised.base.clone(),
                    quote: revised.quote.clone(),
                    volume: revised.volume.clone(),
                    quote_notional: Quantity::zero(),
                    settling: revised.settling,
                },
                &replacement,
            )
            .await?;
        let fee_admission = self
            .verify_margin_fee_admission(account, &[fee_scope], &fee_caps)
            .await?;
        margin.orders[0] = revised;
        margin.controls = controls.clone();
        let mut revised_binding = b.clone();
        revised_binding["margin"] = json!(margin);
        revised_binding["parameters"] = replacement;
        revised_binding["marginFeeAdmission"] = fee_admission.clone();
        revised_binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::value_bounds::KEY);
        own.expected.push(EffectConstraint::SubmissionAmendment {
            operation_id: operation_id.into(),
            submission: original.clone(),
            revision: Box::new(gofin_domain::amendments::AmendmentScope {
                intent: full,
                binding: revised_binding,
                valuation: Some(full_value),
            }),
        });
        let mut prepared=PreparedOperation{operation:E::KrakenPrivatePostAmendOrder.operation().into(),input:input.clone(),binding:json!({"endpoint":E::KrakenPrivatePostAmendOrder.method(),"product":self.product,"account":account.name,"physicalIdentity":account.physical_identity,"parameters":wire,"requestId":controls.request_id,"amendmentPreviousScope":previous,"amendment":{"operationId":operation_id,"originalSubmission":original,"orderId":id}}),summary:format!("Amend original Kraken margin order {id}; original fills, full loan and native position baseline retain their original accounting owner."),checks:vec![],warnings:vec!["A margin amendment changes a standing leveraged order. Existing collateral, loan liabilities, liquidation and rollover exposure persist.".into()],observed_at:gofin_domain::now(),expires_at:(Utc::now()+Duration::seconds(90)).to_rfc3339(),valuation:Some(value),intent:own,action_deadline: None, operation_version:1};
        if clamp_to_filled {
            prepared.binding["amendment"]["clampToFilled"] =
                json!({"requestedQuantity":requested_quantity,"observedFilledQuantity":filled});
            prepared.warnings.push("The requested quantity is below independently observed existing fills. Kraken clamps the live native quantity to executed volume and cancels its remaining order; prior fills and full loan principal remain with the original operation.".into());
        }
        warnings_from_fee_evidence(&mut prepared, &fee_admission);
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    pub(crate) async fn prepare_margin_legacy_edit(
        &self,
        input: &Value,
        account: &KrakenAccount,
        parameters: Value,
        controls: &TradingControls,
        previous: Option<&gofin_domain::amendments::AmendmentScope>,
    ) -> Result<PreparedOperation> {
        let previous = previous.ok_or_else(|| {
            error(
                "KRAKEN_AMENDMENT_TARGET_INVALID",
                "Legacy margin edit needs the protected original cumulative scope.",
            )
        })?;
        contracts::validate_fin_controls(
            E::KrakenPrivatePostEditOrder,
            input,
            Some(&previous.binding),
        )?;
        let margin: MarginBinding = serde_json::from_value(previous.binding["margin"].clone())
            .map_err(|_| {
                error(
                    "KRAKEN_AMENDMENT_TARGET_INVALID",
                    "Original margin scope unavailable.",
                )
            })?;
        check_conditional_margin_revision_target(&margin, &parameters)?;
        if margin.orders.len() > 1 {
            return Box::pin(self.prepare_margin_batch_revision(
                E::KrakenPrivatePostEditOrder,
                input,
                account,
                parameters,
                controls,
                previous,
            ))
            .await;
        }
        if margin.orders.len() != 1 || parameters["pair"] != margin.orders[0].pair {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Legacy margin edit requires one original exact native market.",
            ));
        }
        if parameters["oflags"]
            .as_str()
            .is_some_and(|v| v.split(',').any(|v| !v.is_empty() && v != "post"))
        {
            return Err(error(
                "INVALID_INPUT",
                "Legacy margin edit can change only the documented post flag.",
            ));
        }
        let open = self
            .json(
                E::KrakenPrivatePostOpenOrders,
                Some(account),
                json!({"trades":true}),
            )
            .await?;
        let old = &margin.orders[0];
        let matches = result(&open)["open"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_ORDER_PROOF_PENDING",
                    "Margin open order map unavailable.",
                )
            })?
            .iter()
            .filter(|(id, row)| {
                if !old.order_ids.is_empty() {
                    old.order_ids.contains(id)
                } else if let Some(reference) = &old.user_reference {
                    row["userref"] == *reference
                } else {
                    row["cl_ord_id"] == old.client_id
                }
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(error(
                "KRAKEN_ORDER_NOT_OPEN",
                "Exactly one original margin order remainder must be open.",
            ));
        }
        let (id, row) = matches[0];
        crate::trading::check_legacy_native_trigger(row)?;
        if parameters["txid"].as_str().is_some_and(|v| v != id)
            || parameters["txid"]
                .as_i64()
                .is_some_and(|v| row["userref"].as_i64() != Some(v))
        {
            return Err(error(
                "KRAKEN_AMENDMENT_TARGET_MISMATCH",
                "Legacy edit selector differs from original margin order.",
            ));
        }
        let filled = atoms(&row["vol_exec"])?;
        let requested = parameters
            .get("volume")
            .map(positive)
            .transpose()?
            .unwrap_or(old.volume.clone());
        if requested < filled {
            return Err(error(
                "KRAKEN_AMENDMENT_BELOW_PROVED_FILL",
                "Legacy margin quantity cannot be below already executed predecessor fills.",
            ));
        }
        let mut mapped =
            json!({"txid":id,"order_qty":requested.checked_add(&filled)?.to_decimal(36)});
        for (from, to) in [
            ("displayvol", "display_qty"),
            (
                "price",
                if matches!(
                    previous.binding["parameters"]["ordertype"].as_str(),
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
            if let Some(v) = parameters.get(from) {
                mapped[to] = v.clone();
            }
        }
        let mut prepared = self
            .prepare_margin_amendment(input, account, mapped, controls, Some(previous))
            .await?;
        let mut native = parameters;
        native["txid"] = json!(id);
        for constraint in &mut prepared.intent.expected {
            if let EffectConstraint::SubmissionAmendment { revision, .. } = constraint {
                let flags = previous.binding["parameters"]["oflags"]
                    .as_str()
                    .unwrap_or("");
                let mut keep = flags
                    .split(',')
                    .filter(|f| !f.is_empty() && *f != "post")
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if native["oflags"]
                    .as_str()
                    .is_some_and(|f| f.split(',').any(|f| f == "post"))
                {
                    keep.push("post".into());
                }
                if keep.is_empty() {
                    revision.binding["parameters"]
                        .as_object_mut()
                        .unwrap()
                        .remove("oflags");
                } else {
                    revision.binding["parameters"]["oflags"] = json!(keep.join(","));
                }
            }
        }
        prepared.operation = E::KrakenPrivatePostEditOrder.operation().into();
        prepared.binding["endpoint"] = json!(E::KrakenPrivatePostEditOrder.method());
        prepared.binding["parameters"] = native;
        prepared.binding["legacyEdit"] = json!(true);
        prepared.summary = format!(
            "Legacy edit original margin order {id}; reserve predecessor executions plus the full requested successor margin exposure."
        );
        prepared.warnings.push("Legacy edit creates a new order identity and does not copy predecessor executions; settlement requires the proved complete lineage.".into());
        Ok(prepared)
    }
}

impl KrakenProfile {
    async fn resolve_margin_children(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        a: &KrakenAccount,
        orders: &mut [MarginOrder],
        binding: &Value,
    ) -> Result<bool> {
        if !orders.iter().any(|o| o.conditional_parent.is_some()) {
            return Ok(false);
        }
        let since = chrono::DateTime::parse_from_rfc3339(
            submission.data["submittedAt"].as_str().unwrap_or(""),
        )
        .map_err(|_| {
            error(
                "KRAKEN_SUBMISSION_INVALID",
                "Conditional margin submission time missing.",
            )
        })?
        .timestamp()
            - 1;
        let open = self
            .json(
                E::KrakenPrivatePostOpenOrders,
                Some(a),
                json!({"trades":true}),
            )
            .await?;
        let mut inventory = result(&open)["open"]
            .as_object()
            .ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                    "Generated margin open-order inventory missing.",
                )
            })?
            .clone();
        let mut offset = 0usize;
        let mut complete = false;
        let mut pages = BTreeSet::new();
        for _ in 0..64 {
            let closed = self
                .json(
                    E::KrakenPrivatePostClosedOrders,
                    Some(a),
                    json!({"trades":true,"start":since,"ofs":offset}),
                )
                .await?;
            let rows = result(&closed)["closed"].as_object().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                    "Generated margin closed-order inventory missing.",
                )
            })?;
            if !rows.is_empty() && !pages.insert(rows.keys().cloned().collect::<Vec<_>>()) {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_HISTORY_INCOMPLETE",
                    "Native child history repeated a page.",
                ));
            }
            for (id, row) in rows {
                if inventory.insert(id.clone(), row.clone()).is_some() {
                    return Err(error(
                        "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                        "A native order transitions between open and closed inventory; recover after the race.",
                    ));
                }
            }
            offset = offset.checked_add(rows.len()).ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_HISTORY_INCOMPLETE",
                    "Native history cursor overflow.",
                )
            })?;
            if rows.len() < 50
                || result(&closed)["count"]
                    .as_u64()
                    .is_some_and(|count| offset as u64 >= count)
            {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err(error(
                "KRAKEN_MARGIN_CHILD_HISTORY_INCOMPLETE",
                "Generated child history exceeds the bounded complete scope.",
            ));
        }
        let mut pending = false;
        let roots = orders
            .iter()
            .filter(|o| o.conditional_parent.is_none())
            .cloned()
            .collect::<Vec<_>>();
        for child in orders.iter_mut().filter(|o| o.conditional_parent.is_some()) {
            let parent = roots
                .iter()
                .find(|o| Some(&o.client_id) == child.conditional_parent.as_ref())
                .ok_or_else(|| {
                    error(
                        "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                        "Generated child has no original entry scope.",
                    )
                })?;
            let parent_rows = inventory
                .iter()
                .filter(|(id, row)| {
                    if !parent.order_ids.is_empty() {
                        parent.order_ids.contains(id)
                    } else {
                        row["cl_ord_id"] == parent.client_id
                    }
                })
                .collect::<Vec<_>>();
            if parent_rows.len() != 1 {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                    "Original conditional margin parent is absent or ambiguous.",
                ));
            }
            let (parent_id, parent_row) = parent_rows[0];
            let filled = atoms(&parent_row["vol_exec"])?;
            if filled > parent.volume {
                return Err(error(
                    "KRAKEN_MARGIN_BOUND_EXCEEDED",
                    "Original conditional parent fill exceeds its exact scope.",
                ));
            }
            if parent_row["descr"]["pair"] != parent.pair
                || parent_row["descr"]["type"] != if parent.buy { "buy" } else { "sell" }
            {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                    "Original generated margin parent changed market or direction.",
                ));
            }
            let mut generated = BTreeMap::new();
            let mut volume = Quantity::zero();
            let mut loan = Quantity::zero();
            let trade_ids = parent_row["trades"].as_array().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                    "Original parent execution identities missing.",
                )
            })?;
            for chunk in trade_ids.chunks(20) {
                let ids = chunk
                    .iter()
                    .map(|id| {
                        id.as_str().ok_or_else(|| {
                            error(
                                "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                                "Parent trade identity invalid.",
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                if ids.is_empty() {
                    continue;
                }
                let response = self
                    .json(
                        E::KrakenPrivatePostQueryTrades,
                        Some(a),
                        json!({"txid":ids.join(","),"trades":true,"ledgers":true}),
                    )
                    .await?;
                for id in ids {
                    let row = &result(&response)[id];
                    if row["ordertxid"] != *parent_id
                        || row["pair"] != parent.pair
                        || row["type"] != if parent.buy { "buy" } else { "sell" }
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                            "Generated position has no exact original parent trade.",
                        ));
                    }
                    let v = positive(&row["vol"])?;
                    let cost = atoms(&row["cost"])?;
                    volume = volume.checked_add(&v)?;
                    loan = loan.checked_add(&if parent.buy { cost.clone() } else { v.clone() })?;
                    if generated.insert(id.into(),json!({"pair":parent.pair,"type":if parent.buy{"buy"}else{"sell"},"posstatus":"open","vol":v.to_decimal(36),"vol_closed":"0","cost":cost.to_decimal(36),"nativeParentTrade":row})).is_some(){return Err(error("KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH","Original parent trade identity repeated."));}
                }
            }
            if volume != filled || loan > parent.maximum_loan {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_EVIDENCE_PENDING",
                    "Exact parent full-loan principal is incomplete or exceeds the frozen scope.",
                ));
            }
            let parameters = child.conditional_parameters.as_ref().ok_or_else(|| {
                error(
                    "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                    "Original child trigger/limit scope missing.",
                )
            })?;
            let mut ordered_volume = Quantity::zero();
            let approved_volume =
                conditional_revision::margin_child_goal(binding, parent_id, &filled)?;
            let overrides = binding["conditionalChildRevisions"].as_object();
            let mut child_ids = vec![];
            for (id, row) in inventory.iter().filter(|(id, row)| {
                conditional_revision::margin_child_belongs(id, row, parent_id, binding)
            }) {
                let parameters = overrides
                    .and_then(|o| o.get(id))
                    .and_then(|o| o.get("parameters"))
                    .filter(|p| p.is_object())
                    .unwrap_or(parameters);
                if row["descr"]["pair"] != child.pair
                    || row["descr"]["type"] != if child.buy { "sell" } else { "buy" }
                    || row["descr"]["ordertype"] != parameters["ordertype"]
                {
                    return Err(error(
                        "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH",
                        "Native generated margin close changed parent, market, direction or order type.",
                    ));
                }
                for field in ["price", "price2"] {
                    if let Some(approved) = parameters.get(field)
                        && atoms(approved)? != atoms(&row["descr"][field])?
                    {
                        return Err(error(
                            "KRAKEN_MARGIN_CHILD_PRICE_MISMATCH",
                            "Native generated close changed its approved exact trigger or execution limit.",
                        ));
                    }
                }
                let replaced = overrides.is_some_and(|overrides| {
                    overrides.values().any(|s| s["predecessorOrderId"] == *id)
                });
                ordered_volume = ordered_volume
                    .checked_add(&atoms(&row[if replaced { "vol_exec" } else { "vol" }])?)?;
                child_ids.push(id.clone());
            }
            if ordered_volume > filled
                || ordered_volume > approved_volume
                || ordered_volume > child.volume
            {
                return Err(error(
                    "KRAKEN_MARGIN_CHILD_QUANTITY_MISMATCH",
                    "Generated close quantity exceeds the proved original parent fills.",
                ));
            }
            pending |= ordered_volume < approved_volume;
            child.order_ids = child_ids;
            child.generated_positions = generated;
        }
        let _ = record;
        Ok(pending)
    }
}

#[cfg(test)]
mod loan_bound_tests {
    use super::*;
    #[test]
    fn closure_bounds_highest_native_opening_cost_without_average_or_fifo_assumption() {
        let records = json!({"CHEAP":{"pair":"PAIR","type":"buy","posstatus":"open","vol":"1","vol_closed":"0","cost":"100"},"EXPENSIVE":{"pair":"PAIR","type":"buy","posstatus":"open","vol":"1","vol_closed":"0","cost":"400"}});
        let prior = position_totals(&records, "PAIR", true).unwrap();
        assert_eq!(
            maximum_close_loan(&prior, &Quantity::from_decimal("1", 36).unwrap(), true).unwrap(),
            Quantity::from_decimal("400", 36).unwrap()
        );
        assert_eq!(
            maximum_close_loan(&prior, &Quantity::from_decimal("2", 36).unwrap(), true).unwrap(),
            prior.loan
        );
    }
    #[test]
    fn fractional_remaining_full_loan_closes_to_exact_zero_without_per_fill_drift() {
        let records = json!({"P":{"pair":"PAIR","type":"buy","posstatus":"open","vol":"3","vol_closed":"1","cost":"0.000000000000000000000000000000000001"}});
        let remaining = position_totals(&records, "PAIR", true).unwrap();
        assert_eq!(remaining.loan, Quantity::from_u64(1));
        let mut closed = records;
        closed["P"]["vol_closed"] = json!("3");
        assert!(
            position_totals(&closed, "PAIR", true)
                .unwrap()
                .loan
                .is_zero()
        );
    }
}
