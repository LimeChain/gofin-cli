//! Finalized external Solana funding receipt verification for exchange deposits.
//!
//! A wallet balance is never sufficient evidence: the receipt must bind an
//! exact finalized signature to an exact recipient and asset effect.
use fin_domain::{AssetId, FinError, Result};
use serde::{Deserialize, Serialize};

use crate::{
    NETWORK, SolanaRuntime, parse_address,
    token::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID},
    transaction::{FinalizedReceiptProof, ReceiptOutcome},
};
pub const MAINNET_USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Exact accepted interval in the asset's smallest unit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AmountBounds {
    pub minimum_atoms: u64,
    pub maximum_atoms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalPayoutRequest {
    pub signature: String,
    pub asset: AssetId,
    /// Native recipient address, or SPL token owner address.
    pub recipient: String,
    /// For SPL, bind the token-account owner as well when known.
    pub recipient_owner: Option<String>,
    /// Optional owner whose aggregate asset debit must match the payout.
    pub source_owner: Option<String>,
    pub amount: AmountBounds,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalPayoutEvidence {
    pub signature: String,
    pub slot: u64,
    pub asset: AssetId,
    pub recipient: String,
    pub recipient_accounts: Vec<String>,
    pub recipient_owner: Option<String>,
    pub credited_atoms: u64,
    pub debited_atoms: Option<u64>,
    pub network_fee_lamports: u64,
    pub transfer_instruction_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalPayoutStatus {
    Pending { reason: String },
    Finalized { evidence: ExternalPayoutEvidence },
}

/// Resolves exact asset identity. SOL is canonical; tokens require an explicit
/// mint address (a ticker never selects an arbitrary token).
pub async fn resolve_external_asset(
    runtime: &SolanaRuntime,
    coin: &str,
    mint: Option<&str>,
) -> Result<AssetId> {
    if coin == "SOL" {
        if mint.is_some_and(|m| !m.is_empty() && m != "native") {
            return Err(invalid());
        }
        return Ok(AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        });
    }
    let mint = match (coin, mint) {
        ("USDC", None) => MAINNET_USDC_MINT,
        ("USDC", Some("")) => MAINNET_USDC_MINT,
        ("USDC", Some(value)) if value != MAINNET_USDC_MINT => return Err(invalid()),
        (_, Some(value)) => value,
        (_, None) => return Err(invalid()),
    };
    let is_usdc = coin == "USDC";
    let mint = parse_address(mint)?;
    let value = runtime
        .rpc()
        .request(
            "getAccountInfo",
            serde_json::json!([mint,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let value = value
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(invalid)?;
    let owner = value
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    if owner != TOKEN_PROGRAM_ID && owner != TOKEN_2022_PROGRAM_ID {
        return Err(invalid());
    }
    let data = crate::token::decode_rpc_account_data(value.get("data").ok_or_else(invalid)?)?;
    let info = crate::token::parse_mint(owner, &data, 0)?;
    if !info.initialized || (is_usdc && (info.decimals != 6 || info.program_id != TOKEN_PROGRAM_ID))
    {
        return Err(invalid());
    }
    Ok(AssetId {
        network: NETWORK.into(),
        reference: mint,
        decimals: info.decimals,
    })
}

impl SolanaRuntime {
    /// Inspect signature history and verify the exact finalized recipient asset effect.
    pub async fn verify_external_payout(
        &self,
        request: &ExternalPayoutRequest,
    ) -> Result<ExternalPayoutStatus> {
        validate_request(request)?;
        if request.asset.reference != "native" {
            let verified =
                resolve_external_asset(self, "TOKEN", Some(&request.asset.reference)).await?;
            if verified != request.asset {
                return Err(invalid());
            }
        }
        match self
            .observe_finalized_transaction(&request.signature)
            .await?
        {
            ReceiptOutcome::Pending { reason, .. }
            | ReceiptOutcome::NotDispatched { reason, .. } => {
                Ok(ExternalPayoutStatus::Pending { reason })
            }
            ReceiptOutcome::Finalized { proof, .. } => {
                verify_external_payout_proof(&proof, request)
                    .map(|evidence| ExternalPayoutStatus::Finalized { evidence })
            }
        }
    }
}

/// Validate a previously obtained runtime proof; useful for synthetic adapter tests.
pub fn verify_external_payout_proof(
    proof: &FinalizedReceiptProof,
    request: &ExternalPayoutRequest,
) -> Result<ExternalPayoutEvidence> {
    validate_request(request)?;
    if proof.failed || proof.signature != request.signature {
        return Err(invalid());
    }
    let (credit, debit, instruction_count, owner, recipient_accounts) = if request.asset.reference
        == "native"
    {
        let delta = proof.lamport_delta(&request.recipient)?;
        if delta.is_negative() || delta.is_zero() {
            return Err(invalid());
        }
        let credited = delta
            .magnitude()
            .as_str()
            .parse::<u64>()
            .map_err(|_| invalid())?;
        let (sum, count) = system_transfer_evidence(proof, &request.recipient)?;
        if sum != credited {
            return Err(invalid());
        }
        let debited = if let Some(source) = &request.source_owner {
            let sent = system_transfer_from_to(proof, source, &request.recipient)?;
            let source_delta = proof.lamport_delta(source)?;
            let source_debit = source_delta
                .magnitude()
                .as_str()
                .parse::<u64>()
                .map_err(|_| invalid())?;
            let fee_payer = proof.account_keys.first().map(String::as_str) == Some(source);
            let expected_debit = credited
                .checked_add(if fee_payer { proof.fee_lamports } else { 0 })
                .ok_or_else(invalid)?;
            if source_delta.is_negative() && sent == credited && source_debit == expected_debit {
                Some(sent)
            } else {
                return Err(invalid());
            }
        } else {
            None
        };
        (
            credited,
            debited,
            count,
            None,
            vec![request.recipient.clone()],
        )
    } else {
        let mint = &request.asset.reference;
        let owner = &request.recipient;
        if request
            .recipient_owner
            .as_ref()
            .is_some_and(|expected| expected != owner)
        {
            return Err(invalid());
        }
        let program = token_program_for(proof, mint)?;
        let delta = proof.token_delta_verified(owner, mint, program, request.asset.decimals)?;
        let matching = if delta.is_negative() {
            return Err(invalid());
        } else {
            delta
                .magnitude()
                .as_str()
                .parse::<u64>()
                .map_err(|_| invalid())?
        };
        let mut accounts = Vec::new();
        for row in &proof.post_token_balances {
            if row.get("mint").and_then(serde_json::Value::as_str) == Some(mint)
                && row.get("owner").and_then(serde_json::Value::as_str) == Some(owner)
            {
                let index = row
                    .get("accountIndex")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(invalid)? as usize;
                let account = proof.account_keys.get(index).ok_or_else(invalid)?.clone();
                let pre = token_amount_at(&proof.pre_token_balances, index)?;
                let post = token_amount_at(&proof.post_token_balances, index)?;
                if post > pre {
                    accounts.push(account);
                }
            }
        }
        if accounts.is_empty() {
            return Err(invalid());
        }
        let debit = if let Some(source) = &request.source_owner {
            let delta =
                proof.token_delta_verified(source, mint, program, request.asset.decimals)?;
            if !delta.is_negative() {
                return Err(invalid());
            }
            let source_debit = delta
                .magnitude()
                .as_str()
                .parse::<u64>()
                .map_err(|_| invalid())?;
            // SPL transfer fees and unrelated bundled debits make the
            // owner-level debit differ from the recipient's credit. We
            // cannot attribute those safely to this payout, so fail
            // closed when a source owner was requested.
            if source_debit != matching {
                return Err(invalid());
            }
            Some(source_debit)
        } else {
            None
        };
        let source_accounts = if let Some(source) = &request.source_owner {
            let mut source_accounts = Vec::new();
            for row in proof
                .pre_token_balances
                .iter()
                .chain(&proof.post_token_balances)
            {
                if row.get("mint").and_then(serde_json::Value::as_str) == Some(mint)
                    && row.get("owner").and_then(serde_json::Value::as_str) == Some(source)
                {
                    if row.get("programId").and_then(serde_json::Value::as_str) != Some(program) {
                        return Err(invalid());
                    }
                    let index = row
                        .get("accountIndex")
                        .and_then(serde_json::Value::as_u64)
                        .ok_or_else(invalid)? as usize;
                    let account = proof.account_keys.get(index).ok_or_else(invalid)?;
                    if !source_accounts.contains(account) {
                        source_accounts.push(account.clone());
                    }
                }
            }
            if source_accounts.is_empty() {
                return Err(invalid());
            }
            Some(source_accounts)
        } else {
            None
        };
        let (sum, count) = token_transfer_evidence(
            proof,
            &accounts,
            source_accounts.as_deref(),
            program,
            mint,
            request.asset.decimals,
        )?;
        if sum != matching {
            return Err(invalid());
        }
        (matching, debit, count, Some(owner.clone()), accounts)
    };
    if credit < request.amount.minimum_atoms
        || credit > request.amount.maximum_atoms
        || credit == 0
        || instruction_count == 0
    {
        return Err(invalid());
    }
    if let Some(source) = &request.source_owner {
        if let Some(debit) = debit {
            if debit < request.amount.minimum_atoms || debit > request.amount.maximum_atoms {
                return Err(invalid());
            }
        } else {
            return Err(invalid());
        }
        if source == &request.recipient {
            return Err(invalid());
        }
    }
    Ok(ExternalPayoutEvidence {
        signature: proof.signature.clone(),
        slot: proof.slot,
        asset: request.asset.clone(),
        recipient: request.recipient.clone(),
        recipient_accounts,
        recipient_owner: owner,
        credited_atoms: credit,
        debited_atoms: debit,
        network_fee_lamports: proof.fee_lamports,
        transfer_instruction_count: instruction_count,
    })
}

fn token_amount_at(rows: &[serde_json::Value], index: usize) -> Result<u64> {
    let matches = rows
        .iter()
        .filter(|row| {
            row.get("accountIndex").and_then(serde_json::Value::as_u64) == Some(index as u64)
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(invalid());
    }
    match matches.first() {
        None => Ok(0),
        Some(row) => row
            .pointer("/uiTokenAmount/amount")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?
            .parse::<u64>()
            .map_err(|_| invalid()),
    }
}

fn system_transfer_evidence(
    proof: &FinalizedReceiptProof,
    recipient: &str,
) -> Result<(u64, usize)> {
    let mut total = 0u64;
    let mut count = 0usize;
    for ix in &proof.message_instructions {
        if ix.program_id != "11111111111111111111111111111111"
            || ix.accounts.len() < 2
            || ix.accounts[1].address != recipient
        {
            continue;
        }
        let data =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ix.data_base64)
                .map_err(|_| invalid())?;
        if data.len() != 12 || u32::from_le_bytes(data[..4].try_into().map_err(|_| invalid())?) != 2
        {
            continue;
        }
        let atoms = u64::from_le_bytes(data[4..12].try_into().map_err(|_| invalid())?);
        total = total.checked_add(atoms).ok_or_else(invalid)?;
        count += 1;
    }
    for group in &proof.inner_instructions {
        for ix in group
            .get("instructions")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let pid = inner_program(ix, proof)?;
            let accounts = inner_accounts(ix, proof)?;
            if pid != "11111111111111111111111111111111"
                || accounts.len() < 2
                || accounts[1] != recipient
            {
                continue;
            }
            let data = ix
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(invalid)?;
            let data = bs58::decode(data).into_vec().map_err(|_| invalid())?;
            if data.len() != 12
                || u32::from_le_bytes(data[..4].try_into().map_err(|_| invalid())?) != 2
            {
                continue;
            }
            total = total
                .checked_add(u64::from_le_bytes(
                    data[4..12].try_into().map_err(|_| invalid())?,
                ))
                .ok_or_else(invalid)?;
            count += 1;
        }
    }
    Ok((total, count))
}

fn system_transfer_from_to(
    proof: &FinalizedReceiptProof,
    source: &str,
    recipient: &str,
) -> Result<u64> {
    let mut total = 0u64;
    for ix in &proof.message_instructions {
        if ix.program_id != "11111111111111111111111111111111"
            || ix.accounts.len() < 2
            || ix.accounts[0].address != source
            || ix.accounts[1].address != recipient
        {
            continue;
        }
        let data =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ix.data_base64)
                .map_err(|_| invalid())?;
        if data.len() == 12 && u32::from_le_bytes(data[..4].try_into().map_err(|_| invalid())?) == 2
        {
            total = total
                .checked_add(u64::from_le_bytes(
                    data[4..12].try_into().map_err(|_| invalid())?,
                ))
                .ok_or_else(invalid)?;
        }
    }
    for group in &proof.inner_instructions {
        for ix in group
            .get("instructions")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if inner_program(ix, proof)? != "11111111111111111111111111111111" {
                continue;
            }
            let accounts = inner_accounts(ix, proof)?;
            if accounts.len() < 2 || accounts[0] != source || accounts[1] != recipient {
                continue;
            }
            let data = ix
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(invalid)?;
            let data = bs58::decode(data).into_vec().map_err(|_| invalid())?;
            if data.len() == 12
                && u32::from_le_bytes(data[..4].try_into().map_err(|_| invalid())?) == 2
            {
                total = total
                    .checked_add(u64::from_le_bytes(
                        data[4..12].try_into().map_err(|_| invalid())?,
                    ))
                    .ok_or_else(invalid)?;
            }
        }
    }
    Ok(total)
}

fn token_transfer_evidence(
    proof: &FinalizedReceiptProof,
    recipients: &[String],
    sources: Option<&[String]>,
    program: &str,
    mint: &str,
    decimals: u8,
) -> Result<(u64, usize)> {
    let mut total = 0u64;
    let mut count = 0usize;
    for ix in &proof.message_instructions {
        if ix.program_id != program {
            continue;
        }
        let data =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ix.data_base64)
                .map_err(|_| invalid())?;
        let (destination_index, amount) = match data.first().copied() {
            Some(3) if data.len() == 9 && ix.accounts.len() >= 2 => (
                1,
                u64::from_le_bytes(data[1..9].try_into().map_err(|_| invalid())?),
            ),
            Some(12) if data.len() == 10 && ix.accounts.len() >= 3 => {
                if ix.accounts[1].address != mint || data[9] != decimals {
                    return Err(invalid());
                }
                (
                    2,
                    u64::from_le_bytes(data[1..9].try_into().map_err(|_| invalid())?),
                )
            }
            _ => continue,
        };
        if recipients.contains(&ix.accounts[destination_index].address)
            && sources.is_none_or(|sources| {
                ix.accounts
                    .first()
                    .is_some_and(|source| sources.contains(&source.address))
            })
        {
            total = total.checked_add(amount).ok_or_else(invalid)?;
            count += 1;
        }
    }
    for group in &proof.inner_instructions {
        for ix in group
            .get("instructions")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if inner_program(ix, proof)? != program {
                continue;
            }
            let accounts = inner_accounts(ix, proof)?;
            let data = ix
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(invalid)?;
            let data = bs58::decode(data).into_vec().map_err(|_| invalid())?;
            let (destination_index, amount) = match data.first().copied() {
                Some(3) if data.len() == 9 && accounts.len() >= 2 => (
                    1,
                    u64::from_le_bytes(data[1..9].try_into().map_err(|_| invalid())?),
                ),
                Some(12) if data.len() == 10 && accounts.len() >= 3 => {
                    if accounts[1] != mint || data[9] != decimals {
                        return Err(invalid());
                    }
                    (
                        2,
                        u64::from_le_bytes(data[1..9].try_into().map_err(|_| invalid())?),
                    )
                }
                _ => continue,
            };
            if recipients.contains(&accounts[destination_index])
                && sources.is_none_or(|sources| {
                    accounts
                        .first()
                        .is_some_and(|source| sources.contains(source))
                })
            {
                total = total.checked_add(amount).ok_or_else(invalid)?;
                count += 1;
            }
        }
    }
    Ok((total, count))
}

