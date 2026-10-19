//! Minimal fixed-layout Anchor codec for pinned official Kamino liquidity IDLs.
//! Source: Kamino-Finance/kliquidity-sdk@369ed3ffd62338539a9beb4147d411a21489fd35.
//! Resources preserve field order/types and instruction account privilege declarations.

use base64::{Engine, engine::general_purpose::STANDARD};
use gofin_domain::{GoFinError, Result};
use gofin_solana::{NormalizedInstruction, transaction::NormalizedAccountMeta};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::OnceLock};

fn invalid() -> GoFinError {
    GoFinError::validation(
        "Kamino account data does not match the pinned official protocol definition.",
    )
}

pub fn liquidity() -> &'static Value {
    static IDL: OnceLock<Value> = OnceLock::new();
    IDL.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/liquidity-idl.json"))
            .expect("embedded liquidity IDL")
    })
}
pub fn meteora() -> &'static Value {
    static IDL: OnceLock<Value> = OnceLock::new();
    IDL.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/meteora-idl.json"))
            .expect("embedded Meteora IDL")
    })
}
pub fn orca_ticks() -> &'static Value {
    static IDL: OnceLock<Value> = OnceLock::new();
    IDL.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/orca-ticks-idl.json"))
            .expect("embedded Orca ticks IDL")
    })
}
pub fn raydium_ticks() -> &'static Value {
    static IDL: OnceLock<Value> = OnceLock::new();
    IDL.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/raydium-ticks-idl.json"))
            .expect("embedded Raydium ticks IDL")
    })
}

pub fn discriminator(namespace: &str, name: &str) -> [u8; 8] {
    Sha256::digest(format!("{namespace}:{name}").as_bytes())[..8]
        .try_into()
        .expect("eight bytes")
}

pub fn decode(idl: &Value, name: &str, data: &[u8]) -> Result<Value> {
    if data.get(..8) != Some(&discriminator("account", name)) {
        return Err(invalid());
    }
    let definition = idl["accounts"]
        .as_array()
        .and_then(|v| v.iter().find(|x| x["name"] == name))
        .ok_or_else(invalid)?;
    let mut cursor = 8;
    let value = structure(idl, &definition["type"], data, &mut cursor)?;
    // Fixed account layouts must not silently accept truncated or unknown revisions.
    if cursor != data.len() {
        return Err(invalid());
    }
    Ok(value)
}

fn take<'a>(data: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = cursor.checked_add(len).ok_or_else(invalid)?;
    let bytes = data.get(*cursor..end).ok_or_else(invalid)?;
    *cursor = end;
    Ok(bytes)
}
fn structure(idl: &Value, definition: &Value, data: &[u8], cursor: &mut usize) -> Result<Value> {
    let fields = definition["fields"].as_array().ok_or_else(invalid)?;
    let mut object = serde_json::Map::new();
    for field in fields {
        object.insert(
            field["name"].as_str().ok_or_else(invalid)?.to_owned(),
            decode_type(idl, &field["type"], data, cursor)?,
        );
    }
    Ok(Value::Object(object))
}
fn decode_type(idl: &Value, t: &Value, data: &[u8], cursor: &mut usize) -> Result<Value> {
    if let Some(kind) = t.as_str() {
        return match kind {
            "publicKey" | "pubkey" => {
                Ok(json!(bs58::encode(take(data, cursor, 32)?).into_string()))
            }
            "bool" => match take(data, cursor, 1)?[0] {
                0 => Ok(json!(false)),
                1 => Ok(json!(true)),
                _ => Err(invalid()),
            },
            _ => {
                let bits: usize = kind
                    .get(1..)
                    .ok_or_else(invalid)?
                    .parse()
                    .map_err(|_| invalid())?;
                if !matches!(bits, 8 | 16 | 32 | 64 | 128)
                    || !matches!(kind.as_bytes()[0], b'u' | b'i')
                {
                    return Err(invalid());
                }
                let bytes = take(data, cursor, bits / 8)?;
                let mut padded = [0u8; 16];
                padded[..bytes.len()].copy_from_slice(bytes);
                let value = if kind.starts_with('i') {
                    if bytes.last().is_some_and(|b| b & 0x80 != 0) {
                        padded[bytes.len()..].fill(255);
                    }
                    i128::from_le_bytes(padded).to_string()
                } else {
                    u128::from_le_bytes(padded).to_string()
                };
                Ok(json!(value))
            }
        };
    }
    if let Some(a) = t["array"].as_array() {
        let count = a[1].as_u64().ok_or_else(invalid)?;
        if count > 10000 {
            return Err(invalid());
        }
        return (0..count)
            .map(|_| decode_type(idl, &a[0], data, cursor))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    if let Some(name) = t["defined"].as_str() {
        let definition = idl["types"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["name"] == name))
            .ok_or_else(invalid)?;
        return structure(idl, &definition["type"], data, cursor);
    }
    Err(invalid())
}

