//! Narrow EVM `Order` v1 hashStruct port from
//! `@relay-protocol/settlement-sdk@0.0.146` (gitHead
//! `8e7d794475836ae77e7f4cf3fc788f02ed079150`). Relay's protocol.v2 envelope
//! still carries this v1 order shape. No EIP-712 domain separator is applied:
//! the upstream `getOrderId` calls `hashStruct` directly.
use super::bridge_error;
use alloy::primitives::{Address, B256, U256, keccak256};
use gofin_domain::{GoFinError, Result};
use serde_json::Value;
use std::str::FromStr;

const ORDER_TYPE: &str = "Order(string version,string solverChainId,address solver,uint256 salt,Input[] inputs,Output output,Fee[] fees)Fee(string recipientChainId,bytes recipient,string currencyChainId,bytes currency,uint256 amount)Input(InputPayment payment,InputRefund[] refunds)InputPayment(string chainId,bytes currency,uint256 amount,uint256 weight)InputRefund(string chainId,bytes recipient,bytes currency,uint256 minimumAmount,uint32 deadline,bytes extraData)Output(string chainId,OutputPayment[] payments,uint32 deadline,bytes[] calls,bytes extraData)OutputPayment(bytes recipient,bytes currency,uint256 minimumAmount,uint256 expectedAmount)";
const INPUT_TYPE: &str = "Input(InputPayment payment,InputRefund[] refunds)InputPayment(string chainId,bytes currency,uint256 amount,uint256 weight)InputRefund(string chainId,bytes recipient,bytes currency,uint256 minimumAmount,uint32 deadline,bytes extraData)";
const PAYMENT_TYPE: &str =
    "InputPayment(string chainId,bytes currency,uint256 amount,uint256 weight)";
const REFUND_TYPE: &str = "InputRefund(string chainId,bytes recipient,bytes currency,uint256 minimumAmount,uint32 deadline,bytes extraData)";
const OUTPUT_TYPE: &str = "Output(string chainId,OutputPayment[] payments,uint32 deadline,bytes[] calls,bytes extraData)OutputPayment(bytes recipient,bytes currency,uint256 minimumAmount,uint256 expectedAmount)";
const OUTPUT_PAYMENT_TYPE: &str =
    "OutputPayment(bytes recipient,bytes currency,uint256 minimumAmount,uint256 expectedAmount)";
const FEE_TYPE: &str = "Fee(string recipientChainId,bytes recipient,string currencyChainId,bytes currency,uint256 amount)";

