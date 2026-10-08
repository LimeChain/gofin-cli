//! Documented public address-book reads. No address/whitelist mutation endpoint exists here.
use super::*;

const MAX_ADDRESSES: usize = 1000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WhitelistInput {
    account: String,
    coin: Option<String>,
    network: Option<String>,
    /// Optional exact destination to check; requires coin and network.
    address: Option<String>,
    /// Exact memo/tag; omission means an empty tag, not any tag.
    memo: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DepositEntry {
    coin: String,
    address: String,
    tag: String,
    is_default: u8,
}

pub(super) fn definitions() -> Vec<Definition> {
    [
        ("binance.deposit-addresses", "List the selected account's own Binance deposit addresses for an explicit coin/network using that account's credential, including a subaccount-only connection. Creating an additional address requires Binance's website/app.", serde_json::to_value(schema_for!(DepositAddressInput)).unwrap(), json!({"account":"master","coin":"ETH","network":"ETH"}), "Use Binance Deposit Crypto to claim a new address where available, then repeat this read."),
        ("binance.whitelist", "Inspect withdrawal-address whitelist flags, optionally checking an exact coin/network/address/memo. Editing requires Binance's website/app.", serde_json::to_value(schema_for!(WhitelistInput)).unwrap(), json!({"account":"master","coin":"ETH","network":"ETH","address":"0x1111111111111111111111111111111111111111","memo":""}), "Use Binance withdrawal Address Management to edit entries and complete its security checks, then repeat this read."),
    ].into_iter().map(|(name, description, input_schema, example, human_action)| Definition {
        name: name.into(), description: description.into(), kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable, input_schema: input_schema_with_constraints(name, input_schema),
        output_schema: output_schema(name), error_schema: schema_for!(FinError).into(), examples:vec![example],
        coverage: "Binance own-account GET routes use the selected connection's credential; no parent/master lookup is used. If Binance denies a specific route, its response is reported for that capability only. Synthetic tests; authenticated live observations pending.".into(),
        completion: "Bounded address-book observation only; no address creation, whitelist modification, account-wide enforcement or withdrawal eligibility proof.".into(),
        semantics:json!({"financial_movement":false,"editing_supported":false,"human_action":human_action,"next_check":format!("fin read {name} --input JSON --json"),"limits":"Per-entry whitelist flags do not prove account-wide enforcement or that a security cooldown has elapsed.","maximum_rows":MAX_ADDRESSES}),
    }).collect()
}

fn input_schema_with_constraints(operation: &str, mut schema: Value) -> Value {
    for key in ["coin", "network"] {
        if operation == "binance.whitelist" {
            schema["properties"][key] = json!({"anyOf":[{"type":"string","minLength":1,"maxLength":40,"pattern":"^[A-Z0-9_-]+$"},{"type":"null"}]});
        } else {
            schema["properties"][key] =
                json!({"type":"string","minLength":1,"maxLength":40,"pattern":"^[A-Z0-9_-]+$"});
        }
    }
    if operation == "binance.whitelist" {
        schema["properties"]["address"] =
            json!({"anyOf":[{"type":"string","minLength":1,"maxLength":256},{"type":"null"}]});
        schema["properties"]["memo"] =
            json!({"anyOf":[{"type":"string","maxLength":256},{"type":"null"}]});
        schema["allOf"] = json!([
            {"if":{"required":["address"],"properties":{"address":{"type":"string"}}},"then":{"required":["coin","network"],"properties":{"coin":{"type":"string"},"network":{"type":"string"}}}},
            {"if":{"required":["memo"],"properties":{"memo":{"type":"string"}}},"then":{"required":["address"],"properties":{"address":{"type":"string"}}}}
        ]);
    }
    schema
}

fn output_schema(operation: &str) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false,"required":["operation","account","observedAt","humanAction"],"properties":{"operation":{"const":operation},"account":{"type":"string"},"observedAt":{"type":"string"},"humanAction":{"type":"string"}}});
    if operation == "binance.deposit-addresses" {
        schema["required"] = json!([
            "operation",
            "account",
            "coin",
            "network",
            "observedAt",
            "addresses",
            "creationSupported",
            "humanAction",
            "creditProof"
        ]);
        for key in ["coin", "network", "creditProof"] {
            schema["properties"][key] = json!({"type":"string"});
        }
        schema["properties"]["creationSupported"] = json!({"const":false});
        schema["properties"]["addresses"] = json!({"type":"array","maxItems":MAX_ADDRESSES,"items":{"type":"object","additionalProperties":false,"required":["coin","address","tag","isDefault"],"properties":{"coin":{"type":"string"},"address":{"type":"string"},"tag":{"type":"string"},"isDefault":{"type":"integer","enum":[0,1]}}}});
    } else {
        schema["required"] = json!([
            "operation",
            "account",
            "observedAt",
            "entries",
            "matchStatus",
            "accountWhitelistEnforcement",
            "activationStatus",
            "editingSupported",
            "humanAction"
        ]);
        schema["properties"]["matchStatus"] =
            json!({"enum":["not_requested","absent","enabled","disabled","unknown"]});
        for key in ["accountWhitelistEnforcement", "activationStatus"] {
            schema["properties"][key] = json!({"const":"unknown"});
        }
        schema["properties"]["editingSupported"] = json!({"const":false});
        let mut row = json!({"type":"object","additionalProperties":false,"required":["address","whiteStatus"],"properties":{"address":{"type":"string"},"whiteStatus":{"enum":["enabled","disabled","unknown"]},"originType":{"type":["string","integer","null"]}}});
        for key in ["addressTag", "coin", "name", "network", "origin"] {
            row["properties"][key] = json!({"type":["string","null"]});
        }
        schema["properties"]["entries"] =
            json!({"type":"array","maxItems":MAX_ADDRESSES,"items":row});
    }
    schema
}

