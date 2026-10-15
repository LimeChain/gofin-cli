//! Pinned-IDL decoder for the DFlow Swap Orchestrator transaction envelope.
//!
//! The provider's route programs and pool accounts remain dynamic. This module
//! decodes the published Anchor/Borsh arguments for diagnostics and bounds
//! checks. The current public IDL does not define the action-to-source and
//! destination account mapping, so decoded amounts are not write authority
//! and all DFlow swap formats fail closed until that graph is independently
//! pinned and verified.

use gofin_domain::{GoFinError, Result};
use serde_json::{Value, json};

const IDL: &str = include_str!("../resources/dflow-swap-orchestrator-mainnet-idl.json");
pub(crate) const PROGRAM: &str = "DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH";
const MAX_DEPTH: usize = 32;
const MAX_VECTOR_ITEMS: usize = 1024;
const MAX_ACTIONS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedDflowSwap {
    pub instruction: String,
    pub quoted_output: u64,
    pub slippage_bps: u16,
    pub platform_fee_bps: u16,
    pub positive_slippage_fee_limit_pct: u8,
    pub action_count: usize,
    /// Diagnostic sum of declared swap-action inputs. Heterogeneous action
    /// amounts are not authoritative without a proved source-account graph.
    pub swap_input_atoms: Option<u64>,
    /// Explicit TransferFee action amounts. These are part of the total
    /// approved input debit, not an extra amount outside it.
    pub transfer_fee_atoms: u64,
    pub wrap_sol_lamports: Option<u64>,
    pub unsupported_actions: Vec<String>,
    pub output_native: bool,
    pub destination_index: Option<usize>,
}

pub(crate) fn decode_swap(data: &[u8]) -> Result<DecodedDflowSwap> {
    let idl: Value = serde_json::from_str(IDL).map_err(|_| malformed())?;
    if idl.get("address").and_then(Value::as_str) != Some(PROGRAM) {
        return Err(malformed());
    }
    let instruction = idl
        .get("instructions")
        .and_then(Value::as_array)
        .and_then(|instructions| {
            instructions.iter().find(|instruction| {
                instruction
                    .get("discriminator")
                    .and_then(Value::as_array)
                    .is_some_and(|discriminator| {
                        discriminator.len() == 8
                            && data.len() >= 8
                            && discriminator
                                .iter()
                                .zip(data.iter())
                                .all(|(left, right)| left.as_u64() == Some(u64::from(*right)))
                    })
            })
        })
        .ok_or_else(malformed)?;
    let name = instruction
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    if !matches!(
        name,
        "swap"
            | "swap2"
            | "swap_with_destination"
            | "swap2_with_destination"
            | "swap_with_destination_native"
            | "swap2_with_destination_native"
    ) {
        return Err(malformed());
    }
    let args = instruction
        .get("args")
        .and_then(Value::as_array)
        .filter(|args| args.len() == 1)
        .and_then(|args| args.first())
        .and_then(|arg| arg.get("type"))
        .ok_or_else(malformed)?;
    let mut cursor = 8usize;
    let decoded = parse_type(data, &mut cursor, args, &idl, 0)?;
    if cursor != data.len() {
        return Err(malformed());
    }
    let params = decoded.as_object().ok_or_else(malformed)?;
    let actions = params
        .get("actions")
        .and_then(Value::as_array)
        .filter(|actions| !actions.is_empty() && actions.len() <= MAX_ACTIONS)
        .ok_or_else(malformed)?;
    let quoted_output = params
        .get("quoted_out_amount")
        .and_then(Value::as_u64)
        .filter(|amount| *amount > 0)
        .ok_or_else(malformed)?;
    let slippage_bps = params
        .get("slippage_bps")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value <= 10_000)
        .ok_or_else(malformed)?;
    let platform_fee_bps = params
        .get("platform_fee_bps")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value <= 10_000)
        .ok_or_else(malformed)?;
    let positive_slippage_fee_limit_pct = params
        .get("positive_slippage_fee_limit_pct")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(0);
    let mut swap_input_atoms = None;
    let mut transfer_fee_atoms = 0u64;
    let mut wrap_sol_lamports = None;
    let mut unsupported_actions = Vec::new();
    if actions.len() != 1 {
        // The pinned IDL exposes heterogeneous and nested actions but does
        // not specify a common source-account relation for their `amount`
        // fields. Numeric summation across a split route could mix an
        // intermediate output amount with wallet input. Only a single direct
        // swap action is currently reviewed.
        unsupported_actions.push("multi_action_route".into());
    }
    for action in actions {
        let action = action.as_object().ok_or_else(malformed)?;
        if action.len() != 1 {
            return Err(malformed());
        }
        let (name, params) = action.iter().next().ok_or_else(malformed)?;
        // The published IDL names these flags but does not define their
        // semantics. A nonzero value can alter action amount handling, so it
        // cannot inherit the ordinary amount-bound review.
        if params
            .get("orchestrator_flags")
            .is_some_and(|flags| flags.get("flags").and_then(Value::as_u64) != Some(0))
        {
            unsupported_actions.push(name.clone());
        }
        if is_swap_action(name) {
            let amount = params
                .get("amount")
                .and_then(Value::as_u64)
                .filter(|amount| *amount > 0)
                .ok_or_else(malformed)?;
            let total = swap_input_atoms
                .unwrap_or(0u64)
                .checked_add(amount)
                .ok_or_else(malformed)?;
            swap_input_atoms = Some(total);
        } else if is_transfer_fee_action(name) {
            let amount = params
                .get("amount")
                .and_then(Value::as_u64)
                .ok_or_else(malformed)?;
            transfer_fee_atoms = transfer_fee_atoms
                .checked_add(amount)
                .ok_or_else(malformed)?;
        } else if name == "WrapSol" {
            let amount = params
                .get("lamports")
                .and_then(Value::as_u64)
                .filter(|amount| *amount > 0)
                .ok_or_else(malformed)?;
            wrap_sol_lamports = Some(
                wrap_sol_lamports
                    .unwrap_or(0u64)
                    .checked_add(amount)
                    .ok_or_else(malformed)?,
            );
        }
        if name.ends_with("DynamicRouteV1") {
            // This variant carries nested candidate actions and its own
            // amount. The pinned IDL does not specify how that amount is
            // sourced relative to sibling actions; treating it as metadata
            // would leave a second wallet debit outside the approved bound.
            unsupported_actions.push(name.clone());
        }
    }
    unsupported_actions.extend(
        actions
            .iter()
            .filter_map(|action| action.as_object()?.keys().next().cloned())
            .filter(|name| !is_supported_action(name)),
    );
    unsupported_actions.sort();
    unsupported_actions.dedup();
    // The public Anchor IDL lists action arguments but omits the binding from
    // each action to the wallet source account, destination account, and
    // intermediate route account graph. Amount bytes alone cannot prove the
    // approved wallet debit or recipient, even for a single action.
    unsupported_actions.push("unproven_action_account_graph".into());
    Ok(DecodedDflowSwap {
        instruction: name.to_owned(),
        quoted_output,
        slippage_bps,
        platform_fee_bps,
        positive_slippage_fee_limit_pct,
        action_count: actions.len(),
        swap_input_atoms,
        transfer_fee_atoms,
        wrap_sol_lamports,
        unsupported_actions,
        output_native: name.ends_with("_native"),
        destination_index: name.contains("with_destination").then_some(4),
    })
}

