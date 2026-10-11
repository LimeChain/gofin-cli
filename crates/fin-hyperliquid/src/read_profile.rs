use crate::{AccountMode, AccountModeState, AccountRole, CashWallet, profile::*, reads, types::*};
use fin_domain::{
    Amount, AuthorityState, FinError, FinancialState, PositionKind, PositionState, Quantity,
    ResourceAmount, StateSnapshot, now,
};
use fin_exchange::ccxt::HyperliquidInfoRequest as Info;
use serde_json::{Value, json};

pub(crate) fn input<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, FinError> {
    serde_json::from_value(value.clone()).map_err(|_| {
        FinError::validation("Input does not match the typed Hyperliquid operation schema.")
    })
}
pub(crate) fn native_error(message: &str) -> FinError {
    FinError::new(
        "HYPERLIQUID_EVIDENCE_INVALID",
        "reconciliation",
        message,
        "Keep the original submission unresolved and obtain exact venue evidence.",
    )
}

fn normalize_snapshot_quantity(value: &str, native_decimals: u8) -> Result<Quantity, FinError> {
    let native = Quantity::from_decimal(value, native_decimals)?;
    Quantity::from_decimal(&native.to_decimal(native_decimals), 36)
}

fn spot_snapshot_amount(
    token_id: &str,
    token_index: Option<u32>,
    native_decimals: u8,
    total: &str,
) -> Result<Amount, FinError> {
    Ok(Amount {
        asset: spot_token_asset(token_id, token_index, native_decimals),
        atoms: normalize_snapshot_quantity(total, native_decimals)?,
    })
}

fn derivative_snapshot_amount(market: &NativeMarket, size: &str) -> Result<Amount, FinError> {
    Ok(Amount {
        asset: asset(&market.base_asset.reference),
        atoms: normalize_snapshot_quantity(size, market.sz_decimals)?,
    })
}

fn filter_history_rows(
    raw: Value,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    limit: Option<u16>,
    timestamp: impl Fn(&Value) -> Option<u64>,
) -> Result<Value, FinError> {
    if since_ms
        .zip(until_ms)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(FinError::validation("History start must precede end."));
    }
    let rows = raw
        .as_array()
        .ok_or_else(|| native_error("History is not a native array."))?;
    let selected: Vec<_> = rows
        .iter()
        .filter(|row| {
            let Some(time) = timestamp(row) else {
                return false;
            };
            since_ms.is_none_or(|start| time >= start) && until_ms.is_none_or(|end| time <= end)
        })
        .take(usize::from(limit.unwrap_or(1000)))
        .cloned()
        .collect();
    Ok(
        json!({"rows":selected,"coverage":"native result filtered by the requested millisecond range; endpoint pagination limits still apply"}),
    )
}

fn is_account_liquidation_fill(row: &Value, owner: &str) -> bool {
    row.get("liquidation")
        .and_then(Value::as_object)
        .and_then(|liquidation| liquidation.get("liquidatedUser"))
        .and_then(Value::as_str)
        .is_some_and(|address| address.eq_ignore_ascii_case(owner))
}

fn exchange_time(status: Value) -> Result<Value, FinError> {
    let time = status
        .get("time")
        .and_then(Value::as_u64)
        .ok_or_else(|| native_error("Exchange-status response has no integer server time."))?;
    Ok(json!({"time":time,"unit":"milliseconds since Unix epoch"}))
}

fn zero_decimal(value: &str) -> bool {
    value
        .strip_prefix('-')
        .unwrap_or(value)
        .bytes()
        .all(|byte| byte == b'0' || byte == b'.')
}

fn missing_role_has_zero_perpetual_state(raw: &Value) -> Result<bool, FinError> {
    let Some(withdrawable) = raw.get("withdrawable").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(summary) = raw.get("marginSummary") else {
        return Ok(false);
    };
    let Some(total_raw_usd) = summary.get("totalRawUsd").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(total_margin_used) = summary.get("totalMarginUsed").and_then(Value::as_str) else {
        return Ok(false);
    };
    let Some(positions) = raw.get("assetPositions").and_then(Value::as_array) else {
        return Ok(false);
    };
    Ok(zero_decimal(withdrawable)
        && zero_decimal(total_raw_usd)
        && zero_decimal(total_margin_used)
        && positions.iter().all(|row| {
            row.get("position")
                .and_then(|position| position.get("szi"))
                .and_then(Value::as_str)
                .is_some_and(zero_decimal)
        }))
}

fn market_coin_matches(market: &NativeMarket, coin: Option<&str>) -> bool {
    coin.is_some_and(|coin| coin == market.native_coin || coin == market.symbol)
}

fn validate_order_market(raw: &Value, expected_coin: Option<&str>) -> Result<(), FinError> {
    let Some(expected_coin) = expected_coin else {
        return Ok(());
    };
    // Native `orderStatus` wraps the actual order under `order.order`; the
    // outer `order` is the response envelope, not the order object.
    let coin = raw.pointer("/order/order/coin").and_then(Value::as_str);
    if raw.get("status").and_then(Value::as_str) != Some("order") || coin != Some(expected_coin) {
        return Err(FinError::validation(
            "Order ID does not resolve to the requested market.",
        ));
    }
    Ok(())
}

impl HyperliquidProfile {
    async fn validated_hip3_dex(&self, dex: Option<&str>) -> Result<Option<String>, FinError> {
        let Some(dex) = dex else {
            return Ok(None);
        };
        if dex.trim().is_empty() || dex.trim() != dex {
            return Err(FinError::validation(
                "DEX must be an exact nonempty name returned by hyperliquid.markets.",
            ));
        }
        let rows = self.transport.info(Info::PerpDexs).await?;
        let rows = rows
            .as_array()
            .ok_or_else(|| native_error("Invalid DEX metadata."))?;
        if !rows
            .iter()
            .any(|row| row.get("name").and_then(Value::as_str) == Some(dex))
        {
            return Err(FinError::validation(
                "HIP-3 DEX was not found in authoritative metadata.",
            ));
        }
        Ok(Some(dex.to_owned()))
    }