fn selector(value: &str) -> Result<(), FinError> {
    if value.is_empty()
        || value.len() > 40
        || !value
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || b"_-".contains(&c))
    {
        return Err(FinError::validation(
            "Use the exact uppercase Binance coin/network identifier.",
        ));
    }
    Ok(())
}

fn same_address(network: Option<&str>, a: &str, b: &str) -> bool {
    super::withdrawal::same_address(network.unwrap_or_default(), a, b)
}

impl BinanceProfile {
    pub(super) async fn read_address_book(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<Value, FinError> {
        if operation == "binance.deposit-addresses" {
            let input: DepositAddressInput = self.parse_input(raw).await?;
            selector(&input.coin)?;
            selector(&input.network)?;
            let account = self.account(&input.account)?;
            let currency = self.currency(account, &input.coin).await?;
            let network = self.network(&currency, &input.network)?;
            if currency.deposit_all_enable != Some(true) || network.deposit_enable != Some(true) {
                return Err(error(
                    "DEPOSIT_DISABLED",
                    "read",
                    "Selected Binance deposit route is unavailable.",
                ));
            }
            let response = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::DepositAddressList)
                        .push("coin", &input.coin)
                        .push("network", &input.network),
                    "read",
                )
                .await?;
            let rows: Vec<DepositEntry> = parse_response(response).map_err(|_| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "read",
                    "Binance deposit address list was invalid.",
                )
            })?;
            if rows.len() > MAX_ADDRESSES {
                return Err(error(
                    "ADDRESS_BOOK_TOO_LARGE",
                    "read",
                    "The address book exceeds this observer's bound; no complete list was established.",
                ));
            }
            let mut seen = BTreeSet::new();
            for row in &rows {
                if row.coin != input.coin
                    || row.is_default > 1
                    || row.address.is_empty()
                    || (input.network == "ETH" && !valid_eth_address(&row.address))
                    || !seen.insert((
                        if input.network == "ETH" {
                            row.address.to_ascii_lowercase()
                        } else {
                            row.address.clone()
                        },
                        row.tag.clone(),
                    ))
                {
                    return Err(error(
                        "VENUE_RESPONSE_INVALID",
                        "read",
                        "Deposit addresses have invalid or ambiguous asset/address/default identities.",
                    ));
                }
            }
            return Ok(
                json!({"operation":operation,"account":account.name,"coin":input.coin,"network":input.network,"observedAt":now(),"addresses":rows,"creationSupported":false,"humanAction":"Use Binance Deposit Crypto to claim a new address where available; then repeat this read.","creditProof":"A deposit address is not a deposit credit; reconcile Binance deposit history."}),
            );
        }
        let input: WhitelistInput = self.parse_input(raw).await?;
        if input.address.is_some() && (input.coin.is_none() || input.network.is_none())
            || input.memo.is_some() && input.address.is_none()
        {
            return Err(FinError::validation(
                "Checking an address requires coin and network; memo is only used with an address.",
            ));
        }
        for value in [&input.coin, &input.network].into_iter().flatten() {
            selector(value)?;
        }
        if input
            .address
            .as_deref()
            .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
            || input
                .memo
                .as_deref()
                .is_some_and(|v| v.len() > 256 || v.chars().any(char::is_control))
            || (input.network.as_deref() == Some("ETH")
                && input
                    .address
                    .as_deref()
                    .is_some_and(|v| !valid_eth_address(v)))
        {
            return Err(FinError::validation(
                "Use an exact valid destination address and memo/tag.",
            ));
        }
        let account = self.account(&input.account)?;
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::WithdrawAddressList),
                "read",
            )
            .await?;
        let rows: Vec<WithdrawAddressListEntry> = parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance withdrawal address list was invalid.",
            )
        })?;
        if rows.len() > MAX_ADDRESSES {
            return Err(error(
                "ADDRESS_BOOK_TOO_LARGE",
                "read",
                "The address book exceeds this observer's bound; no complete whitelist observation was established.",
            ));
        }
        if rows.iter().any(|r| r.address.is_empty()) {
            return Err(error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance returned an empty withdrawal address.",
            ));
        }
        // Missing asset/network metadata remains a possible match with unknown status.
        // It must never turn into either an enabled entry or a definite absence.
        let rows: Vec<_> =
            rows.into_iter()
                .filter(|row| {
                    !input
                        .coin
                        .as_ref()
                        .is_some_and(|coin| row.coin.as_ref().is_some_and(|v| v != coin))
                        && !input.network.as_ref().is_some_and(|network| {
                            row.network.as_ref().is_some_and(|v| v != network)
                        })
                        && !input.address.as_ref().is_some_and(|address| {
                            !same_address(input.network.as_deref(), address, &row.address)
                                || row
                                    .address_tag
                                    .as_deref()
                                    .is_some_and(|tag| tag != input.memo.as_deref().unwrap_or(""))
                        })
                })
                .collect();
        // A null/missing tag can mean no memo on a tagless route, but only
        // venue metadata for this exact coin/network can establish that.
        // Retain the original row; this inference affects matching only.
        let missing_tag_is_empty = if input.address.is_some()
            && input.memo.as_deref().unwrap_or("").is_empty()
            && matches!(rows.as_slice(), [row]
                if row.coin == input.coin && row.network == input.network
                    && row.address_tag.is_none()
                    && row.whitelist_status != WithdrawWhitelistStatus::Unknown)
        {
            self.capital_config(account)
                .await
                .ok()
                .is_some_and(|currencies| {
                    let mut routes = currencies
                        .iter()
                        .filter(|currency| Some(&currency.coin) == input.coin.as_ref())
                        .flat_map(|currency| &currency.network_list)
                        .filter(|route| {
                            Some(&route.coin) == input.coin.as_ref()
                                && Some(&route.network) == input.network.as_ref()
                        });
                    routes
                        .next()
                        .is_some_and(|route| route.withdraw_tag == Some(false))
                        && routes.next().is_none()
                })
        } else {
            false
        };
        let match_status = if input.address.is_none() {
            "not_requested"
        } else {
            match rows.as_slice() {
                [] => "absent",
                [row]
                    if row.coin == input.coin
                        && row.network == input.network
                        && (row.address_tag.is_some() || missing_tag_is_empty) =>
                {
                    match row.whitelist_status {
                        WithdrawWhitelistStatus::Enabled => "enabled",
                        WithdrawWhitelistStatus::Disabled => "disabled",
                        WithdrawWhitelistStatus::Unknown => "unknown",
                    }
                }
                _ => "unknown",
            }
        };
        Ok(
            json!({"operation":operation,"account":account.name,"observedAt":now(),"entries":rows,"matchStatus":match_status,"accountWhitelistEnforcement":"unknown","activationStatus":"unknown","editingSupported":false,"humanAction":"Use Binance withdrawal Address Management to edit entries and complete its security checks; then repeat this read."}),
        )
    }
}
