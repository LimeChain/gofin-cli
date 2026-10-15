//! Bounded trading observations. Financial fields stay exact decimal strings;
//! SPOT cash and USD-M collateral/positions keep different venue semantics.
use crate::{
    BinanceAccount, BinanceConfig, BinanceEndpoint, BinanceRequest, BinanceTransport,
    CredentialHandle, ExactDecimal,
};
use gofin_domain::{Definition, ExecutionActor, GoFinError, OperationKind, now};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const MAX_ROWS: usize = 5_000;
const MAX_LIMIT: u16 = 1_000;
fn default_limit() -> u16 {
    100
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingMarketsInput {
    pub symbol: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingAccountInput {
    pub account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingPositionsInput {
    pub account: String,
    pub symbol: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingListInput {
    pub account: String,
    pub symbol: String,
    #[serde(default = "default_limit")]
    pub limit: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingOrderInput {
    pub account: String,
    pub symbol: String,
    pub order_id: Option<String>,
    pub client_order_id: Option<String>,
}

fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "read",
        message,
        "Check the named account and documented trading read inputs.",
    )
}
fn invalid_response() -> GoFinError {
    error(
        "VENUE_RESPONSE_INVALID",
        "Binance returned invalid or inexact trading data.",
    )
}
fn input<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, GoFinError> {
    serde_json::from_value(value.clone()).map_err(|_| {
        error(
            "INVALID_INPUT",
            "Trading read input does not match its schema.",
        )
    })
}
// Binance lists native Han identifiers (for example 币安人生USDT). Keep the
// request alphabet finite and the same in discovery, without normalizing names.
const SYMBOL_PATTERN: &str = r"^[A-Z0-9_\u3400-\u4DBF\u4E00-\u9FFF]{1,32}$";
fn han_character(value: char) -> bool {
    matches!(value, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}')
}
fn valid_symbol(value: &str) -> bool {
    (1..=32).contains(&value.chars().count())
        && value
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || han_character(c))
}
fn check_symbol(value: &str) -> Result<(), GoFinError> {
    if valid_symbol(value) {
        Ok(())
    } else {
        Err(error(
            "INVALID_SYMBOL",
            "Use the exact Binance symbol: at most 32 uppercase ASCII letters, digits, underscores or Han characters.",
        ))
    }
}
fn account<'a>(config: &'a BinanceConfig, name: &str) -> Result<&'a BinanceAccount, GoFinError> {
    if name.is_empty() || name.len() > 128 {
        return Err(error("INVALID_ACCOUNT", "Select a named Binance account."));
    }
    config
        .accounts
        .iter()
        .find(|account| account.name == name)
        .ok_or_else(|| {
            error(
                "UNKNOWN_CONNECTION",
                "The named Binance account is not enrolled.",
            )
        })
}
fn public_handle() -> CredentialHandle {
    CredentialHandle::new("public-market-data").expect("fixed public handle is valid")
}
async fn request(
    transport: &dyn BinanceTransport,
    credential: &CredentialHandle,
    request: BinanceRequest,
) -> Result<Value, GoFinError> {
    transport
        .request(credential, request)
        .await
        .map_err(|cause| {
            use crate::{HttpFailureKind, TransportError};
            let (code, message) = match cause {
                TransportError::RateLimited
                | TransportError::HttpFailure {
                    kind: HttpFailureKind::RateLimited,
                    ..
                } => (
                    "VENUE_RATE_LIMITED",
                    "Binance rate-limited this read; retry after its rate-limit window.",
                ),
                TransportError::PermissionDenied
                | TransportError::HttpFailure {
                    kind: HttpFailureKind::PermissionDenied,
                    ..
                } => (
                    "VENUE_PERMISSION_DENIED",
                    "The selected Binance credential cannot read this account data.",
                ),
                TransportError::InvalidResponse
                | TransportError::HttpFailure {
                    kind: HttpFailureKind::InvalidResponse,
                    ..
                } => (
                    "VENUE_RESPONSE_INVALID",
                    "Binance returned invalid trading data.",
                ),
                _ => (
                    "VENUE_UNAVAILABLE",
                    "Binance trading data could not be read. No order was submitted.",
                ),
            };
            error(code, message)
        })
}
fn rows(value: &Value) -> Result<&Vec<Value>, GoFinError> {
    value
        .as_array()
        .filter(|rows| rows.len() <= MAX_ROWS)
        .ok_or_else(invalid_response)
}
fn token(value: &Value, key: &str) -> Result<String, GoFinError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_:/.-".contains(c) || han_character(c))
        })
        .map(str::to_owned)
        .ok_or_else(invalid_response)
}
fn decimal(value: &Value, key: &str, signed: bool) -> Result<Value, GoFinError> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 128)
        .ok_or_else(invalid_response)?;
    let magnitude = if signed {
        text.strip_prefix('-').unwrap_or(text)
    } else {
        text
    };
    ExactDecimal::parse(magnitude).map_err(|_| invalid_response())?;
    Ok(Value::String(text.to_owned()))
}
fn identifier(value: &Value, key: &str) -> Result<Value, GoFinError> {
    let value = value.get(key).ok_or_else(invalid_response)?;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return Err(invalid_response()),
    };
    if text.is_empty() || text.len() > 40 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid_response());
    }
    Ok(Value::String(text))
}
fn project(
    row: &Value,
    tokens: &[&str],
    unsigned: &[&str],
    signed: &[&str],
    ids: &[&str],
) -> Result<Map<String, Value>, GoFinError> {
    if !row.is_object() {
        return Err(invalid_response());
    }
    let mut out = Map::new();
    for key in tokens {
        out.insert((*key).into(), Value::String(token(row, key)?));
    }
    for key in unsigned {
        out.insert((*key).into(), decimal(row, key, false)?);
    }
    for key in signed {
        out.insert((*key).into(), decimal(row, key, true)?);
    }
    for key in ids {
        out.insert((*key).into(), identifier(row, key)?);
    }
    Ok(out)
}
fn optional_decimal(
    row: &Value,
    out: &mut Map<String, Value>,
    keys: &[&str],
    signed: bool,
) -> Result<(), GoFinError> {
    for key in keys {
        if row.get(*key).is_some() {
            out.insert((*key).into(), decimal(row, key, signed)?);
        }
    }
    Ok(())
}
fn optional_bool(
    row: &Value,
    out: &mut Map<String, Value>,
    keys: &[&str],
) -> Result<(), GoFinError> {
    for key in keys {
        if let Some(value) = row.get(*key) {
            if !value.is_boolean() {
                return Err(invalid_response());
            }
            out.insert((*key).into(), value.clone());
        }
    }
    Ok(())
}
fn response(operation: &str, key: &str, data: Value, account_name: Option<&str>) -> Value {
    let mut result = json!({"operation":operation, "marketType":if operation.starts_with("binance.futures.") {"usdm_perpetual"} else {"spot"}, "observedAt":now()});
    result[key] = data;
    if let Some(name) = account_name {
        result["account"] = json!(name);
    }
    result
}

