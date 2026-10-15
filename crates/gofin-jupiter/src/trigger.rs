//! Noncustodial Jupiter Trigger V1 (Limit Order V2) lifecycle.
//!
//! Trigger API transactions are proposals. Placement settles as an escrow
//! transfer, and a later fill remains a separate on-chain lifecycle event.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration as ChronoDuration, Utc};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata,
    CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor, FinancialEffect,
    FinancialIntent, FinancialState, GoFinError, OperationKind, OperationRecord, PreparedOperation,
    Reconciliation, ResourceAmount, ResourceId, Result, SnapshotScope, StateSnapshot, Status,
    Submission, SubmissionJournal, Valuation,
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use gofin_pricing::UsdPriceSource;
use gofin_solana::{
    CLUSTER, NETWORK, SolanaWalletAdapter,
    transaction::{FrozenTransaction, ReceiptOutcome, TransactionSender},
};
use reqwest::Method;
use serde_json::{Value, json};
use solana_pubkey3::Pubkey;

use crate::{
    adapter::{self, token_decimals},
    api::{JupiterApi, JupiterHost, RateBucket, RequestPriority},
};

const CREATE: &str = "jupiter.limit.create";
const CANCEL: &str = "jupiter.limit.cancel";
const ORDERS: &str = "jupiter.limit.orders";
const STATUS: &str = "jupiter.limit.status";
const PROGRAM: &str = "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X";
const LIMIT_ORDER_DISC: [u8; 8] = [134, 173, 223, 185, 77, 86, 28, 51];
const INITIALIZE_DISC: [u8; 8] = [133, 110, 74, 175, 112, 159, 245, 159];
const CANCEL_DISC: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];
const TRADE_EVENT_DISC: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];
const CANCEL_EVENT_DISC: [u8; 8] = [174, 66, 141, 17, 4, 224, 162, 77];
const CREATE_EVENT_DISC: [u8; 8] = [49, 142, 72, 166, 230, 29, 84, 84];
const SYSTEM: &str = "11111111111111111111111111111111";
const ASSOCIATED: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const COMPUTE: &str = "ComputeBudget111111111111111111111111111111";
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const TOKEN_2022: &str = gofin_solana::token::TOKEN_2022_PROGRAM_ID;
const NATIVE: &str = "So11111111111111111111111111111111111111112";
const DEADLINE: Duration = Duration::from_secs(20);

fn append_snapshot_resource_identity(coverage: &mut Vec<String>, resource: &ResourceId) {
    if !coverage.iter().any(|entry| entry == &resource.0) {
        coverage.push(resource.0.clone());
    }
}

fn wallet_name(input: &Value) -> Result<String> {
    input
        .get("wallet")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| GoFinError::validation("Select an enrolled Solana wallet."))
}

pub struct TriggerAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    api: Arc<dyn JupiterApi>,
    prices: Arc<dyn UsdPriceSource>,
    execution_state: Arc<TriggerExecutionState>,
}

#[derive(Default)]
struct TriggerExecutionState(
    std::sync::Mutex<Option<Arc<dyn gofin_domain::execution_state::ExecutionState>>>,
);

impl TriggerExecutionState {
    fn get(&self) -> Result<Arc<dyn gofin_domain::execution_state::ExecutionState>> {
        self.0
            .lock()
            .map_err(|_| GoFinError::validation("Jupiter Trigger execution-state lock failed."))?
            .clone()
            .ok_or_else(|| {
                GoFinError::new(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "journal",
                    "Jupiter Trigger is not attached to its durable execution state.",
                    "Start the matching GoFin broker before recovering an order.",
                )
            })
    }
}