fn is_swap_action(name: &str) -> bool {
    name.ends_with("Swap") && !name.ends_with("DynamicRouteV1")
}

fn is_transfer_fee_action(name: &str) -> bool {
    matches!(
        name,
        "TransferFee" | "TransferFeeWithMint" | "TransferFeeV2" | "TransferFeeWithMintV2"
    )
}

fn is_supported_action(name: &str) -> bool {
    is_swap_action(name)
        || matches!(
            name,
            "TransferFee"
                | "TransferFeeWithMint"
                | "TransferFeeV2"
                | "TransferFeeWithMintV2"
                | "TransferHookAccounts"
                | "RecordId"
                | "RecordId2"
                | "IncludeAccount"
                | "SetMinimumLegOutputs"
                | "SetMinimumLegPrices"
                | "SetMaxUnderconsumptionBps"
                | "WrapSol"
                | "UnwrapSol"
                | "InitAtaIdempotent"
        )
}

fn parse_type(
    data: &[u8],
    cursor: &mut usize,
    ty: &Value,
    idl: &Value,
    depth: usize,
) -> Result<Value> {
    if depth >= MAX_DEPTH {
        return Err(malformed());
    }
    if let Some(name) = ty.as_str() {
        return parse_primitive(data, cursor, name);
    }
    if let Some(fields) = ty.get("defined") {
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let def = idl
            .get("types")
            .and_then(Value::as_array)
            .and_then(|types| {
                types
                    .iter()
                    .find(|candidate| candidate.get("name").and_then(Value::as_str) == Some(name))
            })
            .and_then(|definition| definition.get("type"))
            .ok_or_else(malformed)?;
        return parse_type_body(data, cursor, def, idl, depth + 1);
    }
    if let Some(inner) = ty.get("vec") {
        let length = read_u32(data, cursor)? as usize;
        if length > MAX_VECTOR_ITEMS {
            return Err(malformed());
        }
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(parse_type(data, cursor, inner, idl, depth + 1)?);
        }
        return Ok(Value::Array(values));
    }
    if let Some(inner) = ty.get("option") {
        return match read_u8(data, cursor)? {
            0 => Ok(Value::Null),
            1 => parse_type(data, cursor, inner, idl, depth + 1),
            _ => Err(malformed()),
        };
    }
    if let Some(array) = ty
        .get("array")
        .and_then(Value::as_array)
        .filter(|array| array.len() == 2)
    {
        let length = array[1]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value <= MAX_VECTOR_ITEMS)
            .ok_or_else(malformed)?;
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(parse_type(data, cursor, &array[0], idl, depth + 1)?);
        }
        return Ok(Value::Array(values));
    }
    if let Some(tuple) = ty.get("tuple").and_then(Value::as_array) {
        let mut values = Vec::with_capacity(tuple.len());
        for item in tuple {
            values.push(parse_type(data, cursor, item, idl, depth + 1)?);
        }
        return Ok(Value::Array(values));
    }
    Err(malformed())
}

