//! Closed, pure Hyperliquid read routing and exact native response normalization.
use crate::types::{ExactDecimal, MarketRef, NativeMarket};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_RESPONSE_ROWS: usize = 10_000;
const DECIMAL_KEYS: &[&str] = &[
    "px",
    "sz",
    "price",
    "amount",
    "size",
    "total",
    "hold",
    "entryNtl",
    "equity",
    "accountValue",
    "totalRawUsd",
    "totalMarginUsed",
    "totalNtlPos",
    "cumRealizedPnl",
    "withdrawable",
    "availableToTrade",
    "maxTradeSzs",
    "marginUsed",
    "unrealizedPnl",
    "realizedPnl",
    "cumFunding",
    "funding",
    "fundingRate",
    "markPx",
    "midPx",
    "prevDayPx",
    "openInterest",
    "notional",
    "fee",
    "fees",
    "volume",
    "dayNtlVlm",
    "oraclePx",
    "liquidationPx",
    "leverage",
    "maintenanceMarginUsed",
    "returnOnEquity",
    "triggerPx",
    "avgPx",
    "cost",
    "szi",
    "totalSz",
    "positionValue",
    "entryPx",
    "feeRate",
    "feeAmount",
    "balance",
    "available",
    "maxWithdrawable",
    "spotBalance",
    "accountBalance",
    "origSz",
    "closedPnl",
    "startPosition",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRoute {
    Markets,
    Currencies,
    Ticker,
    Tickers,
    OrderBook,
    Candles,
    Trades,
    FundingRate,
    FundingRates,
    FundingHistory,
    FundingRateHistory,
    OpenInterest,
    ExchangeStatus,
    Balances,
    Positions,
    Order,
    OpenOrders,
    ClosedOrders,
    CanceledOrders,
    Orders,
    MyTrades,
    MyLiquidations,
    Ledger,
    Deposits,
    Withdrawals,
    Fees,
    Abstraction,
}

/// Checked typed request intent. Account aliases stay separate from protected owner addresses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadIntent {
    pub route: ReadRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
}

