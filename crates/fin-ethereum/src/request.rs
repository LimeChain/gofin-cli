use fin_domain::{Definition, ExecutionActor, FinError, OperationKind, Result};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub wallet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_debt_usd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_debt_to_collateral_bps: Option<u16>,
    pub network: String,
    pub chain_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fee_eth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_out: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_tier: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_spacing: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pool_fee_ppm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v4_pool_key: Option<super::V4PoolKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price_impact_bps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_health_factor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
pub const OPERATIONS: &[&str] = &[
    "wallet.balances",
    "wallet.token",
    "wallet.allowance",
    "wallet.permit2.allowance",
    "wallet.receipt",
    "uniswap.quote",
    "ramses.quote",
    "sushi.quote",
    "pancakeswap.quote",
    "aave.markets",
    "aave.reserves",
    "aave.positions",
    "wallet.transfer",
    "wallet.wrap",
    "wallet.unwrap",
    "wallet.approve",
    "wallet.permit2.approve",
    "uniswap.swap",
    "ramses.swap",
    "sushi.swap",
    "pancakeswap.swap",
    "aave.supply",
    "aave.borrow",
    "aave.repay",
    "aave.withdraw",
    "aave.collateral",
];
pub fn is_read(operation: &str) -> bool {
    matches!(
        operation,
        "wallet.balances"
            | "wallet.token"
            | "wallet.allowance"
            | "wallet.permit2.allowance"
            | "wallet.receipt"
            | "uniswap.quote"
            | "ramses.quote"
            | "sushi.quote"
            | "pancakeswap.quote"
            | "aave.markets"
            | "aave.reserves"
            | "aave.positions"
    )
}
fn extras(operation: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match operation {
        "aave.markets" => (&[], &[]),
        "wallet.balances" => (&["tokens"], &[]),
        "aave.reserves" | "aave.positions" => (&["tokens", "block_number"], &[]),
        "wallet.token" => (&["token"], &["token"]),
        "wallet.allowance" => (&["token", "spender"], &["token", "spender"]),
        "wallet.permit2.allowance" => (&["token", "spender"], &["token", "spender"]),
        "wallet.receipt" => (&["transaction_hash"], &["transaction_hash"]),
        "uniswap.quote" | "sushi.quote" | "pancakeswap.quote" => (
            &[
                "token_in",
                "token_out",
                "amount",
                "fee_tier",
                "protocol",
                "v4_pool_key",
            ],
            &["token_in", "token_out", "amount"],
        ),
        "ramses.quote" => (
            &["token_in", "token_out", "amount", "tick_spacing"],
            &["token_in", "token_out", "amount", "tick_spacing"],
        ),
        "wallet.transfer" => (
            &["token", "amount", "recipient"],
            &["token", "amount", "recipient"],
        ),
        "aave.withdraw" => (
            &[
                "token",
                "amount",
                "recipient",
                "max_amount",
                "min_health_factor",
            ],
            &["token", "amount", "recipient"],
        ),
        "wallet.wrap" => (&["amount"], &["amount"]),
        "wallet.unwrap" => (&["amount", "max_amount"], &["amount"]),
        "wallet.approve" => (
            &["token", "spender", "amount"],
            &["token", "spender", "amount"],
        ),
        "wallet.permit2.approve" => (
            &["token", "spender", "amount", "expiration"],
            &["token", "spender", "amount", "expiration"],
        ),
        "uniswap.swap" | "sushi.swap" | "pancakeswap.swap" => (
            &[
                "token_in",
                "token_out",
                "amount",
                "max_amount",
                "fee_tier",
                "recipient",
                "min_output",
                "slippage_bps",
                "deadline",
                "protocol",
                "max_price_impact_bps",
                "v4_pool_key",
            ],
            &[
                "token_in",
                "token_out",
                "amount",
                "recipient",
                "min_output",
                "slippage_bps",
                "deadline",
            ],
        ),
        "ramses.swap" => (
            &[
                "token_in",
                "token_out",
                "amount",
                "max_amount",
                "tick_spacing",
                "max_pool_fee_ppm",
                "recipient",
                "min_output",
                "slippage_bps",
                "deadline",
                "max_price_impact_bps",
            ],
            &[
                "token_in",
                "token_out",
                "amount",
                "recipient",
                "min_output",
                "slippage_bps",
                "deadline",
                "tick_spacing",
            ],
        ),
        "aave.supply" => (&["token", "amount"], &["token", "amount"]),
        "aave.borrow" => (
            &[
                "token",
                "amount",
                "min_health_factor",
                "max_debt_usd",
                "max_debt_to_collateral_bps",
            ],
            &["token", "amount", "min_health_factor"],
        ),
        "aave.repay" => (&["token", "amount", "max_amount"], &["token", "amount"]),
        "aave.collateral" => (
            &["token", "enabled", "min_health_factor"],
            &["token", "enabled"],
        ),
        _ => (&[], &[]),
    }
}
pub fn parse(operation: &str, input: &Value) -> Result<Request> {
    if !OPERATIONS.contains(&operation) {
        return Err(FinError::validation(
            "This EVM operation is not implemented.",
        ));
    }
    let object = input
        .as_object()
        .ok_or_else(|| FinError::validation("EVM input must be an object."))?;
    let (allowed, required) = extras(operation);
    for key in object.keys() {
        if !["wallet", "network", "chain_id"].contains(&key.as_str())
            && !(operation.starts_with("aave.") && key == "market")
            && !allowed.contains(&key.as_str())
            && !(!is_read(operation) && ["request_id", "max_fee_eth"].contains(&key.as_str()))
        {
            return Err(FinError::validation(
                "Input includes a field outside this operation's exact schema.",
            ));
        }
    }
    for field in required {
        if object.get(*field).is_none_or(Value::is_null) {
            return Err(FinError::validation(format!(
                "Required operation field {field} is missing."
            )));
        }
    }
    let request: Request = serde_json::from_value(input.clone())
        .map_err(|_| FinError::validation("EVM input has invalid field types."))?;
    if matches!(operation, "uniswap.quote" | "uniswap.swap") {
        let protocol = request.protocol.as_deref().unwrap_or("v3");
        if (protocol == "v4") != request.v4_pool_key.is_some() {
            return Err(FinError::validation(
                "Uniswap v4 requires one explicit v4_pool_key; v3 requests must omit it.",
            ));
        }
        if protocol == "v4" && request.fee_tier.is_some() {
            return Err(FinError::validation(
                "Uniswap v4 fee is fixed by its explicit v4_pool_key; omit fee_tier.",
            ));
        }
    }
    if let Some(dex) = super::uniswap::profile::additional_dex(operation)
        && (request.protocol.as_deref().is_some_and(|p| p != "v3")
            || request.v4_pool_key.is_some()
            || request
                .fee_tier
                .is_some_and(|fee| !dex.fee_tiers().contains(&fee)))
    {
        return Err(FinError::validation(
            "This venue requires v3 and its reviewed fee tiers; v4 keys are unsupported.",
        ));
    }
    if matches!(operation, "ramses.quote" | "ramses.swap")
        && (request
            .protocol
            .as_deref()
            .is_some_and(|value| value != "v3")
            || request.v4_pool_key.is_some()
            || request.fee_tier.is_some()
            || request
                .tick_spacing
                .is_some_and(|spacing| ![1, 5, 10, 50, 100, 200].contains(&spacing))
            || request.max_pool_fee_ppm.is_some_and(|fee| fee >= 1_000_000))
    {
        return Err(FinError::validation(
            "Ramses requires a reviewed v3 tick spacing and does not accept Uniswap fee tiers or v4 keys.",
        ));
    }
    if request.wallet.is_empty()
        || request.wallet.len() > 80
        || !matches!(request.network.as_str(), "evm" | "ethereum")
        || request.chain_id == 0
    {
        return Err(FinError::validation(
            "Use a named wallet and explicit EVM chain identity.",
        ));
    }
    if operation == "wallet.permit2.approve"
        && request
            .expiration
            .is_none_or(|value| value > (1u64 << 48) - 1)
    {
        return Err(FinError::validation(
            "Permit2 approval requires an expiration within uint48.",
        ));
    }
    let supported_fee_tiers = super::uniswap::profile::additional_dex(operation)
        .map(super::V3Dex::fee_tiers)
        .unwrap_or(&[100, 500, 3000, 10000]);
    if request
        .tokens
        .as_ref()
        .is_some_and(|tokens| tokens.len() > 64)
        || request.slippage_bps.is_some_and(|n| n > 1000)
        || request
            .fee_tier
            .is_some_and(|n| !supported_fee_tiers.contains(&n))
        || request
            .protocol
            .as_deref()
            .is_some_and(|p| !matches!(p, "v3" | "v4"))
        || request.max_price_impact_bps.is_some_and(|n| n > 10_000)
    {
        return Err(FinError::validation(
            "Token list, fee tier or slippage exceeds its reviewed bound.",
        ));
    }
    if !is_read(operation)
        && (request
            .request_id
            .as_ref()
            .is_none_or(|id| id.is_empty() || id.len() > 128)
            || request.max_fee_eth.is_none())
    {
        return Err(FinError::validation(
            "Writes require a stable request_id and exact max_fee_eth.",
        ));
    }
    let all_supported = matches!(
        operation,
        "aave.repay"
            | "aave.withdraw"
            | "wallet.unwrap"
            | "uniswap.swap"
            | "ramses.swap"
            | "sushi.swap"
            | "pancakeswap.swap"
    );
    if request.amount.as_deref() == Some("all") && (!all_supported || request.max_amount.is_none())
    {
        return Err(FinError::validation(
            "Only supported full-balance operations accept all, and require finite max_amount.",
        ));
    }
    if request.block_number == Some(0) {
        return Err(FinError::validation(
            "Historical block_number must be positive.",
        ));
    }
    if request.max_amount.is_some() && request.amount.as_deref() != Some("all") {
        return Err(FinError::validation(
            "max_amount is valid only with amount all; exact quantities have no detached cap.",
        ));
    }
    Ok(request)
}
pub fn definitions() -> Vec<Definition> {
    OPERATIONS.iter().map(|operation| {
        let read=is_read(operation);
        let (allowed,required)=extras(operation);
        let mut schema=serde_json::to_value(schema_for!(Request)).expect("schema serialization");
        schema["properties"].as_object_mut().expect("object schema").retain(|name,_|
            ["wallet","network","chain_id"].contains(&name.as_str()) || (operation.starts_with("aave.") && name == "market") || allowed.contains(&name.as_str()) ||
            (!read && ["request_id","max_fee_eth"].contains(&name.as_str())));
        let mut fields=vec!["wallet","network","chain_id"];
        fields.extend_from_slice(required);
        if !read {fields.extend(["request_id","max_fee_eth"]);}
        schema["required"]=json!(fields);
        schema["properties"]["network"]=json!({"type":"string","enum":["evm","ethereum"]});
        if *operation == "wallet.permit2.approve" {
            schema["properties"]["expiration"] = json!({"type":"integer","minimum":0u64,"maximum":281474976710655u64});
        }
        let properties=schema["properties"].as_object_mut().expect("schema object");
        let decimal=json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":100});
        let contract=json!({"type":"string","pattern":"^0x[0-9a-fA-F]{40}$"});
        for (name,value) in [
            ("block_number",json!({"type":"integer","minimum":1,"description":"Optional historical state block for read-only Aave observations. Omission uses the current head; financial writes always use fresh state."})),("wallet",json!({"type":"string","minLength":1,"maxLength":80})),("chain_id",json!({"type":"integer","minimum":1})),
            ("request_id",json!({"type":"string","minLength":1,"maxLength":128})),("max_fee_eth",decimal.clone()),("max_amount",decimal.clone()),("min_output",decimal.clone()),("min_health_factor",decimal.clone()),
            ("amount",if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap"|"ramses.swap"|"sushi.swap"|"pancakeswap.swap"){json!({"anyOf":[decimal.clone(),{"const":"all"}]})}else{decimal.clone()}),
            ("market",contract.clone()),("max_debt_usd",decimal.clone()),("max_debt_to_collateral_bps",json!({"type":"integer","minimum":1,"maximum":10000})),("recipient",contract.clone()),("spender",contract.clone()),("token_in",json!({"anyOf":[contract.clone(),{"const":"native"}]})),("token_out",json!({"anyOf":[contract.clone(),{"const":"native"}]})),("token",json!({"anyOf":[contract.clone(),{"const":"native"}]})),
            ("tokens",json!({"type":"array","items":contract,"maxItems":64,"uniqueItems":true})),("transaction_hash",json!({"type":"string","pattern":"^0x[0-9a-fA-F]{64}$"})),
            ("fee_tier",json!({"type":"integer","enum":[100,500,3000,10000]})),("tick_spacing",json!({"type":"integer","enum":[1,5,10,50,100,200]})),("max_pool_fee_ppm",json!({"type":"integer","minimum":0,"maximum":999999,"description":"Maximum fee observed from the Ramses pool at the pinned quote block. This is a quote-time ceiling, not an on-chain in-transaction fee cap."})),("slippage_bps",json!({"type":"integer","minimum":0,"maximum":1000})),("max_price_impact_bps",json!({"type":"integer","minimum":0,"maximum":10000,"default":10000})),("deadline",json!({"type":"integer","minimum":1})),("protocol",json!({"type":"string","enum":["v3","v4"],"default":"v3"})),("v4_pool_key",serde_json::to_value(schema_for!(super::V4PoolKey)).expect("v4 pool key schema")),("enabled",json!({"type":"boolean"}))
        ]{if properties.contains_key(name){properties.insert(name.into(),value);}}
        if !read {
            properties.get_mut("max_fee_eth").expect("write fee field")["description"] = json!("Maximum total native ETH gas cost. Preparation adds 20% gas-unit and up to 50% fee-quote headroom, including priority fees, within this cap. Changes inside the approved envelope do not require a new gas approval. Fresh USD quotes must remain within the separate displayed approval ceilings.");
        }
        if let Some(dex) = super::uniswap::profile::additional_dex(operation) {
            schema["properties"]["fee_tier"] = json!({"type":"integer","enum":dex.fee_tiers()});
            schema["properties"]["protocol"] = json!({"type":"string","enum":["v3"],"default":"v3"});
            schema["properties"].as_object_mut().expect("schema").remove("v4_pool_key");
        }
        schema["additionalProperties"]=json!(false);
        if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap"|"ramses.swap"|"sushi.swap"|"pancakeswap.swap") {
            schema["allOf"]=json!([{"if":{"properties":{"amount":{"const":"all"}},"required":["amount"]},"then":{"required":["max_amount"]},"else":{"not":{"required":["max_amount"]}}}]);
        }
        let mut semantics = json!({"amount":"Exact decimal asset quantity; no floating-point conversion or rounding.","authorization":if read {"read-only"}else{"fresh human per-write authorization"},"network":"explicit immutable chain/deployment identity","deadline_example":"Fixed schema illustration; replace with the current Unix timestamp plus 120 seconds before preview.","bounded_all":"Supported full-balance operations require finite max_amount. Cash sweeps freeze observed units; full Aave repayment/withdrawal proves zero remaining scaled debt/claim. Every actual write receives a fresh preview and authorization.","retry":"Reconcile the original persisted hash, never blind replay."});
        if matches!(*operation, "uniswap.swap" | "ramses.swap" | "sushi.swap" | "pancakeswap.swap") {
            semantics["broker_generated_attempt_input_fields"] = json!(["/request_id"]);
            semantics["authority_expiry_fields"] =
                json!([{"pointer":"/deadline","format":"unix_seconds"}]);
            semantics["boundary_authorization"] = uniswap_boundary_discovery(*operation == "ramses.swap");
            if super::uniswap::profile::additional_dex(operation).is_some() {
                semantics["boundary_authorization"]["scope"] = json!("Single-pool exact-input v3 swap with fixed venue, factory, router and approved fee-to-pool map, returning output to the signing wallet.");
                semantics["boundary_authorization"]["fixed_fields"] = json!(["wallet","network","chain_id","signer","recipient","router","token_in","token_out","dex","factory","pool_deployer","approved_pools"]);
                semantics["boundary_authorization"]["permitted_variations"]["fee_tier"] = json!("A reviewed allowed fee tier may be selected only with its exact approved pool.");
                semantics["boundary_authorization"]["permitted_variations"].as_object_mut().expect("variations").remove("max_pool_fee_ppm");
                semantics["boundary_authorization"]["requirements_by_protocol"] = json!({"v3":["fixed venue, factory, router, token pair and approved fee-to-pool map", "explicit reviewed allowed_fee_tiers", "fresh quote no older than 120 seconds"]});
                let dex = super::uniswap::profile::additional_dex(operation).expect("matched");
                semantics["boundary_authorization"]["bounds_schema"]["properties"]["allowed_fee_tiers"]["description"] = json!("Reviewed fee tiers for the selected venue; each tier binds one exact approved pool address.");
                semantics["boundary_authorization"]["bounds_schema"]["properties"]["allowed_fee_tiers"]["items"] = json!({"type":"integer","enum":dex.fee_tiers()});
                semantics["boundary_authorization"]["bounds_schema"]["properties"].as_object_mut().expect("bounds properties").remove("max_pool_fee_ppm");
            }
        }
        if matches!(*operation, "uniswap.quote" | "uniswap.swap" | "ramses.quote" | "ramses.swap" | "sushi.quote" | "sushi.swap" | "pancakeswap.quote" | "pancakeswap.swap") {
            semantics["example_token_addresses"] = json!("Illustrative token contract addresses only; resolve exact deployed identities before use.");
            if matches!(*operation, "ramses.quote" | "ramses.swap") {
                semantics["pool_fee_semantics"] = json!("fee is read from the selected pool at the quote block and may change before inclusion; max_pool_fee_ppm limits the quote observation only, not the in-transaction fee.");
            }
        }
        let mut examples = vec![example(operation)];
        if operation.starts_with("aave.") {
            if matches!(*operation,"aave.reserves"|"aave.positions") {
                semantics["state_block"] = json!("block_number selects a historical read-only observation with canonical block/hash checks; it never supplies financial write authority or current-funds evidence.");
            }
            if *operation == "aave.markets" {
                semantics["reviewed_market_profiles"] = json!(fin_networks::all().iter().map(|n|json!({"chain_id":n.chain_id,"markets":super::aave::reviewed_markets(n.chain_id),"available":!super::aave::reviewed_markets(n.chain_id).is_empty()})).collect::<Vec<_>>());
                semantics["source_commit"] = json!(super::aave::ADDRESS_BOOK_COMMIT);
            }
            if !read {
                semantics["broker_generated_attempt_input_fields"] = json!(["/request_id"]);
                semantics["plan_permission"] = json!({"supported":true,"bounds_schema":schemars::schema_for!(super::aave::permissions::AaveBounds),"variable_fields":["amount","max_fee_eth"],"fixed_fields":"All remaining fields, including market, asset, recipient, collateral setting and risk limits."});
                if *operation == "aave.borrow" {
                    semantics["plan_permission"]["requires_adapter_scope"] = json!(true);
                }
            }
            semantics["market"] = json!("Optional exact enabled pool address; omission preserves the configured legacy default. Market identity is immutable within authorization.");
            semantics["risk"] = json!("Health factor and debt limits are fresh execution constraints, not guarantees against later interest, price changes or liquidation.");
        }
        if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap"|"ramses.swap") {
            let mut all=example(operation);
            all["amount"]=json!("all");
            all["max_amount"]=json!("2");
            examples.push(all);
        }
        if *operation == "uniswap.quote" {
            let mut v4 = example(operation);
            v4["protocol"] = json!("v4");
            v4["token_in"] = json!(EXAMPLE_TOKEN_B);
            v4["token_out"] = json!("native");
            v4["v4_pool_key"] = json!({"currency0":"0x0000000000000000000000000000000000000000","currency1":EXAMPLE_TOKEN_B,"fee":500,"tick_spacing":10,"hooks":"0x0000000000000000000000000000000000000000"});
            examples.push(v4);
        }
        if matches!(*operation, "ramses.quote" | "ramses.swap") {
            for example in &mut examples {
                example["chain_id"] = json!(4663);
                example["tick_spacing"] = json!(1);
            }
        }
        Definition {
            name: (*operation).into(),
            description: description(operation).into(),
            kind: if read { OperationKind::Read } else { OperationKind::Write },
            execution_actor: if read { ExecutionActor::AgentReadable } else { ExecutionActor::AgentInvocable },
            input_schema: schema,
            output_schema: if read { json!({"type":"object","required":["operation","resource","chain_id","observed_at","data"]}) } else { serde_json::to_value(schema_for!(fin_domain::PreparedOperation)).expect("schema") },
            error_schema: serde_json::to_value(schema_for!(FinError)).expect("schema"),
            examples,
            coverage: "Rust implementation; synthetic and fixed-fork evidence recorded separately. Human/mainnet acceptance remains pending.".into(),
            completion: "Writes require canonical original transaction proof, confirmations, bounded actual gas, application-specific events and inclusion-block post-state.".into(),
            semantics,
        }
    }).collect()
}

