//! Preparation for native Hyperliquid order amendments.
//!
//! An amendment is a new immutable GoFin submission over the original native
//! order id. The original order's status is frozen separately from the
//! replacement scope so settlement can account for old fills and prove the
//! replacement identity without rewriting either history.

use crate::{
    account_mode::AccountModeState,
    profile::HyperliquidProfile,
    read_profile::{input, native_error},
    types::{
        EditOrderInput, EditOrderItem, EditOrdersInput, MarketKind, MarketType, NativeMarket,
        OrderSide, OrderType, TimeInForce, TriggerKind,
    },
    writes::{ActionBinding, OrderScope, positive, q},
};
use gofin_domain::{
    FinancialIntent, GoFinError, Quantity, ReplacementBinding, Result, Status, binding_hash,
};
use gofin_exchange::ccxt::{
    HyperliquidAction as Action, HyperliquidActionEnvelope, HyperliquidAmend,
    HyperliquidInfoRequest as Info, HyperliquidOrder as Order,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Exact prepared replacement requests consumed by the normal journal path.
#[derive(Clone, Debug)]
pub struct AmendPreparation {
    pub binding: ActionBinding,
    /// Durable parent submission to inject into prepared binding metadata.
    pub replacement: Option<ReplacementBinding>,
    pub intent: FinancialIntent,
    pub movement: Quantity,
    pub fee: Quantity,
    pub warnings: Vec<String>,
}

impl AmendPreparation {
    /// Serialize the typed action and trusted parent link for the durable
    /// prepared binding. The parent is never read from free-form metadata.
    pub fn persisted_binding(&self) -> Result<Value> {
        let mut binding = self.binding.clone();
        binding.replacement = self.replacement.clone();
        serde_json::to_value(binding)
            .map_err(|_| native_error("Amendment binding serialization failed."))
    }
}

#[derive(Clone, Debug)]
struct OriginalOrder {
    oid: u64,
    coin: String,
    side: OrderSide,
    size: String,
    remaining: String,
    price: String,
    tif: Option<String>,
    reduce_only: bool,
    is_trigger: bool,
    order_type: String,
    trigger_price: String,
    trigger_condition: String,
    is_position_tpsl: bool,
    cloid: Option<String>,
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        native_error("Native order status is missing a required exact string field.")
    })
}

fn parse_original(value: &Value, expected_oid: u64, expected_coin: &str) -> Result<OriginalOrder> {
    if value["status"] != "order" || value["order"]["status"] != "open" {
        return Err(GoFinError::validation(
            "Only one currently open native order can be amended.",
        ));
    }
    let order = &value["order"]["order"];
    let oid = order["oid"]
        .as_u64()
        .ok_or_else(|| native_error("Native order ID is missing or invalid."))?;
    let coin = required_string(order, "coin")?.to_owned();
    if oid != expected_oid || coin != expected_coin {
        return Err(GoFinError::validation(
            "The order ID does not belong to the selected owner and market.",
        ));
    }
    let side = match required_string(order, "side")? {
        "B" => OrderSide::Buy,
        "A" => OrderSide::Sell,
        _ => return Err(native_error("Native order side is invalid.")),
    };
    let original_size = required_string(order, "origSz")?;
    let remaining = required_string(order, "sz")?;
    let price = required_string(order, "limitPx")?;
    let original_size_q = positive(original_size)?;
    let remaining_q = positive(remaining)?;
    positive(price)?;
    if remaining_q > original_size_q {
        return Err(native_error(
            "Native remaining order size exceeds its original quantity.",
        ));
    }
    let reduce_only = order["reduceOnly"]
        .as_bool()
        .ok_or_else(|| native_error("Native reduce-only state is missing."))?;
    let is_trigger = order["isTrigger"]
        .as_bool()
        .ok_or_else(|| native_error("Native trigger state is missing."))?;
    let order_type = required_string(order, "orderType")?.to_owned();
    let trigger_price = required_string(order, "triggerPx")?.to_owned();
    let trigger_condition = required_string(order, "triggerCondition")?.to_owned();
    let is_position_tpsl = order["isPositionTpsl"]
        .as_bool()
        .ok_or_else(|| native_error("Native position TP/SL state is missing."))?;
    let tif = order.get("tif").and_then(Value::as_str).map(str::to_owned);
    if !is_trigger && tif.is_none() {
        return Err(native_error("Native limit order time-in-force is missing."));
    }
    let cloid = match order.get("cloid") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if valid_cloid(value) => Some(value.clone()),
        Some(_) => return Err(native_error("Native client order ID is malformed.")),
    };
    if is_trigger {
        if trigger_price == "0" || trigger_price == "0.0" {
            return Err(native_error("Native trigger price is missing."));
        }
        positive(&trigger_price)?;
        if !matches!(
            order_type.as_str(),
            "Stop Market" | "Stop Limit" | "Take Profit Market" | "Take Profit Limit"
        ) {
            return Err(GoFinError::validation(
                "The selected trigger order type cannot be preserved by the native amendment.",
            ));
        }
    } else if order_type != "Limit" {
        return Err(GoFinError::validation(
            "Only native limit and trigger orders can be amended through this operation.",
        ));
    }
    Ok(OriginalOrder {
        oid,
        coin,
        side,
        size: original_size.to_owned(),
        remaining: remaining.to_owned(),
        price: price.to_owned(),
        tif,
        reduce_only,
        is_trigger,
        order_type,
        trigger_price,
        trigger_condition,
        is_position_tpsl,
        cloid,
    })
}