impl TriggerAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
    ) -> Result<Self> {
        let rpc = wallets
            .values()
            .next()
            .ok_or_else(|| {
                GoFinError::validation("Jupiter Trigger requires an enrolled Solana wallet.")
            })?
            .runtime()
            .rpc()
            .clone();
        let prices: Arc<dyn UsdPriceSource> =
            Arc::new(crate::JupiterUsdPrices::new(api.clone(), rpc));
        Self::with_price_source(wallets, api, prices)
    }

    pub fn with_price_source(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        if wallets.is_empty() {
            return Err(GoFinError::validation(
                "Jupiter Trigger requires an enrolled Solana wallet.",
            ));
        }
        Ok(Self {
            wallets,
            api,
            prices,
            execution_state: Arc::new(TriggerExecutionState::default()),
        })
    }

    fn cancellation_parent(&self, input: &Value) -> Result<(String, Submission)> {
        let operation_id = input
            .get("original_operation_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                GoFinError::validation(
                    "Bounded Trigger cancellation requires the exact original operation identity.",
                )
            })?;
        let parent = self
            .execution_state
            .get()?
            .operation(operation_id)?
            .ok_or_else(|| GoFinError::validation("Original Trigger placement was not found."))?;
        if parent.operation != CREATE {
            return Err(GoFinError::validation(
                "Trigger cancellation must reference an original placement operation.",
            ));
        }
        if !matches!(
            parent.status,
            Status::Pending | Status::Submitted | Status::Unknown
        ) {
            return Err(GoFinError::validation(
                "Trigger placement is already terminal and cannot be cancelled as an unresolved order.",
            ));
        }
        let binding = parent
            .preview
            .as_ref()
            .map(|preview| &preview.binding)
            .ok_or_else(|| {
                GoFinError::validation("Original Trigger placement binding is missing.")
            })?;
        let parent_wallet = binding
            .get("wallet_address")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                GoFinError::validation("Original Trigger placement wallet is missing.")
            })?;
        let parent_order = binding
            .get("order_key")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                GoFinError::validation("Original Trigger placement order is missing.")
            })?;
        let input_order = input
            .get("order_key")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select the exact Trigger order key."))?;
        let selected_wallet = self.wallet(input)?;
        if parent_wallet != selected_wallet.address() || parent_order != input_order {
            return Err(GoFinError::validation(
                "Trigger cancellation identity does not match the saved placement wallet and order.",
            ));
        }
        let saved_submission = parent.submission.ok_or_else(|| {
            GoFinError::validation("Original Trigger placement has no saved submission.")
        })?;
        if let Some(echo) = input
            .get("original_submission")
            .filter(|value| !value.is_null())
        {
            let echo: Submission = serde_json::from_value(echo.clone()).map_err(|_| {
                GoFinError::validation("Original Trigger submission echo is malformed.")
            })?;
            if echo != saved_submission {
                return Err(GoFinError::validation(
                    "Trigger cancellation submission does not match the durable placement.",
                ));
            }
        }
        Ok((operation_id.to_owned(), saved_submission))
    }

    fn validate_order_cancellation_origin(
        &self,
        input: &Value,
        wallet: &SolanaWalletAdapter,
        order_key: &str,
    ) -> Result<()> {
        let candidates = self.execution_state.get()?.operations_by_prepared_binding(
            CREATE,
            "/preview/binding/order_key",
            &json!(order_key),
        )?;
        if candidates.len() > 1 {
            return Err(GoFinError::validation(
                "More than one durable Trigger placement claims this order identity; cancellation must remain unresolved until the journal is repaired.",
            ));
        }
        let Some(parent) = candidates.first() else {
            return Ok(());
        };
        let parent_wallet = parent
            .preview
            .as_ref()
            .and_then(|preview| preview.binding.get("wallet_address"))
            .and_then(Value::as_str);
        if parent_wallet != Some(wallet.address()) {
            return Err(unsafe_tx());
        }
        if input.get("original_operation_id").and_then(Value::as_str) != Some(parent.id.as_str()) {
            return Err(GoFinError::validation(
                "This Trigger order was created by GoFin and must be cancelled through its exact original operation identity.",
            ));
        }
        Ok(())
    }

    async fn cancellation_parent_funding(
        &self,
        input: &Value,
    ) -> Result<Vec<adapter::AccountFundingRequirement>> {
        let operation_id = input
            .get("original_operation_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GoFinError::validation("Select the original Trigger placement."))?;
        let parent = self
            .execution_state
            .get()?
            .operation(operation_id)?
            .ok_or_else(|| GoFinError::validation("Original Trigger placement was not found."))?;
        let binding = parent
            .preview
            .as_ref()
            .map(|preview| &preview.binding)
            .ok_or_else(bad_response)?;
        let mut requirements: Vec<adapter::AccountFundingRequirement> = serde_json::from_value(
            binding
                .get("funding_accounts")
                .cloned()
                .ok_or_else(bad_response)?,
        )
        .map_err(|_| bad_response())?;
        let wallet = self.wallet(input)?;
        let submission = parent.submission.as_ref().ok_or_else(bad_response)?;
        let outcome = wallet
            .runtime()
            .reconcile_submission(submission, parent.dispatch_evidence.as_ref())
            .await?;
        let proof = match outcome {
            ReceiptOutcome::Finalized {
                proof,
                failed: false,
                message_sha256,
                ..
            } if message_sha256
                == binding["message_sha256"]
                    .as_str()
                    .ok_or_else(bad_response)?
                && !proof.failed =>
            {
                *proof
            }
            _ => {
                return Err(GoFinError::validation(
                    "Original Trigger placement does not yet have a finalized, bound transaction proof.",
                ));
            }
        };
        let mut wallet_funding = 0u64;
        for requirement in &mut requirements {
            let delta = proof
                .lamport_delta(&requirement.address)?
                .as_str()
                .parse::<i128>()
                .map_err(|_| bad_response())?;
            if delta != 0 && delta != requirement.lamports as i128 {
                return Err(GoFinError::validation(
                    "Original Trigger placement proof does not match its approved account funding.",
                ));
            }
            if requirement.payer != wallet.address() {
                requirement.lamports = 0;
                continue;
            }
            requirement.lamports = u64::try_from(delta).map_err(|_| bad_response())?;
            wallet_funding = wallet_funding
                .checked_add(requirement.lamports)
                .ok_or_else(bad_response)?;
        }
        let mut expected_wallet_delta = -(wallet_funding as i128);
        if binding["fee_payer"].as_str() == Some(wallet.address()) {
            expected_wallet_delta = expected_wallet_delta
                .checked_sub(proof.fee_lamports as i128)
                .ok_or_else(bad_response)?;
        }
        expected_wallet_delta = expected_wallet_delta
            .checked_sub(
                binding["wrapped_native_lamports"]
                    .as_u64()
                    .ok_or_else(bad_response)? as i128,
            )
            .ok_or_else(bad_response)?;
        if proof
            .lamport_delta(wallet.address())?
            .as_str()
            .parse::<i128>()
            .map_err(|_| bad_response())?
            != expected_wallet_delta
        {
            return Err(GoFinError::validation(
                "Original Trigger placement wallet balance does not match its finalized rent and fee proof.",
            ));
        }
        Ok(requirements)
    }

    fn wallet(&self, input: &Value) -> Result<Arc<SolanaWalletAdapter>> {
        let name = input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select an enrolled Solana wallet."))?;
        self.wallets.get(name).cloned().ok_or_else(|| {
            GoFinError::new(
                "JUPITER_WALLET_UNKNOWN",
                "connection",
                "The selected Solana wallet is not enrolled for Jupiter Trigger.",
                "Choose an enrolled wallet from connection discovery.",
            )
        })
    }

    fn validate_network(input: &Value) -> Result<()> {
        if input.get("network").and_then(Value::as_str) != Some("solana")
            || input
                .get("cluster")
                .and_then(Value::as_str)
                .unwrap_or(CLUSTER)
                != CLUSTER
        {
            return Err(GoFinError::validation(
                "Jupiter Trigger requires Solana mainnet-beta.",
            ));
        }
        Ok(())
    }

    async fn list_orders(&self, wallet: &SolanaWalletAdapter, status: &str) -> Result<Value> {
        let path = format!(
            "/trigger/v1/getTriggerOrders?user={}&orderStatus={}&page=1&includeFailedTx=true",
            wallet.address(),
            status
        );
        let raw = self
            .api
            .request(
                JupiterHost::Platform,
                Method::GET,
                &path,
                None,
                RateBucket::Main,
                RequestPriority::Normal,
                DEADLINE,
            )
            .await?;
        let orders = raw
            .get("orders")
            .and_then(Value::as_array)
            .ok_or_else(bad_response)?;
        if raw.get("user").and_then(Value::as_str) != Some(wallet.address()) {
            return Err(bad_response());
        }
        let mut checked = Vec::with_capacity(orders.len());
        for entry in orders {
            let key = entry
                .get("orderKey")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?;
            gofin_solana::parse_address(key)?;
            let account = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([key,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let state = if account.get("value").is_some_and(|v| !v.is_null()) {
                if account["value"].get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                    return Err(bad_response());
                }
                let bytes =
                    gofin_solana::token::decode_rpc_account_data(&account["value"]["data"])?;
                let decoded = decode_order(key, &bytes)?;
                if decoded.key != key
                    || decoded.maker != wallet.address()
                    || derive_order(&decoded.maker, decoded.unique_id)? != key
                    || expected_reserve(key, &decoded.input_token_program, &decoded.input_mint)?
                        != decoded.input_reserve
                {
                    return Err(bad_response());
                }
                let now = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getBlockTime",
                        json!([wallet
                            .runtime()
                            .rpc()
                            .request("getSlot", json!([{"commitment":"finalized"}]))
                            .await?
                            .as_u64()
                            .ok_or_else(bad_response)?]),
                    )
                    .await?
                    .as_i64()
                    .ok_or_else(bad_response)?;
                if decoded.expired_at.is_some_and(|expiry| expiry <= now) {
                    "expired_escrowed"
                } else {
                    "open_escrowed"
                }
            } else {
                "account_closed_unverified"
            };
            checked.push(json!({"order_key":key,"verified_on_chain_state":state,"provider_status":entry.get("status"),"input_mint":entry.get("inputMint"),"output_mint":entry.get("outputMint"),"raw_making_amount":entry.get("rawMakingAmount"),"raw_taking_amount":entry.get("rawTakingAmount"),"raw_remaining_making_amount":entry.get("rawRemainingMakingAmount"),"raw_remaining_taking_amount":entry.get("rawRemainingTakingAmount"),"expired_at":entry.get("expiredAt"),"open_tx":entry.get("openTx"),"close_tx":entry.get("closeTx")}));
        }
        Ok(
            json!({"network":NETWORK,"cluster":CLUSTER,"wallet":wallet.address(),"provider_order_status":status,"orders":checked,"total_pages":raw.get("totalPages")}),
        )
    }

    async fn order_history(
        &self,
        wallet: &SolanaWalletAdapter,
        order_key: &str,
        initial_cursor: Option<&str>,
    ) -> Result<Value> {
        validate_history_cursor(initial_cursor)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        let order_info = tokio::time::timeout_at(
            deadline,
            wallet.runtime().rpc().request(
                "getAccountInfo",
                json!([order_key,{"encoding":"base64","commitment":"finalized"}]),
            ),
        )
        .await
        .map_err(|_| history_timeout())??;
        let mut order = if let Some(account) = order_info.get("value").filter(|v| !v.is_null()) {
            if account.get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                return Err(bad_response());
            }
            let bytes = gofin_solana::token::decode_rpc_account_data(
                account.get("data").ok_or_else(bad_response)?,
            )?;
            let decoded = decode_order(order_key, &bytes)?;
            if decoded.maker != wallet.address() {
                return Err(unsafe_tx());
            }
            Some(decoded)
        } else {
            None
        };
        let order_account_open = order.is_some();
        let mut rows = Vec::new();
        let mut before: Option<String> = initial_cursor.map(str::to_owned);
        let mut complete = false;
        let mut interrupted = false;
        for _ in 0..10 {
            let mut config = json!({"commitment":"finalized","limit":1000});
            if let Some(signature) = &before {
                config["before"] = json!(signature);
            }
            let signatures = match tokio::time::timeout_at(
                deadline,
                wallet
                    .runtime()
                    .rpc()
                    .request("getSignaturesForAddress", json!([order_key, config])),
            )
            .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(_)) | Err(_) => {
                    interrupted = true;
                    break;
                }
            };
            let batch = signatures.as_array().ok_or_else(bad_response)?;
            if batch.is_empty() || batch.len() < 1000 {
                rows.extend(batch.iter().cloned());
                complete = true;
                break;
            }
            before = batch
                .last()
                .and_then(|row| row.get("signature"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if before.is_none() {
                return Err(bad_response());
            }
            rows.extend(batch.iter().cloned());
        }
        let mut fills = Vec::new();
        let mut cancelled = false;
        let mut cancel_events = Vec::new();
        let mut event_digests = BTreeMap::new();
        let mut total_making = 0u128;
        let mut total_taking = 0u128;
        let mut fully_filled_event = false;
        let mut last_remaining: Option<(u64, u64)> = None;
        let mut finalized_transactions = Vec::new();
        let mut seen_signatures = BTreeMap::<String, u64>::new();
        for row in rows {
            let signature = row
                .get("signature")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?;
            let row_slot = row
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or_else(bad_response)?;
            if let Some(previous_slot) = seen_signatures.insert(signature.to_owned(), row_slot) {
                if previous_slot != row_slot {
                    return Err(bad_response());
                }
                continue;
            }
            let outcome = match tokio::time::timeout_at(
                deadline,
                wallet.runtime().observe_finalized_transaction(signature),
            )
            .await
            {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(_)) | Err(_) => {
                    interrupted = true;
                    complete = false;
                    break;
                }
            };
            let (slot, proof) = match outcome {
                ReceiptOutcome::NotDispatched { .. } => {
                    complete = false;
                    continue;
                }
                ReceiptOutcome::Pending { .. } => {
                    complete = false;
                    continue;
                }
                ReceiptOutcome::Finalized {
                    signature: finalized,
                    slot,
                    failed,
                    proof,
                    ..
                } => {
                    if finalized != signature {
                        return Err(bad_response());
                    }
                    if failed || proof.error.is_some() {
                        continue;
                    }
                    if row_slot != slot || proof.slot != slot {
                        return Err(bad_response());
                    }
                    (slot, proof)
                }
            };
            finalized_transactions.push((slot, signature.to_owned(), proof));
        }

        // getSignaturesForAddress returns newest-first, but its ordering for
        // transactions sharing a slot is not a settlement contract. Resolve
        // those ties from the finalized block's canonical signature order.
        let mut by_slot = BTreeMap::<u64, Vec<String>>::new();
        for (slot, signature, _) in &finalized_transactions {
            by_slot.entry(*slot).or_default().push(signature.clone());
        }
        let mut canonical_indices = BTreeMap::<(u64, String), usize>::new();
        for (slot, signatures) in by_slot
            .iter()
            .filter(|(_, signatures)| signatures.len() > 1)
        {
            let block = tokio::time::timeout_at(
                deadline,
                wallet.runtime().rpc().request(
                    "getBlock",
                    json!([slot,{
                        "commitment":"finalized",
                        "transactionDetails":"signatures",
                        "rewards":false,
                        "maxSupportedTransactionVersion":0
                    }]),
                ),
            )
            .await
            .map_err(|_| history_timeout())??;
            let indices = canonical_block_signature_indices(&block)?;
            for signature in signatures {
                let Some(index) = indices.get(signature) else {
                    return Err(bad_response());
                };
                canonical_indices.insert((*slot, signature.clone()), *index);
            }
        }
        finalized_transactions.sort_by_key(|(slot, signature, _)| {
            (
                *slot,
                canonical_indices
                    .get(&(*slot, signature.clone()))
                    .copied()
                    .unwrap_or(0),
                signature.clone(),
            )
        });

        let mut remainder_chain = order
            .as_ref()
            .map(|order| (order.original_making, order.original_taking));
        let mut history_terminal = false;
        for (slot, signature, proof) in finalized_transactions {
            let keys = proof.account_keys.clone();
            if !keys.iter().any(|key| key == order_key) {
                return Err(bad_response());
            }
            let has_program = proof
                .message_instructions
                .iter()
                .any(|ix| ix.program_id == PROGRAM)
                || proof.inner_instructions.iter().any(|group| {
                    group
                        .get("instructions")
                        .and_then(Value::as_array)
                        .is_some_and(|rows| {
                            rows.iter().any(|ix| {
                                ix.get("programId").and_then(Value::as_str) == Some(PROGRAM)
                            })
                        })
                });
            if !has_program {
                continue;
            }
            let meta = json!({"preTokenBalances":proof.pre_token_balances,"postTokenBalances":proof.post_token_balances});
            for (event_index, encoded) in program_event_data(&proof.log_messages, PROGRAM)
                .into_iter()
                .enumerate()
            {
                let bytes = match STANDARD.decode(encoded) {
                    Ok(data) => data,
                    Err(_) => continue,
                };
                if let Some(created) = decode_create_event(&bytes, order_key)? {
                    if history_terminal {
                        complete = false;
                        continue;
                    }
                    if created.maker != wallet.address() {
                        return Err(unsafe_tx());
                    }
                    if let Some(existing) = &order {
                        if existing.original_making != created.original_making
                            || existing.original_taking != created.original_taking
                            || existing.input_mint != created.input_mint
                            || existing.output_mint != created.output_mint
                        {
                            return Err(bad_response());
                        }
                    } else {
                        order = Some(created);
                    }
                    if let Some(created) = &order {
                        let original = (created.original_making, created.original_taking);
                        if remainder_chain.is_some_and(|remainder| remainder != original) {
                            complete = false;
                            continue;
                        }
                        remainder_chain = Some(original);
                    }
                }
                if let Some(event) = decode_trade_event(&bytes, order_key) {
                    if history_terminal {
                        complete = false;
                        continue;
                    }
                    let Some(ref order) = order else {
                        complete = false;
                        continue;
                    };
                    let Some(next_remaining) = remainder_chain.and_then(|previous| {
                        verified_trigger_fill_remainder(
                            previous,
                            event.making,
                            event.taking,
                            (event.remaining_making, event.remaining_taking),
                        )
                    }) else {
                        complete = false;
                        continue;
                    };
                    if event.making == 0
                        || event.taking == 0
                        || verify_fill_balance_effects(
                            &meta,
                            &keys,
                            wallet.address(),
                            order,
                            &event,
                            token_decimals(&wallet.runtime(), &order.input_mint).await?,
                            token_decimals(&wallet.runtime(), &order.output_mint).await?,
                        )
                        .is_err()
                    {
                        complete = false;
                        continue;
                    }
                    total_making = total_making
                        .checked_add(u128::from(event.making))
                        .ok_or_else(bad_response)?;
                    total_taking = total_taking
                        .checked_add(u128::from(event.taking))
                        .ok_or_else(bad_response)?;
                    remainder_chain = Some(next_remaining);
                    let event_id = format!("fill:{signature}:{event_index}");
                    let event_digest = gofin_domain::binding_hash(
                        &json!({"signature":signature,"slot":slot,"message_sha256":proof.message_sha256,"order_key":order_key,"event_data":encoded}),
                    )?;
                    event_digests.insert(event_id.clone(), event_digest.clone());
                    fully_filled_event = next_remaining == (0, 0);
                    last_remaining = Some(next_remaining);
                    history_terminal = fully_filled_event;
                    fills.push(json!({"event_id":event_id,"event_digest":event_digest,"signature":signature,"slot":slot,"taker":event.taker,"making_atoms":event.making.to_string(),"taking_atoms":event.taking.to_string(),"remaining_making_atoms":event.remaining_making.to_string(),"remaining_taking_atoms":event.remaining_taking.to_string(),"verified_finalized":true}));
                } else if decode_cancel_event(&bytes, order_key) {
                    if history_terminal || cancelled {
                        complete = false;
                        continue;
                    }
                    let Some(ref order) = order else {
                        complete = false;
                        continue;
                    };
                    let expected_return = u128::from(
                        remainder_chain
                            .or(last_remaining)
                            .unwrap_or((order.original_making, order.original_taking))
                            .0,
                    );
                    let input_decimals =
                        token_decimals(&wallet.runtime(), &order.input_mint).await?;
                    let wallet_delta = proof.token_delta_verified(
                        wallet.address(),
                        &order.input_mint,
                        &order.input_token_program,
                        input_decimals,
                    )?;
                    let escrow_delta = proof.token_delta_verified(
                        order_key,
                        &order.input_mint,
                        &order.input_token_program,
                        input_decimals,
                    )?;
                    let wallet_return = wallet_delta.as_str().parse::<u128>().ok();
                    let escrow_release = escrow_delta.magnitude().as_str().parse::<u128>().ok();
                    if expected_return == 0
                        || wallet_delta.is_negative()
                        || !escrow_delta.is_negative()
                        || wallet_return != Some(expected_return)
                        || escrow_release != Some(expected_return)
                    {
                        complete = false;
                        continue;
                    }
                    cancelled = true;
                    history_terminal = true;
                    let event_id = format!("cancel:{signature}:{event_index}");
                    let event_digest = gofin_domain::binding_hash(
                        &json!({"signature":signature,"slot":slot,"message_sha256":proof.message_sha256,"order_key":order_key,"event_data":encoded}),
                    )?;
                    event_digests.insert(event_id.clone(), event_digest.clone());
                    cancel_events.push(json!({"event_id":event_id,"event_digest":event_digest,"signature":signature,"slot":slot,"verified_finalized":true}));
                }
            }
        }
        if order_account_open
            && let (Some(order), Some((making, taking))) = (&order, remainder_chain)
            && (order.making != making || order.taking != taking)
        {
            complete = false;
        }
        let full_history_complete = complete && initial_cursor.is_none();
        let state = if full_history_complete {
            if order_account_open {
                let order = order.as_ref().ok_or_else(bad_response)?;
                if order.making == 0 && fully_filled_event {
                    "filled"
                } else if order.making == 0 {
                    "closed_without_verified_fill"
                } else if order
                    .expired_at
                    .is_some_and(|e| e <= Utc::now().timestamp())
                {
                    "expired_escrowed"
                } else {
                    "open_escrowed"
                }
            } else if fully_filled_event {
                "filled"
            } else if cancelled {
                "cancelled"
            } else if total_making > 0 {
                "closed_after_partial_fill_unclassified"
            } else {
                "closed_history_unproven"
            }
        } else {
            "history_incomplete"
        };
        let remainder_report = trigger_remainder_report(
            cancelled,
            fully_filled_event,
            order_account_open,
            order.as_ref().map(|o| (o.making, o.taking)),
            last_remaining.or(remainder_chain),
        );
        Ok(
            json!({"order_key":order_key,"state":state,"bounded_rpc_scan_complete":full_history_complete,"history_cursor":if interrupted { Value::Null } else { before.map_or(Value::Null,Value::String) },"history_completeness":if initial_cursor.is_some() { "older finalized RPC signatures from the supplied cursor; this page is not archival completeness" } else { "bounded finalized RPC signature scan only; archival completeness is not attested" },"verified_fills":fills,"verified_cancel_events":cancel_events,"event_digests":event_digests,"total_filled_making_atoms":total_making.to_string(),"total_filled_taking_atoms":total_taking.to_string(),"cancel_event_seen":cancelled,"original_making_atoms":order.as_ref().map(|o|o.original_making.to_string()),"original_taking_atoms":order.as_ref().map(|o|o.original_taking.to_string()),"remaining_making_atoms":remainder_report.quote_remaining.map(|v|v.0.to_string()),"remaining_taking_atoms":remainder_report.quote_remaining.map(|v|v.1.to_string()),"escrowed_remaining_making_atoms":remainder_report.escrowed_making.map(|v|v.to_string()),"returned_making_atoms":remainder_report.returned_making.map(|v|v.to_string()),"remaining_semantics":if cancelled { "historical unfilled quote before verified cancellation; escrow has been returned" } else { "current on-chain remainder when open, otherwise last verified event remainder" },"settlement":"only finalized successful transactions with decoded Trigger events and matching token effects are listed as fills"}),
        )
    }

    async fn request_tx(
        &self,
        operation: &str,
        input: &Value,
        wallet: &SolanaWalletAdapter,
    ) -> Result<TriggerTx> {
        let body = if operation == CREATE {
            let input_mint = mint(input, "input_mint")?;
            let output_mint = mint(input, "output_mint")?;
            let making = atoms(input, "making_amount_atoms")?;
            let taking = atoms(input, "taking_amount_atoms")?;
            if input_mint == output_mint || making == 0 || taking == 0 {
                return Err(GoFinError::validation(
                    "A Trigger order needs distinct mints and positive exact amounts.",
                ));
            }
            let mut params =
                json!({"makingAmount":making.to_string(),"takingAmount":taking.to_string()});
            if let Some(expiry) = input.get("expired_at_unix").and_then(Value::as_i64) {
                let now = Utc::now().timestamp();
                if expiry <= now || expiry > now + 365 * 24 * 60 * 60 {
                    return Err(GoFinError::validation(
                        "Trigger expiry must be in the future and within one year.",
                    ));
                }
                params["expiredAt"] = json!(expiry);
            }
            json!({"maker":wallet.address(),"payer":wallet.address(),"inputMint":input_mint,"outputMint":output_mint,"params":params,"computeUnitPrice":"auto","wrapAndUnwrapSol":false})
        } else {
            let key = input
                .get("order_key")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("Provide the Trigger order account to cancel.")
                })?;
            gofin_solana::parse_address(key)?;
            json!({"maker":wallet.address(),"order":key,"computeUnitPrice":"auto"})
        };
        let path = if operation == CREATE {
            "/trigger/v1/createOrder"
        } else {
            "/trigger/v1/cancelOrder"
        };
        let raw = self
            .api
            .request(
                JupiterHost::Platform,
                Method::POST,
                path,
                Some(body),
                RateBucket::Main,
                RequestPriority::Financial,
                DEADLINE,
            )
            .await?;
        let tx = raw
            .get("transaction")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?
            .to_owned();
        let request_id = raw
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?
            .to_owned();
        let order = raw
            .get("order")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                input
                    .get("order_key")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let order = order.ok_or_else(bad_response)?;
        gofin_solana::parse_address(&order)?;
        Ok(TriggerTx {
            encoded: tx,
            request_id,
            order,
        })
    }

    async fn inspect(
        &self,
        operation: &str,
        input: &Value,
        wallet: &SolanaWalletAdapter,
        tx: &TriggerTx,
        simulate: bool,
    ) -> Result<(FrozenTransaction, adapter::AuxiliaryEffects)> {
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected Solana wallet has no protected signing capability.",
                "Enroll a protected signer for this wallet before preparing a Trigger order.",
            )
        })?;
        let frozen = wallet
            .runtime()
            .freeze_transaction_with_expiry(
                &tx.encoded,
                &signer,
                None,
                None,
                atoms_optional(input, "maximum_network_fee_lamports")?,
                Some(tx.request_id.clone()),
                &[],
            )
            .await?;
        let valid = wallet
            .runtime()
            .rpc()
            .request(
                "isBlockhashValid",
                json!([frozen.recent_blockhash(),{"commitment":"confirmed"}]),
            )
            .await?
            .get("value")
            .and_then(Value::as_bool)
            .ok_or_else(bad_response)?;
        if !valid {
            return Err(changed());
        }
        if frozen.signer() != wallet.address()
            || !frozen
                .instructions()
                .iter()
                .any(|ix| ix.program_id == PROGRAM)
        {
            return Err(unsafe_tx());
        }
        let (allowed_mints, wrap_amount) = if operation == CREATE {
            (
                vec![mint(input, "input_mint")?, mint(input, "output_mint")?],
                if input.get("input_mint").and_then(Value::as_str) == Some("native") {
                    Some(atoms(input, "making_amount_atoms")?)
                } else {
                    None
                },
            )
        } else {
            let info = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([tx.order,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            if info
                .get("value")
                .and_then(|v| v.get("owner"))
                .and_then(Value::as_str)
                != Some(PROGRAM)
            {
                return Err(bad_response());
            }
            let bytes = gofin_solana::token::decode_rpc_account_data(
                info.get("value")
                    .and_then(|v| v.get("data"))
                    .ok_or_else(bad_response)?,
            )?;
            let order = decode_order(&tx.order, &bytes)?;
            if order.maker != wallet.address() {
                return Err(unsafe_tx());
            }
            (vec![order.input_mint, order.output_mint], None)
        };
        if operation == CANCEL {
            // Reconciliation currently models exact gross escrow refunds and
            // wallet proceeds. Do not sign a close for an order whose
            // mint can apply future transfer fees or invoke an unreviewed hook.
            let refund_mint = allowed_mints.first().ok_or_else(bad_response)?;
            for mint in &allowed_mints {
                let (mint_info, _) = finalized_trigger_mint(wallet, mint).await?;
                validate_trigger_mint_support(&mint_info)?;
                if mint == refund_mint {
                    validate_trigger_recipient_ata(wallet, refund_mint, &mint_info.program_id)
                        .await?;
                }
            }
        }
        let mut auxiliary = adapter::validate_auxiliary_instructions(
            wallet,
            &frozen,
            &allowed_mints,
            wrap_amount,
            atoms_optional(input, "maximum_account_funding_lamports")?,
        )
        .await?;
        if operation == CREATE {
            // Anchor's `init` allocates the order PDA through CPI, so this
            // rent and the reserve token account rent do not appear as
            // top-level system instructions. Both are refundable funding and
            // must be bounded before dispatch.
            let order_rent = wallet
                .runtime()
                .rpc()
                .request(
                    "getMinimumBalanceForRentExemption",
                    json!([310,{"commitment":"finalized"}]),
                )
                .await?
                .as_u64()
                .ok_or_else(bad_response)?;
            let input_mint = mint(input, "input_mint")?;
            let (input_mint_info, mint_data) = finalized_trigger_mint(wallet, &input_mint).await?;
            validate_trigger_mint_support(&input_mint_info)?;
            let input_token_program = input_mint_info.program_id.clone();
            let output_mint = mint(input, "output_mint")?;
            let (output_mint_info, _) = finalized_trigger_mint(wallet, &output_mint).await?;
            validate_trigger_mint_support(&output_mint_info)?;
            validate_trigger_recipient_ata(wallet, &output_mint, &output_mint_info.program_id)
                .await?;
            let reserve = expected_reserve(&tx.order, &input_token_program, &input_mint)?;
            let reserve_size =
                gofin_solana::token::required_token_account_len(&input_token_program, &mint_data)?;
            for (address, size, owner) in [
                (tx.order.clone(), 310usize, PROGRAM),
                (reserve, reserve_size, input_token_program.as_str()),
            ] {
                let existing = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([address,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                if !existing.get("value").is_some_and(Value::is_null) {
                    return Err(GoFinError::new(
                        "JUPITER_TRIGGER_ACCOUNT_ALREADY_EXISTS",
                        "prepare",
                        "The new Trigger order or reserve account already exists on chain.",
                        "Choose a fresh order identity and prepare again from current chain state.",
                    ));
                }
                let lamports = if size == 310 {
                    order_rent
                } else {
                    wallet
                        .runtime()
                        .rpc()
                        .request(
                            "getMinimumBalanceForRentExemption",
                            json!([size,{"commitment":"finalized"}]),
                        )
                        .await?
                        .as_u64()
                        .ok_or_else(bad_response)?
                };
                auxiliary
                    .funding_accounts
                    .push(adapter::AccountFundingRequirement {
                        address,
                        lamports,
                        account_owner: owner.to_owned(),
                        refund_authority: wallet.address().into(),
                        payer: wallet.address().into(),
                    });
                auxiliary.account_funding_lamports = auxiliary
                    .account_funding_lamports
                    .checked_add(lamports)
                    .ok_or_else(bad_response)?;
            }
            if auxiliary.account_funding_lamports
                > atoms_optional(input, "maximum_account_funding_lamports")?
            {
                return Err(GoFinError::new(
                    "JUPITER_ACCOUNT_FUNDING_LIMIT_EXCEEDED",
                    "prepare",
                    "Trigger order and token-account rent exceeds the approved SOL funding cap.",
                    "Review the exact rent requirement and approve a sufficient bounded funding amount.",
                ));
            }
        }
        validate_trigger_instructions(operation, input, wallet, &tx.order, &frozen).await?;
        wallet.runtime().validate_fee(&frozen).await?;
        if simulate {
            wallet.runtime().simulate(&frozen).await?;
        }
        Ok((frozen, auxiliary))
    }
}

#[async_trait]
impl Adapter for TriggerAdapter {
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        *self.execution_state.0.lock().map_err(|_| {
            GoFinError::validation("Jupiter Trigger execution-state lock failed.")
        })? = Some(state);
        Ok(())
    }

    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets.iter().map(|(name,w)|ConnectionMetadata{name:name.clone(),provider:"jupiter-trigger".into(),resource:ResourceId(format!("solana:mainnet-beta:{}",w.address())),role:Some("limit_orders".into()),parent:None,network:Some(NETWORK.into()),address:Some(w.address().into()),credential_ref:None,signing_enabled:w.signer().is_some(),details:json!({"cluster":CLUSTER,"custody":"noncustodial","api_key_required":false})}).collect()
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<BoundarySpec> {
        if prepared.operation == CANCEL {
            let wallet = self.wallet(&prepared.input)?;
            let (parent_id, parent_submission) = self.cancellation_parent(&prepared.input)?;
            let spec = crate::boundaries::trigger_cancel_spec(
                &wallet_name(&prepared.input)?,
                wallet.address(),
                prepared.input["order_key"]
                    .as_str()
                    .ok_or_else(bad_response)?,
                &parent_id,
                &parent_submission,
                &ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                bounds,
            )?;
            crate::boundaries::validate_trigger_cancel_candidate(
                &spec,
                &wallet_name(&prepared.input)?,
                wallet.address(),
                prepared.input["order_key"]
                    .as_str()
                    .ok_or_else(bad_response)?,
                &parent_id,
                &parent_submission,
                atoms_optional(&prepared.input, "maximum_network_fee_lamports")?,
                prepared.binding["account_funding_lamports"]
                    .as_u64()
                    .ok_or_else(bad_response)?,
            )?;
            return Ok(spec);
        }
        if prepared.operation != CREATE {
            return Err(GoFinError::new(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "authorization",
                "Trigger boundary authorization supports order creation only.",
                "Use exact approval for cancellation or status operations.",
            ));
        }
        Self::validate_network(&prepared.input)?;
        let wallet = self.wallet(&prepared.input)?;
        let input_asset = asset_for(&wallet, &mint(&prepared.input, "input_mint")?).await?;
        let output_asset = asset_for(&wallet, &mint(&prepared.input, "output_mint")?).await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let now = Utc::now().timestamp();
        let spec = crate::boundaries::trigger_spec(
            &wallet_name(&prepared.input)?,
            wallet.address(),
            &input_asset,
            &output_asset,
            &resource,
            bounds,
            now,
        )?;
        let expiry = prepared
            .input
            .get("expired_at_unix")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                GoFinError::validation("Bounded Trigger orders require an explicit expiry.")
            })?;
        crate::boundaries::validate_trigger_candidate(
            &spec,
            &wallet_name(&prepared.input)?,
            wallet.address(),
            &input_asset,
            &output_asset,
            atoms(&prepared.input, "making_amount_atoms")?,
            atoms(&prepared.input, "taking_amount_atoms")?,
            expiry,
            atoms_optional(&prepared.input, "maximum_network_fee_lamports")?,
            prepared.binding["account_funding_lamports"]
                .as_u64()
                .ok_or_else(bad_response)?,
            now,
        )?;
        Ok(spec)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        if operation == CANCEL {
            Self::validate_network(input)?;
            let wallet = self.wallet(input)?;
            let recovery_step = bounds
                .get("recovery_parent_step_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let supplied_parent = input.get("original_operation_id").is_some()
                || input.get("original_submission").is_some();
            let (parent_id, parent_submission, recovery_parent) = match (
                self.cancellation_parent(input),
                recovery_step,
            ) {
                (Ok((parent_id, submission)), None) => (parent_id, submission, None),
                (Ok(_), Some(_)) => {
                    return Err(GoFinError::validation(
                        "Choose either an exact cancellation parent or a plan recovery parent step.",
                    ));
                }
                (Err(_), Some(_)) if supplied_parent => {
                    return Err(GoFinError::validation(
                        "Recovery parent operation and submission fields must be omitted; GoFin resolves them from the earlier plan step.",
                    ));
                }
                (Err(_), Some(parent_step_id)) => (
                    "__fin_plan_recovery_parent__".into(),
                    Submission {
                        kind: "plan_recovery_parent".into(),
                        reference: parent_step_id.into(),
                        data: json!({"recovery_parent":true}),
                    },
                    Some(gofin_domain::financial_plans::FinancialPlanRecoveryParent {
                        step_id: parent_step_id.into(),
                        operation_id_pointer: "/original_operation_id".into(),
                        submission_pointer: "/original_submission".into(),
                        parent_binding_fields: vec![
                            gofin_domain::financial_plans::FinancialPlanRecoveryBinding {
                                parent_binding_pointer: "/order_key".into(),
                                child_input_pointer: "/order_key".into(),
                            },
                        ],
                    }),
                ),
                (Err(error), None) => return Err(error),
            };
            let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
            let permission_order_key = if recovery_step.is_some() {
                "__fin_plan_recovery_order_key__"
            } else {
                input["order_key"].as_str().ok_or_else(bad_response)?
            };
            let spec = crate::boundaries::trigger_cancel_spec(
                &wallet_name(input)?,
                wallet.address(),
                permission_order_key,
                &parent_id,
                &parent_submission,
                &resource,
                bounds,
            )?;
            return Ok(PlanPermission {
                spec,
                recovery_parent,
                // Cancellation is only a control action. Its returned escrow
                // remains attributed to the original placement lifecycle.
                possible_credits: vec![],
                fee_assets: vec![AssetId {
                    network: NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                }],
                deferred_receipt: None,
                authorization_seconds: Some(600),
                cumulative_scope: None,
            });
        }
        if operation != CREATE {
            return Err(GoFinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Trigger plan permissions support new noncustodial order placement only.",
                "Use exact approval for cancellation or status operations.",
            ));
        }
        Self::validate_network(input)?;
        let wallet = self.wallet(input)?;
        validate_trigger_mint_mode(input)?;
        let input_asset = asset_for(&wallet, &mint(input, "input_mint")?).await?;
        let output_asset = asset_for(&wallet, &mint(input, "output_mint")?).await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let now = Utc::now().timestamp();
        let spec = crate::boundaries::trigger_spec(
            &wallet_name(input)?,
            wallet.address(),
            &input_asset,
            &output_asset,
            &resource,
            bounds,
            now,
        )?;
        let expiry = input
            .get("expired_at_unix")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                GoFinError::validation("Bounded Trigger plans require an explicit expiry.")
            })?;
        crate::boundaries::validate_trigger_candidate(
            &spec,
            &wallet_name(input)?,
            wallet.address(),
            &input_asset,
            &output_asset,
            atoms(input, "making_amount_atoms")?,
            atoms(input, "taking_amount_atoms")?,
            expiry,
            atoms_optional(input, "maximum_network_fee_lamports")?,
            0,
            now,
        )?;
        Ok(PlanPermission {
            spec,
            recovery_parent: None,
            possible_credits: vec![FinancialPlanFundingIdentity {
                resource,
                asset: output_asset,
            }],
            fee_assets: vec![AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            }],
            deferred_receipt: None,
            authorization_seconds: Some(600),
            cumulative_scope: None,
        })
    }
    fn validate_boundary_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        if prepared.operation == CANCEL {
            let wallet = self.wallet(&prepared.input)?;
            let parent_id = prepared.binding["original_operation_id"]
                .as_str()
                .ok_or_else(bad_response)?;
            let parent_submission: Submission =
                serde_json::from_value(prepared.binding["original_submission"].clone())
                    .map_err(|_| bad_response())?;
            return crate::boundaries::validate_trigger_cancel_candidate(
                spec,
                &wallet_name(&prepared.input)?,
                wallet.address(),
                prepared.input["order_key"]
                    .as_str()
                    .ok_or_else(bad_response)?,
                parent_id,
                &parent_submission,
                atoms_optional(&prepared.input, "maximum_network_fee_lamports")?,
                prepared.binding["account_funding_lamports"]
                    .as_u64()
                    .ok_or_else(bad_response)?,
            );
        }
        if prepared.operation != CREATE {
            return Err(GoFinError::new(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "authorization",
                "The prepared operation is not a Trigger order placement.",
                "Use exact approval for cancellation.",
            ));
        }
        let wallet = self.wallet(&prepared.input)?;
        let input_asset = AssetId {
            network: NETWORK.into(),
            reference: mint(&prepared.input, "input_mint")?,
            decimals: spec.fixed["input_asset"]["decimals"]
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(bad_response)?,
        };
        let output_asset = AssetId {
            network: NETWORK.into(),
            reference: mint(&prepared.input, "output_mint")?,
            decimals: spec.fixed["output_asset"]["decimals"]
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(bad_response)?,
        };
        crate::boundaries::validate_trigger_candidate(
            spec,
            &wallet_name(&prepared.input)?,
            wallet.address(),
            &input_asset,
            &output_asset,
            atoms(&prepared.input, "making_amount_atoms")?,
            atoms(&prepared.input, "taking_amount_atoms")?,
            prepared
                .input
                .get("expired_at_unix")
                .and_then(Value::as_i64)
                .ok_or_else(bad_response)?,
            atoms_optional(&prepared.input, "maximum_network_fee_lamports")?,
            prepared.binding["account_funding_lamports"]
                .as_u64()
                .ok_or_else(bad_response)?,
            Utc::now().timestamp(),
        )
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        let mut state = FinancialState::default();
        let pending = Vec::new();
        let mut coverage=vec!["Outstanding Trigger escrow is enumerated from finalized Limit Order V2 program accounts filtered by exact maker; each order PDA and reserve token account is verified.".to_owned()];
        for wallet in self.wallets.values() {
            let wallet_resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
            let response=wallet.runtime().rpc().request("getProgramAccounts",json!([PROGRAM,{"encoding":"base64","commitment":"finalized","filters":[{"memcmp":{"offset":8,"bytes":wallet.address()}}]}])).await?;
            let rows = response.as_array().ok_or_else(bad_response)?;
            if rows.len() > 10000 {
                return Err(bad_response());
            }
            for row in rows {
                let key = row
                    .get("pubkey")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                let account = row.get("account").ok_or_else(bad_response)?;
                if account.get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                    return Err(bad_response());
                }
                let bytes = gofin_solana::token::decode_rpc_account_data(
                    account.get("data").ok_or_else(bad_response)?,
                )?;
                let order = decode_order(key, &bytes)?;
                if order.maker != wallet.address()
                    || !valid_token_program(&order.input_token_program)
                    || !valid_token_program(&order.output_token_program)
                    || derive_order(&order.maker, order.unique_id)? != key
                    || expected_reserve(key, &order.input_token_program, &order.input_mint)?
                        != order.input_reserve
                {
                    return Err(bad_response());
                }
                let reserve = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([order.input_reserve,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let reserve = reserve
                    .get("value")
                    .filter(|v| !v.is_null())
                    .ok_or_else(bad_response)?;
                let token_program = reserve
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                if token_program != order.input_token_program {
                    return Err(bad_response());
                }
                let reserve_data = gofin_solana::token::decode_rpc_account_data(
                    reserve.get("data").ok_or_else(bad_response)?,
                )?;
                let token = gofin_solana::token::parse_token_account(
                    token_program,
                    &order.input_reserve,
                    &reserve_data,
                )?;
                let amount = token.amount_atoms;
                if token.token_owner != key
                    || token.mint != order.input_mint
                    || amount != order.making
                {
                    return Err(bad_response());
                }
                if amount > 0 {
                    let asset = asset_for(wallet, &order.input_mint).await?;
                    state.holdings.push(ResourceAmount {
                        resource: ResourceId(format!("solana:mainnet-beta:trigger-order:{key}")),
                        amount: Amount {
                            asset,
                            atoms: gofin_domain::Quantity::from_u64(amount),
                        },
                    })
                }
            }
            // The engine requires an exact resource coverage token to use a
            // scoped observer for position-anchor bootstrap. Add it only after
            // the finalized owner-filtered account and reserve scan succeeded;
            // the token is emitted even for a verified empty result set.
            append_snapshot_resource_identity(&mut coverage, &wallet_resource);
        }
        if self.wallets.is_empty() {
            coverage.push("No Trigger wallets are enrolled.".into())
        }
        Ok(StateSnapshot {
            state,
            observed_at: Utc::now().to_rfc3339(),
            complete: true,
            coverage,
            pending,
        })
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        let wallets = self
            .wallets
            .iter()
            .filter(|(_, wallet)| {
                scope.matches_network(NETWORK)
                    && scope.matches_resource(&ResourceId(format!(
                        "solana:mainnet-beta:{}",
                        wallet.address()
                    )))
            })
            .map(|(name, wallet)| (name.clone(), wallet.clone()))
            .collect();
        let scoped = Self {
            wallets,
            api: self.api.clone(),
            prices: self.prices.clone(),
            execution_state: self.execution_state.clone(),
        };
        let selected_wallet_resources = scoped
            .wallets
            .values()
            .map(|wallet| ResourceId(format!("solana:mainnet-beta:{}", wallet.address())))
            .collect::<Vec<_>>();
        let mut snapshot = scoped.snapshot().await?;
        snapshot.state.holdings.retain(|h| {
            (scope.matches_resource(&h.resource)
                || selected_wallet_resources
                    .iter()
                    .any(|wallet_resource| scope.matches_resource(wallet_resource)))
                && scope.matches_asset(&h.amount.asset)
        });
        if snapshot.state.holdings.is_empty() {
            snapshot.complete = true;
            snapshot.pending.clear();
            snapshot
                .coverage
                .push("No Trigger escrow identities matched the requested scope.".into())
        }
        Ok(snapshot)
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        Self::validate_network(input)?;
        if operation == CREATE {
            validate_trigger_mint_mode(input)?;
        }
        let wallet = self.wallet(input)?;
        match operation {
            ORDERS => {
                let status = input
                    .get("order_status")
                    .and_then(Value::as_str)
                    .unwrap_or("active");
                if status != "active" && status != "history" {
                    return Err(GoFinError::validation(
                        "Trigger order_status must be active or history.",
                    ));
                }
                self.list_orders(&wallet, status).await
            }
            STATUS => {
                let key = input
                    .get("order_key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        GoFinError::validation("Provide an order account to inspect.")
                    })?;
                gofin_solana::parse_address(key)?;
                let account = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([key,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let Some(value) = account.get("value").filter(|v| !v.is_null()) else {
                    return self
                        .order_history(
                            &wallet,
                            key,
                            input
                                .get("history_before_signature")
                                .and_then(Value::as_str),
                        )
                        .await;
                };
                if value.get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                    return Err(bad_response());
                }
                let bytes = gofin_solana::token::decode_rpc_account_data(
                    value.get("data").ok_or_else(bad_response)?,
                )?;
                let decoded = decode_order(key, &bytes)?;
                if decoded.key != key
                    || decoded.maker != wallet.address()
                    || derive_order(&decoded.maker, decoded.unique_id)? != key
                    || expected_reserve(key, &decoded.input_token_program, &decoded.input_mint)?
                        != decoded.input_reserve
                {
                    return Err(bad_response());
                }
                let now = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getBlockTime",
                        json!([wallet
                            .runtime()
                            .rpc()
                            .request("getSlot", json!([{"commitment":"finalized"}]))
                            .await?
                            .as_u64()
                            .ok_or_else(bad_response)?]),
                    )
                    .await?
                    .as_i64()
                    .ok_or_else(bad_response)?;
                let expired = decoded.expired_at.is_some_and(|expiry| expiry <= now);
                let mut history = self
                    .order_history(
                        &wallet,
                        key,
                        input
                            .get("history_before_signature")
                            .and_then(Value::as_str),
                    )
                    .await?;
                let history_state = history
                    .get("state")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                history["state"] = json!(trigger_status_state(history_state, expired));
                history["maker"] = json!(decoded.maker);
                history["input_mint"] = json!(decoded.input_mint);
                history["output_mint"] = json!(decoded.output_mint);
                history["expired_at"] = json!(decoded.expired_at);
                Ok(history)
            }
            _ => Err(unknown()),
        }
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != CREATE && operation != CANCEL {
            return Err(unknown());
        }
        Self::validate_network(input)?;
        let wallet = self.wallet(input)?;
        let tx = self.request_tx(operation, input, &wallet).await?;
        if operation == CANCEL {
            let account = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([tx.order,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let value = account
                .get("value")
                .filter(|v| !v.is_null())
                .ok_or_else(|| {
                    GoFinError::new(
                        "JUPITER_ORDER_CLOSED",
                        "prepare",
                        "The Trigger order is no longer open on chain.",
                        "Inspect its finalized fill, expiry or cancellation history before acting.",
                    )
                })?;
            if value.get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                return Err(bad_response());
            }
            let decoded = decode_order(
                &tx.order,
                &gofin_solana::token::decode_rpc_account_data(
                    value.get("data").ok_or_else(bad_response)?,
                )?,
            )?;
            if decoded.maker != wallet.address() {
                return Err(unsafe_tx());
            }
            self.validate_order_cancellation_origin(input, &wallet, &tx.order)?;
        }
        let (frozen, auxiliary) = self.inspect(operation, input, &wallet, &tx, true).await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let order_resource = ResourceId(format!("solana:mainnet-beta:trigger-order:{}", tx.order));
        let (asset, amount) = if operation == CREATE {
            let mint = mint(input, "input_mint")?;
            (
                asset_for(&wallet, &mint).await?,
                atoms(input, "making_amount_atoms")?,
            )
        } else {
            let key = &tx.order;
            let account = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([key,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let bytes = gofin_solana::token::decode_rpc_account_data(&account["value"]["data"])?;
            if account["value"].get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                return Err(bad_response());
            }
            let decoded = decode_order(key, &bytes)?;
            (
                asset_for(&wallet, &decoded.input_mint).await?,
                decoded.making,
            )
        };
        let bound_input_mint = if asset.reference == "native" {
            NATIVE.to_owned()
        } else {
            asset.reference.clone()
        };
        let input_token_program =
            adapter::token_program_for(&wallet.runtime(), &bound_input_mint).await?;
        let output_mint = if operation == CREATE {
            Some(mint(input, "output_mint")?)
        } else {
            None
        };
        let output_asset = if let Some(output_mint) = &output_mint {
            Some(asset_for(&wallet, output_mint).await?)
        } else {
            None
        };
        let output_token_program = if let Some(output_mint) = &output_mint {
            Some(adapter::token_program_for(&wallet.runtime(), output_mint).await?)
        } else {
            None
        };
        let cancellation_parent = if operation == CANCEL
            && input
                .get("original_operation_id")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        {
            Some(self.cancellation_parent(input)?)
        } else {
            if operation == CANCEL
                && input
                    .get("original_submission")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(GoFinError::validation(
                    "An original submission can only be supplied with its exact original operation ID.",
                ));
            }
            None
        };
        let cancellation_parent_funding = if cancellation_parent.is_some() {
            self.cancellation_parent_funding(input).await?
        } else {
            Vec::new()
        };
        if cancellation_parent.is_some()
            && (auxiliary.wrapped_native_lamports > 0
                || !auxiliary.closed_wrapped_native_accounts.is_empty())
        {
            return Err(GoFinError::new(
                "TRIGGER_CANCEL_REQUIRES_EXISTING_DESTINATION",
                "preview",
                "Fee-bearing cancellation requires the original escrow destination accounts to exist already.",
                "Create or restore the wallet token account as a separate approved operation, then prepare cancellation again.",
            ));
        }
        if operation == CANCEL
            && cancellation_parent.is_none()
            && (auxiliary.account_funding_lamports > 0
                || atoms_optional(input, "maximum_account_funding_lamports")? > 0
                || auxiliary.wrapped_native_lamports > 0
                || !auxiliary.closed_wrapped_native_accounts.is_empty())
        {
            return Err(GoFinError::new(
                "TRIGGER_EXTERNAL_CANCEL_REQUIRES_EXISTING_ACCOUNTS",
                "preview",
                "Cancelling an externally created order requires existing destination accounts and a zero account-funding cap.",
                "Create the exact destination account in a separate approved operation, set maximum_account_funding_lamports to 0, then prepare cancellation again.",
            ));
        }
        let constraint = if let Some((operation_id, submission)) = &cancellation_parent {
            EffectConstraint::SubmissionCancellation {
                operation_id: operation_id.clone(),
                submission: submission.clone(),
            }
        } else if operation == CREATE {
            let output_asset = asset_for(&wallet, &mint(input, "output_mint")?).await?;
            EffectConstraint::EscrowLifecycle {
                wallet_resource: resource.clone(),
                escrow_resource: order_resource.clone(),
                input_asset: asset.clone(),
                output_asset,
                maximum_wallet_debit: gofin_domain::Quantity::from_u64(amount),
                minimum_output_per_input_numerator: gofin_domain::Quantity::from_u64(atoms(
                    input,
                    "taking_amount_atoms",
                )?),
                minimum_output_per_input_denominator: gofin_domain::Quantity::from_u64(amount),
            }
        } else {
            EffectConstraint::Transfer {
                from: order_resource.clone(),
                to: resource.clone(),
                debit_asset: asset.clone(),
                credit_asset: asset.clone(),
                maximum_debit: gofin_domain::Quantity::from_u64(amount),
                minimum_credit: gofin_domain::Quantity::from_u64(amount),
            }
        };
        let fee_bound = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: AssetId {
                    network: NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                atoms: gofin_domain::Quantity::from_u64(atoms_optional(
                    input,
                    "maximum_network_fee_lamports",
                )?),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let effect_name = if operation == CREATE {
            "placement"
        } else {
            "cancellation"
        };
        let maximum_network_fee = atoms_optional(input, "maximum_network_fee_lamports")?;
        let maximum_funding = atoms_optional(input, "maximum_account_funding_lamports")?;
        let mut valuation = if operation == CREATE {
            let output_mint = mint(input, "output_mint")?;
            let output_asset = asset_for(&wallet, &output_mint).await?;
            adapter::valuation(
                self.prices.clone(),
                asset.clone(),
                amount,
                output_asset,
                atoms(input, "taking_amount_atoms")?,
                maximum_network_fee,
                wallet.address(),
            )
            .await?
            .0
        } else {
            cancellation_valuation(self.prices.clone(), maximum_network_fee).await?
        };
        if maximum_funding > 0 {
            let native = AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            };
            let native_price = valuation
                .prices
                .iter()
                .find(|price| price.asset == native)
                .ok_or_else(bad_response)?;
            valuation.additional_allocation_usd_micros =
                gofin_domain::Quantity::from_u64(maximum_funding)
                    .mul_div_ceil(&native_price.usd_micros_per_unit, &native_price.unit_atoms)?;
        }
        let mut debits =
            if operation == CREATE || cancellation_parent.is_none() && operation == CANCEL {
                vec![ResourceAmount {
                    resource: if operation == CREATE {
                        resource.clone()
                    } else {
                        order_resource.clone()
                    },
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: gofin_domain::Quantity::from_u64(amount),
                    },
                }]
            } else {
                Vec::new()
            };
        let mut expected = vec![constraint];
        if maximum_funding > 0 {
            let native = AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            };
            let wallet_funding = auxiliary
                .funding_accounts
                .iter()
                .filter(|funding| funding.payer == wallet.address())
                .try_fold(0u64, |total, funding| {
                    total.checked_add(funding.lamports).ok_or_else(bad_response)
                })?;
            if operation == CREATE && wallet_funding > 0 {
                debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: native.clone(),
                        atoms: gofin_domain::Quantity::from_u64(wallet_funding),
                    },
                });
            }
            if operation == CREATE {
                let accounts = auxiliary
                    .funding_accounts
                    .iter()
                    .filter(|funding| funding.payer == wallet.address())
                    .map(|funding| gofin_domain::RefundableFundingLimit {
                        position: format!("account-funding:{}", funding.address),
                        account: funding.address.clone(),
                        maximum_amount: gofin_domain::Quantity::from_u64(funding.lamports),
                    })
                    .collect::<Vec<_>>();
                if !accounts.is_empty() {
                    expected.push(EffectConstraint::AccountFundingLifecycle {
                        resource: resource.clone(),
                        asset: native.clone(),
                        accounts,
                    });
                }
            } else if cancellation_parent.is_some() && wallet_funding > 0 {
                expected.push(EffectConstraint::OptionalAccountFunding {
                    resource: resource.clone(),
                    asset: native.clone(),
                    accounts: auxiliary
                        .funding_accounts
                        .iter()
                        .filter(|funding| funding.payer == wallet.address())
                        .map(|funding| gofin_domain::AccountFundingLimit {
                            position: format!("account-funding:{}", funding.address),
                            maximum_amount: gofin_domain::Quantity::from_u64(funding.lamports),
                        })
                        .collect(),
                });
            }
        }
        let readable_amount =
            crate::boundaries::format_atomic_quantity(&amount.to_string(), asset.decimals);
        let summary = if operation == CREATE {
            format!(
                "Place a noncustodial Trigger order locking {readable_amount} tokens of mint {} in escrow.",
                asset.reference
            )
        } else {
            format!(
                "Cancel Trigger order {} and return its remaining {readable_amount} tokens of mint {}.",
                tx.order, asset.reference
            )
        };
        Ok(PreparedOperation{operation:operation.into(),input:input.clone(),binding:json!({"kind":effect_name,"wallet_address":wallet.address(),"order_key":tx.order,"request_id":tx.request_id,"original_operation_id":cancellation_parent.as_ref().map(|(id,_)|id),"original_submission":cancellation_parent.as_ref().map(|(_,s)|s),"original_funding_accounts":cancellation_parent_funding,"transaction_base64":tx.encoded,"message_sha256":frozen.message_sha256(),"input_mint":asset.reference,"input_decimals":asset.decimals,"input_token_program":input_token_program,"output_mint":output_mint,"output_decimals":output_asset.map(|output|output.decimals),"output_token_program":output_token_program,"amount_atoms":amount.to_string(),"output_atoms":if operation==CREATE{Some(atoms(input,"taking_amount_atoms")?.to_string())}else{None},"last_valid_block_height":frozen.last_valid_block_height(),"fee_payer":frozen.fee_payer(),"account_funding_lamports":auxiliary.account_funding_lamports,"funding_accounts":auxiliary.funding_accounts,"wrapped_native_lamports":auxiliary.wrapped_native_lamports,"closed_wrapped_native_accounts":auxiliary.closed_wrapped_native_accounts,"closed_wrapped_native_refunds":auxiliary.closed_wrapped_native_refunds}),summary,checks:vec![pass("Pinned Limit Order V2 instruction and account layout"),pass("Exact maker, escrow and token mint binding"),pass("Network fee limit"),pass("Account funding limit"),pass("Simulation"),pass("Blockhash expiry")],warnings:vec!["Placement moves assets into escrow; it does not complete the later swap. Rent-bearing accounts remain refundable funding and are tracked separately from fees. Fill, expiry and cancellation are separate states.".into()],observed_at:Utc::now().to_rfc3339(),expires_at:(Utc::now()+ChronoDuration::seconds(20)).to_rfc3339(),valuation:Some(valuation),intent:FinancialIntent{debits,maximum_costs:vec![fee_bound],expected,locks:vec![resource,order_resource]},action_deadline:None,operation_version:1})
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let wallet = self.wallet(&prepared.input)?;
        let tx = TriggerTx {
            encoded: prepared.binding["transaction_base64"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            request_id: prepared.binding["request_id"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            order: prepared.binding["order_key"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
        };
        let (frozen, auxiliary) = self
            .inspect(&prepared.operation, &prepared.input, &wallet, &tx, true)
            .await?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(changed());
        }
        if prepared.operation == CANCEL {
            let info = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([tx.order,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let bytes = gofin_solana::token::decode_rpc_account_data(&info["value"]["data"])?;
            if info["value"].get("owner").and_then(Value::as_str) != Some(PROGRAM) {
                return Err(bad_response());
            }
            let order = decode_order(&tx.order, &bytes)?;
            if order.making.to_string() != prepared.binding["amount_atoms"].as_str().unwrap_or("") {
                return Err(GoFinError::new(
                    "JUPITER_ORDER_CHANGED",
                    "revalidation",
                    "The remaining Trigger escrow changed after this cancellation was prepared.",
                    "Inspect the fill race and prepare a new cancellation for the current remaining amount.",
                ));
            }
        }
        if prepared.binding["account_funding_lamports"].as_u64()
            != Some(auxiliary.account_funding_lamports)
            || prepared.binding.get("funding_accounts") != Some(&json!(auxiliary.funding_accounts))
            || prepared.binding["wrapped_native_lamports"].as_u64()
                != Some(auxiliary.wrapped_native_lamports)
            || prepared.binding.get("closed_wrapped_native_accounts")
                != Some(&json!(auxiliary.closed_wrapped_native_accounts))
            || prepared.binding.get("closed_wrapped_native_refunds")
                != Some(&json!(auxiliary.closed_wrapped_native_refunds))
        {
            return Err(changed());
        }
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let tx = TriggerTx {
            encoded: prepared.binding["transaction_base64"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            request_id: prepared.binding["request_id"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            order: prepared.binding["order_key"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
        };
        let (frozen, _auxiliary) = self
            .inspect(&prepared.operation, &prepared.input, &wallet, &tx, true)
            .await?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(changed());
        }
        let sender = TriggerExecuteSender {
            api: self.api.clone(),
            request_id: tx.request_id,
            expires_at: prepared.expires_at.clone(),
        };
        wallet
            .runtime()
            .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
            .await
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let tx = TriggerTx {
            encoded: prepared.binding["transaction_base64"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            request_id: prepared.binding["request_id"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
            order: prepared.binding["order_key"]
                .as_str()
                .ok_or_else(bad_response)?
                .into(),
        };
        // Recheck the order, account graph, intent and fee after approval. The
        // broker has already simulated this exact candidate during final
        // revalidation, so the handoff omits only a second simulation.
        let (frozen, _) = self
            .inspect(&prepared.operation, &prepared.input, &wallet, &tx, false)
            .await?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(changed());
        }
        validated.assert_fresh()?;
        let sender = TriggerExecuteSender {
            api: self.api.clone(),
            request_id: tx.request_id,
            expires_at: prepared.expires_at.clone(),
        };
        wallet
            .runtime()
            .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Trigger transaction submission exists."}),
                verified: None,
            });
        };
        let wallet = self.wallet(&record.input)?;
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let verified = adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
                Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            }
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason}),
                verified: None,
            }),
            ReceiptOutcome::Finalized {
                signature,
                slot,
                fee_lamports,
                failed,
                proof,
                ..
            } => {
                if failed {
                    let verified = adapter::verify_failed_cost(
                        record,
                        submission,
                        &signature,
                        slot,
                        fee_lamports,
                        &proof,
                    )?;
                    return Ok(Reconciliation {
                        status: Status::Failed,
                        evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"proof":proof}),
                        verified: Some(verified),
                    });
                }
                let b = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
                let order = b["order_key"].as_str().ok_or_else(bad_response)?;
                let mint = b["input_mint"].as_str().ok_or_else(bad_response)?;
                let chain_mint = if mint == "native" { NATIVE } else { mint };
                let wallet_address = b["wallet_address"].as_str().ok_or_else(bad_response)?;
                let state = b["kind"].as_str().ok_or_else(bad_response)?;
                let input_token_program =
                    b["input_token_program"].as_str().ok_or_else(bad_response)?;
                let input_decimals = b["input_decimals"]
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(bad_response)?;
                if state == "cancellation" {
                    let operation_id = b["original_operation_id"]
                        .as_str()
                        .filter(|id| !id.trim().is_empty());
                    let original_submission = b
                        .get("original_submission")
                        .filter(|value| !value.is_null())
                        .cloned()
                        .map(serde_json::from_value::<Submission>)
                        .transpose()
                        .map_err(|_| bad_response())?;
                    if let (Some(operation_id), Some(original_submission)) =
                        (operation_id, original_submission.clone())
                    {
                        let cancel_event = program_event_data(&proof.log_messages, PROGRAM)
                            .into_iter()
                            .filter_map(|encoded| STANDARD.decode(encoded).ok())
                            .any(|data| decode_cancel_event(&data, order));
                        let wallet_delta = proof.token_delta_verified(
                            wallet_address,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?;
                        let order_delta = proof.token_delta_verified(
                            order,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?;
                        let expected = b["amount_atoms"]
                            .as_str()
                            .ok_or_else(bad_response)?
                            .parse::<u128>()
                            .map_err(|_| bad_response())?;
                        let returned = verified_cancel_return(
                            cancel_event,
                            &wallet_delta,
                            &order_delta,
                            expected,
                        )?;
                        let mut effects = vec![FinancialEffect::SubmissionCancellation {
                            operation_id: operation_id.to_owned(),
                            submission: original_submission,
                        }];
                        let requirements: Vec<adapter::AccountFundingRequirement> =
                            serde_json::from_value(
                                b.get("funding_accounts")
                                    .cloned()
                                    .ok_or_else(bad_response)?,
                            )
                            .map_err(|_| bad_response())?;
                        let mut total_funding = 0u64;
                        let mut wallet_funding = 0u64;
                        let mut account_proofs = Vec::new();
                        let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
                        let native = AssetId {
                            network: NETWORK.into(),
                            reference: "native".into(),
                            decimals: 9,
                        };
                        for requirement in requirements {
                            if requirement.refund_authority != wallet_address
                                || requirement.lamports == 0
                            {
                                return Err(bad_response());
                            }
                            let delta = proof
                                .lamport_delta(&requirement.address)?
                                .as_str()
                                .parse::<i128>()
                                .map_err(|_| bad_response())?;
                            if delta != 0 && delta != requirement.lamports as i128 {
                                return Err(GoFinError::new(
                                    "JUPITER_ACCOUNT_FUNDING_MISMATCH",
                                    "reconciliation",
                                    "Finalized cancellation does not prove zero or exact approved account rent.",
                                    "Keep the cancellation unresolved and inspect its finalized account creation proof.",
                                ));
                            }
                            total_funding = total_funding
                                .checked_add(delta as u64)
                                .ok_or_else(bad_response)?;
                            if delta == 0 {
                                account_proofs.push(json!({
                                    "account":requirement.address,
                                    "rent_lamports":"0",
                                    "verified_finalized":true
                                }));
                                continue;
                            }
                            if requirement.payer != wallet_address {
                                account_proofs.push(json!({
                                    "account":requirement.address,
                                    "rent_lamports":requirement.lamports.to_string(),
                                    "payer":requirement.payer,
                                    "verified_finalized":true
                                }));
                                continue;
                            }
                            wallet_funding = wallet_funding
                                .checked_add(requirement.lamports)
                                .ok_or_else(bad_response)?;
                            let principal = gofin_domain::Quantity::from_u64(requirement.lamports);
                            let position = format!("account-funding:{}", requirement.address);
                            let anchor = gofin_domain::PositionAnchor {
                                resource: resource.clone(),
                                position: position.clone(),
                                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                                principal: Amount {
                                    asset: native.clone(),
                                    atoms: principal.clone(),
                                },
                                scaled_receipt: None,
                                evidence_version: 1,
                                evidence: json!({
                                    "cluster":CLUSTER,
                                    "account":requirement.address,
                                    "account_owner":requirement.account_owner,
                                    "refund_authority":requirement.refund_authority,
                                    "rent_lamports":requirement.lamports,
                                    "proof_signature":signature,
                                    "proof_message_sha256":proof.message_sha256
                                }),
                                observed_at: Utc::now().to_rfc3339(),
                            };
                            effects.push(FinancialEffect::PositionChange {
                                resource: resource.clone(),
                                position: position.clone(),
                                asset: native.clone(),
                                delta: gofin_domain::SignedQuantity::positive(&principal),
                                scaled_receipt: None,
                                anchor: Some(anchor),
                            });
                            effects.push(FinancialEffect::OriginMovement {
                                from: AccountingLocation::Cash {
                                    resource: resource.clone(),
                                },
                                to: AccountingLocation::Position {
                                    resource: resource.clone(),
                                    position,
                                    position_kind:
                                        gofin_domain::PositionKind::RefundableAccountFunding,
                                },
                                debit: Amount {
                                    asset: native.clone(),
                                    atoms: principal.clone(),
                                },
                                credit: Amount {
                                    asset: native.clone(),
                                    atoms: principal,
                                },
                            });
                            account_proofs.push(json!({
                                "account":requirement.address,
                                "rent_lamports":requirement.lamports.to_string(),
                                "payer":requirement.payer,
                                "verified_finalized":true
                            }));
                        }
                        if total_funding
                            > b["account_funding_lamports"]
                                .as_u64()
                                .ok_or_else(bad_response)?
                        {
                            return Err(bad_response());
                        }
                        let mut wallet_debit = wallet_funding;
                        if b["fee_payer"].as_str() == Some(wallet_address) {
                            wallet_debit = wallet_debit
                                .checked_add(fee_lamports)
                                .ok_or_else(bad_response)?;
                        }
                        let parent_funding: Vec<adapter::AccountFundingRequirement> =
                            serde_json::from_value(
                                b.get("original_funding_accounts")
                                    .cloned()
                                    .ok_or_else(bad_response)?,
                            )
                            .map_err(|_| bad_response())?;
                        let mut parent_rent_return = 0u64;
                        let mut returned_parent_accounts = Vec::new();
                        for requirement in parent_funding {
                            if requirement.refund_authority != wallet_address {
                                continue;
                            }
                            let account_delta = proof
                                .lamport_delta(&requirement.address)?
                                .as_str()
                                .parse::<i128>()
                                .map_err(|_| bad_response())?;
                            if account_delta != 0
                                && account_delta != -(requirement.lamports as i128)
                            {
                                return Err(GoFinError::new(
                                    "JUPITER_ACCOUNT_FUNDING_MISMATCH",
                                    "reconciliation",
                                    "Finalized cancellation does not prove zero or the exact rent return for an account funded by the original order.",
                                    "Keep the cancellation unresolved and inspect the exact original order funding accounts.",
                                ));
                            }
                            if account_delta == -(requirement.lamports as i128) {
                                parent_rent_return = parent_rent_return
                                    .checked_add(requirement.lamports)
                                    .ok_or_else(bad_response)?;
                                returned_parent_accounts.push(json!({
                                    "account": requirement.address,
                                    "rent_lamports": requirement.lamports.to_string(),
                                    "refund_authority": requirement.refund_authority,
                                    "funding_owner": operation_id,
                                }));
                            }
                        }
                        let expected_wallet_delta = (parent_rent_return as i128)
                            .checked_sub(wallet_debit as i128)
                            .ok_or_else(bad_response)?;
                        if proof
                            .lamport_delta(wallet_address)?
                            .as_str()
                            .parse::<i128>()
                            .map_err(|_| bad_response())?
                            != expected_wallet_delta
                        {
                            return Err(GoFinError::new(
                                "JUPITER_NATIVE_BALANCE_MISMATCH",
                                "reconciliation",
                                "Finalized cancellation SOL changes do not match original-order rent returns, wallet-paid rent and network fees.",
                                "Keep the cancellation unresolved and inspect its full finalized lamport proof.",
                            ));
                        }
                        if wallet_funding > 0 {
                            effects.push(FinancialEffect::CashDebit {
                                resource: resource.clone(),
                                amount: Amount {
                                    asset: native.clone(),
                                    atoms: gofin_domain::Quantity::from_u64(wallet_funding),
                                },
                            });
                        }
                        if b["fee_payer"].as_str() == Some(wallet_address) && fee_lamports > 0 {
                            effects.push(native_cost(
                                wallet_address,
                                fee_lamports,
                                "solana_network_fee",
                            ));
                        }
                        let evidence = json!({
                            "cluster":CLUSTER,
                            "signature":signature,
                            "slot":slot,
                            "request_id":b["request_id"],
                            "order_key":order,
                            "returned_escrow_atoms":returned.to_string(),
                            "kind":"cancellation",
                            "fee_lamports":fee_lamports,
                            "account_funding":account_proofs,
                            "original_order_rent_returns":returned_parent_accounts,
                            "proof":proof,
                            "settlement_semantics":"cancellation_marker_only; original order operation owns fills and escrow refunds"
                        });
                        let mut verified = gofin_domain::VerifiedEffects {
                            evidence_version: 1,
                            submission: submission.clone(),
                            effects,
                            evidence,
                            observed_at: Utc::now().to_rfc3339(),
                            proof_digest: String::new(),
                        };
                        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                        return Ok(Reconciliation {
                            status: Status::Completed,
                            evidence: verified.evidence.clone(),
                            verified: Some(verified),
                        });
                    }
                    if operation_id.is_none() && original_submission.is_none() {
                        let cancel_event = program_event_data(&proof.log_messages, PROGRAM)
                            .into_iter()
                            .filter_map(|encoded| STANDARD.decode(encoded).ok())
                            .any(|data| decode_cancel_event(&data, order));
                        let wallet_delta = proof.token_delta_verified(
                            wallet_address,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?;
                        let order_delta = proof.token_delta_verified(
                            order,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?;
                        let maximum = b["amount_atoms"]
                            .as_str()
                            .ok_or_else(bad_response)?
                            .parse::<u128>()
                            .map_err(|_| bad_response())?;
                        // External orders have no local escrow journal to recover.
                        // The finalized program event and exact owner/order token
                        // deltas therefore have to establish the approved return.
                        verified_cancel_return(cancel_event, &wallet_delta, &order_delta, maximum)?;
                    }
                }
                let amount = b["amount_atoms"]
                    .as_str()
                    .ok_or_else(bad_response)?
                    .parse::<u128>()
                    .map_err(|_| bad_response())?;
                let (from, to, wallet_delta, order_delta) = if state == "placement" {
                    (
                        ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                        ResourceId(format!("solana:mainnet-beta:trigger-order:{order}")),
                        proof.token_delta_verified(
                            wallet_address,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?,
                        proof.token_delta_verified(
                            order,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?,
                    )
                } else {
                    (
                        ResourceId(format!("solana:mainnet-beta:trigger-order:{order}")),
                        ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                        proof.token_delta_verified(
                            wallet_address,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?,
                        proof.token_delta_verified(
                            order,
                            chain_mint,
                            input_token_program,
                            input_decimals,
                        )?,
                    )
                };
                let (wallet_change, order_change) = if state == "placement" {
                    if !wallet_delta.is_negative() || order_delta.is_negative() {
                        return Err(bad_response());
                    }
                    (
                        wallet_delta
                            .magnitude()
                            .as_str()
                            .parse::<u128>()
                            .map_err(|_| bad_response())?,
                        order_delta
                            .as_str()
                            .parse::<u128>()
                            .map_err(|_| bad_response())?,
                    )
                } else {
                    if wallet_delta.is_negative() || !order_delta.is_negative() {
                        return Err(bad_response());
                    }
                    (
                        wallet_delta
                            .as_str()
                            .parse::<u128>()
                            .map_err(|_| bad_response())?,
                        order_delta
                            .magnitude()
                            .as_str()
                            .parse::<u128>()
                            .map_err(|_| bad_response())?,
                    )
                };
                if wallet_change != amount || order_change != amount {
                    return Err(GoFinError::new(
                        "JUPITER_ESCROW_EFFECT_MISMATCH",
                        "reconciliation",
                        "Finalized token balances do not prove the exact Trigger escrow movement.",
                        "Keep the operation unresolved and inspect the finalized order-account effects.",
                    ));
                }
                let asset = record
                    .preview
                    .as_ref()
                    .and_then(|p| p.intent.expected.first())
                    .and_then(|e| match e {
                        EffectConstraint::EscrowLifecycle { input_asset, .. } => {
                            Some(input_asset.clone())
                        }
                        EffectConstraint::Transfer { debit_asset, .. } => Some(debit_asset.clone()),
                        _ => None,
                    })
                    .ok_or_else(bad_response)?;
                let is_placement = state == "placement";
                let mut effects = if is_placement {
                    let (wallet_resource, escrow_resource, output_asset) = record
                        .preview
                        .as_ref()
                        .and_then(|preview| preview.intent.expected.first())
                        .and_then(|expected| match expected {
                            EffectConstraint::EscrowLifecycle {
                                wallet_resource,
                                escrow_resource,
                                output_asset,
                                ..
                            } => Some((
                                wallet_resource.clone(),
                                escrow_resource.clone(),
                                output_asset.clone(),
                            )),
                            _ => None,
                        })
                        .ok_or_else(bad_response)?;
                    vec![FinancialEffect::EscrowProgress {
                        wallet_resource,
                        escrow_resource,
                        input_asset: asset,
                        output_asset,
                        wallet_debit: gofin_domain::Quantity::from_u64(amount as u64),
                        escrow_deposit: gofin_domain::Quantity::from_u64(amount as u64),
                        input_transfer_fee: gofin_domain::Quantity::from_u64(0),
                        input_filled: gofin_domain::Quantity::from_u64(0),
                        output_filled: gofin_domain::Quantity::from_u64(0),
                        input_refunded: gofin_domain::Quantity::from_u64(0),
                        remaining_input: gofin_domain::Quantity::from_u64(amount as u64),
                        state: gofin_domain::EscrowLifecycleState::Open,
                    }]
                } else {
                    vec![FinancialEffect::Transfer {
                        from,
                        to,
                        debit: Amount {
                            asset: asset.clone(),
                            atoms: gofin_domain::Quantity::from_u64(amount as u64),
                        },
                        credit: Amount {
                            asset,
                            atoms: gofin_domain::Quantity::from_u64(amount as u64),
                        },
                    }]
                };
                if is_placement {
                    let requirements: Vec<adapter::AccountFundingRequirement> =
                        serde_json::from_value(
                            b.get("funding_accounts")
                                .cloned()
                                .ok_or_else(bad_response)?,
                        )
                        .map_err(|_| bad_response())?;
                    let native = AssetId {
                        network: NETWORK.into(),
                        reference: "native".into(),
                        decimals: 9,
                    };
                    let mut actual_funding = 0u64;
                    let mut wallet_actual_funding = 0u64;
                    for requirement in requirements {
                        let delta = proof.lamport_delta(&requirement.address)?;
                        let atoms = delta.as_str().parse::<i128>().map_err(|_| bad_response())?;
                        if atoms < 0 || (atoms != 0 && atoms as u64 != requirement.lamports) {
                            return Err(GoFinError::new(
                                "JUPITER_ACCOUNT_FUNDING_MISMATCH",
                                "reconciliation",
                                "Finalized account balances do not prove zero or the exact approved refundable rent funding.",
                                "Keep the Trigger operation unresolved and inspect its finalized account-creation proof.",
                            ));
                        }
                        actual_funding = actual_funding
                            .checked_add(atoms as u64)
                            .ok_or_else(bad_response)?;
                        if requirement.payer != wallet_address {
                            continue;
                        }
                        wallet_actual_funding = wallet_actual_funding
                            .checked_add(atoms as u64)
                            .ok_or_else(bad_response)?;
                        let position = format!("account-funding:{}", requirement.address);
                        let funded = gofin_domain::Quantity::from_u64(atoms as u64);
                        let anchor = (atoms > 0).then(|| gofin_domain::PositionAnchor {
                            resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                            position: position.clone(),
                            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                            principal: Amount {
                                asset: native.clone(),
                                atoms: funded.clone(),
                            },
                            scaled_receipt: None,
                            evidence_version: 1,
                            evidence: json!({
                                "cluster": CLUSTER,
                                "account": requirement.address,
                                "account_owner": requirement.account_owner,
                                "refund_authority": requirement.refund_authority,
                                "funding_lamports": atoms,
                                "proof_signature": signature,
                                "proof_message_sha256": proof.message_sha256,
                            }),
                            observed_at: Utc::now().to_rfc3339(),
                        });
                        effects.push(FinancialEffect::AccountFundingProgress {
                            resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                            position,
                            account: requirement.address,
                            asset: native.clone(),
                            funded: funded.clone(),
                            returned: gofin_domain::Quantity::zero(),
                            remaining: funded.clone(),
                            state: if funded.is_zero() {
                                gofin_domain::AccountFundingState::Closed
                            } else {
                                gofin_domain::AccountFundingState::Open
                            },
                            anchor,
                        });
                    }
                    if actual_funding
                        > b["account_funding_lamports"]
                            .as_u64()
                            .ok_or_else(bad_response)?
                    {
                        return Err(bad_response());
                    }
                    let mut wallet_native_debit = wallet_actual_funding;
                    wallet_native_debit = wallet_native_debit
                        .checked_add(
                            b["wrapped_native_lamports"]
                                .as_u64()
                                .ok_or_else(bad_response)?,
                        )
                        .ok_or_else(bad_response)?;
                    if b["fee_payer"].as_str() == Some(wallet_address) {
                        wallet_native_debit = wallet_native_debit
                            .checked_add(fee_lamports)
                            .ok_or_else(bad_response)?;
                    }
                    let wallet_delta = proof.lamport_delta(wallet_address)?;
                    let delta = wallet_delta
                        .as_str()
                        .parse::<i128>()
                        .map_err(|_| bad_response())?;
                    if delta != -(wallet_native_debit as i128) {
                        return Err(GoFinError::new(
                            "JUPITER_NATIVE_BALANCE_MISMATCH",
                            "reconciliation",
                            "Finalized wallet SOL changes do not match network fees, rent funding and native wrapping.",
                            "Keep the Trigger operation unresolved and inspect its complete finalized lamport proof.",
                        ));
                    }
                }
                if b["fee_payer"].as_str() == Some(wallet_address) && fee_lamports > 0 {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                        },
                        amount: Amount {
                            asset: AssetId {
                                network: NETWORK.into(),
                                reference: "native".into(),
                                decimals: 9,
                            },
                            atoms: gofin_domain::Quantity::from_u64(fee_lamports),
                        },
                        category: "solana_network_fee".into(),
                        treatment: CostTreatment::AdditionalDebit,
                    })
                }
                let mut evidence = json!({"cluster":CLUSTER,"signature":signature,"slot":slot,"request_id":b["request_id"],"order_key":order,"kind":state,"fee_lamports":fee_lamports,"proof":proof,"settlement_semantics":"escrow_movement_only_not_swap_fill"});
                if is_placement {
                    let event_digest = gofin_domain::binding_hash(
                        &json!({"signature":signature,"slot":slot,"message_sha256":proof.message_sha256,"proof":proof}),
                    )?;
                    let history = self.order_history(&wallet, order, None).await?;
                    let history_state = history
                        .get("state")
                        .and_then(Value::as_str)
                        .ok_or_else(bad_response)?;
                    if history.get("original_making_atoms").and_then(Value::as_str)
                        != Some(amount.to_string().as_str())
                        || history.get("original_taking_atoms").and_then(Value::as_str)
                            != b["output_atoms"].as_str()
                    {
                        return Err(bad_response());
                    }
                    let mut event_digests: BTreeMap<String, String> = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            gofin_domain::SettlementProgress::from_evidence(&effects.evidence)
                                .ok()
                                .flatten()
                        })
                        .map(|progress| progress.event_digests)
                        .unwrap_or_default();
                    let previous_sequence = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            gofin_domain::SettlementProgress::from_evidence(&effects.evidence)
                                .ok()
                                .flatten()
                        })
                        .map(|progress| progress.sequence)
                        .unwrap_or(0);
                    let placement_id = format!("placement:{signature}");
                    if event_digests
                        .insert(placement_id, event_digest.clone())
                        .is_some_and(|existing| existing != event_digest)
                    {
                        return Err(bad_response());
                    }
                    let history_digests: BTreeMap<String, String> = serde_json::from_value(
                        history
                            .get("event_digests")
                            .cloned()
                            .ok_or_else(bad_response)?,
                    )
                    .map_err(|_| bad_response())?;
                    for (identity, digest) in history_digests {
                        if event_digests
                            .insert(identity, digest.clone())
                            .is_some_and(|existing| existing != digest)
                        {
                            return Err(bad_response());
                        }
                    }
                    let mut input_filled = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            effects.effects.iter().find_map(|effect| match effect {
                                FinancialEffect::EscrowProgress { input_filled, .. } => {
                                    input_filled.integer().to_string().parse::<u128>().ok()
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    let mut output_filled = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            effects.effects.iter().find_map(|effect| match effect {
                                FinancialEffect::EscrowProgress { output_filled, .. } => {
                                    output_filled.integer().to_string().parse::<u128>().ok()
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    let old_event_ids = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            gofin_domain::SettlementProgress::from_evidence(&effects.evidence)
                                .ok()
                                .flatten()
                        })
                        .map(|progress| {
                            progress
                                .event_digests
                                .into_keys()
                                .collect::<std::collections::BTreeSet<_>>()
                        })
                        .unwrap_or_default();
                    for fill in history
                        .get("verified_fills")
                        .and_then(Value::as_array)
                        .ok_or_else(bad_response)?
                    {
                        let event_id = fill
                            .get("event_id")
                            .and_then(Value::as_str)
                            .ok_or_else(bad_response)?;
                        if old_event_ids.contains(event_id) {
                            continue;
                        }
                        input_filled = input_filled
                            .checked_add(
                                fill.get("making_atoms")
                                    .and_then(Value::as_str)
                                    .ok_or_else(bad_response)?
                                    .parse::<u128>()
                                    .map_err(|_| bad_response())?,
                            )
                            .ok_or_else(bad_response)?;
                        output_filled = output_filled
                            .checked_add(
                                fill.get("taking_atoms")
                                    .and_then(Value::as_str)
                                    .ok_or_else(bad_response)?
                                    .parse::<u128>()
                                    .map_err(|_| bad_response())?,
                            )
                            .ok_or_else(bad_response)?;
                    }
                    let deposit = amount;
                    if input_filled > deposit {
                        return Err(bad_response());
                    }
                    let previous_refund = record
                        .effects
                        .as_ref()
                        .and_then(|effects| {
                            effects.effects.iter().find_map(|effect| match effect {
                                FinancialEffect::EscrowProgress { input_refunded, .. } => {
                                    input_refunded.integer().to_string().parse::<u128>().ok()
                                }
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    let terminal_fill = history_state == "filled";
                    let terminal_cancel = history_state == "cancelled";
                    if (terminal_fill || terminal_cancel)
                        && history
                            .get("bounded_rpc_scan_complete")
                            .and_then(Value::as_bool)
                            != Some(true)
                    {
                        return Err(bad_response());
                    }
                    if terminal_cancel {
                        let cancel_event = history
                            .get("verified_cancel_events")
                            .and_then(Value::as_array)
                            .and_then(|events| events.first())
                            .ok_or_else(bad_response)?;
                        let cancel_signature = cancel_event
                            .get("signature")
                            .and_then(Value::as_str)
                            .ok_or_else(bad_response)?;
                        let cancel_proof = wallet
                            .runtime()
                            .observe_finalized_proof(cancel_signature)
                            .await?;
                        if cancel_proof.error.is_some()
                            || !program_event_data(&cancel_proof.log_messages, PROGRAM)
                                .into_iter()
                                .filter_map(|encoded| STANDARD.decode(encoded).ok())
                                .any(|data| decode_cancel_event(&data, order))
                        {
                            return Err(bad_response());
                        }
                        let requirements: Vec<adapter::AccountFundingRequirement> =
                            serde_json::from_value(
                                b.get("funding_accounts")
                                    .cloned()
                                    .ok_or_else(bad_response)?,
                            )
                            .map_err(|_| bad_response())?;
                        let native = AssetId {
                            network: NETWORK.into(),
                            reference: "native".into(),
                            decimals: 9,
                        };
                        let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
                        for requirement in requirements
                            .into_iter()
                            .filter(|requirement| requirement.payer == wallet_address)
                        {
                            let Some(index) = effects.iter().position(|effect| {
                                matches!(effect,
                                    FinancialEffect::AccountFundingProgress { account, .. }
                                    if account == &requirement.address)
                            }) else {
                                return Err(bad_response());
                            };
                            let (position, asset, funded, mut returned, mut anchor) =
                                match effects.get(index).ok_or_else(bad_response)? {
                                    FinancialEffect::AccountFundingProgress {
                                        position,
                                        asset,
                                        funded,
                                        returned,
                                        anchor,
                                        ..
                                    } => (
                                        position.clone(),
                                        asset.clone(),
                                        funded.clone(),
                                        returned.clone(),
                                        anchor.clone(),
                                    ),
                                    _ => return Err(bad_response()),
                                };
                            if asset != native
                                || funded > gofin_domain::Quantity::from_u64(requirement.lamports)
                            {
                                return Err(bad_response());
                            }
                            let delta = cancel_proof
                                .lamport_delta(&requirement.address)?
                                .as_str()
                                .parse::<i128>()
                                .map_err(|_| bad_response())?;
                            if delta != 0
                                && delta
                                    != -(funded
                                        .as_str()
                                        .parse::<i128>()
                                        .map_err(|_| bad_response())?)
                            {
                                return Err(bad_response());
                            }
                            if delta < 0 {
                                returned = funded.clone();
                            }
                            let remaining = funded.checked_sub(&returned)?;
                            if remaining.is_zero() {
                                if let Some(existing) = anchor.as_mut() {
                                    existing.principal.atoms = gofin_domain::Quantity::zero();
                                    existing.evidence = json!({
                                        "cluster":CLUSTER,
                                        "account":requirement.address,
                                        "account_owner":requirement.account_owner,
                                        "refund_authority":requirement.refund_authority,
                                        "funding_lamports":requirement.lamports,
                                        "returned_lamports":requirement.lamports,
                                        "proof_signature":cancel_signature,
                                        "proof_message_sha256":cancel_proof.message_sha256,
                                    });
                                    existing.observed_at = Utc::now().to_rfc3339();
                                }
                            } else if let Some(existing) = anchor.as_mut() {
                                existing.principal.atoms = remaining.clone();
                            } else {
                                return Err(bad_response());
                            }
                            effects[index] = FinancialEffect::AccountFundingProgress {
                                resource: resource.clone(),
                                position,
                                account: requirement.address,
                                asset: native.clone(),
                                funded,
                                returned,
                                remaining: remaining.clone(),
                                state: if remaining.is_zero() {
                                    gofin_domain::AccountFundingState::Closed
                                } else {
                                    gofin_domain::AccountFundingState::Open
                                },
                                anchor,
                            };
                        }
                    } else if terminal_fill {
                        let final_fill = history
                            .get("verified_fills")
                            .and_then(Value::as_array)
                            .and_then(|fills| fills.last())
                            .ok_or_else(bad_response)?;
                        let fill_signature = final_fill
                            .get("signature")
                            .and_then(Value::as_str)
                            .ok_or_else(bad_response)?;
                        let fill_slot = final_fill
                            .get("slot")
                            .and_then(Value::as_u64)
                            .ok_or_else(bad_response)?;
                        let fill_proof = wallet
                            .runtime()
                            .observe_finalized_proof(fill_signature)
                            .await?;
                        if fill_proof.failed
                            || fill_proof.error.is_some()
                            || fill_proof.signature != fill_signature
                            || fill_proof.slot != fill_slot
                        {
                            return Err(bad_response());
                        }
                        let requirements: Vec<adapter::AccountFundingRequirement> =
                            serde_json::from_value(
                                b.get("funding_accounts")
                                    .cloned()
                                    .ok_or_else(bad_response)?,
                            )
                            .map_err(|_| bad_response())?;
                        let native = AssetId {
                            network: NETWORK.into(),
                            reference: "native".into(),
                            decimals: 9,
                        };
                        let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
                        let mut returned_lamports = 0u64;
                        let mut returned_accounts = Vec::new();
                        for requirement in requirements.into_iter().filter(|requirement| {
                            requirement.payer == wallet_address
                                && requirement.refund_authority == wallet_address
                        }) {
                            let Some(index) = effects.iter().position(|effect| {
                                matches!(effect,
                                    FinancialEffect::AccountFundingProgress { account, .. }
                                    if account == &requirement.address)
                            }) else {
                                return Err(bad_response());
                            };
                            let (position, asset, funded, mut returned, mut anchor) =
                                match effects.get(index).ok_or_else(bad_response)? {
                                    FinancialEffect::AccountFundingProgress {
                                        position,
                                        asset,
                                        funded,
                                        returned,
                                        anchor,
                                        ..
                                    } => (
                                        position.clone(),
                                        asset.clone(),
                                        funded.clone(),
                                        returned.clone(),
                                        anchor.clone(),
                                    ),
                                    _ => return Err(bad_response()),
                                };
                            if asset != native
                                || funded > gofin_domain::Quantity::from_u64(requirement.lamports)
                            {
                                return Err(bad_response());
                            }
                            // A rent return is recognized only when this
                            // finalized fill receipt names the funded account
                            // and proves its exact full lamport decrease. An
                            // absent account in a later RPC snapshot is not
                            // evidence that rent was returned by this fill.
                            if !fill_proof
                                .account_keys
                                .iter()
                                .any(|key| key == &requirement.address)
                            {
                                continue;
                            }
                            let delta = fill_proof
                                .lamport_delta(&requirement.address)?
                                .as_str()
                                .parse::<i128>()
                                .map_err(|_| bad_response())?;
                            if delta == 0 {
                                continue;
                            }
                            let funded_lamports =
                                funded.as_str().parse::<u64>().map_err(|_| bad_response())?;
                            let account_indexes: Vec<usize> = fill_proof
                                .account_keys
                                .iter()
                                .enumerate()
                                .filter_map(|(index, key)| {
                                    (key == &requirement.address).then_some(index)
                                })
                                .collect();
                            if account_indexes.len() != 1 {
                                return Err(bad_response());
                            }
                            let account_index = account_indexes[0];
                            let pre_lamports = *fill_proof
                                .pre_balances_lamports
                                .get(account_index)
                                .ok_or_else(bad_response)?;
                            let post_lamports = *fill_proof
                                .post_balances_lamports
                                .get(account_index)
                                .ok_or_else(bad_response)?;
                            if funded_lamports == 0
                                || delta != -(funded_lamports as i128)
                                || !exact_full_account_rent_close(
                                    pre_lamports,
                                    post_lamports,
                                    funded_lamports,
                                )
                                || !returned.is_zero()
                            {
                                return Err(GoFinError::new(
                                    "JUPITER_ACCOUNT_FUNDING_MISMATCH",
                                    "reconciliation",
                                    "A finalized full fill did not prove either retained order accounts or their exact rent return.",
                                    "Keep the Trigger operation unresolved and inspect the finalized fill account and lamport proof.",
                                ));
                            }
                            returned = funded.clone();
                            returned_lamports = returned_lamports
                                .checked_add(funded_lamports)
                                .ok_or_else(bad_response)?;
                            if let Some(existing) = anchor.as_mut() {
                                existing.principal.atoms = gofin_domain::Quantity::zero();
                                existing.evidence = json!({
                                    "cluster": CLUSTER,
                                    "account": requirement.address,
                                    "account_owner": requirement.account_owner,
                                    "refund_authority": requirement.refund_authority,
                                    "funding_lamports": funded_lamports,
                                    "returned_lamports": funded_lamports,
                                    "proof_signature": fill_signature,
                                    "proof_slot": fill_slot,
                                    "proof_message_sha256": fill_proof.message_sha256,
                                });
                                existing.observed_at = Utc::now().to_rfc3339();
                            }
                            let remaining = funded.checked_sub(&returned)?;
                            effects[index] = FinancialEffect::AccountFundingProgress {
                                resource: resource.clone(),
                                position,
                                account: requirement.address.clone(),
                                asset: native.clone(),
                                funded,
                                returned,
                                remaining,
                                state: gofin_domain::AccountFundingState::Closed,
                                anchor,
                            };
                            returned_accounts.push(json!({
                                "account": requirement.address,
                                "rent_lamports": funded_lamports.to_string(),
                                "refund_authority": wallet_address,
                                "proof_signature": fill_signature,
                                "verified_finalized": true,
                            }));
                        }
                        let wallet_is_fee_payer =
                            fill_proof.account_keys.first().map(String::as_str)
                                == Some(wallet_address);
                        if fill_proof
                            .account_keys
                            .iter()
                            .any(|key| key == wallet_address)
                        {
                            let expected_delta = (returned_lamports as i128)
                                .checked_sub(if wallet_is_fee_payer {
                                    fill_proof.fee_lamports as i128
                                } else {
                                    0
                                })
                                .ok_or_else(bad_response)?;
                            let actual_delta = fill_proof
                                .lamport_delta(wallet_address)?
                                .as_str()
                                .parse::<i128>()
                                .map_err(|_| bad_response())?;
                            if actual_delta != expected_delta {
                                return Err(GoFinError::new(
                                    "JUPITER_NATIVE_BALANCE_MISMATCH",
                                    "reconciliation",
                                    "Finalized full-fill SOL changes do not match the exact rent returns and transaction fee.",
                                    "Keep the Trigger operation unresolved and inspect the finalized fill lamport proof.",
                                ));
                            }
                        } else if returned_lamports > 0 || wallet_is_fee_payer {
                            return Err(bad_response());
                        }
                        evidence["full_fill_account_rent_returns"] = json!({"signature":fill_signature,"slot":fill_slot,"returned_accounts":returned_accounts,"returned_lamports":returned_lamports.to_string()});
                    }
                    let input_refunded = if terminal_cancel {
                        deposit.checked_sub(input_filled).ok_or_else(bad_response)?
                    } else {
                        previous_refund
                    };
                    let remaining = if terminal_fill || terminal_cancel {
                        0
                    } else if let Some(onchain_remaining) = history
                        .get("remaining_making_atoms")
                        .and_then(Value::as_str)
                    {
                        onchain_remaining
                            .parse::<u128>()
                            .map_err(|_| bad_response())?
                    } else {
                        deposit
                            .checked_sub(input_filled)
                            .and_then(|left| left.checked_sub(input_refunded))
                            .ok_or_else(bad_response)?
                    };
                    if deposit
                        != input_filled
                            .checked_add(input_refunded)
                            .and_then(|left| left.checked_add(remaining))
                            .ok_or_else(bad_response)?
                    {
                        return Err(bad_response());
                    }
                    if remaining == 0 && !terminal_fill && !terminal_cancel {
                        return Ok(Reconciliation {
                            status: Status::Pending,
                            evidence: json!({
                                "order_key":order,
                                "state":history_state,
                                "reason":"The order appears closed, but finalized terminal closure evidence is incomplete.",
                                "order_history":history,
                            }),
                            verified: None,
                        });
                    }
                    let escrow = effects
                        .iter_mut()
                        .find(|effect| matches!(effect, FinancialEffect::EscrowProgress { .. }))
                        .ok_or_else(bad_response)?;
                    if let FinancialEffect::EscrowProgress {
                        input_filled: filled,
                        output_filled: output,
                        input_refunded: refunded,
                        remaining_input,
                        state: lifecycle_state,
                        ..
                    } = escrow
                    {
                        *filled = gofin_domain::Quantity::new(input_filled.to_string())?;
                        *output = gofin_domain::Quantity::new(output_filled.to_string())?;
                        *refunded = gofin_domain::Quantity::new(input_refunded.to_string())?;
                        *remaining_input = gofin_domain::Quantity::new(remaining.to_string())?;
                        *lifecycle_state = if terminal_fill {
                            gofin_domain::EscrowLifecycleState::Filled
                        } else if terminal_cancel {
                            gofin_domain::EscrowLifecycleState::Cancelled
                        } else {
                            gofin_domain::EscrowLifecycleState::Open
                        };
                    }
                    let sequence = previous_sequence.checked_add(1).ok_or_else(bad_response)?;
                    evidence["order_history"] = history;
                    evidence["settlement_progress"] = json!({"version":1,"sequence":sequence,"snapshot_id":format!("trigger:{}:{}",order,sequence),"event_digests":event_digests});
                }
                let placement_terminal = is_placement
                    && matches!(
                        evidence["order_history"]["state"].as_str(),
                        Some("filled" | "cancelled")
                    );
                let mut verified = gofin_domain::VerifiedEffects {
                    evidence_version: 1,
                    submission: submission.clone(),
                    effects,
                    evidence,
                    observed_at: Utc::now().to_rfc3339(),
                    proof_digest: String::new(),
                };
                verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                Ok(Reconciliation {
                    // An exact placement proves only that principal entered
                    // escrow. Keep the original operation and its reservation
                    // open until a finalized fill, refund, or expiry is proved.
                    status: if placement_terminal {
                        Status::Completed
                    } else if is_placement {
                        Status::Pending
                    } else {
                        Status::Completed
                    },
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            }
        }
    }
}

#[derive(Clone)]
struct TriggerTx {
    encoded: String,
    request_id: String,
    order: String,
}
struct TriggerExecuteSender {
    api: Arc<dyn JupiterApi>,
    request_id: String,
    expires_at: String,
}
impl TriggerExecuteSender {
    async fn send_checked(
        &self,
        encoded: &str,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<String> {
        let max_wait = crate::api::remaining_until_deadline(&self.expires_at)?.min(DEADLINE);
        let v = self
            .api
            .request_checked(
                JupiterHost::Platform,
                Method::POST,
                "/trigger/v1/execute",
                Some(json!({"requestId":self.request_id,"signedTransaction":encoded})),
                RateBucket::Main,
                RequestPriority::Financial,
                max_wait,
                before_send,
                None,
            )
            .await?;
        let signature = v
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(uncertain)?;
        if v.get("status").and_then(Value::as_str) != Some("Success")
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |x| x.len() != 64)
        {
            return Err(uncertain());
        }
        Ok(signature.into())
    }
}
#[async_trait]
impl TransactionSender for TriggerExecuteSender {
    fn submission_reference(&self, frozen: &FrozenTransaction, _: Option<&str>) -> Result<String> {
        if frozen.provider_request_id() != Some(self.request_id.as_str()) {
            return Err(bad_response());
        }
        Ok(format!("jupiter:limit:{}", self.request_id))
    }
    async fn send_signed(&self, encoded: &str, _: &FrozenTransaction) -> Result<String> {
        self.send_checked(encoded, &|| Ok(())).await
    }

    async fn send_signed_checked(
        &self,
        encoded: &str,
        _: &FrozenTransaction,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
    ) -> Result<String> {
        self.send_checked(encoded, before_send).await
    }

    async fn send_signed_with_guard(
        &self,
        encoded: &str,
        _: &FrozenTransaction,
        guard: &dyn gofin_solana::transaction::AsyncDispatchGuard,
    ) -> Result<String> {
        let max_wait = crate::api::remaining_until_deadline(&self.expires_at)?.min(DEADLINE);
        let value = self
            .api
            .request_checked(
                JupiterHost::Platform,
                Method::POST,
                "/trigger/v1/execute",
                Some(json!({"requestId":self.request_id,"signedTransaction":encoded})),
                RateBucket::Main,
                RequestPriority::Financial,
                max_wait,
                &|| Ok(()),
                Some(guard),
            )
            .await?;
        let signature = value
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(uncertain)?;
        if value.get("status").and_then(Value::as_str) != Some("Success")
            || bs58::decode(signature)
                .into_vec()
                .map_or(true, |bytes| bytes.len() != 64)
        {
            return Err(uncertain());
        }
        Ok(signature.into())
    }
}

async fn validate_trigger_instructions(
    operation: &str,
    input: &Value,
    wallet: &SolanaWalletAdapter,
    order: &str,
    frozen: &FrozenTransaction,
) -> Result<()> {
    let mut main = 0;
    for ix in frozen.instructions() {
        if ix.program_id == COMPUTE {
            continue;
        }
        if ix.program_id == PROGRAM {
            main += 1;
            let data = STANDARD.decode(&ix.data_base64).map_err(|_| unsafe_tx())?;
            if operation == CREATE {
                if data.len() < 8 + 8 + 8 + 8 + 1 + 1 + 1
                    || data[..8] != INITIALIZE_DISC
                    || ix.accounts.len() != 15
                    || ix.accounts[1].address != wallet.address()
                    || ix.accounts[2].address != order
                    || !ix.accounts[0].signer
                    || !ix.accounts[0].writable
                    || ix.accounts[0].address != wallet.address()
                    || !ix.accounts[1].signer
                    || ix.accounts[2].signer
                    || !ix.accounts[2].writable
                {
                    return Err(unsafe_tx());
                }
                let maker = atoms(input, "making_amount_atoms")?;
                let taker = atoms(input, "taking_amount_atoms")?;
                if u64::from_le_bytes(data[16..24].try_into().map_err(|_| unsafe_tx())?) != maker
                    || u64::from_le_bytes(data[24..32].try_into().map_err(|_| unsafe_tx())?)
                        != taker
                {
                    return Err(unsafe_tx());
                }
                let pda = derive_order(
                    wallet.address(),
                    u64::from_le_bytes(data[8..16].try_into().map_err(|_| unsafe_tx())?),
                )?;
                if pda != order {
                    return Err(unsafe_tx());
                }
                let in_mint = mint(input, "input_mint")?;
                let out_mint = mint(input, "output_mint")?;
                let input_program = ix.accounts[9].address.as_str();
                let output_program = ix.accounts[10].address.as_str();
                let maker_input = expected_associated(wallet.address(), input_program, &in_mint)?;
                let fee = derive_program_address(&[b"fee"])?;
                let event_authority = derive_program_address(&[b"__event_authority"])?;
                if ix.accounts[7].address != in_mint
                    || ix.accounts[8].address != out_mint
                    || ix.accounts[3].address != expected_reserve(order, input_program, &in_mint)?
                    || !valid_token_program(input_program)
                    || !valid_token_program(output_program)
                    || ix.accounts[4].address != maker_input
                    || ix.accounts[5].address != fee
                    || ix.accounts[6].address != PROGRAM
                    || ix.accounts[11].address != SYSTEM
                    || ix.accounts[12].address != ASSOCIATED
                    || ix.accounts[13].address != event_authority
                    || ix.accounts[14].address != PROGRAM
                {
                    return Err(unsafe_tx());
                }
                for (mint_address, token_program) in [
                    (in_mint.as_str(), input_program),
                    (out_mint.as_str(), output_program),
                ] {
                    let account = wallet
                        .runtime()
                        .rpc()
                        .request(
                            "getAccountInfo",
                            json!([mint_address,{"encoding":"base64","commitment":"finalized"}]),
                        )
                        .await?;
                    if account
                        .get("value")
                        .and_then(|v| v.get("owner"))
                        .and_then(Value::as_str)
                        != Some(token_program)
                    {
                        return Err(unsafe_tx());
                    }
                }
                let expiry = decode_initialize_args(&data)?;
                if expiry != input.get("expired_at_unix").and_then(Value::as_i64)
                    || !ix.accounts[3].writable
                    || !ix.accounts[4].writable
                    || ix.accounts[3].signer
                    || ix.accounts[4].signer
                {
                    return Err(unsafe_tx());
                }
            } else if data.len() != 8
                || data[..8] != CANCEL_DISC
                || ix.accounts.len() != 9
                || ix.accounts[0].address != wallet.address()
                || ix.accounts[1].address != wallet.address()
                || ix.accounts[2].address != order
                || !ix.accounts[0].signer
                || !ix.accounts[1].writable
                || !ix.accounts[2].writable
            {
                return Err(unsafe_tx());
            } else {
                // The optional maker ATA is either the Anchor absent-account
                // sentinel or the canonical ATA for this exact order mint.
                let info = wallet
                    .runtime()
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([order,{"encoding":"base64","commitment":"finalized"}]),
                    )
                    .await?;
                let order_data = gofin_solana::token::decode_rpc_account_data(
                    info.get("value")
                        .and_then(|v| v.get("data"))
                        .ok_or_else(bad_response)?,
                )?;
                let decoded = decode_order(order, &order_data)?;
                let event_authority = derive_program_address(&[b"__event_authority"])?;
                let expected_ata = expected_associated(
                    wallet.address(),
                    &decoded.input_token_program,
                    &decoded.input_mint,
                )?;
                if ix.accounts[3].address != decoded.input_reserve
                    || (ix.accounts[4].address != expected_ata && ix.accounts[4].address != PROGRAM)
                    || ix.accounts[5].address != decoded.input_mint
                    || ix.accounts[6].address != decoded.input_token_program
                    || ix.accounts[7].address != event_authority
                    || ix.accounts[8].address != PROGRAM
                    || !valid_token_program(&ix.accounts[6].address)
                    || !ix.accounts[3].writable
                    || !ix.accounts[4].writable && ix.accounts[4].address != PROGRAM
                {
                    return Err(unsafe_tx());
                }
            }
        } else if ix.program_id == ASSOCIATED {
            // Exact ATA derivation, ownership, rent and supported lifecycle
            // are checked by validate_auxiliary_instructions above.
            continue;
        } else if ix.program_id == SYSTEM {
            // System transfers for native wrapping are validated with exact
            // amount and canonical WSOL ATA by the shared auxiliary checker.
            continue;
        } else if ix.program_id == TOKEN || ix.program_id == TOKEN_2022 {
            // Token close/sync operations are validated by the shared helper.
            continue;
        } else {
            return Err(unsafe_tx());
        }
    }
    if main != 1 {
        return Err(unsafe_tx());
    }
    Ok(())
}

fn valid_token_program(program: &str) -> bool {
    program == TOKEN || program == TOKEN_2022
}

fn derive_program_address(seeds: &[&[u8]]) -> Result<String> {
    let program = Pubkey::try_from(PROGRAM).map_err(|_| unsafe_tx())?;
    Ok(Pubkey::find_program_address(seeds, &program).0.to_string())
}

fn expected_associated(owner: &str, token_program: &str, mint: &str) -> Result<String> {
    let owner = Pubkey::try_from(owner).map_err(|_| unsafe_tx())?;
    let token = Pubkey::try_from(token_program).map_err(|_| unsafe_tx())?;
    let mint = Pubkey::try_from(mint).map_err(|_| unsafe_tx())?;
    let associated = Pubkey::try_from(ASSOCIATED).map_err(|_| unsafe_tx())?;
    Ok(Pubkey::find_program_address(
        &[owner.as_ref(), token.as_ref(), mint.as_ref()],
        &associated,
    )
    .0
    .to_string())
}

fn decode_initialize_args(data: &[u8]) -> Result<Option<i64>> {
    let mut offset = 32;
    let expiry = match *data.get(offset).ok_or_else(unsafe_tx)? {
        0 => {
            offset += 1;
            None
        }
        1 => {
            offset += 1;
            let value = i64::from_le_bytes(
                data.get(offset..offset + 8)
                    .ok_or_else(unsafe_tx)?
                    .try_into()
                    .map_err(|_| unsafe_tx())?,
            );
            offset += 8;
            Some(value)
        }
        _ => return Err(unsafe_tx()),
    };
    // Fees and slippage are optional u16 values. This adapter currently
    // permits only None, keeping the order's protocol charges at zero.
    for _ in 0..2 {
        if data.get(offset) != Some(&0) {
            return Err(unsafe_tx());
        }
        offset += 1;
    }
    if offset != data.len() {
        return Err(unsafe_tx());
    }
    Ok(expiry)
}

// Trigger V1 accountsStrict layout; creation is additionally checked against
// its PDA seeds from the pinned SDK IDL.
fn derive_order(maker: &str, unique: u64) -> Result<String> {
    let maker = Pubkey::try_from(maker).map_err(|_| unsafe_tx())?;
    let program = Pubkey::try_from(PROGRAM).map_err(|_| unsafe_tx())?;
    Ok(
        Pubkey::find_program_address(&[b"order", maker.as_ref(), &unique.to_le_bytes()], &program)
            .0
            .to_string(),
    )
}
fn expected_reserve(order: &str, token_program: &str, mint: &str) -> Result<String> {
    let order = Pubkey::try_from(order).map_err(|_| unsafe_tx())?;
    let token = Pubkey::try_from(token_program).map_err(|_| unsafe_tx())?;
    let mint = Pubkey::try_from(mint).map_err(|_| unsafe_tx())?;
    let associated = Pubkey::try_from(ASSOCIATED).map_err(|_| unsafe_tx())?;
    Ok(Pubkey::find_program_address(
        &[order.as_ref(), token.as_ref(), mint.as_ref()],
        &associated,
    )
    .0
    .to_string())
}
#[derive(Debug)]
struct DecodedOrder {
    key: String,
    maker: String,
    input_mint: String,
    output_mint: String,
    original_making: u64,
    original_taking: u64,
    making: u64,
    taking: u64,
    expired_at: Option<i64>,
    unique_id: u64,
    input_token_program: String,
    output_token_program: String,
    input_reserve: String,
}

struct TradeEvent {
    taker: String,
    remaining_making: u64,
    remaining_taking: u64,
    making: u64,
    taking: u64,
}

fn advance_trigger_remainder(previous: (u64, u64), making: u64, taking: u64) -> Option<(u64, u64)> {
    if making == 0 || taking == 0 {
        return None;
    }
    Some((
        previous.0.checked_sub(making)?,
        previous.1.checked_sub(taking)?,
    ))
}

struct TriggerRemainderReport {
    quote_remaining: Option<(u64, u64)>,
    escrowed_making: Option<u64>,
    returned_making: Option<u64>,
}

fn trigger_remainder_report(
    cancelled: bool,
    fully_filled: bool,
    order_open: bool,
    onchain_remaining: Option<(u64, u64)>,
    historical_remaining: Option<(u64, u64)>,
) -> TriggerRemainderReport {
    TriggerRemainderReport {
        quote_remaining: if cancelled {
            historical_remaining
        } else if order_open {
            onchain_remaining
        } else {
            historical_remaining
        },
        escrowed_making: if cancelled || fully_filled {
            Some(0)
        } else if order_open {
            onchain_remaining.map(|remainder| remainder.0)
        } else {
            None
        },
        returned_making: if cancelled {
            historical_remaining.map(|remainder| remainder.0)
        } else {
            None
        },
    }
}

fn trigger_status_state(history_state: &str, expired_at_finalized_time: bool) -> &str {
    match history_state {
        "open_escrowed" | "expired_escrowed" => {
            if expired_at_finalized_time {
                "expired_escrowed"
            } else {
                "open_escrowed"
            }
        }
        // A finalized event/history classification is stronger than the
        // account's generic existence. In particular, a zero-remainder order
        // with a verified full-fill event must remain Filled.
        state => state,
    }
}

fn verified_trigger_fill_remainder(
    previous: (u64, u64),
    making: u64,
    taking: u64,
    reported_remaining: (u64, u64),
) -> Option<(u64, u64)> {
    let next = advance_trigger_remainder(previous, making, taking)?;
    (next == reported_remaining).then_some(next)
}

fn canonical_block_signature_indices(block: &Value) -> Result<BTreeMap<String, usize>> {
    let rows = block
        .get("signatures")
        .and_then(Value::as_array)
        .or_else(|| block.get("transactions").and_then(Value::as_array))
        .ok_or_else(bad_response)?;
    let mut indices = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        let signature = row
            .as_str()
            .or_else(|| {
                row.as_array()
                    .and_then(|signatures| signatures.first())
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                row.get("signatures")
                    .and_then(Value::as_array)
                    .and_then(|signatures| signatures.first())
                    .and_then(Value::as_str)
            })
            .ok_or_else(bad_response)?;
        if indices.insert(signature.to_owned(), index).is_some() {
            return Err(bad_response());
        }
    }
    Ok(indices)
}

fn decode_trade_event(data: &[u8], order: &str) -> Option<TradeEvent> {
    if data.len() != 8 + 32 + 32 + 8 * 4 || data[..8] != TRADE_EVENT_DISC {
        return None;
    }
    let mut offset = 8;
    let event_order = read_pubkey(data, &mut offset).ok()?;
    if event_order != order {
        return None;
    }
    let taker = read_pubkey(data, &mut offset).ok()?;
    Some(TradeEvent {
        taker,
        remaining_making: read_u64(data, &mut offset).ok()?,
        remaining_taking: read_u64(data, &mut offset).ok()?,
        making: read_u64(data, &mut offset).ok()?,
        taking: read_u64(data, &mut offset).ok()?,
    })
}

fn decode_cancel_event(data: &[u8], order: &str) -> bool {
    data.len() == 8 + 32
        && data[..8] == CANCEL_EVENT_DISC
        && bs58::encode(&data[8..]).into_string() == order
}

fn verified_cancel_return(
    event_matches_order: bool,
    wallet_delta: &gofin_domain::SignedQuantity,
    order_delta: &gofin_domain::SignedQuantity,
    maximum_original_escrow: u128,
) -> Result<u128> {
    let returned = wallet_delta
        .as_str()
        .parse::<u128>()
        .map_err(|_| bad_response())?;
    let released = order_delta
        .magnitude()
        .as_str()
        .parse::<u128>()
        .map_err(|_| bad_response())?;
    if !event_matches_order
        || wallet_delta.is_negative()
        || !order_delta.is_negative()
        || returned != released
        || returned > maximum_original_escrow
    {
        return Err(GoFinError::new(
            "JUPITER_ESCROW_EFFECT_MISMATCH",
            "reconciliation",
            "Finalized cancellation does not prove the exact order cancellation and escrow return.",
            "Keep the original order and cancellation unresolved and inspect the finalized event and token balances.",
        ));
    }
    Ok(returned)
}

fn program_event_data<'a>(logs: &'a [String], program: &str) -> Vec<&'a str> {
    let mut stack: Vec<&str> = Vec::new();
    let mut events = Vec::new();
    for line in logs {
        if let Some(encoded) = line.strip_prefix("Program data: ") {
            if stack.last().copied() == Some(program) {
                events.push(encoded);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("Program ") else {
            continue;
        };
        if let Some((program_id, _)) = rest.split_once(" invoke [") {
            stack.push(program_id);
        } else if rest.contains(" success") || rest.contains(" failed:") {
            let ended = rest.split_whitespace().next().unwrap_or_default();
            if stack.last().copied() == Some(ended) {
                stack.pop();
            } else {
                stack.clear();
            }
        }
    }
    events
}

fn decode_create_event(data: &[u8], order: &str) -> Result<Option<DecodedOrder>> {
    if data.len() < 8 || data[..8] != CREATE_EVENT_DISC {
        return Ok(None);
    }
    let mut offset = 8;
    let event_order = read_pubkey(data, &mut offset)?;
    if event_order != order {
        return Ok(None);
    }
    let maker = read_pubkey(data, &mut offset)?;
    let input_mint = read_pubkey(data, &mut offset)?;
    let output_mint = read_pubkey(data, &mut offset)?;
    let input_token_program = read_pubkey(data, &mut offset)?;
    let output_token_program = read_pubkey(data, &mut offset)?;
    let original_making = read_u64(data, &mut offset)?;
    let original_taking = read_u64(data, &mut offset)?;
    let expired_at = match *data.get(offset).ok_or_else(bad_response)? {
        0 => {
            offset += 1;
            None
        }
        1 => {
            offset += 1;
            let value = i64::from_le_bytes(
                data.get(offset..offset + 8)
                    .ok_or_else(bad_response)?
                    .try_into()
                    .map_err(|_| bad_response())?,
            );
            offset += 8;
            Some(value)
        }
        _ => return Err(bad_response()),
    };
    offset += 2; // fee bps
    let _fee_account = read_pubkey(data, &mut offset)?;
    offset += 2; // slippage bps
    if offset != data.len()
        || !valid_token_program(&input_token_program)
        || !valid_token_program(&output_token_program)
    {
        return Err(bad_response());
    }
    Ok(Some(DecodedOrder {
        key: order.to_owned(),
        maker,
        input_reserve: expected_reserve(order, &input_token_program, &input_mint)?,
        input_mint,
        output_mint,
        original_making,
        original_taking,
        making: original_making,
        taking: original_taking,
        expired_at,
        unique_id: 0,
        input_token_program,
        output_token_program,
    }))
}

fn verify_fill_balance_effects(
    meta: &Value,
    keys: &[String],
    wallet: &str,
    order: &DecodedOrder,
    event: &TradeEvent,
    input_decimals: u8,
    output_decimals: u8,
) -> Result<()> {
    let expected = [
        (
            order.input_mint.as_str(),
            order.input_token_program.as_str(),
            input_decimals,
        ),
        (
            order.output_mint.as_str(),
            order.output_token_program.as_str(),
            output_decimals,
        ),
    ];
    let pre = aggregate_owner_mint_balances(
        meta.get("preTokenBalances")
            .and_then(Value::as_array)
            .ok_or_else(bad_response)?,
        keys,
        &expected,
    )?;
    let post = aggregate_owner_mint_balances(
        meta.get("postTokenBalances")
            .and_then(Value::as_array)
            .ok_or_else(bad_response)?,
        keys,
        &expected,
    )?;
    let reserve_before = pre
        .get(&(order.key.clone(), order.input_mint.clone()))
        .copied()
        .unwrap_or(0);
    let reserve_after = post
        .get(&(order.key.clone(), order.input_mint.clone()))
        .copied()
        .unwrap_or(0);
    let wallet_before = pre
        .get(&(wallet.to_owned(), order.output_mint.clone()))
        .copied()
        .unwrap_or(0);
    let wallet_after = post
        .get(&(wallet.to_owned(), order.output_mint.clone()))
        .copied()
        .unwrap_or(0);
    if reserve_before.checked_sub(reserve_after) != Some(event.making)
        || wallet_after.checked_sub(wallet_before) != Some(event.taking)
    {
        return Err(bad_response());
    }
    Ok(())
}

fn aggregate_owner_mint_balances(
    rows: &[Value],
    keys: &[String],
    expected: &[(&str, &str, u8)],
) -> Result<BTreeMap<(String, String), u64>> {
    let mut amounts = BTreeMap::new();
    for row in rows {
        let index = row
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(bad_response)?;
        let key = keys.get(index).ok_or_else(bad_response)?;
        let mint = row
            .get("mint")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let owner = row
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        if let Some((_, token_program, decimals)) =
            expected.iter().find(|(value, _, _)| *value == mint)
            && (row.get("programId").and_then(Value::as_str) != Some(*token_program)
                || row
                    .pointer("/uiTokenAmount/decimals")
                    .and_then(Value::as_u64)
                    != Some(u64::from(*decimals)))
        {
            return Err(bad_response());
        }
        let amount = row
            .pointer("/uiTokenAmount/amount")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?
            .parse::<u64>()
            .map_err(|_| bad_response())?;
        let entry = amounts
            .entry((owner.to_owned(), mint.to_owned()))
            .or_insert(0u64);
        *entry = entry.checked_add(amount).ok_or_else(bad_response)?;
        let _ = key;
    }
    Ok(amounts)
}

fn decode_order(key: &str, data: &[u8]) -> Result<DecodedOrder> {
    if data.len() < 8 + 6 * 32 + 6 * 8 + 1 + 2 + 32 + 16 + 1 + 2 || data[..8] != LIMIT_ORDER_DISC {
        return Err(bad_response());
    }
    let mut o = 8;
    let maker = read_pubkey(data, &mut o)?;
    let input_mint = read_pubkey(data, &mut o)?;
    let output_mint = read_pubkey(data, &mut o)?;
    let input_token_program = read_pubkey(data, &mut o)?;
    let output_token_program = read_pubkey(data, &mut o)?;
    let input_reserve = read_pubkey(data, &mut o)?;
    let unique_id = read_u64(data, &mut o)?;
    let original_making = read_u64(data, &mut o)?;
    let original_taking = read_u64(data, &mut o)?;
    let making = read_u64(data, &mut o)?;
    let taking = read_u64(data, &mut o)?;
    let _borrow = read_u64(data, &mut o)?;
    let tag = *data.get(o).ok_or_else(bad_response)?;
    o += 1;
    let expired_at = match tag {
        0 => None,
        1 => Some(i64::from_le_bytes(
            data.get(o..o + 8)
                .ok_or_else(bad_response)?
                .try_into()
                .map_err(|_| bad_response())?,
        )),
        _ => return Err(bad_response()),
    };
    if tag == 1 {
        o += 8
    }
    o += 2;
    let _fee = read_pubkey(data, &mut o)?;
    o += 16 + 1 + 2;
    if data
        .get(o..)
        .is_some_and(|tail| tail.iter().any(|b| *b != 0))
    {
        return Err(bad_response());
    }
    Ok(DecodedOrder {
        key: key.into(),
        maker,
        input_mint,
        output_mint,
        original_making,
        original_taking,
        making,
        taking,
        expired_at,
        unique_id,
        input_token_program,
        output_token_program,
        input_reserve,
    })
}
fn read_u64(data: &[u8], o: &mut usize) -> Result<u64> {
    let v = u64::from_le_bytes(
        data.get(*o..*o + 8)
            .ok_or_else(bad_response)?
            .try_into()
            .map_err(|_| bad_response())?,
    );
    *o += 8;
    Ok(v)
}
fn read_pubkey(data: &[u8], o: &mut usize) -> Result<String> {
    let value = bs58::encode(data.get(*o..*o + 32).ok_or_else(bad_response)?).into_string();
    *o += 32;
    Ok(value)
}
async fn asset_for(wallet: &SolanaWalletAdapter, mint: &str) -> Result<AssetId> {
    let decimals = token_decimals(&wallet.runtime(), mint).await?;
    Ok(AssetId {
        network: NETWORK.into(),
        reference: trigger_asset_reference(mint),
        decimals,
    })
}

fn trigger_asset_reference(mint: &str) -> String {
    mint.to_owned()
}

fn validate_trigger_mint_mode(input: &Value) -> Result<()> {
    if ["input_mint", "output_mint"]
        .iter()
        .any(|field| input.get(*field).and_then(Value::as_str) == Some("native"))
    {
        return Err(GoFinError::validation(
            "Jupiter Trigger currently requires exact SPL mint identities, including wrapped SOL; native SOL needs an explicit wrap or unwrap step.",
        ));
    }
    Ok(())
}

async fn finalized_trigger_mint(
    wallet: &SolanaWalletAdapter,
    mint: &str,
) -> Result<(gofin_solana::token::MintInfo, Vec<u8>)> {
    let info = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([mint,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = info
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(bad_response)?;
    let owner = info
        .get("value")
        .and_then(|value| value.get("owner"))
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    if !valid_token_program(owner) {
        return Err(unsafe_tx());
    }
    let data = gofin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(bad_response)?,
    )?;
    let epoch = wallet
        .runtime()
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(bad_response)?;
    let parsed = gofin_solana::token::parse_mint(owner, &data, epoch)?;
    Ok((parsed, data))
}

fn validate_trigger_mint_support(mint: &gofin_solana::token::MintInfo) -> Result<()> {
    if !mint.transfer_supported_by_runtime
        || mint.paused == Some(true)
        || mint.transfer_fee.is_some()
        || mint.transfer_hook_program.is_some()
    {
        return Err(GoFinError::new(
            "JUPITER_TRIGGER_TOKEN_EXTENSION_UNSUPPORTED",
            "prepare",
            "This Trigger order uses a token extension whose future escrow transfer effects are not yet bounded by the adapter.",
            "Use a standard SPL token or a Token-2022 mint without transfer fees, transfer hooks, paused transfers, or unsupported transfer extensions.",
        ));
    }
    Ok(())
}

async fn validate_trigger_recipient_ata(
    wallet: &SolanaWalletAdapter,
    mint: &str,
    token_program: &str,
) -> Result<()> {
    let address = expected_associated(wallet.address(), token_program, mint)?;
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let Some(account) = response.get("value").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    if account.get("owner").and_then(Value::as_str) != Some(token_program) {
        return Err(unsafe_tx());
    }
    let data = gofin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(bad_response)?,
    )?;
    let token = gofin_solana::token::parse_token_account(token_program, &address, &data)?;
    if token.token_owner != wallet.address() || token.mint != mint || token.state != "initialized" {
        return Err(unsafe_tx());
    }
    validate_trigger_recipient_extensions(&token)?;
    Ok(())
}

fn validate_trigger_recipient_extensions(
    token: &gofin_solana::token::TokenAccountInfo,
) -> Result<()> {
    if token.memo_required
        || token.confidential_transfer_approved == Some(false)
            && token.allow_non_confidential_credits != Some(true)
    {
        return Err(GoFinError::new(
            "JUPITER_TRIGGER_RECIPIENT_EXTENSION_UNSUPPORTED",
            "prepare",
            "The existing recipient token account requires transfer behavior the Trigger adapter cannot guarantee for a future keeper fill.",
            "Use a recipient account that permits public credits without a required memo or disabled non-confidential deposits.",
        ));
    }
    Ok(())
}
fn mint(input: &Value, key: &str) -> Result<String> {
    let s = input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| GoFinError::validation("Provide an exact token mint."))?;
    if s == "native" {
        Ok(NATIVE.into())
    } else {
        gofin_solana::parse_address(s)
    }
}
fn atoms(input: &Value, key: &str) -> Result<u64> {
    let s = input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| GoFinError::validation("Amounts must be exact integer atom strings."))?;
    if s.len() > 20 || s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(GoFinError::validation(
            "Amounts must be exact integer atom strings.",
        ));
    }
    s.parse()
        .map_err(|_| GoFinError::validation("The exact atom amount is too large."))
}
fn atoms_optional(input: &Value, key: &str) -> Result<u64> {
    let value = input.get(key).and_then(Value::as_str).ok_or_else(|| {
        GoFinError::validation("Writes require explicit network fee and account funding caps.")
    })?;
    if value.len() > 20 || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(GoFinError::validation(
            "Fee limits must be exact lamport integers.",
        ));
    }
    value
        .parse()
        .map_err(|_| GoFinError::validation("The fee limit is too large."))
}

