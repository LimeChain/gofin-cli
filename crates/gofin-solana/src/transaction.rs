use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_address::Address;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::VersionedMessage;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::{
    failure,
    key::{MessageSigner, SolanaSigner},
    parse_error,
    rpc::SolanaRpc,
    token::decode_rpc_account_data,
};
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};

const MAX_TRANSACTION_BYTES: usize = 1232;
const ALT_PROGRAM_ID: &str = "AddressLookupTab1e1111111111111111111111111";

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedAccountMeta {
    pub address: String,
    pub signer: bool,
    pub writable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedInstruction {
    pub program_id: String,
    pub accounts: Vec<NormalizedAccountMeta>,
    pub data_base64: String,
}

/// Frozen provider message, privately held so callers can validate its
/// normalized instructions without mutating its blockhash, signers, or bytes.
pub struct FrozenTransaction {
    transaction: VersionedTransaction,
    message_bytes: Vec<u8>,
    message_sha256: String,
    signer: String,
    fee_payer: String,
    signer_index: usize,
    required_signers: Vec<String>,
    missing_cosigners: Vec<String>,
    recent_blockhash: String,
    last_valid_block_height: Option<u64>,
    provider_expire_at: Option<String>,
    maximum_network_fee_lamports: u64,
    provider_request_id: Option<String>,
    instructions: Vec<NormalizedInstruction>,
}

impl std::fmt::Debug for FrozenTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenTransaction")
            .field("message_sha256", &self.message_sha256)
            .field("signer", &self.signer)
            .field("recent_blockhash", &self.recent_blockhash)
            .field("last_valid_block_height", &self.last_valid_block_height)
            .field("wire", &"[REDACTED]")
            .finish()
    }
}

impl FrozenTransaction {
    pub fn message_sha256(&self) -> &str {
        &self.message_sha256
    }
    pub fn signer(&self) -> &str {
        &self.signer
    }
    pub fn fee_payer(&self) -> &str {
        &self.fee_payer
    }
    pub fn recent_blockhash(&self) -> &str {
        &self.recent_blockhash
    }
    pub fn last_valid_block_height(&self) -> Option<u64> {
        self.last_valid_block_height
    }
    pub fn provider_expire_at(&self) -> Option<&str> {
        self.provider_expire_at.as_deref()
    }
    pub fn maximum_network_fee_lamports(&self) -> u64 {
        self.maximum_network_fee_lamports
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn required_signers(&self) -> &[String] {
        &self.required_signers
    }
    pub fn missing_cosigners(&self) -> &[String] {
        &self.missing_cosigners
    }
    pub fn instructions(&self) -> &[NormalizedInstruction] {
        &self.instructions
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptOutcome {
    Pending {
        confirmed: bool,
        reason: String,
    },
    NotDispatched {
        reason_code: String,
        reason: String,
    },
    Finalized {
        signature: String,
        slot: u64,
        fee_lamports: u64,
        failed: bool,
        message_sha256: String,
        proof: Box<FinalizedReceiptProof>,
    },
}

/// Finalized runtime evidence. Applications must still interpret instruction
/// semantics and verify the exact expected asset effects before settlement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FinalizedReceiptProof {
    pub signature: String,
    pub slot: u64,
    pub fee_lamports: u64,
    pub failed: bool,
    pub message_sha256: String,
    pub account_keys: Vec<String>,
    pub message_instructions: Vec<NormalizedInstruction>,
    /// Ordered encoded signature bytes from the finalized transaction.
    /// These are not signer public keys. Signer identities are the first
    /// `num_required_signatures` entries of `account_keys`, in message-header
    /// order, after the runtime has verified the transaction envelope.
    pub required_signatures: Vec<String>,
    pub pre_balances_lamports: Vec<u64>,
    pub post_balances_lamports: Vec<u64>,
    pub pre_token_balances: Vec<Value>,
    pub post_token_balances: Vec<Value>,
    pub loaded_addresses: Value,
    pub inner_instructions: Vec<Value>,
    pub log_messages: Vec<String>,
    pub error: Option<Value>,
}

impl FinalizedReceiptProof {
    /// Exact account lamport delta (post minus pre) from this finalized proof.
    pub fn lamport_delta(&self, account: &str) -> Result<gofin_domain::SignedQuantity> {
        let matches = self
            .account_keys
            .iter()
            .enumerate()
            .filter(|(_, key)| key.as_str() == account)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(pending_evidence_error());
        }
        let index = matches[0];
        let pre = gofin_domain::Quantity::from_u64(
            *self
                .pre_balances_lamports
                .get(index)
                .ok_or_else(pending_evidence_error)?,
        );
        let post = gofin_domain::Quantity::from_u64(
            *self
                .post_balances_lamports
                .get(index)
                .ok_or_else(pending_evidence_error)?,
        );
        signed_delta(&pre, &post)
    }

    /// Aggregate exact SPL atom balances for one owner and mint across all of
    /// its token accounts, including accounts loaded through address tables.
    pub fn token_delta(&self, owner: &str, mint: &str) -> Result<gofin_domain::SignedQuantity> {
        let pre = sum_owner_mint(&self.pre_token_balances, &self.account_keys, owner, mint)?;
        let post = sum_owner_mint(&self.post_token_balances, &self.account_keys, owner, mint)?;
        signed_delta(&pre, &post)
    }

    /// Strict aggregate token delta bound to the reviewed owner, mint, token
    /// program and decimals. If an account index appears on both sides, its
    /// token identity must remain unchanged across the transaction.
    pub fn token_delta_verified(
        &self,
        owner: &str,
        mint: &str,
        token_program: &str,
        decimals: u8,
    ) -> Result<gofin_domain::SignedQuantity> {
        if token_program != crate::token::TOKEN_PROGRAM_ID
            && token_program != crate::token::TOKEN_2022_PROGRAM_ID
        {
            return Err(receipt_identity_error());
        }
        let (pre, pre_accounts) = sum_owner_mint_verified(
            &self.pre_token_balances,
            &self.account_keys,
            owner,
            mint,
            token_program,
            decimals,
        )?;
        let (post, post_accounts) = sum_owner_mint_verified(
            &self.post_token_balances,
            &self.account_keys,
            owner,
            mint,
            token_program,
            decimals,
        )?;
        for (index, identity) in &pre_accounts {
            if let Some(post_identity) = post_accounts.get(index)
                && (identity.is_target(owner, mint) || post_identity.is_target(owner, mint))
                && identity != post_identity
            {
                return Err(receipt_identity_error());
            }
        }
        signed_delta(&pre, &post)
    }

    /// Exact SPL atom delta for one token account, bound to its expected token
    /// owner and mint. Unlike `token_delta`, this deliberately does not
    /// aggregate other accounts controlled by that owner.
    pub fn token_account_delta(
        &self,
        account: &str,
        owner: &str,
        mint: &str,
        decimals: u8,
    ) -> Result<gofin_domain::SignedQuantity> {
        let pre = balance_for_account(
            &self.pre_token_balances,
            &self.account_keys,
            account,
            owner,
            mint,
            decimals,
        )?;
        let post = balance_for_account(
            &self.post_token_balances,
            &self.account_keys,
            account,
            owner,
            mint,
            decimals,
        )?;
        signed_delta(&pre, &post)
    }

    /// Exact token-account delta that also binds the token program from both
    /// sides of the finalized receipt. A one-sided row is accepted for an
    /// account created or closed in this transaction; both rows absent is not.
    pub fn token_account_delta_verified(
        &self,
        account: &str,
        owner: &str,
        mint: &str,
        token_program: &str,
        decimals: u8,
    ) -> Result<gofin_domain::SignedQuantity> {
        if token_program != crate::token::TOKEN_PROGRAM_ID
            && token_program != crate::token::TOKEN_2022_PROGRAM_ID
        {
            return Err(receipt_identity_error());
        }
        let (pre, pre_program) = balance_for_account_verified(
            &self.pre_token_balances,
            &self.account_keys,
            account,
            owner,
            mint,
            decimals,
        )?;
        let (post, post_program) = balance_for_account_verified(
            &self.post_token_balances,
            &self.account_keys,
            account,
            owner,
            mint,
            decimals,
        )?;
        if pre_program.is_none() && post_program.is_none()
            || pre_program
                .as_deref()
                .is_some_and(|program| program != token_program)
            || post_program
                .as_deref()
                .is_some_and(|program| program != token_program)
        {
            return Err(receipt_identity_error());
        }
        if pre_program.is_some() && post_program.is_some() && pre_program != post_program {
            return Err(receipt_identity_error());
        }
        signed_delta(&pre, &post)
    }
}

fn signed_delta(
    pre: &gofin_domain::Quantity,
    post: &gofin_domain::Quantity,
) -> Result<gofin_domain::SignedQuantity> {
    if post.integer() >= pre.integer() {
        gofin_domain::SignedQuantity::new(post.checked_sub(pre)?.as_str())
    } else {
        gofin_domain::SignedQuantity::new(format!("-{}", pre.checked_sub(post)?.as_str()))
    }
}

fn sum_owner_mint(
    balances: &[Value],
    keys: &[String],
    owner: &str,
    mint: &str,
) -> Result<gofin_domain::Quantity> {
    if balances.len() > 256 {
        return Err(pending_evidence_error());
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = gofin_domain::Quantity::zero();
    for balance in balances {
        if balance.get("owner").and_then(Value::as_str) != Some(owner)
            || balance.get("mint").and_then(Value::as_str) != Some(mint)
        {
            continue;
        }
        let index = balance
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(pending_evidence_error)?;
        let key = keys.get(index).ok_or_else(pending_evidence_error)?;
        if !seen.insert((index, key)) {
            return Err(receipt_identity_error());
        }
        let atoms = balance
            .get("uiTokenAmount")
            .and_then(|v| v.get("amount"))
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        total = total.checked_add(&gofin_domain::Quantity::new(atoms)?)?;
    }
    Ok(total)
}

#[derive(PartialEq, Eq)]
struct TokenBalanceIdentity {
    owner: String,
    mint: String,
    program: String,
    decimals: u8,
}

impl TokenBalanceIdentity {
    fn is_target(&self, owner: &str, mint: &str) -> bool {
        self.owner == owner && self.mint == mint
    }
}

fn sum_owner_mint_verified(
    balances: &[Value],
    keys: &[String],
    owner: &str,
    mint: &str,
    token_program: &str,
    expected_decimals: u8,
) -> Result<(
    gofin_domain::Quantity,
    std::collections::HashMap<usize, TokenBalanceIdentity>,
)> {
    if balances.len() > 256 {
        return Err(pending_evidence_error());
    }
    let mut seen = std::collections::HashSet::new();
    let mut target_accounts = std::collections::HashMap::new();
    let mut total = gofin_domain::Quantity::zero();
    for balance in balances {
        let index = balance
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(pending_evidence_error)?;
        if keys.get(index).is_none() || !seen.insert(index) {
            return Err(receipt_identity_error());
        }
        let row_owner = balance
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        let row_mint = balance
            .get("mint")
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        let program = balance
            .get("programId")
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        if program != crate::token::TOKEN_PROGRAM_ID
            && program != crate::token::TOKEN_2022_PROGRAM_ID
        {
            return Err(receipt_identity_error());
        }
        let decimals = balance
            .get("uiTokenAmount")
            .and_then(|value| value.get("decimals"))
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(pending_evidence_error)?;
        let identity = TokenBalanceIdentity {
            owner: row_owner.to_owned(),
            mint: row_mint.to_owned(),
            program: program.to_owned(),
            decimals,
        };
        if row_owner == owner && row_mint == mint {
            if program != token_program || decimals != expected_decimals {
                return Err(receipt_identity_error());
            }
            let atoms = balance
                .get("uiTokenAmount")
                .and_then(|value| value.get("amount"))
                .and_then(Value::as_str)
                .ok_or_else(pending_evidence_error)?;
            total = total.checked_add(&gofin_domain::Quantity::new(atoms)?)?;
        }
        target_accounts.insert(index, identity);
    }
    Ok((total, target_accounts))
}

fn balance_for_account(
    balances: &[Value],
    keys: &[String],
    account: &str,
    owner: &str,
    mint: &str,
    expected_decimals: u8,
) -> Result<gofin_domain::Quantity> {
    if balances.len() > 256 {
        return Err(pending_evidence_error());
    }
    let indexes = keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| (key == account).then_some(index))
        .collect::<Vec<_>>();
    if indexes.len() != 1 {
        return Err(pending_evidence_error());
    }
    let account_index = indexes[0];
    let mut amount = None;
    for balance in balances {
        let Some(index) = balance
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        else {
            return Err(pending_evidence_error());
        };
        if index != account_index {
            continue;
        }
        if balance.get("owner").and_then(Value::as_str) != Some(owner)
            || balance.get("mint").and_then(Value::as_str) != Some(mint)
        {
            return Err(receipt_identity_error());
        }
        let decimals = balance
            .get("uiTokenAmount")
            .and_then(|value| value.get("decimals"))
            .and_then(Value::as_u64)
            .ok_or_else(pending_evidence_error)?;
        if decimals != u64::from(expected_decimals) {
            return Err(pending_evidence_error());
        }
        if let Some(program) = balance.get("programId").and_then(Value::as_str)
            && program != crate::token::TOKEN_PROGRAM_ID
            && program != crate::token::TOKEN_2022_PROGRAM_ID
        {
            return Err(receipt_identity_error());
        }
        if amount.is_some() {
            return Err(receipt_identity_error());
        }
        let atoms = balance
            .get("uiTokenAmount")
            .and_then(|value| value.get("amount"))
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        amount = Some(gofin_domain::Quantity::new(atoms)?);
    }
    Ok(amount.unwrap_or_else(gofin_domain::Quantity::zero))
}

fn balance_for_account_verified(
    balances: &[Value],
    keys: &[String],
    account: &str,
    owner: &str,
    mint: &str,
    expected_decimals: u8,
) -> Result<(gofin_domain::Quantity, Option<String>)> {
    if balances.len() > 256 {
        return Err(pending_evidence_error());
    }
    let indexes = keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| (key == account).then_some(index))
        .collect::<Vec<_>>();
    if indexes.len() != 1 {
        return Err(pending_evidence_error());
    }
    let expected_index = indexes[0];
    let mut amount = None;
    let mut program = None;
    for balance in balances {
        let index = balance
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(pending_evidence_error)?;
        if index != expected_index {
            continue;
        }
        if balance.get("owner").and_then(Value::as_str) != Some(owner)
            || balance.get("mint").and_then(Value::as_str) != Some(mint)
        {
            return Err(receipt_identity_error());
        }
        let decimals = balance
            .get("uiTokenAmount")
            .and_then(|value| value.get("decimals"))
            .and_then(Value::as_u64)
            .ok_or_else(pending_evidence_error)?;
        if decimals != u64::from(expected_decimals) {
            return Err(receipt_identity_error());
        }
        let current_program = balance
            .get("programId")
            .and_then(Value::as_str)
            .filter(|value| {
                *value == crate::token::TOKEN_PROGRAM_ID
                    || *value == crate::token::TOKEN_2022_PROGRAM_ID
            })
            .ok_or_else(pending_evidence_error)?;
        let atoms = balance
            .get("uiTokenAmount")
            .and_then(|value| value.get("amount"))
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        if amount.is_some() {
            return Err(receipt_identity_error());
        }
        amount = Some(gofin_domain::Quantity::new(atoms)?);
        program = Some(current_program.to_owned());
    }
    Ok((amount.unwrap_or_else(gofin_domain::Quantity::zero), program))
}

