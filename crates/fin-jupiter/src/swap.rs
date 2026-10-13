//! Strict contracts and instruction decoding for Jupiter Swap API V2.

use fin_domain::{FinError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

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
    /// Exact base-unit amount, without decimal formatting or exponent.
    pub amount_atoms: String,
    pub minimum_output_atoms: String,
    pub maximum_slippage_bps: u16,
    /// Maximum explicitly approved Jupiter platform/referral fee in basis points.
    pub maximum_platform_fee_bps: u16,
    pub maximum_network_fee_lamports: u64,
    /// Maximum SOL spent on token-account/rent setup in addition to swap input.
    pub maximum_account_funding_lamports: u64,
    /// The human/agent explicitly chooses the route family.
    pub route: SwapRoute,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwapRoute {
    MetaAggregator,
    Router,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MetaOrderResponse {
    pub transaction: Option<String>,
    #[serde(rename = "requestId")]
    pub request_id: String,
    #[serde(rename = "outAmount")]
    pub out_amount: String,
    pub router: String,
    pub mode: String,
    #[serde(
        rename = "lastValidBlockHeight",
        default,
        deserialize_with = "deserialize_optional_u64_from_string_or_number"
    )]
    pub last_valid_block_height: Option<u64>,
    #[serde(rename = "feeBps")]
    pub fee_bps: Option<u16>,
    #[serde(rename = "feeMint")]
    pub fee_mint: Option<String>,
    #[serde(rename = "expireAt")]
    pub expire_at: Option<Value>,
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
            .ok_or_else(|| serde::de::Error::custom("expected an unsigned integer block height")),
        Some(Value::String(text))
            if !text.is_empty()
                && text.len() <= 20
                && text.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            text.parse::<u64>()
                .map(Some)
                .map_err(|_| serde::de::Error::custom("block height is outside the u64 range"))
        }
        Some(_) => Err(serde::de::Error::custom(
            "expected a decimal string or unsigned integer block height",
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedOrder {
    pub transaction_base64: String,
    pub request_id: String,
    pub expected_output_atoms: u64,
    pub last_valid_block_height: Option<u64>,
    pub provider_expire_at: Option<String>,
    pub router: String,
    pub fee_bps: u16,
    pub fee_mint: Option<String>,
}

pub fn meta_order_path(intent: &SwapIntent, address: &str) -> Result<String> {
    let (input, _) = validate_intent(intent)?;
    let taker = fin_solana::parse_address(address)?;
    let input_mint = intent_mint(&intent.input_mint)?;
    let output_mint = intent_mint(&intent.output_mint)?;
    Ok(format!(
        "/swap/v2/order?inputMint={input_mint}&outputMint={output_mint}&amount={input}&taker={taker}&slippageBps={}",
        intent.maximum_slippage_bps
    ))
}

pub fn validate_meta_order(
    intent: &SwapIntent,
    response: MetaOrderResponse,
) -> Result<ValidatedOrder> {
    let (_, minimum_output) = validate_intent(intent)?;
    let out_amount = exact_atoms(&response.out_amount)?;
    let transaction = response
        .transaction
        .filter(|tx| !tx.is_empty())
        .ok_or_else(|| {
            FinError::new(
                "JUPITER_ORDER_EMPTY",
                "prepare",
                "Jupiter did not return a transaction for the exact swap request.",
                "Request a fresh order for the same exact mints and amount.",
            )
        })?;
    if response.request_id.trim().is_empty()
        || response.request_id.len() > 256
        || response.router.trim().is_empty()
    {
        return Err(invalid_order().with_details(json!({"field":"requestId_or_router"})));
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
    let is_rfq = response.router == "jupiterz";
    let last_valid_block_height = response.last_valid_block_height;
    if (is_rfq && provider_expire_at.is_none()) || (!is_rfq && last_valid_block_height.is_none()) {
        return Err(FinError::new(
            "JUPITER_EXPIRY_MISSING",
            "prepare",
            "Jupiter did not return the route's applicable finite transaction or RFQ expiry.",
            "Request a fresh order with a valid blockhash height or maker quote expiry.",
        ));
    }
    if out_amount < minimum_output {
        return Err(FinError::new(
            "JUPITER_MINIMUM_OUTPUT_UNMET",
            "prepare",
            "Jupiter's exact order quote is below the approved minimum output.",
            "Request a new quote with a lower input, different route, or explicitly reviewed minimum.",
        ));
    }
    let fee_mint = response
        .fee_mint
        .map(|mint| fin_solana::parse_address(&mint))
        .transpose()?;
    let fee_bps = response.fee_bps.unwrap_or(0);
    if fee_bps > 10_000
        || fee_bps > intent.maximum_platform_fee_bps
        || (fee_bps > 0 && fee_mint.is_none())
    {
        return Err(FinError::new(
            "JUPITER_PLATFORM_FEE_LIMIT_EXCEEDED",
            "prepare",
            "The order includes a platform fee above the approved limit or without an exact fee mint.",
            "Review the exact fee amount and mint before authorizing this route.",
        ));
    }
    Ok(ValidatedOrder {
        transaction_base64: transaction,
        request_id: response.request_id,
        expected_output_atoms: out_amount,
        last_valid_block_height,
        provider_expire_at,
        router: response.router,
        fee_bps,
        fee_mint,
    })
}

pub fn router_build_path(intent: &SwapIntent, address: &str) -> Result<String> {
    let (input, _) = validate_intent(intent)?;
    let taker = fin_solana::parse_address(address)?;
    let wrap_and_unwrap_sol = intent.input_mint == "native" || intent.output_mint == "native";
    Ok(format!(
        "/swap/v2/build?inputMint={}&outputMint={}&amount={input}&taker={taker}&slippageBps={}&wrapAndUnwrapSol={wrap_and_unwrap_sol}",
        intent_mint(&intent.input_mint)?,
        intent_mint(&intent.output_mint)?,
        intent.maximum_slippage_bps
    ))
}

pub fn validate_order_instruction(
    intent: &SwapIntent,
    decoded: &DecodedSwapInstruction,
) -> Result<()> {
    let (input_atoms, minimum_output_atoms) = validate_intent(intent)?;
    if decoded.positive_slippage_bps != 0 {
        return Err(FinError::new(
            "JUPITER_POSITIVE_SLIPPAGE_UNSUPPORTED",
            "prepare",
            "The transaction routes positive slippage to an additional fee account.",
            "Request a fresh transaction with positive slippage disabled.",
        ));
    }
    if decoded.platform_fee_bps > intent.maximum_platform_fee_bps {
        return Err(FinError::new(
            "JUPITER_PLATFORM_FEE_LIMIT_EXCEEDED",
            "prepare",
            "The provider transaction includes a platform fee above the explicitly approved limit.",
            "Review a fresh quote with a lower fee or approve a higher exact fee bound.",
        ));
    }
    if decoded.slippage_bps > intent.maximum_slippage_bps {
        return Err(FinError::new(
            "JUPITER_SLIPPAGE_LIMIT_EXCEEDED",
            "prepare",
            "The actual Jupiter instruction allows more slippage than the approved maximum.",
            "Request a fresh transaction with the approved slippage limit.",
        ));
    }
    if !matches!(
        decoded.variant,
        SwapInstructionVariant::ExactIn
            | SwapInstructionVariant::SharedExactIn
            | SwapInstructionVariant::ExactInV2
            | SwapInstructionVariant::SharedExactInV2
    ) || decoded.input_amount != Some(input_atoms)
    {
        return Err(FinError::new(
            "JUPITER_INPUT_BINDING_MISMATCH",
            "prepare",
            "The actual Jupiter instruction does not spend the exact approved input amount.",
            "Request a fresh transaction for the exact approved input amount.",
        ));
    }
    let actual_minimum = (u128::from(decoded.quoted_output)
        * (10_000u128 - u128::from(decoded.slippage_bps)))
        / 10_000u128;
    if actual_minimum < u128::from(minimum_output_atoms) {
        return Err(FinError::new(
            "JUPITER_MINIMUM_OUTPUT_UNPROTECTED",
            "prepare",
            "The actual Jupiter instruction does not enforce the approved minimum output.",
            "Request a fresh transaction whose on-chain minimum output meets the approved amount.",
        ));
    }
    Ok(())
}

fn intent_mint(mint: &str) -> Result<String> {
    if mint == "native" {
        return Ok(WRAPPED_SOL_MINT.into());
    }
    fin_solana::parse_address(mint)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSwapInstruction {
    pub variant: SwapInstructionVariant,
    pub authority_id: Option<u8>,
    pub input_amount: Option<u64>,
    pub quoted_output: u64,
    pub slippage_bps: u16,
    pub platform_fee_bps: u16,
    pub positive_slippage_bps: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapInstructionVariant {
    ExactIn,
    SharedExactIn,
    ExactOut,
    SharedExactOut,
    TokenLedgerExactIn,
    SharedTokenLedgerExactIn,
    ExactInV2,
    SharedExactInV2,
    ExactOutV2,
    SharedExactOutV2,
}

/// Decode reviewed Jupiter v6 and Swap API V2 instruction envelopes. Route
/// variants and their Borsh layouts are pinned in the bundled upstream IDL.
pub fn decode_swap_instruction(data: &[u8]) -> Result<DecodedSwapInstruction> {
    if data.len() < 8 {
        return Err(invalid_instruction());
    }
    let discriminator: [u8; 8] = data[..8].try_into().map_err(|_| invalid_instruction())?;
    let variant = match discriminator {
        [229, 23, 203, 151, 122, 227, 173, 42] => SwapInstructionVariant::ExactIn,
        [193, 32, 155, 51, 65, 214, 156, 129] => SwapInstructionVariant::SharedExactIn,
        [208, 51, 239, 151, 123, 43, 237, 92] => SwapInstructionVariant::ExactOut,
        [176, 209, 105, 168, 154, 125, 69, 62] => SwapInstructionVariant::SharedExactOut,
        [150, 86, 71, 116, 167, 93, 14, 104] => SwapInstructionVariant::TokenLedgerExactIn,
        [230, 121, 143, 80, 119, 159, 106, 170] => SwapInstructionVariant::SharedTokenLedgerExactIn,
        [187, 100, 250, 204, 49, 196, 175, 20] => SwapInstructionVariant::ExactInV2,
        [209, 152, 83, 147, 124, 254, 216, 233] => SwapInstructionVariant::SharedExactInV2,
        [157, 138, 184, 82, 21, 244, 243, 36] => SwapInstructionVariant::ExactOutV2,
        [53, 96, 229, 202, 216, 187, 250, 24] => SwapInstructionVariant::SharedExactOutV2,
        _ => return Err(unsupported_discriminator(&discriminator)),
    };
    let mut reader = Reader { bytes: data, at: 8 };
    let shared = matches!(
        variant,
        SwapInstructionVariant::SharedExactIn
            | SwapInstructionVariant::SharedExactOut
            | SwapInstructionVariant::SharedTokenLedgerExactIn
            | SwapInstructionVariant::SharedExactInV2
            | SwapInstructionVariant::SharedExactOutV2
    );
    let authority_id = if shared { Some(reader.u8()?) } else { None };
    let v2 = matches!(
        variant,
        SwapInstructionVariant::ExactInV2
            | SwapInstructionVariant::SharedExactInV2
            | SwapInstructionVariant::ExactOutV2
            | SwapInstructionVariant::SharedExactOutV2
    );
    let (input_amount, quoted_output, slippage_bps, platform_fee_bps, positive_slippage_bps) = if v2
    {
        let (input, output) = match variant {
            SwapInstructionVariant::ExactInV2 | SwapInstructionVariant::SharedExactInV2 => {
                (Some(reader.u64()?), reader.u64()?)
            }
            SwapInstructionVariant::ExactOutV2 | SwapInstructionVariant::SharedExactOutV2 => {
                let output = reader.u64()?;
                (Some(reader.u64()?), output)
            }
            _ => unreachable!(),
        };
        let slippage = reader.u16()?;
        let fee = reader.u16()?;
        let positive_slippage = reader.u16()?;
        if positive_slippage > 10_000 {
            return Err(invalid_instruction());
        }
        (input, output, slippage, fee, positive_slippage)
    } else {
        let plan_len = reader.u32()? as usize;
        if plan_len == 0 || plan_len > 32 {
            return Err(invalid_instruction());
        }
        for _ in 0..plan_len {
            reader.swap_step()?;
        }
        let (input, output) = match variant {
            SwapInstructionVariant::ExactIn | SwapInstructionVariant::SharedExactIn => {
                (Some(reader.u64()?), reader.u64()?)
            }
            SwapInstructionVariant::ExactOut | SwapInstructionVariant::SharedExactOut => {
                let output = reader.u64()?;
                (Some(reader.u64()?), output)
            }
            SwapInstructionVariant::TokenLedgerExactIn
            | SwapInstructionVariant::SharedTokenLedgerExactIn => (None, reader.u64()?),
            _ => unreachable!(),
        };
        (input, output, reader.u16()?, u16::from(reader.u8()?), 0)
    };
    if v2 {
        let plan_len = reader.u32()? as usize;
        if plan_len == 0 || plan_len > 32 {
            return Err(invalid_instruction());
        }
        for _ in 0..plan_len {
            reader.swap_step_v2()?;
        }
    }
    if reader.at != data.len() || slippage_bps > 10_000 {
        return Err(invalid_instruction());
    }
    Ok(DecodedSwapInstruction {
        variant,
        authority_id,
        input_amount,
        quoted_output,
        slippage_bps,
        platform_fee_bps,
        positive_slippage_bps,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Reader<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(count).ok_or_else(invalid_instruction)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(invalid_instruction)?;
        self.at = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| invalid_instruction())?,
        ))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| invalid_instruction())?,
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| invalid_instruction())?,
        ))
    }
    fn swap_step(&mut self) -> Result<()> {
        self.skip_swap_enum_payload()?;
        self.take(3)?; // percent, input index, output index
        Ok(())
    }
    fn swap_step_v2(&mut self) -> Result<()> {
        self.skip_swap_enum_payload()?;
        let bps = self.u16()?;
        if bps > 10_000 {
            return Err(invalid_instruction());
        }
        self.take(2)?; // input_index, output_index
        Ok(())
    }
    fn skip_swap_enum_payload(&mut self) -> Result<()> {
        let tag = usize::from(self.u8()?);
        let idl = jupiter_idl();
        let swap_type = idl["types"]
            .as_array()
            .and_then(|types| types.iter().find(|t| t["name"] == "Swap"))
            .ok_or_else(invalid_instruction)?;
        let variant = swap_type["type"]["variants"]
            .as_array()
            .and_then(|variants| variants.get(tag))
            .ok_or_else(invalid_instruction)?;
        if let Some(fields) = variant["fields"].as_array() {
            for field in fields {
                self.skip_idl_type(&field["type"], idl, 0)?;
            }
        }
        Ok(())
    }
    fn boolean(&mut self) -> Result<()> {
        if self.u8()? > 1 {
            return Err(invalid_instruction());
        }
        Ok(())
    }
    fn skip_idl_type(&mut self, ty: &Value, idl: &Value, depth: usize) -> Result<()> {
        if depth > 16 {
            return Err(invalid_instruction());
        }
        if let Some(name) = ty.as_str() {
            let width = match name {
                "bool" => {
                    self.boolean()?;
                    return Ok(());
                }
                "u8" | "i8" => 1,
                "u16" | "i16" => 2,
                "u32" | "i32" | "f32" => 4,
                "u64" | "i64" | "f64" => 8,
                "u128" | "i128" => 16,
                "pubkey" => 32,
                "bytes" | "string" => {
                    let len = self.u32()? as usize;
                    if len > 4096 {
                        return Err(invalid_instruction());
                    }
                    self.take(len)?;
                    return Ok(());
                }
                _ => return Err(invalid_instruction()),
            };
            self.take(width)?;
            return Ok(());
        }
        if let Some(inner) = ty.get("defined") {
            let name = inner["name"].as_str().ok_or_else(invalid_instruction)?;
            let def = idl["types"]
                .as_array()
                .and_then(|types| types.iter().find(|t| t["name"] == name))
                .ok_or_else(invalid_instruction)?;
            match def["type"]["kind"].as_str() {
                Some("struct") => {
                    for field in def["type"]["fields"]
                        .as_array()
                        .ok_or_else(invalid_instruction)?
                    {
                        self.skip_idl_type(&field["type"], idl, depth + 1)?;
                    }
                }
                Some("enum") => {
                    let tag = usize::from(self.u8()?);
                    let variant = def["type"]["variants"]
                        .as_array()
                        .and_then(|v| v.get(tag))
                        .ok_or_else(invalid_instruction)?;
                    if let Some(fields) = variant["fields"].as_array() {
                        for field in fields {
                            self.skip_idl_type(&field["type"], idl, depth + 1)?;
                        }
                    }
                }
                _ => return Err(invalid_instruction()),
            }
            return Ok(());
        }
        if let Some(items) = ty.get("array").and_then(Value::as_array) {
            let item = items.first().ok_or_else(invalid_instruction)?;
            let len = items
                .get(1)
                .and_then(Value::as_u64)
                .ok_or_else(invalid_instruction)? as usize;
            if len > 4096 {
                return Err(invalid_instruction());
            }
            for _ in 0..len {
                self.skip_idl_type(item, idl, depth + 1)?;
            }
            return Ok(());
        }
        if let Some(inner) = ty.get("option") {
            match self.u8()? {
                0 => return Ok(()),
                1 => self.skip_idl_type(inner, idl, depth + 1)?,
                _ => return Err(invalid_instruction()),
            }
            return Ok(());
        }
        if let Some(inner) = ty.get("vec") {
            let len = self.u32()? as usize;
            if len > 64 {
                return Err(invalid_instruction());
            }
            for _ in 0..len {
                self.skip_idl_type(inner, idl, depth + 1)?;
            }
            return Ok(());
        }
        Err(invalid_instruction())
    }
}