async fn cancellation_valuation(
    prices: Arc<dyn UsdPriceSource>,
    maximum_fee_and_funding_lamports: u64,
) -> Result<Valuation> {
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let quotes = prices.quotes(std::slice::from_ref(&native)).await?;
    let quote = quotes
        .first()
        .filter(|_| quotes.len() == 1)
        .ok_or_else(bad_response)?;
    let usd_micros_per_unit = gofin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?;
    let unit_atoms = gofin_domain::Quantity::new(format!("1{}", "0".repeat(39)))?;
    let maximum_fee_usd_micros = gofin_domain::Quantity::from_u64(maximum_fee_and_funding_lamports)
        .mul_div_ceil(&usd_micros_per_unit, &unit_atoms)?;
    Ok(Valuation {
        movement_usd_micros: gofin_domain::Quantity::zero(),
        maximum_fee_usd_micros,
        additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
        observed_at: Utc::now().to_rfc3339(),
        source: quote.source.clone(),
        prices: vec![adapter::valuation_asset_price(&native, quote)?],
    })
}

fn native_cost(wallet_address: &str, atoms: u64, category: &str) -> FinancialEffect {
    FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
        },
        amount: Amount {
            asset: AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: gofin_domain::Quantity::from_u64(atoms),
        },
        category: category.into(),
        treatment: CostTreatment::AdditionalDebit,
    }
}