pub fn field_offset(idl: &Value, account: &str, field: &str) -> Result<usize> {
    let a = idl["accounts"]
        .as_array()
        .and_then(|v| v.iter().find(|x| x["name"] == account))
        .ok_or_else(invalid)?;
    let mut offset = 8;
    for f in a["type"]["fields"].as_array().ok_or_else(invalid)? {
        if f["name"] == field {
            return Ok(offset);
        }
        offset += type_size(idl, &f["type"])?;
    }
    Err(invalid())
}
pub fn account_size(idl: &Value, account: &str) -> Result<usize> {
    let a = idl["accounts"]
        .as_array()
        .and_then(|v| v.iter().find(|x| x["name"] == account))
        .ok_or_else(invalid)?;
    struct_size(idl, &a["type"]).map(|n| n + 8)
}
fn struct_size(idl: &Value, t: &Value) -> Result<usize> {
    t["fields"]
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .try_fold(0, |n, f| Ok(n + type_size(idl, &f["type"])?))
}
fn type_size(idl: &Value, t: &Value) -> Result<usize> {
    if let Some(t) = t.as_str() {
        return match t {
            "publicKey" | "pubkey" => Ok(32),
            "bool" => Ok(1),
            _ => t
                .get(1..)
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| matches!(n, 8 | 16 | 32 | 64 | 128))
                .map(|n| n / 8)
                .ok_or_else(invalid),
        };
    }
    if let Some(a) = t["array"].as_array() {
        return Ok(type_size(idl, &a[0])? * a[1].as_u64().ok_or_else(invalid)? as usize);
    }
    let name = t["defined"].as_str().ok_or_else(invalid)?;
    let d = idl["types"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["name"] == name))
        .ok_or_else(invalid)?;
    struct_size(idl, &d["type"])
}

pub fn instruction(
    program: &str,
    name: &str,
    accounts: &BTreeMap<String, String>,
    args: &[u64],
) -> Result<NormalizedInstruction> {
    let i = liquidity()["instructions"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["name"] == name))
        .ok_or_else(invalid)?;
    if i["args"].as_array().ok_or_else(invalid)?.len() != args.len() {
        return Err(invalid());
    }
    let mut snake = String::new();
    for c in name.chars() {
        if c.is_uppercase() {
            snake.push('_');
            snake.push(c.to_ascii_lowercase())
        } else {
            snake.push(c)
        }
    }
    let mut data = discriminator("global", &snake).to_vec();
    for a in args {
        data.extend_from_slice(&a.to_le_bytes());
    }
    let metas = i["accounts"]
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|a| {
            let name = a["name"].as_str().ok_or_else(invalid)?;
            let address = accounts
                .get(name)
                .cloned()
                .or_else(|| {
                    a["isOptional"]
                        .as_bool()
                        .filter(|v| *v)
                        .map(|_| program.to_owned())
                })
                .ok_or_else(invalid)?;
            gofin_solana::parse_address(&address)?;
            Ok(NormalizedAccountMeta {
                address,
                signer: a["isSigner"].as_bool().unwrap_or(false),
                writable: a["isMut"].as_bool().unwrap_or(false),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(NormalizedInstruction {
        program_id: program.into(),
        accounts: metas,
        data_base64: STANDARD.encode(data),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strategy_layout_and_revision_are_pinned() {
        assert_eq!(
            account_size(liquidity(), "WhirlpoolStrategy").unwrap(),
            4064
        );
        assert_eq!(
            field_offset(liquidity(), "WhirlpoolStrategy", "sharesMint").unwrap(),
            720
        );
        let mut data = vec![0; 4064];
        data[..8].copy_from_slice(&discriminator("account", "WhirlpoolStrategy"));
        assert!(decode(liquidity(), "WhirlpoolStrategy", &data).is_ok());
        assert!(decode(liquidity(), "WhirlpoolStrategy", &data[..4063]).is_err());
        data[0] ^= 1;
        assert!(decode(liquidity(), "WhirlpoolStrategy", &data).is_err());
    }
    #[test]
    fn instruction_preserves_official_order_and_financial_arguments() {
        let mut accounts = BTreeMap::new();
        for a in liquidity()["instructions"][0]["accounts"]
            .as_array()
            .unwrap()
        {
            accounts.insert(
                a["name"].as_str().unwrap().into(),
                "11111111111111111111111111111111".into(),
            );
        }
        let ix = instruction(
            "11111111111111111111111111111111",
            "deposit",
            &accounts,
            &[9, 17],
        )
        .unwrap();
        let bytes = STANDARD.decode(ix.data_base64).unwrap();
        assert_eq!(&bytes[8..16], &9u64.to_le_bytes());
        assert_eq!(&bytes[16..], &17u64.to_le_bytes());
        assert!(ix.accounts[0].signer);
        assert!(ix.accounts[15].writable);
        assert!(!ix.accounts[12].writable);
    }
}