fn inner_program<'a>(
    ix: &'a serde_json::Value,
    proof: &'a FinalizedReceiptProof,
) -> Result<&'a str> {
    let index = ix
        .get("programIdIndex")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(invalid)? as usize;
    proof
        .account_keys
        .get(index)
        .map(String::as_str)
        .ok_or_else(invalid)
}
fn inner_accounts(ix: &serde_json::Value, proof: &FinalizedReceiptProof) -> Result<Vec<String>> {
    ix.get("accounts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(invalid)?
        .iter()
        .map(|item| {
            let index = item.as_u64().ok_or_else(invalid)? as usize;
            proof.account_keys.get(index).cloned().ok_or_else(invalid)
        })
        .collect()
}

fn token_program_for(proof: &FinalizedReceiptProof, mint: &str) -> Result<&'static str> {
    for side in [&proof.pre_token_balances, &proof.post_token_balances] {
        for row in side {
            if row.get("mint").and_then(serde_json::Value::as_str) == Some(mint) {
                return match row.get("programId").and_then(serde_json::Value::as_str) {
                    Some(TOKEN_PROGRAM_ID) => Ok(TOKEN_PROGRAM_ID),
                    Some(TOKEN_2022_PROGRAM_ID) => Ok(TOKEN_2022_PROGRAM_ID),
                    _ => Err(invalid()),
                };
            }
        }
    }
    Err(invalid())
}
fn validate_request(r: &ExternalPayoutRequest) -> Result<()> {
    if r.signature.is_empty()
        || r.signature.len() > 88
        || r.asset.network != NETWORK
        || r.amount.minimum_atoms == 0
        || r.amount.minimum_atoms > r.amount.maximum_atoms
    {
        return Err(invalid());
    }
    let mut signature_bytes = zeroize::Zeroizing::new([0u8; 64]);
    let signature_len = bs58::decode(&r.signature)
        .onto(signature_bytes.as_mut())
        .map_err(|_| invalid())?;
    if signature_len != 64 || bs58::encode(signature_bytes.as_ref()).into_string() != r.signature {
        return Err(invalid());
    }
    parse_address(&r.recipient)?;
    if let Some(owner) = &r.recipient_owner {
        parse_address(owner)?;
    }
    if let Some(owner) = &r.source_owner {
        parse_address(owner)?;
    }
    if r.asset.reference == "native" {
        if r.asset.decimals != 9 || r.recipient_owner.is_some() {
            return Err(invalid());
        }
    } else {
        parse_address(&r.asset.reference)?;
    }
    Ok(())
}
fn invalid() -> FinError {
    FinError::new(
        "SOLANA_EXTERNAL_PAYOUT_INVALID",
        "receipt",
        "The finalized Solana transaction does not prove the requested external payout.",
        "Check the signature, asset mint, recipient and exact amount in finalized transaction history.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::{NormalizedAccountMeta, NormalizedInstruction};
    use serde_json::json;

    const OWNER: &str = "11111111111111111111111111111111";
    const TOKEN_ACCOUNT: &str = "Vote111111111111111111111111111111111111111";
    const MINT: &str = "So11111111111111111111111111111111111111112";
    const SOURCE_TOKEN_ACCOUNT: &str = "SysvarRent111111111111111111111111111111111";
    fn proof() -> FinalizedReceiptProof {
        FinalizedReceiptProof {
            signature: "1".repeat(64),
            slot: 7,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "00".repeat(32),
            account_keys: vec![OWNER.into(), TOKEN_ACCOUNT.into(), MINT.into()],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![1_000_000, 0, 0],
            post_balances_lamports: vec![900_000, 100_000, 0],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        }
    }
    fn request(recipient: &str) -> ExternalPayoutRequest {
        ExternalPayoutRequest {
            signature: "1".repeat(64),
            asset: AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            recipient: recipient.into(),
            recipient_owner: None,
            source_owner: Some(OWNER.into()),
            amount: AmountBounds {
                minimum_atoms: 100_000,
                maximum_atoms: 100_000,
            },
        }
    }
    fn system_transfer(to: &str, atoms: u64) -> NormalizedInstruction {
        use base64::Engine;
        let mut bytes = 2u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&atoms.to_le_bytes());
        NormalizedInstruction {
            program_id: "11111111111111111111111111111111".into(),
            accounts: vec![
                NormalizedAccountMeta {
                    address: OWNER.into(),
                    signer: true,
                    writable: true,
                },
                NormalizedAccountMeta {
                    address: to.into(),
                    signer: false,
                    writable: true,
                },
            ],
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    #[test]
    fn native_payout_requires_exact_instruction_and_finalized_effect() {
        let mut p = proof();
        p.post_balances_lamports[0] = 895_000;
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        let evidence = verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT)).unwrap();
        assert_eq!(evidence.credited_atoms, 100_000);
        assert_eq!(evidence.debited_atoms, Some(100_000));
    }
    #[test]
    fn batched_native_transfers_are_aggregated_but_duplicate_credit_is_bounded() {
        let mut p = proof();
        p.post_balances_lamports[0] = 795_000;
        p.post_balances_lamports[1] = 200_000;
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        let mut req = request(TOKEN_ACCOUNT);
        req.amount.minimum_atoms = 200_000;
        req.amount.maximum_atoms = 200_000;
        let ev = verify_external_payout_proof(&p, &req).unwrap();
        assert_eq!(ev.transfer_instruction_count, 2);
        req.amount.maximum_atoms = 100_000;
        assert!(verify_external_payout_proof(&p, &req).is_err());
    }

    #[test]
    fn native_cpi_transfer_is_evidence_and_must_name_exact_recipient() {
        let mut p = proof();
        p.post_balances_lamports[0] = 895_000;
        p.inner_instructions = vec![
            json!({"index":0,"instructions":[{"programIdIndex":0,"accounts":[0,1],"data":bs58::encode([2u32.to_le_bytes().as_slice(),100_000u64.to_le_bytes().as_slice()].concat()).into_string()}]}),
        ];
        let result = verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT));
        assert!(result.is_ok(), "{result:?}");
        p.inner_instructions[0]["instructions"][0]["accounts"] = json!([0, 0]);
        assert!(verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT)).is_err());
    }
    #[test]
    fn balance_change_without_matching_transfer_is_not_proof() {
        assert!(verify_external_payout_proof(&proof(), &request(TOKEN_ACCOUNT)).is_err());
    }
    #[test]
    fn net_outgoing_lamport_delta_cannot_become_a_positive_credit() {
        let mut p = proof();
        p.post_balances_lamports[1] = 0;
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        assert!(verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT)).is_err());
    }
    #[test]
    fn recipient_identity_is_case_sensitive() {
        let mut p = proof();
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        let mut req = request(TOKEN_ACCOUNT);
        req.recipient = TOKEN_ACCOUNT.to_ascii_lowercase();
        assert!(verify_external_payout_proof(&p, &req).is_err());
    }
    #[test]
    fn failed_or_wrong_signature_receipt_is_rejected() {
        let mut p = proof();
        p.failed = true;
        p.message_instructions
            .push(system_transfer(TOKEN_ACCOUNT, 100_000));
        assert!(verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT)).is_err());
        p.failed = false;
        p.signature = "other-signature".into();
        assert!(verify_external_payout_proof(&p, &request(TOKEN_ACCOUNT)).is_err());
    }

    #[test]
    fn token_payout_requires_exact_mint_owner_and_instruction() {
        use base64::Engine;
        let mut p = proof();
        p.account_keys = vec![
            OWNER.into(),
            TOKEN_ACCOUNT.into(),
            MINT.into(),
            TOKEN_PROGRAM_ID.into(),
            SOURCE_TOKEN_ACCOUNT.into(),
        ];
        p.pre_balances_lamports = vec![1_000_000, 0, 0, 0, 0];
        p.post_balances_lamports = vec![995_000, 0, 0, 0, 0];
        p.pre_token_balances = vec![
            json!({"accountIndex":1,"mint":MINT,"owner":OWNER,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"0","decimals":9}}),
        ];
        p.post_token_balances = vec![
            json!({"accountIndex":1,"mint":MINT,"owner":OWNER,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100000","decimals":9}}),
        ];
        p.pre_token_balances.push(json!({"accountIndex":4,"mint":MINT,"owner":MINT,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100000","decimals":9}}));
        p.post_token_balances.push(json!({"accountIndex":4,"mint":MINT,"owner":MINT,"programId":TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"0","decimals":9}}));
        let mut bytes = vec![3];
        bytes.extend_from_slice(&100_000u64.to_le_bytes());
        p.message_instructions.push(NormalizedInstruction {
            program_id: TOKEN_PROGRAM_ID.into(),
            accounts: vec![
                NormalizedAccountMeta {
                    address: SOURCE_TOKEN_ACCOUNT.into(),
                    signer: false,
                    writable: true,
                },
                NormalizedAccountMeta {
                    address: TOKEN_ACCOUNT.into(),
                    signer: false,
                    writable: true,
                },
            ],
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
        let req = ExternalPayoutRequest {
            signature: p.signature.clone(),
            asset: AssetId {
                network: NETWORK.into(),
                reference: MINT.into(),
                decimals: 9,
            },
            recipient: OWNER.into(),
            recipient_owner: None,
            source_owner: Some(MINT.into()),
            amount: AmountBounds {
                minimum_atoms: 100_000,
                maximum_atoms: 100_000,
            },
        };
        let evidence = verify_external_payout_proof(&p, &req).unwrap();
        assert_eq!(evidence.recipient_accounts, vec![TOKEN_ACCOUNT]);
        assert_eq!(evidence.debited_atoms, Some(100_000));
        let mut unrelated_source = p.clone();
        unrelated_source.message_instructions[0].accounts[0].address = "source".into();
        assert!(verify_external_payout_proof(&unrelated_source, &req).is_err());
        let mut wrong = req.clone();
        wrong.asset.reference = "So11111111111111111111111111111111111111121".into();
        assert!(verify_external_payout_proof(&p, &wrong).is_err());
        let mut wrong_owner = req.clone();
        wrong_owner.recipient = TOKEN_ACCOUNT.into();
        assert!(verify_external_payout_proof(&p, &wrong_owner).is_err());
        let mut wrong_decimals = req;
        wrong_decimals.asset.decimals = 6;
        assert!(verify_external_payout_proof(&p, &wrong_decimals).is_err());
        let mut fee_mismatch = p;
        let mut gross = vec![3];
        gross.extend_from_slice(&100_001u64.to_le_bytes());
        fee_mismatch.message_instructions[0].data_base64 =
            base64::engine::general_purpose::STANDARD.encode(gross);
        assert!(
            verify_external_payout_proof(
                &fee_mismatch,
                &ExternalPayoutRequest {
                    signature: fee_mismatch.signature.clone(),
                    asset: AssetId {
                        network: NETWORK.into(),
                        reference: MINT.into(),
                        decimals: 9
                    },
                    recipient: OWNER.into(),
                    recipient_owner: None,
                    source_owner: None,
                    amount: AmountBounds {
                        minimum_atoms: 100_000,
                        maximum_atoms: 100_000
                    }
                }
            )
            .is_err()
        );
    }

    struct StatusRpc(serde_json::Value);
    #[async_trait::async_trait]
    impl crate::SolanaRpc for StatusRpc {
        async fn request(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value> {
            if self.0.get("offline").and_then(serde_json::Value::as_bool) == Some(true) {
                return Err(FinError::new(
                    "SOLANA_RPC_UNAVAILABLE",
                    "rpc",
                    "offline",
                    "retry",
                ));
            }
            if method == "getSignatureStatuses" {
                Ok(self.0.clone())
            } else {
                Err(FinError::new(
                    "SOLANA_RPC_UNAVAILABLE",
                    "rpc",
                    "offline",
                    "retry",
                ))
            }
        }
    }

    #[tokio::test]
    async fn missing_unfinalized_and_unavailable_history_remain_pending() {
        use std::sync::Arc;
        for value in [
            json!({"value":[null]}),
            json!({"value":[{"confirmationStatus":"confirmed","err":null}]}),
        ] {
            let runtime = SolanaRuntime::new(Arc::new(StatusRpc(value)));
            let mut req = request(TOKEN_ACCOUNT);
            req.source_owner = None;
            assert!(matches!(
                runtime.verify_external_payout(&req).await.unwrap(),
                ExternalPayoutStatus::Pending { .. }
            ));
        }
        let runtime = SolanaRuntime::new(Arc::new(StatusRpc(json!({"value":[]}))));
        let req = request(TOKEN_ACCOUNT);
        assert!(matches!(
            runtime.verify_external_payout(&req).await.unwrap(),
            ExternalPayoutStatus::Pending { .. }
        ));
        let runtime = SolanaRuntime::new(Arc::new(StatusRpc(json!({"offline":true}))));
        assert!(matches!(
            runtime.verify_external_payout(&req).await.unwrap(),
            ExternalPayoutStatus::Pending { .. }
        ));
    }
}
