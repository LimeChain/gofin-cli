//! Strict Relay Solana direct-deposit and Hub-linked payout proofs.
//!
//! The program ABI and deployment are from relayprotocol/relay-settlement,
//! packages/depository/packages/solana-vm/programs/relay-depository and
//! smart-contracts/contracts/{RelayExecutor,RelayAllocator,SolanaVmPayloadBuilder}.sol.
//! A Relay status hash only locates a transaction; the caller must establish
//! finalized Solana inclusion and canonical Relay Chain receipt inclusion.
use super::proof::RawEvmLog;
use crate::bridge_error;
use alloy::{
    primitives::{Address, B256, keccak256},
    sol_types::SolValue,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gofin_domain::{Quantity, Result};
use gofin_solana::token::{
    MintInfo, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, decode_rpc_account_data, parse_mint,
};
use gofin_solana::{FinalizedReceiptProof, FrozenTransaction, SolanaRuntime, parse_address};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_pubkey3::Pubkey;
use std::str::FromStr;

pub const DEPOSITORY_PROGRAM: &str = "99vQwtBwYtrqqD9YSXbdum3KBdxPAVxYTaQ3cfnJSrN2";
pub const RELAY_CHAIN_ID: u64 = 537713;
pub const RELAY_SOLANA_API_CHAIN_ID: u64 = 792703809;
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

fn invalid(message: &'static str) -> gofin_domain::GoFinError {
    bridge_error(
        "BRIDGE_SOLANA_PROOF_INVALID",
        "reconcile",
        message,
        "Keep the original transfer pending and inspect its immutable quote and finalized receipts.",
    )
}
fn mismatch(message: &'static str) -> gofin_domain::GoFinError {
    bridge_error(
        "BRIDGE_SOLANA_PROOF_MISMATCH",
        "reconcile",
        message,
        "Keep the original transfer pending and inspect its immutable quote and finalized receipts.",
    )
}
fn check(value: bool, message: &'static str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(mismatch(message))
    }
}
fn key_bytes(value: &str) -> Result<[u8; 32]> {
    parse_address(value)?;
    let mut bytes = [0u8; 32];
    bs58::decode(value)
        .onto(&mut bytes)
        .map_err(|_| invalid("Invalid Solana public key."))?;
    Ok(bytes)
}
fn pda(seed: &[u8]) -> Result<String> {
    let program = Pubkey::from_str(DEPOSITORY_PROGRAM)
        .map_err(|_| invalid("Invalid reviewed depository program."))?;
    Pubkey::try_find_program_address(&[seed], &program)
        .map(|(address, _)| address.to_string())
        .ok_or_else(|| invalid("Relay depository PDA cannot be derived."))
}
pub fn relay_vault_address() -> Result<String> {
    pda(b"vault")
}
fn discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    digest[..8].try_into().expect("sha256 has 8 bytes")
}
fn quantity_u64(value: &Quantity) -> Result<u64> {
    value
        .as_str()
        .parse::<u64>()
        .map_err(|_| invalid("Solana amount exceeds u64."))
}
fn order_bytes(value: &str) -> Result<[u8; 32]> {
    let raw = value
        .strip_prefix("0x")
        .ok_or_else(|| invalid("Relay order id is not canonical hex."))?;
    let bytes = hex::decode(raw).map_err(|_| invalid("Relay order id is not canonical hex."))?;
    bytes
        .try_into()
        .map_err(|_| invalid("Relay order id is not 32 bytes."))
}