fn valid_cloid(value: &str) -> bool {
    value.len() == 34
        && value.starts_with("0x")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn tif(value: Option<&str>) -> Result<Option<TimeInForce>> {
    match value {
        Some("Gtc") => Ok(Some(TimeInForce::Gtc)),
        Some("Ioc") => Ok(Some(TimeInForce::Ioc)),
        Some("Alo") => Ok(Some(TimeInForce::Alo)),
        Some(_) => Err(GoFinError::validation(
            "The original order's time-in-force cannot be represented by the native amendment.",
        )),
        None => Ok(None),
    }
}

fn trigger_kind(order_type: &str) -> Option<TriggerKind> {
    if order_type.starts_with("Take Profit") {
        Some(TriggerKind::TakeProfit)
    } else if order_type.starts_with("Stop ") {
        Some(TriggerKind::StopLoss)
    } else {
        None
    }
}

fn frozen_original(order: &OriginalOrder) -> Value {
    // Exclude observation times, status timestamps, and other volatile fields.
    json!({
        "oid": order.oid,
        "coin": order.coin,
        "side": order.side,
        "original_size": order.size,
        "remaining_size": order.remaining,
        "limit_price": order.price,
        "tif": order.tif,
        "reduce_only": order.reduce_only,
        "is_trigger": order.is_trigger,
        "order_type": order.order_type,
        "trigger_price": order.trigger_price,
        "trigger_condition": order.trigger_condition,
        "is_position_tpsl": order.is_position_tpsl,
        "cloid": order.cloid,
    })
}

fn from_item(request_id: &str, account: &str, item: EditOrderItem) -> EditOrderInput {
    EditOrderInput {
        request_id: request_id.to_owned(),
        account: account.to_owned(),
        symbol: item.symbol,
        market_type: item.market_type,
        dex: item.dex,
        order_id: item.order_id,
        parent_operation_id: item.parent_operation_id,
        size: item.size,
        limit_price: item.limit_price,
        max_notional_usdc: item.max_notional_usdc,
        max_fee_usdc: item.max_fee_usdc,
        max_realized_loss_usdc: item.max_realized_loss_usdc,
    }
}

fn validate_parent_order(
    profile: &HyperliquidProfile,
    parent_operation_id: &str,
    account: &crate::HyperliquidAccount,
    request: &EditOrderInput,
    market: &NativeMarket,
    original: &OriginalOrder,
    account_mode: &AccountModeState,
) -> Result<ReplacementBinding> {
    let parent = profile
        .nonces
        .operation(parent_operation_id)?
        .ok_or_else(|| {
            GoFinError::validation("The linked parent GoFin order operation was not found.")
        })?;
    if parent.id != parent_operation_id
        || !matches!(
            parent.status,
            Status::Submitted | Status::Pending | Status::Unknown
        )
        || !matches!(
            parent.operation.as_str(),
            "hyperliquid.orders.place" | "hyperliquid.orders.batch"
        )
    {
        return Err(GoFinError::validation(
            "A replacement parent must be an unresolved submitted Hyperliquid order operation.",
        ));
    }
    let submission = parent.submission.clone().ok_or_else(|| {
        GoFinError::validation("The parent GoFin order has no durable submission.")
    })?;
    let prepared = parent.preview.as_ref().ok_or_else(|| {
        GoFinError::validation("The parent GoFin order has no frozen preparation.")
    })?;
    if prepared.operation != parent.operation {
        return Err(native_error(
            "The parent preview operation identity changed.",
        ));
    }
    let parent_binding = ActionBinding::decode(&prepared.binding)?;
    if parent_binding.account != *account
        || parent_binding.account_mode.as_ref() != Some(account_mode)
    {
        return Err(GoFinError::validation(
            "The parent GoFin order belongs to a different protected account or account mode.",
        ));
    }
    let Action::Order {
        orders,
        grouping,
        builder_fee,
    } = &parent_binding.action
    else {
        return Err(GoFinError::validation(
            "The linked parent submission is not a native order placement.",
        ));
    };
    if builder_fee.is_some() {
        return Err(GoFinError::validation(
            "Native batchModify cannot preserve an order-level builder fee; this order cannot be amended.",
        ));
    }
    if orders.len() != parent_binding.orders.len() {
        return Err(native_error(
            "The parent native order and frozen scopes differ.",
        ));
    }
    let envelope: HyperliquidActionEnvelope =
        serde_json::from_value(submission.data.get("envelope").cloned().ok_or_else(|| {
            native_error("The parent durable submission is missing its action envelope.")
        })?)
        .map_err(|_| native_error("The parent durable action envelope is invalid."))?;
    let wire = envelope
        .action_wire_json()
        .map_err(|_| native_error("The parent action cannot be reconstructed."))?;
    let since_ms = submission.data.get("since_ms").and_then(Value::as_u64);
    let expected_reference = binding_hash(&json!({
        "version": 1,
        "owner": account.owner,
        "execution_address": account.execution_address(),
        "vault_address": account.vault_address(),
        "operation": parent.operation,
        "wire": wire,
    }))?;
    if submission.kind != crate::writes::SUBMISSION_KIND
        || submission.reference != expected_reference
        || submission.data.get("wire") != Some(&wire)
        || submission.data["owner"] != account.owner
        || submission.data["execution_address"] != json!(account.execution_address())
        || submission.data["vault_address"] != json!(account.vault_address())
        || submission.data["account"] != account.name
        || submission.data["operation"] != parent.operation
        || since_ms != Some(envelope.nonce_ms.saturating_sub(5000))
        || envelope.action != parent_binding.action
        || envelope.vault_address.as_deref() != account.vault_address()
    {
        return Err(GoFinError::validation(
            "The linked parent identity is not the exact persisted owner-scoped order submission.",
        ));
    }
    let side_is_buy = matches!(original.side, OrderSide::Buy);
    let is_position_tpsl = matches!(
        grouping,
        gofin_exchange::ccxt::HyperliquidOrderGrouping::PositionTpsl
    );
    let matches = parent_binding
        .orders
        .iter()
        .zip(orders)
        .filter(|(scope, order)| {
            scope.cloid == original.cloid.as_deref().unwrap_or_default()
                && scope.input.account == account.name
                && scope.input.symbol == request.symbol
                && scope.input.market_type == request.market_type
                && scope.input.dex == request.dex
                && scope.market.native_coin == market.native_coin
                && scope.market.asset_id == market.asset_id
                && order.asset == market.asset_id
                && order.is_buy == side_is_buy
                && q(&order.size).ok() == q(&scope.input.size).ok()
                && order.reduce_only == scope.input.reduce_only
                && order.client_order_id.as_deref() == Some(scope.cloid.as_str())
                && q(&order.size).ok() == q(&original.size).ok()
                && q(&order.price).ok() == q(&original.price).ok()
                && native_order_type_matches(&order.order_type, original)
                && original.is_position_tpsl == is_position_tpsl
        })
        .count();
    if original.cloid.is_none() || matches != 1 {
        return Err(GoFinError::validation(
            "The selected native order is not uniquely bound to the linked GoFin parent submission.",
        ));
    }
    Ok(ReplacementBinding {
        version: 1,
        parent_operation_id: parent.id,
        parent_submission: submission,
    })
}

fn native_order_type_matches(
    order_type: &gofin_exchange::ccxt::HyperliquidOrderType,
    original: &OriginalOrder,
) -> bool {
    use gofin_exchange::ccxt::{HyperliquidOrderType, HyperliquidTimeInForce, HyperliquidTpsl};
    if original.is_trigger {
        let tpsl = if original.order_type.starts_with("Take Profit") {
            HyperliquidTpsl::TakeProfit
        } else {
            HyperliquidTpsl::StopLoss
        };
        matches!(
            order_type,
            HyperliquidOrderType::Trigger {
                trigger_price,
                is_market,
                tpsl: native_tpsl,
            } if q(trigger_price).ok() == q(&original.trigger_price).ok()
                && *is_market == original.order_type.ends_with("Market")
                && *native_tpsl == tpsl
        )
    } else {
        let tif = match original.tif.as_deref() {
            Some("Gtc") => HyperliquidTimeInForce::Gtc,
            Some("Ioc") => HyperliquidTimeInForce::Ioc,
            Some("Alo") => HyperliquidTimeInForce::Alo,
            _ => return false,
        };
        matches!(order_type, HyperliquidOrderType::Limit { time_in_force } if *time_in_force == tif)
    }
}

impl HyperliquidProfile {
    /// Prepare one edit or a bounded batch of edits. This builds only native
    /// `batchModify`; it never cancels and recreates an order as a fallback.
    pub async fn prepare_amend(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<Option<AmendPreparation>> {
        let (account_name, requests) = match operation {
            "hyperliquid.orders.edit" => {
                let request: EditOrderInput = input(raw)?;
                (request.account.clone(), vec![request])
            }
            "hyperliquid.orders.edit_batch" => {
                let request: EditOrdersInput = input(raw)?;
                if request.orders.is_empty() || request.orders.len() > 100 {
                    return Err(GoFinError::validation(
                        "Amend batch must contain 1–100 exact open orders.",
                    ));
                }
                let mut ids = BTreeSet::new();
                let mut requests = Vec::with_capacity(request.orders.len());
                for item in request.orders {
                    let oid: u64 = item.order_id.parse().map_err(|_| {
                        GoFinError::validation("Amendment requires a numeric native order ID.")
                    })?;
                    if !ids.insert(oid) {
                        return Err(GoFinError::validation(
                            "An amendment batch cannot target one native order more than once.",
                        ));
                    }
                    requests.push(from_item(&request.request_id, &request.account, item));
                }
                (request.account, requests)
            }
            _ => return Ok(None),
        };
        let parent_ids: BTreeSet<_> = requests
            .iter()
            .filter_map(|request| request.parent_operation_id.as_deref())
            .collect();
        if parent_ids.len() > 1
            || (!parent_ids.is_empty()
                && requests.iter().any(|request| {
                    request.parent_operation_id.as_deref() != parent_ids.first().copied()
                }))
        {
            return Err(GoFinError::validation(
                "A native edit batch must link every child to the same parent submission.",
            ));
        }
        let account = self.account(&account_name)?.clone();
        let account_mode = self.verified_account_mode(&account).await?;
        account_mode.ensure_supported_cash_accounting()?;
        if !account.signing_enabled {
            return Err(GoFinError::new(
                "SIGNING_UNAVAILABLE",
                "preview",
                "This Hyperliquid account is read-only.",
                "Enroll a protected owner signer in your own Terminal.",
            ));
        }
        let mut intent = FinancialIntent::default();
        let mut scopes = Vec::<OrderScope>::with_capacity(requests.len());
        let mut modifies = Vec::with_capacity(requests.len());
        let mut originals = Vec::with_capacity(requests.len());
        let mut replacement = None::<ReplacementBinding>;
        let mut movement = Quantity::zero();
        let mut fee = Quantity::zero();
        for (ordinal, request) in requests.into_iter().enumerate() {
            if request.account != account_name {
                return Err(GoFinError::validation(
                    "Every edit child must use the enrolled account.",
                ));
            }
            let order_id: u64 = request.order_id.parse().map_err(|_| {
                GoFinError::validation("Amendment requires a numeric native order ID.")
            })?;
            let market_kind = match request.market_type {
                MarketType::Spot => MarketKind::Spot,
                MarketType::Perpetual if request.dex.is_some() => MarketKind::Hip3Perpetual,
                MarketType::Perpetual => MarketKind::Perpetual,
            };
            let market = self
                .market(&request.symbol, Some(market_kind), request.dex.as_deref())
                .await?;
            let original = self
                .transport
                .info(Info::OrderStatus {
                    owner: account.execution_address().to_owned(),
                    order_id,
                })
                .await?;
            let old = parse_original(&original, order_id, &market.native_coin)?;
            if let Some(parent_operation_id) = request.parent_operation_id.as_deref() {
                let resolved = validate_parent_order(
                    self,
                    parent_operation_id,
                    &account,
                    &request,
                    &market,
                    &old,
                    &account_mode,
                )?;
                if replacement.as_ref().is_some_and(|saved| saved != &resolved) {
                    return Err(GoFinError::validation(
                        "Every linked edit in one native batch must use the same durable parent submission.",
                    ));
                }
                replacement = Some(resolved);
            }
            let trigger = if old.is_trigger {
                Some(trigger_kind(&old.order_type).ok_or_else(|| {
                    GoFinError::validation("The original trigger kind cannot be preserved.")
                })?)
            } else {
                None
            };
            let order_request = crate::types::LimitOrderInput {
                request_id: request.request_id.clone(),
                account: account_name.clone(),
                symbol: request.symbol.clone(),
                market_type: request.market_type.clone(),
                dex: request.dex.clone(),
                side: old.side.clone(),
                order_type: if old.is_trigger {
                    OrderType::Trigger
                } else {
                    OrderType::Limit
                },
                size: request.size.clone(),
                limit_price: Some(request.limit_price.clone()),
                min_price: Some(request.limit_price.clone()),
                max_price: Some(request.limit_price.clone()),
                time_in_force: if old.is_trigger {
                    None
                } else {
                    tif(old.tif.as_deref())?
                },
                trigger_price: trigger.as_ref().map(|_| old.trigger_price.clone()),
                trigger_kind: trigger,
                trigger_is_market: old.is_trigger.then_some(old.order_type.ends_with("Market")),
                grouping: None,
                max_notional_usdc: request.max_notional_usdc.clone(),
                max_fee_usdc: request.max_fee_usdc.clone(),
                max_realized_loss_usdc: request.max_realized_loss_usdc.clone(),
                builder_fee: None,
                reduce_only: old.reduce_only,
            };
            let (scope, replacement, child_intent, child_movement, child_fee) = self
                .order_scope(operation, order_request, ordinal as u32, &account_mode)
                .await?;
            let child = Order {
                asset: replacement.asset,
                is_buy: replacement.is_buy,
                price: replacement.price,
                size: replacement.size,
                reduce_only: replacement.reduce_only,
                order_type: replacement.order_type,
                client_order_id: replacement.client_order_id,
            };
            movement = movement.checked_add(&child_movement)?;
            fee = fee.checked_add(&child_fee)?;
            intent.debits.extend(child_intent.debits);
            intent.maximum_costs.extend(child_intent.maximum_costs);
            intent.expected.extend(child_intent.expected);
            intent.locks.extend(child_intent.locks);
            modifies.push(HyperliquidAmend {
                asset: market.asset_id,
                order_id,
                order: child,
            });
            scopes.push(scope);
            originals.push(frozen_original(&old));
        }
        intent.locks.sort();
        intent.locks.dedup();
        Ok(Some(AmendPreparation {
            binding: ActionBinding {
                account,
                account_mode: Some(account_mode),
                action: Action::Amend { modifies },
                orders: scopes,
                authority_before: Some(json!({"amendments":originals})),
                replacement: None,
                recovery: None,
                metadata: Default::default(),
            },
            replacement,
            intent,
            movement,
            fee,
            warnings: vec![
                "An amendment may race with fills on the original order; its prior fills remain independently reconciled and the original reservation remains until replacement is proved.".into(),
            ],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HyperliquidAccount, HyperliquidNonces, HyperliquidPrices, HyperliquidTransport,
        profile::{HyperliquidProfile, usdc},
    };
    use async_trait::async_trait;
    use gofin_domain::AssetPrice;
    use gofin_exchange::ccxt::{HyperliquidActionEnvelope, HyperliquidInfoRequest};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    const OWNER: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Default)]
    struct Venue {
        orders: Mutex<HashMap<u64, Value>>,
    }

    #[async_trait]
    impl HyperliquidTransport for Venue {
        async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value> {
            Ok(match request {
                HyperliquidInfoRequest::MetaAndAssetCtxs => json!([
                    {"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":50}]},
                    [{"markPx":"10","midPx":"10","oraclePx":"10"}]
                ]),
                HyperliquidInfoRequest::ClearinghouseState { .. } => {
                    json!({"assetPositions":[],"withdrawable":"1000"})
                }
                HyperliquidInfoRequest::ActiveAssetData { owner, coin } => json!({
                    "user":owner,"coin":coin,
                    "leverage":{"type":"cross","value":10},
                    "availableToTrade":["1000","1000"],
                    "maxTradeSzs":["100","100"],"markPx":"10"
                }),
                HyperliquidInfoRequest::SpotClearinghouseState { .. } => json!({"balances":[
                    {"coin":"USDC","token":0,"total":"1000","hold":"0"},
                    {"coin":"ETH","token":1,"total":"1000","hold":"0"}
                ]}),
                HyperliquidInfoRequest::UserRole { .. } => json!({"role":"user"}),
                HyperliquidInfoRequest::UserAbstractionState { .. } => {
                    json!({"abstraction":"disabled"})
                }
                HyperliquidInfoRequest::UserDexAbstractionState { .. } => json!(false),
                HyperliquidInfoRequest::OrderStatus { order_id, .. } => self
                    .orders
                    .lock()
                    .unwrap()
                    .get(&order_id)
                    .cloned()
                    .unwrap_or_else(|| json!({"status":"unknownOid"})),
                other => panic!("unexpected synthetic info request: {other:?}"),
            })
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &HyperliquidActionEnvelope,
            _journal: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Value> {
            panic!("amend preparation must never dispatch")
        }
    }

    struct Prices;

    #[async_trait]
    impl HyperliquidPrices for Prices {
        async fn usdc_price(&self) -> Result<AssetPrice> {
            let at = gofin_domain::now();
            Ok(AssetPrice {
                asset: usdc(),
                usd_micros_per_unit: Quantity::new("1000000")?,
                unit_atoms: Quantity::new("1")?,
                observed_at: at,
                source: "synthetic-amend-test".into(),
            })
        }
    }

    #[derive(Default)]
    struct Nonces(Mutex<HashMap<String, gofin_domain::OperationRecord>>);

    impl HyperliquidNonces for Nonces {
        fn allocate(&self, _owner: &str, now_ms: u64) -> Result<u64> {
            Ok(now_ms)
        }
        fn operation(&self, id: &str) -> Result<Option<gofin_domain::OperationRecord>> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
    }

    fn test_profile(orders: &[(u64, Value)]) -> HyperliquidProfile {
        test_profile_with_parent(orders, None)
    }

    fn test_profile_with_parent(
        orders: &[(u64, Value)],
        parent: Option<gofin_domain::OperationRecord>,
    ) -> HyperliquidProfile {
        let venue = Arc::new(Venue::default());
        venue.orders.lock().unwrap().extend(orders.iter().cloned());
        let nonces = Arc::new(Nonces::default());
        if let Some(parent) = parent {
            nonces.0.lock().unwrap().insert(parent.id.clone(), parent);
        }
        HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "synthetic".into(),
                owner: OWNER.into(),
                credential_ref: Some("synthetic-key-ref".into()),
                signing_enabled: true,
                execution_scope: Default::default(),
            }],
            venue,
            Arc::new(Prices),
            nonces,
        )
        .unwrap()
    }

    fn durable_parent(status: Status) -> gofin_domain::OperationRecord {
        use crate::types::{AssetReference, LimitOrderInput, NativeMarket};
        use gofin_exchange::ccxt::{
            HyperliquidAction, HyperliquidOrderGrouping, HyperliquidOrderType,
            HyperliquidTimeInForce,
        };
        let account = HyperliquidAccount {
            name: "synthetic".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-key-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        };
        let cloid = "0x11111111111111111111111111111111".to_owned();
        let market = NativeMarket {
            symbol: "BTC/USDC:USDC".into(),
            native_coin: "BTC".into(),
            base_asset: AssetReference {
                reference: "BTC".into(),
                symbol: "BTC".into(),
                decimals: 36,
                token_index: None,
            },
            quote_asset: AssetReference {
                reference: "USDC".into(),
                symbol: "USDC".into(),
                decimals: 36,
                token_index: None,
            },
            kind: MarketKind::Perpetual,
            observed_at_ms: 100,
            dex: None,
            universe_index: 0,
            dex_index: None,
            asset_id: 0,
            sz_decimals: 3,
            max_leverage: Some(50),
            is_delisted: false,
            context: None,
            spot_tokens: None,
        };
        let input = LimitOrderInput {
            request_id: "parent-request".into(),
            account: account.name.clone(),
            symbol: market.symbol.clone(),
            market_type: MarketType::Perpetual,
            dex: None,
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            size: "1.0".into(),
            limit_price: Some("100".into()),
            min_price: Some("100".into()),
            max_price: Some("100".into()),
            time_in_force: Some(TimeInForce::Gtc),
            trigger_price: None,
            trigger_kind: None,
            trigger_is_market: None,
            grouping: None,
            max_notional_usdc: "100".into(),
            max_fee_usdc: "1".into(),
            max_realized_loss_usdc: None,
            builder_fee: None,
            reduce_only: false,
        };
        let native = Order {
            asset: 0,
            is_buy: true,
            price: "100".into(),
            size: "1.0".into(),
            reduce_only: false,
            order_type: HyperliquidOrderType::Limit {
                time_in_force: HyperliquidTimeInForce::Gtc,
            },
            client_order_id: Some(cloid.clone()),
        };
        let action = HyperliquidAction::Order {
            orders: vec![native],
            grouping: HyperliquidOrderGrouping::Na,
            builder_fee: None,
        };
        let binding = ActionBinding {
            account: account.clone(),
            account_mode: Some(AccountModeState {
                owner: account.owner.clone(),
                execution_address: account.execution_address().to_owned(),
                native_abstraction: "disabled".into(),
                mode: crate::account_mode::AccountMode::Separate,
                dex_abstraction: Some(false),
            }),
            action: action.clone(),
            orders: vec![OrderScope {
                input,
                market,
                cloid,
                min_price: "100".into(),
                max_price: "100".into(),
                position_before: "0".into(),
                capacity_before: None,
            }],
            authority_before: None,
            replacement: None,
            recovery: None,
            metadata: Default::default(),
        };
        let envelope = HyperliquidActionEnvelope {
            nonce_ms: 1234,
            vault_address: None,
            action,
        };
        let wire = envelope.action_wire_json().unwrap();
        let operation = "hyperliquid.orders.place";
        let submission = gofin_domain::Submission {
            kind: crate::writes::SUBMISSION_KIND.into(),
            reference: binding_hash(&json!({
                "version": 1,
                "owner": OWNER,
                "execution_address": account.execution_address(),
                "vault_address": account.vault_address(),
                "operation": operation,
                "wire": wire,
            }))
            .unwrap(),
            data: json!({
                "owner":OWNER,
                "execution_address":account.execution_address(),
                "vault_address":account.vault_address(),
                "account":account.name,
                "envelope":envelope,
                "wire":wire,
                "operation":operation,
                "since_ms":1234u64.saturating_sub(5000)
            }),
        };
        let now = gofin_domain::now();
        let prepared = gofin_domain::PreparedOperation {
            operation: operation.into(),
            input: json!({"request_id":"parent-request","account":"synthetic"}),
            binding: serde_json::to_value(binding).unwrap(),
            summary: "synthetic parent".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: now.clone(),
            expires_at: now.clone(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        gofin_domain::OperationRecord {
            id: "parent-operation".into(),
            request_id: "parent-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: operation.into(),
            input: prepared.input.clone(),
            status,
            created_at: now.clone(),
            updated_at: now,
            preview: Some(prepared),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    fn edit_input() -> Value {
        json!({
            "request_id":"synthetic-edit",
            "account":"synthetic",
            "symbol":"BTC/USDC:USDC",
            "market_type":"perpetual",
            "order_id":"12",
            "size":"1.0",
            "limit_price":"11",
            "max_notional_usdc":"12",
            "max_fee_usdc":"0.01"
        })
    }

    fn status(status: &str) -> Value {
        json!({"status":"order","order":{"status":status,"order":{
            "coin":"BTC","side":"B","limitPx":"100.00","sz":"0.4","oid":12,
            "timestamp":1700000000000u64,"origSz":"1.0","triggerCondition":"N/A",
            "isTrigger":false,"triggerPx":"0.0","isPositionTpsl":false,
            "reduceOnly":false,"orderType":"Limit","tif":"Gtc",
            "cloid":"0x11111111111111111111111111111111"
        }}})
    }

    #[test]
    fn open_order_snapshot_preserves_partial_fill_and_native_options() {
        let original = parse_original(&status("open"), 12, "BTC").unwrap();
        assert!(matches!(original.side, OrderSide::Buy));
        assert_eq!(original.size, "1.0");
        assert_eq!(original.remaining, "0.4");
        assert_eq!(original.tif.as_deref(), Some("Gtc"));
        assert_eq!(
            original.cloid.as_deref(),
            Some("0x11111111111111111111111111111111")
        );
        let frozen = frozen_original(&original);
        assert_eq!(frozen["remaining_size"], "0.4");
        assert!(frozen.get("timestamp").is_none());
    }

    #[test]
    fn order_status_must_match_owner_oid_market_and_live_open_state() {
        assert!(parse_original(&status("canceled"), 12, "BTC").is_err());
        assert!(parse_original(&status("open"), 13, "BTC").is_err());
        assert!(parse_original(&status("open"), 12, "ETH").is_err());
        let mut malformed = status("open");
        malformed["order"]["order"]["reduceOnly"] = Value::Null;
        assert!(parse_original(&malformed, 12, "BTC").is_err());
    }

    #[test]
    fn trigger_order_kind_and_time_in_force_are_closed() {
        let mut trigger = status("open");
        {
            let order = &mut trigger["order"]["order"];
            order["isTrigger"] = json!(true);
            order["orderType"] = json!("Take Profit Market");
            order["triggerPx"] = json!("120");
            order["triggerCondition"] = json!("Price above 120");
        }
        let parsed = parse_original(&trigger, 12, "BTC").unwrap();
        assert!(parsed.is_trigger);
        assert!(matches!(
            trigger_kind(&parsed.order_type),
            Some(TriggerKind::TakeProfit)
        ));
        assert!(tif(Some("Alo")).unwrap().is_some());
        assert!(tif(Some("FrontendMarket")).is_err());
        trigger["order"]["order"]["orderType"] = json!("Unknown Trigger");
        assert!(parse_original(&trigger, 12, "BTC").is_err());
    }

    #[tokio::test]
    async fn edit_preparation_uses_original_side_and_binds_partial_replacement() {
        let profile = test_profile(&[(12, status("open"))]);
        let prepared = profile
            .prepare_amend("hyperliquid.orders.edit", &edit_input())
            .await
            .unwrap()
            .unwrap();
        let Action::Amend { modifies } = prepared.binding.action else {
            panic!("expected native batchModify action")
        };
        assert_eq!(modifies.len(), 1);
        assert_eq!(modifies[0].order_id, 12);
        assert!(modifies[0].order.is_buy);
        assert_eq!(modifies[0].order.size, "1");
        assert_eq!(modifies[0].order.price, "11");
        assert_ne!(
            modifies[0].order.client_order_id.as_deref(),
            Some("0x11111111111111111111111111111111")
        );
        let frozen = &prepared.binding.authority_before.as_ref().unwrap()["amendments"][0];
        assert_eq!(frozen["remaining_size"], "0.4");
        assert_eq!(frozen["original_size"], "1.0");
        assert_eq!(prepared.binding.orders[0].input.size, "1.0");
    }

    #[tokio::test]
    async fn batch_edit_has_unique_cloids_and_rejects_failed_child_without_partial_action() {
        let mut second = status("open");
        second["order"]["order"]["oid"] = json!(13);
        second["order"]["order"]["side"] = json!("A");
        let profile = test_profile(&[(12, status("open")), (13, second.clone())]);
        let item = |order_id: &str| {
            json!({
                "symbol":"BTC/USDC:USDC","market_type":"perpetual","order_id":order_id,
                "size":"1.0","limit_price":"11","max_notional_usdc":"12","max_fee_usdc":"0.01"
            })
        };
        let batch = json!({
            "request_id":"synthetic-batch","account":"synthetic",
            "orders":[item("12"),item("13")]
        });
        let prepared = profile
            .prepare_amend("hyperliquid.orders.edit_batch", &batch)
            .await
            .unwrap()
            .unwrap();
        let Action::Amend { modifies } = prepared.binding.action else {
            panic!("expected native batchModify action")
        };
        assert_eq!(modifies.len(), 2);
        assert_ne!(
            modifies[0].order.client_order_id,
            modifies[1].order.client_order_id
        );
        assert!(modifies[0].order.is_buy);
        assert!(!modifies[1].order.is_buy);

        let mut rejected = second;
        rejected["order"]["status"] = json!("canceled");
        let profile = test_profile(&[(12, status("open")), (13, rejected)]);
        assert!(
            profile
                .prepare_amend("hyperliquid.orders.edit_batch", &batch)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn batch_rejects_duplicate_numeric_order_ids_before_venue_reads() {
        let profile = test_profile(&[]);
        let item = |order_id: &str| {
            json!({
                "symbol":"BTC/USDC:USDC","market_type":"perpetual","order_id":order_id,
                "size":"1.0","limit_price":"11","max_notional_usdc":"12","max_fee_usdc":"0.01"
            })
        };
        let batch = json!({
            "request_id":"synthetic-batch","account":"synthetic",
            "orders":[item("12"),item("012")]
        });
        assert!(
            profile
                .prepare_amend("hyperliquid.orders.edit_batch", &batch)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn edit_batch_rejects_mixed_or_multiple_parent_links_before_native_reads() {
        let profile = test_profile(&[]);
        let item = |order_id: &str, parent: Option<&str>| {
            let mut item = json!({
                "symbol":"BTC/USDC:USDC","market_type":"perpetual","order_id":order_id,
                "size":"0.5","limit_price":"11","max_notional_usdc":"6","max_fee_usdc":"0.01"
            });
            if let Some(parent) = parent {
                item["parent_operation_id"] = json!(parent);
            }
            item
        };
        for orders in [
            vec![item("12", Some("parent-operation")), item("13", None)],
            vec![
                item("12", Some("parent-operation")),
                item("13", Some("other-parent")),
            ],
        ] {
            let batch = json!({
                "request_id":"synthetic-batch","account":"synthetic","orders":orders
            });
            assert!(
                profile
                    .prepare_amend("hyperliquid.orders.edit_batch", &batch)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn amendment_links_only_exact_unresolved_parent_submission() {
        let parent = durable_parent(Status::Submitted);
        let expected_submission = parent.submission.clone().unwrap();
        let profile = test_profile_with_parent(&[(12, status("open"))], Some(parent));
        let mut request = edit_input();
        request["parent_operation_id"] = json!("parent-operation");
        let prepared = profile
            .prepare_amend("hyperliquid.orders.edit", &request)
            .await
            .unwrap()
            .unwrap();
        let replacement = prepared.replacement.as_ref().unwrap();
        assert_eq!(replacement.version, 1);
        assert_eq!(replacement.parent_operation_id, "parent-operation");
        assert_eq!(replacement.parent_submission, expected_submission);
        let persisted = prepared.persisted_binding().unwrap();
        assert_eq!(
            gofin_domain::product_names::get(&persisted, gofin_domain::REPLACEMENT_METADATA_KEY)
                .unwrap()
                .unwrap()
                .clone(),
            serde_json::to_value(replacement).unwrap()
        );

        for terminal in [
            Status::Completed,
            Status::Failed,
            Status::Cancelled,
            Status::Rejected,
        ] {
            let profile =
                test_profile_with_parent(&[(12, status("open"))], Some(durable_parent(terminal)));
            let mut request = edit_input();
            request["parent_operation_id"] = json!("parent-operation");
            assert!(
                profile
                    .prepare_amend("hyperliquid.orders.edit", &request)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn parent_binding_rejects_foreign_or_non_order_native_identity() {
        let mut parent = durable_parent(Status::Pending);
        let mut binding = parent.preview.as_ref().unwrap().binding.clone();
        binding["account"]["owner"] = json!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        parent.preview.as_mut().unwrap().binding = binding;
        let profile = test_profile_with_parent(&[(12, status("open"))], Some(parent));
        let mut request = edit_input();
        request["parent_operation_id"] = json!("parent-operation");
        assert!(
            profile
                .prepare_amend("hyperliquid.orders.edit", &request)
                .await
                .is_err()
        );

        let mut parent = durable_parent(Status::Unknown);
        parent.operation = "hyperliquid.account.transfer".into();
        let profile = test_profile_with_parent(&[(12, status("open"))], Some(parent));
        assert!(
            profile
                .prepare_amend("hyperliquid.orders.edit", &request)
                .await
                .is_err()
        );

        let mut parent = durable_parent(Status::Submitted);
        parent.submission.as_mut().unwrap().data["envelope"]["nonce_ms"] = json!(6000);
        let profile = test_profile_with_parent(&[(12, status("open"))], Some(parent));
        assert!(
            profile
                .prepare_amend("hyperliquid.orders.edit", &request)
                .await
                .is_err()
        );
    }
}