fn parse_type_body(
    data: &[u8],
    cursor: &mut usize,
    body: &Value,
    idl: &Value,
    depth: usize,
) -> Result<Value> {
    match body.get("kind").and_then(Value::as_str) {
        Some("struct") => {
            let fields = body
                .get("fields")
                .and_then(Value::as_array)
                .ok_or_else(malformed)?;
            let mut values = serde_json::Map::new();
            for field in fields {
                let name = field
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(malformed)?;
                let ty = field.get("type").ok_or_else(malformed)?;
                values.insert(
                    name.to_owned(),
                    parse_type(data, cursor, ty, idl, depth + 1)?,
                );
            }
            Ok(Value::Object(values))
        }
        Some("enum") => {
            let index = usize::from(read_u8(data, cursor)?);
            let variant = body
                .get("variants")
                .and_then(Value::as_array)
                .and_then(|variants| variants.get(index))
                .ok_or_else(malformed)?;
            let name = variant
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(malformed)?;
            let mut values = Vec::new();
            if let Some(fields) = variant.get("fields").and_then(Value::as_array) {
                for field in fields {
                    let ty = field.get("type").unwrap_or(field);
                    values.push(parse_type(data, cursor, ty, idl, depth + 1)?);
                }
            }
            Ok(json!({name:values.first().cloned().unwrap_or(Value::Null)}))
        }
        _ => Err(malformed()),
    }
}

fn parse_primitive(data: &[u8], cursor: &mut usize, name: &str) -> Result<Value> {
    match name {
        "bool" => match read_u8(data, cursor)? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            _ => Err(malformed()),
        },
        "u8" => Ok(json!(read_u8(data, cursor)?)),
        "u16" => Ok(json!(u16::from_le_bytes(read_array(data, cursor)?))),
        "u32" => Ok(json!(read_u32(data, cursor)?)),
        "u64" => Ok(json!(u64::from_le_bytes(read_array(data, cursor)?))),
        "i64" => Ok(json!(i64::from_le_bytes(read_array(data, cursor)?))),
        "u128" => Ok(json!(
            u128::from_le_bytes(read_array(data, cursor)?).to_string()
        )),
        "i128" => Ok(json!(
            i128::from_le_bytes(read_array(data, cursor)?).to_string()
        )),
        "pubkey" => {
            let bytes: [u8; 32] = read_array(data, cursor)?;
            Ok(json!(bs58::encode(bytes).into_string()))
        }
        "string" => {
            let length = read_u32(data, cursor)? as usize;
            if length > 4096 {
                return Err(malformed());
            }
            let bytes = take(data, cursor, length)?;
            Ok(json!(std::str::from_utf8(bytes).map_err(|_| malformed())?))
        }
        _ => Err(malformed()),
    }
}

fn read_u8(data: &[u8], cursor: &mut usize) -> Result<u8> {
    Ok(read_array::<1>(data, cursor)?[0])
}
fn read_u32(data: &[u8], cursor: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(data, cursor)?))
}
fn read_array<const N: usize>(data: &[u8], cursor: &mut usize) -> Result<[u8; N]> {
    take(data, cursor, N)?.try_into().map_err(|_| malformed())
}
fn take<'a>(data: &'a [u8], cursor: &mut usize, size: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(size)
        .filter(|end| *end <= data.len())
        .ok_or_else(malformed)?;
    let result = &data[*cursor..end];
    *cursor = end;
    Ok(result)
}

fn malformed() -> GoFinError {
    GoFinError::new(
        "JUPITER_INSTRUCTION_UNREVIEWED",
        "prepare",
        "The DFlow transaction does not match the pinned Swap Orchestrator IDL or approved swap bounds.",
        "Keep the transaction unsubmitted and request a new reviewed Jupiter quote.",
    )
}