pub struct SolanaRuntime {
    rpc: Arc<dyn SolanaRpc>,
}

struct RuntimeDispatchGuard<'a> {
    runtime: &'a SolanaRuntime,
    frozen: &'a FrozenTransaction,
    journal: &'a dyn SubmissionJournal,
    prepared: &'a PreparedOperation,
    denied: &'a AtomicBool,
}

#[async_trait]
impl AsyncDispatchGuard for RuntimeDispatchGuard<'_> {
    async fn check(&self) -> Result<()> {
        let checked = async {
            self.journal.assert_candidate(self.prepared)?;
            self.runtime.assert_lifetime_valid(self.frozen).await?;
            // Lifetime validation may await one or more RPC calls. Consent and
            // candidate identity must still be current at the actual send edge.
            self.journal.assert_candidate(self.prepared)
        }
        .await;
        if let Err(error) = checked {
            self.denied.store(true, Ordering::Release);
            return Err(GoFinError::new(
                "SOLANA_PRE_SEND_GUARD_DENIED",
                "dispatch",
                "Authorization or the original transaction lifetime expired before network send.",
                "No transaction was sent; prepare and approve a fresh operation if still intended.",
            )
            .with_details(json!({"reason_code":error.code})));
        }
        Ok(())
    }
}

/// Pluggable one-shot landing path. The signed wire is supplied only for the
/// immediate provider call and is never written to the financial journal.
#[async_trait]
pub trait TransactionSender: Send + Sync {
    /// Recheck authorization at the last practical instant before a network
    /// send. Providers with a rate queue should override this and invoke the
    /// callback after acquiring a slot.
    async fn send_signed_checked(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<String> {
        before_send()?;
        self.send_signed(signed_transaction_base64, frozen).await
    }

    /// Provider senders with asynchronous rate queues should override this and
    /// invoke `guard.check().await` after waiting and immediately before send.
    async fn send_signed_with_guard(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
        guard: &dyn AsyncDispatchGuard,
    ) -> Result<String> {
        guard.check().await?;
        self.send_signed(signed_transaction_base64, frozen).await
    }

    fn submission_reference(
        &self,
        frozen: &FrozenTransaction,
        first_signature: Option<&str>,
    ) -> Result<String> {
        first_signature
            .map(str::to_owned)
            .or_else(|| {
                frozen
                    .provider_request_id
                    .as_ref()
                    .map(|id| format!("provider:{id}"))
            })
            .ok_or_else(|| {
                failure(
                    "SOLANA_SUBMISSION_IDENTITY_UNAVAILABLE",
                    "dispatch",
                    "No stable chain signature or provider request identity is available.",
                    "Prepare a fresh transaction with an immutable provider request id.",
                )
            })
    }
    async fn send_signed(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
    ) -> Result<String>;
}

fn canonical_message_reference(message_sha256: &str) -> String {
    format!("solana:message:{message_sha256}")
}

#[async_trait]
pub trait AsyncDispatchGuard: Send + Sync {
    async fn check(&self) -> Result<()>;
}

impl SolanaRuntime {
    pub fn new(rpc: Arc<dyn SolanaRpc>) -> Self {
        Self { rpc }
    }
    pub fn rpc(&self) -> &Arc<dyn SolanaRpc> {
        &self.rpc
    }

    /// Derive the exact static required signer identities from provider bytes.
    pub fn required_signers(encoded: &str) -> Result<Vec<String>> {
        let transaction = decode_supported_transaction(encoded)?;
        let required = usize::from(transaction.message.header().num_required_signatures);
        let keys = transaction.message.static_account_keys();
        if required == 0 || keys.len() < required || transaction.signatures.len() != required {
            return Err(invalid_transaction());
        }
        Ok(keys[..required].iter().map(ToString::to_string).collect())
    }

    /// Inspect zero signature slots without trusting provider-supplied signer
    /// labels. Applications may allow only their reviewed provider cosigner.
    pub fn missing_signers(encoded: &str) -> Result<Vec<String>> {
        let transaction = decode_supported_transaction(encoded)?;
        let required = usize::from(transaction.message.header().num_required_signatures);
        let keys = transaction.message.static_account_keys();
        if transaction.signatures.len() != required || keys.len() < required {
            return Err(invalid_transaction());
        }
        Ok(transaction
            .signatures
            .iter()
            .take(required)
            .enumerate()
            .filter(|(_, sig)| sig.as_ref().iter().all(|b| *b == 0))
            .map(|(index, _)| keys[index].to_string())
            .collect())
    }

    /// Resolve Token-2022 transfer-hook metas from the mint's on-chain
    /// ExtraAccountMetaList. Only read-only, non-signer extras are accepted;
    /// hooks requiring additional mutation authority are rejected explicitly.
    pub async fn resolve_transfer_hook_instruction(
        &self,
        instruction: &mut NormalizedInstruction,
        hook_program: &str,
        amount_atoms: u64,
    ) -> Result<()> {
        use solana_instruction3::AccountMeta as AccountMeta3;
        use solana_pubkey3::Pubkey;
        use spl_tlv_account_resolution::state::ExtraAccountMetaList;
        use spl_transfer_hook_interface::{
            get_extra_account_metas_address, instruction::ExecuteInstruction,
        };
        use spl_type_length_value::state::TlvStateBorrowed;
        let hook: Address = hook_program.parse().map_err(|_| invalid_transaction())?;
        if instruction.accounts.len() < 4 || instruction.accounts.len() > 32 {
            return Err(invalid_transaction());
        }
        let mut old_instruction = solana_instruction3::Instruction {
            program_id: Pubkey::new_from_array(hook.to_bytes()),
            accounts: instruction
                .accounts
                .iter()
                .map(|meta| {
                    let key = meta
                        .address
                        .parse::<Address>()
                        .map_err(|_| invalid_transaction())?;
                    Ok(if meta.writable {
                        AccountMeta3::new(Pubkey::new_from_array(key.to_bytes()), meta.signer)
                    } else {
                        AccountMeta3::new_readonly(
                            Pubkey::new_from_array(key.to_bytes()),
                            meta.signer,
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            data: STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_transaction())?,
        };
        let original = old_instruction.accounts.clone();
        let source = original[0].pubkey;
        let mint = original[1].pubkey;
        let destination = original[2].pubkey;
        let authority = original[3].pubkey;
        let validation_address = get_extra_account_metas_address(&mint, &hook);
        let validation_response = self
            .rpc
            .request(
                "getAccountInfo",
                json!([validation_address.to_string(),{"encoding":"base64","commitment":"finalized"}]),
            )
            .await
            .map_err(|_| {
                failure(
                    "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED",
                    "prepare",
                    "Token-2022 transfer-hook metadata could not be read from finalized state.",
                    "Refresh the token accounts and hook metadata, then prepare a new transfer.",
                )
            })?;
        let validation_account = validation_response
            .get("value")
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                failure(
                    "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED",
                    "prepare",
                    "The finalized transfer-hook metadata account is missing.",
                    "Refresh the token accounts and hook metadata, then prepare a new transfer.",
                )
            })?;
        let validation_owner = validation_account
            .get("owner")
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<Address>().ok());
        let validation_data = decode_rpc_account_data(
            validation_account
                .get("data")
                .ok_or_else(invalid_transaction)?,
        )?;
        if validation_owner != Some(hook) {
            return Err(failure(
                "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED",
                "prepare",
                "The finalized transfer-hook metadata account has an unexpected owner.",
                "Refresh the token accounts and hook metadata, then prepare a new transfer.",
            ));
        }
        let tlv_state = TlvStateBorrowed::unpack(&validation_data).map_err(|_| {
            failure(
                "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED",
                "prepare",
                "The finalized transfer-hook metadata list is invalid.",
                "Refresh the token accounts and hook metadata, then prepare a new transfer.",
            )
        })?;
        let extra_metas = ExtraAccountMetaList::unpack_with_tlv_state::<ExecuteInstruction>(
            &tlv_state,
        )
        .map_err(|_| {
            failure(
                "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED",
                "prepare",
                "The finalized transfer-hook metadata list is invalid.",
                "Refresh the token accounts and hook metadata, then prepare a new transfer.",
            )
        })?;
        if extra_metas.len() > 16
            || extra_metas
                .iter()
                .any(|meta| bool::from(meta.is_signer) || bool::from(meta.is_writable))
        {
            return Err(failure(
                "SOLANA_TRANSFER_HOOK_PERMISSION_UNSUPPORTED",
                "prepare",
                "This transfer hook requires writable or signer privileges on additional accounts.",
                "Use a hook configuration limited to read-only non-signer accounts.",
            ));
        }
        let rpc = self.rpc.clone();
        let validation_data = Arc::new(validation_data);
        let resolve = spl_transfer_hook_interface::offchain::add_extra_account_metas_for_execute(
            &mut old_instruction,
            &Pubkey::new_from_array(hook.to_bytes()),
            &source,
            &mint,
            &destination,
            &authority,
            amount_atoms,
            move |address| {
                let rpc = rpc.clone();
                let validation_data = validation_data.clone();
                async move {
                    if address == validation_address {
                        return Ok(Some(validation_data.as_ref().clone()));
                    }
                    let response = rpc.request("getAccountInfo", json!([address.to_string(),{"encoding":"base64","commitment":"finalized"}])).await.map_err(|_| Box::new(std::io::Error::other("account data unavailable")) as spl_transfer_hook_interface::offchain::AccountFetchError)?;
                    let Some(account) = response.get("value").filter(|v| !v.is_null()) else { return Ok(None); };
                    let _owner = account.get("owner").and_then(Value::as_str).and_then(|raw| raw.parse::<Address>().ok()).ok_or_else(|| Box::new(std::io::Error::other("invalid account owner")) as spl_transfer_hook_interface::offchain::AccountFetchError)?;
                    let bytes = decode_rpc_account_data(account.get("data").ok_or_else(|| Box::new(std::io::Error::other("missing account data")) as spl_transfer_hook_interface::offchain::AccountFetchError)?).map_err(|_| Box::new(std::io::Error::other("invalid account data")) as spl_transfer_hook_interface::offchain::AccountFetchError)?;
                    Ok(Some(bytes))
                }
            },
        ).await;
        resolve.map_err(|_| failure("SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED", "prepare", "Token-2022 transfer-hook accounts could not be resolved from finalized mainnet state.", "Refresh the token accounts and hook metadata, then prepare a new transfer."))?;
        if old_instruction.accounts.len() > original.len() + 16
            || old_instruction.accounts[..original.len()] != original
        {
            return Err(failure(
                "SOLANA_TRANSFER_HOOK_METAS_INVALID",
                "prepare",
                "The transfer hook produced an invalid account-meta list.",
                "Use a token with a reviewed transfer-hook account configuration.",
            ));
        }
        for meta in &old_instruction.accounts[original.len()..] {
            if meta.is_signer || meta.is_writable {
                return Err(failure(
                    "SOLANA_TRANSFER_HOOK_PERMISSION_UNSUPPORTED",
                    "prepare",
                    "This transfer hook requires writable or signer privileges on additional accounts.",
                    "Use a hook configuration limited to read-only non-signer accounts.",
                ));
            }
        }
        instruction.accounts = old_instruction
            .accounts
            .iter()
            .map(|meta| NormalizedAccountMeta {
                address: Address::new_from_array(meta.pubkey.to_bytes()).to_string(),
                signer: meta.is_signer,
                writable: meta.is_writable,
            })
            .collect();
        Ok(())
    }

    /// Compile app-validated raw instructions. Lookup tables are fetched and
    /// validated from finalized RPC before they are used in the message.
    #[allow(clippy::too_many_arguments)]
    pub async fn freeze_instructions(
        &self,
        instructions: &[NormalizedInstruction],
        lookup_tables: &[String],
        blockhash: &str,
        signer: &SolanaSigner,
        last_valid_block_height: u64,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
    ) -> Result<FrozenTransaction> {
        self.freeze_instructions_with_expiry(
            instructions,
            lookup_tables,
            blockhash,
            signer,
            Some(last_valid_block_height),
            None,
            maximum_network_fee_lamports,
            provider_request_id,
            allowed_missing_cosigners,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn freeze_instructions_with_expiry(
        &self,
        instructions: &[NormalizedInstruction],
        lookup_tables: &[String],
        blockhash: &str,
        signer: &SolanaSigner,
        last_valid_block_height: Option<u64>,
        provider_expire_at: Option<&str>,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
    ) -> Result<FrozenTransaction> {
        if instructions.is_empty() || instructions.len() > 64 || lookup_tables.len() > 8 {
            return Err(invalid_transaction_reason(
                "compile",
                "instruction_limits_exceeded",
            ));
        }
        let hash = blockhash
            .parse::<Hash>()
            .map_err(|_| invalid_transaction_reason("compile", "invalid_blockhash"))?;
        let payer = signer
            .address()
            .parse::<Address>()
            .map_err(|_| invalid_transaction_reason("compile", "invalid_payer"))?;
        let mut compiled = Vec::with_capacity(instructions.len());
        let mut unique_keys = std::collections::HashSet::new();
        unique_keys.insert(payer);
        let mut account_count = 0usize;
        for instruction in instructions {
            let program_id = instruction
                .program_id
                .parse::<Address>()
                .map_err(|_| invalid_transaction_reason("compile", "invalid_program_id"))?;
            if !unique_keys.insert(program_id) { /* repeated program ids are expected */ }
            let data = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_transaction_reason("compile", "invalid_instruction_base64"))?;
            if data.len() > 1024 {
                return Err(invalid_transaction_reason(
                    "compile",
                    "instruction_data_too_large",
                ));
            }
            let mut accounts = Vec::with_capacity(instruction.accounts.len());
            for account in &instruction.accounts {
                let address = account.address.parse::<Address>().map_err(|_| {
                    invalid_transaction_reason("compile", "invalid_account_address")
                })?;
                unique_keys.insert(address);
                account_count += 1;
                if account_count > 240 {
                    return Err(invalid_transaction_reason(
                        "compile",
                        "account_meta_limit_exceeded",
                    ));
                }
                let mut meta = if account.writable {
                    AccountMeta::new(address, account.signer)
                } else {
                    AccountMeta::new_readonly(address, account.signer)
                };
                meta.is_writable = account.writable;
                accounts.push(meta);
            }
            compiled.push(Instruction {
                program_id,
                accounts,
                data,
            });
        }
        if unique_keys.len() > 256 {
            return Err(invalid_transaction_reason(
                "compile",
                "account_key_limit_exceeded",
            ));
        }
        let tables = self.load_lookup_tables(lookup_tables).await?;
        let message = if tables.is_empty() {
            VersionedMessage::Legacy(solana_message::Message::new_with_blockhash(
                &compiled,
                Some(&payer),
                &hash,
            ))
        } else {
            let v0_tables = tables
                .iter()
                .map(
                    |(key, addresses)| solana_message::AddressLookupTableAccount {
                        key: *key,
                        addresses: addresses.clone(),
                    },
                )
                .collect::<Vec<_>>();
            VersionedMessage::V0(
                solana_message::v0::Message::try_compile(&payer, &compiled, &v0_tables, hash)
                    .map_err(|_| invalid_transaction_reason("compile", "message_compile_failed"))?,
            )
        };
        message
            .sanitize()
            .map_err(|_| invalid_transaction_reason("compile", "message_sanitize_failed"))?;
        let required = usize::from(message.header().num_required_signatures);
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); required],
            message,
        };
        // Determine the wire size before the bounded serializer rejects it so
        // adapters can request a smaller route without accepting invalid bytes.
        let wire_size = codec()
            .with_no_limit()
            .serialized_size(&transaction)
            .map_err(|_| invalid_transaction_reason("serialize", "serialization_failed"))?;
        if wire_size > MAX_TRANSACTION_BYTES as u64 {
            return Err(transaction_size_error("compile", wire_size));
        }
        let raw = codec()
            .serialize(&transaction)
            .map_err(|_| invalid_transaction_reason("serialize", "serialization_failed"))?;
        self.freeze_transaction_with_expiry(
            &STANDARD.encode(raw),
            signer,
            last_valid_block_height,
            provider_expire_at,
            maximum_network_fee_lamports,
            provider_request_id,
            allowed_missing_cosigners,
        )
        .await
    }

