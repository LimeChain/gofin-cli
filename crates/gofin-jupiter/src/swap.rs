//! Provider-trusted Jupiter Meta-Aggregator order contract.
//! Limits are checked against the typed order response. Transaction contents
//! are opaque; Jupiter owns transaction construction and RTSE slippage.

use gofin_domain::{GoFinError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const JUPITER_V6_PROGRAM: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
pub const WRAPPED_SOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SwapIntent {
    pub wallet: String,
    pub network: String,
    pub input_mint: String,
    pub output_mint: String,
    pub amount_atoms: String,
    pub minimum_output_atoms: String,
    /// Maximum acceptable RTSE slippage in Jupiter's order response.
    pub maximum_slippage_bps: u16,
    /// Maximum total Jupiter fee, including any gasless recoup fee.
    pub maximum_platform_fee_bps: u16,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MetaOrderResponse {
    pub transaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub request_id: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    pub taker: String,
    pub router: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_u64_from_string_or_number"
    )]
    pub last_valid_block_height: Option<u64>,
    pub fee_bps: u16,
    pub fee_mint: Option<String>,
    pub expire_at: Option<Value>,
    pub signature_fee_lamports: u64,
    pub signature_fee_payer: Option<String>,
    pub prioritization_fee_lamports: u64,
    pub prioritization_fee_payer: Option<String>,
    pub rent_fee_lamports: u64,
    pub rent_fee_payer: Option<String>,
}