/// Resolve exact mint identity at finalized commitment. A ticker or Relay API
/// label never selects the mint. Relay's direct depository cannot carry an
/// arbitrary transfer-hook account list, so hook mints fail closed here.
pub async fn resolve_relay_mint(runtime: &SolanaRuntime, mint: &str) -> Result<MintInfo> {
    let mint = parse_address(mint)?;
    let epoch = runtime
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("Solana epoch evidence is missing."))?;
    let account = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([mint,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = account
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(|| invalid("Solana mint account is missing."))?;
    let owner = account
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Solana mint owner is missing."))?;
    check(
        owner == TOKEN_PROGRAM_ID || owner == TOKEN_2022_PROGRAM_ID,
        "Solana mint is not owned by a supported token program.",
    )?;
    let data = decode_rpc_account_data(
        account
            .get("data")
            .ok_or_else(|| invalid("Solana mint data is missing."))?,
    )?;
    let info = parse_mint(owner, &data, epoch)?;
    check(
        info.initialized
            && info.transfer_supported_by_runtime
            && info.transfer_hook_program.is_none(),
        "Solana mint does not have a reviewed Relay transfer path.",
    )?;
    Ok(info)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedSourceTransaction {
    pub wallet: String,
    pub depositor: String,
    pub order_id: String,
    pub mint: Option<String>,
    pub token_program: Option<String>,
    pub decimals: u8,
    pub amount_atoms: Quantity,
    pub minimum_vault_credit_atoms: Quantity,
    pub max_network_fee_lamports: u64,
    pub max_rent_lamports: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidatedSourceTransaction {
    pub message_sha256: String,
    pub order_id: String,
    pub instruction_index: u64,
    pub vault: String,
    pub vault_token_account: Option<String>,
    pub sender_token_account: Option<String>,
}

/// Accepts only the deployed depository's direct deposit instructions plus
/// bounded ComputeBudget settings. Jupiter, forwarder, CPI and unreviewed
/// swaps are deliberately refused before authorization.
pub fn validate_source_transaction(
    frozen: &FrozenTransaction,
    expected: &ExpectedSourceTransaction,
) -> Result<ValidatedSourceTransaction> {
    let wallet = parse_address(&expected.wallet)?;
    let depositor = parse_address(&expected.depositor)?;
    let order_id = order_bytes(&expected.order_id)?;
    let amount = quantity_u64(&expected.amount_atoms)?;
    let minimum = quantity_u64(&expected.minimum_vault_credit_atoms)?;
    check(
        amount > 0 && minimum > 0 && minimum <= amount,
        "Relay Solana deposit amount or floor is invalid.",
    )?;
    check(
        frozen.signer() == wallet
            && frozen.fee_payer() == wallet
            && frozen.required_signers() == [wallet.clone()]
            && frozen.missing_cosigners().is_empty(),
        "Relay Solana deposit requires exactly the selected wallet signer and fee payer.",
    )?;
    check(
        frozen.maximum_network_fee_lamports() <= expected.max_network_fee_lamports,
        "Relay Solana transaction fee cap exceeds approved authority.",
    )?;
    let vault = pda(b"vault")?;
    let depository = pda(b"relay_depository")?;
    let mut deposit = None;
    let mut compute_limit = false;
    let mut compute_price = false;
    for (index, instruction) in frozen.instructions().iter().enumerate() {
        let data = STANDARD
            .decode(&instruction.data_base64)
            .map_err(|_| invalid("Relay Solana instruction data is malformed."))?;
        if instruction.program_id == COMPUTE_BUDGET_PROGRAM {
            let valid = match data.as_slice() {
                [2, _, _, _, _] if !compute_limit => {
                    compute_limit = true;
                    true
                }
                [3, _, _, _, _, _, _, _, _] if !compute_price => {
                    compute_price = true;
                    true
                }
                _ => false,
            };
            check(
                valid && instruction.accounts.is_empty(),
                "Unsupported Solana compute budget instruction.",
            )?;
            continue;
        }
        check(
            instruction.program_id == DEPOSITORY_PROGRAM && deposit.is_none(),
            "Relay Solana source transaction has an unsupported instruction.",
        )?;
        check(
            data.len() == 48,
            "Relay Solana deposit instruction has an invalid length.",
        )?;
        let instruction_name = if expected.mint.is_some() {
            "deposit_token"
        } else {
            "deposit_native"
        };
        check(
            data[..8] == discriminator(instruction_name)
                && data[8..16] == amount.to_le_bytes()
                && data[16..48] == order_id,
            "Relay Solana deposit instruction differs from the saved amount or order.",
        )?;
        let accounts = &instruction.accounts;
        let expected_len = if expected.mint.is_some() { 10 } else { 5 };
        check(
            accounts.len() == expected_len,
            "Relay Solana deposit accounts are incomplete.",
        )?;
        let addresses: Vec<&str> = accounts
            .iter()
            .map(|account| account.address.as_str())
            .collect();
        check(
            addresses[0] == depository
                && addresses[1] == wallet
                && addresses[2] == depositor
                && addresses[3] == vault,
            "Relay Solana deposit account identities differ from the reviewed program.",
        )?;
        check(
            accounts[1].signer
                && accounts[1].writable
                && accounts[2].signer == (depositor == wallet)
                && !accounts[0].writable
                && accounts[3].writable,
            "Relay Solana deposit account permissions differ from the reviewed program.",
        )?;
        let (sender_token_account, vault_token_account) = if let Some(mint) = &expected.mint {
            let mint = parse_address(mint)?;
            let program = expected
                .token_program
                .as_deref()
                .ok_or_else(|| invalid("Token program is missing."))?;
            check(
                program == TOKEN_PROGRAM_ID || program == TOKEN_2022_PROGRAM_ID,
                "Relay token program is unsupported.",
            )?;
            let sender_ata = gofin_solana::associated_token_address(&wallet, &mint, program)?;
            let vault_ata = gofin_solana::associated_token_address(&vault, &mint, program)?;
            check(
                addresses[4] == mint
                    && addresses[5] == sender_ata
                    && addresses[6] == vault_ata
                    && addresses[7] == program
                    && addresses[8] == ASSOCIATED_TOKEN_PROGRAM
                    && addresses[9] == SYSTEM_PROGRAM
                    && accounts[5].writable
                    && accounts[6].writable,
                "Relay token deposit accounts differ from the exact reviewed mint and owners.",
            )?;
            (Some(sender_ata), Some(vault_ata))
        } else {
            check(
                expected.token_program.is_none()
                    && expected.decimals == 9
                    && addresses[4] == SYSTEM_PROGRAM,
                "Relay native deposit identity is invalid.",
            )?;
            (None, None)
        };
        deposit = Some((index as u64, sender_token_account, vault_token_account));
    }
    let (instruction_index, sender_token_account, vault_token_account) =
        deposit.ok_or_else(|| invalid("Relay Solana source transaction has no direct deposit."))?;
    Ok(ValidatedSourceTransaction {
        message_sha256: frozen.message_sha256().into(),
        order_id: expected.order_id.clone(),
        instruction_index,
        vault,
        vault_token_account,
        sender_token_account,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedSourcePayment {
    pub signature: String,
    pub slot: u64,
    pub message_sha256: String,
    pub vault_credit_atoms: Quantity,
    pub fee_lamports: u64,
    pub wallet_rent_lamports: u64,
    pub order_id: String,
}

pub fn verify_source_receipt(
    proof: &FinalizedReceiptProof,
    validated: &ValidatedSourceTransaction,
    expected: &ExpectedSourceTransaction,
) -> Result<VerifiedSourcePayment> {
    check(
        !proof.failed && proof.message_sha256 == validated.message_sha256,
        "Finalized Solana source receipt differs from the saved message or failed.",
    )?;
    check(
        proof.fee_lamports <= expected.max_network_fee_lamports,
        "Finalized Solana source fee exceeds the approved cap.",
    )?;
    let amount = quantity_u64(&expected.amount_atoms)?;
    let minimum = quantity_u64(&expected.minimum_vault_credit_atoms)?;
    let (credit, rent) = if let Some(mint) = &expected.mint {
        let program = expected
            .token_program
            .as_deref()
            .ok_or_else(|| invalid("Token program is missing."))?;
        let sender_ata = validated
            .sender_token_account
            .as_deref()
            .ok_or_else(|| invalid("Sender token account is missing."))?;
        let vault_ata = validated
            .vault_token_account
            .as_deref()
            .ok_or_else(|| invalid("Vault token account is missing."))?;
        let debit = proof.token_account_delta_verified(
            sender_ata,
            &expected.wallet,
            mint,
            program,
            expected.decimals,
        )?;
        check(
            debit.as_str() == format!("-{amount}"),
            "Finalized token source debit differs from the approved amount.",
        )?;
        let credit = proof.token_account_delta_verified(
            vault_ata,
            &validated.vault,
            mint,
            program,
            expected.decimals,
        )?;
        check(
            !credit.is_negative()
                && credit.magnitude().integer() >= Quantity::from_u64(minimum).integer(),
            "Finalized token vault credit is below the approved floor.",
        )?;
        let wallet_debit = proof.lamport_delta(&expected.wallet)?;
        check(
            wallet_debit.is_negative(),
            "Solana source wallet did not pay its transaction fee.",
        )?;
        let total = quantity_u64(&wallet_debit.magnitude())?;
        let rent = total
            .checked_sub(proof.fee_lamports)
            .ok_or_else(|| mismatch("Solana source wallet fee accounting is inconsistent."))?;
        (credit.magnitude(), rent)
    } else {
        let credit = proof.lamport_delta(&validated.vault)?;
        check(
            credit.as_str() == amount.to_string(),
            "Finalized SOL vault credit differs from the approved amount.",
        )?;
        let wallet_debit = proof.lamport_delta(&expected.wallet)?;
        check(
            wallet_debit.is_negative(),
            "Finalized SOL source wallet debit is missing.",
        )?;
        let total = quantity_u64(&wallet_debit.magnitude())?;
        let rent = total
            .checked_sub(amount)
            .and_then(|v| v.checked_sub(proof.fee_lamports))
            .ok_or_else(|| mismatch("SOL source wallet fee accounting is inconsistent."))?;
        (Quantity::from_u64(amount), rent)
    };
    check(
        rent <= expected.max_rent_lamports,
        "Finalized Solana rent or account funding exceeds the approved cap.",
    )?;
    Ok(VerifiedSourcePayment {
        signature: proof.signature.clone(),
        slot: proof.slot,
        message_sha256: proof.message_sha256.clone(),
        vault_credit_atoms: credit,
        fee_lamports: proof.fee_lamports,
        wallet_rent_lamports: rent,
        order_id: expected.order_id.clone(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HubPayoutExpectation {
    pub order_id: String,
    pub source_protocol_chain_id: String,
    pub executor: String,
    pub allocator: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedHubPayoutWitness {
    pub order_id: String,
    pub order_address: String,
    pub withdraw_request_hash: String,
    pub payload_hex: String,
    pub payload_sha256: String,
}

/// Relay SDK v2.2 order account: last 20 bytes of
/// keccak256(abi.encode(source protocol chain id, deposit/order id)).
pub fn derive_order_address(source_protocol_chain_id: &str, order_id: &str) -> Result<String> {
    check(
        !source_protocol_chain_id.is_empty() && source_protocol_chain_id.len() <= 128,
        "Relay source protocol chain id is invalid.",
    )?;
    let order = B256::from(order_bytes(order_id)?);
    let encoded = (source_protocol_chain_id.to_owned(), order).abi_encode_params();
    let hash = keccak256(encoded);
    Ok(format!("{:#x}", Address::from_slice(&hash[12..])))
}

/// A successful Relay Hub transfer out of the unique order account. The
/// destination fill still needs its own finalized, order-tagged proof.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedHubOrderSettlement {
    pub order_id: String,
    pub order_address: String,
    pub solver_hub_address: String,
    pub amount_atoms: String,
}

pub fn verify_hub_order_settlement(
    log: &RawEvmLog,
    hub: &str,
    oracle: &str,
    source_protocol_chain_id: &str,
    order_id: &str,
) -> Result<VerifiedHubOrderSettlement> {
    let expected_hub =
        Address::from_str(hub).map_err(|_| invalid("Relay Hub address is invalid."))?;
    let expected_oracle =
        Address::from_str(oracle).map_err(|_| invalid("Relay Oracle address is invalid."))?;
    let order_address = derive_order_address(source_protocol_chain_id, order_id)?;
    let order = Address::from_str(&order_address)
        .map_err(|_| invalid("Relay order account is invalid."))?;
    check(
        Address::from_str(&log.address)
            .map_err(|_| invalid("Relay Hub log address is invalid."))?
            == expected_hub
            && log.topics.len() == 4
            && topic(&log.topics[0])?
                == keccak256("Transfer(address,address,address,uint256,uint256)")
            && topic(&log.topics[1])?[12..] == order.as_slice()[..],
        "Relay Hub transfer does not debit the saved order account.",
    )?;
    let to_topic = topic(&log.topics[2])?;
    check(
        to_topic[..12].iter().all(|byte| *byte == 0),
        "Relay Hub solver address is malformed.",
    )?;
    let solver = Address::from_slice(&to_topic[12..]);
    let data = hex_data(&log.data)?;
    check(
        solver != Address::ZERO
            && data.len() == 64
            && data[..12].iter().all(|byte| *byte == 0)
            && Address::from_slice(&data[12..32]) == expected_oracle
            && data[32..64].iter().any(|byte| *byte != 0),
        "Relay Hub order settlement has an invalid solver, oracle or amount.",
    )?;
    Ok(VerifiedHubOrderSettlement {
        order_id: order_id.into(),
        order_address,
        solver_hub_address: format!("{solver:#x}"),
        amount_atoms: alloy::primitives::U256::from_be_slice(&data[32..64]).to_string(),
    })
}

/// Relay's direct Solana solver fill carries the exact order and request IDs
/// as two memos. Require its reviewed signer, one exact financial transfer,
/// and the finalized recipient's net balance increase.
pub fn verify_solver_payout_receipt(
    proof: &FinalizedReceiptProof,
    hub: &VerifiedHubOrderSettlement,
    expected: &ExpectedPayout,
    request_id: &str,
    solver_signer: &str,
) -> Result<VerifiedPayout> {
    check(
        !proof.failed && proof.error.is_none() && hub.order_id == expected.order_id,
        "Relay solver payout failed or belongs to another order.",
    )?;
    let recipient = parse_address(&expected.recipient)?;
    let signer = parse_address(solver_signer)?;
    check(
        proof.required_signatures.len() == 1 && proof.account_keys.first() == Some(&signer),
        "Relay Solana payout was not signed by the reviewed solver.",
    )?;
    let mut order_memo = 0;
    let mut request_memo = 0;
    let mut transfer = None;
    let mut account_creation = None;
    for (index, ix) in proof.message_instructions.iter().enumerate() {
        let data = STANDARD
            .decode(&ix.data_base64)
            .map_err(|_| invalid("Relay payout instruction is malformed."))?;
        match ix.program_id.as_str() {
            COMPUTE_BUDGET_PROGRAM => {
                check(
                    ix.accounts.is_empty() && matches!(data.first(), Some(2 | 3)),
                    "Relay payout compute instruction is unsupported.",
                )?;
            }
            MEMO_PROGRAM => {
                check(
                    ix.accounts.is_empty(),
                    "Relay payout memo has unexpected accounts.",
                )?;
                if data == expected.order_id.as_bytes() {
                    order_memo += 1;
                } else if data == request_id.as_bytes() {
                    request_memo += 1;
                } else {
                    return Err(mismatch("Relay payout contains an unrelated memo."));
                }
            }
            SYSTEM_PROGRAM if expected.mint.is_none() => {
                check(
                    transfer.is_none()
                        && ix.accounts.len() == 2
                        && ix.accounts[0].address == signer
                        && ix.accounts[1].address == recipient
                        && data.len() == 12
                        && data[..4] == 2u32.to_le_bytes(),
                    "Relay SOL payout transfer differs from the approved recipient.",
                )?;
                let amount = u64::from_le_bytes(data[4..12].try_into().expect("8 bytes"));
                check(amount > 0, "Relay SOL payout amount is zero.")?;
                transfer = Some((index as u64, amount));
            }
            ASSOCIATED_TOKEN_PROGRAM if expected.mint.is_some() => {
                let mint = expected.mint.as_deref().expect("guarded above");
                let program = expected
                    .token_program
                    .as_deref()
                    .ok_or_else(|| invalid("Token program is missing."))?;
                let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
                check(
                    account_creation.is_none()
                        && (data.is_empty() || data == [1])
                        && (ix.accounts.len() == 6 || ix.accounts.len() == 7)
                        && ix.accounts[0].address == signer
                        && ix.accounts[1].address == ata
                        && ix.accounts[2].address == recipient
                        && ix.accounts[3].address == mint
                        && ix.accounts[4].address == SYSTEM_PROGRAM
                        && ix.accounts[5].address == program,
                    "Relay payout creates an unrelated token account.",
                )?;
                account_creation = Some(index as u64);
            }
            TOKEN_PROGRAM_ID | TOKEN_2022_PROGRAM_ID if expected.mint.is_some() => {
                let mint = expected.mint.as_deref().expect("guarded above");
                let program = expected
                    .token_program
                    .as_deref()
                    .ok_or_else(|| invalid("Token program is missing."))?;
                let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
                check(
                    transfer.is_none()
                        && ix.program_id == program
                        && ix.accounts.len() >= 4
                        && ix.accounts[1].address == mint
                        && ix.accounts[2].address == ata
                        && ix.accounts[3].address == signer
                        && data.len() == 10
                        && data[0] == 12
                        && data[9] == expected.decimals,
                    "Relay token payout differs from the approved mint or recipient.",
                )?;
                let amount = u64::from_le_bytes(data[1..9].try_into().expect("8 bytes"));
                check(amount > 0, "Relay token payout amount is zero.")?;
                transfer = Some((index as u64, amount));
            }
            _ => {
                return Err(mismatch(
                    "Relay solver payout contains an unsupported instruction.",
                ));
            }
        }
    }
    check(
        order_memo == 1 && request_memo == 1,
        "Relay solver payout lacks exact order and request memos.",
    )?;
    for group in &proof.inner_instructions {
        let index = group.get("index").and_then(Value::as_u64);
        let inner = group.get("instructions").and_then(Value::as_array);
        let allowed_accounts = account_creation
            .and_then(|creation| proof.message_instructions.get(creation as usize))
            .map(|ix| {
                ix.accounts
                    .iter()
                    .map(|account| account.address.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        check(
            index == account_creation
                && inner.is_some_and(|items| !items.is_empty() && items.len() <= 8)
                && inner.expect("checked above").iter().all(|ix| {
                    let program = ix
                        .get("programIdIndex")
                        .and_then(Value::as_u64)
                        .and_then(|index| proof.account_keys.get(index as usize));
                    let accounts = ix.get("accounts").and_then(Value::as_array);
                    let expected_program = expected.token_program.as_deref();
                    (program.map(String::as_str) == Some(SYSTEM_PROGRAM)
                        || program.map(String::as_str) == expected_program)
                        && accounts.is_some_and(|accounts| {
                            accounts.iter().all(|account| {
                                account
                                    .as_u64()
                                    .and_then(|index| proof.account_keys.get(index as usize))
                                    .is_some_and(|address| {
                                        allowed_accounts.contains(&address.as_str())
                                    })
                            })
                        })
                }),
            "Relay solver payout contains an unreviewed inner instruction.",
        )?;
    }
    let (instruction_index, amount) =
        transfer.ok_or_else(|| mismatch("Relay solver payout lacks a transfer."))?;
    let credit = if let Some(mint) = &expected.mint {
        let program = expected
            .token_program
            .as_deref()
            .ok_or_else(|| invalid("Token program is missing."))?;
        let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
        proof.token_account_delta_verified(&ata, &recipient, mint, program, expected.decimals)?
    } else {
        check(
            expected.decimals == 9 && expected.token_program.is_none(),
            "Relay SOL asset identity is invalid.",
        )?;
        proof.lamport_delta(&recipient)?
    };
    let minimum = quantity_u64(&expected.minimum_amount_atoms)?;
    check(
        minimum > 0
            && !credit.is_negative()
            && credit.magnitude().integer() >= Quantity::from_u64(minimum).integer()
            && credit.magnitude().integer() <= Quantity::from_u64(amount).integer(),
        "Finalized Relay Solana credit is outside the approved payout bounds.",
    )?;
    Ok(VerifiedPayout {
        signature: proof.signature.clone(),
        slot: proof.slot,
        instruction_index,
        order_id: expected.order_id.clone(),
        recipient,
        mint: expected.mint.clone(),
        net_amount_atoms: credit.magnitude(),
        proof_method: "relay_hub_order_and_solver_memos_finalized_credit".into(),
    })
}

/// Verify two logs from the *same canonical successful Relay Chain receipt*.
/// The caller must pin both log positions and the Executor/Allocator deployments
/// independently. This does not accept provider status as settlement evidence.
pub fn verify_hub_payout_witness(
    executed: &RawEvmLog,
    payload_built: &RawEvmLog,
    expected: &HubPayoutExpectation,
) -> Result<VerifiedHubPayoutWitness> {
    let executor = Address::from_str(&expected.executor)
        .map_err(|_| invalid("Relay Executor address is invalid."))?;
    let allocator = Address::from_str(&expected.allocator)
        .map_err(|_| invalid("Relay Allocator address is invalid."))?;
    check(
        Address::from_str(&executed.address)
            .map_err(|_| invalid("Executor log address is invalid."))?
            == executor
            && Address::from_str(&payload_built.address)
                .map_err(|_| invalid("Allocator log address is invalid."))?
                == allocator,
        "Relay Hub payout logs were emitted by another deployment.",
    )?;
    let order_address = Address::from_str(&derive_order_address(
        &expected.source_protocol_chain_id,
        &expected.order_id,
    )?)
    .map_err(|_| invalid("Derived Relay order address is invalid."))?;
    check(
        executed.topics.len() == 4 && payload_built.topics.len() == 2,
        "Relay Hub payout events have unexpected indexed fields.",
    )?;
    check(
        topic(&executed.topics[0])?
            == keccak256("Executed(address,uint256,uint256,uint256,uint256,bytes32)")
            && topic(&payload_built.topics[0])?
                == keccak256("PayloadBuilt(bytes32,bytes,bytes32[])")
            && topic(&executed.topics[1])?[12..] == order_address.as_slice()[..],
        "Relay Hub events do not bind the saved protocol order.",
    )?;
    let executed_data = hex_data(&executed.data)?;
    check(
        executed_data.len() == 96,
        "Relay Executor event data length is invalid.",
    )?;
    let withdraw_hash = B256::from_slice(&executed_data[64..96]);
    check(
        topic(&payload_built.topics[1])? == withdraw_hash,
        "Relay Executor and Allocator events refer to different withdrawals.",
    )?;
    let data = hex_data(&payload_built.data)?;
    check(
        data.len() >= 192 && data.len() % 32 == 0,
        "Relay Allocator payload event data is malformed.",
    )?;
    let payload_offset = word_usize(&data[0..32])?;
    let hashes_offset = word_usize(&data[32..64])?;
    check(
        payload_offset == 64 && hashes_offset >= 96 && hashes_offset <= data.len() - 64,
        "Relay Allocator payload offsets are invalid.",
    )?;
    let payload_len = word_usize(&data[64..96])?;
    check(
        (100..=200).contains(&payload_len),
        "Relay Solana payout payload has an unsupported length.",
    )?;
    let padded = payload_len.div_ceil(32) * 32;
    check(
        hashes_offset == 96 + padded && data.len() == hashes_offset + 64,
        "Relay Allocator payload has unexpected trailing data.",
    )?;
    let payload = &data[96..96 + payload_len];
    check(
        data[96 + payload_len..hashes_offset]
            .iter()
            .all(|b| *b == 0)
            && word_usize(&data[hashes_offset..hashes_offset + 32])? == 1,
        "Relay Allocator payload padding or hash count is invalid.",
    )?;
    let digest = Sha256::digest(payload);
    check(
        data[hashes_offset + 32..hashes_offset + 64] == digest[..],
        "Relay Allocator signed hash does not match its payload.",
    )?;
    Ok(VerifiedHubPayoutWitness {
        order_id: expected.order_id.clone(),
        order_address: format!("{order_address:#x}"),
        withdraw_request_hash: format!("{withdraw_hash:#x}"),
        payload_hex: format!("0x{}", hex::encode(payload)),
        payload_sha256: hex::encode(digest),
    })
}

fn topic(value: &str) -> Result<B256> {
    let bytes = hex_data(value)?;
    check(bytes.len() == 32, "Relay Hub event topic is not 32 bytes.")?;
    Ok(B256::from_slice(&bytes))
}
fn hex_data(value: &str) -> Result<Vec<u8>> {
    let raw = value
        .strip_prefix("0x")
        .ok_or_else(|| invalid("Relay Hub data is not canonical hex."))?;
    hex::decode(raw).map_err(|_| invalid("Relay Hub data is not canonical hex."))
}
fn word_usize(value: &[u8]) -> Result<usize> {
    check(
        value.len() == 32 && value[..24].iter().all(|b| *b == 0),
        "Relay Hub ABI offset is out of range.",
    )?;
    Ok(u64::from_be_bytes(value[24..32].try_into().expect("8 bytes")) as usize)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedPayout {
    pub recipient: String,
    pub mint: Option<String>,
    pub token_program: Option<String>,
    pub decimals: u8,
    pub minimum_amount_atoms: Quantity,
    pub order_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedPayout {
    pub signature: String,
    pub slot: u64,
    pub instruction_index: u64,
    pub order_id: String,
    pub recipient: String,
    pub mint: Option<String>,
    pub net_amount_atoms: Quantity,
    pub proof_method: String,
}

/// The Borsh request carried by execute_transfer must be byte-for-byte the
/// canonical Hub payload and its exact recipient credit must meet the floor.
/// This works for destination fills and separately authorized refund requests.
pub fn verify_payout_receipt(
    proof: &FinalizedReceiptProof,
    witness: &VerifiedHubPayoutWitness,
    expected: &ExpectedPayout,
) -> Result<VerifiedPayout> {
    check(
        !proof.failed && witness.order_id == expected.order_id,
        "Relay Solana payout failed or belongs to another order.",
    )?;
    let recipient = parse_address(&expected.recipient)?;
    let minimum = quantity_u64(&expected.minimum_amount_atoms)?;
    check(minimum > 0, "Relay Solana payout floor must be positive.")?;
    let payload = hex_data(&witness.payload_hex)?;
    check(
        hex::encode(Sha256::digest(&payload)) == witness.payload_sha256,
        "Relay Hub payout payload hash is inconsistent.",
    )?;
    let mut cursor = 0;
    let take = |cursor: &mut usize, count: usize| -> Result<&[u8]> {
        let end = cursor
            .checked_add(count)
            .ok_or_else(|| invalid("Relay payout payload overflows."))?;
        let bytes = payload
            .get(*cursor..end)
            .ok_or_else(|| invalid("Relay payout payload is truncated."))?;
        *cursor = end;
        Ok(bytes)
    };
    take(&mut cursor, 32)?; // domain, independently bound by Hub payload
    check(
        take(&mut cursor, 32)? == key_bytes(&recipient)?,
        "Relay payout payload recipient differs from the approved address.",
    )?;
    let option = take(&mut cursor, 1)?[0];
    let payload_mint = match option {
        0 => None,
        1 => Some(take(&mut cursor, 32)?),
        _ => return Err(invalid("Relay payout token option is invalid.")),
    };
    check(
        payload_mint
            == expected
                .mint
                .as_deref()
                .map(key_bytes)
                .transpose()?
                .as_ref()
                .map(|bytes| bytes.as_slice()),
        "Relay payout payload mint differs from the approved asset.",
    )?;
    let amount = u64::from_le_bytes(take(&mut cursor, 8)?.try_into().expect("8 bytes"));
    take(&mut cursor, 8)?; // request nonce
    take(&mut cursor, 8)?; // expiration, enforced by program
    let vault = bs58::encode(take(&mut cursor, 32)?).into_string();
    check(
        cursor == payload.len() && vault == pda(b"vault")? && amount >= minimum,
        "Relay payout payload has an invalid vault or amount.",
    )?;
    let mut matched_index = None;
    let mut account_creation = 0;
    for (index, instruction) in proof.message_instructions.iter().enumerate() {
        if instruction.program_id == COMPUTE_BUDGET_PROGRAM
            || instruction.program_id == "Ed25519SigVerify111111111111111111111111111"
        {
            continue;
        }
        if instruction.program_id == ASSOCIATED_TOKEN_PROGRAM {
            let mint = expected.mint.as_deref().ok_or_else(|| {
                mismatch("SOL payout includes an unexpected token account creation.")
            })?;
            let program = expected
                .token_program
                .as_deref()
                .ok_or_else(|| invalid("Token program is missing."))?;
            let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
            let data = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid("Relay associated-account instruction is malformed."))?;
            check(
                account_creation == 0
                    && (data.is_empty() || data == [1])
                    && (instruction.accounts.len() == 6 || instruction.accounts.len() == 7)
                    && instruction.accounts[1].address == ata
                    && instruction.accounts[2].address == recipient
                    && instruction.accounts[3].address == mint
                    && instruction.accounts[4].address == SYSTEM_PROGRAM
                    && instruction.accounts[5].address == program,
                "Relay payout creates an account outside the approved recipient and mint.",
            )?;
            account_creation += 1;
            continue;
        }
        check(
            instruction.program_id == DEPOSITORY_PROGRAM,
            "Relay payout transaction has an unsupported financial instruction.",
        )?;
        let data = STANDARD
            .decode(&instruction.data_base64)
            .map_err(|_| invalid("Relay payout instruction is malformed."))?;
        check(
            data.len() == payload.len() + 8
                && data[..8] == discriminator("execute_transfer")
                && data[8..] == payload,
            "Relay Solana payout instruction differs from the canonical Hub payload.",
        )?;
        check(
            index > 0
                && proof.message_instructions[index - 1].program_id
                    == "Ed25519SigVerify111111111111111111111111111",
            "Relay Solana payout is missing adjacent allocator signature verification.",
        )?;
        check(
            instruction.accounts.len() == 12
                && instruction.accounts[2].address == recipient
                && instruction.accounts[3].address == vault,
            "Relay Solana payout accounts differ from the approved recipient or vault.",
        )?;
        if let Some(mint) = &expected.mint {
            let program = expected
                .token_program
                .as_deref()
                .ok_or_else(|| invalid("Token program is missing."))?;
            check(
                program == TOKEN_PROGRAM_ID || program == TOKEN_2022_PROGRAM_ID,
                "Relay payout token program is unsupported.",
            )?;
            let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
            check(
                instruction.accounts[4].address == *mint
                    && instruction.accounts[5].address == ata
                    && instruction.accounts[9].address == program,
                "Relay payout token accounts differ from the approved mint and owner.",
            )?;
        }
        check(
            matched_index.is_none(),
            "Relay Solana payout contains duplicate transfers.",
        )?;
        matched_index = Some(index as u64);
    }
    let instruction_index = matched_index.ok_or_else(|| {
        mismatch("Relay Solana payout receipt lacks one exact depository transfer.")
    })?;
    let credit = if let Some(mint) = &expected.mint {
        let program = expected
            .token_program
            .as_deref()
            .ok_or_else(|| invalid("Token program is missing."))?;
        let ata = gofin_solana::associated_token_address(&recipient, mint, program)?;
        proof.token_account_delta_verified(&ata, &recipient, mint, program, expected.decimals)?
    } else {
        check(
            expected.token_program.is_none() && expected.decimals == 9,
            "Relay SOL payout asset identity is invalid.",
        )?;
        proof.lamport_delta(&recipient)?
    };
    check(
        !credit.is_negative()
            && credit.magnitude().integer() >= Quantity::from_u64(minimum).integer()
            && credit.magnitude().integer() <= Quantity::from_u64(amount).integer(),
        "Finalized Relay Solana recipient credit is below the approved floor.",
    )?;
    Ok(VerifiedPayout {
        signature: proof.signature.clone(),
        slot: proof.slot,
        instruction_index,
        order_id: expected.order_id.clone(),
        recipient,
        mint: expected.mint.clone(),
        net_amount_atoms: credit.magnitude(),
        proof_method: "relay_hub_payload_and_solana_depository_credit".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_solana::transaction::{NormalizedAccountMeta, NormalizedInstruction};

    const ORDER: &str = "0x9400f1b21cb527d7fa3d3eabba93557a18ebe7a2ca4e471cfe5e4c5b4ca7f767";
    const RECIPIENT: &str = "GTtzwxqy67xx9DVESJjx28TgXqpc8xTqtiytgNMaQBTE";
    const EXECUTOR: &str = "0x6747c7e64B90DCcc0D54F865cBd9513156adBC0c";
    const ALLOCATOR: &str = "0x613D3c588F6B8f89302b463F8F19f7241B2857E2";

    fn payload() -> Vec<u8> {
        let mut bytes = vec![7; 32];
        bytes.extend(key_bytes(RECIPIENT).unwrap());
        bytes.push(0);
        bytes.extend(5_000_000u64.to_le_bytes());
        bytes.extend(42u64.to_le_bytes());
        bytes.extend(1_800_000_000i64.to_le_bytes());
        bytes.extend(key_bytes(&pda(b"vault").unwrap()).unwrap());
        bytes
    }
    fn logs_for(payload: Vec<u8>) -> (RawEvmLog, RawEvmLog) {
        let order = derive_order_address("ethereum", ORDER).unwrap();
        let order_address = Address::from_str(&order).unwrap();
        let withdrawal = B256::repeat_byte(9);
        let mut executed_data = vec![0; 96];
        executed_data[31] = 1; // amount in
        executed_data[63] = 2; // amount out
        executed_data[64..96].copy_from_slice(withdrawal.as_slice());
        let executed = RawEvmLog {
            address: EXECUTOR.into(),
            topics: vec![
                format!(
                    "{:#x}",
                    keccak256("Executed(address,uint256,uint256,uint256,uint256,bytes32)")
                ),
                format!("0x{:0>64}", hex::encode(order_address.as_slice())),
                format!("0x{:064x}", 1),
                format!("0x{:064x}", 2),
            ],
            data: format!("0x{}", hex::encode(executed_data)),
        };
        let padded = payload.len().div_ceil(32) * 32;
        let mut data = vec![0; 96 + padded + 64];
        data[31] = 64;
        data[56..64].copy_from_slice(&((96 + padded) as u64).to_be_bytes());
        data[95] = payload.len() as u8;
        data[96..96 + payload.len()].copy_from_slice(&payload);
        data[96 + padded + 31] = 1;
        data[96 + padded + 32..96 + padded + 64].copy_from_slice(&Sha256::digest(&payload));
        let built = RawEvmLog {
            address: ALLOCATOR.into(),
            topics: vec![
                format!("{:#x}", keccak256("PayloadBuilt(bytes32,bytes,bytes32[])")),
                format!("{withdrawal:#x}"),
            ],
            data: format!("0x{}", hex::encode(data)),
        };
        (executed, built)
    }
    fn logs() -> (RawEvmLog, RawEvmLog) {
        logs_for(payload())
    }
    fn expectation() -> HubPayoutExpectation {
        HubPayoutExpectation {
            order_id: ORDER.into(),
            source_protocol_chain_id: "ethereum".into(),
            executor: EXECUTOR.into(),
            allocator: ALLOCATOR.into(),
        }
    }

    #[test]
    fn sdk_order_address_vector() {
        assert_eq!(
            derive_order_address("ethereum", ORDER).unwrap(),
            "0xf85a144b09258a2059b514360936b633e941b685"
        );
    }

    #[test]
    fn solver_fill_binds_hub_order_memos_signer_and_net_credit() {
        const HUB: &str = "0xddd361727c22a01eb137880678a20b0beae69318";
        const ORACLE: &str = "0xd180dc3b8cb71b185c563b6e4857592ca93dacaf";
        const SOLVER: &str = "F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe";
        const REQUEST: &str = "0x1791998013d17c56b97817818856085eaf3b3e4264615b9235fce932cd82fae6";
        let order_address = derive_order_address("ethereum", ORDER).unwrap();
        let topic_address = |address: &str| format!("0x{:0>64}", &address[2..]);
        let mut data = vec![0u8; 64];
        data[12..32].copy_from_slice(Address::from_str(ORACLE).unwrap().as_slice());
        data[63] = 7;
        let hub_log = RawEvmLog {
            address: HUB.into(),
            topics: vec![
                format!(
                    "{:#x}",
                    keccak256("Transfer(address,address,address,uint256,uint256)")
                ),
                topic_address(&order_address),
                topic_address("0xd5fa4f5834722d85d95f45387ed60ce09ba4e36f"),
                format!("0x{:064x}", 123),
            ],
            data: format!("0x{}", hex::encode(&data)),
        };
        let witness =
            verify_hub_order_settlement(&hub_log, HUB, ORACLE, "ethereum", ORDER).unwrap();
        assert_eq!(witness.amount_atoms, "7");
        assert!(verify_hub_order_settlement(&hub_log, HUB, ORACLE, "solana", ORDER).is_err());
        let meta = |address: &str| NormalizedAccountMeta {
            address: address.into(),
            signer: false,
            writable: true,
        };
        let mut transfer_data = 2u32.to_le_bytes().to_vec();
        transfer_data.extend(5_000_000u64.to_le_bytes());
        let ix = |program_id: &str, accounts: Vec<NormalizedAccountMeta>, bytes: Vec<u8>| {
            NormalizedInstruction {
                program_id: program_id.into(),
                accounts,
                data_base64: STANDARD.encode(bytes),
            }
        };
        let proof = FinalizedReceiptProof {
            signature: "synthetic-solver-signature".into(),
            slot: 10,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "synthetic".into(),
            account_keys: vec![SOLVER.into(), RECIPIENT.into()],
            message_instructions: vec![
                ix(
                    SYSTEM_PROGRAM,
                    vec![meta(SOLVER), meta(RECIPIENT)],
                    transfer_data,
                ),
                ix(MEMO_PROGRAM, vec![], ORDER.as_bytes().to_vec()),
                ix(MEMO_PROGRAM, vec![], REQUEST.as_bytes().to_vec()),
            ],
            required_signatures: vec!["synthetic".into()],
            pre_balances_lamports: vec![6_000_000, 100],
            post_balances_lamports: vec![995_000, 5_000_100],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let expected = ExpectedPayout {
            recipient: RECIPIENT.into(),
            mint: None,
            token_program: None,
            decimals: 9,
            minimum_amount_atoms: Quantity::from_u64(4_900_000),
            order_id: ORDER.into(),
        };
        assert_eq!(
            verify_solver_payout_receipt(&proof, &witness, &expected, REQUEST, SOLVER)
                .unwrap()
                .net_amount_atoms,
            Quantity::from_u64(5_000_000)
        );
        let mut wrong = proof.clone();
        wrong.message_instructions[2].data_base64 = STANDARD.encode(b"wrong request");
        assert!(
            verify_solver_payout_receipt(&wrong, &witness, &expected, REQUEST, SOLVER).is_err()
        );
        let mut wrong = proof.clone();
        wrong.message_instructions[0].accounts[1].address = SOLVER.into();
        assert!(
            verify_solver_payout_receipt(&wrong, &witness, &expected, REQUEST, SOLVER).is_err()
        );
        let mut wrong = proof.clone();
        wrong.post_balances_lamports[1] -= 200_000;
        assert!(
            verify_solver_payout_receipt(&wrong, &witness, &expected, REQUEST, SOLVER).is_err()
        );
        let mut wrong = proof;
        wrong.account_keys[0] = RECIPIENT.into();
        assert!(
            verify_solver_payout_receipt(&wrong, &witness, &expected, REQUEST, SOLVER).is_err()
        );
    }

    #[test]
    fn solver_token_fill_requires_verified_spl_or_token_2022_net_credit() {
        const SOLVER: &str = "F7p3dFrjRTbtRp8FRF6qHLomXbKRBzpvBLjtQcfcgmNe";
        const REQUEST: &str = "0x1791998013d17c56b97817818856085eaf3b3e4264615b9235fce932cd82fae6";
        let mint = gofin_solana::funding::MAINNET_USDC_MINT;
        let witness = VerifiedHubOrderSettlement {
            order_id: ORDER.into(),
            order_address: derive_order_address("ethereum", ORDER).unwrap(),
            solver_hub_address: "0xd5fa4f5834722d85d95f45387ed60ce09ba4e36f".into(),
            amount_atoms: "2000000000000000".into(),
        };
        for program in [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
            let ata = gofin_solana::associated_token_address(RECIPIENT, mint, program).unwrap();
            let source_ata = gofin_solana::associated_token_address(SOLVER, mint, program).unwrap();
            let meta = |address: &str| NormalizedAccountMeta {
                address: address.into(),
                signer: false,
                writable: true,
            };
            let ix = |program_id: &str, accounts: Vec<NormalizedAccountMeta>, data: Vec<u8>| {
                NormalizedInstruction {
                    program_id: program_id.into(),
                    accounts,
                    data_base64: STANDARD.encode(data),
                }
            };
            let mut transfer_data = vec![12];
            transfer_data.extend(5_000_000u64.to_le_bytes());
            transfer_data.push(6);
            let balance = |amount: &str| json!({"accountIndex":1,"mint":mint,"owner":RECIPIENT,"programId":program,"uiTokenAmount":{"amount":amount,"decimals":6}});
            let mut proof = FinalizedReceiptProof {
                signature: "synthetic-token-fill".into(),
                slot: 10,
                fee_lamports: 5000,
                failed: false,
                message_sha256: "synthetic".into(),
                account_keys: vec![SOLVER.into(), ata.clone()],
                message_instructions: vec![
                    ix(
                        program,
                        vec![meta(&source_ata), meta(mint), meta(&ata), meta(SOLVER)],
                        transfer_data,
                    ),
                    ix(MEMO_PROGRAM, vec![], ORDER.as_bytes().to_vec()),
                    ix(MEMO_PROGRAM, vec![], REQUEST.as_bytes().to_vec()),
                ],
                required_signatures: vec!["synthetic".into()],
                pre_balances_lamports: vec![100_000, 100],
                post_balances_lamports: vec![95_000, 100],
                pre_token_balances: vec![balance("0")],
                post_token_balances: vec![balance("4900000")],
                loaded_addresses: json!({"writable":[],"readonly":[]}),
                inner_instructions: vec![],
                log_messages: vec![],
                error: None,
            };
            let expected = ExpectedPayout {
                recipient: RECIPIENT.into(),
                mint: Some(mint.into()),
                token_program: Some(program.into()),
                decimals: 6,
                minimum_amount_atoms: Quantity::from_u64(4_800_000),
                order_id: ORDER.into(),
            };
            assert_eq!(
                verify_solver_payout_receipt(&proof, &witness, &expected, REQUEST, SOLVER)
                    .unwrap()
                    .net_amount_atoms,
                Quantity::from_u64(4_900_000)
            );
            let mut created = proof.clone();
            created.account_keys.extend([
                source_ata.clone(),
                RECIPIENT.into(),
                mint.into(),
                SYSTEM_PROGRAM.into(),
                program.into(),
            ]);
            created
                .pre_balances_lamports
                .resize(created.account_keys.len(), 0);
            created
                .post_balances_lamports
                .resize(created.account_keys.len(), 0);
            created.message_instructions.insert(
                0,
                ix(
                    ASSOCIATED_TOKEN_PROGRAM,
                    vec![
                        meta(SOLVER),
                        meta(&ata),
                        meta(RECIPIENT),
                        meta(mint),
                        meta(SYSTEM_PROGRAM),
                        meta(program),
                    ],
                    vec![1],
                ),
            );
            created.pre_token_balances.clear();
            created.inner_instructions = vec![json!({"index":0,"instructions":[
                {"programIdIndex":5,"accounts":[0,1,5],"data":"111"}
            ]})];
            assert_eq!(
                verify_solver_payout_receipt(&created, &witness, &expected, REQUEST, SOLVER)
                    .unwrap()
                    .net_amount_atoms,
                Quantity::from_u64(4_900_000)
            );
            created.inner_instructions[0]["index"] = json!(1);
            assert!(
                verify_solver_payout_receipt(&created, &witness, &expected, REQUEST, SOLVER)
                    .is_err()
            );
            proof.post_token_balances = vec![balance("4700000")];
            assert!(
                verify_solver_payout_receipt(&proof, &witness, &expected, REQUEST, SOLVER).is_err()
            );
            proof.post_token_balances = vec![balance("4900000")];
            proof.message_instructions[0].accounts[1].address = SOLVER.into();
            assert!(
                verify_solver_payout_receipt(&proof, &witness, &expected, REQUEST, SOLVER).is_err()
            );
        }
    }

    #[test]
    fn hub_events_bind_order_and_exact_solana_payload() {
        let (executed, built) = logs();
        let witness = verify_hub_payout_witness(&executed, &built, &expectation()).unwrap();
        assert_eq!(
            witness.payload_sha256,
            hex::encode(Sha256::digest(payload()))
        );
        let mut wrong = expectation();
        wrong.order_id = format!("0x{}", "11".repeat(32));
        assert!(verify_hub_payout_witness(&executed, &built, &wrong).is_err());
        let mut wrong_built = built.clone();
        wrong_built.topics[1] = format!("0x{}", "22".repeat(32));
        assert!(verify_hub_payout_witness(&executed, &wrong_built, &expectation()).is_err());
    }

    #[test]
    fn payout_requires_exact_hub_payload_and_net_credit() {
        let (executed, built) = logs();
        let witness = verify_hub_payout_witness(&executed, &built, &expectation()).unwrap();
        let mut data = discriminator("execute_transfer").to_vec();
        data.extend(payload());
        let meta = |address: String| NormalizedAccountMeta {
            address,
            signer: false,
            writable: true,
        };
        let mut accounts = vec![meta(SYSTEM_PROGRAM.into()); 12];
        accounts[2] = meta(RECIPIENT.into());
        accounts[3] = meta(pda(b"vault").unwrap());
        let proof = FinalizedReceiptProof {
            signature: "synthetic-finalized-signature".into(),
            slot: 10,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "synthetic".into(),
            account_keys: vec![RECIPIENT.into(), SYSTEM_PROGRAM.into()],
            message_instructions: vec![
                NormalizedInstruction {
                    program_id: "Ed25519SigVerify111111111111111111111111111".into(),
                    accounts: vec![],
                    data_base64: "".into(),
                },
                NormalizedInstruction {
                    program_id: DEPOSITORY_PROGRAM.into(),
                    accounts,
                    data_base64: STANDARD.encode(data),
                },
            ],
            required_signatures: vec!["synthetic".into()],
            pre_balances_lamports: vec![100, 100],
            post_balances_lamports: vec![5_000_100, 100],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let expected = ExpectedPayout {
            recipient: RECIPIENT.into(),
            mint: None,
            token_program: None,
            decimals: 9,
            minimum_amount_atoms: Quantity::from_u64(5_000_000),
            order_id: ORDER.into(),
        };
        assert_eq!(
            verify_payout_receipt(&proof, &witness, &expected)
                .unwrap()
                .net_amount_atoms,
            Quantity::from_u64(5_000_000)
        );
        let mut short = proof.clone();
        short.post_balances_lamports[0] -= 1;
        assert!(verify_payout_receipt(&short, &witness, &expected).is_err());
        let mut wrong = witness;
        wrong.payload_sha256 = "00".repeat(32);
        assert!(verify_payout_receipt(&proof, &wrong, &expected).is_err());
    }

    #[test]
    fn spl_and_token_2022_payouts_require_exact_recipient_account_and_net_floor() {
        let mint = gofin_solana::funding::MAINNET_USDC_MINT;
        for program in [TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
            let mut token_payload = vec![7; 32];
            token_payload.extend(key_bytes(RECIPIENT).unwrap());
            token_payload.push(1);
            token_payload.extend(key_bytes(mint).unwrap());
            token_payload.extend(5_000_000u64.to_le_bytes());
            token_payload.extend(42u64.to_le_bytes());
            token_payload.extend(1_800_000_000i64.to_le_bytes());
            token_payload.extend(key_bytes(&pda(b"vault").unwrap()).unwrap());
            let (executed, built) = logs_for(token_payload.clone());
            let witness = verify_hub_payout_witness(&executed, &built, &expectation()).unwrap();
            let ata = gofin_solana::associated_token_address(RECIPIENT, mint, program).unwrap();
            let meta = |address: String| NormalizedAccountMeta {
                address,
                signer: false,
                writable: true,
            };
            let mut accounts = vec![meta(SYSTEM_PROGRAM.into()); 12];
            accounts[2] = meta(RECIPIENT.into());
            accounts[3] = meta(pda(b"vault").unwrap());
            accounts[4] = meta(mint.into());
            accounts[5] = meta(ata.clone());
            accounts[9] = meta(program.into());
            let mut data = discriminator("execute_transfer").to_vec();
            data.extend(token_payload);
            let balance = |amount: &str| json!({"accountIndex":1,"mint":mint,"owner":RECIPIENT,"programId":program,"uiTokenAmount":{"amount":amount,"decimals":6}});
            let mut proof = FinalizedReceiptProof {
                signature: "synthetic-token-signature".into(),
                slot: 10,
                fee_lamports: 5000,
                failed: false,
                message_sha256: "synthetic".into(),
                account_keys: vec![RECIPIENT.into(), ata],
                message_instructions: vec![
                    NormalizedInstruction {
                        program_id: "Ed25519SigVerify111111111111111111111111111".into(),
                        accounts: vec![],
                        data_base64: "".into(),
                    },
                    NormalizedInstruction {
                        program_id: DEPOSITORY_PROGRAM.into(),
                        accounts,
                        data_base64: STANDARD.encode(data),
                    },
                ],
                required_signatures: vec!["synthetic".into()],
                pre_balances_lamports: vec![100, 100],
                post_balances_lamports: vec![100, 100],
                pre_token_balances: vec![balance("0")],
                post_token_balances: vec![balance("4900000")],
                loaded_addresses: json!({"writable":[],"readonly":[]}),
                inner_instructions: vec![],
                log_messages: vec![],
                error: None,
            };
            let expected = ExpectedPayout {
                recipient: RECIPIENT.into(),
                mint: Some(mint.into()),
                token_program: Some(program.into()),
                decimals: 6,
                minimum_amount_atoms: Quantity::from_u64(4_900_000),
                order_id: ORDER.into(),
            };
            assert_eq!(
                verify_payout_receipt(&proof, &witness, &expected)
                    .unwrap()
                    .net_amount_atoms,
                Quantity::from_u64(4_900_000)
            );
            proof.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("4899999");
            assert!(verify_payout_receipt(&proof, &witness, &expected).is_err());
            proof.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("4900000");
            proof.post_token_balances[0]["mint"] = json!(RECIPIENT);
            assert!(verify_payout_receipt(&proof, &witness, &expected).is_err());
            proof.post_token_balances[0]["mint"] = json!(mint);
            proof.account_keys.pop();
            assert!(verify_payout_receipt(&proof, &witness, &expected).is_err());
        }
    }

    #[test]
    fn sol_source_receipt_requires_exact_vault_credit_and_fee_cap() {
        let vault = pda(b"vault").unwrap();
        let expected = ExpectedSourceTransaction {
            wallet: RECIPIENT.into(),
            depositor: RECIPIENT.into(),
            order_id: ORDER.into(),
            mint: None,
            token_program: None,
            decimals: 9,
            amount_atoms: Quantity::from_u64(5_000_000),
            minimum_vault_credit_atoms: Quantity::from_u64(5_000_000),
            max_network_fee_lamports: 5_000,
            max_rent_lamports: 10_000,
        };
        let validated = ValidatedSourceTransaction {
            message_sha256: "frozen-message".into(),
            order_id: ORDER.into(),
            instruction_index: 0,
            vault: vault.clone(),
            vault_token_account: None,
            sender_token_account: None,
        };
        let mut proof = FinalizedReceiptProof {
            signature: "synthetic-sol-source".into(),
            slot: 10,
            fee_lamports: 5_000,
            failed: false,
            message_sha256: "frozen-message".into(),
            account_keys: vec![RECIPIENT.into(), vault],
            message_instructions: vec![],
            required_signatures: vec![RECIPIENT.into()],
            pre_balances_lamports: vec![6_000_000, 100],
            post_balances_lamports: vec![995_000, 5_000_100],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        assert_eq!(
            verify_source_receipt(&proof, &validated, &expected)
                .unwrap()
                .vault_credit_atoms,
            Quantity::from_u64(5_000_000)
        );
        proof.post_balances_lamports[1] -= 1;
        assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
        proof.post_balances_lamports[1] += 1;
        proof.post_balances_lamports[0] -= 10_001;
        assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
        proof.post_balances_lamports[0] += 10_001;
        proof.message_sha256 = "different-message".into();
        assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
    }

    #[test]
    fn spl_and_token_2022_source_receipts_bind_mint_program_accounts_and_net_floor() {
        let wallet = RECIPIENT;
        let vault = pda(b"vault").unwrap();
        for (mint, program, vault_credit) in [
            (
                gofin_solana::funding::MAINNET_USDC_MINT,
                TOKEN_PROGRAM_ID,
                5_000_000u64,
            ),
            (RECIPIENT, TOKEN_2022_PROGRAM_ID, 4_900_000u64),
        ] {
            let sender_ata = gofin_solana::associated_token_address(wallet, mint, program).unwrap();
            let vault_ata = gofin_solana::associated_token_address(&vault, mint, program).unwrap();
            let expected = ExpectedSourceTransaction {
                wallet: wallet.into(),
                depositor: wallet.into(),
                order_id: ORDER.into(),
                mint: Some(mint.into()),
                token_program: Some(program.into()),
                decimals: 6,
                amount_atoms: Quantity::from_u64(5_000_000),
                minimum_vault_credit_atoms: Quantity::from_u64(vault_credit),
                max_network_fee_lamports: 5_000,
                max_rent_lamports: 10_000,
            };
            let validated = ValidatedSourceTransaction {
                message_sha256: "frozen-token-message".into(),
                order_id: ORDER.into(),
                instruction_index: 0,
                vault: vault.clone(),
                vault_token_account: Some(vault_ata.clone()),
                sender_token_account: Some(sender_ata.clone()),
            };
            let balance = |index: usize, owner: &str, amount: u64| {
                json!({"accountIndex":index,"mint":mint,"owner":owner,"programId":program,
                    "uiTokenAmount":{"amount":amount.to_string(),"decimals":6}})
            };
            let mut proof = FinalizedReceiptProof {
                signature: "synthetic-token-source".into(),
                slot: 11,
                fee_lamports: 5_000,
                failed: false,
                message_sha256: "frozen-token-message".into(),
                account_keys: vec![wallet.into(), vault.clone(), sender_ata, vault_ata],
                message_instructions: vec![],
                required_signatures: vec![wallet.into()],
                pre_balances_lamports: vec![100_000, 100, 100, 100],
                post_balances_lamports: vec![85_000, 100, 100, 100],
                pre_token_balances: vec![balance(2, wallet, 7_000_000), balance(3, &vault, 0)],
                post_token_balances: vec![
                    balance(2, wallet, 2_000_000),
                    balance(3, &vault, vault_credit),
                ],
                loaded_addresses: json!({"writable":[],"readonly":[]}),
                inner_instructions: vec![],
                log_messages: vec![],
                error: None,
            };
            assert_eq!(
                verify_source_receipt(&proof, &validated, &expected)
                    .unwrap()
                    .vault_credit_atoms,
                Quantity::from_u64(vault_credit)
            );
            proof.post_token_balances[1]["uiTokenAmount"]["amount"] =
                json!((vault_credit - 1).to_string());
            assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
            proof.post_token_balances[1] = balance(3, &vault, vault_credit);
            proof.post_token_balances[1]["mint"] = json!(vault);
            assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
            proof.post_token_balances[1] = balance(3, &vault, vault_credit);
            proof.post_token_balances[1]["programId"] = json!(SYSTEM_PROGRAM);
            assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
            proof.post_token_balances[1] = balance(3, &vault, vault_credit);
            proof.account_keys.pop();
            assert!(verify_source_receipt(&proof, &validated, &expected).is_err());
        }
    }
}
