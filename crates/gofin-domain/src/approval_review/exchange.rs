//! Exchange-specific review fields come from the frozen broker binding.
use super::{ApprovalAccount, ApprovalAction, ApprovalLeg, ApprovalReview};
use crate::{GoFinError, PreparedOperation, Result};
use serde_json::Value;

fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        v.get(*key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
}
fn account(leg: &mut Option<ApprovalLeg>, name: Option<&str>, domain: &str) {
    if let Some(name) = name {
        leg.get_or_insert_with(ApprovalLeg::default).account = Some(ApprovalAccount {
            name: name.into(),
            domain: Some(domain.into()),
        });
    }
}

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let binding = &prepared.binding;
    let params = binding
        .get("parameters")
        .or_else(|| binding.get("params"))
        .unwrap_or(&Value::Null);
    let body = binding
        .get("requestBodyExact")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);
    let funding = binding.get("funding").unwrap_or(&Value::Null);
    let transfer = binding.get("transfer").unwrap_or(&Value::Null);
    let get = |keys: &[&str]| {
        text(binding, keys)
            .or_else(|| text(funding, keys))
            .or_else(|| text(transfer, keys))
            .or_else(|| text(&binding["action"], keys))
            .or_else(|| text(params, keys))
            .or_else(|| {
                params
                    .get("params")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        keys.iter().find_map(|key| {
                            items.iter().find_map(|item| {
                                (item.get("key").and_then(Value::as_str) == Some(*key))
                                    .then(|| item.get("value").and_then(Value::as_str))
                                    .flatten()
                                    .filter(|value| !value.is_empty())
                            })
                        })
                    })
            })
            .or_else(|| text(&body, keys))
    };
    let service = review.service.clone();
    review.market = get(&["symbol", "productId", "instrument", "pair", "market"])
        .or_else(|| {
            binding
                .pointer("/authority_before/native_coin")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            binding
                .pointer("/authority_before/market/symbol")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            binding
                .pointer("/authority_before/twap/symbol")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            binding
                .pointer("/authority_before/twap_cancel/market/symbol")
                .and_then(Value::as_str)
        })
        .or_else(|| text(&binding["action"], &["vault_address"]))
        .or_else(|| text(&binding["amendment"], &["symbol"]))
        .or_else(|| text(&binding["earn"], &["strategy"]))
        .map(str::to_owned);
    let source = get(&["from", "source_account", "account", "authority"])
        .or_else(|| binding.pointer("/account/name").and_then(Value::as_str));
    let target = get(&[
        "destinationName",
        "targetAccount",
        "destination",
        "to",
        "destination_account",
        "destinationAddress",
        "destination_recipient",
        "sub_account_user",
        "receiptAddress",
        "address",
    ]);
    account(&mut review.input, source, &service);
    if matches!(
        review.action,
        ApprovalAction::AccountTransfer
            | ApprovalAction::ExchangeWithdrawal
            | ApprovalAction::ExchangeDeposit
    ) {
        account(
            &mut review.output,
            target,
            get(&["network"]).unwrap_or(&service),
        );
    }
    if service == "hyperliquid"
        && matches!(
            binding["action"]["kind"].as_str(),
            Some("sub_account_transfer" | "sub_account_spot_transfer")
        )
    {
        let child = text(&binding["action"], &["sub_account_user"])
            .ok_or_else(|| GoFinError::validation("Subaccount transfer lacks child identity."))?;
        let parent = binding
            .pointer("/account/name")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Subaccount transfer lacks parent identity."))?;
        let to_child = binding["action"]["is_deposit"]
            .as_bool()
            .ok_or_else(|| GoFinError::validation("Subaccount transfer lacks direction."))?;
        account(
            &mut review.input,
            Some(if to_child { parent } else { child }),
            &service,
        );
        account(
            &mut review.output,
            Some(if to_child { child } else { parent }),
            &service,
        );
    }
    if service == "hyperliquid"
        && matches!(
            review.action,
            ApprovalAction::VaultDeposit | ApprovalAction::VaultWithdraw
        )
    {
        let vault = text(&binding["action"], &["vault_address"])
            .ok_or_else(|| GoFinError::validation("Vault transfer lacks vault identity."))?;
        let parent = binding
            .pointer("/account/name")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Vault transfer lacks account identity."))?;
        let deposit = matches!(review.action, ApprovalAction::VaultDeposit);
        account(
            &mut review.input,
            Some(if deposit { parent } else { vault }),
            &service,
        );
        account(
            &mut review.output,
            Some(if deposit { vault } else { parent }),
            &service,
        );
    }
    if matches!(review.action, ApprovalAction::ExchangeDeposit) {
        let wallet = text(binding, &["source_wallet"])
            .or_else(|| text(&prepared.input, &["wallet"]))
            .ok_or_else(|| GoFinError::validation("Deposit review lacks source wallet."))?;
        let destination = text(binding, &["account"])
            .or_else(|| text(&prepared.input, &["account"]))
            .ok_or_else(|| GoFinError::validation("Deposit review lacks exchange account."))?;
        let source_domain = prepared
            .intent
            .debits
            .first()
            .map(|d| super::display_domain(&d.amount.asset.network));
        review
            .input
            .get_or_insert_with(ApprovalLeg::default)
            .account = Some(ApprovalAccount {
            name: wallet.into(),
            domain: source_domain,
        });
        account(&mut review.output, Some(destination), &service);
    }
    let mut details = Vec::new();
    if matches!(review.action, ApprovalAction::ExchangeWithdrawal)
        && let Some(address) = get(&[
            "destinationAddress",
            "destination_recipient",
            "receiptAddress",
            "address",
        ])
        && target != Some(address)
    {
        details.push(format!("Address: {address}"));
    }
    if let Some(orders) = binding
        .get("orders")
        .and_then(Value::as_array)
        .filter(|orders| service == "hyperliquid" && !orders.is_empty())
    {
        let mut descriptions = Vec::with_capacity(orders.len());
        for order in orders {
            let input = order.get("input").unwrap_or(&Value::Null);
            let market = order
                .pointer("/market/symbol")
                .and_then(Value::as_str)
                .or_else(|| text(input, &["symbol"]))
                .ok_or_else(|| GoFinError::validation("An order in this review has no market."))?;
            let side = text(input, &["side"])
                .ok_or_else(|| GoFinError::validation("An order in this review has no side."))?;
            let size = text(input, &["size"])
                .ok_or_else(|| GoFinError::validation("An order in this review has no size."))?;
            let kind = text(input, &["order_type"])
                .ok_or_else(|| GoFinError::validation("An order in this review has no type."))?;
            let min = text(order, &["min_price"]).ok_or_else(|| {
                GoFinError::validation("An order in this review has no minimum price.")
            })?;
            let max = text(order, &["max_price"]).ok_or_else(|| {
                GoFinError::validation("An order in this review has no maximum price.")
            })?;
            let mut order_details = format!("{side} {size} {market} {kind} {min}–{max}");
            for (label, key) in [
                ("TIF", "time_in_force"),
                ("trigger", "trigger_price"),
                ("trigger kind", "trigger_kind"),
                ("group", "grouping"),
                ("max notional", "max_notional_usdc"),
                ("max fee", "max_fee_usdc"),
                ("max loss", "max_realized_loss_usdc"),
            ] {
                if let Some(value) = text(input, &[key]) {
                    order_details.push_str(&format!(" {label} {value}"));
                }
            }
            if input.get("trigger_is_market").and_then(Value::as_bool) == Some(true) {
                order_details.push_str(" trigger-market");
            }
            if let Some(builder) = input.get("builder_fee").filter(|v| !v.is_null()) {
                let address = text(builder, &["address"]).ok_or_else(|| {
                    GoFinError::validation("A builder fee requires an address in the review.")
                })?;
                let rate = builder
                    .get("fee_tenths_bps")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        GoFinError::validation("A builder fee requires a rate in the review.")
                    })?;
                order_details.push_str(&format!(" builder {address} {rate}/10bps"));
            }
            if input.get("reduce_only").and_then(Value::as_bool) == Some(true) {
                order_details.push_str(" reduce-only");
            }
            descriptions.push(order_details);
        }
        if orders.len() == 1 {
            review.market = orders[0]
                .pointer("/market/symbol")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        details.push(descriptions.join("; "));
    }
    if matches!(review.action, ApprovalAction::BatchOrders) && service == "krakenfutures" {
        let orders = params
            .get("batchOrder")
            .and_then(Value::as_array)
            .filter(|orders| !orders.is_empty())
            .ok_or_else(|| GoFinError::validation("Cannot show every derivative batch action."))?;
        let mut members = Vec::new();
        for order in orders {
            let kind = text(order, &["order"])
                .ok_or_else(|| GoFinError::validation("A derivative batch action has no type."))?;
            let mut fields = vec![kind.to_owned()];
            match kind {
                "send" => {
                    for keys in [
                        &["side"][..],
                        &["size"][..],
                        &["symbol"][..],
                        &["orderType"][..],
                    ] {
                        fields.push(
                            text(order, keys)
                                .ok_or_else(|| {
                                    GoFinError::validation(
                                        "A derivative batch order lacks a required term.",
                                    )
                                })?
                                .to_owned(),
                        );
                    }
                }
                "cancel" | "edit" => fields.push(
                    text(order, &["order_id", "orderId", "cliOrdId"])
                        .ok_or_else(|| {
                            GoFinError::validation(
                                "A derivative batch action lacks its order identity.",
                            )
                        })?
                        .to_owned(),
                ),
                _ => return Err(GoFinError::validation("Unknown derivative batch action.")),
            }
            for (label, key) in [
                ("size", "size"),
                ("price", "limitPrice"),
                ("stop", "stopPrice"),
                ("trigger", "triggerSignal"),
                ("trailing", "trailingStopMaxDeviation"),
                ("unit", "trailingStopDeviationUnit"),
                ("quantity mode", "qtyMode"),
            ] {
                if key == "size" && kind == "send" {
                    continue;
                }
                if let Some(value) = text(order, &[key]) {
                    fields.push(format!("{label} {value}"));
                }
            }
            if order.get("reduceOnly").and_then(Value::as_bool) == Some(true) {
                fields.push("reduce-only".into());
            }
            members.push(fields.join(" "));
        }
        details.push(members.join("; "));
    }
    if matches!(review.action, ApprovalAction::BatchOrders)
        && service != "hyperliquid"
        && service != "krakenfutures"
    {
        let orders = params
            .get("orders")
            .and_then(Value::as_array)
            .ok_or_else(|| GoFinError::validation("Cannot show every order in this batch."))?;
        let pair = text(params, &["pair", "symbol"]).unwrap_or("");
        let mut members = Vec::new();
        for order in orders {
            let side = text(order, &["type", "side"])
                .ok_or_else(|| GoFinError::validation("A batch order has no side."))?;
            let size = text(order, &["volume", "size", "quantity"])
                .ok_or_else(|| GoFinError::validation("A batch order has no size."))?;
            let kind = text(order, &["ordertype", "orderType"])
                .ok_or_else(|| GoFinError::validation("A batch order has no type."))?;
            let price = text(order, &["price", "limitPrice"]).unwrap_or("");
            members.push(format!("{side} {size} {pair} {kind} {price}"));
        }
        if members.is_empty() {
            return Err(GoFinError::validation("The batch contains no orders."));
        }
        details.push(members.join("; "));
    }
    if matches!(review.action, ApprovalAction::CancelOrder)
        && matches!(service.as_str(), "kraken" | "krakenfutures")
    {
        let targets = binding["targets"]
            .as_array()
            .filter(|targets| !targets.is_empty())
            .ok_or_else(|| GoFinError::validation("Cancellation review lacks frozen targets."))?;
        let mut members = Vec::new();
        for target in targets {
            let id = text(target, &["orderId"])
                .ok_or_else(|| GoFinError::validation("Cancellation target lacks its identity."))?;
            let market = text(target, &["symbol"]).unwrap_or("");
            members.push(if market.is_empty() {
                id.to_owned()
            } else {
                format!("{market} {id}")
            });
        }
        details.push(format!("Orders: {}", members.join("; ")));
    }
    if prepared.operation == "krakenfutures.edit-order-spring" {
        let size = text(&binding["amendment"], &["totalSize"])
            .ok_or_else(|| GoFinError::validation("Derivative edit lacks revised size."))?;
        let id = text(&binding["amendment"], &["orderId"])
            .ok_or_else(|| GoFinError::validation("Derivative edit lacks order identity."))?;
        details.push(format!("Order {id}; total size {size}"));
        if let Some(mode) = text(params, &["qtyMode"]) {
            details.push(format!("Quantity mode: {mode}"));
        }
        if let Some(size) = text(params, &["size"]) {
            details.push(format!("Requested size: {size}"));
        }
    }
    if prepared.operation == "kraken.cancel-withdrawal" {
        let id = text(params, &["refid"])
            .ok_or_else(|| GoFinError::validation("Withdrawal cancellation lacks reference."))?;
        details.push(format!("Withdrawal: {id}"));
    }
    if matches!(
        prepared.operation.as_str(),
        "kraken.claim-funding-deposit-address-v1" | "kraken.create-deposit-address-legacy"
    ) {
        let method = text(&binding["claim"], &["method_id"])
            .ok_or_else(|| GoFinError::validation("Deposit address review lacks its method."))?;
        details.push(format!("Deposit method: {method}"));
    }
    if prepared.operation == "coinbase.cancel-orders" {
        let orders = binding["orders"]
            .as_array()
            .filter(|orders| !orders.is_empty())
            .ok_or_else(|| GoFinError::validation("Cannot show every cancellation target."))?;
        let mut targets = Vec::new();
        for order in orders {
            let id = text(order, &["orderId"]).ok_or_else(|| {
                GoFinError::validation("Cancellation target lacks order identity.")
            })?;
            let market = text(order, &["productId"]).ok_or_else(|| {
                GoFinError::validation("Cancellation target lacks market identity.")
            })?;
            targets.push(format!("{id} {market}"));
        }
        details.push(format!("Orders: {}", targets.join("; ")));
    }
    if matches!(
        prepared.operation.as_str(),
        "binance.futures.leverage-set" | "binance.futures.margin-type-set"
    ) {
        let setting = text(binding, &["setting"])
            .ok_or_else(|| GoFinError::validation("Futures setting is missing."))?;
        let after = text(binding, &["after"])
            .ok_or_else(|| GoFinError::validation("Futures setting value is missing."))?;
        details.push(format!("{setting}: {after}"));
    }
    if matches!(review.action, ApprovalAction::ScheduleCancel) {
        if service == "hyperliquid" {
            let value = &binding["action"]["time_ms"];
            details.push(if value.is_null() {
                "Disable scheduled cancellation".into()
            } else if let Some(ms) = value.as_u64() {
                format!("Cancel at Unix {ms}ms")
            } else {
                return Err(GoFinError::validation(
                    "Scheduled cancellation time is unavailable.",
                ));
            });
        } else {
            let value = &params["timeout"];
            let timeout = value
                .as_u64()
                .map(|v| v.to_string())
                .or_else(|| value.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    GoFinError::validation("Scheduled cancellation timeout is unavailable.")
                })?;
            details.push(format!("Cancel all after {timeout}s; zero disables"));
        }
    }
    if service == "hyperliquid" {
        let action = &binding["action"];
        if let Some(cancels) = action["cancels"].as_array() {
            let originals = binding["authority_before"].as_array().ok_or_else(|| {
                GoFinError::validation("Cancellation review lacks the frozen order identities.")
            })?;
            if cancels.len() != originals.len() || cancels.is_empty() {
                return Err(GoFinError::validation(
                    "Cancellation review has incomplete targets.",
                ));
            }
            let mut targets = Vec::new();
            for (cancel, original) in cancels.iter().zip(originals) {
                let id = cancel["order_id"]
                    .as_u64()
                    .ok_or_else(|| GoFinError::validation("Cancellation lacks order identity."))?;
                if original["oid"].as_u64() != Some(id) {
                    return Err(GoFinError::validation(
                        "Cancellation identity does not match its market.",
                    ));
                }
                let coin = text(original, &["coin"])
                    .ok_or_else(|| GoFinError::validation("Cancellation lacks market identity."))?;
                targets.push(format!("{coin} #{id}"));
            }
            details.push(format!("Orders: {}", targets.join("; ")));
        }
        if let Some(modifies) = action["modifies"].as_array() {
            if modifies.len() != binding["orders"].as_array().map_or(0, Vec::len)
                || modifies.is_empty()
            {
                return Err(GoFinError::validation(
                    "Amendment review has incomplete order targets.",
                ));
            }
            let ids = modifies
                .iter()
                .map(|item| {
                    item["order_id"]
                        .as_u64()
                        .map(|id| format!("#{id}"))
                        .ok_or_else(|| {
                            GoFinError::validation("Amendment lacks its order identity.")
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            details.push(format!("Amend: {}", ids.join(", ")));
        }
        if let Some(id) = action["twap_id"].as_u64() {
            details.push(format!("TWAP #{id}"));
        }
        if let Some(weight) = action["weight"].as_u64() {
            details.push(format!("Request weight: {weight}"));
            if let Some(destination) = text(action, &["destination"]) {
                details.push(format!("Recipient: {destination}"));
            }
        }
        if action["kind"] == "twap_order" {
            let side = action["is_buy"]
                .as_bool()
                .ok_or_else(|| GoFinError::validation("TWAP lacks side."))?;
            let size = text(action, &["size"])
                .ok_or_else(|| GoFinError::validation("TWAP lacks size."))?;
            let minutes = action["duration_minutes"]
                .as_u64()
                .ok_or_else(|| GoFinError::validation("TWAP lacks duration."))?;
            details.push(format!(
                "{} {size}; {minutes}min{}{}",
                if side { "Buy" } else { "Sell" },
                if action["reduce_only"] == true {
                    "; reduce-only"
                } else {
                    ""
                },
                if action["randomize"] == true {
                    "; randomized"
                } else {
                    ""
                }
            ));
        }
    }
    if let Some(action) = binding.get("action") {
        if let Some(leverage) = action.get("leverage").and_then(Value::as_u64) {
            details.push(format!("Leverage: {leverage}x"));
        }
        if let Some(cross) = action.get("is_cross").and_then(Value::as_bool) {
            details.push(if cross {
                "Cross margin".into()
            } else {
                "Isolated margin".into()
            });
        }
    }
    for (label, keys) in [
        ("Side", &["side", "type"][..]),
        ("Order type", &["orderType", "ordertype"][..]),
        ("Limit", &["newPrice", "limitPrice", "price"][..]),
        ("Stop", &["stopPrice", "trigger_price"][..]),
        ("Trigger", &["triggerSignal"][..]),
        ("Trailing", &["trailingStopMaxDeviation"][..]),
        ("Trailing unit", &["trailingStopDeviationUnit"][..]),
        ("Leverage", &["leverage"][..]),
        ("Time in force", &["timeInForce", "timeinforce"][..]),
        ("Network", &["network"][..]),
        ("Memo", &["memo", "tag", "addressTag"][..]),
        ("Order", &["orderId", "order_id"][..]),
    ] {
        if matches!(review.action, ApprovalAction::ExchangeWithdrawal)
            && (label == "Side" || label == "Network")
        {
            continue;
        }
        let value = get(keys).or_else(|| {
            (label == "Order type" && service == "binance")
                .then(|| get(&["type"]))
                .flatten()
        });
        if let Some(value) = value {
            details.push(format!("{label}: {value}"));
        }
    }
    if let Some(existing) = review.other_details.take() {
        details.push(existing);
    }
    if !details.is_empty() {
        review.other_details = Some(details.join("; "));
    }
    if matches!(review.action, ApprovalAction::ExchangeWithdrawal) && target.is_none() {
        return Err(GoFinError::validation(
            "Cannot show withdrawal destination in the approval review.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Amount, AssetId, FinancialIntent, Quantity, ResourceAmount, ResourceId};
    use serde_json::json;

    fn prepared(operation: &str, binding: Value) -> PreparedOperation {
        PreparedOperation {
            operation: operation.into(),
            input: Value::Null,
            binding,
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: ResourceId("source".into()),
                    amount: Amount {
                        asset: AssetId {
                            network: "kraken".into(),
                            reference: "USDC".into(),
                            decimals: 6,
                        },
                        atoms: Quantity::new("1000000").unwrap(),
                    },
                }],
                ..Default::default()
            },
        }
    }
    #[test]
    fn kraken_cancel_all_lists_frozen_targets() {
        let p = prepared(
            "kraken.cancel-all-orders",
            json!({"account":"desk","targets":[{"orderId":"A"},{"orderId":"B"}]}),
        );
        assert!(
            crate::approval_review::approval_review(&p)
                .unwrap()
                .other_details
                .unwrap()
                .contains("Orders: A; B")
        );
    }
    #[test]
    fn kraken_funding_controls_keep_strategy_and_reference() {
        let cancel = prepared(
            "kraken.cancel-withdrawal",
            json!({"account":"desk","parameters":{"refid":"W123"}}),
        );
        assert!(
            crate::approval_review::approval_review(&cancel)
                .unwrap()
                .other_details
                .unwrap()
                .contains("W123")
        );
        let claim = prepared(
            "kraken.claim-funding-deposit-address-v1",
            json!({"account":"desk","claim":{"method_id":"ethereum"}}),
        );
        assert!(
            crate::approval_review::approval_review(&claim)
                .unwrap()
                .other_details
                .unwrap()
                .contains("ethereum")
        );
        let earn = prepared(
            "kraken.allocate-strategy",
            json!({"account":"desk","earn":{"strategy":"strategy-123"}}),
        );
        assert_eq!(
            crate::approval_review::approval_review(&earn)
                .unwrap()
                .market
                .as_deref(),
            Some("strategy-123")
        );
    }
    #[test]
    fn kraken_nested_withdrawal_retains_network_destination_and_fee() {
        let p = prepared(
            "kraken.withdraw-funds",
            json!({"account":"desk", "funding":{"network":"ethereum", "address":"0x123", "memo":"42"}}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.input.unwrap().account.unwrap().name, "desk");
        let target = r.output.unwrap().account.unwrap();
        assert_eq!(target.name, "0x123");
        assert_eq!(target.domain.as_deref(), Some("ethereum"));
        assert!(r.other_details.unwrap().contains("Memo: 42"));
    }
    #[test]
    fn hyperliquid_frozen_order_scope_shows_every_order_bound() {
        let p = prepared(
            "hyperliquid.orders.batch",
            json!({"account":{"name":"desk"}, "action":{"type":"order"}, "orders":[{"input":{"account":"desk","symbol":"BTC/USDC","side":"buy","order_type":"limit","size":"0.01","time_in_force":"gtc","max_notional_usdc":"1000","max_fee_usdc":"1","reduce_only":true},"market":{"symbol":"BTC/USDC"},"min_price":"80000","max_price":"90000"}]}),
        );
        let review = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(review.market.as_deref(), Some("BTC/USDC"));
        assert_eq!(review.input.unwrap().account.unwrap().name, "desk");
        let details = review.other_details.unwrap();
        assert!(details.contains("buy 0.01 BTC/USDC limit 80000–90000"));
        assert!(details.contains("max fee 1"));
        assert!(details.contains("reduce-only"));
    }
    #[test]
    fn hyperliquid_cancellation_displays_every_frozen_market_and_order() {
        let p = prepared(
            "hyperliquid.orders.cancel_batch",
            json!({"account":{"name":"desk"},"action":{"kind":"cancel","cancels":[{"asset":0,"order_id":12},{"asset":1,"order_id":34}]},"authority_before":[{"oid":12,"coin":"BTC"},{"oid":34,"coin":"ETH"}],"orders":[]}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert!(r.other_details.unwrap().contains("BTC #12; ETH #34"));
    }
    #[test]
    fn hyperliquid_child_transfer_preserves_both_directions() {
        for to_child in [true, false] {
            let p = prepared(
                "hyperliquid.account.transfer",
                json!({"account":{"name":"parent"},"action":{"kind":"sub_account_transfer","sub_account_user":"child","is_deposit":to_child},"orders":[]}),
            );
            let r = crate::approval_review::approval_review(&p).unwrap();
            assert_eq!(
                r.input.unwrap().account.unwrap().name,
                if to_child { "parent" } else { "child" }
            );
            assert_eq!(
                r.output.unwrap().account.unwrap().name,
                if to_child { "child" } else { "parent" }
            );
        }
    }
    #[test]
    fn hyperliquid_withdrawal_reads_destination_from_action() {
        let p = prepared(
            "hyperliquid.account.withdraw",
            json!({"account":{"name":"desk"},"action":{"kind":"withdraw","destination":"0x123","amount_usdc":"1"},"orders":[]}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.output.unwrap().account.unwrap().name, "0x123");
    }
    #[test]
    fn hyperliquid_isolated_margin_retains_frozen_market() {
        for operation in [
            "hyperliquid.account.add_margin",
            "hyperliquid.account.reduce_margin",
        ] {
            let p = prepared(
                operation,
                json!({"account":{"name":"desk"},"authority_before":{"market":{"symbol":"BTC/USDC:USDC"}},"action":{"kind":"update_isolated_margin","asset":0,"amount_usdc":"1000000"},"orders":[]}),
            );
            assert_eq!(
                crate::approval_review::approval_review(&p)
                    .unwrap()
                    .market
                    .as_deref(),
                Some("BTC/USDC:USDC")
            );
        }
    }
    #[test]
    fn hyperliquid_leverage_uses_frozen_market_and_action() {
        let p = prepared(
            "hyperliquid.account.set_leverage",
            json!({"account":{"name":"desk"},"authority_before":{"asset":0,"native_coin":"BTC"},"action":{"kind":"update_leverage","asset":0,"leverage":3,"is_cross":true},"orders":[]}),
        );
        let review = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(review.market.as_deref(), Some("BTC"));
        let details = review.other_details.unwrap();
        assert!(details.contains("Leverage: 3x"));
        assert!(details.contains("Cross margin"));
    }
    #[test]
    fn coinbase_cancel_batch_and_edit_keep_targets_and_new_price() {
        let cancel = prepared(
            "coinbase.cancel-orders",
            json!({"account":"desk","orders":[{"orderId":"one","productId":"BTC-USD"},{"orderId":"two","productId":"ETH-USD"}]}),
        );
        let details = crate::approval_review::approval_review(&cancel)
            .unwrap()
            .other_details
            .unwrap();
        assert!(details.contains("one BTC-USD; two ETH-USD"));
        let edit = prepared(
            "coinbase.edit-order",
            json!({"account":"desk","productId":"BTC-USD","orderId":"one","newPrice":"120"}),
        );
        assert!(
            crate::approval_review::approval_review(&edit)
                .unwrap()
                .other_details
                .unwrap()
                .contains("Limit: 120")
        );
    }
    #[test]
    fn kraken_futures_edit_retains_revised_size_and_quantity_mode() {
        let p = prepared(
            "krakenfutures.edit-order-spring",
            json!({"account":"futures","parameters":{"size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000"},"amendment":{"symbol":"PF_XBTUSD","orderId":"ORDER-1","totalSize":"0.03"}}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.market.as_deref(), Some("PF_XBTUSD"));
        let details = r.other_details.unwrap();
        assert!(details.contains("total size 0.03"));
        assert!(details.contains("Quantity mode: RELATIVE"));
    }
    #[test]
    fn kraken_futures_batch_displays_mixed_native_actions() {
        let p = prepared(
            "krakenfutures.send-batch-order",
            json!({"account":"futures","parameters":{"batchOrder":[{"order":"cancel","order_id":"OLD-ORDER"},{"order":"send","symbol":"PF_ETHUSD","orderType":"ioc","size":"0.01","limitPrice":"2000","side":"buy"}]}}),
        );
        let review = crate::approval_review::approval_review(&p).unwrap();
        let details = review.other_details.unwrap();
        assert!(details.contains("cancel OLD-ORDER"));
        assert!(details.contains("send buy 0.01 PF_ETHUSD ioc price 2000"));
    }
    #[test]
    fn kraken_batch_lists_every_native_order() {
        let p = prepared(
            "kraken.add-order-batch",
            json!({"account":"desk","parameters":{"pair":"ETHUSD","orders":[{"type":"buy","volume":"1","ordertype":"limit","price":"100"},{"type":"buy","volume":"2","ordertype":"limit","price":"90"}]}}),
        );
        let review = crate::approval_review::approval_review(&p).unwrap();
        let details = review.other_details.unwrap();
        assert!(details.contains("buy 1 ETHUSD limit 100"));
        assert!(details.contains("buy 2 ETHUSD limit 90"));
    }
    #[test]
    fn coinbase_withdrawal_discloses_uncapped_fee_and_exact_destination() {
        use crate::{AccountingLocation, CostBound, CostTreatment};
        let mut p = prepared(
            "coinbase.withdraw",
            json!({"account":"desk","destinationName":"vault","destinationAddress":"0x123","network":"ethereum","feeMode":"venue_calculated"}),
        );
        p.intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("source".into()),
            },
            amount: p.intent.debits[0].amount.clone(),
            treatment: CostTreatment::VenueCalculatedAdditionalDebit,
        });
        let review = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(review.output.unwrap().account.unwrap().name, "vault");
        let details = review.other_details.unwrap();
        assert!(details.contains("Address: 0x123"));
        assert!(details.contains("Venue fee not capped; reserve: 1 USDC"));
        assert!(!details.contains("Maximum cost"));
    }
    #[test]
    fn binance_futures_setting_shows_requested_value() {
        for (operation, setting, after) in [
            ("binance.futures.margin-type-set", "marginType", "CROSSED"),
            ("binance.futures.leverage-set", "leverage", "3"),
        ] {
            let p = prepared(
                operation,
                json!({"account":"desk","symbol":"ETHUSDT","setting":setting,"after":after}),
            );
            let details = crate::approval_review::approval_review(&p)
                .unwrap()
                .other_details
                .unwrap();
            assert!(details.contains(&format!("{setting}: {after}")));
        }
    }
    #[test]
    fn binance_spot_request_displays_serialized_order_terms() {
        let p = prepared(
            "binance.spot-order",
            json!({"account":"desk", "params":{"endpoint":"SpotOrder", "params":[{"key":"symbol","value":"ETHUSDT"},{"key":"side","value":"BUY"},{"key":"type","value":"LIMIT"},{"key":"price","value":"2000"},{"key":"timeInForce","value":"IOC"}]}}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.market.as_deref(), Some("ETHUSDT"));
        let details = r.other_details.unwrap();
        assert!(details.contains("Side: BUY"));
        assert!(details.contains("Order type: LIMIT"));
        assert!(details.contains("Limit: 2000"));
        assert!(details.contains("Time in force: IOC"));
    }
    #[test]
    fn binance_withdrawal_displays_frozen_destination_and_address() {
        let p = prepared(
            "binance.withdraw",
            json!({"account":"desk","destination":"vault","receiptAddress":"0x123","network":"ethereum"}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.output.unwrap().account.unwrap().name, "vault");
        assert!(r.other_details.unwrap().contains("Address: 0x123"));
    }
    #[test]
    fn coinbase_portfolio_transfer_uses_enrolled_target_name() {
        let p = prepared(
            "coinbase.move-portfolio-funds",
            json!({"account":"source", "targetAccount":"target"}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.input.unwrap().account.unwrap().name, "source");
        assert_eq!(r.output.unwrap().account.unwrap().name, "target");
    }
    #[test]
    fn deposit_uses_source_wallet_and_exchange_account() {
        let mut p = prepared(
            "coinbase.deposit",
            json!({"account":"desk", "address":"0x123"}),
        );
        p.input = json!({"wallet":"cold-wallet","account":"desk"});
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.input.unwrap().account.unwrap().name, "cold-wallet");
        assert_eq!(r.output.unwrap().account.unwrap().name, "desk");
    }
    #[test]
    fn binance_transfer_preserves_both_account_names() {
        let p = prepared(
            "binance.transfer",
            json!({"from":"sub", "to":"main", "coin":"USDC", "amount":"1"}),
        );
        let r = crate::approval_review::approval_review(&p).unwrap();
        assert_eq!(r.input.unwrap().account.unwrap().name, "sub");
        assert_eq!(r.output.unwrap().account.unwrap().name, "main");
    }
}
