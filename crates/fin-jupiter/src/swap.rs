//! Strict contracts and instruction decoding for Jupiter Swap API V2.

use fin_domain::{FinError, Result};
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
    #[serde(rename = "lastValidBlockHeight")]
    pub last_valid_block_height: Option<u64>,
    #[serde(rename = "feeBps")]
    pub fee_bps: Option<u16>,
    #[serde(rename = "feeMint")]
    pub fee_mint: Option<String>,
    #[serde(rename = "expireAt")]
    pub expire_at: Option<Value>,
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
        return Err(invalid_order());
    }
    let provider_expire_at = match response.expire_at {
        Some(Value::String(value)) => {
            let parsed =
                chrono::DateTime::parse_from_rfc3339(&value).map_err(|_| invalid_order())?;
            Some(parsed.to_utc().to_rfc3339())
        }
        Some(Value::Number(value)) => {
            let timestamp = value.as_i64().ok_or_else(invalid_order)?;
            Some(
                chrono::DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(invalid_order)?
                    .to_rfc3339(),
            )
        }
        Some(_) => return Err(invalid_order()),
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
    if u16::from(decoded.platform_fee_bps) > intent.maximum_platform_fee_bps {
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
        SwapInstructionVariant::ExactIn | SwapInstructionVariant::SharedExactIn
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
    pub platform_fee_bps: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapInstructionVariant {
    ExactIn,
    SharedExactIn,
    ExactOut,
    SharedExactOut,
    TokenLedgerExactIn,
    SharedTokenLedgerExactIn,
}

/// Decode the reviewed Jupiter v6 aggregator instruction envelope. The route
/// plan is skipped according to the pinned upstream IDL's Borsh layout; unknown
/// enum variants and trailing bytes fail closed. Pool programs remain dynamic.
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
        _ => return Err(invalid_instruction()),
    };
    let mut reader = Reader { bytes: data, at: 8 };
    let shared = matches!(
        variant,
        SwapInstructionVariant::SharedExactIn
            | SwapInstructionVariant::SharedExactOut
            | SwapInstructionVariant::SharedTokenLedgerExactIn
    );
    let authority_id = if shared { Some(reader.u8()?) } else { None };
    let plan_len = reader.u32()? as usize;
    if plan_len == 0 || plan_len > 32 {
        return Err(invalid_instruction());
    }
    for _ in 0..plan_len {
        reader.swap_step()?;
    }
    let (input_amount, quoted_output) = match variant {
        SwapInstructionVariant::ExactIn | SwapInstructionVariant::SharedExactIn => {
            (Some(reader.u64()?), reader.u64()?)
        }
        SwapInstructionVariant::ExactOut | SwapInstructionVariant::SharedExactOut => {
            let output = reader.u64()?;
            let maximum_input = reader.u64()?;
            (Some(maximum_input), output)
        }
        SwapInstructionVariant::TokenLedgerExactIn
        | SwapInstructionVariant::SharedTokenLedgerExactIn => (None, reader.u64()?),
    };
    let slippage_bps = reader.u16()?;
    let platform_fee_bps = reader.u8()?;
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
        let tag = self.u8()?;
        let payload = match tag {
            0..=7
            | 9..=11
            | 13..=14
            | 19..=20
            | 22
            | 25..=26
            | 30..=32
            | 34..=38
            | 40
            | 46
            | 48..=57
            | 62..=63
            | 65..=70
            | 72..=74
            | 76..=80
            | 83..=84
            | 88 => 0,
            8 | 17..=18 | 21 | 23..=24 | 27..=28 | 39 | 58 | 60..=61 | 64 | 85 | 89 => 1,
            12 | 15..=16 => 1,
            29 => 16,
            33 | 41 => 4,
            42 => 3,
            43 => 10,
            44..=45 => 5,
            47 => {
                if self.u8()? > 1 {
                    return Err(invalid_instruction());
                }
                self.option_remaining_accounts()?;
                0
            }
            71 => 2,
            75 => {
                self.remaining_accounts()?;
                0
            }
            81..=82 => 8,
            86 => 2,
            87 => 9,
            _ => return Err(invalid_instruction()),
        };
        self.take(payload)?;
        self.take(3)?; // percent, input index, output index
        Ok(())
    }
    fn option_remaining_accounts(&mut self) -> Result<()> {
        match self.u8()? {
            0 => Ok(()),
            1 => self.remaining_accounts(),
            _ => Err(invalid_instruction()),
        }
    }
    fn remaining_accounts(&mut self) -> Result<()> {
        let count = self.u32()? as usize;
        if count > 16 {
            return Err(invalid_instruction());
        }
        for _ in 0..count {
            self.take(2)?;
        } // account category enum and length
        Ok(())
    }
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
        "The transaction does not contain a supported Jupiter v6 swap instruction.",
        "Request a fresh Swap API V2 transaction and inspect its exact route and limits.",
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
}