/// This mirrors the fixed request vocabulary in the protected transport.
/// It intentionally has no caller-controlled endpoint, method, or raw params field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InfoRequest {
    MetaAndAssetCtxs,
    SpotMetaAndAssetCtxs,
    PerpDexs,
    L2Book {
        coin: String,
    },
    CandleSnapshot {
        coin: String,
        interval: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    RecentTrades {
        coin: String,
    },
    FundingHistory {
        coin: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    ExchangeStatus,
    AllMids,
    ClearinghouseState {
        owner: String,
    },
    SpotClearinghouseState {
        owner: String,
    },
    OpenOrders {
        owner: String,
    },
    FrontendOpenOrders {
        owner: String,
    },
    HistoricalOrders {
        owner: String,
    },
    OrderStatus {
        owner: String,
        order_id: u64,
    },
    UserFills {
        owner: String,
    },
    UserFillsByTime {
        owner: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    UserFunding {
        owner: String,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    },
    UserNonFundingLedgerUpdates {
        owner: String,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    },
    UserTwapSliceFills {
        owner: String,
    },
    UserRateLimit {
        owner: String,
    },
    UserFees {
        owner: String,
    },
    UserRole {
        owner: String,
    },
    UserVaultEquities {
        owner: String,
    },
    SubAccounts {
        owner: String,
    },
    UserAbstractionState {
        owner: String,
    },
    UserDexAbstractionState {
        owner: String,
    },
}

pub fn route_for(operation: &str) -> Option<ReadRoute> {
    Some(match operation {
        "hyperliquid.markets" => ReadRoute::Markets,
        "hyperliquid.currencies" => ReadRoute::Currencies,
        "hyperliquid.ticker" => ReadRoute::Ticker,
        "hyperliquid.tickers" => ReadRoute::Tickers,
        "hyperliquid.order_book" => ReadRoute::OrderBook,
        "hyperliquid.candles" => ReadRoute::Candles,
        "hyperliquid.trades" => ReadRoute::Trades,
        "hyperliquid.funding_rate" => ReadRoute::FundingRate,
        "hyperliquid.funding_rates" => ReadRoute::FundingRates,
        "hyperliquid.funding_history" => ReadRoute::FundingHistory,
        "hyperliquid.funding_rate_history" => ReadRoute::FundingRateHistory,
        "hyperliquid.open_interest" => ReadRoute::OpenInterest,
        "hyperliquid.time" => ReadRoute::ExchangeStatus,
        "hyperliquid.status" => ReadRoute::ExchangeStatus,
        "hyperliquid.balances" => ReadRoute::Balances,
        "hyperliquid.positions" => ReadRoute::Positions,
        "hyperliquid.order" => ReadRoute::Order,
        "hyperliquid.open_orders" => ReadRoute::OpenOrders,
        "hyperliquid.closed_orders" => ReadRoute::ClosedOrders,
        "hyperliquid.canceled_orders" => ReadRoute::CanceledOrders,
        "hyperliquid.orders" => ReadRoute::Orders,
        "hyperliquid.my_trades" => ReadRoute::MyTrades,
        "hyperliquid.my_liquidations" => ReadRoute::MyLiquidations,
        "hyperliquid.ledger" => ReadRoute::Ledger,
        "hyperliquid.deposits" => ReadRoute::Deposits,
        "hyperliquid.withdrawals" => ReadRoute::Withdrawals,
        "hyperliquid.fees" => ReadRoute::Fees,
        "hyperliquid.abstraction" => ReadRoute::Abstraction,
        _ => return None,
    })
}

/// Map a catalog intent to the closed transport vocabulary after protected account resolution.
pub fn info_request(intent: &ReadIntent, owner: Option<&str>) -> Result<InfoRequest, &'static str> {
    let user = || {
        owner
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or("protected owner required")
    };
    let symbol = || {
        intent
            .symbol
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or("symbol required")
    };
    let bounded_end = intent.end_ms;
    Ok(match intent.route {
        ReadRoute::Markets => InfoRequest::MetaAndAssetCtxs,
        ReadRoute::Currencies => InfoRequest::SpotMetaAndAssetCtxs,
        ReadRoute::Ticker | ReadRoute::Tickers => InfoRequest::AllMids,
        ReadRoute::OrderBook => InfoRequest::L2Book { coin: symbol()? },
        ReadRoute::Candles => InfoRequest::CandleSnapshot {
            coin: symbol()?,
            interval: intent
                .interval
                .clone()
                .filter(|s| is_interval(s))
                .ok_or("supported interval required")?,
            start_time_ms: intent.start_ms.ok_or("start_ms required")?,
            end_time_ms: bounded_end.ok_or("end_ms required")?,
        },
        ReadRoute::Trades => InfoRequest::RecentTrades { coin: symbol()? },
        ReadRoute::FundingRate | ReadRoute::FundingRates | ReadRoute::OpenInterest => {
            InfoRequest::MetaAndAssetCtxs
        }
        ReadRoute::FundingHistory => InfoRequest::UserFunding {
            owner: user()?,
            start_time_ms: intent.start_ms.ok_or("start_ms required")?,
            end_time_ms: bounded_end,
        },
        ReadRoute::FundingRateHistory => InfoRequest::FundingHistory {
            coin: symbol()?,
            start_time_ms: intent.start_ms.ok_or("start_ms required")?,
            end_time_ms: bounded_end.ok_or("end_ms required")?,
        },
        ReadRoute::ExchangeStatus => InfoRequest::ExchangeStatus,
        ReadRoute::Balances => InfoRequest::ClearinghouseState { owner: user()? },
        ReadRoute::Positions => InfoRequest::ClearinghouseState { owner: user()? },
        ReadRoute::Order => InfoRequest::OrderStatus {
            owner: user()?,
            order_id: intent
                .order_id
                .as_deref()
                .ok_or("order id required")?
                .parse()
                .map_err(|_| "order id must be an unsigned integer")?,
        },
        ReadRoute::OpenOrders => InfoRequest::FrontendOpenOrders { owner: user()? },
        ReadRoute::ClosedOrders | ReadRoute::CanceledOrders | ReadRoute::Orders => {
            InfoRequest::HistoricalOrders { owner: user()? }
        }
        ReadRoute::MyTrades => InfoRequest::UserFillsByTime {
            owner: user()?,
            start_time_ms: intent.start_ms.ok_or("start_ms required")?,
            end_time_ms: bounded_end.ok_or("end_ms required")?,
        },
        ReadRoute::MyLiquidations if intent.start_ms.is_some() || intent.end_ms.is_some() => {
            InfoRequest::UserFillsByTime {
                owner: user()?,
                start_time_ms: intent.start_ms.unwrap_or(0),
                end_time_ms: bounded_end
                    .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() as u64),
            }
        }
        ReadRoute::MyLiquidations => InfoRequest::UserFills { owner: user()? },
        ReadRoute::Ledger | ReadRoute::Deposits | ReadRoute::Withdrawals => {
            InfoRequest::UserNonFundingLedgerUpdates {
                owner: user()?,
                start_time_ms: intent.start_ms.ok_or("start_ms required")?,
                end_time_ms: bounded_end,
            }
        }
        ReadRoute::Fees => InfoRequest::UserFees { owner: user()? },
        ReadRoute::Abstraction => InfoRequest::UserAbstractionState { owner: user()? },
    })
}
/// Some public reads are composed from two closed info requests by the adapter.
pub fn info_requests(
    intent: &ReadIntent,
    owner: Option<&str>,
) -> Result<Vec<InfoRequest>, &'static str> {
    if intent.route == ReadRoute::Abstraction {
        let address = owner
            .filter(|s| !s.is_empty())
            .ok_or("protected owner required")?
            .to_owned();
        return Ok(vec![
            InfoRequest::UserAbstractionState {
                owner: address.clone(),
            },
            InfoRequest::UserDexAbstractionState { owner: address },
        ]);
    }
    if intent.route == ReadRoute::Balances {
        let address = owner
            .filter(|s| !s.is_empty())
            .ok_or("protected owner required")?
            .to_owned();
        return Ok(vec![
            InfoRequest::ClearinghouseState {
                owner: address.clone(),
            },
            InfoRequest::SpotClearinghouseState { owner: address },
        ]);
    }
    Ok(vec![info_request(intent, owner)?])
}