    /// Decode provider-built legacy/v0 transaction bytes, verify every existing
    /// co-signature, pin all static and lookup-table identities, and freeze a
    /// single wallet's signing slot. Application code must inspect and validate
    /// `instructions()` before asking the engine to authorize submission.
    pub async fn freeze_transaction(
        &self,
        encoded: &str,
        signer: &SolanaSigner,
        last_valid_block_height: u64,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
    ) -> Result<FrozenTransaction> {
        self.freeze_transaction_with_expiry(
            encoded,
            signer,
            Some(last_valid_block_height),
            None,
            maximum_network_fee_lamports,
            provider_request_id,
            allowed_missing_cosigners,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn freeze_transaction_with_expiry(
        &self,
        encoded: &str,
        signer: &SolanaSigner,
        last_valid_block_height: Option<u64>,
        provider_expire_at: Option<&str>,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
    ) -> Result<FrozenTransaction> {
        let transaction = decode_supported_transaction(encoded)?;
        let message_bytes = codec()
            .serialize(&transaction.message)
            .map_err(|_| invalid_transaction())?;
        let message_sha256 = hex::encode(Sha256::digest(&message_bytes));
        let header = transaction.message.header();
        let required = usize::from(header.num_required_signatures);
        if transaction.signatures.len() != required {
            return Err(invalid_transaction());
        }
        let keys = transaction.message.static_account_keys();
        if required == 0 || keys.len() < required {
            return Err(invalid_transaction());
        }
        let signer_index = keys[..required]
            .iter()
            .position(|key| key.to_string() == signer.address())
            .ok_or_else(|| {
                failure(
                    "SOLANA_SIGNER_NOT_REQUIRED",
                    "prepare",
                    "The wallet is not a required signer for this transaction.",
                    "Request a transaction bound to the enrolled Solana wallet.",
                )
            })?;
        let fee_payer = keys.first().ok_or_else(invalid_transaction)?.to_string();
        let required_signers = keys[..required]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mut allowed = allowed_missing_cosigners.to_vec();
        allowed.sort();
        allowed.dedup();
        if allowed.len() != allowed_missing_cosigners.len() {
            return Err(failure(
                "SOLANA_COSIGNER_SCOPE_INVALID",
                "prepare",
                "The approved co-signer set repeats a signer identity.",
                "Review the exact external signer identities and request a fresh provider transaction.",
            ));
        }
        if allowed
            .iter()
            .any(|address| !required_signers.contains(address))
        {
            return Err(failure(
                "SOLANA_COSIGNER_SCOPE_INVALID",
                "prepare",
                "The approved co-signer set contains an address that is not a required signer.",
                "Review the exact external signer identities and request a fresh provider transaction.",
            ));
        }
        if allowed.iter().any(|address| address == signer.address()) {
            return Err(failure(
                "SOLANA_COSIGNER_SCOPE_INVALID",
                "prepare",
                "The wallet signer cannot be listed as an external co-signer.",
                "Review the exact external signer identities and request a fresh provider transaction.",
            ));
        }
        let mut missing_cosigners = Vec::new();
        if signer_index >= transaction.signatures.len()
            || !transaction.signatures[signer_index]
                .as_ref()
                .iter()
                .all(|b| *b == 0)
        {
            return Err(failure(
                "SOLANA_SIGNER_SLOT_NOT_EMPTY",
                "prepare",
                "The enrolled wallet signature slot is already populated.",
                "Request an unsigned provider transaction and create a fresh reviewed operation.",
            ));
        }
        for (index, key) in keys.iter().take(required).enumerate() {
            if index == signer_index {
                continue;
            }
            let signature = &transaction.signatures[index];
            if signature.as_ref().iter().all(|b| *b == 0) {
                let address = key.to_string();
                if allowed.binary_search(&address).is_ok() {
                    missing_cosigners.push(address);
                    continue;
                }
                return Err(failure(
                    "SOLANA_COSIGNATURE_INVALID",
                    "prepare",
                    "A required transaction co-signature is missing or invalid.",
                    "Obtain all required external signatures for this exact transaction before approval.",
                ));
            }
            if !signature.verify(key.as_ref(), &message_bytes) {
                return Err(failure(
                    "SOLANA_COSIGNATURE_INVALID",
                    "prepare",
                    "A required transaction co-signature is missing or invalid.",
                    "Obtain all required external signatures for this exact transaction before approval.",
                ));
            }
        }
        if allowed
            .iter()
            .any(|address| !missing_cosigners.contains(address))
        {
            return Err(failure(
                "SOLANA_COSIGNER_SCOPE_INVALID",
                "prepare",
                "An approved external signer already has a valid signature or is not a required signer.",
                "Review the exact external signer identities and request a fresh provider transaction.",
            ));
        }
        let blockhash = transaction.message.recent_blockhash().to_string();
        if let Some(expire_at) = provider_expire_at {
            let expiry = chrono::DateTime::parse_from_rfc3339(expire_at).map_err(|_| {
                failure("SOLANA_EXPIRY_INVALID", "prepare", "The provider expiry is not a valid RFC3339 timestamp.", "Use the original provider expiry value or omit it when the provider supplied none.")
            })?;
            if expiry <= chrono::Utc::now() {
                return Err(failure(
                    "SOLANA_TX_EXPIRED",
                    "prepare",
                    "The provider transaction has expired.",
                    "Request a fresh provider transaction and prepare a new operation.",
                ));
            }
        }
        let instructions = self.resolve_instructions(&transaction.message).await?;
        let frozen = FrozenTransaction {
            transaction,
            message_bytes,
            message_sha256,
            signer: signer.address().to_owned(),
            fee_payer,
            signer_index,
            required_signers,
            missing_cosigners,
            recent_blockhash: blockhash,
            last_valid_block_height,
            provider_expire_at: provider_expire_at.map(str::to_owned),
            maximum_network_fee_lamports,
            provider_request_id,
            instructions,
        };
        self.assert_lifetime_valid(&frozen).await?;
        Ok(frozen)
    }

    /// Recheck the original transaction lifetime without changing its blockhash.
    /// Call after any provider-side queue/rate wait and immediately before HTTP send.
    pub async fn assert_lifetime_valid(&self, frozen: &FrozenTransaction) -> Result<()> {
        if let Some(expire_at) = frozen.provider_expire_at.as_deref() {
            let expiry = chrono::DateTime::parse_from_rfc3339(expire_at)
                .map_err(|_| invalid_transaction())?;
            if expiry <= chrono::Utc::now() {
                return Err(failure(
                    "SOLANA_TX_EXPIRED",
                    "dispatch",
                    "The original provider transaction expired before dispatch.",
                    "Do not resend it; request and approve a fresh transaction.",
                ));
            }
        }
        if let Some(last_valid) = frozen.last_valid_block_height {
            let height = self
                .rpc
                .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
                .await?
                .as_u64()
                .ok_or_else(parse_error)?;
            if height > last_valid {
                return Err(failure(
                    "SOLANA_BLOCKHASH_EXPIRED",
                    "dispatch",
                    "The frozen transaction blockhash has expired; no transaction was sent.",
                    "Prepare and approve a new operation; do not replay this transaction.",
                ));
            }
        } else {
            let response = self
                .rpc
                .request(
                    "isBlockhashValid",
                    json!([frozen.recent_blockhash,{"commitment":"confirmed"}]),
                )
                .await?;
            if response.get("value").and_then(Value::as_bool) != Some(true) {
                return Err(failure(
                    "SOLANA_BLOCKHASH_EXPIRED",
                    "dispatch",
                    "The original transaction blockhash is no longer valid; no transaction was sent.",
                    "Request and approve a fresh provider transaction.",
                ));
            }
        }
        Ok(())
    }

    pub async fn validate_fee(&self, frozen: &FrozenTransaction) -> Result<u64> {
        let message = STANDARD.encode(&frozen.message_bytes);
        let result = self
            .rpc
            .request(
                "getFeeForMessage",
                json!([message,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let fee = result
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(parse_error)?;
        if fee > frozen.maximum_network_fee_lamports {
            return Err(failure(
                "SOLANA_FEE_LIMIT_EXCEEDED",
                "prepare",
                "The network fee exceeds the approved maximum.",
                "Review a new transaction with a sufficient explicit fee limit.",
            ));
        }
        Ok(fee)
    }

    pub async fn simulate(&self, frozen: &FrozenTransaction) -> Result<SimulationResult> {
        let wire = codec()
            .serialize(&frozen.transaction)
            .map_err(|_| invalid_transaction())?;
        let result = self.rpc.request("simulateTransaction", json!([STANDARD.encode(wire),{"encoding":"base64","sigVerify":false,"replaceRecentBlockhash":false,"commitment":"confirmed"}])).await?;
        let value = result.get("value").ok_or_else(parse_error)?;
        let err = simulation_error_value(value);
        let units_consumed = value.get("unitsConsumed").and_then(Value::as_u64);
        if let Some(err) = err {
            return Err(simulation_failed(err));
        }
        if units_consumed.is_some_and(|units| units > 1_400_000) {
            return Err(failure(
                "SOLANA_COMPUTE_LIMIT_EXCEEDED",
                "prepare",
                "The transaction exceeds Solana's maximum compute-unit limit.",
                "Use a reviewed transaction within the network compute limit.",
            ));
        }
        Ok(SimulationResult { units_consumed })
    }

    /// Simulate the immutable transaction and return the post-simulation state
    /// for a bounded, exact set of accounts. This is read-only: it never signs,
    /// persists, or submits the transaction.
    pub async fn simulate_with_accounts(
        &self,
        frozen: &FrozenTransaction,
        account_addresses: &[String],
    ) -> Result<SimulationWithAccountsResult> {
        const MAX_SIMULATION_ACCOUNTS: usize = 100;
        if account_addresses.is_empty() || account_addresses.len() > MAX_SIMULATION_ACCOUNTS {
            return Err(GoFinError::validation(
                "Simulation must request between 1 and 100 exact account addresses.",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for address in account_addresses {
            crate::parse_address(address)?;
            if !seen.insert(address) {
                return Err(GoFinError::validation(
                    "Simulation account addresses must be unique.",
                ));
            }
        }
        let wire = codec()
            .serialize(&frozen.transaction)
            .map_err(|_| invalid_transaction())?;
        let result = self
            .rpc
            .request(
                "simulateTransaction",
                json!([
                    STANDARD.encode(wire),
                    {
                        "encoding":"base64",
                        "sigVerify":false,
                        "replaceRecentBlockhash":false,
                        "commitment":"confirmed",
                        "accounts":{"encoding":"base64","addresses":account_addresses}
                    }
                ]),
            )
            .await?;
        let value = result.get("value").ok_or_else(parse_error)?;
        if let Some(err) = simulation_error_value(value) {
            return Err(simulation_failed(err));
        }
        let units_consumed = value.get("unitsConsumed").and_then(Value::as_u64);
        if units_consumed.is_some_and(|units| units > 1_400_000) {
            return Err(failure(
                "SOLANA_COMPUTE_LIMIT_EXCEEDED",
                "prepare",
                "The transaction exceeds Solana's maximum compute-unit limit.",
                "Use a reviewed transaction within the network compute limit.",
            ));
        }
        let returned = value
            .get("accounts")
            .and_then(Value::as_array)
            .filter(|accounts| accounts.len() == account_addresses.len())
            .ok_or_else(parse_error)?;
        let accounts = account_addresses
            .iter()
            .zip(returned)
            .map(|(address, account)| {
                if account.is_null() {
                    return Ok(None);
                }
                let owner = account
                    .get("owner")
                    .and_then(Value::as_str)
                    .filter(|owner| crate::parse_address(owner).is_ok())
                    .ok_or_else(parse_error)?
                    .to_owned();
                let lamports = account
                    .get("lamports")
                    .and_then(Value::as_u64)
                    .ok_or_else(parse_error)?;
                let data = crate::token::decode_rpc_account_data(
                    account.get("data").ok_or_else(parse_error)?,
                )?;
                let executable = account
                    .get("executable")
                    .and_then(Value::as_bool)
                    .ok_or_else(parse_error)?;
                let rent_epoch = account
                    .get("rentEpoch")
                    .map(|epoch| epoch.as_u64().ok_or_else(parse_error))
                    .transpose()?;
                Ok(Some(SimulatedAccountInfo {
                    address: address.clone(),
                    owner,
                    lamports,
                    data,
                    executable,
                    rent_epoch,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SimulationWithAccountsResult {
            units_consumed,
            accounts,
        })
    }

    /// Persist the immutable submission identity before a one-shot network
    /// dispatch. A timeout is returned as an uncertain RPC error; callers must
    /// reconcile this same reference and must never refresh its blockhash.
    pub async fn dispatch(
        &self,
        frozen: &FrozenTransaction,
        signer: &SolanaSigner,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        if !frozen.missing_cosigners.is_empty() {
            return Err(failure(
                "SOLANA_COSIGNER_LANDING_REQUIRED",
                "dispatch",
                "This transaction needs an external co-signer and cannot use direct RPC submission.",
                "Use its reviewed provider landing path and reconcile the original request identity.",
            ));
        }
        let sender = RpcTransactionSender {
            rpc: self.rpc.clone(),
        };
        self.dispatch_with_sender(frozen, signer, prepared, journal, &sender)
            .await
    }

    pub async fn dispatch_with_sender(
        &self,
        frozen: &FrozenTransaction,
        signer: &SolanaSigner,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
        sender: &dyn TransactionSender,
    ) -> Result<Submission> {
        journal.assert_candidate(prepared)?;
        if signer.address() != frozen.signer {
            return Err(failure(
                "SOLANA_SIGNER_BINDING_MISMATCH",
                "dispatch",
                "The supplied signer does not match the frozen transaction.",
                "Keep the original operation identity and use its enrolled signer.",
            ));
        }
        self.assert_lifetime_valid(frozen).await?;
        let mut transaction = frozen.transaction.clone();
        let signature = frozen.signer_index;
        let signed = signer.sign_message(&frozen.message_bytes)?;
        transaction.signatures[signature] = Signature::from(signed);
        let wire = codec()
            .serialize(&transaction)
            .map_err(|_| invalid_transaction())?;
        if wire.len() > MAX_TRANSACTION_BYTES {
            return Err(invalid_transaction());
        }
        let first_signature = transaction
            .signatures
            .first()
            .filter(|sig| !sig.as_ref().iter().all(|b| *b == 0))
            .map(ToString::to_string);
        // Let provider senders validate their own immutable request identity,
        // but key the durable submission by the signed message itself. This
        // deduplicates the same Solana transaction across RPC, Jupiter Meta,
        // and other sponsored landing paths even if provider request IDs differ.
        let _provider_reference =
            sender.submission_reference(frozen, first_signature.as_deref())?;
        let reference = canonical_message_reference(frozen.message_sha256());
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: reference.clone(),
            data: json!({"schema_version":1,"message_sha256":frozen.message_sha256,"recent_blockhash":frozen.recent_blockhash,"last_valid_block_height":frozen.last_valid_block_height,"lifetime_kind":if frozen.last_valid_block_height.is_some(){"block_height"}else{"blockhash_validity"},"provider_expire_at":frozen.provider_expire_at,"provider_request_id":frozen.provider_request_id,"chain_signature":first_signature,"wallet_signature":transaction.signatures[signature].to_string(),"signer":frozen.signer,"fee_payer":frozen.fee_payer,"required_signers":frozen.required_signers,"missing_cosigners":frozen.missing_cosigners,"existing_signatures":transaction.signatures.iter().enumerate().map(|(i,sig)|json!({"signer":frozen.required_signers[i],"signature":if sig.as_ref().iter().all(|b|*b==0){Value::Null}else{json!(sig.to_string())}})).collect::<Vec<_>>()}),
        };
        journal.persist_submission(&submission)?;
        let signed_wire = STANDARD.encode(wire);
        let guard_denied = AtomicBool::new(false);
        let guard = RuntimeDispatchGuard {
            runtime: self,
            frozen,
            journal: journal.as_ref(),
            prepared,
            denied: &guard_denied,
        };
        if let Err(error) = guard.check().await {
            let reason_code = error
                .details
                .as_deref()
                .and_then(|details| details.get("reason_code"))
                .and_then(Value::as_str)
                .unwrap_or("PRE_SEND_GUARD_DENIED");
            journal.persist_dispatch_evidence(
                &submission,
                &json!({"no_network_send":true,"reason_code":reason_code,"message_sha256":frozen.message_sha256}),
            )?;
            return Ok(submission);
        }
        let result = sender
            .send_signed_with_guard(&signed_wire, frozen, &guard)
            .await;
        match result {
            Ok(value) if first_signature.as_deref() == Some(value.as_str()) => {
                journal.persist_dispatch_evidence(
                    &submission,
                    &json!({"chain_signature":value,"message_sha256":frozen.message_sha256}),
                )?;
                Ok(submission)
            }
            Ok(value) if first_signature.is_none() && !value.is_empty() => {
                journal.persist_dispatch_evidence(
                    &submission,
                    &json!({"chain_signature":value,"message_sha256":frozen.message_sha256}),
                )?;
                Ok(submission)
            }
            Ok(_) => Err(failure(
                "SOLANA_SUBMISSION_UNCERTAIN",
                "dispatch",
                "The landing service response did not match the persisted Solana transaction identity.",
                "Reconcile the original transaction reference; do not submit a replacement.",
            )),
            Err(error) if guard_denied.load(Ordering::Acquire) => {
                let reason_code = error
                    .details
                    .as_deref()
                    .and_then(|details| details.get("reason_code"))
                    .and_then(Value::as_str)
                    .unwrap_or("PRE_SEND_GUARD_DENIED");
                journal.persist_dispatch_evidence(
                    &submission,
                    &json!({"no_network_send":true,"reason_code":reason_code,"message_sha256":frozen.message_sha256}),
                )?;
                Ok(submission)
            }
            Err(_) => Err(failure(
                "SOLANA_SUBMISSION_UNCERTAIN",
                "dispatch",
                "The RPC outcome is uncertain after the transaction identity was persisted.",
                "Reconcile the original transaction reference; do not submit a replacement.",
            )),
        }
    }

    pub async fn reconcile(
        &self,
        signature: &str,
        expected_message_sha256: &str,
    ) -> Result<ReceiptOutcome> {
        self.reconcile_inner(signature, Some(expected_message_sha256))
            .await
    }

    /// Read and verify finalized transaction evidence when the caller knows
    /// the canonical signature but the message was created by an external
    /// keeper/provider. Application code must still bind the instruction and
    /// event semantics to its own request identity.
    pub async fn observe_finalized_transaction(&self, signature: &str) -> Result<ReceiptOutcome> {
        match self.reconcile_inner(signature, None).await {
            Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => Ok(ReceiptOutcome::Pending {
                confirmed: false,
                reason: "The receipt RPC is temporarily unavailable; keep the original transaction unresolved and reconcile it again later.".into(),
            }),
            result => result,
        }
    }

    /// Convenience for application observers that only consume finalized
    /// keeper/provider transactions. Pending evidence cannot be mistaken for
    /// a proof.
    pub async fn observe_finalized_proof(&self, signature: &str) -> Result<FinalizedReceiptProof> {
        match self.observe_finalized_transaction(signature).await? {
            ReceiptOutcome::Finalized { proof, .. } => Ok(*proof),
            ReceiptOutcome::Pending { .. } | ReceiptOutcome::NotDispatched { .. } => {
                Err(pending_evidence_error())
            }
        }
    }

    async fn reconcile_inner(
        &self,
        signature: &str,
        expected_message_sha256: Option<&str>,
    ) -> Result<ReceiptOutcome> {
        let status = self
            .rpc
            .request(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
            )
            .await?;
        let entry = status
            .get("value")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        if entry.is_null() {
            return Ok(ReceiptOutcome::Pending {
                confirmed: false,
                reason: "The transaction is not present in available signature history.".into(),
            });
        }
        let confirmation = entry
            .get("confirmationStatus")
            .and_then(Value::as_str)
            .unwrap_or("");
        let confirmed = matches!(confirmation, "confirmed" | "finalized");
        if confirmation != "finalized" {
            return Ok(ReceiptOutcome::Pending {
                confirmed,
                reason: "The transaction has not reached finalized commitment.".into(),
            });
        }
        let result = self.rpc.request("getTransaction", json!([signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}])).await?;
        if result.is_null() {
            return Ok(ReceiptOutcome::Pending {
                confirmed: true,
                reason: "Finalized signature status has no transaction details yet.".into(),
            });
        }
        let tx_data = result
            .get("transaction")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .ok_or_else(parse_error)?;
        if tx_data.len() > MAX_TRANSACTION_BYTES.saturating_mul(2) {
            return Err(parse_error());
        }
        let wire = STANDARD.decode(tx_data).map_err(|_| parse_error())?;
        if wire.len() > MAX_TRANSACTION_BYTES {
            return Err(parse_error());
        }
        let tx: VersionedTransaction = codec().deserialize(&wire).map_err(|_| parse_error())?;
        tx.sanitize().map_err(|_| parse_error())?;
        if matches!(tx.message, VersionedMessage::V1(_)) {
            return Err(parse_error());
        }
        let hash = hex::encode(Sha256::digest(
            codec().serialize(&tx.message).map_err(|_| parse_error())?,
        ));
        if expected_message_sha256.is_some_and(|expected| hash != expected)
            || tx.signatures.first().map(ToString::to_string).as_deref() != Some(signature)
            || !verify_signatures(&tx)
        {
            return Err(receipt_identity_error());
        }
        let slot = result
            .get("slot")
            .and_then(Value::as_u64)
            .ok_or_else(parse_error)?;
        let meta = result
            .get("meta")
            .filter(|v| !v.is_null())
            .ok_or_else(pending_evidence_error)?;
        let fee_lamports = meta
            .get("fee")
            .and_then(Value::as_u64)
            .ok_or_else(parse_error)?;
        let error = meta
            .get("err")
            .cloned()
            .ok_or_else(pending_evidence_error)?;
        let status_error = entry
            .get("err")
            .cloned()
            .ok_or_else(pending_evidence_error)?;
        if error != status_error {
            return Err(receipt_identity_error());
        }
        let pre_balances_lamports =
            numbers(meta.get("preBalances").ok_or_else(pending_evidence_error)?)?;
        let post_balances_lamports = numbers(
            meta.get("postBalances")
                .ok_or_else(pending_evidence_error)?,
        )?;
        let static_keys = tx
            .message
            .static_account_keys()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let loaded_addresses = meta
            .get("loadedAddresses")
            .cloned()
            .unwrap_or_else(|| json!({"writable":[],"readonly":[]}));
        if !matches!(tx.message, VersionedMessage::Legacy(_)) {
            self.verify_finalized_loaded_addresses(&tx.message, &loaded_addresses)
                .await?;
        }
        let mut account_keys = static_keys;
        for kind in ["writable", "readonly"] {
            let addresses = loaded_addresses
                .get(kind)
                .and_then(Value::as_array)
                .ok_or_else(pending_evidence_error)?;
            if addresses.iter().any(|v| v.as_str().is_none()) {
                return Err(parse_error());
            }
            account_keys.extend(
                addresses
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        if account_keys.len() != pre_balances_lamports.len()
            || account_keys.len() != post_balances_lamports.len()
        {
            return Err(pending_evidence_error());
        }
        let failed = !error.is_null();
        let message_instructions = decoded_message_instructions(
            &tx.message,
            &account_keys,
            loaded_addresses
                .get("writable")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
        )?;
        let proof = FinalizedReceiptProof {
            signature: signature.to_owned(),
            slot,
            fee_lamports,
            failed,
            message_sha256: hash.clone(),
            account_keys,
            message_instructions,
            required_signatures: tx.signatures.iter().map(ToString::to_string).collect(),
            pre_balances_lamports,
            post_balances_lamports,
            pre_token_balances: token_balance_array(meta.get("preTokenBalances"))?,
            post_token_balances: token_balance_array(meta.get("postTokenBalances"))?,
            loaded_addresses,
            inner_instructions: value_array(meta.get("innerInstructions"))?,
            log_messages: string_array(meta.get("logMessages"))?,
            error: (!error.is_null()).then_some(error),
        };
        Ok(ReceiptOutcome::Finalized {
            signature: signature.into(),
            slot,
            fee_lamports,
            failed,
            message_sha256: hash,
            proof: Box::new(proof),
        })
    }

    /// Reconcile against the exact frozen request. `wallet_signature` is read
    /// from the durable submission; for sponsored transactions it binds the
    /// wallet's signed intent even when the provider chose the first signature.
    pub async fn reconcile_bound(
        &self,
        signature: &str,
        frozen: &FrozenTransaction,
        wallet_signature: &str,
    ) -> Result<ReceiptOutcome> {
        let outcome = self.reconcile(signature, &frozen.message_sha256).await?;
        if let ReceiptOutcome::Finalized { .. } = &outcome {
            let result = self.rpc.request("getTransaction", json!([signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}])).await?;
            let tx_data = result
                .get("transaction")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .ok_or_else(pending_evidence_error)?;
            if tx_data.len() > MAX_TRANSACTION_BYTES.saturating_mul(2) {
                return Err(parse_error());
            }
            let raw = STANDARD.decode(tx_data).map_err(|_| parse_error())?;
            let tx: VersionedTransaction = codec().deserialize(&raw).map_err(|_| parse_error())?;
            let wallet_sig: Signature = wallet_signature
                .parse()
                .map_err(|_| receipt_identity_error())?;
            if tx.signatures.get(frozen.signer_index) != Some(&wallet_sig)
                || frozen.required_signers.len() != tx.signatures.len()
            {
                return Err(receipt_identity_error());
            }
            for (idx, sig) in tx.signatures.iter().enumerate() {
                let old = &frozen.transaction.signatures[idx];
                if idx != frozen.signer_index && old != &Signature::default() && old != sig {
                    return Err(receipt_identity_error());
                }
            }
        }
        Ok(outcome)
    }

    /// Restart-safe reconciliation using only the immutable durable submission
    /// plus dispatch evidence. It never needs to refresh or reconstruct a
    /// transaction from a new blockhash.
    pub async fn reconcile_submission(
        &self,
        submission: &Submission,
        dispatch_evidence: Option<&Value>,
    ) -> Result<ReceiptOutcome> {
        if submission.kind != "solana.transaction" {
            return Err(receipt_identity_error());
        }
        let data = &submission.data;
        let expected_hash = data
            .get("message_sha256")
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        if submission.reference.starts_with("solana:message:")
            && submission.reference != canonical_message_reference(expected_hash)
        {
            return Err(receipt_identity_error());
        }
        let wallet_signature = data
            .get("wallet_signature")
            .and_then(Value::as_str)
            .ok_or_else(pending_evidence_error)?;
        if data.get("schema_version").and_then(Value::as_u64) != Some(1)
            || data
                .get("recent_blockhash")
                .and_then(Value::as_str)
                .is_none()
            || !(data
                .get("last_valid_block_height")
                .and_then(Value::as_u64)
                .is_some()
                || (data
                    .get("last_valid_block_height")
                    .is_some_and(Value::is_null)
                    && data.get("lifetime_kind").and_then(Value::as_str)
                        == Some("blockhash_validity")))
        {
            return Err(pending_evidence_error());
        }
        let expected_wallet = data
            .get("signer")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(pending_evidence_error)?;
        let required_signers = data
            .get("required_signers")
            .and_then(Value::as_array)
            .ok_or_else(pending_evidence_error)?;
        let known_signatures = data
            .get("existing_signatures")
            .and_then(Value::as_array)
            .ok_or_else(pending_evidence_error)?;
        let missing_cosigners = data
            .get("missing_cosigners")
            .and_then(Value::as_array)
            .ok_or_else(pending_evidence_error)?;
        let chain_signature = dispatch_evidence
            .and_then(|evidence| evidence.get("chain_signature"))
            .and_then(Value::as_str)
            .or_else(|| data.get("chain_signature").and_then(Value::as_str))
            .or_else(|| {
                if submission.reference.starts_with("provider:")
                    || submission.reference.starts_with("jupiter:")
                    || submission.reference.starts_with("solana:message:")
                {
                    None
                } else {
                    Some(submission.reference.as_str())
                }
            });
        if let Some(evidence) = dispatch_evidence
            && evidence
                .get("message_sha256")
                .and_then(Value::as_str)
                .is_some_and(|hash| hash != expected_hash)
        {
            return Err(receipt_identity_error());
        }
        let wallet_index = required_signers
            .iter()
            .position(|address| address.as_str() == Some(expected_wallet))
            .ok_or_else(pending_evidence_error)?;
        if let Some(evidence) = dispatch_evidence.filter(|evidence| {
            evidence.get("no_network_send").and_then(Value::as_bool) == Some(true)
        }) {
            if evidence.get("message_sha256").and_then(Value::as_str) != Some(expected_hash)
                || evidence
                    .get("reason_code")
                    .and_then(Value::as_str)
                    .is_none()
                || required_signers.is_empty()
                || required_signers.len() != known_signatures.len()
                || required_signers.len() > 256
                || wallet_index >= required_signers.len()
            {
                return Err(receipt_identity_error());
            }
            let mut identities = std::collections::HashSet::new();
            for (index, (required, known)) in
                required_signers.iter().zip(known_signatures).enumerate()
            {
                let signer = required.as_str().ok_or_else(receipt_identity_error)?;
                if !identities.insert(signer)
                    || known.get("signer").and_then(Value::as_str) != Some(signer)
                {
                    return Err(receipt_identity_error());
                }
                let known_signature = known.get("signature").and_then(Value::as_str);
                if index == wallet_index {
                    if known_signature != Some(wallet_signature) {
                        return Err(receipt_identity_error());
                    }
                } else if let Some(value) = known_signature {
                    let signature: Signature =
                        value.parse().map_err(|_| receipt_identity_error())?;
                    if signature.as_ref().iter().all(|byte| *byte == 0) {
                        return Err(receipt_identity_error());
                    }
                }
            }
            return Ok(ReceiptOutcome::NotDispatched {
                reason_code: evidence["reason_code"].as_str().unwrap_or("PRE_SEND_GUARD_DENIED").to_owned(),
                reason: "The original transaction was durably recorded, then its authorization/lifetime guard rejected before network send.".into(),
            });
        }
        let (chain_signature, outcome) = if let Some(signature) = chain_signature {
            let outcome = match self.reconcile(signature, expected_hash).await {
                Ok(outcome) => outcome,
                Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "The receipt RPC is temporarily unavailable; this submission remains unresolved.".into() }),
                Err(error) => return Err(error),
            };
            (signature.to_owned(), outcome)
        } else {
            // Sponsored providers may return no first signature when execute
            // times out. Search a bounded finalized wallet history and accept
            // only a transaction that independently matches the frozen hash
            // and this durable wallet signature.
            let history = match self
                .rpc
                .request(
                    "getSignaturesForAddress",
                    json!([expected_wallet,{"commitment":"finalized","limit":100}]),
                )
                .await {
                Ok(value) => value,
                Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "The receipt RPC is temporarily unavailable; this submission remains unresolved.".into() }),
                Err(error) => return Err(error),
            };
            let candidates = history.as_array().ok_or_else(pending_evidence_error)?;
            let mut match_found = None;
            for candidate in candidates.iter().take(100) {
                let Some(signature) = candidate.get("signature").and_then(Value::as_str) else {
                    continue;
                };
                if signature.len() > 128 {
                    continue;
                }
                let candidate_outcome = match self.reconcile(signature, expected_hash).await {
                    Ok(outcome) => outcome,
                    Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "Receipt-history verification is temporarily unavailable; this submission remains unresolved.".into() }),
                    Err(_) => continue,
                };
                let ReceiptOutcome::Finalized { proof, .. } = &candidate_outcome else {
                    continue;
                };
                if proof
                    .required_signatures
                    .get(wallet_index)
                    .map(String::as_str)
                    == Some(wallet_signature)
                {
                    match_found = Some((signature.to_owned(), candidate_outcome));
                    break;
                }
            }
            match match_found {
                Some(found) => found,
                None => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "No finalized transaction matching the durable message hash and wallet signature appeared in the bounded history window.".into() }),
            }
        };
        let chain_signature = chain_signature.as_str();
        if !matches!(outcome, ReceiptOutcome::Finalized { .. }) {
            return Ok(outcome);
        }
        let ReceiptOutcome::Finalized { proof, .. } = &outcome else {
            unreachable!()
        };
        let required = required_signers.len();
        let keys = &proof.account_keys;
        if required == 0 || required != known_signatures.len() || keys.len() < required {
            return Err(receipt_identity_error());
        }
        if proof.required_signatures.len() != required
            || proof.required_signatures.first().map(String::as_str) != Some(chain_signature)
        {
            return Err(receipt_identity_error());
        }
        let wallet_indices = required_signers
            .iter()
            .enumerate()
            .filter(|(_, address)| address.as_str() == Some(expected_wallet))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if wallet_indices.len() != 1 {
            return Err(receipt_identity_error());
        }
        let wallet_index = wallet_indices[0];
        for (index, key) in keys.iter().take(required).enumerate() {
            if required_signers[index].as_str() != Some(key.as_str())
                || known_signatures[index]
                    .get("signer")
                    .and_then(Value::as_str)
                    != required_signers[index].as_str()
            {
                return Err(receipt_identity_error());
            }
            let signer = required_signers[index]
                .as_str()
                .ok_or_else(pending_evidence_error)?;
            let actual = proof
                .required_signatures
                .get(index)
                .ok_or_else(pending_evidence_error)?;
            if index == wallet_index {
                if actual != wallet_signature {
                    return Err(receipt_identity_error());
                }
            } else if let Some(expected) = known_signatures[index]
                .get("signature")
                .and_then(Value::as_str)
            {
                if actual != expected {
                    return Err(receipt_identity_error());
                }
            } else if known_signatures[index]
                .get("signature")
                .is_some_and(Value::is_null)
            {
                if !missing_cosigners.iter().any(|v| v.as_str() == Some(signer)) {
                    return Err(receipt_identity_error());
                }
            } else {
                return Err(receipt_identity_error());
            }
        }
        let durable_missing = missing_cosigners
            .iter()
            .map(Value::as_str)
            .collect::<Option<std::collections::HashSet<_>>>()
            .ok_or_else(pending_evidence_error)?;
        let actual_missing = known_signatures
            .iter()
            .enumerate()
            .filter(|(idx, item)| {
                *idx != wallet_index && item.get("signature").is_some_and(Value::is_null)
            })
            .filter_map(|(idx, _)| required_signers[idx].as_str())
            .collect::<std::collections::HashSet<_>>();
        if durable_missing != actual_missing {
            return Err(receipt_identity_error());
        }
        Ok(outcome)
    }

    async fn resolve_instructions(
        &self,
        message: &VersionedMessage,
    ) -> Result<Vec<NormalizedInstruction>> {
        let static_keys = message.static_account_keys();
        let mut full_keys = static_keys
            .iter()
            .map(|key| key.to_string())
            .collect::<Vec<_>>();
        let mut loaded_writable = Vec::new();
        let mut loaded_readonly = Vec::new();
        if let Some(lookups) = message.address_table_lookups() {
            let slot = self
                .rpc
                .request("getSlot", json!([{"commitment":"finalized"}]))
                .await?
                .as_u64()
                .ok_or_else(parse_error)?;
            for lookup in lookups {
                let address = lookup.account_key.to_string();
                let response = self
                    .rpc
                    .request(
                        "getAccountInfo",
                        json!([address,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let value = response.get("value").filter(|v| !v.is_null()).ok_or_else(|| failure("SOLANA_LOOKUP_TABLE_MISSING", "prepare", "A required version-0 address lookup table is unavailable.", "Refresh the provider transaction and verify its lookup-table accounts."))?;
                if value.get("owner").and_then(Value::as_str) != Some(ALT_PROGRAM_ID) {
                    return Err(failure(
                        "SOLANA_LOOKUP_TABLE_OWNER_INVALID",
                        "prepare",
                        "A required lookup table has an unexpected program owner.",
                        "Use a valid Solana address lookup table.",
                    ));
                }
                let data = decode_rpc_account_data(value.get("data").ok_or_else(parse_error)?)?;
                let table =
                    solana_address_lookup_table_interface::state::AddressLookupTable::deserialize(
                        &data,
                    )
                    .map_err(|_| parse_error())?;
                if table.meta.deactivation_slot != u64::MAX || slot <= table.meta.last_extended_slot
                {
                    return Err(failure(
                        "SOLANA_LOOKUP_TABLE_INACTIVE",
                        "prepare",
                        "A required lookup table is inactive or has not reached a finalized slot.",
                        "Request a fresh transaction using active, finalized lookup tables.",
                    ));
                }
                for index in &lookup.writable_indexes {
                    loaded_writable.push(
                        table
                            .addresses
                            .get(usize::from(*index))
                            .ok_or_else(invalid_lookup_index)?
                            .to_bytes(),
                    );
                }
                for index in &lookup.readonly_indexes {
                    loaded_readonly.push(
                        table
                            .addresses
                            .get(usize::from(*index))
                            .ok_or_else(invalid_lookup_index)?
                            .to_bytes(),
                    );
                }
            }
        }
        full_keys.extend(
            loaded_writable
                .iter()
                .map(|bytes| solana_address::Address::new_from_array(*bytes).to_string()),
        );
        full_keys.extend(
            loaded_readonly
                .iter()
                .map(|bytes| solana_address::Address::new_from_array(*bytes).to_string()),
        );
        let header = message.header();
        let required = usize::from(header.num_required_signatures);
        let readonly_signed = usize::from(header.num_readonly_signed_accounts);
        let readonly_unsigned = usize::from(header.num_readonly_unsigned_accounts);
        let static_len = static_keys.len();
        let mut instructions = Vec::new();
        for ix in message.instructions() {
            let program_id = full_keys
                .get(usize::from(ix.program_id_index))
                .ok_or_else(invalid_transaction)?
                .clone();
            let mut accounts = Vec::new();
            for index in &ix.accounts {
                let index = usize::from(*index);
                let address = full_keys
                    .get(index)
                    .ok_or_else(invalid_transaction)?
                    .clone();
                let signer = index < required;
                let writable = if index < static_len {
                    if signer {
                        index < required.saturating_sub(readonly_signed)
                    } else {
                        index < static_len.saturating_sub(readonly_unsigned)
                    }
                } else {
                    index < static_len + loaded_writable.len()
                };
                accounts.push(NormalizedAccountMeta {
                    address,
                    signer,
                    writable,
                });
            }
            instructions.push(NormalizedInstruction {
                program_id,
                accounts,
                data_base64: STANDARD.encode(&ix.data),
            });
        }
        Ok(instructions)
    }

    async fn verify_finalized_loaded_addresses(
        &self,
        message: &VersionedMessage,
        loaded_addresses: &Value,
    ) -> Result<()> {
        let Some(lookups) = message.address_table_lookups() else {
            return if loaded_addresses
                .get("writable")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
                && loaded_addresses
                    .get("readonly")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
            {
                Ok(())
            } else {
                Err(receipt_identity_error())
            };
        };
        let mut expected_writable = Vec::new();
        let mut expected_readonly = Vec::new();
        for lookup in lookups {
            let address = lookup.account_key.to_string();
            let response = self
                .rpc
                .request(
                    "getAccountInfo",
                    json!([address,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let value = response
                .get("value")
                .filter(|value| !value.is_null())
                .ok_or_else(pending_evidence_error)?;
            if value.get("owner").and_then(Value::as_str) != Some(ALT_PROGRAM_ID) {
                return Err(receipt_identity_error());
            }
            let data = decode_rpc_account_data(value.get("data").ok_or_else(parse_error)?)?;
            let table =
                solana_address_lookup_table_interface::state::AddressLookupTable::deserialize(
                    &data,
                )
                .map_err(|_| receipt_identity_error())?;
            for index in &lookup.writable_indexes {
                expected_writable.push(
                    table
                        .addresses
                        .get(usize::from(*index))
                        .ok_or_else(receipt_identity_error)?
                        .to_string(),
                );
            }
            for index in &lookup.readonly_indexes {
                expected_readonly.push(
                    table
                        .addresses
                        .get(usize::from(*index))
                        .ok_or_else(receipt_identity_error)?
                        .to_string(),
                );
            }
        }
        let actual_writable = loaded_addresses
            .get("writable")
            .and_then(Value::as_array)
            .ok_or_else(pending_evidence_error)?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(parse_error))
            .collect::<Result<Vec<_>>>()?;
        let actual_readonly = loaded_addresses
            .get("readonly")
            .and_then(Value::as_array)
            .ok_or_else(pending_evidence_error)?
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(parse_error))
            .collect::<Result<Vec<_>>>()?;
        if actual_writable != expected_writable || actual_readonly != expected_readonly {
            return Err(receipt_identity_error());
        }
        Ok(())
    }

    async fn load_lookup_tables(
        &self,
        table_addresses: &[String],
    ) -> Result<Vec<(Address, Vec<Address>)>> {
        if table_addresses.len() > 8 {
            return Err(invalid_transaction());
        }
        let slot = self
            .rpc
            .request("getSlot", json!([{"commitment":"finalized"}]))
            .await?
            .as_u64()
            .ok_or_else(parse_error)?;
        let mut seen = std::collections::HashSet::new();
        let mut tables = Vec::with_capacity(table_addresses.len());
        for raw_address in table_addresses {
            let address = raw_address
                .parse::<Address>()
                .map_err(|_| invalid_transaction())?;
            if !seen.insert(address) {
                return Err(invalid_transaction());
            }
            let response = self
                .rpc
                .request(
                    "getAccountInfo",
                    json!([raw_address,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let value = response
                .get("value")
                .filter(|v| !v.is_null())
                .ok_or_else(|| {
                    failure(
                        "SOLANA_LOOKUP_TABLE_MISSING",
                        "prepare",
                        "A required version-0 address lookup table is unavailable.",
                        "Refresh the provider transaction and verify its lookup-table accounts.",
                    )
                })?;
            if value.get("owner").and_then(Value::as_str) != Some(ALT_PROGRAM_ID) {
                return Err(failure(
                    "SOLANA_LOOKUP_TABLE_OWNER_INVALID",
                    "prepare",
                    "A required lookup table has an unexpected program owner.",
                    "Use a valid Solana address lookup table.",
                ));
            }
            let data = decode_rpc_account_data(value.get("data").ok_or_else(parse_error)?)?;
            let table =
                solana_address_lookup_table_interface::state::AddressLookupTable::deserialize(
                    &data,
                )
                .map_err(|_| parse_error())?;
            if table.meta.deactivation_slot != u64::MAX || slot <= table.meta.last_extended_slot {
                return Err(failure(
                    "SOLANA_LOOKUP_TABLE_INACTIVE",
                    "prepare",
                    "A required lookup table is inactive or was not active before the finalized slot.",
                    "Request a fresh transaction using active, finalized lookup tables.",
                ));
            }
            tables.push((
                address,
                table
                    .addresses
                    .iter()
                    .map(|key| Address::new_from_array(key.to_bytes()))
                    .collect(),
            ));
        }
        Ok(tables)
    }
}

/// Convert simulation errors into a small, non-sensitive diagnostic. Solana's
/// error value is provider-controlled, so never copy its string, logs, or
/// arbitrary nested fields into the public error.
fn simulation_error_value(value: &Value) -> Option<&Value> {
    value.get("err").filter(|err| !err.is_null())
}

fn simulation_failed(error: &Value) -> gofin_domain::GoFinError {
    let (category, instruction_index, custom_code) = if let Some(text) = error.as_str() {
        let category = match text {
            "BlockhashNotFound" => "blockhash_not_found",
            "AccountNotFound" => "account_not_found",
            "InsufficientFundsForFee" => "insufficient_funds_for_fee",
            "InvalidAccountForFee" => "invalid_account_for_fee",
            "AlreadyProcessed" => "already_processed",
            _ => "unknown",
        };
        (category, None, None)
    } else if let Some(parts) = error
        .get("InstructionError")
        .and_then(Value::as_array)
        .filter(|parts| parts.len() == 2)
    {
        let index = parts[0].as_u64().filter(|index| *index <= u32::MAX as u64);
        let custom = parts[1]
            .get("Custom")
            .and_then(Value::as_u64)
            .filter(|code| *code <= u32::MAX as u64);
        let category = if custom.is_some() {
            "instruction_custom"
        } else {
            "instruction_error"
        };
        (category, index, custom)
    } else {
        ("unknown", None, None)
    };

    let mut details = serde_json::Map::new();
    details.insert("simulation_error_kind".into(), json!(category));
    if let Some(index) = instruction_index {
        details.insert("instruction_index".into(), json!(index));
    }
    if let Some(code) = custom_code {
        details.insert("custom_error_code".into(), json!(code));
    }
    failure(
        "SOLANA_SIMULATION_FAILED",
        "prepare",
        "The transaction simulation failed on mainnet-beta.",
        "Refresh the operation and review the returned transaction before approving it.",
    )
    .with_details(Value::Object(details))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationResult {
    pub units_consumed: Option<u64>,
}

/// One account returned by Solana's `simulateTransaction` account override.
/// `None` in [`SimulationWithAccountsResult::accounts`] means the account was
/// absent or closed by the simulated transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulatedAccountInfo {
    pub address: String,
    pub owner: String,
    pub lamports: u64,
    pub data: Vec<u8>,
    pub executable: bool,
    pub rent_epoch: Option<u64>,
}

/// Simulation output for an exact, caller-selected set of account addresses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SimulationWithAccountsResult {
    pub units_consumed: Option<u64>,
    /// Same order as the requested address list; null entries are preserved.
    pub accounts: Vec<Option<SimulatedAccountInfo>>,
}

pub(crate) fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(MAX_TRANSACTION_BYTES as u64)
}

fn verify_signatures(transaction: &VersionedTransaction) -> bool {
    let required = usize::from(transaction.message.header().num_required_signatures);
    let keys = transaction.message.static_account_keys();
    if required == 0 || required != transaction.signatures.len() || keys.len() < required {
        return false;
    }
    let Ok(message) = codec().serialize(&transaction.message) else {
        return false;
    };
    transaction
        .signatures
        .iter()
        .take(required)
        .enumerate()
        .all(|(index, signature)| {
            !signature.as_ref().iter().all(|byte| *byte == 0)
                && signature.verify(keys[index].as_ref(), &message)
        })
}

fn decoded_message_instructions(
    message: &VersionedMessage,
    resolved_keys: &[String],
    loaded_writable_count: usize,
) -> Result<Vec<NormalizedInstruction>> {
    if matches!(message, VersionedMessage::V1(_)) {
        return Err(invalid_transaction());
    }
    let static_len = message.static_account_keys().len();
    let header = message.header();
    let required = usize::from(header.num_required_signatures);
    let readonly_signed = usize::from(header.num_readonly_signed_accounts);
    let readonly_unsigned = usize::from(header.num_readonly_unsigned_accounts);
    let writable_signed_end = required
        .checked_sub(readonly_signed)
        .ok_or_else(invalid_transaction)?;
    let writable_unsigned_end = static_len
        .checked_sub(readonly_unsigned)
        .ok_or_else(invalid_transaction)?;
    let mut output = Vec::new();
    for compiled in message.instructions() {
        if output.len() >= 512 {
            return Err(invalid_transaction());
        }
        let program_index = usize::from(compiled.program_id_index);
        let program_id = resolved_keys
            .get(program_index)
            .ok_or_else(invalid_lookup_index)?
            .clone();
        let mut accounts = Vec::with_capacity(compiled.accounts.len());
        for raw_index in &compiled.accounts {
            let index = usize::from(*raw_index);
            let address = resolved_keys
                .get(index)
                .ok_or_else(invalid_lookup_index)?
                .clone();
            let signer = index < required;
            let writable = if index < required {
                index < writable_signed_end
            } else if index < static_len {
                index < writable_unsigned_end
            } else {
                index < static_len + loaded_writable_count
            };
            accounts.push(NormalizedAccountMeta {
                address,
                signer,
                writable,
            });
        }
        output.push(NormalizedInstruction {
            program_id,
            accounts,
            data_base64: STANDARD.encode(&compiled.data),
        });
    }
    Ok(output)
}

fn numbers(value: &Value) -> Result<Vec<u64>> {
    value
        .as_array()
        .filter(|values| values.len() <= 256)
        .ok_or_else(pending_evidence_error)?
        .iter()
        .map(|value| value.as_u64().ok_or_else(pending_evidence_error))
        .collect()
}

fn value_array(value: Option<&Value>) -> Result<Vec<Value>> {
    value
        .and_then(Value::as_array)
        .filter(|values| values.len() <= 256)
        .cloned()
        .ok_or_else(pending_evidence_error)
}

/// Keep authoritative token-account identity and integer atom amounts from
/// finalized RPC metadata while dropping floating/string UI display fields.
/// Older serialized proofs remain deserializable; normalization is performed
/// only when crossing the trusted RPC response boundary.
fn token_balance_array(value: Option<&Value>) -> Result<Vec<Value>> {
    let mut balances = value_array(value)?;
    for row in &mut balances {
        if let Some(ui_amount) = row.get_mut("uiTokenAmount").and_then(Value::as_object_mut) {
            ui_amount.remove("uiAmount");
            ui_amount.remove("uiAmountString");
        }
    }
    Ok(balances)
}

fn string_array(value: Option<&Value>) -> Result<Vec<String>> {
    value
        .and_then(Value::as_array)
        .filter(|values| values.len() <= 10_000)
        .ok_or_else(pending_evidence_error)?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(pending_evidence_error)
        })
        .collect()
}

fn receipt_identity_error() -> GoFinError {
    failure(
        "SOLANA_RECEIPT_IDENTITY_MISMATCH",
        "reconciliation",
        "The finalized transaction does not match the persisted message and signer identities.",
        "Keep the submission unresolved and inspect the original transaction record.",
    )
}
fn pending_evidence_error() -> GoFinError {
    failure(
        "SOLANA_RECEIPT_EVIDENCE_INCOMPLETE",
        "reconciliation",
        "Finalized transaction details are missing required verification evidence.",
        "Keep the original submission unresolved and retry reconciliation later.",
    )
}

struct RpcTransactionSender {
    rpc: Arc<dyn SolanaRpc>,
}
#[async_trait]
impl TransactionSender for RpcTransactionSender {
    async fn send_signed(
        &self,
        signed_transaction_base64: &str,
        _frozen: &FrozenTransaction,
    ) -> Result<String> {
        let value = self.rpc.request("sendTransaction", json!([signed_transaction_base64,{"encoding":"base64","preflightCommitment":"confirmed","maxRetries":0}])).await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(crate::parse_error)
    }
}
/// Decode every externally supplied wire transaction through one bounded path.
/// Diagnostics contain only static categories and lengths, never bytes or keys.
fn decode_supported_transaction(encoded: &str) -> Result<VersionedTransaction> {
    if encoded.len() > MAX_TRANSACTION_BYTES.saturating_mul(2) {
        return Err(invalid_transaction_reason(
            "decode",
            "encoded_size_limit_exceeded",
        ));
    }
    let raw = STANDARD
        .decode(encoded)
        .map_err(|_| invalid_transaction_reason("decode", "invalid_base64"))?;
    if raw.is_empty() {
        return Err(invalid_transaction_reason("decode", "empty_transaction"));
    }
    if raw.len() > MAX_TRANSACTION_BYTES {
        return Err(transaction_size_error("decode", raw.len() as u64));
    }
    let transaction: VersionedTransaction = codec()
        .deserialize(&raw)
        .map_err(|_| invalid_transaction_reason("decode", "wire_deserialization_failed"))?;
    if matches!(transaction.message, VersionedMessage::V1(_)) {
        return Err(failure(
            "SOLANA_TX_VERSION_UNSUPPORTED",
            "prepare",
            "This Solana transaction version is not supported.",
            "Request a legacy or version-0 transaction.",
        )
        .with_details(json!({"transaction_stage":"decode","transaction_version":1})));
    }
    transaction
        .sanitize()
        .map_err(|_| invalid_transaction_reason("sanitize", "transaction_sanitize_failed"))?;
    Ok(transaction)
}

fn invalid_transaction_reason(stage: &str, reason: &str) -> GoFinError {
    invalid_transaction().with_details(json!({"transaction_stage":stage,"reason":reason}))
}

fn transaction_size_error(stage: &str, wire_size: u64) -> GoFinError {
    GoFinError::new(
        "SOLANA_TRANSACTION_INVALID",
        "prepare",
        format!("The Solana transaction is {wire_size} bytes and exceeds the {MAX_TRANSACTION_BYTES}-byte network limit."),
        "Request a smaller supported route with fewer accounts and review its decoded instructions.",
    ).with_details(json!({
        "transaction_stage":stage,
        "reason":"wire_size_limit_exceeded",
        "wire_size_bytes":wire_size,
        "maximum_wire_size_bytes":MAX_TRANSACTION_BYTES,
    }))
}

fn invalid_transaction() -> GoFinError {
    failure(
        "SOLANA_TRANSACTION_INVALID",
        "prepare",
        "The provider transaction is malformed or outside supported legacy/version-0 format.",
        "Request a fresh supported transaction and review its decoded instructions.",
    )
}
fn invalid_lookup_index() -> GoFinError {
    failure(
        "SOLANA_LOOKUP_INDEX_INVALID",
        "prepare",
        "A version-0 transaction references an address missing from its lookup table.",
        "Request a fresh transaction with valid lookup-table indices.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_provider_transaction_entrypoints_reject_v1_explicitly() {
        let signer = signer();
        let payer = signer.address().parse::<Address>().unwrap();
        let message = solana_message::v1::Message {
            header: solana_message::MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            account_keys: vec![payer],
            ..Default::default()
        };
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V1(message),
        };
        let wire = STANDARD.encode(codec().serialize(&transaction).unwrap());
        for error in [
            SolanaRuntime::required_signers(&wire).unwrap_err(),
            SolanaRuntime::missing_signers(&wire).unwrap_err(),
        ] {
            assert_eq!(error.code, "SOLANA_TX_VERSION_UNSUPPORTED");
            assert_eq!(
                error.details.as_deref().unwrap(),
                &json!({
                    "transaction_stage":"decode","transaction_version":1
                })
            );
        }
    }

    #[tokio::test]
    async fn transaction_format_diagnostics_distinguish_decode_sanitize_and_wire_size() {
        for (wire, reason) in [
            ("private-not-base64", "invalid_base64"),
            ("", "empty_transaction"),
            ("AQ==", "wire_deserialization_failed"),
        ] {
            for error in [
                SolanaRuntime::required_signers(wire).unwrap_err(),
                SolanaRuntime::missing_signers(wire).unwrap_err(),
            ] {
                assert_eq!(error.code, "SOLANA_TRANSACTION_INVALID");
                assert_eq!(
                    error.details.as_deref().unwrap(),
                    &json!({
                        "transaction_stage":"decode", "reason":reason
                    })
                );
                assert!(
                    !serde_json::to_string(&error)
                        .unwrap()
                        .contains("private-not-base64")
                );
            }
        }
        let signer = signer();
        let (mut transaction, _) = signed_wire(&signer);
        transaction.signatures.clear();
        let wire = STANDARD.encode(codec().serialize(&transaction).unwrap());
        let error = SolanaRuntime::required_signers(&wire).unwrap_err();
        assert_eq!(
            error.details.as_deref().unwrap(),
            &json!({
                "transaction_stage":"sanitize", "reason":"transaction_sanitize_failed"
            })
        );

        let wire = STANDARD.encode(vec![0; MAX_TRANSACTION_BYTES + 1]);
        let error = SolanaRuntime::missing_signers(&wire).unwrap_err();
        assert_eq!(
            error.details.as_deref().unwrap(),
            &json!({
                "transaction_stage":"decode", "reason":"wire_size_limit_exceeded",
                "wire_size_bytes":1233, "maximum_wire_size_bytes":1232
            })
        );

        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!(100), // finalized slot for the empty lookup-table set
        ])))));
        let instruction = NormalizedInstruction {
            program_id: Address::new_from_array([99; 32]).to_string(),
            accounts: (1..=10)
                .map(|index| NormalizedAccountMeta {
                    address: Address::new_from_array([index; 32]).to_string(),
                    signer: false,
                    writable: false,
                })
                .collect(),
            data_base64: STANDARD.encode(vec![0; 1024]),
        };
        let error = runtime
            .freeze_instructions(
                &[instruction],
                &[],
                &Hash::default().to_string(),
                &signer,
                1000,
                5000,
                None,
                &[],
            )
            .await
            .unwrap_err();
        let details = error.details.as_deref().unwrap();
        assert_eq!(error.code, "SOLANA_TRANSACTION_INVALID");
        assert_eq!(details["transaction_stage"], "compile");
        assert_eq!(details["reason"], "wire_size_limit_exceeded");
        assert!(details["wire_size_bytes"].as_u64().unwrap() > 1232);
        assert_eq!(details["maximum_wire_size_bytes"], 1232);
    }

    #[test]
    fn simulation_error_details_are_bounded_and_redacted() {
        let custom = simulation_failed(&json!({"InstructionError":[3,{"Custom":6001}]}));
        assert_eq!(custom.code, "SOLANA_SIMULATION_FAILED");
        assert_eq!(
            custom.details.as_deref().unwrap(),
            &json!({
                "simulation_error_kind":"instruction_custom",
                "instruction_index":3,
                "custom_error_code":6001
            })
        );

        let blockhash = simulation_failed(&json!("BlockhashNotFound"));
        assert_eq!(
            blockhash.details.as_deref().unwrap(),
            &json!({"simulation_error_kind":"blockhash_not_found"})
        );

        let unknown = simulation_failed(&json!({
            "arbitrary":"wallet-secret-like text",
            "logs":["private account mapping"],
            "nested":{"Custom":6001}
        }));
        assert_eq!(
            unknown.details.as_deref().unwrap(),
            &json!({"simulation_error_kind":"unknown"})
        );
        assert!(!unknown.to_string().contains("wallet-secret-like text"));
        assert!(!unknown.to_string().contains("private account mapping"));
    }

    #[test]
    fn absent_simulation_error_is_not_a_failure() {
        assert!(simulation_error_value(&json!({"err":null})).is_none());
        assert!(simulation_error_value(&json!({})).is_none());
    }

    struct TransferHookRpc {
        owner: String,
        data: Vec<u8>,
    }

    #[async_trait]
    impl SolanaRpc for TransferHookRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            assert_eq!(method, "getAccountInfo");
            Ok(json!({
                "value": {
                    "owner": self.owner,
                    "data": [STANDARD.encode(&self.data), "base64"]
                }
            }))
        }
    }

    fn transfer_hook_runtime(
        extra: spl_tlv_account_resolution::account::ExtraAccountMeta,
        hook: &solana_pubkey3::Pubkey,
    ) -> SolanaRuntime {
        use spl_tlv_account_resolution::state::ExtraAccountMetaList;
        let mut data = vec![0; ExtraAccountMetaList::size_of(1).unwrap()];
        ExtraAccountMetaList::init::<spl_transfer_hook_interface::instruction::ExecuteInstruction>(
            &mut data,
            &[extra],
        )
        .unwrap();
        SolanaRuntime::new(Arc::new(TransferHookRpc {
            owner: hook.to_string(),
            data,
        }))
    }

    fn transfer_hook_instruction(
        hook: &solana_pubkey3::Pubkey,
    ) -> (NormalizedInstruction, [solana_pubkey3::Pubkey; 3]) {
        use solana_pubkey3::Pubkey;
        let source = Pubkey::new_from_array([1; 32]);
        let mint = Pubkey::new_from_array([2; 32]);
        let destination = Pubkey::new_from_array([3; 32]);
        let authority = Pubkey::new_from_array([4; 32]);
        (
            NormalizedInstruction {
                program_id: hook.to_string(),
                accounts: [source, mint, destination, authority]
                    .into_iter()
                    .map(|address| NormalizedAccountMeta {
                        address: address.to_string(),
                        signer: address == authority,
                        writable: address != mint && address != authority,
                    })
                    .collect(),
                data_base64: STANDARD.encode([0; 8]),
            },
            [source, mint, destination],
        )
    }

    #[tokio::test]
    async fn transfer_hook_resolves_seeded_readonly_metas_and_rejects_privilege_escalation() {
        use solana_pubkey3::Pubkey;
        use spl_tlv_account_resolution::{account::ExtraAccountMeta, seeds::Seed};
        use spl_transfer_hook_interface::get_extra_account_metas_address;

        let hook = Pubkey::new_from_array([9; 32]);
        let seeded =
            ExtraAccountMeta::new_with_seeds(&[Seed::AccountKey { index: 0 }], false, false)
                .unwrap();
        let runtime = transfer_hook_runtime(seeded, &hook);
        let (mut instruction, [source, mint, _destination]) = transfer_hook_instruction(&hook);
        let expected_pda = Pubkey::find_program_address(&[source.as_ref()], &hook).0;
        let original_len = instruction.accounts.len();
        runtime
            .resolve_transfer_hook_instruction(&mut instruction, &hook.to_string(), 123)
            .await
            .unwrap();
        assert_eq!(
            instruction.accounts[original_len].address,
            expected_pda.to_string()
        );
        assert!(!instruction.accounts[original_len].signer);
        assert!(!instruction.accounts[original_len].writable);
        assert_eq!(
            instruction.accounts[original_len + 1].address,
            hook.to_string()
        );
        assert_eq!(
            instruction.accounts[original_len + 2].address,
            get_extra_account_metas_address(&mint, &hook).to_string()
        );
        assert!(
            instruction.accounts[original_len..]
                .iter()
                .all(|meta| !meta.signer && !meta.writable)
        );

        for (signer, writable) in [(false, true), (true, false)] {
            let elevated = ExtraAccountMeta::new_with_pubkey(
                &Pubkey::new_from_array([7; 32]),
                signer,
                writable,
            )
            .unwrap();
            let runtime = transfer_hook_runtime(elevated, &hook);
            let (mut instruction, _) = transfer_hook_instruction(&hook);
            let error = runtime
                .resolve_transfer_hook_instruction(&mut instruction, &hook.to_string(), 123)
                .await
                .unwrap_err();
            assert_eq!(error.code, "SOLANA_TRANSFER_HOOK_PERMISSION_UNSUPPORTED");
        }

        let readonly =
            ExtraAccountMeta::new_with_pubkey(&Pubkey::new_from_array([8; 32]), false, false)
                .unwrap();
        let mut invalid_owner_data =
            vec![0; spl_tlv_account_resolution::state::ExtraAccountMetaList::size_of(1).unwrap()];
        spl_tlv_account_resolution::state::ExtraAccountMetaList::init::<
            spl_transfer_hook_interface::instruction::ExecuteInstruction,
        >(&mut invalid_owner_data, &[readonly])
        .unwrap();
        let runtime = SolanaRuntime::new(Arc::new(TransferHookRpc {
            owner: Pubkey::new_from_array([6; 32]).to_string(),
            data: invalid_owner_data,
        }));
        let (mut instruction, _) = transfer_hook_instruction(&hook);
        let error = runtime
            .resolve_transfer_hook_instruction(&mut instruction, &hook.to_string(), 123)
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_TRANSFER_HOOK_RESOLUTION_FAILED");
    }

    #[test]
    fn canonical_submission_identity_is_message_bound_across_provider_routes() {
        let hash = "ab".repeat(32);
        let first = canonical_message_reference(&hash);
        let second = canonical_message_reference(&hash);
        assert_eq!(first, second);
        assert_eq!(first, format!("solana:message:{hash}"));
    }

    #[test]
    fn exact_token_account_delta_does_not_aggregate_owner_accounts() {
        let keys = vec!["source".to_owned(), "other".to_owned()];
        let balances = vec![
            json!({"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"10","decimals":6}}),
            json!({"accountIndex":1,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"900","decimals":6}}),
        ];
        assert_eq!(
            balance_for_account(&balances, &keys, "source", "wallet", "mint", 6)
                .unwrap()
                .as_str(),
            "10"
        );
        assert!(balance_for_account(&balances, &keys, "source", "other-owner", "mint", 6).is_err());
        assert!(
            balance_for_account(&balances, &keys, "source", "wallet", "other-mint", 6).is_err()
        );
        assert!(balance_for_account(&balances, &keys, "source", "wallet", "mint", 9).is_err());
    }

    #[test]
    fn aggregate_token_delta_requires_exact_program_decimals_and_stable_identity() {
        let mut proof = FinalizedReceiptProof {
            signature: "signature".into(),
            slot: 1,
            fee_lamports: 0,
            failed: false,
            message_sha256: "00".repeat(32),
            account_keys: vec!["one".into(), "two".into()],
            message_instructions: Vec::new(),
            required_signatures: Vec::new(),
            pre_balances_lamports: vec![0, 0],
            post_balances_lamports: vec![0, 0],
            pre_token_balances: vec![
                json!({"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100","decimals":6}}),
                json!({"accountIndex":1,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"50","decimals":6}}),
            ],
            post_token_balances: vec![
                json!({"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"75","decimals":6}}),
                json!({"accountIndex":1,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100","decimals":6}}),
            ],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: Vec::new(),
            log_messages: Vec::new(),
            error: None,
        };
        assert_eq!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .unwrap()
                .as_str(),
            "25"
        );
        assert_eq!(
            proof
                .token_account_delta_verified(
                    "one",
                    "wallet",
                    "mint",
                    crate::token::TOKEN_PROGRAM_ID,
                    6,
                )
                .unwrap()
                .as_str(),
            "-25"
        );
        assert!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_2022_PROGRAM_ID, 6)
                .is_err()
        );
        assert!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 9)
                .is_err()
        );
        assert!(
            proof
                .token_account_delta_verified(
                    "one",
                    "wallet",
                    "mint",
                    crate::token::TOKEN_2022_PROGRAM_ID,
                    6,
                )
                .is_err()
        );

        proof.post_token_balances[0]["uiTokenAmount"]["decimals"] = json!(9);
        assert!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .is_err()
        );
        proof.post_token_balances[0] = json!({"accountIndex":0,"mint":"other-mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"75","decimals":6}});
        assert!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .is_err()
        );
    }
    use solana_message::Message;
    use std::{
        collections::VecDeque,
        sync::{Mutex, atomic::AtomicUsize},
    };

    struct FixtureRpc(Mutex<VecDeque<Value>>);
    #[async_trait]
    impl SolanaRpc for FixtureRpc {
        async fn request(&self, _method: &str, _params: Value) -> Result<Value> {
            self.0.lock().unwrap().pop_front().ok_or_else(parse_error)
        }
    }

    struct UnavailableRpc;
    #[async_trait]
    impl SolanaRpc for UnavailableRpc {
        async fn request(&self, _method: &str, _params: Value) -> Result<Value> {
            Err(GoFinError::new(
                "SOLANA_RPC_UNAVAILABLE",
                "rpc",
                "Synthetic receipt RPC timeout.",
                "Retry reconciliation later.",
            ))
        }
    }

    struct ExpiringJournal {
        candidate_checks: AtomicUsize,
        evidence: Mutex<Option<Value>>,
    }

    impl SubmissionJournal for ExpiringJournal {
        fn assert_authorized(&self) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn assert_candidate(&self, _prepared: &PreparedOperation) -> gofin_domain::Result<()> {
            let count = self.candidate_checks.fetch_add(1, Ordering::AcqRel) + 1;
            if count == 3 {
                return Err(GoFinError::new(
                    "APPROVAL_EXPIRED",
                    "authorization",
                    "Synthetic approval expired during the RPC lifetime check.",
                    "Prepare and approve a fresh operation.",
                ));
            }
            Ok(())
        }

        fn persist_submission(&self, _submission: &Submission) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            evidence: &Value,
        ) -> gofin_domain::Result<()> {
            *self.evidence.lock().unwrap() = Some(evidence.clone());
            Ok(())
        }
    }

    struct NoSend {
        sends: AtomicUsize,
    }

    #[async_trait]
    impl TransactionSender for NoSend {
        async fn send_signed(
            &self,
            _signed_transaction_base64: &str,
            _frozen: &FrozenTransaction,
        ) -> gofin_domain::Result<String> {
            self.sends.fetch_add(1, Ordering::AcqRel);
            Ok("unexpected-send".into())
        }
    }

    fn signer() -> SolanaSigner {
        let key = ed25519_dalek::SigningKey::from_bytes(&[19u8; 32]);
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&[19u8; 32]);
        bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
        SolanaSigner::parse(&bs58::encode(bytes).into_string()).unwrap()
    }

    fn signed_wire(signer: &SolanaSigner) -> (VersionedTransaction, String) {
        let payer = signer.address().parse::<Address>().unwrap();
        let message = VersionedMessage::Legacy(Message::new_with_blockhash(
            &[],
            Some(&payer),
            &Hash::default(),
        ));
        let bytes = codec().serialize(&message).unwrap();
        let signature = Signature::from(signer.sign_message(&bytes).unwrap());
        let tx = VersionedTransaction {
            signatures: vec![signature],
            message,
        };
        (tx.clone(), STANDARD.encode(codec().serialize(&tx).unwrap()))
    }

    fn finalized_fixture(encoded: &str) -> VecDeque<Value> {
        VecDeque::from([
            json!({"value":[{"confirmationStatus":"finalized","err":null}]}),
            json!({"slot":99,"transaction":[encoded,"base64"],"meta":{"fee":5000,"err":null,"preBalances":[10000],"postBalances":[5000],"preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}}),
        ])
    }

    #[tokio::test]
    async fn blockhash_validity_lifetime_has_no_fabricated_height_and_is_rechecked() {
        let signer = signer();
        let (mut tx, _signed) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!({"context":{"slot":5},"value":true}),
            json!({"context":{"slot":6},"value":false}),
        ])))));
        let frozen = runtime
            .freeze_transaction_with_expiry(
                &wire,
                &signer,
                None,
                None,
                10_000,
                Some("stable-trigger-request".into()),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(frozen.last_valid_block_height(), None);
        assert_eq!(frozen.provider_expire_at(), None);
        let error = runtime.assert_lifetime_valid(&frozen).await.unwrap_err();
        assert_eq!(error.code, "SOLANA_BLOCKHASH_EXPIRED");
    }

    #[tokio::test]
    async fn simulation_returns_exact_ordered_accounts_and_preserves_closed_accounts() {
        let signer = signer();
        let (mut tx, _) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!({"context":{"slot":1},"value":true}),
            json!({"value":{"err":null,"unitsConsumed":1234,"accounts":[
                {"owner":"11111111111111111111111111111111","lamports":42,"data":[STANDARD.encode([1,2,3]),"base64"],"executable":false,"rentEpoch":7},
                null
            ]}}),
        ])))));
        let frozen = runtime
            .freeze_transaction_with_expiry(&wire, &signer, None, None, 10_000, None, &[])
            .await
            .unwrap();
        let keys = vec![
            "11111111111111111111111111111111".to_owned(),
            signer.address().to_owned(),
        ];
        let result = runtime
            .simulate_with_accounts(&frozen, &keys)
            .await
            .unwrap();
        assert_eq!(result.units_consumed, Some(1234));
        assert_eq!(result.accounts.len(), 2);
        let account = result.accounts[0].as_ref().unwrap();
        assert_eq!(account.address, keys[0]);
        assert_eq!(account.owner, "11111111111111111111111111111111");
        assert_eq!(account.lamports, 42);
        assert_eq!(account.data, vec![1, 2, 3]);
        assert_eq!(account.rent_epoch, Some(7));
        assert!(result.accounts[1].is_none());

        let duplicate = vec![keys[0].clone(), keys[0].clone()];
        assert!(
            runtime
                .simulate_with_accounts(&frozen, &duplicate)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn simulation_rejects_incomplete_requested_account_results() {
        let signer = signer();
        let (mut tx, _) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!({"context":{"slot":1},"value":true}),
            json!({"value":{"err":null,"accounts":[]}}),
        ])))));
        let frozen = runtime
            .freeze_transaction_with_expiry(&wire, &signer, None, None, 10_000, None, &[])
            .await
            .unwrap();
        let error = runtime
            .simulate_with_accounts(&frozen, &["11111111111111111111111111111111".to_owned()])
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_RESPONSE_INVALID");
    }

    #[tokio::test]
    async fn durable_pre_send_rejection_is_not_confused_with_a_transport_timeout() {
        let signer = signer();
        let (tx, _wire) = signed_wire(&signer);
        let hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        let signature = tx.signatures[0].to_string();
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: signature.clone(),
            data: json!({
                "schema_version":1,
                "message_sha256":hash,
                "recent_blockhash":Hash::default().to_string(),
                "last_valid_block_height":null,
                "lifetime_kind":"blockhash_validity",
                "provider_expire_at":null,
                "wallet_signature":signature,
                "signer":signer.address(),
                "missing_cosigners":[],
                "required_signers":[signer.address()],
                "existing_signatures":[{"signer":signer.address(),"signature":tx.signatures[0].to_string()}]
            }),
        };
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::new()))));
        let evidence = json!({
            "no_network_send":true,
            "reason_code":"SOLANA_BLOCKHASH_EXPIRED",
            "message_sha256":submission.data["message_sha256"]
        });
        let outcome = runtime
            .reconcile_submission(&submission, Some(&evidence))
            .await
            .unwrap();
        assert!(
            matches!(outcome, ReceiptOutcome::NotDispatched { reason_code, .. } if reason_code=="SOLANA_BLOCKHASH_EXPIRED")
        );

        let mut corrupt = evidence;
        corrupt["message_sha256"] = json!("00");
        let error = runtime
            .reconcile_submission(&submission, Some(&corrupt))
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_RECEIPT_IDENTITY_MISMATCH");
    }

    #[tokio::test]
    async fn authorization_is_rechecked_after_lifetime_rpc_and_persisted_as_no_send() {
        let signer = signer();
        let (mut tx, _) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
        let rpc = Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!({"context":{"slot":5},"value":true}),
            json!({"context":{"slot":6},"value":true}),
            json!({"context":{"slot":7},"value":true}),
            json!({"context":{"slot":8},"value":true}),
        ]))));
        let runtime = SolanaRuntime::new(rpc);
        let frozen = runtime
            .freeze_transaction_with_expiry(&wire, &signer, None, None, 10_000, None, &[])
            .await
            .unwrap();
        let prepared = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({}),
            binding: json!({"message_sha256":frozen.message_sha256}),
            summary: "synthetic guarded dispatch".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: gofin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            action_deadline: None,
            operation_version: 1,
        };
        let journal = Arc::new(ExpiringJournal {
            candidate_checks: AtomicUsize::new(0),
            evidence: Mutex::new(None),
        });
        let sender = NoSend {
            sends: AtomicUsize::new(0),
        };
        let submission = runtime
            .dispatch_with_sender(&frozen, &signer, &prepared, journal.clone(), &sender)
            .await
            .unwrap();
        assert_eq!(sender.sends.load(Ordering::Acquire), 0);
        let evidence = journal.evidence.lock().unwrap().clone().unwrap();
        assert_eq!(evidence["no_network_send"], true);
        assert_eq!(evidence["reason_code"], "APPROVAL_EXPIRED");
        assert_eq!(evidence["message_sha256"], frozen.message_sha256);
        assert_eq!(submission.kind, "solana.transaction");
    }

    #[tokio::test]
    async fn restart_reconciliation_binds_signature_and_returns_finalized_proof() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        assert!(verify_signatures(&tx));
        let signature = tx.signatures[0].to_string();
        let message_hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: signature.clone(),
            data: json!({
                "schema_version":1,"message_sha256":message_hash,"recent_blockhash":Hash::default().to_string(),"last_valid_block_height":100,
                "wallet_signature":signature,"signer":signer.address(),"missing_cosigners":[],
                "required_signers":[signer.address()],"existing_signatures":[{"signer":signer.address(),"signature":tx.signatures[0].to_string()}]
            }),
        };
        let runtime =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(finalized_fixture(&wire)))));
        let outcome = runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap();
        let ReceiptOutcome::Finalized {
            proof,
            fee_lamports,
            failed,
            ..
        } = outcome
        else {
            panic!("expected finalized proof")
        };
        assert_eq!(fee_lamports, 5000);
        assert!(!failed);
        assert_eq!(proof.account_keys, vec![signer.address()]);
        assert_eq!(proof.pre_balances_lamports, vec![10000]);
        assert_eq!(proof.post_balances_lamports, vec![5000]);

        let mut bad = submission;
        bad.data["wallet_signature"] = json!(Signature::default().to_string());
        let runtime =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(finalized_fixture(&wire)))));
        assert!(runtime.reconcile_submission(&bad, None).await.is_err());
    }

    #[tokio::test]
    async fn sponsored_timeout_recovers_exact_finalized_wallet_signature_from_bounded_history() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let signature = tx.signatures[0].to_string();
        let message_hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        let responses = VecDeque::from([
            json!([{"signature":signature,"confirmationStatus":"finalized"}]),
            json!({"value":[{"confirmationStatus":"finalized","err":null}]}),
            json!({"slot":99,"transaction":[wire,"base64"],"meta":{"fee":5000,"err":null,"preBalances":[10000],"postBalances":[5000],"preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}}),
        ]);
        assert_eq!(responses.len(), 3);
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: "jupiter:durable-request-1".into(),
            data: json!({"schema_version":1,"message_sha256":message_hash,"recent_blockhash":Hash::default().to_string(),"last_valid_block_height":100,"wallet_signature":signature,"signer":signer.address(),"missing_cosigners":[],"required_signers":[signer.address()],"existing_signatures":[{"signer":signer.address(),"signature":tx.signatures[0].to_string()}]}),
        };
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(responses))));
        let outcome = runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap();
        let ReceiptOutcome::Finalized {
            signature: found,
            proof,
            ..
        } = outcome
        else {
            panic!("expected exact recovered finality")
        };
        assert_eq!(found, signature);
        assert_eq!(proof.required_signatures[0], signature);
    }

    #[tokio::test]
    async fn true_sponsored_recovery_binds_wallet_second_signature_and_zero_wallet_fee() {
        use ed25519_dalek::Signer as _;
        use solana_instruction::{AccountMeta, Instruction};

        let wallet = signer();
        let sponsor_seed = [73u8; 32];
        let sponsor_key = ed25519_dalek::SigningKey::from_bytes(&sponsor_seed);
        let sponsor = Address::new_from_array(sponsor_key.verifying_key().to_bytes());
        let wallet_address = wallet.address().parse::<Address>().unwrap();
        let message = Message::new_with_blockhash(
            &[Instruction {
                program_id: Address::default(),
                accounts: vec![AccountMeta::new(wallet_address, true)],
                data: vec![1],
            }],
            Some(&sponsor),
            &Hash::default(),
        );
        assert_eq!(message.header.num_required_signatures, 2);
        assert_eq!(message.account_keys[0], sponsor);
        assert_eq!(message.account_keys[1], wallet_address);
        let message_bytes = codec()
            .serialize(&VersionedMessage::Legacy(message.clone()))
            .unwrap();
        let sponsor_signature = Signature::from(sponsor_key.sign(&message_bytes).to_bytes());
        let wallet_signature = Signature::from(wallet.sign_message(&message_bytes).unwrap());
        let unsigned_provider_tx = VersionedTransaction {
            signatures: vec![Signature::default(), wallet_signature],
            message: VersionedMessage::Legacy(message.clone()),
        };
        let finalized_tx = VersionedTransaction {
            signatures: vec![sponsor_signature, wallet_signature],
            message: VersionedMessage::Legacy(message),
        };
        assert!(verify_signatures(&finalized_tx));
        let message_hash = hex::encode(Sha256::digest(
            codec().serialize(&finalized_tx.message).unwrap(),
        ));
        let chain_signature = finalized_tx.signatures[0].to_string();
        let wallet_signature = finalized_tx.signatures[1].to_string();
        let finalized_wire = STANDARD.encode(codec().serialize(&finalized_tx).unwrap());
        let unsigned_wire = STANDARD.encode(codec().serialize(&unsigned_provider_tx).unwrap());
        let keys = finalized_tx.message.static_account_keys();
        let history = json!([{"signature":chain_signature,"confirmationStatus":"finalized"}]);
        let details = json!({"slot":101,"transaction":[finalized_wire,"base64"],"meta":{"fee":5000,"err":null,"preBalances":[10000,10000,0],"postBalances":[5000,10000,0],"preTokenBalances":[],"postTokenBalances":[],"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":[]}});
        assert_eq!(keys.len(), 3);
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: canonical_message_reference(&message_hash),
            data: json!({
                "schema_version":1,
                "message_sha256":message_hash,
                "recent_blockhash":Hash::default().to_string(),
                "last_valid_block_height":100,
                "wallet_signature":wallet_signature,
                "signer":wallet.address(),
                "missing_cosigners":[sponsor.to_string()],
                "required_signers":[sponsor.to_string(),wallet.address()],
                "existing_signatures":[
                    {"signer":sponsor.to_string(),"signature":null},
                    {"signer":wallet.address(),"signature":wallet_signature}
                ]
            }),
        };
        let rpc = Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            history,
            json!({"value":[{"confirmationStatus":"finalized","err":null}]}),
            details,
        ]))));
        let runtime = SolanaRuntime::new(rpc);
        let outcome = runtime
            .reconcile_submission(&submission, None)
            .await
            .unwrap();
        let ReceiptOutcome::Finalized {
            signature,
            proof,
            fee_lamports,
            ..
        } = outcome
        else {
            panic!("expected sponsored transaction finality")
        };
        assert_eq!(signature, chain_signature);
        assert_eq!(fee_lamports, 5000);
        assert_eq!(proof.required_signatures[0], chain_signature);
        assert_eq!(proof.required_signatures[1], wallet_signature);
        assert_eq!(proof.lamport_delta(wallet.address()).unwrap().as_str(), "0");
        assert_eq!(proof.account_keys[0], sponsor.to_string());
        let _ = unsigned_wire; // The persisted binding represents the wallet-signed provider candidate.
    }

    #[tokio::test]
    async fn sponsored_recovery_with_no_history_stays_pending() {
        let wallet = signer();
        let (tx, _wire) = signed_wire(&wallet);
        let hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        let wallet_signature = tx.signatures[0].to_string();
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: canonical_message_reference(&hash),
            data: json!({
                "schema_version":1,
                "message_sha256":hash,
                "recent_blockhash":Hash::default().to_string(),
                "last_valid_block_height":100,
                "wallet_signature":wallet_signature,
                "signer":wallet.address(),
                "missing_cosigners":["synthetic-sponsor"],
                "required_signers":["synthetic-sponsor",wallet.address()],
                "existing_signatures":[
                    {"signer":"synthetic-sponsor","signature":null},
                    {"signer":wallet.address(),"signature":wallet_signature}
                ]
            }),
        };
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!([]),
        ])))));
        assert!(matches!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::Pending { .. }
        ));
    }

    #[tokio::test]
    async fn public_finalized_observer_keeps_rpc_timeouts_pending() {
        let runtime = SolanaRuntime::new(Arc::new(UnavailableRpc));
        assert!(matches!(
            runtime
                .observe_finalized_transaction(&Signature::default().to_string())
                .await
                .unwrap(),
            ReceiptOutcome::Pending {
                confirmed: false,
                ..
            }
        ));
    }

    fn v0_lookup_fixture() -> (VersionedMessage, String, String) {
        use solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta};
        use solana_message::{
            MessageHeader, compiled_instruction::CompiledInstruction, v0::MessageAddressTableLookup,
        };
        use std::borrow::Cow;

        let payer = Address::new_from_array([81u8; 32]);
        let program = Address::new_from_array([82u8; 32]);
        let table_key = Address::new_from_array([83u8; 32]);
        let loaded = Address::new_from_array([84u8; 32]);
        let message = VersionedMessage::V0(solana_message::v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, program],
            recent_blockhash: Hash::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 2],
                data: vec![7],
            }],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: table_key,
                writable_indexes: vec![0],
                readonly_indexes: Vec::new(),
            }],
        });
        let table = AddressLookupTable {
            meta: LookupTableMeta {
                last_extended_slot: 1,
                ..LookupTableMeta::default()
            },
            addresses: Cow::Owned(vec![loaded]),
        };
        let data = STANDARD.encode(table.serialize_for_tests().unwrap());
        (message, table_key.to_string(), data)
    }

    #[tokio::test]
    async fn v0_finalized_loaded_addresses_are_bound_to_the_lookup_table() {
        let (message, table_key, table_data) = v0_lookup_fixture();
        let response = json!({"value":{"owner":ALT_PROGRAM_ID,"data":[table_data,"base64"]}});
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            response.clone(),
        ])))));
        runtime
            .verify_finalized_loaded_addresses(
                &message,
                &json!({"writable":[Address::new_from_array([84u8;32]).to_string()],"readonly":[]}),
            )
            .await
            .unwrap();

        let runtime =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([response])))));
        let error = runtime
            .verify_finalized_loaded_addresses(
                &message,
                &json!({"writable":[Address::new_from_array([85u8;32]).to_string()],"readonly":[]}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_RECEIPT_IDENTITY_MISMATCH");
        assert_eq!(
            message.address_table_lookups().unwrap()[0]
                .account_key
                .to_string(),
            table_key
        );
    }

    #[tokio::test]
    async fn v0_prepare_rejects_missing_inactive_and_wrong_owner_lookup_tables() {
        let (message, table_key, table_data) = v0_lookup_fixture();
        let missing = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!(10),
            json!({"value":null}),
        ])))));
        assert_eq!(
            missing
                .resolve_instructions(&message)
                .await
                .unwrap_err()
                .code,
            "SOLANA_LOOKUP_TABLE_MISSING"
        );

        let inactive_data = {
            use std::borrow::Cow;
            let table = solana_address_lookup_table_interface::state::AddressLookupTable {
                meta: solana_address_lookup_table_interface::state::LookupTableMeta {
                    deactivation_slot: 9,
                    ..Default::default()
                },
                addresses: Cow::Owned(vec![Address::new_from_array([84u8; 32])]),
            };
            STANDARD.encode(table.serialize_for_tests().unwrap())
        };
        let inactive = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!(10),
            json!({"value":{"owner":ALT_PROGRAM_ID,"data":[inactive_data,"base64"]}}),
        ])))));
        assert_eq!(
            inactive
                .resolve_instructions(&message)
                .await
                .unwrap_err()
                .code,
            "SOLANA_LOOKUP_TABLE_INACTIVE"
        );

        let wrong_owner = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!(10),
            json!({"value":{"owner":"11111111111111111111111111111111","data":[table_data,"base64"]}}),
        ])))));
        assert_eq!(
            wrong_owner
                .resolve_instructions(&message)
                .await
                .unwrap_err()
                .code,
            "SOLANA_LOOKUP_TABLE_OWNER_INVALID"
        );
        assert_eq!(
            table_key,
            message.address_table_lookups().unwrap()[0]
                .account_key
                .to_string()
        );
    }

    #[test]
    fn rejects_missing_or_invalid_signatures_and_oversized_provider_data() {
        let signer = signer();
        let (mut tx, _) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        assert!(!verify_signatures(&tx));
        tx.signatures[0] = Signature::from([3u8; 64]);
        assert!(!verify_signatures(&tx));
        assert!(
            SolanaRuntime::missing_signers(&"A".repeat(MAX_TRANSACTION_BYTES * 2 + 1)).is_err()
        );
    }

    #[tokio::test]
    async fn rejects_oversized_normalized_source_instruction_before_rpc() {
        let signer = signer();
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::new()))));
        let instruction = NormalizedInstruction {
            program_id: "11111111111111111111111111111111".into(),
            accounts: Vec::new(),
            data_base64: STANDARD.encode(vec![0u8; 1025]),
        };
        let error = runtime
            .freeze_instructions_with_expiry(
                &[instruction],
                &[],
                &Hash::default().to_string(),
                &signer,
                Some(100),
                None,
                5_000,
                None,
                &[],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_TRANSACTION_INVALID");
    }

    #[test]
    fn finalized_token_proof_drops_ui_floats_but_keeps_exact_atoms_and_digestable_evidence() {
        let raw = json!([
            {"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,
             "uiTokenAmount":{"amount":"9007199254740992","decimals":6,"uiAmount":9007199254.740992,"uiAmountString":"9007199254.740992"}}
        ]);
        let pre = token_balance_array(Some(&raw)).unwrap();
        let post_raw = json!([
            {"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,
             "uiTokenAmount":{"amount":"9007199254740993","decimals":6,"uiAmount":9007199254.740993,"uiAmountString":"9007199254.740993"}}
        ]);
        let post = token_balance_array(Some(&post_raw)).unwrap();
        assert_eq!(pre[0]["uiTokenAmount"]["amount"], "9007199254740992");
        assert_eq!(post[0]["uiTokenAmount"]["amount"], "9007199254740993");
        assert_eq!(post[0]["uiTokenAmount"]["decimals"], 6);
        assert!(pre[0]["uiTokenAmount"].get("uiAmount").is_none());
        assert!(post[0]["uiTokenAmount"].get("uiAmountString").is_none());

        let proof = FinalizedReceiptProof {
            signature: "synthetic-signature".into(),
            slot: 1,
            fee_lamports: 0,
            failed: false,
            message_sha256: "00".repeat(32),
            account_keys: vec!["token-account".into()],
            message_instructions: Vec::new(),
            required_signatures: Vec::new(),
            pre_balances_lamports: vec![0],
            post_balances_lamports: vec![0],
            pre_token_balances: pre,
            post_token_balances: post,
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: Vec::new(),
            log_messages: Vec::new(),
            error: None,
        };
        assert_eq!(
            proof
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .unwrap()
                .as_str(),
            "1"
        );

        let mut bundle = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: Submission {
                kind: "solana.transaction".into(),
                reference: "synthetic-signature".into(),
                data: json!({}),
            },
            effects: Vec::new(),
            evidence: json!({"proof":proof}),
            observed_at: "2026-01-01T00:00:00Z".into(),
            proof_digest: String::new(),
        };
        bundle.proof_digest = gofin_domain::verified_effect_digest(&bundle).unwrap();
        assert_eq!(bundle.proof_digest.len(), 64);
        let mut unrelated_float = bundle.clone();
        unrelated_float.evidence["unrelated_display_value"] = json!(1.25);
        assert!(gofin_domain::verified_effect_digest(&unrelated_float).is_err());

        let mut tampered =
            serde_json::from_value::<FinalizedReceiptProof>(bundle.evidence["proof"].clone())
                .unwrap();
        tampered.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("9007199254740994");
        assert_eq!(
            tampered
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .unwrap()
                .as_str(),
            "2",
            "the exact atom field, not uiAmount, controls accounting"
        );
        tampered.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("not-an-integer");
        assert!(
            tampered
                .token_delta_verified("wallet", "mint", crate::token::TOKEN_PROGRAM_ID, 6)
                .is_err()
        );
    }
}