pub fn operation_definitions() -> Vec<Definition> {
    let base = json!({"wallet":{"type":"string"},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"}});
    let create = json!({
        "type":"object","additionalProperties":false,
        "required":["wallet","network","cluster","input_mint","output_mint","making_amount_atoms","taking_amount_atoms","maximum_network_fee_lamports","maximum_account_funding_lamports"],
        "properties":{
            "wallet":base["wallet"],"network":base["network"],"cluster":base["cluster"],
            "input_mint":{"type":"string"},"output_mint":{"type":"string"},
            "making_amount_atoms":{"type":"string","pattern":"^[0-9]+$"},
            "taking_amount_atoms":{"type":"string","pattern":"^[0-9]+$"},
            "expired_at_unix":{"type":"integer","minimum":1},
            "maximum_network_fee_lamports":{"type":"string","pattern":"^[0-9]+$"},
            "maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$"}
        }
    });
    let cancel = json!({
        "type":"object","additionalProperties":false,
        "required":["wallet","network","cluster","maximum_network_fee_lamports","maximum_account_funding_lamports"],
        "properties":{
            "wallet":base["wallet"],"network":base["network"],"cluster":base["cluster"],
            "order_key":{"type":"string","description":"Exact order account. Required for externally created orders; a named-plan recovery step inherits it from the earlier create step."},
            "maximum_network_fee_lamports":{"type":"string","pattern":"^[0-9]+$"},
            "maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$"},
            "original_operation_id":{"type":"string","minLength":1,"description":"Required only when cancelling an order created by GoFin. Omit for an externally created wallet-owned order; GoFin then verifies the on-chain owner and uses a fresh standalone approval."},
            "original_submission":{"type":["object","null"],"description":"Optional exact echo of GoFin's saved submission when original_operation_id is supplied. Omit for externally created orders; a named-plan recovery step supplies it from the earlier create step.","required":["kind","reference","data"],"properties":{"kind":{"type":"string"},"reference":{"type":"string"},"data":{"type":"object"}},"additionalProperties":false}
        }
    });
    let orders = json!({
        "type":"object","additionalProperties":false,"required":["wallet","network","cluster"],
        "properties":{"wallet":base["wallet"],"network":base["network"],"cluster":base["cluster"],
            "order_status":{"enum":["active","history"]}}
    });
    let status = json!({
        "type":"object","additionalProperties":false,"required":["wallet","network","cluster","order_key"],
        "properties":{"wallet":base["wallet"],"network":base["network"],"cluster":base["cluster"],"order_key":{"type":"string"},"history_before_signature":{"type":"string","description":"Continue an older page from the history_cursor returned by a prior status read."}}
    });
    let specs = [
        (
            CREATE,
            "Place a noncustodial Trigger V1 order. Standard SPL and plain Token-2022 mints are supported; transfer-fee, transfer-hook, paused, and other unbounded transfer behavior is rejected before signing.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            create,
        ),
        (
            CANCEL,
            "Cancel an open wallet-owned Trigger V1 order and return its remaining escrow. GoFin-created orders use exact parent-bound recovery; externally created orders use a fresh standalone approval.",
            OperationKind::Write,
            ExecutionActor::AgentInvocable,
            cancel,
        ),
        (
            ORDERS,
            "Read active or historical Jupiter Trigger order records and verify active order accounts on chain.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            orders,
        ),
        (
            STATUS,
            "Read one Trigger order account, remaining escrow and expiry state.",
            OperationKind::Read,
            ExecutionActor::AgentReadable,
            status,
        ),
    ];
    specs.into_iter().map(|(name,description,kind,actor,schema)| Definition {
        name:name.into(), description:description.into(), kind, execution_actor:actor,
        input_schema:schema, output_schema:trigger_output_schema(name),
        error_schema:json!({"type":"object","required":["code","stage","message","next_action"]}),
        examples:vec![trigger_example(name)],
        coverage:"Noncustodial Trigger V1 order placement, active/history reads, wallet-owned order cancellation, expiry and fill-state tracking from the Limit Order V2 on-chain account. Plain Token-2022 is accepted. Placement/cancellation reject configured transfer fees, transfer hooks, paused mints, and transfer extensions whose future escrow effects are not bounded. Recipient token-account checks are independent of mint metadata: memo-required accounts and accounts that disallow public credits are unsupported; confidential-transfer metadata alone is allowed when non-confidential credits are enabled. A full fill returns funded account rent only when that same finalized fill receipt proves the exact account closed from its funded balance to zero. If funded accounts remain open after fill, their balances stay tracked as refundable funding; automatic post-fill account cleanup is not supported. Externally created orders can be cancelled with a fresh standalone approval after exact on-chain ownership and escrow verification; GoFin-created orders retain parent-bound recovery accounting.".into(),
        completion:if name==CREATE {"Placement is cumulative progress, not a completed swap; the original operation stays pending until finalized fill, refund or expiry evidence settles its lifecycle.".into()} else if name==CANCEL {"The cancellation operation records a cancellation marker against the original submission; the original order operation owns fill and refund settlement.".into()} else {"Provider history is labeled separately from finalized on-chain order account state.".into()},
        semantics:json!({"network":NETWORK,"cluster":CLUSTER,"program":PROGRAM,"keyless_default":true,"escrowed_not_swapped":true,"uncertain_submission":"reconcile_original_identity_no_retry","token_extensions":{"plain_token_2022":"supported","transfer_fee":"unsupported_until_net_escrow_effects_are_bounded","transfer_hook":"unsupported_until_hook_accounts_and_effects_are_reviewed","paused_mint":"unsupported","memo_required_recipient":"unsupported","confidential_transfer_metadata":"allowed_when_recipient_allows_non_confidential_credits","confidential_transfer_recipient_blocking_public_credits":"unsupported"},"full_fill_account_rent":"returned only when the finalized fill receipt proves exact funded account pre-balance and zero post-balance; otherwise retained funding remains tracked and no post-fill cleanup operation is available","external_order_cancel":"fresh standalone approval only; verify wallet maker, live order PDA, finalized cancel event, exact token return and zero new account funding","fin_created_order_cancel":"exact original operation and submission remain bound; original lifecycle owns fills and escrow refunds"})
    }).collect()
}
fn trigger_output_schema(name: &str) -> Value {
    if name == ORDERS {
        json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"wallet":{"type":"string"},"provider_order_status":{"enum":["active","history"]},"orders":{"type":"array","items":{"type":"object","properties":{"order_key":{"type":"string"},"verified_on_chain_state":{"enum":["open_escrowed","expired_escrowed","account_closed_unverified"]},"provider_status":{"type":["string","null"]},"input_mint":{"type":["string","null"]},"output_mint":{"type":["string","null"]},"raw_making_amount":{"type":["string","null"]},"raw_taking_amount":{"type":["string","null"]},"raw_remaining_making_amount":{"type":["string","null"]},"raw_remaining_taking_amount":{"type":["string","null"]},"expired_at":{"type":["string","integer","null"]},"open_tx":{"type":["string","null"]},"close_tx":{"type":["string","null"]}},"required":["order_key","verified_on_chain_state"]}},"total_pages":{"type":["integer","null"]}},"required":["network","cluster","wallet","provider_order_status","orders"]})
    } else if name == STATUS {
        json!({"type":"object","properties":{"order_key":{"type":"string"},"state":{"enum":["filled","cancelled","expired_escrowed","open_escrowed","closed_without_verified_fill","closed_after_partial_fill_unclassified","closed_history_unproven","history_incomplete"]},"bounded_rpc_scan_complete":{"type":"boolean"},"history_cursor":{"type":["string","null"]},"history_completeness":{"type":"string"},"verified_fills":{"type":"array","items":{"type":"object","properties":{"event_id":{"type":"string"},"event_digest":{"type":"string"},"signature":{"type":"string"},"slot":{"type":"integer"},"taker":{"type":"string"},"making_atoms":{"type":"string"},"taking_atoms":{"type":"string"},"remaining_making_atoms":{"type":"string"},"remaining_taking_atoms":{"type":"string"},"verified_finalized":{"const":true}},"required":["signature","making_atoms","taking_atoms","verified_finalized"]}},"verified_cancel_events":{"type":"array","items":{"type":"object","properties":{"event_id":{"type":"string"},"event_digest":{"type":"string"},"signature":{"type":"string"},"slot":{"type":"integer"},"verified_finalized":{"const":true}},"required":["signature","verified_finalized"]}},"event_digests":{"type":"object","additionalProperties":{"type":"string"}},"total_filled_making_atoms":{"type":"string"},"total_filled_taking_atoms":{"type":"string"},"cancel_event_seen":{"type":"boolean"},"original_making_atoms":{"type":["string","null"]},"original_taking_atoms":{"type":["string","null"]},"remaining_making_atoms":{"type":["string","null"],"description":"Historical unfilled quote before a verified cancellation; while open, this is current on-chain escrow remainder."},"remaining_taking_atoms":{"type":["string","null"],"description":"Historical unfilled quote before a verified cancellation; while open, this is current on-chain quote remainder."},"escrowed_remaining_making_atoms":{"type":["string","null"],"description":"Current input atoms still in escrow; zero after verified cancellation or complete fill."},"returned_making_atoms":{"type":["string","null"],"description":"Input atoms returned by the verified cancellation event."},"remaining_semantics":{"type":"string"},"settlement":{"type":"string"},"maker":{"type":"string"},"input_mint":{"type":"string"},"output_mint":{"type":"string"},"expired_at":{"type":["integer","null"]}},"required":["order_key","state","bounded_rpc_scan_complete","history_cursor","history_completeness","verified_fills","verified_cancel_events","event_digests","total_filled_making_atoms","total_filled_taking_atoms"]})
    } else {
        json!({"type":"object","properties":{"operation_id":{"type":"string"},"status":{"enum":["prepared","submitted","pending","completed","failed"]},"order_key":{"type":["string","null"]},"signature":{"type":["string","null"]},"verified":{"type":"boolean"},"evidence":{"type":["object","null"]},"escrowed_atoms":{"type":["string","null"]},"maximum_network_fee_lamports":{"type":"string"},"maximum_account_funding_lamports":{"type":"string"}},"required":["status"]})
    }
}
fn trigger_example(name: &str) -> Value {
    match name {
        CREATE => {
            json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","input_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","output_mint":"So11111111111111111111111111111111111111112","making_amount_atoms":"10000000","taking_amount_atoms":"9900000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"10000000"})
        }
        CANCEL => {
            json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","order_key":"<order-key>","original_operation_id":"<original-operation-id>","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"0"})
        }
        ORDERS => {
            json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","order_status":"active"})
        }
        _ => {
            json!({"wallet":"wallet","network":"solana","cluster":"mainnet-beta","order_key":"<order-key>"})
        }
    }
}
fn pass(name: &str) -> Check {
    Check {
        name: name.into(),
        status: CheckStatus::Passed,
        detail:
            "Bound to pinned Trigger V1 instruction data and finalized Solana runtime evidence."
                .into(),
    }
}
fn bad_response() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_RESPONSE_INVALID",
        "prepare",
        "Jupiter Trigger returned incomplete or invalid data.",
        "Inspect the order account on chain before preparing another request.",
    )
}