async fn markets(
    transport: &dyn BinanceTransport,
    futures: bool,
    symbol: Option<&str>,
) -> Result<Vec<Value>, GoFinError> {
    if let Some(symbol) = symbol {
        check_symbol(symbol)?;
    }
    let endpoint = if futures {
        BinanceEndpoint::FuturesExchangeInfo
    } else {
        BinanceEndpoint::SpotExchangeInfo
    };
    let mut req = BinanceRequest::new(endpoint);
    // USD-M exchangeInfo does not document a symbol request parameter.
    if !futures && let Some(symbol) = symbol {
        req = req.push("symbol", symbol);
    }
    let raw = request(transport, &public_handle(), req).await?;
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows(&raw["symbols"])? {
        let name = token(row, "symbol")?;
        if !valid_symbol(&name) || !seen.insert(name.clone()) {
            return Err(invalid_response());
        }
        if futures && token(row, "contractType")? != "PERPETUAL" {
            continue;
        }
        if symbol.is_some_and(|symbol| symbol != name) {
            continue;
        }
        let mut normalized = project(
            row,
            &["symbol", "status", "baseAsset", "quoteAsset"],
            &[],
            &[],
            &[],
        )?;
        if futures {
            normalized.insert("contractType".into(), json!("PERPETUAL"));
            normalized.insert("marginAsset".into(), json!(token(row, "marginAsset")?));
        }
        let mut filters = Vec::new();
        for filter in rows(&row["filters"])? {
            let kind = token(filter, "filterType")?;
            let keys: &[&str] = match kind.as_str() {
                "PRICE_FILTER" => &["minPrice", "maxPrice", "tickSize"],
                "LOT_SIZE" | "MARKET_LOT_SIZE" => &["minQty", "maxQty", "stepSize"],
                "MIN_NOTIONAL" if futures => &["notional"],
                "MIN_NOTIONAL" => &["minNotional"],
                "NOTIONAL" => &["minNotional", "maxNotional"],
                _ => continue,
            };
            let mut filter = project(filter, &["filterType"], keys, &[], &[])?;
            filter.insert("filterType".into(), json!(kind));
            filters.push(Value::Object(filter));
        }
        normalized.insert("filters".into(), json!(filters));
        optional_bool(
            row,
            &mut normalized,
            &["isSpotTradingAllowed", "isMarginTradingAllowed"],
        )?;
        found.push(Value::Object(normalized));
    }
    if symbol.is_some() && found.is_empty() {
        return Err(error(
            "UNSUPPORTED_MARKET",
            "The requested symbol is not in the supported Binance market scope.",
        ));
    }
    Ok(found)
}