/// Convert the local closed query model to the pinned CCXT Rust transport enum.
pub fn to_ccxt_info_request(request: InfoRequest) -> gofin_exchange::ccxt::HyperliquidInfoRequest {
    use gofin_exchange::ccxt::HyperliquidInfoRequest as C;
    match request {
        InfoRequest::MetaAndAssetCtxs => C::MetaAndAssetCtxs,
        InfoRequest::SpotMetaAndAssetCtxs => C::SpotMetaAndAssetCtxs,
        InfoRequest::PerpDexs => C::PerpDexs,
        InfoRequest::AllMids => C::AllMids,
        InfoRequest::L2Book { coin } => C::L2Book { coin },
        InfoRequest::CandleSnapshot {
            coin,
            interval,
            start_time_ms,
            end_time_ms,
        } => C::CandleSnapshot {
            coin,
            interval,
            start_time_ms,
            end_time_ms,
        },
        InfoRequest::RecentTrades { coin } => C::RecentTrades { coin },
        InfoRequest::FundingHistory {
            coin,
            start_time_ms,
            end_time_ms,
        } => C::FundingHistory {
            coin,
            start_time_ms,
            end_time_ms,
        },
        InfoRequest::ExchangeStatus => C::ExchangeStatus,
        InfoRequest::ClearinghouseState { owner } => C::ClearinghouseState { owner },
        InfoRequest::SpotClearinghouseState { owner } => C::SpotClearinghouseState { owner },
        InfoRequest::OpenOrders { owner } => C::OpenOrders { owner },
        InfoRequest::FrontendOpenOrders { owner } => C::FrontendOpenOrders { owner },
        InfoRequest::HistoricalOrders { owner } => C::HistoricalOrders { owner },
        InfoRequest::OrderStatus { owner, order_id } => C::OrderStatus { owner, order_id },
        InfoRequest::UserFills { owner } => C::UserFills { owner },
        InfoRequest::UserFillsByTime {
            owner,
            start_time_ms,
            end_time_ms,
        } => C::UserFillsByTime {
            owner,
            start_time_ms,
            end_time_ms,
        },
        InfoRequest::UserFunding {
            owner,
            start_time_ms,
            end_time_ms,
        } => C::UserFunding {
            owner,
            start_time_ms,
            end_time_ms,
        },
        InfoRequest::UserNonFundingLedgerUpdates {
            owner,
            start_time_ms,
            end_time_ms,
        } => C::UserNonFundingLedgerUpdates {
            owner,
            start_time_ms,
            end_time_ms,
        },
        InfoRequest::UserTwapSliceFills { owner } => C::UserTwapSliceFills { owner },
        InfoRequest::UserRateLimit { owner } => C::UserRateLimit { owner },
        InfoRequest::UserFees { owner } => C::UserFees { owner },
        InfoRequest::UserRole { owner } => C::UserRole { owner },
        InfoRequest::UserVaultEquities { owner } => C::UserVaultEquities { owner },
        InfoRequest::SubAccounts { owner } => C::SubAccounts { owner },
        InfoRequest::UserAbstractionState { owner } => C::UserAbstractionState { owner },
        InfoRequest::UserDexAbstractionState { owner } => C::UserDexAbstractionState { owner },
    }
}

fn is_interval(s: &str) -> bool {
    matches!(
        s,
        "1m" | "3m"
            | "5m"
            | "15m"
            | "30m"
            | "1h"
            | "2h"
            | "4h"
            | "8h"
            | "12h"
            | "1d"
            | "3d"
            | "1w"
            | "1M"
    )
}

/// Convert only documented decimal-valued numeric fields to strings, recursively.
/// Other numeric fields (indices, IDs and timestamps) retain their integer type.
pub fn normalize_native_response(raw: Value) -> Result<Value, &'static str> {
    fn walk(value: Value, key: Option<&str>, rows: &mut usize) -> Result<Value, &'static str> {
        Ok(match value {
            Value::Number(n) if key.is_some_and(|k| DECIMAL_KEYS.contains(&k)) => {
                let text = n.to_string();
                ExactDecimal::parse(text.clone())?;
                Value::String(text)
            }
            Value::Array(values) => {
                *rows = rows.saturating_add(values.len());
                if *rows > MAX_RESPONSE_ROWS {
                    return Err("response too large");
                }
                Value::Array(
                    values
                        .into_iter()
                        .map(|v| walk(v, key, rows))
                        .collect::<Result<_, _>>()?,
                )
            }
            Value::Object(values) => {
                let mut normalized = Map::new();
                for (k, v) in values {
                    normalized.insert(k.clone(), walk(v, Some(&k), rows)?);
                }
                Value::Object(normalized)
            }
            other => other,
        })
    }
    walk(raw, None, &mut 0)
}