fn jupiter_idl() -> &'static Value {
    static IDL: OnceLock<Value> = OnceLock::new();
    IDL.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/jupiter-swap-v6-idl.json"))
            .expect("pinned Jupiter IDL must remain valid JSON")
    })
}

pub fn validate_intent(intent: &SwapIntent) -> Result<(u64, u64)> {
    if intent.network != "solana"
        || intent.wallet.trim().is_empty()
        || intent.input_mint == intent.output_mint
        || intent.maximum_slippage_bps > 10_000
    {
        return Err(FinError::validation(
            "A Jupiter swap requires Solana, distinct exact mint identities, a wallet and bounded slippage.",
        ));
    }
    let input = exact_atoms(&intent.amount_atoms)?;
    let minimum = exact_atoms(&intent.minimum_output_atoms)?;
    if input == 0 || minimum == 0 {
        return Err(FinError::validation(
            "Swap input and minimum output must be positive exact atom amounts.",
        ));
    }
    for mint in [&intent.input_mint, &intent.output_mint] {
        if mint != "native" {
            fin_solana::parse_address(mint)?;
        }
    }
    Ok((input, minimum))
}

pub fn definition_input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["wallet","network","cluster","input_mint","output_mint","amount_atoms","minimum_output_atoms","maximum_slippage_bps","maximum_platform_fee_bps","maximum_network_fee_lamports","maximum_account_funding_lamports","route"],"properties":{"wallet":{"type":"string","minLength":1},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"input_mint":{"type":"string","description":"Exact SPL mint address or native SOL."},"output_mint":{"type":"string","description":"Exact SPL mint address or native SOL."},"amount_atoms":{"type":"string","pattern":"^[0-9]+$"},"minimum_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000},"maximum_platform_fee_bps":{"type":"integer","minimum":0,"maximum":10000},"maximum_network_fee_lamports":{"type":"integer","minimum":0},"maximum_account_funding_lamports":{"type":"integer","minimum":0},"route":{"type":"string","enum":["meta_aggregator","router"]}}})
}