fn normalize_order(row: &Value, symbol: &str, futures: bool) -> Result<Value, GoFinError> {
    if token(row, "symbol")? != symbol {
        return Err(invalid_response());
    }
    let mut out = project(
        row,
        &["symbol", "clientOrderId", "status", "side", "type"],
        &["price", "origQty", "executedQty"],
        &[],
        &["orderId"],
    )?;
    if !matches!(out["side"].as_str(), Some("BUY" | "SELL")) {
        return Err(invalid_response());
    }
    optional_decimal(
        row,
        &mut out,
        &["cummulativeQuoteQty", "cumQuote", "avgPrice", "stopPrice"],
        false,
    )?;
    if futures {
        let side = token(row, "positionSide")?;
        if !matches!(side.as_str(), "BOTH" | "LONG" | "SHORT") {
            return Err(invalid_response());
        }
        out.insert("positionSide".into(), json!(side));
        optional_bool(row, &mut out, &["reduceOnly", "closePosition"])?;
    }
    for key in ["time", "updateTime"] {
        if row.get(key).is_some() {
            out.insert(key.into(), identifier(row, key)?);
        }
    }
    Ok(Value::Object(out))
}
fn normalize_trade(row: &Value, symbol: &str, futures: bool) -> Result<Value, GoFinError> {
    if token(row, "symbol")? != symbol {
        return Err(invalid_response());
    }
    let mut out = project(
        row,
        &["symbol", "commissionAsset"],
        &["price", "qty", "quoteQty"],
        &["commission"],
        &["id", "orderId", "time"],
    )?;
    if futures {
        out.insert("realizedPnl".into(), decimal(row, "realizedPnl", true)?);
        for key in ["side", "positionSide"] {
            out.insert(key.into(), json!(token(row, key)?));
        }
    }
    optional_bool(row, &mut out, &["isBuyer", "isMaker", "buyer", "maker"])?;
    Ok(Value::Object(out))
}