fn exact_full_account_rent_close(
    pre_lamports: u64,
    post_lamports: u64,
    funded_lamports: u64,
) -> bool {
    funded_lamports > 0 && pre_lamports == funded_lamports && post_lamports == 0
}
fn history_timeout() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_HISTORY_DEADLINE",
        "reconcile",
        "Trigger history inspection exceeded its bounded read deadline.",
        "Retry the read later; no transaction was submitted by this history inspection.",
    )
}
fn unsafe_tx() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_INSTRUCTION_UNREVIEWED",
        "prepare",
        "The Trigger transaction contains an unreviewed instruction or account effect.",
        "Do not sign it; prepare a fresh transaction whose full instruction set matches the pinned order interface.",
    )
}
fn changed() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_BINDING_CHANGED",
        "revalidation",
        "The Trigger transaction changed or expired after preview.",
        "Prepare a new exact order action and request fresh approval.",
    )
}
fn uncertain() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_EXECUTE_UNCERTAIN",
        "submission",
        "Trigger execution did not return a valid chain signature after durable dispatch.",
        "Reconcile the original request identity; do not execute it again.",
    )
}
fn unknown() -> GoFinError {
    GoFinError::new(
        "JUPITER_TRIGGER_OPERATION_UNKNOWN",
        "operation",
        "This Jupiter Trigger operation is not supported by the adapter.",
        "Inspect Jupiter Trigger discovery for available lifecycle operations.",
    )
}