fn deserialize_optional_u64_from_string_or_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected unsigned block height")),
        Some(Value::String(text))
            if !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()) =>
        {
            text.parse::<u64>()
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
        Some(_) => Err(serde::de::Error::custom(
            "expected decimal string or unsigned block height",
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedOrder {
    pub transaction_base64: String,
    pub quote: ValidatedQuote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedQuote {
    pub request_id: String,
    pub expected_output_atoms: u64,
    pub minimum_output_atoms: u64,
    pub slippage_bps: u16,
    pub last_valid_block_height: Option<u64>,
    pub provider_expire_at: Option<String>,
    pub router: String,
    pub fee_bps: u16,
    pub fee_mint: Option<String>,
    pub wallet_network_fee_lamports: u64,
    pub wallet_account_funding_lamports: u64,
}

pub fn meta_order_path(intent: &SwapIntent, address: &str) -> Result<String> {
    let (input, _) = validate_intent(intent)?;
    let taker = gofin_solana::parse_address(address)?;
    // Omit slippageBps to let Jupiter RTSE choose the order's slippage.
    Ok(format!(
        "/swap/v2/order?inputMint={}&outputMint={}&amount={input}&taker={taker}",
        intent_mint(&intent.input_mint)?,
        intent_mint(&intent.output_mint)?
    ))
}

pub fn validate_meta_order(
    intent: &SwapIntent,
    address: &str,
    response: MetaOrderResponse,
) -> Result<ValidatedOrder> {
    let quote = validate_meta_quote(intent, address, response.clone())?;
    let transaction = response
        .transaction
        .as_ref()
        .filter(|tx| !tx.trim().is_empty());
    if transaction.is_none() || response.error_code.is_some() || response.error_message.is_some() {
        return Err(GoFinError::new(
            "JUPITER_ORDER_EMPTY",
            "prepare",
            "Jupiter did not return a usable transaction for the exact swap request.",
            response.quote_next_action(),
        )
        .with_details(json!({"provider_error": response.provider_error()})));
    }
    let is_rfq = quote.router == "jupiterz";
    if (is_rfq && quote.provider_expire_at.is_none())
        || (!is_rfq && quote.last_valid_block_height.is_none())
    {
        return Err(GoFinError::new(
            "JUPITER_EXPIRY_MISSING",
            "prepare",
            "Jupiter did not return the route's applicable finite transaction or RFQ expiry.",
            "Request a fresh order with a valid blockhash height or maker quote expiry.",
        ));
    }
    Ok(ValidatedOrder {
        transaction_base64: transaction.expect("checked above").clone(),
        quote,
    })
}

impl MetaOrderResponse {
    pub fn transaction_available(&self) -> bool {
        self.transaction
            .as_ref()
            .is_some_and(|tx| !tx.trim().is_empty())
            && self.error_code.is_none()
            && self.error_message.is_none()
    }

    pub fn provider_error(&self) -> Option<Value> {
        (self.error_code.is_some() || self.error_message.is_some()).then(|| {
            json!({
                "router": self.router,
                "code": self.error_code,
                "message": self.error_message,
            })
        })
    }

    pub fn quote_next_action(&self) -> &'static str {
        match (self.router.as_str(), self.error_code) {
            ("metis" | "dflow" | "okx" | "jupiterz", Some(1)) => {
                "Once the required input funds are available in the wallet, retry jupiter.quote for a fresh quote before execution. Pricing and transaction availability may change."
            }
            ("metis" | "dflow" | "okx", Some(2)) => {
                "Once sufficient SOL for gas is available in the wallet, retry jupiter.quote for a fresh quote before execution."
            }
            _ if self.transaction_available() => {
                "Use jupiter.swap with explicit financial limits to prepare a fresh executable order; this quote does not authorize execution."
            }
            _ => {
                "Resolve the provider's transaction blocker, then retry jupiter.quote for a fresh quote before execution."
            }
        }
    }
}

pub fn validate_meta_quote(
    intent: &SwapIntent,
    address: &str,
    response: MetaOrderResponse,
) -> Result<ValidatedQuote> {
    let (input, minimum_output) = validate_intent(intent)?;
    if response.input_mint != intent_mint(&intent.input_mint)?
        || response.output_mint != intent_mint(&intent.output_mint)?
        || exact_atoms(&response.in_amount)? != input
        || response.taker != address
        || response.swap_mode != "ExactIn"
    {
        return Err(GoFinError::new(
            "JUPITER_ORDER_BINDING_MISMATCH",
            "prepare",
            "Jupiter's order changed the exact wallet, assets, input amount or swap mode.",
            "Request a fresh order for the approved swap intent.",
        ));
    }
    let out_amount = exact_atoms(&response.out_amount)?;
    let threshold = exact_atoms(&response.other_amount_threshold)?;
    if out_amount < minimum_output || threshold < minimum_output || threshold > out_amount {
        return Err(GoFinError::new(
            "JUPITER_MINIMUM_OUTPUT_UNMET",
            "prepare",
            "Jupiter's order minimum output is below the approved amount or inconsistent with the quote.",
            "Request a fresh order whose quoted minimum meets the approved output floor.",
        ));
    }
    if response.slippage_bps > intent.maximum_slippage_bps || response.slippage_bps > 10_000 {
        return Err(GoFinError::new(
            "JUPITER_SLIPPAGE_LIMIT_EXCEEDED",
            "prepare",
            format!(
                "Jupiter's RTSE slippage is {} bps ({}.{:02}%), exceeding the approved maximum of {} bps ({}.{:02}%).",
                response.slippage_bps,
                response.slippage_bps / 100,
                response.slippage_bps % 100,
                intent.maximum_slippage_bps,
                intent.maximum_slippage_bps / 100,
                intent.maximum_slippage_bps % 100,
            ),
            "Request a fresh order within the approved slippage limit.",
        ).with_details(json!({
            "actual_slippage_bps": response.slippage_bps,
            "maximum_slippage_bps": intent.maximum_slippage_bps,
        })));
    }
    if response.request_id.trim().is_empty()
        || response.request_id.len() > 256
        || response.router.trim().is_empty()
    {
        return Err(invalid_order());
    }
    let provider_expire_at = match response.expire_at {
        // Optional API strings may be empty for blockhash-based orders. RFQ
        // expiry remains mandatory below; this never synthesizes fresh consent.
        Some(Value::String(value)) if value.is_empty() => None,
        Some(Value::String(value)) => {
            let parsed = if value.bytes().all(|byte| byte.is_ascii_digit()) {
                if value.len() > 1 && value.starts_with('0') {
                    return Err(invalid_order().with_details(
                        json!({"field":"expireAt","format":"decimal_unix_seconds_string"}),
                    ));
                }
                let timestamp = value.parse::<i64>().map_err(|_| {
                    invalid_order().with_details(
                        json!({"field":"expireAt","format":"decimal_unix_seconds_string"}),
                    )
                })?;
                if timestamp < 0 {
                    return Err(invalid_order().with_details(
                        json!({"field":"expireAt","format":"decimal_unix_seconds_string"}),
                    ));
                }
                chrono::DateTime::from_timestamp(timestamp, 0).ok_or_else(|| {
                    invalid_order().with_details(
                        json!({"field":"expireAt","format":"decimal_unix_seconds_string"}),
                    )
                })?
            } else {
                chrono::DateTime::parse_from_rfc3339(&value)
                    .map_err(|_| invalid_order().with_details(json!({"field":"expireAt","format":"rfc3339_string","length":value.len()})))?
                    .to_utc()
            };
            Some(parsed.to_utc().to_rfc3339())
        }
        Some(Value::Number(value)) => {
            let timestamp = value.as_i64().ok_or_else(|| {
                invalid_order()
                    .with_details(json!({"field":"expireAt","format":"integer_timestamp"}))
            })?;
            Some(
                chrono::DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(|| {
                        invalid_order()
                            .with_details(json!({"field":"expireAt","format":"integer_timestamp"}))
                    })?
                    .to_rfc3339(),
            )
        }
        Some(_) => {
            return Err(invalid_order()
                .with_details(json!({"field":"expireAt","format":"unexpected_json_type"})));
        }
        None => None,
    };
    let last_valid_block_height = response.last_valid_block_height;

    let fee_mint = response
        .fee_mint
        .map(|mint| gofin_solana::parse_address(&mint))
        .transpose()?;
    let fee_bps = response.fee_bps;
    if fee_bps > 10_000
        || fee_bps > intent.maximum_platform_fee_bps
        || (fee_bps > 0
            && fee_mint
                .as_deref()
                .is_none_or(|mint| mint != response.input_mint && mint != response.output_mint))
    {
        return Err(GoFinError::new(
            "JUPITER_PLATFORM_FEE_LIMIT_EXCEEDED",
            "prepare",
            "Jupiter's total quoted fee exceeds the approved limit or has no exact mint.",
            "Review a fresh order within the approved fee limit.",
        ));
    }
    let wallet_network_fee_lamports = wallet_fee(
        response.signature_fee_lamports,
        response.signature_fee_payer.as_deref(),
        address,
    )?
    .checked_add(wallet_fee(
        response.prioritization_fee_lamports,
        response.prioritization_fee_payer.as_deref(),
        address,
    )?)
    .ok_or_else(invalid_order)?;
    let wallet_account_funding_lamports = wallet_fee(
        response.rent_fee_lamports,
        response.rent_fee_payer.as_deref(),
        address,
    )?;
    if wallet_network_fee_lamports > intent.maximum_network_fee_lamports {
        return Err(GoFinError::new(
            "JUPITER_NETWORK_FEE_LIMIT_EXCEEDED",
            "prepare",
            "Jupiter's quoted wallet network fees exceed the approved maximum.",
            "Request a fresh order within the approved network fee limit.",
        ));
    }
    if wallet_account_funding_lamports > intent.maximum_account_funding_lamports {
        return Err(GoFinError::new(
            "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED",
            "prepare",
            "Jupiter's quoted wallet account funding exceeds the approved maximum.",
            "Request a fresh order within the approved account funding limit.",
        ));
    }
    Ok(ValidatedQuote {
        request_id: response.request_id,
        expected_output_atoms: out_amount,
        minimum_output_atoms: threshold,
        slippage_bps: response.slippage_bps,
        last_valid_block_height,
        provider_expire_at,
        router: response.router,
        fee_bps,
        fee_mint,
        wallet_network_fee_lamports,
        wallet_account_funding_lamports,
    })
}

fn wallet_fee(amount: u64, payer: Option<&str>, wallet: &str) -> Result<u64> {
    match payer {
        Some(payer) => {
            gofin_solana::parse_address(payer)?;
            Ok(if payer == wallet { amount } else { 0 })
        }
        None if amount == 0 => Ok(0),
        None => Err(invalid_order()),
    }
}

fn intent_mint(mint: &str) -> Result<String> {
    if mint == "native" {
        Ok(WRAPPED_SOL_MINT.into())
    } else {
        gofin_solana::parse_address(mint)
    }
}

pub fn validate_intent(intent: &SwapIntent) -> Result<(u64, u64)> {
    if intent.input_mint == WRAPPED_SOL_MINT || intent.output_mint == WRAPPED_SOL_MINT {
        return Err(GoFinError::new(
            "JUPITER_WRAPPED_SOL_UNSUPPORTED",
            "validation",
            "Jupiter Meta-Aggregator orders use native SOL and cannot independently bind an explicit wrapped-SOL balance.",
            "Use native for SOL, or unwrap existing wrapped SOL separately before requesting a spot swap.",
        ));
    }
    if intent.network != "solana"
        || intent.wallet.trim().is_empty()
        || intent.input_mint == intent.output_mint
        || intent.maximum_slippage_bps > 10_000
    {
        return Err(GoFinError::validation(
            "A Jupiter swap requires Solana, distinct exact mint identities, a wallet and bounded slippage.",
        ));
    }
    let input = exact_atoms(&intent.amount_atoms)?;
    let minimum = exact_atoms(&intent.minimum_output_atoms)?;
    if input == 0 || minimum == 0 {
        return Err(GoFinError::validation(
            "Swap input and minimum output must be positive exact atom amounts.",
        ));
    }
    for mint in [&intent.input_mint, &intent.output_mint] {
        if mint != "native" {
            gofin_solana::parse_address(mint)?;
        }
    }
    Ok((input, minimum))
}

pub fn definition_input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","input_mint","output_mint","amount_atoms","minimum_output_atoms","maximum_slippage_bps","maximum_platform_fee_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"input_mint":{"type":"string","description":"Exact SPL mint address or native SOL. Explicit wrapped-SOL mint is unsupported by this order contract."},"output_mint":{"type":"string","description":"Exact SPL mint address or native SOL. Explicit wrapped-SOL mint is unsupported by this order contract."},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"minimum_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000},"maximum_platform_fee_bps":{"type":"integer","minimum":0,"maximum":10000,"description":"Maximum total Jupiter fee rate including platform fees and gasless recoup."},"maximum_network_fee_lamports":{"type":"integer","minimum":0},"maximum_account_funding_lamports":{"type":"integer","minimum":0}}})
}

pub fn definition_output_schema() -> Value {
    json!({"type":"object","properties":{"network":{"const":"solana:mainnet-beta"},"cluster":{"const":"mainnet-beta"},"wallet":{"type":"string"},"input_mint":{"type":"string"},"output_mint":{"type":"string"},"input_atoms":{"type":"string","pattern":"^[0-9]+$"},"expected_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"minimum_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000},"slippage_bps":{"type":"integer","minimum":0,"maximum":10000},"router":{"type":"string"},"provider_request_id":{"type":["string","null"]},"last_valid_block_height":{"type":["integer","null"],"minimum":1},"provider_expire_at":{"type":["string","null"],"format":"date-time"},"fees":{"type":"object","properties":{"estimated_network_fee_lamports":{"type":["string","null"]},"network_fee_estimate_status":{"type":"string"},"maximum_network_fee_lamports":{"type":"string"},"maximum_platform_fee_bps":{"type":"integer","minimum":0,"maximum":10000,"description":"Maximum total Jupiter fee rate including platform fees and gasless recoup."},"platform_fee_mint":{"type":["string","null"]},"account_funding_estimate_lamports":{"type":["string","null"]},"account_funding_estimate_status":{"type":"string"}},"required":["network_fee_estimate_status","maximum_platform_fee_bps","account_funding_estimate_status"]},"expires_at":{"type":"string","format":"date-time"},"operation_id":{"type":"string"},"status":{"enum":["prepared","submitted","pending","completed","failed"]},"signature":{"type":["string","null"]},"verified":{"type":"boolean"},"evidence":{"type":["object","null"]}},"additionalProperties":true})
}

pub fn definition_quote_output_schema() -> Value {
    let mut schema = definition_output_schema();
    schema["properties"]["transaction_available"] = json!({
        "type":"boolean",
        "description":"Jupiter returned a nonempty transaction without a provider error. This does not authorize execution or prove funds remain available."
    });
    schema["properties"]["provider_error"] = json!({
        "type":["object","null"],
        "description":"Jupiter's transaction construction error; pricing can still be available. Interpret code together with router.",
        "required":["router","code","message"],
        "properties":{
            "router":{"type":"string"},
            "code":{"type":["integer","null"]},
            "message":{"type":["string","null"]}
        },
        "additionalProperties":false
    });
    schema["properties"]["next_action"] = json!({"type":"string"});
    schema["required"] = json!([
        "input_atoms",
        "expected_output_atoms",
        "minimum_output_atoms",
        "transaction_available",
        "provider_error",
        "next_action"
    ]);
    schema
}

fn exact_atoms(value: &str) -> Result<u64> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(GoFinError::validation(
            "Financial atom amounts must be canonical unsigned decimal integer strings.",
        ));
    }
    value.parse::<u64>().map_err(|_| {
        GoFinError::validation("The atom amount is outside the supported exact range.")
    })
}