/// Reads only named operations through the finite endpoint transport. No mutation,
/// recovery decision, or authoritative ledger effect is inferred from these rows.
pub async fn trading_read(
    operation: &str,
    raw: &Value,
    config: &BinanceConfig,
    transport: &dyn BinanceTransport,
) -> Result<Value, GoFinError> {
    let futures = operation.starts_with("binance.futures.");
    match operation {
        "binance.spot.markets" | "binance.futures.markets" => {
            let args: TradingMarketsInput = input(raw)?;
            Ok(response(
                operation,
                "markets",
                json!(markets(transport, futures, args.symbol.as_deref()).await?),
                None,
            ))
        }
        "binance.futures.balances" => {
            let args: TradingAccountInput = input(raw)?;
            let account = account(config, &args.account)?;
            let raw = request(
                transport,
                &account.credential,
                BinanceRequest::new(BinanceEndpoint::FuturesAccount),
            )
            .await?;
            let mut assets = Vec::new();
            let mut seen = BTreeSet::new();
            for row in rows(&raw["assets"])? {
                let mut out = project(
                    row,
                    &["asset"],
                    &["initialMargin", "maintMargin"],
                    &[
                        "walletBalance",
                        "unrealizedProfit",
                        "marginBalance",
                        "availableBalance",
                    ],
                    &[],
                )?;
                if !seen.insert(token(row, "asset")?) {
                    return Err(invalid_response());
                }
                optional_decimal(
                    row,
                    &mut out,
                    &[
                        "maxWithdrawAmount",
                        "crossWalletBalance",
                        "crossUnPnl",
                        "positionInitialMargin",
                        "openOrderInitialMargin",
                    ],
                    true,
                )?;
                optional_bool(row, &mut out, &["marginAvailable"])?;
                assets.push(Value::Object(out));
            }
            let mut result = response(operation, "balances", json!(assets), Some(&args.account));
            result["balanceSemantics"] = json!(
                "USD-M wallet collateral, margin equity, available collateral and unrealized PnL; not SPOT free/locked cash."
            );
            result["collateralScope"] =
                json!("USD-M account collateral is shared by perpetual and delivery contracts.");
            // V3 does not always include the account mode; absence stays unknown.
            result["multiAssetsMargin"] = match raw.get("multiAssetsMargin") {
                Some(Value::Bool(value)) => json!(value),
                None => Value::Null,
                _ => return Err(invalid_response()),
            };
            Ok(result)
        }
        "binance.futures.positions" => {
            let args: TradingPositionsInput = input(raw)?;
            let account = account(config, &args.account)?;
            let supported: BTreeSet<String> = markets(transport, true, args.symbol.as_deref())
                .await?
                .iter()
                .map(|row| row["symbol"].as_str().unwrap().to_owned())
                .collect();
            let mut req = BinanceRequest::new(BinanceEndpoint::FuturesPositions);
            if let Some(symbol) = &args.symbol {
                req = req.push("symbol", symbol);
            }
            let raw = request(transport, &account.credential, req).await?;
            let mut positions = Vec::new();
            let mut seen = BTreeSet::new();
            for row in rows(&raw)? {
                let symbol = token(row, "symbol")?;
                if args
                    .symbol
                    .as_deref()
                    .is_some_and(|selected| selected != symbol)
                {
                    return Err(invalid_response());
                }
                if !supported.contains(&symbol) {
                    continue;
                }
                let side = token(row, "positionSide")?;
                if !matches!(side.as_str(), "BOTH" | "LONG" | "SHORT")
                    || !seen.insert((symbol, side))
                {
                    return Err(invalid_response());
                }
                let mut out = project(
                    row,
                    &["symbol", "positionSide", "marginAsset"],
                    &["entryPrice", "markPrice"],
                    &["positionAmt", "unRealizedProfit", "notional"],
                    &[],
                )?;
                optional_decimal(
                    row,
                    &mut out,
                    &[
                        "breakEvenPrice",
                        "liquidationPrice",
                        "isolatedMargin",
                        "isolatedWallet",
                        "initialMargin",
                        "maintMargin",
                        "positionInitialMargin",
                        "openOrderInitialMargin",
                    ],
                    true,
                )?;
                positions.push(Value::Object(out));
            }
            Ok(response(
                operation,
                "positions",
                json!(positions),
                Some(&args.account),
            ))
        }
        "binance.spot.order" | "binance.futures.order" => {
            let args: TradingOrderInput = input(raw)?;
            check_symbol(&args.symbol)?;
            if args.order_id.is_some() == args.client_order_id.is_some() {
                return Err(error(
                    "INVALID_ORDER_SELECTOR",
                    "Provide exactly one order_id or client_order_id.",
                ));
            }
            let mut req = BinanceRequest::new(if futures {
                BinanceEndpoint::FuturesOrder
            } else {
                BinanceEndpoint::SpotOrder
            })
            .push("symbol", &args.symbol);
            if let Some(id) = &args.order_id {
                if id.starts_with('0')
                    || id.len() > 20
                    || !id.bytes().all(|b| b.is_ascii_digit())
                    || id.parse::<u64>().is_err()
                {
                    return Err(error(
                        "INVALID_ORDER_SELECTOR",
                        "order_id must be a positive u64 decimal integer string without leading zeros.",
                    ));
                }
                req = req.push("orderId", id);
            }
            if let Some(id) = &args.client_order_id {
                if id.is_empty()
                    || id.len() > 36
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.:/".contains(&b))
                {
                    return Err(error(
                        "INVALID_ORDER_SELECTOR",
                        "client_order_id must use at most 36 supported identifier characters.",
                    ));
                }
                req = req.push("origClientOrderId", id);
            }
            let account = account(config, &args.account)?;
            if futures {
                markets(transport, true, Some(&args.symbol)).await?;
            }
            let row = request(transport, &account.credential, req).await?;
            let order = normalize_order(&row, &args.symbol, futures)?;
            if args
                .order_id
                .as_ref()
                .is_some_and(|id| order["orderId"].as_str() != Some(id))
                || args
                    .client_order_id
                    .as_ref()
                    .is_some_and(|id| order["clientOrderId"].as_str() != Some(id))
            {
                return Err(invalid_response());
            }
            Ok(response(operation, "order", order, Some(&args.account)))
        }
        "binance.spot.orders"
        | "binance.futures.orders"
        | "binance.spot.trades"
        | "binance.futures.trades"
        | "binance.futures.income" => {
            let args: TradingListInput = input(raw)?;
            check_symbol(&args.symbol)?;
            if !(1..=MAX_LIMIT).contains(&args.limit) {
                return Err(error("INVALID_LIMIT", "limit must be between 1 and 1000."));
            }
            let account = account(config, &args.account)?;
            let orders = operation.ends_with(".orders");
            let income = operation.ends_with(".income");
            let endpoint = match operation {
                "binance.spot.orders" => BinanceEndpoint::SpotOpenOrders,
                "binance.futures.orders" => BinanceEndpoint::FuturesOpenOrders,
                "binance.spot.trades" => BinanceEndpoint::SpotTrades,
                "binance.futures.trades" => BinanceEndpoint::FuturesTrades,
                _ => BinanceEndpoint::FuturesIncome,
            };
            let mut req = BinanceRequest::new(endpoint).push("symbol", &args.symbol);
            if !orders {
                req = req.push("limit", args.limit.to_string());
            }
            if futures {
                markets(transport, true, Some(&args.symbol)).await?;
            }
            let raw = request(transport, &account.credential, req).await?;
            let source = rows(&raw)?;
            let mut normalized = Vec::new();
            for row in source {
                let value = if orders {
                    normalize_order(row, &args.symbol, futures)?
                } else if income {
                    if token(row, "symbol")? != args.symbol {
                        return Err(invalid_response());
                    }
                    Value::Object(project(
                        row,
                        &["symbol", "incomeType", "asset"],
                        &[],
                        &["income"],
                        &["tranId", "time"],
                    )?)
                } else {
                    normalize_trade(row, &args.symbol, futures)?
                };
                normalized.push(value);
            }
            let truncated = normalized.len() > usize::from(args.limit);
            normalized.truncate(usize::from(args.limit));
            let key = if orders {
                "orders"
            } else if income {
                "income"
            } else {
                "trades"
            };
            let mut result = response(operation, key, json!(normalized), Some(&args.account));
            result["limit"] = json!(args.limit);
            result["truncated"] = json!(truncated);
            result["coverage"] = json!(if orders {
                "current_open_orders"
            } else {
                "bounded_recent_history"
            });
            Ok(result)
        }
        _ => Err(error(
            "UNKNOWN_OPERATION",
            "Unsupported Binance trading read operation.",
        )),
    }
}