fn validate_history_cursor(cursor: Option<&str>) -> Result<()> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    let decoded = bs58::decode(cursor).into_vec().map_err(|_| {
        GoFinError::validation("The Trigger history cursor is not a transaction signature.")
    })?;
    if decoded.len() != 64 {
        return Err(GoFinError::validation(
            "The Trigger history cursor must be a finalized transaction signature returned by an earlier status read.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_fill_closes_funding_only_for_exact_zeroed_rent_account() {
        assert!(exact_full_account_rent_close(2_039_280, 0, 2_039_280));
        assert!(!exact_full_account_rent_close(
            2_039_280 + 100,
            100,
            2_039_280
        ));
        assert!(!exact_full_account_rent_close(2_039_280, 1, 2_039_280));
        assert!(!exact_full_account_rent_close(0, 0, 0));
    }

    #[test]
    fn trigger_history_uses_canonical_block_order_and_monotone_remainders() {
        let block = json!({
            "transactions":[
                {"signatures":["create-signature"]},
                {"signatures":["fill-a-signature"]},
                {"signatures":["fill-b-signature"]},
                {"signatures":["cancel-signature"]}
            ]
        });
        let indices = canonical_block_signature_indices(&block).unwrap();
        assert!(indices["fill-a-signature"] < indices["fill-b-signature"]);
        assert!(indices["fill-b-signature"] < indices["cancel-signature"]);

        let first = advance_trigger_remainder((100, 250), 40, 100).unwrap();
        assert_eq!(first, (60, 150));
        assert_eq!(
            verified_trigger_fill_remainder(first, 60, 150, (0, 0)),
            Some((0, 0))
        );
        assert_eq!(
            verified_trigger_fill_remainder(first, 60, 149, (0, 0)),
            None
        );
        assert_eq!(
            verified_trigger_fill_remainder(first, 61, 150, (0, 0)),
            None
        );
        assert_eq!(
            verified_trigger_fill_remainder(first, 0, 1, (60, 149)),
            None
        );
    }

    #[test]
    fn trigger_history_rejects_duplicate_or_malformed_block_order() {
        let duplicate = json!({"signatures":["same-signature","same-signature"]});
        assert!(canonical_block_signature_indices(&duplicate).is_err());
        assert!(canonical_block_signature_indices(&json!({"transactions":[{}]})).is_err());
    }

    #[test]
    fn cancelled_order_reports_historical_quote_separately_from_escrow() {
        let cancelled = trigger_remainder_report(true, false, false, None, Some((40, 30)));
        assert_eq!(cancelled.quote_remaining, Some((40, 30)));
        assert_eq!(cancelled.escrowed_making, Some(0));
        assert_eq!(cancelled.returned_making, Some(40));

        let open = trigger_remainder_report(false, false, true, Some((40, 30)), Some((60, 40)));
        assert_eq!(open.quote_remaining, Some((40, 30)));
        assert_eq!(open.escrowed_making, Some(40));
        assert_eq!(open.returned_making, None);

        let filled = trigger_remainder_report(false, true, false, None, Some((0, 0)));
        assert_eq!(filled.quote_remaining, Some((0, 0)));
        assert_eq!(filled.escrowed_making, Some(0));
    }

    #[test]
    fn account_expiry_only_refines_open_history_classification() {
        assert_eq!(
            trigger_status_state("open_escrowed", false),
            "open_escrowed"
        );
        assert_eq!(
            trigger_status_state("open_escrowed", true),
            "expired_escrowed"
        );
        assert_eq!(
            trigger_status_state("expired_escrowed", false),
            "open_escrowed"
        );
        for finalized_state in [
            "filled",
            "cancelled",
            "closed_without_verified_fill",
            "closed_after_partial_fill_unclassified",
            "closed_history_unproven",
            "history_incomplete",
        ] {
            assert_eq!(
                trigger_status_state(finalized_state, true),
                finalized_state,
                "an existing account must not overwrite {finalized_state}"
            );
        }
    }

    fn supported_mint_info() -> gofin_solana::token::MintInfo {
        gofin_solana::token::MintInfo {
            program_id: gofin_solana::token::TOKEN_2022_PROGRAM_ID.into(),
            supply_atoms: 1_000_000,
            decimals: 6,
            initialized: true,
            mint_authority: None,
            freeze_authority: None,
            extensions: vec!["ImmutableOwner".into()],
            transfer_fee: None,
            transfer_hook_program: None,
            permanent_delegate: None,
            default_account_state: None,
            paused: Some(false),
            transfer_supported_by_runtime: true,
            transfer_limitation: None,
        }
    }

    fn supported_recipient_info() -> gofin_solana::token::TokenAccountInfo {
        gofin_solana::token::TokenAccountInfo {
            address: "SyntheticAta111111111111111111111111111111111".into(),
            program_id: gofin_solana::token::TOKEN_2022_PROGRAM_ID.into(),
            mint: "SyntheticMint11111111111111111111111111111111".into(),
            token_owner: "SyntheticWallet11111111111111111111111111111".into(),
            amount_atoms: 0,
            delegate: None,
            delegated_amount_atoms: 0,
            close_authority: None,
            state: "initialized".into(),
            is_native: false,
            rent_exempt_reserve_lamports: None,
            withheld_transfer_fee_atoms: None,
            memo_required: false,
            confidential_transfer_approved: Some(false),
            allow_non_confidential_credits: Some(true),
            extensions: vec!["ConfidentialTransferAccount".into()],
        }
    }

    #[test]
    fn trigger_allows_plain_token_2022_but_rejects_unbounded_transfer_extensions() {
        assert!(validate_trigger_mint_support(&supported_mint_info()).is_ok());

        let mut fee_mint = supported_mint_info();
        fee_mint.transfer_fee = Some(gofin_solana::token::TransferFeeSchedule {
            epoch: 10,
            basis_points: 1,
            maximum_fee_atoms: 100,
        });
        assert_eq!(
            validate_trigger_mint_support(&fee_mint).unwrap_err().code,
            "JUPITER_TRIGGER_TOKEN_EXTENSION_UNSUPPORTED"
        );

        let mut hooked_mint = supported_mint_info();
        hooked_mint.transfer_hook_program = Some("Hook111111111111111111111111111111111111".into());
        assert!(validate_trigger_mint_support(&hooked_mint).is_err());

        let mut paused_mint = supported_mint_info();
        paused_mint.paused = Some(true);
        assert!(validate_trigger_mint_support(&paused_mint).is_err());

        let mut unsupported_mint = supported_mint_info();
        unsupported_mint.transfer_supported_by_runtime = false;
        assert!(validate_trigger_mint_support(&unsupported_mint).is_err());
    }

    #[test]
    fn trigger_checks_recipient_account_memo_and_public_credit_flags() {
        // Confidential metadata alone is not a reason to reject an account
        // that explicitly permits ordinary public credits.
        let mut account = supported_recipient_info();
        validate_trigger_recipient_extensions(&account).unwrap();

        account.allow_non_confidential_credits = Some(false);
        assert!(validate_trigger_recipient_extensions(&account).is_err());

        account.allow_non_confidential_credits = Some(true);
        account.memo_required = true;
        assert!(validate_trigger_recipient_extensions(&account).is_err());
    }

    #[test]
    fn snapshot_identity_is_an_exact_resource_token() {
        let resource =
            ResourceId("solana:mainnet-beta:Wallet111111111111111111111111111111111".into());
        let mut coverage = vec!["verified finalized owner-filtered Trigger scan".into()];
        append_snapshot_resource_identity(&mut coverage, &resource);
        assert!(coverage.iter().any(|entry| entry == &resource.0));
        assert!(
            coverage
                .iter()
                .any(|entry| entry == "verified finalized owner-filtered Trigger scan")
        );
        append_snapshot_resource_identity(&mut coverage, &resource);
        assert_eq!(
            coverage
                .iter()
                .filter(|entry| *entry == &resource.0)
                .count(),
            1
        );
    }

    #[test]
    fn trigger_requires_explicit_spl_mints_and_preserves_wsol_identity() {
        assert!(
            validate_trigger_mint_mode(&json!({"input_mint":"native","output_mint":NATIVE}))
                .is_err()
        );
        validate_trigger_mint_mode(&json!({"input_mint":NATIVE,"output_mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"})).unwrap();
        assert_eq!(trigger_asset_reference(NATIVE), NATIVE);
    }

    fn push_key(bytes: &mut Vec<u8>, key: &str) {
        bytes.extend_from_slice(Pubkey::try_from(key).unwrap().as_ref());
    }

    #[test]
    fn status_read_exposes_bounded_history_cursor_contract() {
        validate_history_cursor(None).unwrap();
        validate_history_cursor(Some(&bs58::encode([7u8; 64]).into_string())).unwrap();
        assert!(validate_history_cursor(Some("not-a-signature")).is_err());
        let status = operation_definitions()
            .into_iter()
            .find(|definition| definition.name == STATUS)
            .unwrap();
        assert!(status.input_schema["properties"]["history_before_signature"].is_object());
        assert_eq!(
            status.output_schema["properties"]["history_cursor"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn lifecycle_examples_and_result_schemas_match_each_operation() {
        for definition in operation_definitions() {
            assert_eq!(definition.examples.len(), 1);
            let example = &definition.examples[0];
            assert!(example.is_object());
            let output_properties = definition.output_schema["properties"]
                .as_object()
                .expect("typed output properties");
            match definition.name.as_str() {
                CREATE => {
                    assert!(example.get("making_amount_atoms").is_some());
                    assert!(output_properties.contains_key("escrowed_atoms"));
                }
                CANCEL => {
                    assert!(example.get("original_operation_id").is_some());
                    assert!(example.get("original_submission").is_none());
                    assert!(output_properties.contains_key("status"));
                }
                ORDERS => {
                    assert_eq!(example["order_status"], "active");
                    assert!(output_properties.contains_key("provider_order_status"));
                    assert!(!output_properties.contains_key("verified_fills"));
                }
                STATUS => {
                    assert!(example.get("order_key").is_some());
                    assert!(output_properties.contains_key("verified_fills"));
                    assert!(output_properties.contains_key("history_cursor"));
                }
                other => panic!("unexpected Trigger definition {other}"),
            }
        }
    }

    #[test]
    fn pinned_limit_order_layout_decodes_remaining_escrow_and_expiry() {
        let maker = SYSTEM;
        let input_mint = NATIVE;
        let output_mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let unique = 42u64;
        let order = derive_order(maker, unique).unwrap();
        let reserve = expected_reserve(&order, TOKEN, input_mint).unwrap();
        let mut data = LIMIT_ORDER_DISC.to_vec();
        for key in [maker, input_mint, output_mint, TOKEN, TOKEN, &reserve] {
            push_key(&mut data, key);
        }
        for value in [unique, 1000, 900, 600, 540, 0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.push(1);
        data.extend_from_slice(&1_900_000_000i64.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        push_key(&mut data, SYSTEM);
        data.extend_from_slice(&1_800_000_000i64.to_le_bytes());
        data.extend_from_slice(&1_800_000_100i64.to_le_bytes());
        data.push(254);
        data.extend_from_slice(&0u16.to_le_bytes());

        let parsed = decode_order(&order, &data).unwrap();
        assert_eq!(parsed.key, order);
        assert_eq!(parsed.maker, maker);
        assert_eq!(parsed.input_mint, input_mint);
        assert_eq!(parsed.output_mint, output_mint);
        assert_eq!(parsed.input_token_program, TOKEN);
        assert_eq!(parsed.output_token_program, TOKEN);
        assert_eq!(parsed.input_reserve, reserve);
        assert_eq!(parsed.making, 600);
        assert_eq!(parsed.taking, 540);
        assert_eq!(parsed.expired_at, Some(1_900_000_000));
        assert_eq!(parsed.unique_id, unique);
    }

    #[test]
    fn initialize_args_accept_only_exact_optional_expiry_without_fees() {
        let mut absent = INITIALIZE_DISC.to_vec();
        absent.extend_from_slice(&[0; 24]);
        absent.extend_from_slice(&[0, 0, 0]);
        assert_eq!(decode_initialize_args(&absent).unwrap(), None);

        let mut present = INITIALIZE_DISC.to_vec();
        present.extend_from_slice(&[0; 24]);
        present.push(1);
        present.extend_from_slice(&1_900_000_000i64.to_le_bytes());
        present.extend_from_slice(&[0, 0]);
        assert_eq!(
            decode_initialize_args(&present).unwrap(),
            Some(1_900_000_000)
        );

        let mut fee = absent.clone();
        *fee.last_mut().unwrap() = 1;
        assert!(decode_initialize_args(&fee).is_err());
        let mut trailing = absent;
        trailing.push(0);
        assert!(decode_initialize_args(&trailing).is_err());
    }

    #[test]
    fn fill_event_requires_exact_discriminator_length_and_order_binding() {
        let order = SYSTEM.to_owned();
        let mut data = TRADE_EVENT_DISC.to_vec();
        data.extend_from_slice(Pubkey::try_from(order.as_str()).unwrap().as_ref());
        data.extend_from_slice(Pubkey::try_from(SYSTEM).unwrap().as_ref());
        for amount in [0u64, 0, 25, 20] {
            data.extend_from_slice(&amount.to_le_bytes());
        }
        let event = decode_trade_event(&data, &order).unwrap();
        assert_eq!(event.making, 25);
        assert_eq!(event.taking, 20);
        assert_eq!(event.remaining_making, 0);
        assert_eq!(event.remaining_taking, 0);
        assert!(decode_trade_event(&data[..data.len() - 1], &order).is_none());
        assert!(decode_trade_event(&data, "11111111111111111111111111111112").is_none());
    }

    #[test]
    fn event_log_data_is_bound_to_trigger_call_stack() {
        let event = STANDARD.encode([1u8, 2, 3]);
        let logs = vec![
            format!("Program {PROGRAM} invoke [1]"),
            "Program Child111111111111111111111111111111111 invoke [2]".into(),
            format!("Program data: {event}"),
            "Program Child111111111111111111111111111111111 success".into(),
            format!("Program data: {event}"),
            format!("Program {PROGRAM} success"),
        ];
        assert_eq!(program_event_data(&logs, PROGRAM), vec![event.as_str()]);
    }

    #[test]
    fn fill_settlement_requires_matching_escrow_debit_and_wallet_credit() {
        let wallet = SYSTEM.to_owned();
        let order = "11111111111111111111111111111112".to_owned();
        let input = NATIVE.to_owned();
        let output = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_owned();
        let keys = vec![wallet.clone(), order.clone()];
        let rows = |order_atoms: &str, wallet_atoms: &str| {
            json!([
                {"accountIndex":1,"owner":order,"mint":input,"programId":TOKEN,"uiTokenAmount":{"amount":order_atoms,"decimals":9}},
                {"accountIndex":0,"owner":wallet,"mint":output,"programId":TOKEN,"uiTokenAmount":{"amount":wallet_atoms,"decimals":6}}
            ])
        };
        let meta = json!({"preTokenBalances":rows("100","0"),"postTokenBalances":rows("75","20")});
        let decoded = DecodedOrder {
            key: order.clone(),
            maker: wallet.clone(),
            input_mint: input.clone(),
            output_mint: output.clone(),
            original_making: 100,
            original_taking: 80,
            making: 75,
            taking: 60,
            expired_at: None,
            unique_id: 1,
            input_token_program: TOKEN.into(),
            output_token_program: TOKEN.into(),
            input_reserve: String::new(),
        };
        let event = TradeEvent {
            taker: wallet.clone(),
            remaining_making: 75,
            remaining_taking: 60,
            making: 25,
            taking: 20,
        };
        assert!(verify_fill_balance_effects(&meta, &keys, &wallet, &decoded, &event, 9, 6).is_ok());
        let tampered =
            json!({"preTokenBalances":rows("100","0"),"postTokenBalances":rows("74","20")});
        assert!(
            verify_fill_balance_effects(&tampered, &keys, &wallet, &decoded, &event, 9, 6).is_err()
        );
        let wrong_program = json!({
            "preTokenBalances": rows("100", "0"),
            "postTokenBalances": [
                {"accountIndex":1,"owner":order,"mint":input,"programId":TOKEN_2022,"uiTokenAmount":{"amount":"75","decimals":9}},
                {"accountIndex":0,"owner":wallet,"mint":output,"programId":TOKEN,"uiTokenAmount":{"amount":"20","decimals":6}}
            ]
        });
        assert!(
            verify_fill_balance_effects(&wrong_program, &keys, &wallet, &decoded, &event, 9, 6)
                .is_err()
        );
        let wrong_decimals = json!({
            "preTokenBalances": rows("100", "0"),
            "postTokenBalances": [
                {"accountIndex":1,"owner":order,"mint":input,"programId":TOKEN,"uiTokenAmount":{"amount":"75","decimals":8}},
                {"accountIndex":0,"owner":wallet,"mint":output,"programId":TOKEN,"uiTokenAmount":{"amount":"20","decimals":6}}
            ]
        });
        assert!(
            verify_fill_balance_effects(&wrong_decimals, &keys, &wallet, &decoded, &event, 9, 6)
                .is_err()
        );
    }

    #[test]
    fn cancellation_accepts_only_the_proved_remaining_partial_escrow() {
        let wallet_delta =
            gofin_domain::SignedQuantity::positive(&gofin_domain::Quantity::new("75").unwrap());
        let order_delta =
            gofin_domain::SignedQuantity::negative(&gofin_domain::Quantity::new("75").unwrap());
        assert_eq!(
            verified_cancel_return(true, &wallet_delta, &order_delta, 100).unwrap(),
            75
        );
        assert!(verified_cancel_return(false, &wallet_delta, &order_delta, 100).is_err());
        assert!(verified_cancel_return(true, &wallet_delta, &order_delta, 74).is_err());
        let mismatched_order =
            gofin_domain::SignedQuantity::negative(&gofin_domain::Quantity::new("76").unwrap());
        assert!(verified_cancel_return(true, &wallet_delta, &mismatched_order, 100).is_err());
        let negative_return =
            gofin_domain::SignedQuantity::negative(&gofin_domain::Quantity::new("75").unwrap());
        assert!(verified_cancel_return(true, &negative_return, &order_delta, 100).is_err());
    }
}