fn invalid_order() -> GoFinError {
    GoFinError::new(
        "JUPITER_ORDER_INVALID",
        "prepare",
        "Jupiter returned an incomplete or inconsistent order response.",
        "Request a fresh order for the exact approved swap intent.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const WALLET: &str = "11111111111111111111111111111111";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn intent() -> SwapIntent {
        SwapIntent {
            wallet: "synthetic".into(),
            network: "solana".into(),
            input_mint: "native".into(),
            output_mint: USDC.into(),
            amount_atoms: "100".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 10,
            maximum_network_fee_lamports: 7_000,
            maximum_account_funding_lamports: 2_039_280,
        }
    }

    fn response() -> Value {
        json!({"transaction":"AQ==", "requestId":"synthetic-order", "inputMint":WRAPPED_SOL_MINT, "outputMint":USDC, "inAmount":"100", "outAmount":"100", "otherAmountThreshold":"99", "swapMode":"ExactIn", "slippageBps":100, "taker":WALLET, "router":"future_jupiter_engine", "lastValidBlockHeight":"123", "feeBps":10, "feeMint":USDC, "signatureFeeLamports":5000, "signatureFeePayer":WALLET, "prioritizationFeeLamports":2000, "prioritizationFeePayer":WALLET, "rentFeeLamports":2039280, "rentFeePayer":WALLET})
    }

    fn validate(value: Value) -> Result<ValidatedQuote> {
        let response = serde_json::from_value(value).map_err(|_| invalid_order())?;
        validate_meta_order(&intent(), WALLET, response).map(|order| order.quote)
    }

    #[test]
    fn order_uses_rtse_without_route_or_slippage_override() {
        let path = meta_order_path(&intent(), WALLET).unwrap();
        assert_eq!(
            path,
            format!(
                "/swap/v2/order?inputMint={WRAPPED_SOL_MINT}&outputMint={USDC}&amount=100&taker={WALLET}"
            )
        );
        let validated = validate(response()).unwrap();
        assert_eq!(validated.wallet_network_fee_lamports, 7000);
        assert_eq!(validated.wallet_account_funding_lamports, 2039280);
        assert_eq!(validated.minimum_output_atoms, 99);
        assert_eq!(validated.slippage_bps, 100);
        assert_eq!(validated.router, "future_jupiter_engine");
    }

    #[test]
    fn unfunded_quotes_preserve_pricing_and_provider_errors_but_cannot_execute() {
        for router in ["metis", "dflow", "okx", "jupiterz"] {
            for transaction in [json!(""), Value::Null] {
                let mut value = response();
                value["router"] = json!(router);
                value["transaction"] = transaction;
                value["lastValidBlockHeight"] = Value::Null;
                value["errorCode"] = json!(1);
                value["errorMessage"] = json!("Insufficient funds");
                let response: MetaOrderResponse = serde_json::from_value(value).unwrap();
                let quote = validate_meta_quote(&intent(), WALLET, response.clone()).unwrap();
                assert_eq!(quote.expected_output_atoms, 100);
                assert_eq!(quote.minimum_output_atoms, 99);
                assert!(!response.transaction_available());
                assert!(
                    response
                        .quote_next_action()
                        .contains("Once the required input funds are available")
                );
                let error = validate_meta_order(&intent(), WALLET, response.clone()).unwrap_err();
                assert_eq!(error.code, "JUPITER_ORDER_EMPTY");
                assert_eq!(
                    serde_json::to_value(error).unwrap()["details"]["provider_error"],
                    json!({
                        "router": router, "code": 1, "message": "Insufficient funds"
                    })
                );
            }
        }
    }

    #[test]
    fn provider_errors_are_router_specific_and_unknown_errors_are_preserved() {
        for (router, code, message, expected_guidance) in [
            (
                "metis",
                2,
                "Insufficient SOL",
                "Once sufficient SOL for gas",
            ),
            (
                "jupiterz",
                2,
                "Missing associated token account",
                "Resolve the provider's transaction blocker",
            ),
            (
                "future-engine",
                1,
                "New provider condition",
                "Resolve the provider's transaction blocker",
            ),
            (
                "metis",
                999,
                "Unknown condition",
                "Resolve the provider's transaction blocker",
            ),
        ] {
            let mut value = response();
            value["router"] = json!(router);
            value["errorCode"] = json!(code);
            value["errorMessage"] = json!(message);
            let response: MetaOrderResponse = serde_json::from_value(value).unwrap();
            assert!(validate_meta_quote(&intent(), WALLET, response.clone()).is_ok());
            // Even an inconsistent provider response with transaction bytes and
            // an error must never become an execution candidate.
            assert!(!response.transaction_available());
            assert!(response.quote_next_action().contains(expected_guidance));
            assert_eq!(response.provider_error().unwrap()["message"], message);
            assert_eq!(
                validate_meta_order(&intent(), WALLET, response)
                    .unwrap_err()
                    .code,
                "JUPITER_ORDER_EMPTY"
            );
        }
    }

    #[test]
    fn quote_only_responses_still_require_bound_pricing_and_slippage_limits() {
        for (field, invalid_value) in [
            ("inputMint", json!(USDC)),
            ("inAmount", json!("101")),
            ("outAmount", json!("0")),
            ("otherAmountThreshold", json!("101")),
            ("slippageBps", json!(101)),
            ("feeBps", json!(11)),
        ] {
            let mut value = response();
            value["transaction"] = json!("");
            value["errorCode"] = json!(1);
            value[field] = invalid_value;
            let response = serde_json::from_value(value).unwrap();
            assert!(
                validate_meta_quote(&intent(), WALLET, response).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn slippage_rejection_reports_order_rtse_and_approved_limit() {
        let mut response = response();
        response["slippageBps"] = json!(102);
        let error = validate(response).unwrap_err();
        assert_eq!(error.code, "JUPITER_SLIPPAGE_LIMIT_EXCEEDED");
        assert_eq!(error.stage, "prepare");
        assert_eq!(
            error.message,
            "Jupiter's RTSE slippage is 102 bps (1.02%), exceeding the approved maximum of 100 bps (1.00%)."
        );
        let serialized = serde_json::to_value(&error).unwrap();
        assert_eq!(
            serialized["details"],
            json!({
                "actual_slippage_bps": 102,
                "maximum_slippage_bps": 100,
            })
        );
    }

    #[test]
    fn exact_order_identity_and_each_financial_limit_are_enforced() {
        for (field, value, code) in [
            ("inputMint", json!(USDC), "JUPITER_ORDER_BINDING_MISMATCH"),
            (
                "outputMint",
                json!(WRAPPED_SOL_MINT),
                "JUPITER_ORDER_BINDING_MISMATCH",
            ),
            ("inAmount", json!("101"), "JUPITER_ORDER_BINDING_MISMATCH"),
            ("taker", json!(USDC), "JUPITER_ORDER_BINDING_MISMATCH"),
            (
                "swapMode",
                json!("ExactOut"),
                "JUPITER_ORDER_BINDING_MISMATCH",
            ),
            ("outAmount", json!("89"), "JUPITER_MINIMUM_OUTPUT_UNMET"),
            (
                "otherAmountThreshold",
                json!("89"),
                "JUPITER_MINIMUM_OUTPUT_UNMET",
            ),
            (
                "otherAmountThreshold",
                json!("101"),
                "JUPITER_MINIMUM_OUTPUT_UNMET",
            ),
            ("slippageBps", json!(101), "JUPITER_SLIPPAGE_LIMIT_EXCEEDED"),
            ("feeBps", json!(11), "JUPITER_PLATFORM_FEE_LIMIT_EXCEEDED"),
            (
                "feeMint",
                Value::Null,
                "JUPITER_PLATFORM_FEE_LIMIT_EXCEEDED",
            ),
            (
                "signatureFeeLamports",
                json!(5001),
                "JUPITER_NETWORK_FEE_LIMIT_EXCEEDED",
            ),
            (
                "prioritizationFeeLamports",
                json!(2001),
                "JUPITER_NETWORK_FEE_LIMIT_EXCEEDED",
            ),
            (
                "rentFeeLamports",
                json!(2039281),
                "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED",
            ),
            ("rentFeePayer", Value::Null, "JUPITER_ORDER_INVALID"),
            ("signatureFeePayer", Value::Null, "JUPITER_ORDER_INVALID"),
            (
                "prioritizationFeePayer",
                Value::Null,
                "JUPITER_ORDER_INVALID",
            ),
        ] {
            let mut response = response();
            response[field] = value;
            assert_eq!(validate(response).unwrap_err().code, code, "field {field}");
        }
    }

    #[test]
    fn sponsor_fees_do_not_consume_wallet_limits_and_missing_quotes_do_not_become_zero() {
        let mut response = response();
        for payer in [
            "signatureFeePayer",
            "prioritizationFeePayer",
            "rentFeePayer",
        ] {
            response[payer] = json!(USDC);
        }
        let validated = validate(response).unwrap();
        assert_eq!(validated.wallet_network_fee_lamports, 0);
        assert_eq!(validated.wallet_account_funding_lamports, 0);
        for field in [
            "inputMint",
            "outputMint",
            "inAmount",
            "otherAmountThreshold",
            "swapMode",
            "slippageBps",
            "taker",
            "feeBps",
            "signatureFeeLamports",
            "prioritizationFeeLamports",
            "rentFeeLamports",
        ] {
            let mut response = self::response();
            response.as_object_mut().unwrap().remove(field);
            assert!(validate(response).is_err(), "missing {field}");
        }
    }

    #[test]
    fn opaque_transaction_is_not_decoded_by_order_limit_validation() {
        let mut response = response();
        response["transaction"] = json!("provider-owned-opaque-transaction");
        assert!(validate(response).is_ok());
    }

    #[test]
    fn old_route_inputs_are_rejected_and_not_advertised() {
        for route in ["router", "meta_aggregator"] {
            let mut input = serde_json::to_value(intent()).unwrap();
            input["route"] = json!(route);
            assert!(serde_json::from_value::<SwapIntent>(input).is_err());
        }
        assert!(
            definition_input_schema()["properties"]
                .get("route")
                .is_none()
        );
        assert!(
            !definition_input_schema()["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "route")
        );
    }

    #[test]
    fn explicit_wrapped_sol_is_rejected_instead_of_silently_spending_native() {
        let native = intent();
        assert!(
            meta_order_path(&native, WALLET)
                .unwrap()
                .contains(WRAPPED_SOL_MINT)
        );
        for output in [false, true] {
            let mut explicit = native.clone();
            if output {
                explicit.output_mint = WRAPPED_SOL_MINT.into();
            } else {
                explicit.input_mint = WRAPPED_SOL_MINT.into();
            }
            assert_eq!(
                validate_intent(&explicit).unwrap_err().code,
                "JUPITER_WRAPPED_SOL_UNSUPPORTED"
            );
        }
        for invalid in ["01", "1e3", "-1", "0", "18446744073709551616"] {
            let mut intent = native.clone();
            intent.amount_atoms = invalid.into();
            assert!(validate_intent(&intent).is_err());
        }
    }

    #[test]
    fn provider_lifetime_accepts_string_height_and_rfq_timestamp() {
        assert_eq!(
            validate(response()).unwrap().last_valid_block_height,
            Some(123)
        );
        let mut response = response();
        response["router"] = json!("jupiterz");
        response["lastValidBlockHeight"] = Value::Null;
        assert_eq!(
            validate(response.clone()).unwrap_err().code,
            "JUPITER_EXPIRY_MISSING"
        );
        response["expireAt"] = json!("1798761600");
        assert_eq!(
            validate(response.clone())
                .unwrap()
                .provider_expire_at
                .as_deref(),
            Some("2027-01-01T00:00:00+00:00")
        );
        for invalid in [json!("01"), json!("-1"), json!(true), json!("garbage")] {
            response["expireAt"] = invalid;
            assert!(validate(response.clone()).is_err());
        }
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("1e3"),
            json!("18446744073709551616"),
        ] {
            let mut response = self::response();
            response["lastValidBlockHeight"] = invalid;
            assert!(validate(response).is_err());
        }
    }
}