pub(crate) fn require_reviewed_format(decoded: &DecodedDflowSwap) -> Result<()> {
    if decoded.unsupported_actions.is_empty() {
        return Ok(());
    }
    let formats = decoded.unsupported_actions.join(", ");
    Err(GoFinError::new(
        "JUPITER_DFLOW_FORMAT_UNREVIEWED",
        "prepare",
        format!(
            "DFlow instruction `{}` was decoded, but its action-to-wallet account graph is not reviewed ({}). No transaction was submitted.",
            decoded.instruction, formats
        ),
        "Choose a reviewed Jupiter route or wait for verified DFlow action-account semantics.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swap2_instruction() -> Vec<u8> {
        let mut data = vec![65, 75, 63, 76, 235, 91, 91, 136]; // swap2
        data.extend_from_slice(&1u32.to_le_bytes()); // one action
        data.push(0); // WhirlpoolsSwap
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.push(1); // a_to_b
        data.push(0); // orchestrator flags
        data.extend_from_slice(&950_000u64.to_le_bytes());
        data.extend_from_slice(&100u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(0); // no positive-slippage fee
        data
    }

    fn add_whirlpool_action(data: &mut Vec<u8>, amount: u64) {
        data.push(0); // WhirlpoolsSwap
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(1); // a_to_b
        data.push(0); // orchestrator flags
    }

    #[test]
    fn pinned_dflow_idl_decodes_but_fails_closed_without_action_account_graph() {
        let decoded = decode_swap(&swap2_instruction()).unwrap();
        assert_eq!(decoded.instruction, "swap2");
        assert_eq!(decoded.action_count, 1);
        assert_eq!(decoded.swap_input_atoms, Some(1_000_000));
        assert_eq!(decoded.transfer_fee_atoms, 0);
        assert_eq!(decoded.quoted_output, 950_000);
        assert_eq!(decoded.slippage_bps, 100);
        assert_eq!(decoded.platform_fee_bps, 0);
        assert!(
            decoded
                .unsupported_actions
                .iter()
                .any(|name| name == "unproven_action_account_graph")
        );
        let error = require_reviewed_format(&decoded).unwrap_err();
        assert_eq!(error.code, "JUPITER_DFLOW_FORMAT_UNREVIEWED");
        assert!(error.message.contains("unproven_action_account_graph"));
        let mut trailing = swap2_instruction();
        trailing.push(0);
        assert!(decode_swap(&trailing).is_err());
        assert!(decode_swap(&swap2_instruction()[..28]).is_err());
    }

    #[test]
    fn decoder_marks_unproven_multiaction_amounts_unreviewed() {
        let mut data = vec![65, 75, 63, 76, 235, 91, 91, 136]; // swap2
        data.extend_from_slice(&4u32.to_le_bytes());
        add_whirlpool_action(&mut data, 600_000);
        add_whirlpool_action(&mut data, 395_000);
        data.push(35); // TransferFee
        data.extend_from_slice(&5_000u64.to_le_bytes());
        data.push(51); // WrapSol
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(&950_000u64.to_le_bytes());
        data.extend_from_slice(&100u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(0);

        let decoded = decode_swap(&data).unwrap();
        assert_eq!(decoded.action_count, 4);
        assert_eq!(decoded.swap_input_atoms, Some(995_000));
        assert_eq!(decoded.transfer_fee_atoms, 5_000);
        assert_eq!(decoded.wrap_sol_lamports, Some(1_000_000));
        assert!(
            decoded
                .unsupported_actions
                .iter()
                .any(|name| name == "multi_action_route")
        );
    }

    #[test]
    fn dynamic_route_is_not_covered_by_sibling_swap_amounts() {
        let mut data = vec![65, 75, 63, 76, 235, 91, 91, 136]; // swap2
        data.extend_from_slice(&2u32.to_le_bytes());
        add_whirlpool_action(&mut data, 1_000_000);
        data.push(31); // DFlowDynamicRouteV1
        data.extend_from_slice(&0u32.to_le_bytes()); // no candidate actions
        data.extend_from_slice(&900_000u64.to_le_bytes()); // independently declared amount
        data.push(0); // flags
        data.extend_from_slice(&950_000u64.to_le_bytes());
        data.extend_from_slice(&100u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(0);

        let decoded = decode_swap(&data).unwrap();
        assert_eq!(decoded.swap_input_atoms, Some(1_000_000));
        assert!(
            decoded
                .unsupported_actions
                .iter()
                .any(|name| name == "DFlowDynamicRouteV1")
        );
    }

    #[test]
    fn undocumented_nonzero_orchestrator_flags_are_not_reviewed() {
        let mut data = swap2_instruction();
        data[22] = 1; // WhirlpoolsSwapOptions.orchestrator_flags.flags
        let decoded = decode_swap(&data).unwrap();
        assert!(
            decoded
                .unsupported_actions
                .iter()
                .any(|name| name == "WhirlpoolsSwap")
        );
    }
}
