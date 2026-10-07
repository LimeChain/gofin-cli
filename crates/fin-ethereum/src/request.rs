use fin_domain::{Definition, ExecutionActor, FinError, OperationKind, Result};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub wallet: String,
    pub network: String,
    pub chain_id: u64,
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
    pub min_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
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
    "wallet.receipt",
    "uniswap.quote",
    "aave.reserves",
    "aave.positions",
    "wallet.transfer",
    "wallet.wrap",
    "wallet.unwrap",
    "wallet.approve",
    "uniswap.swap",
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
            | "wallet.receipt"
            | "uniswap.quote"
            | "aave.reserves"
            | "aave.positions"
    )
}
fn extras(operation: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match operation {
        "wallet.balances" | "aave.reserves" | "aave.positions" => (&["tokens"], &[]),
        "wallet.token" => (&["token"], &["token"]),
        "wallet.allowance" => (&["token", "spender"], &["token", "spender"]),
        "wallet.receipt" => (&["transaction_hash"], &["transaction_hash"]),
        "uniswap.quote" => (
            &["token_in", "token_out", "amount", "fee_tier"],
            &["token_in", "token_out", "amount"],
        ),
        "wallet.transfer" => (
            &["token", "amount", "recipient"],
            &["token", "amount", "recipient"],
        ),
        "aave.withdraw" => (
            &["token", "amount", "recipient", "max_amount"],
            &["token", "amount", "recipient"],
        ),
        "wallet.wrap" => (&["amount"], &["amount"]),
        "wallet.unwrap" => (&["amount", "max_amount"], &["amount"]),
        "wallet.approve" => (
            &["token", "spender", "amount"],
            &["token", "spender", "amount"],
        ),
        "uniswap.swap" => (
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
        "aave.supply" => (&["token", "amount"], &["token", "amount"]),
        "aave.borrow" => (
            &["token", "amount", "min_health_factor"],
            &["token", "amount", "min_health_factor"],
        ),
        "aave.repay" => (&["token", "amount", "max_amount"], &["token", "amount"]),
        "aave.collateral" => (&["token", "enabled"], &["token", "enabled"]),
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
    if request.wallet.is_empty()
        || request.wallet.len() > 80
        || !matches!(request.network.as_str(), "evm" | "ethereum")
        || request.chain_id == 0
    {
        return Err(FinError::validation(
            "Use a named wallet and explicit EVM chain identity.",
        ));
    }
    if request
        .tokens
        .as_ref()
        .is_some_and(|tokens| tokens.len() > 64)
        || request.slippage_bps.is_some_and(|n| n > 1000)
        || request
            .fee_tier
            .is_some_and(|n| ![100, 500, 3000, 10000].contains(&n))
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
        "aave.repay" | "aave.withdraw" | "wallet.unwrap" | "uniswap.swap"
    );
    if request.amount.as_deref() == Some("all") && (!all_supported || request.max_amount.is_none())
    {
        return Err(FinError::validation(
            "Only supported full-balance operations accept all, and require finite max_amount.",
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
            ["wallet","network","chain_id"].contains(&name.as_str()) || allowed.contains(&name.as_str()) ||
            (!read && ["request_id","max_fee_eth"].contains(&name.as_str())));
        let mut fields=vec!["wallet","network","chain_id"];
        fields.extend_from_slice(required);
        if !read {fields.extend(["request_id","max_fee_eth"]);}
        schema["required"]=json!(fields);
        schema["properties"]["network"]=json!({"type":"string","enum":["evm","ethereum"]});
        let properties=schema["properties"].as_object_mut().expect("schema object");
        let decimal=json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]+)?$","maxLength":100});
        let contract=json!({"type":"string","pattern":"^0x[0-9a-fA-F]{40}$"});
        for (name,value) in [
            ("wallet",json!({"type":"string","minLength":1,"maxLength":80})),("chain_id",json!({"type":"integer","minimum":1})),
            ("request_id",json!({"type":"string","minLength":1,"maxLength":128})),("max_fee_eth",decimal.clone()),("max_amount",decimal.clone()),("min_output",decimal.clone()),("min_health_factor",decimal.clone()),
            ("amount",if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap"){json!({"anyOf":[decimal.clone(),{"const":"all"}]})}else{decimal.clone()}),
            ("recipient",contract.clone()),("spender",contract.clone()),("token_in",contract.clone()),("token_out",contract.clone()),("token",json!({"anyOf":[contract.clone(),{"const":"native"}]})),
            ("tokens",json!({"type":"array","items":contract,"maxItems":64,"uniqueItems":true})),("transaction_hash",json!({"type":"string","pattern":"^0x[0-9a-fA-F]{64}$"})),
            ("fee_tier",json!({"type":"integer","enum":[100,500,3000,10000]})),("slippage_bps",json!({"type":"integer","minimum":0,"maximum":1000})),("deadline",json!({"type":"integer","minimum":1})),("enabled",json!({"type":"boolean"}))
        ]{if properties.contains_key(name){properties.insert(name.into(),value);}}
        schema["additionalProperties"]=json!(false);
        if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap") {
            schema["allOf"]=json!([{"if":{"properties":{"amount":{"const":"all"}},"required":["amount"]},"then":{"required":["max_amount"]},"else":{"not":{"required":["max_amount"]}}}]);
        }
        Definition {name:(*operation).into(),description:description(operation).into(),kind:if read {OperationKind::Read}else{OperationKind::Write},execution_actor:if read {ExecutionActor::AgentReadable}else{ExecutionActor::AgentInvocable},input_schema:schema,
          output_schema:if read {json!({"type":"object","required":["operation","resource","chain_id","observed_at","data"]})}else{serde_json::to_value(schema_for!(fin_domain::PreparedOperation)).expect("schema")},error_schema:serde_json::to_value(schema_for!(FinError)).expect("schema"),examples:{let mut examples=vec![example(operation)];if matches!(*operation,"aave.repay"|"aave.withdraw"|"wallet.unwrap"|"uniswap.swap"){let mut all=example(operation);all["amount"]=json!("all");all["max_amount"]=json!("2");examples.push(all);}examples},coverage:"Rust implementation; synthetic and fixed-fork evidence recorded separately. Human/mainnet acceptance remains pending.".into(),completion:"Writes require canonical original transaction proof, confirmations, bounded actual gas, application-specific events and inclusion-block post-state.".into(),semantics:json!({"amount":"Exact decimal asset quantity; no floating-point conversion or rounding.","authorization":if read {"read-only"}else{"fresh human per-write authorization"},"network":"explicit immutable chain/deployment identity","deadline_example":"Fixed schema illustration; replace with the current Unix timestamp plus 120 seconds before preview.","bounded_all":"Supported full-balance operations require finite max_amount. Cash sweeps freeze observed units; full Aave repayment/withdrawal proves zero remaining scaled debt/claim. Every actual write receives a fresh preview and authorization.","retry":"Reconcile the original persisted hash, never blind replay."})}
    }).collect()
}
pub fn description(operation: &str) -> &'static str {
    match operation {
        "wallet.balances" => "Observe native ETH and exact ERC-20 holdings.",
        "wallet.token" => "Verify token contract and display metadata.",
        "wallet.allowance" => "Observe exact spender authority.",
        "wallet.receipt" => "Read original transaction inclusion state.",
        "uniswap.quote" => "Quote supported Uniswap v3 single-pool fee tiers using QuoterV2.",
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
        "uniswap.swap" => "Execute bounded exact-input Uniswap v3 single-pool swap.",
        "aave.supply" => {
            "Supply underlying into Aave v3 with exact revision-5 scaled backing proof."
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
    let weth = super::WETH;
    let usdc = super::USDC;
    let (_, required) = extras(operation);
    for field in required {
        input[*field] = match *field {
            "token" => json!(if operation == "wallet.transfer" {
                "native"
            } else {
                usdc
            }),
            "token_in" => json!(weth),
            "token_out" => json!(usdc),
            "recipient" => json!("0x0000000000000000000000000000000000000001"),
            "spender" => json!("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"),
            "amount" => json!("1"),
            "min_output" => json!("1"),
            "slippage_bps" => json!(50),
            "deadline" => json!(2000000000u64),
            "min_health_factor" => json!("1.5"),
            "enabled" => json!(true),
            "transaction_hash" => json!(format!("0x{}", "00".repeat(32))),
            _ => Value::Null,
        }
    }
    input
}