fn uniswap_boundary_discovery(ramses: bool) -> Value {
    let scope = if ramses {
        "Single-pool exact-input Ramses v3-compatible swap with output returned to the signing wallet; tick spacing and pool deployment are fixed."
    } else {
        "Single-pool exact-input Uniswap v3 or v4 swap with output returned to the signing wallet."
    };
    json!({
        "supported": true,
        "scope": scope,
        "bounds_schema": super::amm_permissions::swap_bounds_schema(),
        "fixed_fields": if ramses { json!(["wallet", "network", "chain_id", "signer", "recipient", "router", "token_in", "token_out", "dex", "tick_spacing", "factory", "pool_deployer", "pool"]) } else { json!(["wallet", "network", "chain_id", "signer", "recipient", "router", "token_in", "token_out", "protocol", "v4_pool_key"]) },
        "permitted_variations": {
            "amount": "Each attempt input is bounded by cumulative max_input.",
            "max_fee_eth": "Each attempt's maximum native gas cost is bounded by cumulative max_fee_eth.",
            "min_output": "Each attempt must satisfy the approved rational minimum_rate and optional absolute_minimum_output.",
            "slippage_bps": "Selected slippage may not exceed max_slippage_bps; the fresh quote and exact minimum output are checked for every attempt.",
            "max_price_impact_bps": "The observed pool-price impact, excluding the separately charged pool fee, must not exceed this approved ceiling.",
            "deadline": "Each attempt deadline must be current and no farther ahead than max_deadline_seconds.",
            "fee_tier": if ramses { "The tick_spacing and pool are fixed; fee is read from the pool at each pinned quote." } else { "For v3, only allowed_fee_tiers may be selected. For v4, fee is fixed by v4_pool_key." },
            "max_pool_fee_ppm": "Ramses quote-time maximum pool fee. It bounds each pinned pool.fee() observation, not the exact fee charged inside a later transaction.",
            "request_id": "The broker generates a fresh correlation identifier for every attempt."
        },
        "requirements_by_protocol": if ramses { json!({"ramses": ["fixed pair, router, factory, pool deployer, pool and tick_spacing", "maximum quoted fee is checked at each pinned quote", "fresh quote no older than 120 seconds"]}) } else { json!({
            "v3": ["fixed token pair and router", "explicit allowed_fee_tiers", "fresh quote no older than 120 seconds"],
            "v4": ["fixed token pair, router and exact v4_pool_key", "allowed_fee_tiers must be empty", "fresh quote no older than 120 seconds"]
        }) },
        "asset_endpoints": "token_in and token_out may be ERC-20 contract addresses or native; native is priced and proven through reviewed WETH settlement.",
        "quote_and_checks": {
            "freshness": "Every attempt obtains and validates a fresh protocol quote, route identity, price-impact measurement and exact output floor.",
            "permit_signature_simulation": "Unchecked in preview. Typed permit signatures are created only after authorization, then the exact materialized transaction is simulated before submission."
        },
        "broker_generated_attempt_input_fields": ["/request_id"],
        "structured_violations": {"path": "error.details.boundary_violation", "schema": schema_for!(fin_domain::boundaries::BoundaryViolation)},
        "default_consent_seconds": 600,
        "cumulative_scope": "Input and native gas costs are lifetime cumulative caps; every attempt retains its own immutable quote, calldata, fee and reconciliation identity.",
        "replacement": "Changed fixed route or authorization bounds require fresh approval; in-bound attempts require a new preview and are checked against remaining cumulative authority.",
        "unsupported": ["arbitrary calldata", "hook execution", "multi-hop routes", "recipient changes", "v4 pool-key changes"]
    })
}
pub fn description(operation: &str) -> &'static str {
    match operation {
        "wallet.balances" => "Observe native ETH and exact ERC-20 holdings.",
        "wallet.token" => "Verify token contract and display metadata.",
        "wallet.allowance" => "Observe exact spender authority.",
        "wallet.permit2.allowance" => "Observe exact Uniswap Permit2 allowance and nonce.",
        "wallet.receipt" => "Read original transaction inclusion state.",
        "sushi.quote" => "Quote reviewed Sushi v3 single-pool fee tiers.",
        "pancakeswap.quote" => "Quote reviewed PancakeSwap v3 single-pool fee tiers.",
        "uniswap.quote" => {
            "Quote configured Uniswap v3 fee tiers or an explicitly selected supported v4 pool."
        }
        "ramses.quote" => {
            "Quote one explicitly selected reviewed Ramses v3-compatible pool spacing."
        }
        "aave.markets" => {
            "List enabled Aave V3 markets and pinned deployment compatibility; unavailable networks are explicit."
        }
        "aave.reserves" => {
            "Discover all Aave reserve eligibility, limits, receipt and debt identities."
        }
        "aave.positions" => {
            "Observe all supplied claims, stable/variable debt and collateral separately."
        }
        "wallet.transfer" => {
            "Transfer exact native ETH or standard ERC-20 to an explicit recipient."
        }
        "wallet.wrap" => "Wrap exact native ETH as WETH.",
        "wallet.unwrap" => "Unwrap exact WETH as native ETH.",
        "wallet.approve" => "Set finite exact ERC-20 spender allowance, including zero.",
        "wallet.permit2.approve" => "Set finite exact Uniswap Permit2 allowance and expiration.",
        "sushi.swap" => "Execute a bounded exact-input Sushi v3 single-pool swap.",
        "pancakeswap.swap" => "Execute a bounded exact-input PancakeSwap v3 single-pool swap.",
        "uniswap.swap" => "Execute bounded exact-input Uniswap v3 or v4 single-pool swap.",
        "ramses.swap" => "Execute a bounded exact-input Ramses v3-compatible single-pool swap.",
        "aave.supply" => {
            "Supply underlying into the selected reviewed Aave V3 market with exact revision-specific backing proof."
        }
        "aave.borrow" => "Borrow variable-rate debt with explicit collateral/health bounds.",
        "aave.repay" => "Repay variable debt; all requires bounded maximum and finite allowance.",
        "aave.withdraw" => "Withdraw exact supplied underlying to an explicit recipient.",
        "aave.collateral" => "Set collateral use for an existing supplied reserve.",
        _ => "Unsupported Ethereum operation.",
    }
}
fn example(operation: &str) -> Value {
    let mut input = json!({"wallet":"main-wallet","network":"evm","chain_id":1});
    if !is_read(operation) {
        input["request_id"] = json!("unique-logical-request");
        input["max_fee_eth"] = json!("0.001");
    }
    let (_, required) = extras(operation);
    for field in required {
        input[*field] = match *field {
            "token" => json!(if operation == "wallet.transfer" {
                "native"
            } else {
                EXAMPLE_TOKEN_A
            }),
            "token_in" => json!(if matches!(
                operation,
                "uniswap.quote"
                    | "uniswap.swap"
                    | "ramses.quote"
                    | "ramses.swap"
                    | "sushi.quote"
                    | "sushi.swap"
                    | "pancakeswap.quote"
                    | "pancakeswap.swap"
            ) {
                "native"
            } else {
                EXAMPLE_TOKEN_A
            }),
            "token_out" => json!(EXAMPLE_TOKEN_B),
            "recipient" => json!("0x0000000000000000000000000000000000000001"),
            "spender" => json!("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"),
            "amount" => json!("1"),
            "expiration" => json!(2000000000u64),
            "min_output" => json!("1"),
            "slippage_bps" => json!(50),
            "deadline" => json!(2000000000u64),
            "min_health_factor" => json!("1.5"),
            "enabled" => json!(true),
            "transaction_hash" => json!(format!("0x{}", "00".repeat(32))),
            _ => Value::Null,
        }
    }
    if operation == "uniswap.quote" {
        input["protocol"] = json!("v3");
    }
    if matches!(operation, "ramses.quote" | "ramses.swap") {
        input["tick_spacing"] = json!(1);
    }
    input
}