    pub async fn markets(
        &self,
        kind: Option<MarketKind>,
        dex: Option<&str>,
    ) -> Result<Vec<NativeMarket>, FinError> {
        let observed = chrono::Utc::now().timestamp_millis() as u64;
        let mut result = Vec::new();
        if dex.is_some() && matches!(kind, Some(MarketKind::Spot)) {
            return Err(FinError::validation(
                "A DEX filter cannot be combined with spot markets.",
            ));
        }
        if dex.is_some() && matches!(kind, Some(MarketKind::Perpetual)) {
            return Err(FinError::validation(
                "A HIP-3 DEX filter cannot be combined with primary perpetual markets.",
            ));
        }
        if dex.is_none() && matches!(kind, None | Some(MarketKind::Perpetual)) {
            let raw = self.transport.info(Info::MetaAndAssetCtxs).await?;
            result.extend(
                reads::parse_perpetual_markets(&raw, None, None, observed).map_err(native_error)?,
            );
        }
        if !matches!(kind, Some(MarketKind::Perpetual | MarketKind::Spot)) {
            let dexes = self.transport.info(Info::PerpDexs).await?;
            let dexes = dexes
                .as_array()
                .ok_or_else(|| native_error("Invalid DEX metadata."))?;
            let selected: Vec<(usize, String)> = if let Some(dex) = dex {
                vec![(
                    dexes
                        .iter()
                        .position(|row| row["name"].as_str() == Some(dex))
                        .ok_or_else(|| {
                            FinError::validation(
                                "HIP-3 DEX was not found in authoritative metadata.",
                            )
                        })?,
                    dex.to_owned(),
                )]
            } else {
                dexes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, row)| {
                        row["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(|s| (i, s.to_owned()))
                    })
                    .collect()
            };
            let spot = self.transport.info(Info::SpotMetaAndAssetCtxs).await?;
            for (index, name) in selected {
                let raw = self
                    .transport
                    .info(Info::MetaAndAssetCtxsForDex { dex: name.clone() })
                    .await?;
                result.extend(
                    reads::parse_perpetual_markets_with_spot(
                        &raw,
                        Some(&name),
                        Some(
                            u32::try_from(index)
                                .map_err(|_| native_error("DEX index exceeds supported range."))?,
                        ),
                        Some(&spot),
                        observed,
                    )
                    .map_err(native_error)?,
                );
            }
        }
        if dex.is_none() && matches!(kind, None | Some(MarketKind::Spot)) {
            let raw = self.transport.info(Info::SpotMetaAndAssetCtxs).await?;
            result.extend(reads::parse_spot_markets(&raw, observed).map_err(native_error)?);
        }
        Ok(result)
    }
    pub async fn market(
        &self,
        symbol: &str,
        kind: Option<MarketKind>,
        dex: Option<&str>,
    ) -> Result<NativeMarket, FinError> {
        let base = symbol.split_once('/').map_or(symbol, |(base, _)| base);
        let symbol_dex = base.split_once(':').map(|(dex, _)| dex);
        let inferred_dex = dex.or(symbol_dex);
        if let (Some(requested), Some(_)) = (&kind, inferred_dex)
            && !matches!(requested, MarketKind::Hip3Perpetual)
        {
            return Err(FinError::validation(
                "A HIP-3-prefixed market must be read with hip3_perpetual kind.",
            ));
        }
        let selected_kind = match kind {
            Some(kind) => kind,
            None if inferred_dex.is_some() => MarketKind::Hip3Perpetual,
            None if symbol.starts_with('@') => MarketKind::Spot,
            None if symbol.contains('/') && !symbol.ends_with(":USDC") => MarketKind::Spot,
            None => MarketKind::Perpetual,
        };
        if matches!(selected_kind, MarketKind::Hip3Perpetual) && inferred_dex.is_none() {
            return Err(FinError::validation(
                "A HIP-3 market read requires its exact DEX-prefixed symbol or DEX name.",
            ));
        }
        if matches!(selected_kind, MarketKind::Spot) && inferred_dex.is_some() {
            return Err(FinError::validation(
                "A DEX-prefixed market cannot be read as spot.",
            ));
        }
        let selected_dex = matches!(selected_kind, MarketKind::Hip3Perpetual)
            .then_some(inferred_dex)
            .flatten();
        let rows = self
            .markets(Some(selected_kind.clone()), selected_dex)
            .await?;
        let mut matching = rows
            .into_iter()
            .filter(|m| m.symbol == symbol || m.native_coin == symbol);
        let market = matching.next().ok_or_else(|| {
            FinError::validation(
                "Select the exact unified or native market identifier from hyperliquid.markets.",
            )
        })?;
        if matching.next().is_some() || market.is_delisted {
            return Err(FinError::validation(
                "Market identity is ambiguous or delisted.",
            ));
        }
        Ok(market)
    }
    pub async fn read_operation(&self, operation: &str, value: &Value) -> Result<Value, FinError> {
        let data = match operation {
            "hyperliquid.markets" => {
                let request: MarketsInput = input(value)?;
                json!(self.markets(request.kind, request.dex.as_deref()).await?)
            }
            "hyperliquid.currencies" => {
                self.transport.info(Info::SpotMetaAndAssetCtxs).await?[0]["tokens"].clone()
            }
            "hyperliquid.ticker" | "hyperliquid.funding_rate" | "hyperliquid.open_interest" => {
                let request: MarketInput = input(value)?;
                let market = self.market(&request.symbol, None, None).await?;
                if matches!(
                    operation,
                    "hyperliquid.funding_rate" | "hyperliquid.open_interest"
                ) && market.kind == MarketKind::Spot
                {
                    return Err(FinError::validation(
                        "Funding rates and open interest are available only for perpetual markets.",
                    ));
                }
                json!({"market":market,"context":market.context})
            }
            "hyperliquid.tickers" | "hyperliquid.funding_rates" => {
                let request: MarketListInput = input(value)?;
                let markets = self.markets(None, None).await?;
                let selected: Vec<_> = markets
                    .into_iter()
                    .filter(|m| {
                        (!matches!(operation, "hyperliquid.funding_rates")
                            || matches!(m.kind, MarketKind::Perpetual | MarketKind::Hip3Perpetual))
                            && request
                                .symbols
                                .as_ref()
                                .is_none_or(|s| s.contains(&m.symbol) || s.contains(&m.native_coin))
                    })
                    .collect();
                if request
                    .symbols
                    .as_ref()
                    .is_some_and(|s| s.len() != selected.len())
                {
                    return Err(FinError::validation(
                        "Every requested market must match one metadata identity.",
                    ));
                }
                json!(
                    selected
                        .into_iter()
                        .map(|market| json!({"market":market,"context":market.context}))
                        .collect::<Vec<_>>()
                )
            }
            "hyperliquid.order_book" => {
                let request: MarketInput = input(value)?;
                let market = self.market(&request.symbol, None, None).await?;
                let raw = self
                    .transport
                    .info(Info::L2Book {
                        coin: market.native_coin.clone(),
                    })
                    .await?;
                if raw["coin"] != market.native_coin {
                    return Err(native_error("Order book returned a different market."));
                }
                raw
            }
            "hyperliquid.candles" => {
                let request: CandleInput = input(value)?;
                if request.end_ms.is_some_and(|end| end < request.start_ms) {
                    return Err(FinError::validation("Candle start must precede end."));
                }
                let market = self.market(&request.symbol, None, None).await?;
                let raw = self
                    .transport
                    .info(Info::CandleSnapshot {
                        coin: market.native_coin,
                        interval: request.interval,
                        start_time_ms: request.start_ms,
                        end_time_ms: request
                            .end_ms
                            .unwrap_or(chrono::Utc::now().timestamp_millis() as u64),
                    })
                    .await?;
                filter_history_rows(
                    raw,
                    Some(request.start_ms),
                    request.end_ms,
                    request.limit,
                    |row| {
                        row.get("t")
                            .and_then(Value::as_u64)
                            .or_else(|| row.get("time").and_then(Value::as_u64))
                    },
                )?
            }
            "hyperliquid.trades" | "hyperliquid.funding_rate_history" => {
                let request: MarketHistoryInput = input(value)?;
                let market = self.market(&request.symbol, None, None).await?;
                let (raw, time_key) = if operation == "hyperliquid.trades" {
                    (
                        self.transport
                            .info(Info::RecentTrades {
                                coin: market.native_coin,
                            })
                            .await?,
                        "time",
                    )
                } else {
                    (
                        self.transport
                            .info(Info::FundingHistory {
                                coin: market.native_coin,
                                start_time_ms: request.since_ms.unwrap_or(0),
                                end_time_ms: request
                                    .until_ms
                                    .unwrap_or(chrono::Utc::now().timestamp_millis() as u64),
                            })
                            .await?,
                        "time",
                    )
                };
                filter_history_rows(
                    raw,
                    request.since_ms,
                    request.until_ms,
                    request.limit,
                    |row| row.get(time_key).and_then(Value::as_u64),
                )?
            }
            "hyperliquid.funding_history" => {
                let request: HistoryInput = input(value)?;
                let account = self.account(&request.account)?;
                let start = request.since_ms.unwrap_or(0);
                let end = request
                    .until_ms
                    .unwrap_or(chrono::Utc::now().timestamp_millis() as u64);
                if start > end {
                    return Err(FinError::validation("History start must precede end."));
                }
                let raw = self
                    .transport
                    .info(Info::UserFunding {
                        owner: account.execution_address().to_owned(),
                        start_time_ms: start,
                        end_time_ms: Some(end),
                    })
                    .await?;
                let native_coin = match request.symbol.as_deref() {
                    Some(symbol) => {
                        let market = self.market(symbol, None, None).await?;
                        if market.kind == MarketKind::Spot {
                            return Err(FinError::validation(
                                "Funding payment history is available only for perpetual markets.",
                            ));
                        }
                        Some(market)
                    }
                    None => None,
                };
                let rows = raw
                    .as_array()
                    .ok_or_else(|| native_error("Funding history is not a native array."))?;
                let selected: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        let time = row.get("time").and_then(Value::as_u64);
                        let coin = row
                            .get("delta")
                            .and_then(|d| d.get("coin"))
                            .and_then(Value::as_str)
                            .or_else(|| row.get("coin").and_then(Value::as_str));
                        time.is_some_and(|t| t >= start && t <= end)
                            && native_coin
                                .as_ref()
                                .is_none_or(|expected| market_coin_matches(expected, coin))
                    })
                    .take(usize::from(request.limit.unwrap_or(1000)))
                    .cloned()
                    .collect();
                json!({"rows":selected,"coverage":"userFunding result filtered by time and optional native coin; venue result limits apply"})
            }
            "hyperliquid.time" => exchange_time(self.transport.info(Info::ExchangeStatus).await?)?,
            "hyperliquid.status" => self.transport.info(Info::ExchangeStatus).await?,
            "hyperliquid.trading_state" => {
                let request: TradingStateInput = input(value)?;
                let account = self.account(&request.account)?;
                let dex = self.validated_hip3_dex(request.dex.as_deref()).await?;
                let kind = if dex.is_some() {
                    MarketKind::Hip3Perpetual
                } else {
                    MarketKind::Perpetual
                };
                let market = self
                    .market(&request.symbol, Some(kind), dex.as_deref())
                    .await?;
                let raw = self
                    .transport
                    .info(Info::ActiveAssetData {
                        owner: account.execution_address().to_owned(),
                        coin: market.native_coin.clone(),
                    })
                    .await?;
                crate::writes::leverage_state(
                    &raw,
                    account.execution_address(),
                    &market.native_coin,
                )?;
                json!({"market":market.symbol,"dex":market.dex,"state":raw})
            }
            "hyperliquid.balances" => {
                let request: AccountMarketInput = input(value)?;
                let account = self.account(&request.account)?;
                let dex = self.validated_hip3_dex(request.dex.as_deref()).await?;
                let perp_request = match dex.as_deref() {
                    Some(dex) => Info::ClearinghouseStateForDex {
                        owner: account.execution_address().to_owned(),
                        dex: dex.to_owned(),
                    },
                    None => Info::ClearinghouseState {
                        owner: account.execution_address().to_owned(),
                    },
                };
                let perp = self.transport.info(perp_request).await?;
                let spot = self
                    .transport
                    .info(Info::SpotClearinghouseState {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                json!({"owner":account.execution_address(),"signer":account.owner,"dex":dex,"perpetual":perp,"spot":spot,"coverage":"selected perpetual DEX collateral and account-wide native spot balances; native margin and unrealized PnL retain venue semantics"})
            }
            "hyperliquid.positions" => {
                let request: AccountMarketInput = input(value)?;
                let account = self.account(&request.account)?;
                let dex = self.validated_hip3_dex(request.dex.as_deref()).await?;
                match dex {
                    Some(dex) => {
                        self.transport
                            .info(Info::ClearinghouseStateForDex {
                                owner: account.execution_address().to_owned(),
                                dex,
                            })
                            .await?
                    }
                    None => {
                        self.transport
                            .info(Info::ClearinghouseState {
                                owner: account.execution_address().to_owned(),
                            })
                            .await?
                    }
                }
            }
            "hyperliquid.order" => {
                let request: OrderInput = input(value)?;
                let account = self.account(&request.account)?;
                let expected_coin = match request.symbol.as_deref() {
                    Some(symbol) => Some(self.market(symbol, None, None).await?.native_coin),
                    None => None,
                };
                let raw = match (request.order_id, request.client_order_id) {
                    (Some(id), None) => {
                        self.transport
                            .info(Info::OrderStatus {
                                owner: account.execution_address().to_owned(),
                                order_id: id.parse().map_err(|_| {
                                    FinError::validation(
                                        "Order ID must be an unsigned native integer.",
                                    )
                                })?,
                            })
                            .await?
                    }
                    (None, Some(id)) => {
                        self.transport
                            .info(Info::OrderStatusByClientId {
                                owner: account.execution_address().to_owned(),
                                client_order_id: id,
                            })
                            .await?
                    }
                    _ => {
                        return Err(FinError::validation(
                            "Select exactly one native order ID or client order ID.",
                        ));
                    }
                };
                validate_order_market(&raw, expected_coin.as_deref())?;
                raw
            }
            "hyperliquid.open_orders" => {
                let request: AccountMarketInput = input(value)?;
                let account = self.account(&request.account)?;
                let dex = self.validated_hip3_dex(request.dex.as_deref()).await?;
                match dex {
                    Some(dex) => {
                        self.transport
                            .info(Info::FrontendOpenOrdersForDex {
                                owner: account.execution_address().to_owned(),
                                dex,
                            })
                            .await?
                    }
                    None => {
                        self.transport
                            .info(Info::FrontendOpenOrders {
                                owner: account.execution_address().to_owned(),
                            })
                            .await?
                    }
                }
            }
            "hyperliquid.fees" | "hyperliquid.abstraction" => {
                let request: AccountInput = input(value)?;
                let account = self.account(&request.account)?;
                match operation {
                    "hyperliquid.fees" => {
                        self.transport
                            .info(Info::UserFees {
                                owner: account.execution_address().to_owned(),
                            })
                            .await?
                    }
                    _ => {
                        json!({"base":self.transport.info(Info::UserAbstractionState { owner:account.execution_address().to_owned() }).await?,
                        "dex":self.transport.info(Info::UserDexAbstractionState { owner:account.execution_address().to_owned() }).await?})
                    }
                }
            }
            "hyperliquid.orders"
            | "hyperliquid.closed_orders"
            | "hyperliquid.canceled_orders"
            | "hyperliquid.my_trades"
            | "hyperliquid.my_liquidations"
            | "hyperliquid.ledger"
            | "hyperliquid.deposits"
            | "hyperliquid.withdrawals" => {
                let request: HistoryInput = input(value)?;
                let account = self.account(&request.account)?;
                let start = request.since_ms.unwrap_or(0);
                let end = request
                    .until_ms
                    .unwrap_or(chrono::Utc::now().timestamp_millis() as u64);
                if start > end {
                    return Err(FinError::validation("History start must precede end."));
                }
                let requested_market = match request.symbol.as_deref() {
                    Some(symbol) => {
                        if matches!(
                            operation,
                            "hyperliquid.ledger"
                                | "hyperliquid.deposits"
                                | "hyperliquid.withdrawals"
                        ) {
                            return Err(FinError::validation(
                                "This ledger view has no market-symbol filter; omit symbol.",
                            ));
                        }
                        let market = self.market(symbol, None, None).await?;
                        if operation == "hyperliquid.my_liquidations"
                            && market.kind == MarketKind::Spot
                        {
                            return Err(FinError::validation(
                                "Liquidation history is available only for perpetual markets.",
                            ));
                        }
                        Some(market)
                    }
                    None => None,
                };
                let query = match operation {
                    "hyperliquid.orders"
                    | "hyperliquid.closed_orders"
                    | "hyperliquid.canceled_orders" => Info::HistoricalOrders {
                        owner: account.execution_address().to_owned(),
                    },
                    "hyperliquid.my_trades" => Info::UserFillsByTime {
                        owner: account.execution_address().to_owned(),
                        start_time_ms: start,
                        end_time_ms: end,
                    },
                    "hyperliquid.my_liquidations"
                        if request.since_ms.is_some() || request.until_ms.is_some() =>
                    {
                        Info::UserFillsByTime {
                            owner: account.execution_address().to_owned(),
                            start_time_ms: start,
                            end_time_ms: end,
                        }
                    }
                    "hyperliquid.my_liquidations" => Info::UserFills {
                        owner: account.execution_address().to_owned(),
                    },
                    _ => Info::UserNonFundingLedgerUpdates {
                        owner: account.execution_address().to_owned(),
                        start_time_ms: start,
                        end_time_ms: Some(end),
                    },
                };
                let raw = self.transport.info(query).await?;
                let rows = raw
                    .as_array()
                    .ok_or_else(|| native_error("History is not a bounded array."))?;
                let selected: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        let order = row.get("order").unwrap_or(row);
                        let status = row
                            .get("status")
                            .and_then(Value::as_str)
                            .or_else(|| order.get("status").and_then(Value::as_str))
                            .unwrap_or("");
                        let time = row
                            .get("statusTimestamp")
                            .and_then(Value::as_u64)
                            .or_else(|| row.get("time").and_then(Value::as_u64));
                        let in_time = time.is_some_and(|t| t >= start && t <= end);
                        let coin = order
                            .get("coin")
                            .and_then(Value::as_str)
                            .or_else(|| {
                                row.get("delta")
                                    .and_then(|delta| delta.get("coin"))
                                    .and_then(Value::as_str)
                            })
                            .or_else(|| row.get("coin").and_then(Value::as_str));
                        let in_market = requested_market
                            .as_ref()
                            .is_none_or(|m| market_coin_matches(m, coin));
                        let liquidation_match =
                            is_account_liquidation_fill(row, account.execution_address());
                        in_time
                            && in_market
                            && match operation {
                                "hyperliquid.closed_orders" => status == "filled",
                                "hyperliquid.canceled_orders" => {
                                    status == "canceled" || status.ends_with("Canceled")
                                }
                                "hyperliquid.deposits" => row["delta"]["type"] == "deposit",
                                "hyperliquid.withdrawals" => row["delta"]["type"] == "withdraw",
                                "hyperliquid.my_liquidations" => liquidation_match,
                                _ => true,
                            }
                    })
                    .take(usize::from(request.limit.unwrap_or(1000)))
                    .cloned()
                    .collect();
                json!({"rows":selected,"coverage":"bounded venue history; not an exhaustive account ledger"})
            }
            _ => {
                return Err(FinError::validation(
                    "Unsupported Hyperliquid read operation.",
                ));
            }
        };
        Ok(
            json!({"data":reads::normalize_native_response(data).map_err(native_error)?,"source":"Hyperliquid REST /info","observed_at":now()}),
        )
    }

    pub async fn financial_snapshot(&self) -> Result<StateSnapshot, FinError> {
        let mut state = FinancialState::default();
        let mut coverage = Vec::new();
        let perp_markets = self.markets(Some(MarketKind::Perpetual), None).await?;
        let spot_meta = self.transport.info(Info::SpotMetaAndAssetCtxs).await?;
        let tokens = spot_meta
            .get(0)
            .and_then(|m| m.get("tokens"))
            .and_then(Value::as_array)
            .ok_or_else(|| native_error("Spot token metadata is missing."))?;
        for account in &self.accounts {
            let observed_mode = self.recipient_account_mode(account).await?;
            let account_mode = observed_mode.mode;
            account_mode.ensure_supported_cash_accounting()?;
            if observed_mode.role == AccountRole::Missing
                && (account_mode.mode != AccountMode::Separate
                    || !matches!(
                        account_mode.native_abstraction.as_str(),
                        "default" | "disabled"
                    ))
            {
                return Err(native_error(
                    "A missing Hyperliquid user role is accepted only with explicit default or disabled abstraction.",
                ));
            }
            let raw = self
                .transport
                .info(Info::ClearinghouseState {
                    owner: account.execution_address().to_owned(),
                })
                .await?;
            let perpetual_cash =
                account_mode.cash_resource(account, CashWallet::Perpetual, None)?;
            if account_mode.mode == AccountMode::Separate {
                let free = raw["withdrawable"]
                    .as_str()
                    .ok_or_else(|| native_error("Withdrawable collateral is missing."))?;
                state.holdings.push(ResourceAmount {
                    resource: perpetual_cash.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: Quantity::from_decimal(free, 36)?,
                    },
                });
                if raw
                    .get("marginSummary")
                    .and_then(Value::as_object)
                    .is_none()
                {
                    return Err(native_error("Primary perpetual margin summary is missing."));
                }
                for (name, field) in [
                    ("perpetual_account_value", "totalRawUsd"),
                    ("perpetual_margin_used", "totalMarginUsed"),
                ] {
                    let value = raw["marginSummary"]
                        .get(field)
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            native_error("Primary perpetual margin facts are incomplete.")
                        })?;
                    state.authorities.push(AuthorityState {
                        resource: perpetual_cash.clone(),
                        authority: format!("hyperliquid:native:{name}"),
                        value: value.to_owned(),
                    });
                }
                state.authorities.push(AuthorityState {
                    resource: perpetual_cash,
                    authority: "hyperliquid:native:perpetual_withdrawable".into(),
                    value: free.to_owned(),
                });
            }
            state.authorities.push(AuthorityState {
                resource: account.resource("account", None),
                authority: "hyperliquid:native:account_abstraction".into(),
                value: account_mode.native_abstraction.clone(),
            });
            state.authorities.push(AuthorityState {
                resource: account.resource("account", None),
                authority: "hyperliquid:native:dex_abstraction".into(),
                value: account_mode
                    .dex_abstraction
                    .map(|enabled| enabled.to_string())
                    .unwrap_or_else(|| "null".into()),
            });
            let positions = raw
                .get("assetPositions")
                .and_then(Value::as_array)
                .ok_or_else(|| native_error("Primary perpetual positions are missing."))?;
            for row in positions {
                let Some(position) = row.get("position") else {
                    continue;
                };
                let coin = position
                    .get("coin")
                    .and_then(Value::as_str)
                    .ok_or_else(|| native_error("Perpetual position coin is missing."))?;
                let signed = position
                    .get("szi")
                    .and_then(Value::as_str)
                    .ok_or_else(|| native_error("Perpetual position size is missing."))?;
                if zero_decimal(signed) {
                    continue;
                }
                let market = perp_markets
                    .iter()
                    .find(|m| m.native_coin == coin)
                    .ok_or_else(|| {
                        native_error("Perpetual position market metadata is unavailable.")
                    })?;
                let negative = signed.starts_with('-');
                let magnitude = signed.strip_prefix('-').unwrap_or(signed);
                let kind = if negative {
                    PositionKind::DerivativeShort
                } else {
                    PositionKind::DerivativeLong
                };
                let position_name = format!(
                    "hyperliquid:perpetual:{coin}:{}",
                    if negative { "short" } else { "long" }
                );
                let amount = derivative_snapshot_amount(market, magnitude)?;
                state.positions.push(PositionState {
                    resource: AccountModeState::derivative_resource(account, None),
                    position: position_name,
                    kind,
                    amount,
                    scaled_receipt: None,
                    anchor: None,
                });
                for (label, field) in [
                    ("entry_price", "entryPx"),
                    ("unrealized_pnl", "unrealizedPnl"),
                    ("liquidation_price", "liquidationPx"),
                    ("margin_used", "marginUsed"),
                ] {
                    if let Some(value) = position.get(field).and_then(Value::as_str) {
                        state.authorities.push(AuthorityState {
                            resource: AccountModeState::derivative_resource(account, None),
                            authority: format!("hyperliquid:native:position:{coin}:{label}"),
                            value: value.to_owned(),
                        });
                    }
                }
            }
            // accountValue includes unrealized PnL and must never be substituted
            // for a cash or usable-credit observation.
            let spot = self
                .transport
                .info(Info::SpotClearinghouseState {
                    owner: account.execution_address().to_owned(),
                })
                .await?;
            let orders = self
                .transport
                .info(Info::FrontendOpenOrders {
                    owner: account.execution_address().to_owned(),
                })
                .await?;
            let orders = orders
                .as_array()
                .ok_or_else(|| native_error("Open orders are not a native array."))?;
            if observed_mode.role == AccountRole::Missing {
                let balances = spot
                    .get("balances")
                    .and_then(Value::as_array)
                    .ok_or_else(|| native_error("Spot balances are missing."))?;
                let zero_spot = balances.iter().all(|balance| {
                    balance
                        .get("total")
                        .and_then(Value::as_str)
                        .is_some_and(zero_decimal)
                        && balance
                            .get("hold")
                            .and_then(Value::as_str)
                            .is_some_and(zero_decimal)
                });
                if !missing_role_has_zero_perpetual_state(&raw)? || !zero_spot || !orders.is_empty()
                {
                    return Err(native_error(
                        "A Hyperliquid address without a user role must have verified zero cash, positions and open orders before it can be used as a first-deposit recipient.",
                    ));
                }
            }
            state.authorities.push(AuthorityState {
                resource: account.resource("account", None),
                authority: "hyperliquid:native:account_role".into(),
                value: match observed_mode.role {
                    AccountRole::MasterUser => "master_user",
                    AccountRole::Missing => "missing_recipient_only",
                }
                .into(),
            });
            for order in orders {
                let id = order
                    .get("oid")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| native_error("Open order identifier is missing."))?;
                state.in_flight.push(format!(
                    "hyperliquid:mainnet:order:{}:{id}",
                    account.execution_address().to_lowercase()
                ));
            }
            for balance in spot["balances"]
                .as_array()
                .ok_or_else(|| native_error("Spot balances are missing."))?
            {
                let coin = balance["coin"]
                    .as_str()
                    .ok_or_else(|| native_error("Spot token identity is missing."))?;
                let token_matches: Vec<_> = tokens
                    .iter()
                    .filter(|token| token.get("name").and_then(Value::as_str) == Some(coin))
                    .collect();
                if token_matches.len() != 1 {
                    return Err(native_error(
                        "Spot balance token name is missing or ambiguous in native token metadata.",
                    ));
                }
                let token = token_matches[0];
                let token_id = token
                    .get("tokenId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| native_error("Spot token ID is missing."))?;
                let decimals = token
                    .get("weiDecimals")
                    .and_then(Value::as_u64)
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| native_error("Spot token decimals are missing."))?;
                let token_index = token
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok());
                let total_text = balance["total"]
                    .as_str()
                    .ok_or_else(|| native_error("Spot total is missing."))?;
                let amount = spot_snapshot_amount(token_id, token_index, decimals, total_text)?;
                let _hold = Quantity::from_decimal(
                    balance["hold"]
                        .as_str()
                        .ok_or_else(|| native_error("Spot hold is missing."))?,
                    decimals,
                )?;
                let spot_cash = account_mode.cash_resource(account, CashWallet::Spot, None)?;
                state.holdings.push(ResourceAmount {
                    resource: spot_cash.clone(),
                    amount,
                });
                state.authorities.push(AuthorityState {
                    resource: spot_cash,
                    authority: format!("hyperliquid:native:spot_hold:{}", token_id),
                    value: balance["hold"]
                        .as_str()
                        .ok_or_else(|| native_error("Spot hold is missing."))?
                        .to_owned(),
                });
            }
            let mode_coverage = match account_mode.mode {
                AccountMode::Separate => "standard separate perps/spot cash",
                AccountMode::Unified => "unified spot cash with separate derivative exposure",
                AccountMode::PortfolioMargin | AccountMode::DexAbstraction => {
                    unreachable!("unsupported modes are rejected above")
                }
            };
            coverage.push(format!("{}: complete for the declared primary-perpetual and spot scope; {mode_coverage}; includes native open orders, positions, total token balances, supported margin facts and token holds; excludes HIP-3 DEX accounts and separate subaccounts", account.name));
        }
        Ok(StateSnapshot {
            state,
            observed_at: now(),
            complete: true,
            coverage,
            pending: Vec::new(),
        })
    }
}