/// Resolve an exact native market identity from metadata supplied by the adapter.
pub fn resolve_market(
    markets: &[NativeMarket],
    request: &MarketRef,
) -> Result<NativeMarket, &'static str> {
    if markets.len() > MAX_RESPONSE_ROWS {
        return Err("market metadata too large");
    }
    let mut matches = markets
        .iter()
        .filter(|m| m.symbol == request.symbol && m.kind == request.kind && m.dex == request.dex);
    let market = matches
        .next()
        .ok_or("market not found in native metadata")?;
    if matches.next().is_some() {
        return Err("market identity is ambiguous");
    }
    if market.kind == crate::types::MarketKind::Spot {
        if market.asset_id < 10_000 || market.spot_tokens.is_none() {
            return Err("spot asset identity is invalid");
        }
    } else if market.spot_tokens.is_some() {
        return Err("perpetual market has spot token indices");
    }
    Ok(market.clone())
}

/// Derive the exchange action asset ID from native metadata indices.
pub fn native_asset_id(
    kind: &crate::types::MarketKind,
    universe_index: u32,
    dex_index: Option<u32>,
) -> Result<u32, &'static str> {
    match kind {
        crate::types::MarketKind::Perpetual if dex_index.is_none() => Ok(universe_index),
        crate::types::MarketKind::Spot if dex_index.is_none() => 10_000u32
            .checked_add(universe_index)
            .ok_or("spot asset id overflow"),
        crate::types::MarketKind::Hip3Perpetual => {
            let dex = dex_index.ok_or("HIP-3 dex index required")?;
            100_000u32
                .checked_add(dex.checked_mul(10_000).ok_or("HIP-3 asset id overflow")?)
                .and_then(|base| base.checked_add(universe_index))
                .ok_or("HIP-3 asset id overflow")
        }
        _ => Err("dex index does not match market kind"),
    }
}

/// Parse primary or one HIP-3 perp DEX response. `dex_index` is the exact position
/// of that DEX in the protected `perpDexs` response; contexts are paired by native index.
pub fn parse_perpetual_markets(
    raw: &Value,
    dex: Option<&str>,
    dex_index: Option<u32>,
    observed_at_ms: u64,
) -> Result<Vec<NativeMarket>, &'static str> {
    parse_perpetual_markets_with_spot(raw, dex, dex_index, None, observed_at_ms)
}