pub fn definition_output_schema() -> Value {
    json!({"type":"object","properties":{"network":{"const":"solana:mainnet-beta"},"cluster":{"const":"mainnet-beta"},"wallet":{"type":"string"},"input_mint":{"type":"string"},"output_mint":{"type":"string"},"input_atoms":{"type":"string","pattern":"^[0-9]+$"},"expected_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"minimum_output_atoms":{"type":"string","pattern":"^[0-9]+$"},"maximum_slippage_bps":{"type":"integer","minimum":0,"maximum":10000},"route":{"enum":["meta_aggregator","router"]},"provider_request_id":{"type":["string","null"]},"last_valid_block_height":{"type":["integer","null"],"minimum":1},"provider_expire_at":{"type":["string","null"],"format":"date-time"},"fees":{"type":"object","properties":{"estimated_network_fee_lamports":{"type":["string","null"]},"network_fee_estimate_status":{"type":"string"},"maximum_network_fee_lamports":{"type":"string"},"maximum_platform_fee_bps":{"type":"integer","minimum":0,"maximum":10000},"platform_fee_mint":{"type":["string","null"]},"account_funding_estimate_lamports":{"type":["string","null"]},"account_funding_estimate_status":{"type":"string"}},"required":["network_fee_estimate_status","maximum_platform_fee_bps","account_funding_estimate_status"]},"expires_at":{"type":"string","format":"date-time"},"operation_id":{"type":"string"},"status":{"enum":["prepared","submitted","pending","completed","failed"]},"signature":{"type":["string","null"]},"verified":{"type":"boolean"},"evidence":{"type":["object","null"]}},"additionalProperties":true})
}

fn exact_atoms(value: &str) -> Result<u64> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(FinError::validation(
            "Financial atom amounts must be canonical unsigned decimal integer strings.",
        ));
    }
    value
        .parse::<u64>()
        .map_err(|_| FinError::validation("The atom amount is outside the supported exact range."))
}

fn invalid_instruction() -> FinError {
    FinError::new(
        "JUPITER_INSTRUCTION_INVALID",
        "prepare",
        "The transaction does not contain a valid supported Jupiter swap instruction.",
        "Request a fresh Swap API V2 transaction and inspect its exact route and limits.",
    )
}

fn unsupported_discriminator(discriminator: &[u8; 8]) -> FinError {
    FinError::new(
        "JUPITER_INSTRUCTION_UNSUPPORTED",
        "prepare",
        format!(
            "Jupiter instruction discriminator {} is not supported.",
            hex::encode(discriminator)
        ),
        "Request a transaction using a supported Jupiter route instruction.",
    )
}

fn invalid_order() -> FinError {
    FinError::new(
        "JUPITER_ORDER_INVALID",
        "prepare",
        "Jupiter returned an incomplete or inconsistent order response.",
        "Request a fresh order for the exact approved swap intent.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_data(discriminator: [u8; 8], trailing: &[u8]) -> Vec<u8> {
        let mut data = discriminator.to_vec();
        if matches!(
            discriminator,
            [193, 32, 155, 51, 65, 214, 156, 129]
                | [176, 209, 105, 168, 154, 125, 69, 62]
                | [230, 121, 143, 80, 119, 159, 106, 170]
                | [209, 152, 83, 147, 124, 254, 216, 233]
                | [53, 96, 229, 202, 216, 187, 250, 24]
        ) {
            data.push(7);
        }
        data.extend_from_slice(&1u32.to_le_bytes()); // one RoutePlanStep
        data.extend_from_slice(&[0, 100, 0, 1]); // Saber, 100%, input/output indices
        data.extend_from_slice(trailing);
        data
    }

    #[test]
    fn decodes_actual_router_instruction_amount_slippage_and_fee() {
        let mut tail = Vec::new();
        tail.extend_from_slice(&123u64.to_le_bytes());
        tail.extend_from_slice(&456u64.to_le_bytes());
        tail.extend_from_slice(&50u16.to_le_bytes());
        tail.push(0);
        let decoded =
            decode_swap_instruction(&route_data([229, 23, 203, 151, 122, 227, 173, 42], &tail))
                .unwrap();
        assert_eq!(decoded.variant, SwapInstructionVariant::ExactIn);
        assert_eq!(decoded.input_amount, Some(123));
        assert_eq!(decoded.quoted_output, 456);
        assert_eq!(decoded.slippage_bps, 50);
        assert_eq!(decoded.platform_fee_bps, 0);
        let mut trailing = route_data([229, 23, 203, 151, 122, 227, 173, 42], &tail);
        trailing.push(0);
        assert!(decode_swap_instruction(&trailing).is_err());
    }

    #[test]
    fn decodes_pinned_idl_shared_exact_out_and_token_ledger_layouts() {
        let mut exact_out = Vec::new();
        exact_out.extend_from_slice(&456u64.to_le_bytes()); // desired output
        exact_out.extend_from_slice(&123u64.to_le_bytes()); // quoted maximum input
        exact_out.extend_from_slice(&25u16.to_le_bytes());
        exact_out.push(0);
        let decoded = decode_swap_instruction(&route_data(
            [208, 51, 239, 151, 123, 43, 237, 92],
            &exact_out,
        ))
        .unwrap();
        assert_eq!(decoded.variant, SwapInstructionVariant::ExactOut);
        assert_eq!(decoded.quoted_output, 456);
        assert_eq!(decoded.input_amount, Some(123));

        let mut shared_exact_out = Vec::new();
        shared_exact_out.extend_from_slice(&456u64.to_le_bytes());
        shared_exact_out.extend_from_slice(&123u64.to_le_bytes());
        shared_exact_out.extend_from_slice(&25u16.to_le_bytes());
        shared_exact_out.push(0);
        let decoded = decode_swap_instruction(&route_data(
            [176, 209, 105, 168, 154, 125, 69, 62],
            &shared_exact_out,
        ))
        .unwrap();
        assert_eq!(decoded.variant, SwapInstructionVariant::SharedExactOut);
        assert_eq!(decoded.authority_id, Some(7));

        let mut token_ledger = Vec::new();
        token_ledger.extend_from_slice(&456u64.to_le_bytes());
        token_ledger.extend_from_slice(&25u16.to_le_bytes());
        token_ledger.push(0);
        let decoded = decode_swap_instruction(&route_data(
            [150, 86, 71, 116, 167, 93, 14, 104],
            &token_ledger,
        ))
        .unwrap();
        assert_eq!(decoded.variant, SwapInstructionVariant::TokenLedgerExactIn);
        assert_eq!(decoded.authority_id, None);
        assert_eq!(decoded.input_amount, None);
        let shared_ledger = decode_swap_instruction(&route_data(
            [230, 121, 143, 80, 119, 159, 106, 170],
            &token_ledger,
        ))
        .unwrap();
        assert_eq!(shared_ledger.authority_id, Some(7));
    }

    fn current_route_v2_data(fee_bps: u16) -> Vec<u8> {
        let mut data = vec![187, 100, 250, 204, 49, 196, 175, 20];
        data.extend_from_slice(&19_000_000u64.to_le_bytes());
        data.extend_from_slice(&2_024_665u64.to_le_bytes());
        data.extend_from_slice(&50u16.to_le_bytes());
        data.extend_from_slice(&fee_bps.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes()); // positive_slippage_bps
        data.extend_from_slice(&2u32.to_le_bytes());
        // Published IDL Swap variants KipseliV3 (bool payload) and Deriverse
        // (Side enum plus u32 instruction id), with exact route bps and indices.
        data.extend_from_slice(&[181, 1]);
        data.extend_from_slice(&7_224u16.to_le_bytes());
        data.extend_from_slice(&[0, 2]);
        data.extend_from_slice(&[161, 1]);
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&2_776u16.to_le_bytes());
        data.extend_from_slice(&[0, 2]);
        data
    }

    #[test]
    fn decodes_current_route_v2_layout_and_pinned_nested_route_enums() {
        let decoded = decode_swap_instruction(&current_route_v2_data(2)).unwrap();
        assert_eq!(decoded.variant, SwapInstructionVariant::ExactInV2);
        assert_eq!(decoded.input_amount, Some(19_000_000));
        assert_eq!(decoded.quoted_output, 2_024_665);
        assert_eq!(decoded.slippage_bps, 50);
        assert_eq!(decoded.platform_fee_bps, 2);
        assert_eq!(decoded.positive_slippage_bps, 0);
    }

    #[test]
    fn route_v2_rejects_malformed_enum_options_bounds_and_suffixes() {
        let mut malformed_bool = current_route_v2_data(0);
        malformed_bool[35] = 2;
        assert!(decode_swap_instruction(&malformed_bool).is_err());

        let mut unknown_tag = current_route_v2_data(0);
        unknown_tag[34] = 197; // outside the pinned 197-variant enum
        assert!(decode_swap_instruction(&unknown_tag).is_err());

        let mut excessive_bps = current_route_v2_data(0);
        excessive_bps[36..38].copy_from_slice(&10_001u16.to_le_bytes());
        assert!(decode_swap_instruction(&excessive_bps).is_err());

        let mut trailing = current_route_v2_data(0);
        trailing.push(0);
        assert!(decode_swap_instruction(&trailing).is_err());
        assert!(decode_swap_instruction(&current_route_v2_data(0)[..49]).is_err());
    }

    #[test]
    fn pinned_v2_idl_matches_reviewed_sha256_and_instruction_set() {
        use sha2::{Digest, Sha256};
        let digest = hex::encode(Sha256::digest(include_bytes!(
            "../resources/jupiter-swap-v6-idl.json"
        )));
        assert_eq!(
            digest,
            "ac409554b289a63e035717a516240c93adc4b8d894d4668fde254e73c7cfdb47"
        );
        let instructions = jupiter_idl()["instructions"].as_array().unwrap();
        for name in [
            "route_v2",
            "exact_out_route_v2",
            "shared_accounts_route_v2",
            "shared_accounts_exact_out_route_v2",
        ] {
            assert!(
                instructions
                    .iter()
                    .any(|instruction| instruction["name"] == name)
            );
        }
    }

    #[test]
    fn intent_uses_exact_atoms_and_preserves_case_sensitive_mint_identity() {
        let intent = SwapIntent {
            wallet: "sol-main".into(),
            network: "solana".into(),
            input_mint: WRAPPED_SOL_MINT.into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_atoms: "1".into(),
            minimum_output_atoms: "1".into(),
            maximum_slippage_bps: 10,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::Router,
        };
        assert_eq!(validate_intent(&intent).unwrap(), (1, 1));
        let explicit_wsol = router_build_path(&intent, "11111111111111111111111111111111").unwrap();
        assert!(explicit_wsol.ends_with("wrapAndUnwrapSol=false"));
        let native_sol = router_build_path(
            &SwapIntent {
                input_mint: "native".into(),
                ..intent.clone()
            },
            "11111111111111111111111111111111",
        )
        .unwrap();
        assert!(native_sol.ends_with("wrapAndUnwrapSol=true"));
        assert!(
            validate_intent(&SwapIntent {
                amount_atoms: "1e3".into(),
                ..intent
            })
            .is_err()
        );
    }

    #[test]
    fn rfq_order_uses_quote_timestamp_expiry_while_aggregator_requires_height() {
        let intent = SwapIntent {
            wallet: "sol-main".into(),
            network: "solana".into(),
            input_mint: WRAPPED_SOL_MINT.into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_atoms: "100".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 100,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::MetaAggregator,
        };
        let rfq = MetaOrderResponse {
            transaction: Some("AQ==".into()),
            request_id: "req-1".into(),
            out_amount: "100".into(),
            router: "jupiterz".into(),
            mode: "ExactIn".into(),
            last_valid_block_height: None,
            fee_bps: None,
            fee_mint: None,
            expire_at: Some(json!("2030-01-01T00:00:00Z")),
        };
        let validated = validate_meta_order(&intent, rfq).unwrap();
        assert_eq!(validated.last_valid_block_height, None);
        assert_eq!(
            validated.provider_expire_at.as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );

        let aggregator = MetaOrderResponse {
            transaction: Some("AQ==".into()),
            request_id: "req-2".into(),
            out_amount: "100".into(),
            router: "dflow".into(),
            mode: "ExactIn".into(),
            last_valid_block_height: None,
            fee_bps: None,
            fee_mint: None,
            expire_at: None,
        };
        assert!(validate_meta_order(&intent, aggregator).is_err());
    }

    #[test]
    fn empty_optional_expiry_preserves_required_route_expiry() {
        let intent = SwapIntent {
            wallet: "synthetic".into(),
            network: "solana".into(),
            input_mint: WRAPPED_SOL_MINT.into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_atoms: "100".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 50,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::MetaAggregator,
        };
        let response = |router: &str, height: Option<u64>, expiry: Value| {
            serde_json::from_value::<MetaOrderResponse>(json!({
                "transaction":"AQ==", "requestId":"synthetic-order", "outAmount":"100",
                "router":router, "mode":"manual", "lastValidBlockHeight":height,
                "feeBps":0, "expireAt":expiry,
            }))
            .unwrap()
        };
        let validated =
            validate_meta_order(&intent, response("metis", Some(123), json!(""))).unwrap();
        assert_eq!(validated.last_valid_block_height, Some(123));
        assert_eq!(validated.provider_expire_at, None);
        for router in ["metis", "jupiterz"] {
            let error =
                validate_meta_order(&intent, response(router, None, json!(""))).unwrap_err();
            assert_eq!(error.code, "JUPITER_EXPIRY_MISSING");
        }
        let error =
            validate_meta_order(&intent, response("jupiterz", Some(123), json!(""))).unwrap_err();
        assert_eq!(error.code, "JUPITER_EXPIRY_MISSING");
        // Malformed nonempty timestamps never become a new or omitted expiry.
        for expiry in [json!(" "), json!("garbage"), json!(true)] {
            assert_eq!(
                validate_meta_order(&intent, response("metis", Some(123), expiry))
                    .unwrap_err()
                    .code,
                "JUPITER_ORDER_INVALID"
            );
        }
    }

    #[test]
    fn meta_order_accepts_canonical_unix_seconds_string_expiry() {
        let intent = SwapIntent {
            wallet: "synthetic".into(),
            network: "solana".into(),
            input_mint: WRAPPED_SOL_MINT.into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_atoms: "100".into(),
            minimum_output_atoms: "90".into(),
            maximum_slippage_bps: 50,
            maximum_platform_fee_bps: 0,
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            route: SwapRoute::MetaAggregator,
        };
        let response = |expiry: Value| {
            serde_json::from_value::<MetaOrderResponse>(json!({
                "transaction":"AQ==", "requestId":"synthetic-order", "outAmount":"100",
                "router":"jupiterz", "mode":"manual", "lastValidBlockHeight":null,
                "feeBps":0, "expireAt":expiry,
            }))
            .unwrap()
        };

        let seconds_string = validate_meta_order(&intent, response(json!("1798761600"))).unwrap();
        let seconds_number = validate_meta_order(&intent, response(json!(1798761600))).unwrap();
        assert_eq!(
            seconds_string.provider_expire_at,
            seconds_number.provider_expire_at
        );
        assert_eq!(
            seconds_string.provider_expire_at.as_deref(),
            Some("2027-01-01T00:00:00+00:00")
        );

        for invalid in [
            "-1",
            "+1",
            "01",
            "1.0",
            "1e3",
            " 1234567890",
            "1234567890 ",
            "9223372036854775808",
            "9000000000000", // Beyond chrono's supported timestamp range.
        ] {
            assert_eq!(
                validate_meta_order(&intent, response(json!(invalid)))
                    .unwrap_err()
                    .code,
                "JUPITER_ORDER_INVALID",
                "unexpectedly accepted expiry {invalid:?}"
            );
        }
        assert_eq!(
            validate_meta_order(&intent, response(json!("253402300800")))
                .unwrap()
                .provider_expire_at,
            validate_meta_order(&intent, response(json!(253402300800_i64)))
                .unwrap()
                .provider_expire_at
        );
    }

    #[test]
    fn order_block_height_accepts_decimal_text_from_current_order_response() {
        let parse = |height: Value| {
            serde_json::from_value::<MetaOrderResponse>(json!({
                "transaction": "AQ==",
                "requestId": "request-1",
                "outAmount": "100",
                "router": "metis",
                "mode": "manual",
                "lastValidBlockHeight": height,
                "feeBps": 0,
                "feeMint": "So11111111111111111111111111111111111111112"
            }))
        };

        assert_eq!(
            parse(json!("123456789")).unwrap().last_valid_block_height,
            Some(123_456_789)
        );
        assert_eq!(
            parse(json!(123456789)).unwrap().last_valid_block_height,
            Some(123_456_789)
        );
        assert_eq!(parse(Value::Null).unwrap().last_valid_block_height, None);
    }

    #[test]
    fn order_block_height_rejects_negative_fractional_and_overflow_values() {
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("-1"),
            json!("1.5"),
            json!("18446744073709551616"),
        ] {
            let response = json!({
                "transaction": "AQ==",
                "requestId": "request-1",
                "outAmount": "100",
                "router": "metis",
                "mode": "manual",
                "lastValidBlockHeight": invalid,
                "feeBps": 0,
                "feeMint": "So11111111111111111111111111111111111111112"
            });
            assert!(serde_json::from_value::<MetaOrderResponse>(response).is_err());
        }
    }
}