fn output_row_schema(key: &str, futures: bool) -> Value {
    let (required, text, decimals, ids): (&[&str], &[&str], &[&str], &[&str]) = match key {
        "markets" => (
            &["symbol", "status", "baseAsset", "quoteAsset", "filters"],
            &[
                "symbol",
                "status",
                "baseAsset",
                "quoteAsset",
                "contractType",
                "marginAsset",
            ],
            &[],
            &[],
        ),
        "balances" => (
            &[
                "asset",
                "walletBalance",
                "unrealizedProfit",
                "marginBalance",
                "availableBalance",
                "initialMargin",
                "maintMargin",
            ],
            &["asset"],
            &[
                "walletBalance",
                "unrealizedProfit",
                "marginBalance",
                "availableBalance",
                "initialMargin",
                "maintMargin",
                "maxWithdrawAmount",
                "crossWalletBalance",
                "crossUnPnl",
                "positionInitialMargin",
                "openOrderInitialMargin",
            ],
            &[],
        ),
        "positions" => (
            &[
                "symbol",
                "positionSide",
                "marginAsset",
                "positionAmt",
                "entryPrice",
                "markPrice",
                "unRealizedProfit",
                "notional",
            ],
            &["symbol", "positionSide", "marginAsset"],
            &[
                "positionAmt",
                "entryPrice",
                "markPrice",
                "unRealizedProfit",
                "notional",
                "breakEvenPrice",
                "liquidationPrice",
                "isolatedMargin",
                "isolatedWallet",
                "initialMargin",
                "maintMargin",
                "positionInitialMargin",
                "openOrderInitialMargin",
            ],
            &[],
        ),
        "order" | "orders" => (
            &[
                "symbol",
                "clientOrderId",
                "status",
                "side",
                "type",
                "price",
                "origQty",
                "executedQty",
                "orderId",
            ],
            &[
                "symbol",
                "clientOrderId",
                "status",
                "side",
                "type",
                "positionSide",
            ],
            &[
                "price",
                "origQty",
                "executedQty",
                "cummulativeQuoteQty",
                "cumQuote",
                "avgPrice",
                "stopPrice",
            ],
            &["orderId", "time", "updateTime"],
        ),
        "trades" => (
            &[
                "symbol",
                "commissionAsset",
                "price",
                "qty",
                "quoteQty",
                "commission",
                "id",
                "orderId",
                "time",
            ],
            &["symbol", "commissionAsset", "side", "positionSide"],
            &["price", "qty", "quoteQty", "commission", "realizedPnl"],
            &["id", "orderId", "time"],
        ),
        _ => (
            &["symbol", "incomeType", "asset", "income", "tranId", "time"],
            &["symbol", "incomeType", "asset"],
            &["income"],
            &["tranId", "time"],
        ),
    };
    let mut row = json!({"type":"object", "required":required, "properties":{}});
    for field in text {
        row["properties"][field] = json!({"type":"string", "maxLength":128});
    }
    for field in decimals {
        row["properties"][field] = json!({"type":"string", "pattern":"^-?(0|[1-9][0-9]*)(\\.[0-9]{1,36})?$", "description":"Exact venue decimal string; signed where the venue field represents PnL or exposure."});
    }
    for field in ids {
        row["properties"][field] = json!({"type":"string", "pattern":"^[0-9]{1,40}$"});
    }
    let booleans: &[&str] = match key {
        "markets" => &["isSpotTradingAllowed", "isMarginTradingAllowed"],
        "balances" => &["marginAvailable"],
        "order" | "orders" if futures => &["reduceOnly", "closePosition"],
        "trades" => &["isBuyer", "isMaker", "buyer", "maker"],
        _ => &[],
    };
    for field in booleans {
        row["properties"][field] = json!({"type":"boolean"});
    }
    if key == "markets" {
        row["properties"]["filters"] = json!({"type":"array","items":{"type":"object","required":["filterType"],"description":"Supported venue filter fields are exact unsigned decimal strings."}});
    }
    row
}