/// HIP-3 metadata encodes its collateral as a spot-token index. Resolve that index
/// against the exact spot token table before exposing a quote asset.
pub fn parse_perpetual_markets_with_spot(
    raw: &Value,
    dex: Option<&str>,
    dex_index: Option<u32>,
    spot_raw: Option<&Value>,
    observed_at_ms: u64,
) -> Result<Vec<NativeMarket>, &'static str> {
    use crate::types::AssetReference;
    let pair = raw.as_array().ok_or("expected [meta, asset contexts]")?;
    if pair.len() != 2 {
        return Err("expected [meta, asset contexts]");
    }
    let meta = &pair[0];
    let universe = meta
        .get("universe")
        .and_then(Value::as_array)
        .ok_or("missing perp universe")?;
    let contexts = pair[1].as_array().ok_or("missing perp asset contexts")?;
    if universe.len() != contexts.len() || universe.len() > MAX_RESPONSE_ROWS {
        return Err("perp metadata/context count mismatch");
    }
    match (dex, dex_index) {
        (None, None) => {}
        (Some(name), Some(_)) if !name.is_empty() => {}
        _ => return Err("HIP-3 DEX name and index must be supplied together"),
    }
    let hip3_quote = if dex.is_some() {
        let collateral = meta
            .get("collateralToken")
            .ok_or("HIP-3 collateral token index missing")?;
        let index = collateral
            .as_u64()
            .or_else(|| collateral.as_str()?.parse::<u64>().ok())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("invalid HIP-3 collateral token index")?;
        let spot = spot_raw.ok_or("HIP-3 collateral token metadata required")?;
        let tokens = spot
            .get(0)
            .and_then(|m| m.get("tokens"))
            .and_then(Value::as_array)
            .ok_or("spot token metadata missing")?;
        let token = tokens
            .iter()
            .find(|t| t.get("index").and_then(Value::as_u64) == Some(u64::from(index)))
            .ok_or("HIP-3 collateral token not found")?;
        let symbol = token
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("HIP-3 collateral token name missing")?;
        let reference = token
            .get("tokenId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("HIP-3 collateral token ID missing")?;
        let decimals = token
            .get("weiDecimals")
            .and_then(Value::as_u64)
            .and_then(|n| u8::try_from(n).ok())
            .ok_or("HIP-3 collateral token decimals missing")?;
        Some(AssetReference {
            reference: reference.to_owned(),
            symbol: symbol.to_owned(),
            decimals,
            token_index: Some(index),
        })
    } else {
        None
    };
    universe
        .iter()
        .zip(contexts)
        .enumerate()
        .map(|(index, (meta, ctx))| {
            let coin = meta
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("perp coin missing")?;
            let native_coin = match dex {
                Some(name) if coin.starts_with(&format!("{name}:")) => coin.to_owned(),
                Some(name) => format!("{name}:{coin}"),
                None => coin.to_owned(),
            };
            let kind = if dex.is_some() {
                crate::types::MarketKind::Hip3Perpetual
            } else {
                crate::types::MarketKind::Perpetual
            };
            let universe_index = u32::try_from(index).map_err(|_| "perp index overflow")?;
            let asset_id = native_asset_id(&kind, universe_index, dex_index)?;
            let sz_decimals = meta
                .get("szDecimals")
                .and_then(Value::as_u64)
                .and_then(|x| u8::try_from(x).ok())
                .ok_or("invalid szDecimals")?;
            let max_leverage = meta
                .get("maxLeverage")
                .and_then(Value::as_u64)
                .and_then(|x| u32::try_from(x).ok());
            let is_delisted = meta
                .get("isDelisted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let base = native_coin.clone();
            let context = parse_market_context(ctx)?;
            Ok(NativeMarket {
                symbol: {
                    let quote = hip3_quote.as_ref().map_or("USDC", |a| a.symbol.as_str());
                    format!("{native_coin}/{quote}:{quote}")
                },
                native_coin: native_coin.clone(),
                base_asset: AssetReference {
                    reference: native_coin,
                    symbol: base,
                    decimals: sz_decimals,
                    token_index: None,
                },
                quote_asset: hip3_quote.clone().unwrap_or_else(|| AssetReference {
                    reference: "USDC".into(),
                    symbol: "USDC".into(),
                    decimals: 6,
                    token_index: None,
                }),
                kind,
                observed_at_ms,
                dex: dex.map(str::to_owned),
                universe_index,
                dex_index,
                asset_id,
                sz_decimals,
                max_leverage,
                is_delisted,
                context: Some(context),
                spot_tokens: None,
            })
        })
        .collect()
}

/// Parse spot pairs and contexts from `spotMetaAndAssetCtxs`; token indexes and
/// token IDs stay distinct from the spot pair's action asset ID.
pub fn parse_spot_markets(
    raw: &Value,
    observed_at_ms: u64,
) -> Result<Vec<NativeMarket>, &'static str> {
    use crate::types::{AssetReference, MarketKind};
    let pair = raw
        .as_array()
        .ok_or("expected [spot metadata, asset contexts]")?;
    if pair.len() != 2 {
        return Err("expected [spot metadata, asset contexts]");
    }
    let meta = &pair[0];
    let tokens = meta
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or("missing spot tokens")?;
    let universe = meta
        .get("universe")
        .and_then(Value::as_array)
        .ok_or("missing spot universe")?;
    let contexts = pair[1].as_array().ok_or("missing spot contexts")?;
    if universe.len() > MAX_RESPONSE_ROWS || contexts.len() > MAX_RESPONSE_ROWS {
        return Err("spot metadata or contexts too large");
    }
    let token_by_index = |idx: u32| -> Result<(&Value, &str), &'static str> {
        let token = tokens
            .iter()
            .find(|t| t.get("index").and_then(Value::as_u64) == Some(u64::from(idx)))
            .ok_or("spot token index missing")?;
        let name = token
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("spot token name missing")?;
        Ok((token, name))
    };
    let mut seen_indexes = std::collections::BTreeSet::new();
    universe
        .iter()
        .map(|market| {
            let native_index = market
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("spot market index missing or invalid")?;
            if !seen_indexes.insert(native_index) {
                return Err("duplicate spot market index");
            }
            let ctx = contexts
                .get(usize::try_from(native_index).map_err(|_| "spot index overflow")?)
                .ok_or("spot context missing at native market index")?;
            if let Some(context_coin) = ctx.get("coin").and_then(Value::as_str)
                && market.get("name").and_then(Value::as_str) != Some(context_coin)
            {
                return Err("spot context does not match native market index");
            }
            let refs = market
                .get("tokens")
                .and_then(Value::as_array)
                .filter(|v| v.len() == 2)
                .ok_or("spot token pair missing")?;
            let base_idx = refs[0]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("invalid base token index")?;
            let quote_idx = refs[1]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("invalid quote token index")?;
            let (base, base_symbol) = token_by_index(base_idx)?;
            let (quote, quote_symbol) = token_by_index(quote_idx)?;
            let token_ref = |token: &Value, symbol: &str| -> Result<AssetReference, &'static str> {
                let reference = token
                    .get("tokenId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or("spot token ID missing")?;
                let decimals = token
                    .get("weiDecimals")
                    .and_then(Value::as_u64)
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or("spot token weiDecimals missing")?;
                Ok(AssetReference {
                    reference: reference.to_owned(),
                    symbol: symbol.to_owned(),
                    decimals,
                    token_index: token
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok()),
                })
            };
            let base_ref = token_ref(base, base_symbol)?;
            let quote_ref = token_ref(quote, quote_symbol)?;
            let sz_decimals = base
                .get("szDecimals")
                .and_then(Value::as_u64)
                .and_then(|n| u8::try_from(n).ok())
                .ok_or("spot base szDecimals missing")?;
            let asset_id = native_asset_id(&MarketKind::Spot, native_index, None)?;
            Ok(NativeMarket {
                symbol: format!("{base_symbol}/{quote_symbol}"),
                native_coin: format!("@{native_index}"),
                base_asset: base_ref,
                quote_asset: quote_ref,
                kind: MarketKind::Spot,
                observed_at_ms,
                dex: None,
                universe_index: native_index,
                dex_index: None,
                asset_id,
                sz_decimals,
                max_leverage: None,
                is_delisted: market
                    .get("isDelisted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                context: Some(parse_market_context(ctx)?),
                spot_tokens: Some([base_idx, quote_idx]),
            })
        })
        .collect()
}