#[cfg(test)]
mod read_profile_tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn snapshots_share_scale_36_assets_with_order_effects() {
        let spot = spot_snapshot_amount(NATIVE_USDC_TOKEN_ID, Some(0), 8, "1.25").unwrap();
        assert_eq!(spot.asset, usdc());
        assert_eq!(spot.asset, asset(NATIVE_USDC_TOKEN_ID));
        assert_eq!(spot.atoms, Quantity::from_decimal("1.25", 36).unwrap());

        // Native derivative size precision is retained as a validation rule,
        // while the financial identity and atoms match order effects.
        let market = NativeMarket {
            symbol: "BTC/USDC:USDC".into(),
            native_coin: "BTC".into(),
            base_asset: AssetReference {
                reference: "BTC".into(),
                symbol: "BTC".into(),
                decimals: 3,
                token_index: None,
            },
            quote_asset: AssetReference {
                reference: "USDC".into(),
                symbol: "USDC".into(),
                decimals: 6,
                token_index: None,
            },
            kind: MarketKind::Perpetual,
            observed_at_ms: 0,
            dex: None,
            universe_index: 0,
            dex_index: None,
            asset_id: 0,
            sz_decimals: 3,
            max_leverage: None,
            is_delisted: false,
            context: None,
            spot_tokens: None,
        };
        let position = derivative_snapshot_amount(&market, "0.001").unwrap();
        assert_eq!(position.asset, asset(&market.base_asset.reference));
        assert_eq!(position.asset.decimals, 36);
        assert_eq!(position.atoms, Quantity::from_decimal("0.001", 36).unwrap());
        assert!(derivative_snapshot_amount(&market, "0.0001").is_err());
    }

    #[test]
    fn history_filters_require_timestamp_and_apply_inclusive_bounds_and_limit() {
        let rows = json!([
            {"time": 9, "px":"1.0"},
            {"time": 10, "px":"2.0"},
            {"time": 12, "px":"3.0"},
            {"px":"4.0"}
        ]);
        let result = filter_history_rows(rows, Some(10), Some(12), Some(1), |row| {
            row.get("time").and_then(Value::as_u64)
        })
        .unwrap();
        assert_eq!(result["rows"], json!([{"time":10,"px":"2.0"}]));
    }

    #[test]
    fn optional_order_symbol_checks_native_nested_order_and_fails_closed() {
        let native_order = json!({
            "status":"order",
            "order":{
                "order":{"coin":"@107","oid":570963464755_i64,"cloid":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                "status":"filled",
                "statusTimestamp":1791696242000_i64
            }
        });
        assert!(validate_order_market(&native_order, Some("@107")).is_ok());

        let unrelated = json!({
            "status":"order",
            "order":{
                "order":{"coin":"BTC","oid":570963464755_i64},
                "status":"filled"
            }
        });
        assert_eq!(
            validate_order_market(&unrelated, Some("@107"))
                .unwrap_err()
                .code,
            "INVALID_INPUT"
        );

        // A matching-looking envelope field must not substitute for the
        // native nested order identity.
        let malformed = json!({
            "status":"order",
            "order":{
                "coin":"@107",
                "order":{"coin":"BTC"}
            }
        });
        assert!(validate_order_market(&malformed, Some("@107")).is_err());
        assert!(
            validate_order_market(
                &json!({"status":"order","order":{"order":{}}}),
                Some("@107")
            )
            .is_err()
        );
        assert!(
            validate_order_market(
                &json!({"status":"order","order":{"order":{"coin":107}}}),
                Some("@107")
            )
            .is_err()
        );
        assert!(validate_order_market(&json!({"status":"unknownOid"}), Some("@107")).is_err());
    }

    #[test]
    fn liquidation_filter_requires_exact_native_liquidated_user_field() {
        let owner = "0xAbCd000000000000000000000000000000000001";
        assert!(is_account_liquidation_fill(
            &json!({"liquidation":{"liquidatedUser":"0xabcd000000000000000000000000000000000001"}}),
            owner
        ));
        assert!(!is_account_liquidation_fill(
            &json!({"liquidation":{"liquidatedUser":"0x0000000000000000000000000000000000000001"}}),
            owner
        ));
        assert!(!is_account_liquidation_fill(
            &json!({"dir":"Liquidation"}),
            owner
        ));
    }

    #[test]
    fn server_time_comes_from_exchange_status_native_time_field() {
        assert_eq!(
            exchange_time(json!({"status":"ok","time":1234})).unwrap(),
            json!({"time":1234,"unit":"milliseconds since Unix epoch"})
        );
        assert!(exchange_time(json!({"serverTime":1234})).is_err());
    }

    #[test]
    fn zero_decimal_handles_signed_fractional_zero() {
        assert!(zero_decimal("-0.000"));
        assert!(zero_decimal("0"));
        assert!(!zero_decimal("-0.001"));
    }

    struct RequestCapture(Mutex<Vec<Info>>);

    #[async_trait::async_trait]
    impl HyperliquidTransport for RequestCapture {
        async fn info(&self, request: Info) -> Result<Value, FinError> {
            self.0.lock().unwrap().push(request.clone());
            Ok(match request {
                Info::PerpDexs => json!([{"name":"xyz"}]),
                Info::MetaAndAssetCtxs => {
                    json!([{"universe":[{"name":"ETH","szDecimals":4,"maxLeverage":20}]},[{"markPx":"10"}]])
                }
                Info::L2Book { coin } => json!({"coin":coin,"levels":[[],[]]}),
                Info::ActiveAssetData { owner, coin } => {
                    json!({"user":owner,"coin":coin,"leverage":{"type":"isolated","value":1},"markPx":"10","availableToTrade":["0","0"],"maxTradeSzs":["0","0"]})
                }
                Info::ClearinghouseState { .. } | Info::ClearinghouseStateForDex { .. } => {
                    json!({"assetPositions": [], "marginSummary": {"accountValue":"0"}})
                }
                Info::SpotClearinghouseState { .. } => json!({"balances": []}),
                Info::FrontendOpenOrders { .. } | Info::FrontendOpenOrdersForDex { .. } => {
                    json!([])
                }
                _ => return Err(native_error("Unexpected fixture request.")),
            })
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &fin_exchange::ccxt::HyperliquidActionEnvelope,
            _journal: std::sync::Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<Value, FinError> {
            Err(native_error("Read fixture must not dispatch writes."))
        }
    }

    #[tokio::test]
    async fn flat_trading_state_uses_exact_protected_execution_account_without_writes() {
        let transport = std::sync::Arc::new(RequestCapture(Mutex::new(Vec::new())));
        let profile = account_profile(transport.clone());
        let result = profile
            .read_operation(
                "hyperliquid.trading_state",
                &json!({"account":"fixture-account","symbol":"ETH/USDC:USDC"}),
            )
            .await
            .unwrap();
        assert_eq!(
            result["data"]["state"]["leverage"],
            json!({"type":"isolated","value":1})
        );
        assert!(transport.0.lock().unwrap().iter().any(|request| matches!(request, Info::ActiveAssetData {owner,coin} if owner=="0x2222222222222222222222222222222222222222" && coin=="ETH")));
        assert!(!crate::writes::trading_state_identity_matches(
            &json!({"user":"wrong","coin":"ETH"}),
            "0x2222222222222222222222222222222222222222",
            "ETH"
        ));
        assert!(!crate::writes::trading_state_identity_matches(
            &json!({"user":123,"coin":"ETH"}),
            "123",
            "ETH"
        ));
        assert!(!crate::writes::trading_state_identity_matches(
            &json!({"user":"0x2222222222222222222222222222222222222222","coin":"BTC"}),
            "0x2222222222222222222222222222222222222222",
            "ETH"
        ));
    }

    #[tokio::test]
    async fn selected_primary_book_uses_only_primary_metadata_and_never_sweeps_hip3() {
        let transport = std::sync::Arc::new(RequestCapture(Mutex::new(Vec::new())));
        let profile = account_profile(transport.clone());
        let result = profile
            .read_operation("hyperliquid.order_book", &json!({"symbol":"ETH/USDC:USDC"}))
            .await
            .unwrap();
        assert_eq!(result["data"]["coin"], "ETH");
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec![Info::MetaAndAssetCtxs, Info::L2Book { coin: "ETH".into() }]
        );

        transport.0.lock().unwrap().clear();
        assert!(profile.market("ABC", None, None).await.is_err());
        assert_eq!(*transport.0.lock().unwrap(), vec![Info::MetaAndAssetCtxs]);
    }

    struct NoPrice;

    #[async_trait::async_trait]
    impl HyperliquidPrices for NoPrice {
        async fn usdc_price(&self) -> Result<fin_domain::AssetPrice, FinError> {
            Err(native_error(
                "Price fixture must not be used by account reads.",
            ))
        }
    }

    fn account_profile(transport: std::sync::Arc<RequestCapture>) -> HyperliquidProfile {
        HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "fixture-account".into(),
                owner: "0x1111111111111111111111111111111111111111".into(),
                credential_ref: None,
                signing_enabled: false,
                execution_scope: HyperliquidExecutionScope::Vault {
                    address: "0x2222222222222222222222222222222222222222".into(),
                },
            }],
            transport,
            std::sync::Arc::new(NoPrice),
            std::sync::Arc::new(DeferredExecutionState::default()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn selected_dex_account_reads_use_exact_execution_owner_and_typed_info_variants() {
        let transport = std::sync::Arc::new(RequestCapture(Mutex::new(Vec::new())));
        let profile = account_profile(transport.clone());
        let execution_owner = "0x2222222222222222222222222222222222222222";

        let balances = profile
            .read_operation(
                "hyperliquid.balances",
                &json!({"account":"fixture-account","dex":"xyz"}),
            )
            .await
            .unwrap();
        assert_eq!(balances["data"]["owner"], execution_owner);
        assert_eq!(
            balances["data"]["signer"],
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(balances["data"]["dex"], "xyz");
        assert!(
            balances["data"]["coverage"]
                .as_str()
                .unwrap()
                .contains("account-wide native spot balances")
        );
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec![
                Info::PerpDexs,
                Info::ClearinghouseStateForDex {
                    owner: execution_owner.into(),
                    dex: "xyz".into(),
                },
                Info::SpotClearinghouseState {
                    owner: execution_owner.into(),
                },
            ]
        );

        transport.0.lock().unwrap().clear();
        profile
            .read_operation(
                "hyperliquid.positions",
                &json!({"account":"fixture-account","dex":"xyz"}),
            )
            .await
            .unwrap();
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec![
                Info::PerpDexs,
                Info::ClearinghouseStateForDex {
                    owner: execution_owner.into(),
                    dex: "xyz".into(),
                },
            ]
        );

        transport.0.lock().unwrap().clear();
        profile
            .read_operation(
                "hyperliquid.open_orders",
                &json!({"account":"fixture-account","dex":"xyz"}),
            )
            .await
            .unwrap();
        assert_eq!(
            *transport.0.lock().unwrap(),
            vec![
                Info::PerpDexs,
                Info::FrontendOpenOrdersForDex {
                    owner: execution_owner.into(),
                    dex: "xyz".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn account_reads_reject_dex_for_unscoped_methods_and_unknown_dex_names() {
        let transport = std::sync::Arc::new(RequestCapture(Mutex::new(Vec::new())));
        let profile = account_profile(transport.clone());
        assert!(
            profile
                .read_operation(
                    "hyperliquid.fees",
                    &json!({"account":"fixture-account","dex":"xyz"}),
                )
                .await
                .is_err()
        );
        assert!(transport.0.lock().unwrap().is_empty());

        let error = profile
            .read_operation(
                "hyperliquid.positions",
                &json!({"account":"fixture-account","dex":"unknown"}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "INVALID_INPUT");
        assert_eq!(*transport.0.lock().unwrap(), vec![Info::PerpDexs]);
    }

    struct SnapshotFixture {
        abstraction: &'static str,
        requests: Mutex<Vec<Info>>,
    }

    #[async_trait::async_trait]
    impl HyperliquidTransport for SnapshotFixture {
        async fn info(&self, request: Info) -> Result<Value, FinError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(match request {
                Info::UserRole { .. } => json!({"role":"user"}),
                Info::UserAbstractionState { .. } => json!(self.abstraction),
                Info::UserDexAbstractionState { .. } => json!(false),
                Info::MetaAndAssetCtxs => {
                    json!([{"universe":[{"name":"ETH","szDecimals":4,"maxLeverage":20}]},[{"markPx":"10"}]])
                }
                Info::SpotMetaAndAssetCtxs => json!([{
                    "tokens":[{"name":"USDC","tokenId":NATIVE_USDC_TOKEN_ID,"index":0,"weiDecimals":8}],
                    "universe":[]
                },[]]),
                Info::ClearinghouseState { .. } => json!({
                    "withdrawable":"5",
                    "marginSummary":{"totalRawUsd":"5","totalMarginUsed":"0"},
                    "assetPositions":[{"position":{"coin":"ETH","szi":"0.5","entryPx":"10","unrealizedPnl":"1","liquidationPx":"5","marginUsed":"2"}}]
                }),
                Info::SpotClearinghouseState { .. } => json!({"balances":[{
                    "coin":"USDC","total":"10","hold":"2"
                }]}),
                Info::FrontendOpenOrders { .. } => json!([]),
                _ => return Err(native_error("Unexpected snapshot fixture request.")),
            })
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &fin_exchange::ccxt::HyperliquidActionEnvelope,
            _journal: std::sync::Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<Value, FinError> {
            Err(native_error("Snapshot fixture must not dispatch writes."))
        }
    }

    fn snapshot_profile(fixture: std::sync::Arc<SnapshotFixture>) -> HyperliquidProfile {
        HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "primary".into(),
                owner: "0x1111111111111111111111111111111111111111".into(),
                credential_ref: None,
                signing_enabled: false,
                execution_scope: HyperliquidExecutionScope::Owner,
            }],
            fixture,
            std::sync::Arc::new(NoPrice),
            std::sync::Arc::new(DeferredExecutionState::default()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn unified_snapshot_counts_shared_usdc_cash_once_and_keeps_positions_separate() {
        let separate_fixture = std::sync::Arc::new(SnapshotFixture {
            abstraction: "disabled",
            requests: Mutex::new(Vec::new()),
        });
        let separate = snapshot_profile(separate_fixture)
            .financial_snapshot()
            .await
            .unwrap();
        assert_eq!(separate.state.holdings.len(), 2);
        assert!(separate.state.holdings.iter().any(|holding| {
            holding.resource.0.contains(":perpetual:")
                && holding.amount.atoms == Quantity::from_decimal("5", 36).unwrap()
        }));
        assert!(separate.state.holdings.iter().any(|holding| {
            holding.resource.0.contains(":spot:")
                && holding.amount.atoms == Quantity::from_decimal("10", 36).unwrap()
        }));

        let unified_fixture = std::sync::Arc::new(SnapshotFixture {
            abstraction: "unifiedAccount",
            requests: Mutex::new(Vec::new()),
        });
        let unified = snapshot_profile(unified_fixture)
            .financial_snapshot()
            .await
            .unwrap();
        assert_eq!(unified.state.holdings.len(), 1);
        assert_eq!(
            unified.state.holdings[0].resource.0,
            "hyperliquid:mainnet:0x1111111111111111111111111111111111111111:unified:primary"
        );
        assert_eq!(unified.state.holdings[0].amount.asset, usdc());
        assert_eq!(
            unified.state.holdings[0].amount.atoms,
            Quantity::from_decimal("10", 36).unwrap()
        );
        assert_eq!(unified.state.positions.len(), 1);
        assert!(
            unified.state.positions[0]
                .resource
                .0
                .contains(":perpetual:")
        );
        assert!(
            !unified
                .state
                .holdings
                .iter()
                .any(|holding| holding.amount.atoms == Quantity::from_decimal("5", 36).unwrap())
        );
    }

    struct MissingRecipientSnapshot {
        has_balance: bool,
    }

    #[async_trait::async_trait]
    impl HyperliquidTransport for MissingRecipientSnapshot {
        async fn info(&self, request: Info) -> Result<Value, FinError> {
            Ok(match request {
                Info::UserRole { .. } => json!({"role":"missing"}),
                Info::UserAbstractionState { .. } => json!("disabled"),
                Info::UserDexAbstractionState { .. } => Value::Null,
                Info::MetaAndAssetCtxs => json!([{"universe":[]},[]]),
                Info::SpotMetaAndAssetCtxs => json!([{
                    "tokens":[{"name":"USDC","tokenId":NATIVE_USDC_TOKEN_ID,"index":0,"weiDecimals":8}],
                    "universe":[]
                },[]]),
                Info::ClearinghouseState { .. } => json!({
                    "withdrawable":"0",
                    "marginSummary":{"totalRawUsd":"0","totalMarginUsed":"0"},
                    "assetPositions":[]
                }),
                Info::SpotClearinghouseState { .. } => json!({"balances":[{
                    "coin":"USDC",
                    "total": if self.has_balance {"1"} else {"0"},
                    "hold":"0"
                }]}),
                Info::FrontendOpenOrders { .. } => json!([]),
                _ => {
                    return Err(native_error(
                        "Unexpected missing-recipient fixture request.",
                    ));
                }
            })
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &fin_exchange::ccxt::HyperliquidActionEnvelope,
            _journal: std::sync::Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<Value, FinError> {
            Err(native_error("Read fixture must not dispatch writes."))
        }
    }

    #[tokio::test]
    async fn missing_role_snapshot_is_limited_to_explicit_empty_recipient() {
        let empty = HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "primary".into(),
                owner: "0x1111111111111111111111111111111111111111".into(),
                credential_ref: None,
                signing_enabled: false,
                execution_scope: HyperliquidExecutionScope::Owner,
            }],
            std::sync::Arc::new(MissingRecipientSnapshot { has_balance: false }),
            std::sync::Arc::new(NoPrice),
            std::sync::Arc::new(DeferredExecutionState::default()),
        )
        .unwrap();
        let snapshot = empty.financial_snapshot().await.unwrap();
        assert!(
            snapshot
                .state
                .holdings
                .iter()
                .all(|holding| holding.amount.atoms.is_zero())
        );
        assert!(snapshot.state.positions.is_empty());
        assert!(snapshot.state.in_flight.is_empty());
        assert!(snapshot.state.authorities.iter().any(|authority| {
            authority.authority == "hyperliquid:native:account_role"
                && authority.value == "missing_recipient_only"
        }));

        let funded = HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "primary".into(),
                owner: "0x1111111111111111111111111111111111111111".into(),
                credential_ref: None,
                signing_enabled: false,
                execution_scope: HyperliquidExecutionScope::Owner,
            }],
            std::sync::Arc::new(MissingRecipientSnapshot { has_balance: true }),
            std::sync::Arc::new(NoPrice),
            std::sync::Arc::new(DeferredExecutionState::default()),
        )
        .unwrap();
        let error = funded.financial_snapshot().await.unwrap_err();
        assert_eq!(error.code, "HYPERLIQUID_EVIDENCE_INVALID");
    }

    #[tokio::test]
    async fn portfolio_margin_snapshot_fails_closed_before_emitting_cash() {
        let fixture = std::sync::Arc::new(SnapshotFixture {
            abstraction: "portfolioMargin",
            requests: Mutex::new(Vec::new()),
        });
        let error = snapshot_profile(fixture)
            .financial_snapshot()
            .await
            .unwrap_err();
        assert_eq!(error.code, "HYPERLIQUID_ACCOUNT_MODE_UNSUPPORTED");
    }
}