pub fn trading_read_definitions() -> Vec<Definition> {
    let mut defs = Vec::new();
    for (name, description, mut schema, key) in [
        (
            "binance.spot.markets",
            "Binance SPOT market identities and exact price/quantity filters.",
            Value::from(schema_for!(TradingMarketsInput)),
            "markets",
        ),
        (
            "binance.spot.orders",
            "Bounded current SPOT open orders for one symbol.",
            Value::from(schema_for!(TradingListInput)),
            "orders",
        ),
        (
            "binance.spot.order",
            "Query one SPOT order by exactly one venue or client identifier.",
            Value::from(schema_for!(TradingOrderInput)),
            "order",
        ),
        (
            "binance.spot.trades",
            "Bounded recent SPOT fills and commissions for one symbol.",
            Value::from(schema_for!(TradingListInput)),
            "trades",
        ),
        (
            "binance.futures.markets",
            "USD-M perpetual market identities and exact venue filters; delivery contracts excluded.",
            Value::from(schema_for!(TradingMarketsInput)),
            "markets",
        ),
        (
            "binance.futures.balances",
            "USD-M collateral, available balance, margin equity and unrealized PnL by asset.",
            Value::from(schema_for!(TradingAccountInput)),
            "balances",
        ),
        (
            "binance.futures.positions",
            "USD-M perpetual position quantities and margin/PnL observations, preserving hedge sides.",
            Value::from(schema_for!(TradingPositionsInput)),
            "positions",
        ),
        (
            "binance.futures.orders",
            "Bounded current USD-M perpetual open orders for one symbol.",
            Value::from(schema_for!(TradingListInput)),
            "orders",
        ),
        (
            "binance.futures.order",
            "Query one USD-M perpetual order by exactly one venue or client identifier.",
            Value::from(schema_for!(TradingOrderInput)),
            "order",
        ),
        (
            "binance.futures.trades",
            "Bounded recent USD-M perpetual fills, realized PnL and commissions.",
            Value::from(schema_for!(TradingListInput)),
            "trades",
        ),
        (
            "binance.futures.income",
            "Bounded recent income events for one USD-M perpetual symbol; signed funding/commission/PnL amounts.",
            Value::from(schema_for!(TradingListInput)),
            "income",
        ),
    ] {
        if let Some(properties) = schema["properties"].as_object_mut() {
            if let Some(symbol) = properties.get_mut("symbol") {
                symbol["pattern"] = json!(SYMBOL_PATTERN);
                symbol["description"] = json!(
                    "Exact Binance market identifier; uppercase ASCII letters, digits, underscores and native Han characters are supported without normalization."
                );
            }
            if let Some(account) = properties.get_mut("account") {
                account["minLength"] = json!(1);
                account["maxLength"] = json!(128);
            }
            if let Some(limit) = properties.get_mut("limit") {
                limit["minimum"] = json!(1);
                limit["maximum"] = json!(MAX_LIMIT);
            }
            if let Some(id) = properties.get_mut("order_id") {
                id["pattern"] = json!("^[1-9][0-9]{0,19}$");
                id["description"] =
                    json!("Positive u64 venue order identifier, encoded as a decimal string.");
            }
            if let Some(id) = properties.get_mut("client_order_id") {
                id["pattern"] = json!("^[A-Za-z0-9_.:/-]{1,36}$");
            }
        }
        if key == "order" {
            schema["oneOf"] = json!([
                {"required":["order_id"], "properties":{"order_id":{"type":"string"},"client_order_id":{"type":"null"}}},
                {"required":["client_order_id"], "properties":{"client_order_id":{"type":"string"},"order_id":{"type":"null"}}}
            ]);
        }
        let example = if key == "markets" {
            json!({"symbol":"ETHUSDT"})
        } else if key == "balances" {
            json!({"account":"main"})
        } else if key == "order" {
            json!({"account":"main","symbol":"ETHUSDT","order_id":"123"})
        } else if key == "positions" {
            json!({"account":"main","symbol":"ETHUSDT"})
        } else {
            json!({"account":"main","symbol":"ETHUSDT","limit":100})
        };
        let mut output = json!({"type":"object","required":["operation","marketType","observedAt",key],"properties":{"operation":{"const":name},"marketType":{"enum":["spot","usdm_perpetual"]},"observedAt":{"type":"string"},"account":{"type":"string"},"limit":{"type":"integer"},"truncated":{"type":"boolean"},"coverage":{"type":"string"}}});
        let row_schema = output_row_schema(key, name.starts_with("binance.futures."));
        output["properties"][key] = if key == "order" {
            row_schema
        } else {
            json!({"type":"array","maxItems":MAX_ROWS,"items":row_schema})
        };
        defs.push(Definition { name:name.into(), description:description.into(), kind:OperationKind::Read, execution_actor:ExecutionActor::AgentReadable,
            input_schema:schema, output_schema:output, error_schema:schema_for!(GoFinError).into(), examples:vec![example],
            coverage:"Implemented named read; synthetic fixture verification. Production account and live acceptance pending.".into(),
            completion:"A bounded venue observation; not a financial execution or terminal settlement proof.".into(),
            semantics:json!({"amounts":"exact decimal strings","identifiers":"decimal strings","marketScope":if name.starts_with("binance.futures.") {"USD-M perpetual"} else {"SPOT"},"history":"bounded recent slice, not complete history","credentials":"never returned"}),
        });
    }
    defs
}