fn parse_market_context(raw: &Value) -> Result<crate::types::MarketContext, &'static str> {
    fn field(raw: &Value, name: &str) -> Result<Option<crate::types::ExactDecimal>, &'static str> {
        let Some(value) = raw.get(name) else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Err("invalid market decimal"),
        };
        Ok(Some(crate::types::ExactDecimal::parse(text)?))
    }
    Ok(crate::types::MarketContext {
        mark_price: field(raw, "markPx")?,
        mid_price: field(raw, "midPx")?,
        oracle_price: field(raw, "oraclePx")?,
        previous_day_price: field(raw, "prevDayPx")?,
        day_notional_volume: field(raw, "dayNtlVlm")?,
        funding_rate: field(raw, "funding")?,
        open_interest: field(raw, "openInterest")?,
    })
}

/// Hyperliquid precision: size scale from `szDecimals`; price <= 5 significant figures
/// and the applicable 6/8 minus size-decimal scale, while integer prices are exempt.
pub fn validate_order_precision(
    market: &NativeMarket,
    size: &str,
    price: &str,
) -> Result<(), &'static str> {
    let size = ExactDecimal::parse(size.to_owned())?;
    let price = ExactDecimal::parse(price.to_owned())?;
    if size.as_str().starts_with('-') || is_zero(size.as_str()) {
        return Err("size must be positive");
    }
    if price.as_str().starts_with('-') || is_zero(price.as_str()) {
        return Err("price must be positive");
    }
    if fractional_digits(size.as_str()) > market.sz_decimals as usize {
        return Err("size exceeds szDecimals");
    }
    let max_decimals = (if market.kind == crate::types::MarketKind::Spot {
        8u8
    } else {
        6u8
    })
    .saturating_sub(market.sz_decimals) as usize;
    if fractional_digits(price.as_str()) > max_decimals {
        return Err("price has too many decimal places");
    }
    if !is_integer(price.as_str()) && significant_figures(price.as_str()) > 5 {
        return Err("price exceeds five significant figures");
    }
    Ok(())
}
fn fractional_digits(s: &str) -> usize {
    s.split_once('.')
        .map_or(0, |(_, f)| f.trim_end_matches('0').len())
}