pub fn hash_order(order: &Value) -> Result<B256> {
    let version = string(order, "version")?;
    let solver_chain = string(order, "solverChainId")?;
    let solver = address_word(string(order, "solver")?)?;
    let salt = uint_word(string(order, "salt")?)?;
    let inputs = array(order, "inputs")?;
    let mut input_hashes = Vec::new();
    for input in inputs {
        input_hashes.push(hash_input(input)?);
    }
    let output = order
        .get("output")
        .ok_or_else(|| invalid("missing output"))?;
    let output_hash = hash_output(output)?;
    let fees = array(order, "fees")?;
    let mut fee_hashes = Vec::new();
    for fee in fees {
        fee_hashes.push(hash_fee(fee)?);
    }
    Ok(hash_words(
        ORDER_TYPE,
        &[
            hash_string(version),
            hash_string(solver_chain),
            solver,
            salt,
            hash_array(&input_hashes),
            output_hash,
            hash_array(&fee_hashes),
        ],
    ))
}
fn hash_input(input: &Value) -> Result<B256> {
    let payment = input
        .get("payment")
        .ok_or_else(|| invalid("missing payment"))?;
    let payment_hash = hash_words(
        PAYMENT_TYPE,
        &[
            hash_string(string(payment, "chainId")?),
            bytes_word(string(payment, "currency")?)?,
            uint_word(string(payment, "amount")?)?,
            uint_word(string(payment, "weight")?)?,
        ],
    );
    let refunds = array(input, "refunds")?;
    let mut hs = Vec::new();
    for refund in refunds {
        hs.push(hash_refund(refund)?)
    }
    Ok(hash_words(INPUT_TYPE, &[payment_hash, hash_array(&hs)]))
}
fn hash_refund(value: &Value) -> Result<B256> {
    Ok(hash_words(
        REFUND_TYPE,
        &[
            hash_string(string(value, "chainId")?),
            bytes_word(string(value, "recipient")?)?,
            bytes_word(string(value, "currency")?)?,
            uint_word(string(value, "minimumAmount")?)?,
            uint_word(&number_or_string(value, "deadline")?)?,
            bytes_word(string(value, "extraData")?)?,
        ],
    ))
}
fn hash_output(value: &Value) -> Result<B256> {
    let payments = array(value, "payments")?;
    let mut hs = Vec::new();
    for p in payments {
        hs.push(hash_words(
            OUTPUT_PAYMENT_TYPE,
            &[
                bytes_word(string(p, "recipient")?)?,
                bytes_word(string(p, "currency")?)?,
                uint_word(string(p, "minimumAmount")?)?,
                uint_word(string(p, "expectedAmount")?)?,
            ],
        ));
    }
    let calls = array(value, "calls")?;
    let mut call_hashes = Vec::new();
    for call in calls {
        call_hashes.push(bytes_word(
            call.as_str()
                .ok_or_else(|| invalid("call must be a hex string"))?,
        )?);
    }
    Ok(hash_words(
        OUTPUT_TYPE,
        &[
            hash_string(string(value, "chainId")?),
            hash_array(&hs),
            uint_word(&number_or_string(value, "deadline")?)?,
            hash_array(&call_hashes),
            bytes_word(string(value, "extraData")?)?,
        ],
    ))
}
fn hash_fee(value: &Value) -> Result<B256> {
    Ok(hash_words(
        FEE_TYPE,
        &[
            hash_string(string(value, "recipientChainId")?),
            bytes_word(string(value, "recipient")?)?,
            hash_string(string(value, "currencyChainId")?),
            bytes_word(string(value, "currency")?)?,
            uint_word(string(value, "amount")?)?,
        ],
    ))
}
fn hash_words(type_string: &str, words: &[B256]) -> B256 {
    let mut data = Vec::with_capacity(32 * (words.len() + 1));
    data.extend_from_slice(keccak256(type_string.as_bytes()).as_slice());
    for w in words {
        data.extend_from_slice(w.as_slice())
    }
    keccak256(data)
}
fn hash_array(words: &[B256]) -> B256 {
    let mut data = Vec::with_capacity(32 * words.len());
    for w in words {
        data.extend_from_slice(w.as_slice())
    }
    keccak256(data)
}
fn hash_string(v: &str) -> B256 {
    keccak256(v.as_bytes())
}
fn bytes_word(v: &str) -> Result<B256> {
    let s = v
        .strip_prefix("0x")
        .ok_or_else(|| invalid("bytes value is not 0x-prefixed"))?;
    if s.len() % 2 != 0 {
        return Err(invalid("bytes value has odd hex length"));
    };
    let b = hex::decode(s).map_err(|_| invalid("bytes value has invalid hex"))?;
    Ok(keccak256(b))
}
fn uint_word(v: &str) -> Result<B256> {
    let n = U256::from_str(v).map_err(|_| invalid("uint value is not a decimal uint256"))?;
    Ok(B256::from(n.to_be_bytes::<32>()))
}
fn address_word(v: &str) -> Result<B256> {
    let a = Address::from_str(v).map_err(|_| invalid("solver is not an EVM address"))?;
    Ok(B256::from(a.into_word().0))
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("required string missing"))
}
fn number_or_string(v: &Value, key: &str) -> Result<String> {
    let val = v
        .get(key)
        .ok_or_else(|| invalid("required integer missing"))?;
    if let Some(s) = val.as_str() {
        return Ok(s.into());
    }
    if let Some(n) = val.as_u64() {
        return Ok(n.to_string());
    }
    Err(invalid("integer has unsupported representation"))
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a [Value]> {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid("required array missing"))
}
fn invalid(msg: &str) -> GoFinError {
    bridge_error(
        "BRIDGE_ORDER_INVALID",
        "quote",
        format!("Relay order hash input is invalid: {msg}."),
        "Request a new quote; GoFin will not execute unverifiable order data.",
    )
}
