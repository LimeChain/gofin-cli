use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use bincode::Options;
use futures_util::{StreamExt, stream};
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
    rpc::{RpcReadRequirements, SolanaRpc},
    token::decode_rpc_account_data,
};
use gofin_domain::{GoFinError, PreparedOperation, Result, Submission, SubmissionJournal};

#[path = "expiry.rs"]
mod expiry;
pub use expiry::ExpiredWithoutInclusionProof;

const MAX_TRANSACTION_BYTES: usize = 1232;
const RECONCILIATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_TRANSACTION_ACCOUNT_KEYS: usize = u8::MAX as usize + 1;
const ALT_PROGRAM_ID: &str = "AddressLookupTab1e1111111111111111111111111";

fn transfer_hook_account_count_supported(original_accounts: usize, extra_metas: usize) -> bool {
    // The SPL resolver appends the hook program and validation account to the
    // original instruction plus the metadata-defined accounts. Solana's
    // message account indices are one byte; the actual encoded transaction
    // still has the stricter 1232-byte check at compile/simulation time.
    original_accounts
        .checked_add(extra_metas)
        .and_then(|count| count.checked_add(2))
        .is_some_and(|count| count <= MAX_TRANSACTION_ACCOUNT_KEYS)
}

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

/// Compare an adapter's per-instruction graph with the compiled graph stored
/// in a finalized proof. Solana compiles signer/writable privileges globally,
/// so every occurrence of a key in the compiled message carries the union of
/// privileges requested anywhere in the transaction.
pub fn normalized_instruction_graph_matches(
    expected: &[NormalizedInstruction],
    actual: &[NormalizedInstruction],
    fee_payer: &str,
) -> bool {
    if expected.len() != actual.len() || fee_payer.trim().is_empty() {
        return false;
    }
    let mut privileges = std::collections::BTreeMap::<&str, (bool, bool)>::new();
    privileges.insert(fee_payer, (true, true));
    for instruction in expected {
        for meta in &instruction.accounts {
            let entry = privileges.entry(meta.address.as_str()).or_default();
            entry.0 |= meta.signer;
            entry.1 |= meta.writable;
        }
    }
    expected.iter().zip(actual).all(|(expected, actual)| {
        expected.program_id == actual.program_id
            && expected.data_base64 == actual.data_base64
            && expected.accounts.len() == actual.accounts.len()
            && expected
                .accounts
                .iter()
                .zip(&actual.accounts)
                .all(|(requested, compiled)| {
                    requested.address == compiled.address
                        && privileges.get(requested.address.as_str()).is_some_and(
                            |(signer, writable)| {
                                *signer == compiled.signer && *writable == compiled.writable
                            },
                        )
                })
    })
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
    account_keys: Vec<String>,
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
    pub fn account_keys(&self) -> &[String] {
        &self.account_keys
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
    ExpiredWithoutInclusion {
        signature: String,
        message_sha256: String,
        reason: String,
        proof: Box<ExpiredWithoutInclusionProof>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerTokenDelta {
    pub mint: String,
    pub decimals: u8,
    pub token_program: String,
    pub atoms: gofin_domain::SignedQuantity,
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

    /// Return every token-mint delta for one owner from the receipt, with
    /// token program/decimals and account identities validated on both sides.
    /// This is used to reject unapproved hook or CPI debits outside the pool.
    pub fn all_owner_token_deltas_verified(&self, owner: &str) -> Result<Vec<OwnerTokenDelta>> {
        if owner.trim().is_empty()
            || self.pre_token_balances.len() > 256
            || self.post_token_balances.len() > 256
        {
            return Err(pending_evidence_error());
        }
        let mut identities = std::collections::BTreeMap::<(String, String), u8>::new();
        for balance in self
            .pre_token_balances
            .iter()
            .chain(&self.post_token_balances)
        {
            let row_owner = balance
                .get("owner")
                .and_then(Value::as_str)
                .ok_or_else(pending_evidence_error)?;
            if row_owner != owner {
                continue;
            }
            let mint = balance
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
            let key = (mint.to_owned(), program.to_owned());
            if identities
                .insert(key, decimals)
                .is_some_and(|previous| previous != decimals)
            {
                return Err(receipt_identity_error());
            }
        }
        let mut output = Vec::with_capacity(identities.len());
        for ((mint, program), decimals) in identities {
            output.push(OwnerTokenDelta {
                atoms: self.token_delta_verified(owner, &mint, &program, decimals)?,
                mint,
                decimals,
                token_program: program,
            });
        }
        Ok(output)
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
    expiry_cache: std::sync::Mutex<expiry::ExpiryCache>,
}

struct RuntimeDispatchGuard<'a> {
    runtime: &'a SolanaRuntime,
    frozen: &'a FrozenTransaction,
    journal: &'a dyn SubmissionJournal,
    prepared: &'a PreparedOperation,
    denied: &'a AtomicBool,
    preserve_primary_for_send: bool,
}

struct DispatchSignerScope<'a> {
    secondary_signers: &'a [Arc<SolanaSigner>],
    allow_approved_external_partials: bool,
    preserve_primary_for_send: bool,
}

#[async_trait]
impl AsyncDispatchGuard for RuntimeDispatchGuard<'_> {
    async fn check(&self) -> Result<()> {
        let checked = async {
            self.journal.assert_candidate(self.prepared)?;
            self.runtime
                .assert_lifetime_valid_with_read_policy(self.frozen, self.preserve_primary_for_send)
                .await?;
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
/// immediate provider call; the protected journal retains it for identity verification.
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
        Self {
            rpc,
            expiry_cache: Default::default(),
        }
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
    /// ExtraAccountMetaList. Canonically declared writable extras are allowed;
    /// additional signer authority is never inferred from hook metadata.
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
        if !transfer_hook_account_count_supported(original.len(), extra_metas.len()) {
            return Err(failure(
                "SOLANA_TRANSACTION_INVALID",
                "prepare",
                "The transfer hook account graph exceeds Solana's transaction account-index limit.",
                "Use a transaction graph that fits within Solana's message account-key limit.",
            ));
        }
        let requested_signers = extra_metas
            .iter()
            .map(|meta| bool::from(meta.is_signer))
            .collect::<Vec<_>>();
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
        if old_instruction.accounts.len() > MAX_TRANSACTION_ACCOUNT_KEYS
            || old_instruction.accounts[..original.len()] != original
        {
            return Err(failure(
                "SOLANA_TRANSFER_HOOK_METAS_INVALID",
                "prepare",
                "The transfer hook produced an invalid account-meta list.",
                "Use a token with a reviewed transfer-hook account configuration.",
            ));
        }
        // The SPL resolver intentionally de-escalates every extra meta to a
        // non-signer because a CPI cannot manufacture signer authority. Keep
        // the canonical metadata request visible to the adapter anyway: the
        // broker must accept it only when that exact public key is already
        // the wallet or a bound auxiliary signer. The compiled transaction
        // still determines the effective global privilege union.
        for (index, requested_signer) in requested_signers.into_iter().enumerate() {
            let target = old_instruction
                .accounts
                .get_mut(original.len() + index)
                .ok_or_else(invalid_transaction)?;
            target.is_signer = requested_signer;
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
        self.freeze_transaction_inner(
            encoded,
            signer,
            last_valid_block_height,
            provider_expire_at,
            maximum_network_fee_lamports,
            provider_request_id,
            allowed_missing_cosigners,
            true,
        )
        .await
    }

    /// Freeze an opaque transaction from a trusted execution provider.
    ///
    /// The adapter must bind financial authority to the provider's order before
    /// calling this method. This checks signing structure, existing signatures
    /// and lifetime, but deliberately does not load lookup tables, resolve or
    /// validate instructions, estimate fees, or simulate execution. The returned
    /// instruction and account lists are empty; settlement resolves the accounts
    /// independently from finalized chain evidence.
    #[allow(clippy::too_many_arguments)]
    pub async fn freeze_provider_transaction_with_expiry(
        &self,
        encoded: &str,
        signer: &SolanaSigner,
        last_valid_block_height: Option<u64>,
        provider_expire_at: Option<&str>,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
    ) -> Result<FrozenTransaction> {
        self.freeze_transaction_inner(
            encoded,
            signer,
            last_valid_block_height,
            provider_expire_at,
            maximum_network_fee_lamports,
            provider_request_id,
            allowed_missing_cosigners,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn freeze_transaction_inner(
        &self,
        encoded: &str,
        signer: &SolanaSigner,
        last_valid_block_height: Option<u64>,
        provider_expire_at: Option<&str>,
        maximum_network_fee_lamports: u64,
        provider_request_id: Option<String>,
        allowed_missing_cosigners: &[String],
        resolve_instructions: bool,
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
        let (instructions, account_keys) = if resolve_instructions {
            self.resolve_instructions(&transaction.message).await?
        } else {
            (Vec::new(), Vec::new())
        };
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
            account_keys,
        };
        self.assert_lifetime_valid(&frozen).await?;
        Ok(frozen)
    }

    /// Recheck the original transaction lifetime without changing its blockhash.
    /// Call after any provider-side queue/rate wait and immediately before HTTP send.
    pub async fn assert_lifetime_valid(&self, frozen: &FrozenTransaction) -> Result<()> {
        self.assert_lifetime_valid_with_read_policy(frozen, false)
            .await
    }

    async fn assert_lifetime_valid_with_read_policy(
        &self,
        frozen: &FrozenTransaction,
        preserve_primary_for_send: bool,
    ) -> Result<()> {
        let request = |method: &'static str, params: Value| async move {
            if preserve_primary_for_send {
                self.rpc
                    .request_checked(
                        method,
                        params,
                        RpcReadRequirements {
                            preserve_primary_for_send: true,
                            ..Default::default()
                        },
                    )
                    .await
            } else {
                self.rpc.request(method, params).await
            }
        };
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
            let height = request("getBlockHeight", json!([{"commitment":"confirmed"}]))
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
            let response = request(
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

    /// Estimate the fee for an unsigned normalized instruction graph. This
    /// compiles the same legacy/v0 message shape used for a frozen candidate,
    /// but never creates signatures or submits a transaction. Read-only quote
    /// previews use it to report an exact current fee estimate.
    pub async fn estimate_fee_for_instructions(
        &self,
        payer_address: &str,
        instructions: &[NormalizedInstruction],
        lookup_tables: &[String],
        blockhash: &str,
        maximum_network_fee_lamports: u64,
    ) -> Result<u64> {
        if instructions.is_empty() || instructions.len() > 64 || lookup_tables.len() > 8 {
            return Err(invalid_transaction());
        }
        let payer = payer_address
            .parse::<Address>()
            .map_err(|_| invalid_transaction())?;
        let hash = blockhash
            .parse::<Hash>()
            .map_err(|_| invalid_transaction())?;
        let mut compiled = Vec::with_capacity(instructions.len());
        let mut unique_keys = std::collections::HashSet::new();
        unique_keys.insert(payer);
        let mut account_count = 0usize;
        for instruction in instructions {
            let program_id = instruction
                .program_id
                .parse::<Address>()
                .map_err(|_| invalid_transaction())?;
            unique_keys.insert(program_id);
            let data = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_transaction())?;
            if data.len() > 1024 {
                return Err(invalid_transaction());
            }
            let mut accounts = Vec::with_capacity(instruction.accounts.len());
            for account in &instruction.accounts {
                let address = account
                    .address
                    .parse::<Address>()
                    .map_err(|_| invalid_transaction())?;
                unique_keys.insert(address);
                account_count += 1;
                if account_count > 240 {
                    return Err(invalid_transaction());
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
            return Err(invalid_transaction());
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
                    .map_err(|_| invalid_transaction())?,
            )
        };
        message.sanitize().map_err(|_| invalid_transaction())?;
        let message_bytes = codec()
            .serialize(&message)
            .map_err(|_| invalid_transaction())?;
        let required = usize::from(message.header().num_required_signatures);
        let wire = codec()
            .serialize(&VersionedTransaction {
                signatures: vec![Signature::default(); required],
                message,
            })
            .map_err(|_| invalid_transaction())?;
        if wire.len() > MAX_TRANSACTION_BYTES {
            return Err(invalid_transaction());
        }
        let result = self
            .rpc
            .request(
                "getFeeForMessage",
                json!([STANDARD.encode(message_bytes),{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let fee = result
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(parse_error)?;
        if fee > maximum_network_fee_lamports {
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
                        "innerInstructions":true,
                        "accounts":{"encoding":"base64","addresses":account_addresses}
                    }
                ]),
            )
            .await?;
        let value = result.get("value").ok_or_else(parse_error)?;
        if let Some(err) = simulation_error_value(value) {
            return Err(simulation_failed_with_program_detail(
                err,
                value,
                &frozen.account_keys,
            ));
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
        let fee_lamports = match simulation_fee_lamports(value)? {
            Some(fee) => Some(fee),
            None => {
                let message = codec()
                    .serialize(&frozen.transaction.message)
                    .map_err(|_| invalid_transaction())?;
                let fee = self
                    .rpc
                    .request(
                        "getFeeForMessage",
                        json!([
                            STANDARD.encode(message),
                            {"encoding":"base64","commitment":"confirmed"}
                        ]),
                    )
                    .await?
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(parse_error)?;
                Some(fee)
            }
        };
        Ok(SimulationWithAccountsResult {
            units_consumed,
            fee_lamports,
            logs: simulation_logs(value)?,
            inner_instructions: simulation_inner_instructions(value)?,
            account_keys: frozen.account_keys.clone(),
            accounts,
        })
    }

    /// Simulate an unsigned, frozen-ready protocol probe without possessing or
    /// invoking any signer. This is for quote/preparation only: callers must
    /// never authorize, journal, or submit the probe. The exact account list
    /// returned by RPC is bounded and preserves null entries.
    pub async fn simulate_unsigned_instructions_with_accounts(
        &self,
        payer_address: &str,
        instructions: &[NormalizedInstruction],
        lookup_tables: &[String],
        blockhash: &str,
        account_addresses: &[String],
    ) -> Result<SimulationWithAccountsResult> {
        const MAX_SIMULATION_ACCOUNTS: usize = 100;
        if instructions.is_empty()
            || instructions.len() > 64
            || account_addresses.is_empty()
            || account_addresses.len() > MAX_SIMULATION_ACCOUNTS
        {
            return Err(GoFinError::validation(
                "Unsigned simulation requires between 1 and 64 instructions and 1 to 100 exact account addresses.",
            ));
        }
        let payer = payer_address
            .parse::<Address>()
            .map_err(|_| invalid_transaction())?;
        let hash = blockhash
            .parse::<Hash>()
            .map_err(|_| invalid_transaction())?;
        let mut seen = std::collections::BTreeSet::new();
        for address in account_addresses {
            crate::parse_address(address)?;
            if !seen.insert(address) {
                return Err(GoFinError::validation(
                    "Simulation account addresses must be unique.",
                ));
            }
        }
        let mut compiled = Vec::with_capacity(instructions.len());
        let mut unique_keys = std::collections::HashSet::new();
        unique_keys.insert(payer);
        let mut account_count = 0usize;
        for instruction in instructions {
            let program_id = instruction
                .program_id
                .parse::<Address>()
                .map_err(|_| invalid_transaction())?;
            unique_keys.insert(program_id);
            let data = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_transaction())?;
            if data.len() > 1024 {
                return Err(invalid_transaction());
            }
            let mut accounts = Vec::with_capacity(instruction.accounts.len());
            for account in &instruction.accounts {
                let address = account
                    .address
                    .parse::<Address>()
                    .map_err(|_| invalid_transaction())?;
                unique_keys.insert(address);
                account_count += 1;
                if account_count > 240 {
                    return Err(invalid_transaction());
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
            return Err(invalid_transaction());
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
                    .map_err(|_| invalid_transaction())?,
            )
        };
        message.sanitize().map_err(|_| invalid_transaction())?;
        let message_bytes = codec()
            .serialize(&message)
            .map_err(|_| invalid_transaction())?;
        let account_keys = compiled_message_account_keys(&message, &tables)?;
        let required = usize::from(message.header().num_required_signatures);
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); required],
            message,
        };
        let wire = codec()
            .serialize(&transaction)
            .map_err(|_| invalid_transaction())?;
        if wire.len() > MAX_TRANSACTION_BYTES {
            return Err(invalid_transaction());
        }
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
                        "innerInstructions":true,
                        "accounts":{"encoding":"base64","addresses":account_addresses}
                    }
                ]),
            )
            .await?;
        let value = result.get("value").ok_or_else(parse_error)?;
        if let Some(err) = simulation_error_value(value) {
            return Err(simulation_failed_with_program_detail(
                err,
                value,
                &account_keys,
            ));
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
                let data = decode_rpc_account_data(account.get("data").ok_or_else(parse_error)?)?;
                let executable = account
                    .get("executable")
                    .and_then(Value::as_bool)
                    .ok_or_else(parse_error)?;
                let rent_epoch = account.get("rentEpoch").and_then(Value::as_u64);
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
        let fee_lamports = match simulation_fee_lamports(value)? {
            Some(fee) => Some(fee),
            None => {
                let fee = self
                    .rpc
                    .request(
                        "getFeeForMessage",
                        json!([
                            STANDARD.encode(message_bytes),
                            {"encoding":"base64","commitment":"confirmed"}
                        ]),
                    )
                    .await?
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(parse_error)?;
                Some(fee)
            }
        };
        Ok(SimulationWithAccountsResult {
            units_consumed,
            fee_lamports,
            logs: simulation_logs(value)?,
            inner_instructions: simulation_inner_instructions(value)?,
            account_keys,
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
                "This transaction needs a broker-owned signer or external co-signer and cannot use direct RPC submission.",
                "Use the exact broker-owned signer identities bound during preparation, or a reviewed provider landing path.",
            ));
        }
        let sender = RpcTransactionSender {
            rpc: self.rpc.clone(),
        };
        self.dispatch_with_secondary_signers(frozen, signer, &[], prepared, journal, &sender)
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
        self.dispatch_internal(
            frozen,
            signer,
            prepared,
            journal,
            sender,
            DispatchSignerScope {
                secondary_signers: &[],
                allow_approved_external_partials: true,
                preserve_primary_for_send: false,
            },
        )
        .await
    }

    /// Dispatch with protected broker-owned secondary signers. The supplied
    /// set must exactly match the frozen message's missing signer slots; no
    /// signer identity or message bytes can be added or changed here.
    pub async fn dispatch_with_secondary_signers(
        &self,
        frozen: &FrozenTransaction,
        signer: &SolanaSigner,
        secondary_signers: &[Arc<SolanaSigner>],
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
        sender: &dyn TransactionSender,
    ) -> Result<Submission> {
        // Both ordinary RPC dispatch and broker-owned secondary-signature
        // dispatch (including Meteora) may wait for primary readiness before
        // signing or retaining a pending identity. dispatch_internal then
        // rechecks authority/lifetime; send-edge preparation never waits.
        self.rpc.prepare_send_before_signing().await?;
        self.dispatch_internal(
            frozen,
            signer,
            prepared,
            journal,
            sender,
            DispatchSignerScope {
                secondary_signers,
                allow_approved_external_partials: false,
                preserve_primary_for_send: true,
            },
        )
        .await
    }

    async fn dispatch_internal(
        &self,
        frozen: &FrozenTransaction,
        signer: &SolanaSigner,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
        sender: &dyn TransactionSender,
        signer_scope: DispatchSignerScope<'_>,
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
        let mut supplied = signer_scope
            .secondary_signers
            .iter()
            .map(|signer| signer.address().to_owned())
            .collect::<Vec<_>>();
        supplied.sort();
        supplied.dedup();
        let mut expected = frozen.missing_cosigners.clone();
        expected.sort();
        let missing_scope_is_valid = {
            let mut missing = frozen.missing_cosigners.clone();
            missing.sort();
            missing.dedup();
            missing.len() == frozen.missing_cosigners.len()
                && missing.iter().all(|address| {
                    address != &frozen.signer && frozen.required_signers.contains(address)
                })
        };
        let signer_scope_valid = if signer_scope.allow_approved_external_partials {
            signer_scope.secondary_signers.is_empty() && missing_scope_is_valid
        } else {
            supplied.len() == signer_scope.secondary_signers.len() && supplied == expected
        };
        if !signer_scope_valid {
            return Err(failure(
                "SOLANA_COSIGNER_BINDING_MISMATCH",
                "dispatch",
                "The signer set does not match the frozen transaction's approved dispatch mode and missing signer slots.",
                "Keep the operation unresolved and use only the exact broker-owned signers or reviewed provider landing path bound during preparation.",
            ));
        }
        self.assert_lifetime_valid_with_read_policy(frozen, signer_scope.preserve_primary_for_send)
            .await?;
        let mut transaction = frozen.transaction.clone();
        let signature = frozen.signer_index;
        let signed = signer.sign_message(&frozen.message_bytes)?;
        transaction.signatures[signature] = Signature::from(signed);
        for secondary in signer_scope.secondary_signers {
            let index = frozen
                .required_signers
                .iter()
                .position(|address| address == secondary.address())
                .ok_or_else(|| {
                    failure(
                        "SOLANA_COSIGNER_BINDING_MISMATCH",
                        "dispatch",
                        "A protected secondary signer is not required by the frozen transaction.",
                        "Keep the operation unresolved and load only signer identities bound during preparation.",
                    )
                })?;
            if !transaction.signatures[index]
                .as_ref()
                .iter()
                .all(|byte| *byte == 0)
            {
                return Err(failure(
                    "SOLANA_COSIGNER_BINDING_MISMATCH",
                    "dispatch",
                    "The frozen transaction already contains a signature for a broker-owned signer slot.",
                    "Prepare a fresh exact transaction candidate.",
                ));
            }
            transaction.signatures[index] =
                Signature::from(secondary.sign_message(&frozen.message_bytes)?);
        }
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
        let still_missing = missing_signature_addresses(
            &transaction.signatures,
            &frozen.required_signers,
            frozen.signer_index,
        );
        // Let provider senders validate their own immutable request identity,
        // but key the durable submission by the signed message itself. This
        // deduplicates the same Solana transaction across RPC, Jupiter Meta,
        // and other sponsored landing paths even if provider request IDs differ.
        let _provider_reference =
            sender.submission_reference(frozen, first_signature.as_deref())?;
        let reference = canonical_message_reference(frozen.message_sha256());
        let signed_wire = STANDARD.encode(wire);
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: reference.clone(),
            data: json!({"schema_version":2,"signed_transaction_base64":signed_wire,"message_sha256":frozen.message_sha256,"recent_blockhash":frozen.recent_blockhash,"last_valid_block_height":frozen.last_valid_block_height,"lifetime_kind":if frozen.last_valid_block_height.is_some(){"block_height"}else{"blockhash_validity"},"provider_expire_at":frozen.provider_expire_at,"provider_request_id":frozen.provider_request_id,"chain_signature":first_signature,"wallet_signature":transaction.signatures[signature].to_string(),"signer":frozen.signer,"fee_payer":frozen.fee_payer,"required_signers":frozen.required_signers,"missing_cosigners":still_missing,"existing_signatures":transaction.signatures.iter().enumerate().map(|(i,sig)|json!({"signer":frozen.required_signers[i],"signature":if sig.as_ref().iter().all(|b|*b==0){Value::Null}else{json!(sig.to_string())}})).collect::<Vec<_>>()}),
        };
        journal.persist_submission(&submission)?;
        let guard_denied = AtomicBool::new(false);
        let guard = RuntimeDispatchGuard {
            runtime: self,
            frozen,
            journal: journal.as_ref(),
            prepared,
            denied: &guard_denied,
            preserve_primary_for_send: signer_scope.preserve_primary_for_send,
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
            Err(error) => Err(submission_uncertain(&error)),
        }
    }

    pub async fn reconcile(
        &self,
        signature: &str,
        expected_message_sha256: &str,
    ) -> Result<ReceiptOutcome> {
        reconciliation_with_budget(self.reconcile_inner(signature, Some(expected_message_sha256)))
            .await
    }

    /// Read and verify finalized transaction evidence when the caller knows
    /// the canonical signature but the message was created by an external
    /// keeper/provider. Application code must still bind the instruction and
    /// event semantics to its own request identity.
    pub async fn observe_finalized_transaction(&self, signature: &str) -> Result<ReceiptOutcome> {
        match reconciliation_with_budget(self.reconcile_inner(signature, None)).await {
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
            ReceiptOutcome::Pending { .. }
            | ReceiptOutcome::NotDispatched { .. }
            | ReceiptOutcome::ExpiredWithoutInclusion { .. } => Err(pending_evidence_error()),
        }
    }

    async fn reconcile_inner(
        &self,
        signature: &str,
        expected_message_sha256: Option<&str>,
    ) -> Result<ReceiptOutcome> {
        let status = self
            .rpc
            .request_checked(
                "getSignatureStatuses",
                json!([[signature],{"searchTransactionHistory":true}]),
                RpcReadRequirements {
                    finalized_signature: true,
                    ..Default::default()
                },
            )
            .await?;
        let entry = status
            .get("value")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .cloned()
            .unwrap_or(Value::Null);
        let confirmation = entry
            .get("confirmationStatus")
            .and_then(Value::as_str)
            .unwrap_or("");
        let confirmed = matches!(confirmation, "confirmed" | "finalized");
        if !entry.is_null() && confirmation != "finalized" {
            return Ok(ReceiptOutcome::Pending {
                confirmed,
                reason: "The transaction has not reached finalized commitment.".into(),
            });
        }
        let result = self.rpc.request_checked("getTransaction", json!([signature,{"encoding":"base64","commitment":"finalized","maxSupportedTransactionVersion":0}]), RpcReadRequirements { require_non_null: true, ..Default::default() }).await?;
        if result.is_null() {
            return Ok(ReceiptOutcome::Pending {
                confirmed,
                reason: if entry.is_null() {
                    "The transaction is not present in available signature or finalized transaction history."
                } else {
                    "Finalized signature status has no transaction details yet."
                }.into(),
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
        if !entry.is_null() && entry.get("slot").and_then(Value::as_u64) != Some(slot) {
            return Err(receipt_identity_error());
        }
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
        if !entry.is_null() && entry.get("err") != Some(&error) {
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
            self.verify_finalized_loaded_addresses(&tx.message, &loaded_addresses, slot)
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
        if let ReceiptOutcome::Finalized { proof, .. } = &outcome {
            if proof
                .required_signatures
                .get(frozen.signer_index)
                .map(String::as_str)
                != Some(wallet_signature)
                || frozen.required_signers.len() != proof.required_signatures.len()
            {
                return Err(receipt_identity_error());
            }
            for (idx, sig) in proof.required_signatures.iter().enumerate() {
                let old = &frozen.transaction.signatures[idx];
                if idx != frozen.signer_index
                    && old != &Signature::default()
                    && old.to_string() != *sig
                {
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
        let mut outcome = reconciliation_with_budget(
            self.reconcile_submission_inner(submission, dispatch_evidence),
        )
        .await?;
        if let ReceiptOutcome::Pending { reason, .. } = &mut outcome
            && let Some(signature) = submission.data["chain_signature"].as_str()
            && let Some(height) = submission.data["last_valid_block_height"].as_u64()
            && let Some(progress) = self.expiry_progress(signature, height)
        {
            reason.push(' ');
            reason.push_str(&progress);
        }
        Ok(outcome)
    }

    async fn reconcile_submission_inner(
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
        if !matches!(
            data.get("schema_version").and_then(Value::as_u64),
            Some(1 | 2)
        ) || data
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
        if data.get("schema_version").and_then(Value::as_u64) == Some(2) {
            validate_persisted_wire(data)?;
        }
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
                .request_checked(
                    "getSignaturesForAddress",
                    json!([expected_wallet,{"commitment":"finalized","limit":100}]),
                    RpcReadRequirements { merge_signature_history: true, ..Default::default() },
                )
                .await {
                Ok(value) => value,
                Err(error) if error.code == "SOLANA_RPC_UNAVAILABLE" => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "The receipt RPC is temporarily unavailable; this submission remains unresolved.".into() }),
                Err(error) => return Err(error),
            };
            let candidates = history.as_array().ok_or_else(pending_evidence_error)?;
            // Keep searches scoped to this reconciliation: one slow unrelated
            // candidate cannot monopolize the shared budget, and dropping the
            // stream cancels its in-flight reads when an exact match is found.
            let signatures = candidates
                .iter()
                .filter_map(|candidate| {
                    candidate
                        .get("signature")
                        .and_then(Value::as_str)
                        .filter(|signature| signature.len() <= 128)
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>();
            let mut pending = stream::iter(signatures)
                .map(|signature| async move {
                    let outcome = self.reconcile(&signature, expected_hash).await;
                    (signature, outcome)
                })
                .buffer_unordered(2);
            let mut match_found = None;
            while let Some((signature, result)) = pending.next().await {
                let candidate_outcome = match result {
                    Ok(outcome) => outcome,
                    Err(error) if error.code == "SOLANA_RPC_CONFLICT" => return Err(error),
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
                    match_found = Some((signature, candidate_outcome));
                    break;
                }
            }
            match match_found {
                Some(found) => found,
                None => return Ok(ReceiptOutcome::Pending { confirmed: false, reason: "No finalized transaction matching the durable message hash and wallet signature appeared in the bounded history window.".into() }),
            }
        };
        let chain_signature = chain_signature.as_str();
        if matches!(
            outcome,
            ReceiptOutcome::Pending {
                confirmed: false,
                ..
            }
        ) && let Some(proof) = self
            .prove_expired_without_inclusion(data, chain_signature)
            .await?
        {
            return Ok(ReceiptOutcome::ExpiredWithoutInclusion {
                signature: chain_signature.to_owned(),
                message_sha256: expected_hash.to_owned(),
                reason: "Complete finalized block coverage proves the original signed transaction was not included before its recent blockhash expired.".into(),
                proof: Box::new(proof),
            });
        }
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
    ) -> Result<(Vec<NormalizedInstruction>, Vec<String>)> {
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
        Ok((instructions, full_keys))
    }

    async fn verify_finalized_loaded_addresses(
        &self,
        message: &VersionedMessage,
        loaded_addresses: &Value,
        receipt_slot: u64,
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
                .request_checked(
                    "getAccountInfo",
                    json!([address,{"encoding":"base64","commitment":"finalized","minContextSlot":receipt_slot}]),
                    RpcReadRequirements { minimum_context_slot: Some(receipt_slot), require_non_null: true, ..Default::default() },
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

/// Keep bounded transport diagnostics without trusting a sender's error text
/// or arbitrary details. These fields never establish non-dispatch evidence.
fn submission_uncertain(cause: &GoFinError) -> GoFinError {
    let mut diagnostic = serde_json::Map::new();
    let code = match cause.code.as_str() {
        "SOLANA_RPC_UNAVAILABLE"
        | "SOLANA_RPC_RATE_LIMITED"
        | "SOLANA_RPC_UNSUPPORTED"
        | "SOLANA_RPC_UNHEALTHY"
        | "SOLANA_RPC_STALE"
        | "SOLANA_RPC_HISTORY_UNAVAILABLE"
        | "SOLANA_RPC_DATA_INVALID"
        | "SOLANA_RPC_CONFLICT"
        | "SOLANA_NETWORK_MISMATCH"
        | "SOLANA_RESPONSE_INVALID" => Some(cause.code.as_str()),
        _ => None,
    };
    diagnostic.insert(
        "code".into(),
        json!(code.unwrap_or("UNCLASSIFIED_SENDER_ERROR")),
    );
    if code.is_some()
        && let Some(details) = cause.details.as_deref().and_then(Value::as_object)
    {
        if let Some(stage) = details
            .get("stage")
            .and_then(Value::as_str)
            .filter(|stage| {
                matches!(
                    *stage,
                    "transport"
                        | "transport_timeout"
                        | "http_status"
                        | "body_read"
                        | "json"
                        | "id_mismatch"
                        | "missing_result"
                        | "response_too_large"
                        | "rpc_error"
                        | "primary_cooldown"
                )
            })
        {
            diagnostic.insert("stage".into(), json!(stage));
        }
        if let Some(status) = details
            .get("http_status")
            .and_then(Value::as_u64)
            .filter(|status| (100..=599).contains(status))
        {
            diagnostic.insert("http_status".into(), json!(status));
        }
        if let Some(code) = details
            .get("rpc_code")
            .and_then(Value::as_i64)
            .filter(|code| {
                // Standard JSON-RPC failures, reviewed Solana server errors,
                // and the two rate-limit codes recognized by our RPC client.
                matches!(*code, -32700 | -32603..=-32600 | -32016..=-32001 | -32429 | 429)
            })
        {
            diagnostic.insert("rpc_code".into(), json!(code));
        }
    }
    failure(
        "SOLANA_SUBMISSION_UNCERTAIN",
        "dispatch",
        "The RPC outcome is uncertain after the transaction identity was persisted.",
        "Reconcile the original transaction reference; do not submit a replacement.",
    )
    .with_details(json!({"sender_cause":diagnostic}))
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

fn simulation_failed_with_program_detail(
    error: &Value,
    simulation: &Value,
    account_keys: &[String],
) -> gofin_domain::GoFinError {
    let mut failure = simulation_failed(error);
    let Some(details) = failure
        .details
        .as_deref_mut()
        .and_then(Value::as_object_mut)
    else {
        return failure;
    };
    let summary = simulation
        .get("logs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|line| {
            line.starts_with("Program ")
                && (line.contains(" failed:")
                    || line.contains("AnchorError")
                    || line.contains("custom program error"))
                && !line.starts_with("Program data:")
        })
        .take(6)
        .map(|line| line.chars().take(240).collect::<String>())
        .collect::<Vec<_>>();
    if !summary.is_empty() {
        details.insert("program_failure_summary".into(), json!(summary));
    }
    // Failed simulations can still reveal the exact CPI which attempted a
    // Token-2022 transfer before rejecting missing transfer-hook metas. Keep
    // only a small, structurally valid slice so protocol adapters can
    // authenticate it against the immutable compiled parent instruction.
    if let Some(evidence) = bounded_simulation_evidence(simulation, account_keys) {
        details.insert("simulation_evidence".into(), evidence);
    }
    failure
}

fn bounded_simulation_evidence(simulation: &Value, account_keys: &[String]) -> Option<Value> {
    const MAX_KEYS: usize = 64;
    const MAX_GROUPS: usize = 16;
    const MAX_INSTRUCTIONS_PER_GROUP: usize = 32;
    const MAX_DATA_BYTES: usize = 1024;

    if account_keys.is_empty() || account_keys.len() > MAX_KEYS {
        return None;
    }
    let groups = simulation
        .get("innerInstructions")?
        .as_array()?
        .iter()
        .take(MAX_GROUPS)
        .filter_map(|group| {
            let index = group.get("index")?.as_u64()?;
            let instructions = group.get("instructions")?.as_array()?;
            let bounded = instructions
                .iter()
                .take(MAX_INSTRUCTIONS_PER_GROUP)
                .filter_map(|instruction| {
                    let program_id_index = instruction.get("programIdIndex")?.as_u64()?;
                    let program_id_index = usize::try_from(program_id_index).ok()?;
                    if program_id_index >= account_keys.len() {
                        return None;
                    }
                    let account_indexes = instruction
                        .get("accounts")?
                        .as_array()?
                        .iter()
                        .map(|account| {
                            let index = account.as_u64()?;
                            let index = usize::try_from(index).ok()?;
                            (index < account_keys.len()).then_some(index)
                        })
                        .collect::<Option<Vec<_>>>()?;
                    let data = instruction.get("data")?.as_str()?;
                    // RPC returns base58 instruction bytes. Bound encoded
                    // length before preserving it; the consuming adapter
                    // performs canonical decoding and discriminator checks.
                    if data.len() > MAX_DATA_BYTES * 2
                        || bs58::decode(data).into_vec().ok()?.len() > MAX_DATA_BYTES
                    {
                        return None;
                    }
                    let mut bounded = json!({
                        "program_id_index": program_id_index,
                        "accounts": account_indexes,
                        "data": data,
                    });
                    if let Some(stack_height) =
                        instruction.get("stackHeight").and_then(Value::as_u64)
                    {
                        bounded["stack_height"] = json!(stack_height);
                    }
                    Some(bounded)
                })
                .collect::<Vec<_>>();
            Some(json!({ "index": index, "instructions": bounded }))
        })
        .collect::<Vec<_>>();
    Some(json!({ "account_keys": account_keys, "inner_instructions": groups }))
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
    /// Runtime fee estimate reported by simulateTransaction, when supported.
    #[serde(default)]
    pub fee_lamports: Option<u64>,
    /// Exact runtime logs returned by simulateTransaction. Callers must still
    /// authenticate program IDs, CPI nesting and event payloads before using
    /// logs as protocol evidence.
    pub logs: Vec<String>,
    /// Raw simulation inner-instruction groups, bounded at decode time. The
    /// adapter must authenticate parent instruction and program nesting
    /// before treating nested instruction data as protocol evidence.
    #[serde(default)]
    pub inner_instructions: Vec<Value>,
    /// Exact compiled message key order used to resolve program and account
    /// indices in `inner_instructions` (static, loaded writable, loaded readonly).
    #[serde(default)]
    pub account_keys: Vec<String>,
    /// Same order as the requested address list; null entries are preserved.
    pub accounts: Vec<Option<SimulatedAccountInfo>>,
}

fn simulation_fee_lamports(value: &Value) -> Result<Option<u64>> {
    value
        .get("fee")
        .filter(|fee| !fee.is_null())
        .map(|fee| fee.as_u64().ok_or_else(parse_error))
        .transpose()
}

fn missing_signature_addresses(
    signatures: &[Signature],
    required_signers: &[String],
    wallet_signer_index: usize,
) -> Vec<String> {
    signatures
        .iter()
        .enumerate()
        .filter(|(index, signature)| {
            *index != wallet_signer_index && signature.as_ref().iter().all(|byte| *byte == 0)
        })
        .filter_map(|(index, _)| required_signers.get(index).cloned())
        .collect()
}

fn compiled_message_account_keys(
    message: &VersionedMessage,
    tables: &[(Address, Vec<Address>)],
) -> Result<Vec<String>> {
    let mut keys = message
        .static_account_keys()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let Some(lookups) = message.address_table_lookups() else {
        return Ok(keys);
    };
    let mut writable = Vec::new();
    let mut readonly = Vec::new();
    for lookup in lookups {
        let (_, addresses) = tables
            .iter()
            .find(|(key, _)| key == &lookup.account_key)
            .ok_or_else(invalid_transaction)?;
        for index in &lookup.writable_indexes {
            writable.push(
                addresses
                    .get(usize::from(*index))
                    .ok_or_else(invalid_lookup_index)?
                    .to_string(),
            );
        }
        for index in &lookup.readonly_indexes {
            readonly.push(
                addresses
                    .get(usize::from(*index))
                    .ok_or_else(invalid_lookup_index)?
                    .to_string(),
            );
        }
    }
    keys.extend(writable);
    keys.extend(readonly);
    if keys.len() > MAX_TRANSACTION_ACCOUNT_KEYS {
        return Err(invalid_transaction());
    }
    Ok(keys)
}

fn simulation_logs(value: &Value) -> Result<Vec<String>> {
    let Some(logs) = value.get("logs") else {
        return Ok(Vec::new());
    };
    let logs = logs.as_array().ok_or_else(parse_error)?;
    if logs.len() > 10_000 {
        return Err(invalid_transaction());
    }
    logs.iter()
        .map(|line| line.as_str().map(str::to_owned).ok_or_else(parse_error))
        .collect()
}

fn simulation_inner_instructions(value: &Value) -> Result<Vec<Value>> {
    let Some(inner) = value
        .get("innerInstructions")
        .filter(|value| !value.is_null())
    else {
        return Ok(Vec::new());
    };
    let groups = inner.as_array().ok_or_else(parse_error)?;
    let mut total = 0usize;
    for group in groups {
        let count = group
            .get("instructions")
            .and_then(Value::as_array)
            .ok_or_else(parse_error)?
            .len();
        total = total.checked_add(count).ok_or_else(invalid_transaction)?;
        if group.get("index").and_then(Value::as_u64).is_none() {
            return Err(parse_error());
        }
    }
    if groups.len() > 1_000
        || total > 10_000
        || serde_json::to_vec(inner).map_or(true, |bytes| bytes.len() > 1_500_000)
    {
        return Err(invalid_transaction());
    }
    Ok(groups.clone())
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

async fn reconciliation_with_budget(
    work: impl std::future::Future<Output = Result<ReceiptOutcome>>,
) -> Result<ReceiptOutcome> {
    match tokio::time::timeout(RECONCILIATION_BUDGET, work).await {
        Ok(Err(error)) if error.code == "SOLANA_EXPIRY_COVERAGE_PENDING" => {
            Ok(ReceiptOutcome::Pending {
                confirmed: false,
                reason: error.message,
            })
        }
        Ok(Err(error))
            if matches!(
                error.code.as_str(),
                "SOLANA_RPC_UNAVAILABLE"
                    | "SOLANA_RPC_UNHEALTHY"
                    | "SOLANA_RPC_RATE_LIMITED"
                    | "SOLANA_RPC_STALE"
                    | "SOLANA_RPC_UNSUPPORTED"
                    | "SOLANA_RPC_HISTORY_UNAVAILABLE"
            ) =>
        {
            Ok(receipt_temporarily_pending())
        }
        Ok(result) => result,
        Err(_) => Ok(receipt_temporarily_pending()),
    }
}

fn receipt_temporarily_pending() -> ReceiptOutcome {
    ReceiptOutcome::Pending {
        confirmed: false,
        reason: "Receipt verification is temporarily unavailable or exceeded its read budget; keep the original transaction unresolved and reconcile it later.".into(),
    }
}

/// Version two retains the exact signed wire privately. Validate it before any
/// recovery RPC; older submissions retain their original identity checks.
fn validate_persisted_wire(data: &Value) -> Result<()> {
    let encoded = data
        .get("signed_transaction_base64")
        .and_then(Value::as_str)
        .ok_or_else(receipt_identity_error)?;
    let tx = decode_supported_transaction(encoded).map_err(|_| receipt_identity_error())?;
    let message = codec()
        .serialize(&tx.message)
        .map_err(|_| receipt_identity_error())?;
    let hash = hex::encode(Sha256::digest(&message));
    let required = usize::from(tx.message.header().num_required_signatures);
    let keys = tx.message.static_account_keys();
    let signers = data
        .get("required_signers")
        .and_then(Value::as_array)
        .ok_or_else(receipt_identity_error)?;
    let known = data
        .get("existing_signatures")
        .and_then(Value::as_array)
        .ok_or_else(receipt_identity_error)?;
    let missing = data
        .get("missing_cosigners")
        .and_then(Value::as_array)
        .ok_or_else(receipt_identity_error)?;
    if required == 0
        || tx.signatures.len() != required
        || signers.len() != required
        || known.len() != required
        || keys.len() < required
        || data.get("message_sha256").and_then(Value::as_str) != Some(hash.as_str())
        || data.get("recent_blockhash").and_then(Value::as_str)
            != Some(tx.message.recent_blockhash().to_string().as_str())
        || data.get("fee_payer").and_then(Value::as_str) != Some(keys[0].to_string().as_str())
    {
        return Err(receipt_identity_error());
    }
    let wallet = data
        .get("signer")
        .and_then(Value::as_str)
        .ok_or_else(receipt_identity_error)?;
    let mut wallet_seen = false;
    let mut missing_actual = std::collections::BTreeSet::new();
    for (index, signature) in tx.signatures.iter().enumerate() {
        let key = keys[index].to_string();
        if signers[index].as_str() != Some(key.as_str())
            || known[index].get("signer").and_then(Value::as_str) != Some(key.as_str())
        {
            return Err(receipt_identity_error());
        }
        let saved = known[index]
            .get("signature")
            .ok_or_else(receipt_identity_error)?;
        if signature == &Signature::default() {
            if !saved.is_null() || key == wallet {
                return Err(receipt_identity_error());
            }
            missing_actual.insert(key.clone());
        } else if saved.as_str() != Some(signature.to_string().as_str())
            || !signature.verify(keys[index].as_ref(), &message)
        {
            return Err(receipt_identity_error());
        }
        if key == wallet {
            if wallet_seen
                || data.get("wallet_signature").and_then(Value::as_str)
                    != Some(signature.to_string().as_str())
            {
                return Err(receipt_identity_error());
            }
            wallet_seen = true;
        }
    }
    let missing_saved = missing
        .iter()
        .map(Value::as_str)
        .collect::<Option<std::collections::BTreeSet<_>>>()
        .ok_or_else(receipt_identity_error)?;
    let first = tx
        .signatures
        .first()
        .filter(|sig| **sig != Signature::default())
        .map(ToString::to_string);
    if !wallet_seen
        || missing_saved.len() != missing.len()
        || missing_actual
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            != missing_saved
        || data.get("chain_signature").and_then(Value::as_str) != first.as_deref()
    {
        return Err(receipt_identity_error());
    }
    Ok(())
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

pub struct RpcTransactionSender {
    rpc: Arc<dyn SolanaRpc>,
}

impl RpcTransactionSender {
    pub fn new(rpc: Arc<dyn SolanaRpc>) -> Self {
        Self { rpc }
    }
}
#[async_trait]
impl TransactionSender for RpcTransactionSender {
    async fn send_signed_with_guard(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
        guard: &dyn AsyncDispatchGuard,
    ) -> Result<String> {
        self.rpc.prepare_send().await?;
        guard.check().await?;
        self.send_signed(signed_transaction_base64, frozen).await
    }

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
pub(crate) fn decode_supported_transaction(encoded: &str) -> Result<VersionedTransaction> {
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
    fn nullable_simulation_fee_falls_back_to_fee_for_message() {
        assert_eq!(
            simulation_fee_lamports(&json!({"fee": null})).unwrap(),
            None
        );
        assert_eq!(
            simulation_fee_lamports(&json!({"fee": 5000})).unwrap(),
            Some(5000)
        );
        assert!(simulation_fee_lamports(&json!({"fee": "5000"})).is_err());
    }

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

    #[test]
    fn failed_simulation_exposes_only_bounded_program_error_log_lines() {
        let failure = simulation_failed_with_program_detail(
            &json!({"InstructionError":[2,{"Custom":3005}]}),
            &json!({"logs":[
                "Program data: secret-like-opaque-payload",
                "Program log: AnchorError caused by account: bin_array. Error Code: AccountOwnedByWrongProgram. Error Number: 3005.",
                "Program log: arbitrary untrusted provider data"
            ]}),
            &[],
        );
        let rendered = failure.details.unwrap();
        assert_eq!(
            rendered["program_failure_summary"][0],
            "Program log: AnchorError caused by account: bin_array. Error Code: AccountOwnedByWrongProgram. Error Number: 3005."
        );
        assert!(
            rendered["program_failure_summary"]
                .as_array()
                .unwrap()
                .len()
                <= 6
        );
        assert!(!rendered.to_string().contains("secret-like-opaque-payload"));
        assert!(
            !rendered
                .to_string()
                .contains("arbitrary untrusted provider data")
        );
    }

    #[test]
    fn failed_simulation_preserves_bounded_cpi_indices_for_protocol_authentication() {
        let keys = vec!["DLMM111111111111111111111111111111111111".to_owned()];
        let failure = simulation_failed_with_program_detail(
            &json!({"InstructionError":[0,{"Custom":1}]}),
            &json!({
                "innerInstructions":[{"index":0,"instructions":[{
                    "programIdIndex":0,"accounts":[0],"data":"3Bxs4h" ,"stackHeight":2
                }]}]
            }),
            &keys,
        );
        let details = failure.details.unwrap();
        assert_eq!(details["simulation_evidence"]["account_keys"][0], keys[0]);
        assert_eq!(
            details["simulation_evidence"]["inner_instructions"][0]["index"],
            0
        );
        assert_eq!(
            details["simulation_evidence"]["inner_instructions"][0]["instructions"][0],
            json!({"program_id_index":0,"accounts":[0],"data":"3Bxs4h","stack_height":2})
        );

        let too_many_keys = vec!["key".to_owned(); 65];
        let capped = simulation_failed_with_program_detail(
            &json!({"InstructionError":[0,{"Custom":1}]}),
            &json!({"innerInstructions": []}),
            &too_many_keys,
        );
        assert!(capped.details.unwrap().get("simulation_evidence").is_none());
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
    async fn transfer_hook_preserves_canonical_meta_privileges_for_broker_validation() {
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

        let writable =
            ExtraAccountMeta::new_with_pubkey(&Pubkey::new_from_array([7; 32]), false, true)
                .unwrap();
        let runtime = transfer_hook_runtime(writable, &hook);
        let (mut instruction, _) = transfer_hook_instruction(&hook);
        runtime
            .resolve_transfer_hook_instruction(&mut instruction, &hook.to_string(), 123)
            .await
            .unwrap();
        assert!(instruction.accounts[4].writable);
        assert!(!instruction.accounts[4].signer);

        let signer =
            ExtraAccountMeta::new_with_pubkey(&Pubkey::new_from_array([7; 32]), true, false)
                .unwrap();
        let runtime = transfer_hook_runtime(signer, &hook);
        let (mut instruction, _) = transfer_hook_instruction(&hook);
        runtime
            .resolve_transfer_hook_instruction(&mut instruction, &hook.to_string(), 123)
            .await
            .unwrap();
        assert!(instruction.accounts[4].signer);
        assert!(!instruction.accounts[4].writable);

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
    fn transfer_hook_meta_count_uses_solana_account_index_limit() {
        assert!(transfer_hook_account_count_supported(4, 250));
        assert!(!transfer_hook_account_count_supported(4, 251));
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
    fn durable_missing_cosigners_excludes_broker_signatures_added_before_journaling() {
        let signatures = vec![
            Signature::from([1u8; 64]),
            Signature::from([2u8; 64]),
            Signature::default(),
        ];
        let required = vec!["wallet".into(), "position-mint".into(), "other".into()];
        assert_eq!(
            missing_signature_addresses(&signatures, &required, 0),
            vec!["other"]
        );
        assert!(missing_signature_addresses(&signatures[..2], &required, 0).is_empty());
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

    #[test]
    fn all_owner_token_deltas_reject_conflicting_decimals_for_one_mint() {
        let proof = FinalizedReceiptProof {
            signature: "signature".into(),
            slot: 1,
            fee_lamports: 0,
            failed: false,
            message_sha256: "00".repeat(32),
            account_keys: vec!["ata".into()],
            message_instructions: Vec::new(),
            required_signatures: Vec::new(),
            pre_balances_lamports: vec![0],
            post_balances_lamports: vec![0],
            pre_token_balances: vec![
                json!({"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100","decimals":6}}),
            ],
            post_token_balances: vec![
                json!({"accountIndex":0,"mint":"mint","owner":"wallet","programId":crate::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"100","decimals":9}}),
            ],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: Vec::new(),
            log_messages: Vec::new(),
            error: None,
        };
        assert!(proof.all_owner_token_deltas_verified("wallet").is_err());
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

    struct CapturingRpc {
        responses: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl SolanaRpc for CapturingRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests
                .lock()
                .unwrap()
                .push((method.to_owned(), params));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(parse_error)
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

    struct AcceptPartialSender {
        signed_wire: Mutex<Option<String>>,
        sends: AtomicUsize,
    }

    #[async_trait]
    impl TransactionSender for AcceptPartialSender {
        async fn send_signed(
            &self,
            signed_transaction_base64: &str,
            _frozen: &FrozenTransaction,
        ) -> gofin_domain::Result<String> {
            self.sends.fetch_add(1, Ordering::AcqRel);
            *self.signed_wire.lock().unwrap() = Some(signed_transaction_base64.to_owned());
            let bytes = STANDARD
                .decode(signed_transaction_base64)
                .map_err(|_| parse_error())?;
            let transaction: VersionedTransaction =
                codec().deserialize(&bytes).map_err(|_| parse_error())?;
            transaction
                .signatures
                .first()
                .filter(|signature| signature.as_ref().iter().any(|byte| *byte != 0))
                .map(ToString::to_string)
                .ok_or_else(parse_error)
        }
    }

    struct AcceptJournal;

    impl SubmissionJournal for AcceptJournal {
        fn assert_authorized(&self) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn assert_candidate(&self, _prepared: &PreparedOperation) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn persist_submission(&self, _submission: &Submission) -> gofin_domain::Result<()> {
            Ok(())
        }

        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            _evidence: &Value,
        ) -> gofin_domain::Result<()> {
            Ok(())
        }
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

    pub(super) fn signer() -> SolanaSigner {
        let key = ed25519_dalek::SigningKey::from_bytes(&[19u8; 32]);
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&[19u8; 32]);
        bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
        SolanaSigner::parse(&bs58::encode(bytes).into_string()).unwrap()
    }

    pub(super) fn signer_from_seed(seed: [u8; 32]) -> SolanaSigner {
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&seed);
        bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
        SolanaSigner::parse(&bs58::encode(bytes).into_string()).unwrap()
    }

    pub(super) fn signed_wire(signer: &SolanaSigner) -> (VersionedTransaction, String) {
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

    #[tokio::test]
    async fn provider_sender_can_land_only_the_frozen_approved_partial_transaction() {
        let wallet = signer();
        let external = signer_from_seed([20u8; 32]);
        let payer = wallet.address().parse::<Address>().unwrap();
        let external_key = external.address().parse::<Address>().unwrap();
        let instruction = Instruction {
            program_id: "11111111111111111111111111111111"
                .parse::<Address>()
                .unwrap(),
            accounts: vec![AccountMeta::new_readonly(external_key, true)],
            data: vec![0],
        };
        let message = VersionedMessage::Legacy(Message::new_with_blockhash(
            &[instruction],
            Some(&payer),
            &Hash::default(),
        ));
        let signature_count = usize::from(message.header().num_required_signatures);
        assert_eq!(signature_count, 2);
        let wire = STANDARD.encode(
            codec()
                .serialize(&VersionedTransaction {
                    signatures: vec![Signature::default(); signature_count],
                    message,
                })
                .unwrap(),
        );
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            json!({"context":{"slot":1},"value":true}),
            json!({"context":{"slot":2},"value":true}),
            json!({"context":{"slot":3},"value":true}),
            json!({"context":{"slot":4},"value":true}),
        ])))));
        let frozen = runtime
            .freeze_transaction_with_expiry(
                &wire,
                &wallet,
                None,
                None,
                10_000,
                Some("provider-approved-partial".into()),
                &[external.address().to_owned()],
            )
            .await
            .unwrap();
        assert_eq!(frozen.missing_cosigners(), &[external.address().to_owned()]);
        let prepared = PreparedOperation {
            operation: "provider.order".into(),
            input: json!({}),
            binding: json!({"message_sha256":frozen.message_sha256()}),
            summary: "synthetic provider landing".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: gofin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let journal = Arc::new(AcceptJournal);
        let sender = AcceptPartialSender {
            signed_wire: Mutex::new(None),
            sends: AtomicUsize::new(0),
        };

        let direct = runtime
            .dispatch(&frozen, &wallet, &prepared, journal.clone())
            .await
            .unwrap_err();
        assert_eq!(direct.code, "SOLANA_COSIGNER_LANDING_REQUIRED");
        let unbound_auxiliary = runtime
            .dispatch_with_secondary_signers(
                &frozen,
                &wallet,
                &[],
                &prepared,
                journal.clone(),
                &sender,
            )
            .await
            .unwrap_err();
        assert_eq!(unbound_auxiliary.code, "SOLANA_COSIGNER_BINDING_MISMATCH");
        assert_eq!(sender.sends.load(Ordering::Acquire), 0);

        let submission = runtime
            .dispatch_with_sender(&frozen, &wallet, &prepared, journal, &sender)
            .await
            .unwrap();
        assert_eq!(sender.sends.load(Ordering::Acquire), 1);
        assert_eq!(submission.data["missing_cosigners"][0], external.address());
        let signed = STANDARD
            .decode(sender.signed_wire.lock().unwrap().as_ref().unwrap())
            .unwrap();
        let signed: VersionedTransaction = codec().deserialize(&signed).unwrap();
        assert!(signed.signatures[0].as_ref().iter().any(|byte| *byte != 0));
        assert!(signed.signatures[1].as_ref().iter().all(|byte| *byte == 0));
    }

    fn finalized_fixture(encoded: &str) -> VecDeque<Value> {
        VecDeque::from([
            json!({"value":[{"confirmationStatus":"finalized","err":null,"slot":99}]}),
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
            json!({"value":{"err":null,"unitsConsumed":1234,"fee":5000,"accounts":[
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
    async fn simulation_requests_and_returns_authenticated_inner_instruction_context() {
        let signer = signer();
        let (mut tx, _) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&tx).unwrap());
        let rpc = Arc::new(CapturingRpc {
            responses: Mutex::new(VecDeque::from([
                json!({"context":{"slot":5},"value":true}),
                json!({"value":{"err":null,"unitsConsumed":1200,"accounts":[null],
                    "innerInstructions":[{"index":0,"instructions":[{"programIdIndex":2,"accounts":[0,1],"data":"AQ=="}]}]}}),
                json!({"value":10_000}),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let frozen = runtime
            .freeze_transaction_with_expiry(&wire, &signer, None, None, 10_000, None, &[])
            .await
            .unwrap();
        let result = runtime
            .simulate_with_accounts(&frozen, &[signer.address().to_owned()])
            .await
            .unwrap();

        assert_eq!(result.account_keys, frozen.account_keys());
        assert_eq!(result.fee_lamports, Some(10_000));
        assert_eq!(result.inner_instructions.len(), 1);
        assert_eq!(
            result.inner_instructions[0]["instructions"][0]["programIdIndex"],
            2
        );
        let requests = rpc.requests.lock().unwrap();
        assert_eq!(requests[1].0, "simulateTransaction");
        assert_eq!(requests[1].1[1]["innerInstructions"], true);
        assert_eq!(requests[2].0, "getFeeForMessage");
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

    #[derive(Default)]
    struct DurableJournal(Mutex<Option<Submission>>);
    impl SubmissionJournal for DurableJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, submission: &Submission) -> Result<()> {
            let mut saved = self.0.lock().unwrap();
            if saved.is_some() {
                return Err(receipt_identity_error());
            }
            *saved = Some(submission.clone());
            Ok(())
        }
        fn persist_dispatch_evidence(
            &self,
            _submission: &Submission,
            _evidence: &Value,
        ) -> Result<()> {
            Ok(())
        }
    }
    struct PreparationFailureRpc {
        error: GoFinError,
        preparations: AtomicUsize,
        sends: AtomicUsize,
    }
    #[async_trait]
    impl SolanaRpc for PreparationFailureRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            if method == "sendTransaction" {
                self.sends.fetch_add(1, Ordering::AcqRel);
                panic!("preparation failure must prevent financial dispatch");
            }
            assert_eq!(method, "isBlockhashValid");
            Ok(json!({"context":{"slot":99},"value":true}))
        }
        async fn prepare_send(&self) -> Result<()> {
            self.preparations.fetch_add(1, Ordering::AcqRel);
            Err(self.error.clone())
        }
    }

    #[tokio::test]
    async fn direct_primary_preparation_failure_leaves_no_submission_and_preserves_error() {
        let signer = signer();
        let (mut transaction, _) = signed_wire(&signer);
        transaction.signatures[0] = Signature::default();
        let unsigned = STANDARD.encode(codec().serialize(&transaction).unwrap());
        for (code, stage) in [
            ("SOLANA_NETWORK_MISMATCH", "genesis"),
            ("SOLANA_RPC_RATE_LIMITED", "http_status"),
            ("SOLANA_RPC_UNAVAILABLE", "transport_timeout"),
        ] {
            let expected = GoFinError::new(
                code,
                "rpc",
                "Synthetic primary preparation failed.",
                "Retry preparation.",
            )
            .with_details(json!({"stage":stage}));
            let rpc = Arc::new(PreparationFailureRpc {
                error: expected.clone(),
                preparations: AtomicUsize::new(0),
                sends: AtomicUsize::new(0),
            });
            let runtime = SolanaRuntime::new(rpc.clone());
            let frozen = runtime
                .freeze_transaction_with_expiry(&unsigned, &signer, None, None, 10000, None, &[])
                .await
                .unwrap();
            let prepared = PreparedOperation {
                operation: "wallet.transfer".into(),
                input: json!({}),
                binding: json!({"message_sha256":frozen.message_sha256()}),
                summary: "synthetic primary preparation failure".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: gofin_domain::now(),
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                valuation: None,
                intent: gofin_domain::FinancialIntent::default(),
                action_deadline: None,
                operation_version: 1,
            };
            let journal = Arc::new(DurableJournal::default());
            let error = runtime
                .dispatch(&frozen, &signer, &prepared, journal.clone())
                .await
                .unwrap_err();
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            assert!(
                journal.0.lock().unwrap().is_none(),
                "known-unsent preparation must not retain a pending identity"
            );
            assert_eq!(rpc.preparations.load(Ordering::Acquire), 1);
            assert_eq!(rpc.sends.load(Ordering::Acquire), 0);
        }
    }

    struct ResponseLossSender {
        journal: Arc<DurableJournal>,
        sends: AtomicUsize,
        error: GoFinError,
    }
    #[async_trait]
    impl TransactionSender for ResponseLossSender {
        async fn send_signed(&self, encoded: &str, _frozen: &FrozenTransaction) -> Result<String> {
            let saved = self
                .journal
                .0
                .lock()
                .unwrap()
                .clone()
                .expect("persist before send");
            assert_eq!(saved.data["schema_version"], 2);
            assert_eq!(saved.data["signed_transaction_base64"], encoded);
            validate_persisted_wire(&saved.data).unwrap();
            self.sends.fetch_add(1, Ordering::AcqRel);
            Err(self.error.clone())
        }
    }

    #[tokio::test]
    async fn response_loss_persists_wire_before_dispatch_and_restart_does_not_resend() {
        let signer = signer();
        let (mut tx, signed) = signed_wire(&signer);
        tx.signatures[0] = Signature::default();
        let unsigned = STANDARD.encode(codec().serialize(&tx).unwrap());
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from(vec![
            json!({"context":{"slot":99},"value":true});
            10
        ])))));
        let frozen = runtime
            .freeze_transaction_with_expiry(&unsigned, &signer, None, None, 10000, None, &[])
            .await
            .unwrap();
        let prepared = PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({}),
            binding: json!({"message_sha256":frozen.message_sha256()}),
            summary: "synthetic response loss".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: gofin_domain::now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent::default(),
            action_deadline: None,
            operation_version: 1,
        };
        let journal = Arc::new(DurableJournal::default());
        let sender = ResponseLossSender {
            journal: journal.clone(),
            sends: AtomicUsize::new(0),
            error: GoFinError::new(
                "SOLANA_RPC_RATE_LIMITED",
                "rpc",
                "private-provider-message",
                "https://private-provider.example/key",
            )
            .with_details(json!({
                "stage":"http_status", "http_status":429,
                "body":"private-provider-body", "signed_transaction_base64":signed,
                "no_network_send":true
            })),
        };
        let error = runtime
            .dispatch_with_sender(&frozen, &signer, &prepared, journal.clone(), &sender)
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_SUBMISSION_UNCERTAIN");
        assert_eq!(
            error.details.as_deref().unwrap(),
            &json!({"sender_cause":{"code":"SOLANA_RPC_RATE_LIMITED","stage":"http_status","http_status":429}})
        );
        let submission = journal.0.lock().unwrap().clone().unwrap();
        let restarted =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(finalized_fixture(&signed)))));
        assert!(matches!(
            restarted
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::Finalized { .. }
        ));
        assert!(
            runtime
                .dispatch_with_sender(&frozen, &signer, &prepared, journal, &sender)
                .await
                .is_err()
        );
        assert_eq!(sender.sends.load(Ordering::Acquire), 1);
    }

    #[test]
    fn uncertain_sender_diagnostics_retain_only_validated_codes_and_scalar_fields() {
        let cases = [
            (
                "SOLANA_RPC_UNAVAILABLE",
                json!({"stage":"rpc_error","http_status":200,"rpc_code":-32002,"message":"private-message","data":{"logs":["private-log"]},"signed_transaction_base64":"private-wire","url":"private-url"}),
                json!({"code":"SOLANA_RPC_UNAVAILABLE","stage":"rpc_error","http_status":200,"rpc_code":-32002}),
            ),
            (
                "SOLANA_RPC_UNAVAILABLE",
                json!({"stage":"transport_timeout","rpc_health":{"endpoint":"private-url"},"no_network_send":true}),
                json!({"code":"SOLANA_RPC_UNAVAILABLE","stage":"transport_timeout"}),
            ),
            (
                "SOLANA_RPC_RATE_LIMITED",
                json!({"stage":"primary_cooldown","retry_after_ms":123,"no_network_send":true,"url":"private-url"}),
                json!({"code":"SOLANA_RPC_RATE_LIMITED","stage":"primary_cooldown"}),
            ),
            (
                "SOLANA_RPC_RATE_LIMITED",
                json!({"stage":"private-stage","http_status":600,"rpc_code":123456789}),
                json!({"code":"SOLANA_RPC_RATE_LIMITED"}),
            ),
            (
                "SOLANA_RPC_UNAVAILABLE",
                json!({"stage":["transport"],"http_status":"429","rpc_code":"-32002"}),
                json!({"code":"SOLANA_RPC_UNAVAILABLE"}),
            ),
            (
                "SOLANA_RPC_UNAVAILABLE",
                json!({"stage":{"value":"http_status"},"http_status":-1,"rpc_code":1.5}),
                json!({"code":"SOLANA_RPC_UNAVAILABLE"}),
            ),
            (
                "SOLANA_RPC_UNAVAILABLE",
                json!([{"stage":"rpc_error","rpc_code":-32002}]),
                json!({"code":"SOLANA_RPC_UNAVAILABLE"}),
            ),
            (
                "private-code",
                json!({"stage":"rpc_error","http_status":429,"rpc_code":-32002}),
                json!({"code":"UNCLASSIFIED_SENDER_ERROR"}),
            ),
        ];
        for (code, details, expected) in cases {
            let cause = GoFinError::new(
                code,
                "private-error-stage",
                "private-message",
                "private-next-action",
            )
            .with_details(details);
            let error = submission_uncertain(&cause);
            assert_eq!(error.code, "SOLANA_SUBMISSION_UNCERTAIN");
            assert_eq!(
                error.details.as_deref().unwrap(),
                &json!({"sender_cause":expected})
            );
            let public = serde_json::to_string(&error).unwrap();
            assert!(!public.contains("private-"));
            assert!(!public.contains("no_network_send"));
            assert!(public.len() < 600);
        }
    }

    pub(super) fn v2_submission(
        signer: &SolanaSigner,
        tx: &VersionedTransaction,
        wire: &str,
    ) -> Submission {
        let signature = tx.signatures[0].to_string();
        let hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        Submission {
            kind: "solana.transaction".into(),
            reference: canonical_message_reference(&hash),
            data: json!({"schema_version":2,"signed_transaction_base64":wire,"message_sha256":hash,"recent_blockhash":tx.message.recent_blockhash().to_string(),"last_valid_block_height":100,"chain_signature":signature,"wallet_signature":signature,"signer":signer.address(),"fee_payer":signer.address(),"missing_cosigners":[],"required_signers":[signer.address()],"existing_signatures":[{"signer":signer.address(),"signature":signature}]}),
        }
    }

    #[tokio::test]
    async fn v2_restart_verifies_private_wire_and_rejects_tampering_before_rpc() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let submission = v2_submission(&signer, &tx, &wire);
        let runtime =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(finalized_fixture(&wire)))));
        assert!(matches!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::Finalized { .. }
        ));
        let no_send = json!({"no_network_send":true,"reason_code":"APPROVAL_EXPIRED","message_sha256":submission.data["message_sha256"]});
        let no_rpc = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::new()))));
        assert!(matches!(
            no_rpc
                .reconcile_submission(&submission, Some(&no_send))
                .await
                .unwrap(),
            ReceiptOutcome::NotDispatched { .. }
        ));
        for (field, replacement) in [
            ("signed_transaction_base64", json!("invalid-wire")),
            ("message_sha256", json!("00")),
            ("wallet_signature", json!(Signature::default().to_string())),
            ("recent_blockhash", json!("different")),
            ("fee_payer", json!("different")),
            ("chain_signature", Value::Null),
            ("required_signers", json!(["different"])),
            ("missing_cosigners", json!(["different"])),
        ] {
            let mut corrupt = submission.clone();
            corrupt.data[field] = replacement;
            let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::new()))));
            assert_eq!(
                runtime
                    .reconcile_submission(&corrupt, None)
                    .await
                    .unwrap_err()
                    .code,
                "SOLANA_RECEIPT_IDENTITY_MISMATCH",
                "{field}"
            );
            assert_eq!(
                runtime
                    .reconcile_submission(&corrupt, Some(&no_send))
                    .await
                    .unwrap_err()
                    .code,
                "SOLANA_RECEIPT_IDENTITY_MISMATCH",
                "no-network-send must not bypass {field} verification"
            );
        }
    }

    #[tokio::test]
    async fn finalized_transaction_resolves_an_absent_signature_status_index() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let submission = v2_submission(&signer, &tx, &wire);
        let mut responses = finalized_fixture(&wire);
        responses[0] = json!({"context":{"slot":100},"value":[null]});
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(responses))));
        assert!(matches!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::Finalized { .. }
        ));
    }

    #[tokio::test]
    async fn conflicting_finalized_status_slot_and_metadata_cannot_settle() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let hash = hex::encode(Sha256::digest(codec().serialize(&tx.message).unwrap()));
        for field in ["slot", "err"] {
            let mut responses = finalized_fixture(&wire);
            responses[0]["value"][0][field] = if field == "slot" {
                json!(98)
            } else {
                json!({"InstructionError":[0,"Custom"]})
            };
            let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(responses))));
            assert_eq!(
                runtime
                    .reconcile(&tx.signatures[0].to_string(), &hash)
                    .await
                    .unwrap_err()
                    .code,
                "SOLANA_RECEIPT_IDENTITY_MISMATCH"
            );
        }
    }

    #[tokio::test]
    async fn unavailable_receipt_categories_stay_pending_but_conflicts_remain_errors() {
        for code in [
            "SOLANA_RPC_UNAVAILABLE",
            "SOLANA_RPC_RATE_LIMITED",
            "SOLANA_RPC_STALE",
            "SOLANA_RPC_UNSUPPORTED",
            "SOLANA_RPC_HISTORY_UNAVAILABLE",
            "SOLANA_RPC_CONFLICT",
            "SOLANA_RPC_DATA_INVALID",
        ] {
            let result = reconciliation_with_budget(async {
                Err(GoFinError::new(
                    code,
                    "rpc",
                    "Synthetic receipt read failure.",
                    "Reconcile.",
                ))
            })
            .await;
            if matches!(code, "SOLANA_RPC_CONFLICT" | "SOLANA_RPC_DATA_INVALID") {
                assert_eq!(result.unwrap_err().code, code);
            } else {
                assert!(
                    matches!(result.unwrap(), ReceiptOutcome::Pending { .. }),
                    "{code}"
                );
            }
        }
    }

    struct ActiveCandidateRead<'a>(&'a AtomicUsize);
    impl Drop for ActiveCandidateRead<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    struct ConcurrentCandidateRpc {
        target: String,
        unrelated: &'static str,
        wire: String,
        active: AtomicUsize,
        unrelated_reads: AtomicUsize,
    }
    #[async_trait]
    impl SolanaRpc for ConcurrentCandidateRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let unrelated_signature = Signature::from([55u8; 64]).to_string();
            match method {
                "getSignaturesForAddress" => {
                    Ok(json!([{"signature":unrelated_signature},{"signature":self.target}]))
                }
                "getSignatureStatuses" if params[0][0] == unrelated_signature => {
                    self.unrelated_reads.fetch_add(1, Ordering::AcqRel);
                    match self.unrelated {
                        "stall" => {
                            self.active.fetch_add(1, Ordering::AcqRel);
                            let _active = ActiveCandidateRead(&self.active);
                            tokio::time::sleep(std::time::Duration::from_secs(25)).await;
                            Ok(json!({"value":[null]}))
                        }
                        "conflict" => Err(GoFinError::new(
                            "SOLANA_RPC_CONFLICT",
                            "rpc",
                            "Synthetic finalized conflict.",
                            "Keep unresolved.",
                        )),
                        "missing" => Ok(json!({"value":[null]})),
                        _ => unreachable!(),
                    }
                }
                "getSignatureStatuses" => {
                    assert_eq!(params[0][0], self.target);
                    Ok(finalized_fixture(&self.wire)[0].clone())
                }
                "getTransaction" if params[0] == unrelated_signature => Ok(Value::Null),
                "getTransaction" => {
                    assert_eq!(params[0], self.target);
                    Ok(finalized_fixture(&self.wire)[1].clone())
                }
                _ => panic!("unexpected candidate RPC"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sponsored_search_bypasses_unrelated_slow_or_missing_candidate_and_cancels_peers() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let mut submission = v2_submission(&signer, &tx, &wire);
        submission.data["schema_version"] = json!(1);
        submission.data["chain_signature"] = Value::Null;
        for unrelated in ["stall", "missing"] {
            let rpc = Arc::new(ConcurrentCandidateRpc {
                target: tx.signatures[0].to_string(),
                unrelated,
                wire: wire.clone(),
                active: AtomicUsize::new(0),
                unrelated_reads: AtomicUsize::new(0),
            });
            let runtime = SolanaRuntime::new(rpc.clone());
            let start = tokio::time::Instant::now();
            assert!(matches!(
                runtime
                    .reconcile_submission(&submission, None)
                    .await
                    .unwrap(),
                ReceiptOutcome::Finalized { .. }
            ));
            assert!(start.elapsed() < RECONCILIATION_BUDGET);
            assert_eq!(rpc.unrelated_reads.load(Ordering::Acquire), 1);
            assert_eq!(
                rpc.active.load(Ordering::Acquire),
                0,
                "pending reads must cancel after exact proof"
            );
        }
    }

    #[tokio::test]
    async fn sponsored_candidate_conflict_fails_closed() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let mut submission = v2_submission(&signer, &tx, &wire);
        submission.data["schema_version"] = json!(1);
        submission.data["chain_signature"] = Value::Null;
        let rpc = Arc::new(ConcurrentCandidateRpc {
            target: tx.signatures[0].to_string(),
            unrelated: "conflict",
            wire,
            active: AtomicUsize::new(0),
            unrelated_reads: AtomicUsize::new(0),
        });
        let runtime = SolanaRuntime::new(rpc);
        assert_eq!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RPC_CONFLICT"
        );
    }

    struct SlowHistoryRpc {
        signature: String,
        statuses: AtomicUsize,
    }
    #[async_trait]
    impl SolanaRpc for SlowHistoryRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            if method == "getSignaturesForAddress" {
                return Ok(json!(
                    (0..100)
                        .map(|_| json!({"signature":self.signature}))
                        .collect::<Vec<_>>()
                ));
            }
            if method == "getTransaction" {
                return Ok(Value::Null);
            }
            assert_eq!(method, "getSignatureStatuses");
            self.statuses.fetch_add(1, Ordering::AcqRel);
            tokio::time::sleep(std::time::Duration::from_secs(9)).await;
            Ok(json!({"value":[null]}))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sponsored_history_has_one_budget_across_all_candidate_receipts() {
        let signer = signer();
        let (tx, wire) = signed_wire(&signer);
        let mut submission = v2_submission(&signer, &tx, &wire);
        submission.data["schema_version"] = json!(1);
        submission.data["chain_signature"] = Value::Null;
        let rpc = Arc::new(SlowHistoryRpc {
            signature: tx.signatures[0].to_string(),
            statuses: AtomicUsize::new(0),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let start = tokio::time::Instant::now();
        assert!(matches!(
            runtime
                .reconcile_submission(&submission, None)
                .await
                .unwrap(),
            ReceiptOutcome::Pending { .. }
        ));
        assert_eq!(start.elapsed(), RECONCILIATION_BUDGET);
        assert_eq!(rpc.statuses.load(Ordering::Acquire), 6);
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
            json!({"value":[{"confirmationStatus":"finalized","err":null,"slot":99}]}),
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
        for schema_version in [1, 2] {
            let mut submission = submission.clone();
            if schema_version == 2 {
                submission.data["schema_version"] = json!(2);
                submission.data["signed_transaction_base64"] = json!(unsigned_wire);
                submission.data["fee_payer"] = json!(sponsor.to_string());
                submission.data["chain_signature"] = Value::Null;
            }
            let rpc = Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
                history.clone(),
                json!({"value":[{"confirmationStatus":"finalized","err":null,"slot":101}]}),
                details.clone(),
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
        }
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
    async fn trusted_provider_freeze_does_not_resolve_lookup_tables_or_simulate() {
        let signer = signer();
        let (mut message, _, _) = v0_lookup_fixture();
        if let VersionedMessage::V0(message) = &mut message {
            message.account_keys[0] = signer.address().parse().unwrap();
        }
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default()],
            message,
        };
        let wire = STANDARD.encode(codec().serialize(&transaction).unwrap());
        let rpc = Arc::new(CapturingRpc {
            responses: Mutex::new(VecDeque::from([json!(100)])),
            requests: Mutex::new(Vec::new()),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let frozen = runtime
            .freeze_provider_transaction_with_expiry(
                &wire,
                &signer,
                Some(110),
                None,
                10_000,
                Some("trusted-order".into()),
                &[],
            )
            .await
            .unwrap();
        assert!(frozen.instructions().is_empty());
        assert!(frozen.account_keys().is_empty());
        assert_eq!(
            frozen.message_bytes,
            codec().serialize(&transaction.message).unwrap()
        );
        assert_eq!(frozen.provider_request_id(), Some("trusted-order"));
        let requests = rpc.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "getBlockHeight");
    }

    #[tokio::test]
    async fn trusted_provider_freeze_still_binds_wallet_and_expiry() {
        let signer = signer();
        let (mut transaction, _) = signed_wire(&signer);
        transaction.signatures[0] = Signature::default();
        let wire = STANDARD.encode(codec().serialize(&transaction).unwrap());
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::new()))));
        let other = signer_from_seed([42; 32]);
        let error = runtime
            .freeze_provider_transaction_with_expiry(
                &wire,
                &other,
                Some(110),
                None,
                10_000,
                None,
                &[],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_SIGNER_NOT_REQUIRED");
        let error = runtime
            .freeze_provider_transaction_with_expiry(
                &wire,
                &signer,
                Some(110),
                Some("2020-01-01T00:00:00Z"),
                10_000,
                None,
                &[],
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_TX_EXPIRED");
    }

    #[tokio::test]
    async fn v0_finalized_loaded_addresses_are_bound_to_the_lookup_table() {
        let (message, table_key, table_data) = v0_lookup_fixture();
        let response = json!({"context":{"slot":99},"value":{"owner":ALT_PROGRAM_ID,"data":[table_data,"base64"]}});
        let runtime = SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([
            response.clone(),
        ])))));
        runtime
            .verify_finalized_loaded_addresses(
                &message,
                &json!({"writable":[Address::new_from_array([84u8;32]).to_string()],"readonly":[]}),
                99,
            )
            .await
            .unwrap();

        let runtime =
            SolanaRuntime::new(Arc::new(FixtureRpc(Mutex::new(VecDeque::from([response])))));
        let error = runtime
            .verify_finalized_loaded_addresses(
                &message,
                &json!({"writable":[Address::new_from_array([85u8;32]).to_string()],"readonly":[]}),
                99,
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