const EXAMPLE_TOKEN_A: &str = "0x0000000000000000000000000000000000000064";
const EXAMPLE_TOKEN_B: &str = "0x0000000000000000000000000000000000000065";

#[cfg(test)]
mod ramses_request_tests {
    use super::*;

    #[test]
    fn ramses_requests_require_reviewed_tick_spacing_and_reject_uniswap_fee_fields() {
        let quote = json!({
            "wallet":"main", "network":"evm", "chain_id":4663,
            "token_in":"native", "token_out":"0x0000000000000000000000000000000000000065",
            "amount":"0.001", "tick_spacing":1
        });
        assert!(parse("ramses.quote", &quote).is_ok());
        let mut unsupported_spacing = quote.clone();
        unsupported_spacing["tick_spacing"] = json!(2);
        assert!(parse("ramses.quote", &unsupported_spacing).is_err());
        let mut uniswap_fee = quote;
        uniswap_fee["fee_tier"] = json!(500);
        assert!(parse("ramses.quote", &uniswap_fee).is_err());

        let swap = json!({
            "wallet":"main", "network":"evm", "chain_id":4663,
            "request_id":"ramses-1", "max_fee_eth":"0.001",
            "token_in":"native", "token_out":"0x0000000000000000000000000000000000000065",
            "amount":"0.001", "recipient":"0x0000000000000000000000000000000000000001",
            "min_output":"1", "slippage_bps":50, "deadline":2000000000,
            "tick_spacing":1, "max_pool_fee_ppm":700
        });
        assert!(parse("ramses.swap", &swap).is_ok());
        let mut too_high = swap.clone();
        too_high["max_pool_fee_ppm"] = json!(1_000_001);
        assert!(parse("ramses.swap", &too_high).is_err());
        let mut changed_spacing = swap;
        changed_spacing["tick_spacing"] = json!(5);
        assert!(parse("ramses.swap", &changed_spacing).is_ok());
    }
}