// Hyperliquid normalizes L1 decimals before hashing; a non-canonical signed string recovers a different signer.
pub(crate) fn wire_decimal(s: &str) -> String {
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}
fn is_zero(s: &str) -> bool {
    s.bytes().all(|b| !b.is_ascii_digit() || b == b'0')
}
fn is_integer(s: &str) -> bool {
    s.split_once('.')
        .is_none_or(|(_, f)| f.bytes().all(|b| b == b'0'))
}
fn significant_figures(s: &str) -> usize {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let significant = digits.trim_start_matches('0').trim_end_matches('0');
    significant.len().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MarketKind;
    use serde_json::json;

    #[test]
    fn wire_decimals_drop_only_redundant_zeros() {
        assert_eq!(wire_decimal("0.30"), "0.3");
        assert_eq!(wire_decimal("110.0"), "110");
        assert_eq!(wire_decimal("100"), "100");
        assert_eq!(wire_decimal("007.50"), "7.5");
        assert_eq!(wire_decimal("0.000"), "0");
        assert_eq!(wire_decimal("0.001"), "0.001");
    }

    #[test]
    fn exact_decimals_are_never_floats_in_normalized_market_data() {
        let raw: Value = serde_json::from_str(r#"{"px":9007199254740993.123456789,"time":9007199254740993,"szDecimals":4,"totalRawUsd":12345678901234567890.1234,"nested":{"equity":1.2300}}"#).unwrap();
        let got = normalize_native_response(raw).unwrap();
        assert_eq!(got["px"], "9007199254740993.123456789");
        assert_eq!(got["nested"]["equity"], "1.2300");
        assert_eq!(got["time"], 9007199254740993u64);
        assert_eq!(got["szDecimals"], 4);
        assert_eq!(got["totalRawUsd"], "12345678901234567890.1234");
    }

    #[test]
    fn active_asset_capacity_arrays_normalize_as_exact_decimal_strings() {
        let raw: Value = serde_json::from_str(
            r#"{"availableToTrade":[0.125,9007199254740993.25],"maxTradeSzs":[1.5,2],"assetIds":[17,18]}"#,
        )
        .unwrap();
        let normalized = normalize_native_response(raw).unwrap();
        assert_eq!(
            normalized["availableToTrade"],
            json!(["0.125", "9007199254740993.25"])
        );
        assert_eq!(normalized["maxTradeSzs"], json!(["1.5", "2"]));
        assert_eq!(normalized["assetIds"], json!([17, 18]));
    }
    #[test]
    fn routing_is_closed_and_requires_protected_owner_for_user_reads() {
        assert!(route_for("hyperliquid.raw").is_none());
        let intent = ReadIntent {
            route: ReadRoute::Balances,
            account: Some("main".into()),
            symbol: None,
            order_id: None,
            start_ms: None,
            end_ms: None,
            interval: None,
            limit: None,
        };
        assert!(info_request(&intent, None).is_err());
        let requests = info_requests(&intent, Some("0xowner")).unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(&requests[0], InfoRequest::ClearinghouseState { owner } if owner == "0xowner")
        );
        assert!(
            matches!(&requests[1], InfoRequest::SpotClearinghouseState { owner } if owner == "0xowner")
        );
    }
    #[test]
    fn hip3_and_spot_identity_are_explicit_and_precision_is_metadata_driven() {
        let hip3 = json!([{"collateralToken":0,"universe":[{"name":"ABC","szDecimals":2,"maxLeverage":10}]},[{"markPx":"123.45"}]]);
        let collateral = json!([{"tokens":[{"name":"USDC","index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","weiDecimals":8,"szDecimals":8}],"universe":[]},[]]);
        let market =
            parse_perpetual_markets_with_spot(&hip3, Some("xyz"), Some(0), Some(&collateral), 1)
                .unwrap()
                .remove(0);
        let r = MarketRef {
            symbol: market.symbol.clone(),
            kind: market.kind.clone(),
            dex: market.dex.clone(),
        };
        assert_eq!(
            resolve_market(std::slice::from_ref(&market), &r)
                .unwrap()
                .asset_id,
            100_000
        );
        assert!(validate_order_precision(&market, "1.23", "123.45").is_ok());
        assert!(validate_order_precision(&market, "1.234", "123.45").is_err());
        let mut spot = parse_spot_markets(&json!([
            {"tokens":[{"name":"USDC","index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","weiDecimals":8,"szDecimals":8},{"name":"X","index":1,"tokenId":"x","weiDecimals":8,"szDecimals":2}],"universe":[{"tokens":[1,0],"index":0}]},
            [{"markPx":"1"}]
        ]), 1).unwrap().remove(0);
        spot.asset_id = 9999;
        assert!(
            resolve_market(
                &[spot.clone()],
                &MarketRef {
                    symbol: spot.symbol,
                    kind: MarketKind::Spot,
                    dex: None
                }
            )
            .is_err()
        );
    }
    #[test]
    fn metadata_parser_resolves_perps_hip3_spot_and_contexts_by_native_index() {
        let primary = json!([{"universe":[{"name":"ETH","szDecimals":4,"maxLeverage":50}]},[{"markPx":"3000.125","funding":"0.0001","openInterest":"9.50"}]]);
        let core = parse_perpetual_markets(&primary, None, None, 10).unwrap();
        assert_eq!(core[0].symbol, "ETH/USDC:USDC");
        assert_eq!(core[0].asset_id, 0);
        assert_eq!(
            core[0]
                .context
                .as_ref()
                .unwrap()
                .mark_price
                .as_ref()
                .unwrap()
                .as_str(),
            "3000.125"
        );

        let hip3 = json!([{"collateralToken":"0","universe":[{"name":"ABC","szDecimals":2,"maxLeverage":5}]},[{"markPx":"12.34"}]]);
        let collateral = json!([{"tokens":[{"name":"USDC","index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","weiDecimals":8,"szDecimals":8}],"universe":[]},[]]);
        let builder =
            parse_perpetual_markets_with_spot(&hip3, Some("xyz"), Some(3), Some(&collateral), 11)
                .unwrap();
        assert_eq!(builder[0].native_coin, "xyz:ABC");
        assert_eq!(builder[0].asset_id, 130000);
        assert_eq!(builder[0].symbol, "xyz:ABC/USDC:USDC");
        assert_eq!(
            builder[0].quote_asset.reference,
            "0x6d1e7cde53ba9467b783cb7c530ce054"
        );
        assert_eq!(builder[0].quote_asset.token_index, Some(0));
        assert_eq!(builder[0].quote_asset.decimals, 8);
        assert!(parse_perpetual_markets(&hip3, Some("xyz"), Some(3), 11).is_err());

        let mut contexts = vec![json!({}); 108];
        contexts[0] = json!({"coin":"not-the-selected-market","markPx":"999"});
        contexts[107] =
            json!({"coin":"HYPE/USDC","markPx":"42.5","dayNtlVlm":"12345678901234567890.123"});
        let spot = json!([
            {"tokens":[{"name":"USDC","index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","weiDecimals":8,"szDecimals":8},{"name":"HYPE","index":1,"tokenId":"0xbase","weiDecimals":8,"szDecimals":2}],"universe":[{"name":"HYPE/USDC","tokens":[1,0],"index":107}]},
            contexts
        ]);
        let pairs = parse_spot_markets(&spot, 12).unwrap();
        assert_eq!(pairs[0].symbol, "HYPE/USDC");
        assert_eq!(pairs[0].native_coin, "@107");
        assert_eq!(pairs[0].asset_id, 10107);
        assert_eq!(
            pairs[0].quote_asset.reference,
            "0x6d1e7cde53ba9467b783cb7c530ce054"
        );
        assert_eq!(pairs[0].quote_asset.token_index, Some(0));
        assert_eq!(pairs[0].quote_asset.decimals, 8);
        assert_eq!(pairs[0].base_asset.reference, "0xbase");
        assert_eq!(
            pairs[0]
                .context
                .as_ref()
                .unwrap()
                .mark_price
                .as_ref()
                .unwrap()
                .as_str(),
            "42.5"
        );
    }

    #[test]
    fn spot_metadata_uses_sparse_native_index_not_universe_position() {
        let mut contexts = vec![json!({}); 5];
        contexts[1] = json!({"coin":"@1","markPx":"777"});
        contexts[4] = json!({"coin":"@4","markPx":"4.5"});
        let raw = json!([
            {
                "tokens":[
                    {"name":"USDC","index":0,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054","weiDecimals":8,"szDecimals":8},
                    {"name":"A","index":1,"tokenId":"0xa","weiDecimals":8,"szDecimals":2},
                    {"name":"B","index":2,"tokenId":"0xb","weiDecimals":8,"szDecimals":2}
                ],
                "universe":[
                    {"name":"@4","tokens":[2,0],"index":4}
                ]
            },
            contexts
        ]);
        let parsed = parse_spot_markets(&raw, 1).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].asset_id, 10_004);
        assert_eq!(
            parsed[0]
                .context
                .as_ref()
                .unwrap()
                .mark_price
                .as_ref()
                .unwrap()
                .as_str(),
            "4.5"
        );

        let mut mismatched = raw.clone();
        mismatched[1][4]["coin"] = json!("@1");
        assert_eq!(
            parse_spot_markets(&mismatched, 1).unwrap_err(),
            "spot context does not match native market index"
        );
    }

    #[test]
    fn known_intervals_only() {
        let intent = ReadIntent {
            route: ReadRoute::Candles,
            account: None,
            symbol: Some("BTC".into()),
            order_id: None,
            start_ms: Some(0),
            end_ms: Some(1),
            interval: Some("1m".into()),
            limit: None,
        };
        assert!(info_request(&intent, None).is_ok());
        let bad = ReadIntent {
            interval: Some("2m".into()),
            ..intent
        };
        assert!(info_request(&bad, None).is_err());
        assert_eq!(
            json!(route_for("hyperliquid.time")),
            json!(Some(ReadRoute::ExchangeStatus))
        );
    }

    #[test]
    fn account_funding_payments_and_market_funding_rates_use_distinct_queries() {
        let account = ReadIntent {
            route: ReadRoute::FundingHistory,
            account: Some("main".into()),
            symbol: None,
            order_id: None,
            start_ms: Some(10),
            end_ms: Some(20),
            interval: None,
            limit: None,
        };
        assert!(matches!(
            info_request(&account, Some("0xowner")).unwrap(),
            InfoRequest::UserFunding { owner, start_time_ms: 10, end_time_ms: Some(20) } if owner == "0xowner"
        ));
        let market = ReadIntent {
            route: ReadRoute::FundingRateHistory,
            account: None,
            symbol: Some("ETH".into()),
            order_id: None,
            start_ms: Some(10),
            end_ms: Some(20),
            interval: None,
            limit: None,
        };
        assert!(matches!(
            info_request(&market, None).unwrap(),
            InfoRequest::FundingHistory { coin, start_time_ms: 10, end_time_ms: 20 } if coin == "ETH"
        ));
        assert_eq!(
            route_for("hyperliquid.funding_history"),
            Some(ReadRoute::FundingHistory)
        );
        assert_eq!(
            route_for("hyperliquid.funding_rate_history"),
            Some(ReadRoute::FundingRateHistory)
        );
    }
}
