//! Canonical source-side proof helpers for inbound Papertrade CCTP deposits.
//!
//! The route host owns durable submission and dispatch. These helpers only
//! accept an already frozen action plus canonical finalized chain evidence;
//! they cannot sign, submit, or replay a source burn.

use gofin_bridge::{canonical::CanonicalRawLog, cctp};
use gofin_domain::{GoFinError, Quantity};
use gofin_evm::{CanonicalReceipt, FrozenTransaction, alloy::primitives::U256};
use serde_json::Value;
use std::{collections::BTreeMap, str::FromStr, sync::Arc};

/// Read-only finalized source observer for inbound CCTP funding routes. The
/// route host owns signing, durable dispatch identity, and dispatch itself;
/// this type only observes the persisted hash/signature and proves source
/// effects. It has no write methods.
pub(crate) struct PapertradeInboundCctpSource {
    evm_rpcs: BTreeMap<u64, Arc<dyn gofin_evm::EvmRpc>>,
    evm_runtimes: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
    solana_rpc: Option<Arc<dyn gofin_solana::SolanaRpc>>,
    solana_runtime: Option<Arc<gofin_solana::SolanaRuntime>>,
    finality: BTreeMap<u64, (u64, bool)>,
    prepared_solana: std::sync::Mutex<BTreeMap<String, Arc<PreparedSolanaBurn>>>,
}

struct PreparedSolanaBurn {
    frozen: gofin_solana::FrozenTransaction,
    event_signer_ref: gofin_credentials::CredentialRef,
}

impl PapertradeInboundCctpSource {
    pub(crate) fn new(
        evm_rpcs: BTreeMap<u64, Arc<dyn gofin_evm::EvmRpc>>,
        evm_runtimes: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
        solana_rpc: Option<Arc<dyn gofin_solana::SolanaRpc>>,
        solana_runtime: Option<Arc<gofin_solana::SolanaRuntime>>,
        finality: BTreeMap<u64, (u64, bool)>,
    ) -> Result<Self, GoFinError> {
        for chain_id in [1, 42161] {
            if evm_rpcs.contains_key(&chain_id) != evm_runtimes.contains_key(&chain_id)
                || (evm_runtimes.contains_key(&chain_id)
                    && (!finality.contains_key(&chain_id)
                        || evm_runtimes[&chain_id].chain_id != chain_id))
            {
                return Err(invalid(
                    "Inbound CCTP observer has incomplete or mismatched EVM route handles.",
                ));
            }
        }
        if solana_rpc.is_some() != solana_runtime.is_some() {
            return Err(invalid(
                "Inbound CCTP observer has incomplete Solana finalized-read handles.",
            ));
        }
        Ok(Self {
            evm_rpcs,
            evm_runtimes,
            solana_rpc,
            solana_runtime,
            finality,
            prepared_solana: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// Freeze the exact reviewed burn instruction using the enrolled source
    /// signer and a finalized recent blockhash. The opaque transaction stays
    /// in the protected runtime map; only its nonsecret execution identity is
    /// returned into the public immutable funding binding.
    pub(crate) async fn prepare_solana_burn_transaction(
        &self,
        credentials: &dyn crate::BrokerCredentialProvider,
        credential_ref: &gofin_credentials::CredentialRef,
        submission_id: &str,
        instruction: &gofin_solana::NormalizedInstruction,
        maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        if maximum_network_fee_lamports == 0 || submission_id.trim().is_empty() {
            return Err(invalid(
                "Solana CCTP source freeze is missing a fee or leg bound.",
            ));
        }
        let rpc = self
            .solana_rpc
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana RPC is not configured."))?;
        let runtime = self
            .solana_runtime
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana runtime is not configured."))?;
        let signer = credentials.solana_signer(credential_ref)?;
        let event_record = credentials
            .papertrade_runtime_record(&event_signer_record_key(submission_id))?
            .ok_or_else(|| invalid("Solana CCTP event signer identity was not prepared."))?;
        if event_record.get("owner").and_then(Value::as_str) != Some(signer.address())
            || event_record.get("event_address").and_then(Value::as_str)
                != instruction
                    .accounts
                    .get(11)
                    .map(|meta| meta.address.as_str())
        {
            return Err(invalid(
                "Solana CCTP event signer does not match the frozen burn instruction.",
            ));
        }
        let event_ref = gofin_credentials::CredentialRef::new(
            event_record
                .get("event_signer_ref")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Solana CCTP event signer reference is malformed."))?,
        )?;
        let event_signer = credentials.generated_solana_signer(&event_ref)?;
        let event_address = event_record
            .get("event_address")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Solana CCTP event signer address is malformed."))?;
        if event_signer.address() != event_address
            || !instruction.accounts[11].signer
            || !instruction.accounts[11].writable
        {
            return Err(invalid(
                "Solana CCTP event signer is not the exact writable instruction signer.",
            ));
        }
        let event_account = rpc
            .request_checked(
                "getAccountInfo",
                serde_json::json!([event_address, {"commitment":"finalized","encoding":"base64"}]),
                gofin_solana::RpcReadRequirements::default(),
            )
            .await?;
        if event_account
            .get("value")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid(
                "Solana CCTP MessageSent event account already exists before dispatch.",
            ));
        }
        let event_rent = event_record
            .get("event_account_rent_lamports")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("Solana CCTP event-account rent record is malformed."))?;
        let payer_balance = rpc
            .request_checked(
                "getBalance",
                serde_json::json!([signer.address(), {"commitment":"finalized"}]),
                gofin_solana::RpcReadRequirements::default(),
            )
            .await?
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("Finalized Solana source balance is malformed."))?;
        let total_required = event_rent
            .checked_add(maximum_network_fee_lamports)
            .ok_or_else(|| invalid("Solana source native requirement overflowed."))?;
        if payer_balance < total_required {
            return Err(invalid(
                "Solana source wallet lacks the exact rent plus network-fee bound.",
            ));
        }
        let latest = rpc
            .request_checked(
                "getLatestBlockhash",
                serde_json::json!([{"commitment":"finalized"}]),
                gofin_solana::RpcReadRequirements::default(),
            )
            .await?;
        let blockhash = latest["value"]["blockhash"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("Finalized Solana blockhash response is malformed."))?;
        let last_valid = latest["value"]["lastValidBlockHeight"]
            .as_u64()
            .filter(|height| *height > 0)
            .ok_or_else(|| invalid("Finalized Solana blockhash expiry is malformed."))?;
        let frozen = runtime
            .freeze_instructions(
                std::slice::from_ref(instruction),
                &[],
                blockhash,
                signer.as_ref(),
                last_valid,
                maximum_network_fee_lamports,
                Some(submission_id.to_owned()),
                &[event_address.to_owned()],
            )
            .await?;
        let binding = solana_execution_binding(&frozen);
        if frozen.missing_cosigners() != [event_address] {
            return Err(invalid(
                "Frozen Solana CCTP message has an unexpected secondary signer scope.",
            ));
        }
        let mut prepared = self
            .prepared_solana
            .lock()
            .map_err(|_| invalid("Solana funding preparation map is poisoned."))?;
        if prepared
            .get(submission_id)
            .is_some_and(|existing| solana_execution_binding(&existing.frozen) != binding)
        {
            return Err(invalid(
                "Solana funding leg was already frozen differently.",
            ));
        }
        prepared.entry(submission_id.to_owned()).or_insert_with(|| {
            Arc::new(PreparedSolanaBurn {
                frozen,
                event_signer_ref: event_ref,
            })
        });
        Ok(binding)
    }

    /// Build the fixed source request only after checking the enrolled wallet's
    /// native USDC ATA and creating/recovering the protected per-operation
    /// MessageSent event-account signer. The primary wallet remains the rent
    /// payer; the event signer is the exact second signer in Circle's ABI.
    pub(crate) async fn solana_burn_request(
        &self,
        credentials: &dyn crate::BrokerCredentialProvider,
        credential_ref: &gofin_credentials::CredentialRef,
        submission_id: &str,
        amount_atoms: u64,
        metadata: &cctp::CctpQuoteMetadata,
        proxy: &str,
    ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError> {
        use gofin_bridge::cctp::solana::hypercore_source as source;
        use gofin_solana::token::{TOKEN_PROGRAM_ID, decode_rpc_account_data, parse_token_account};

        if submission_id.trim().is_empty()
            || amount_atoms == 0
            || metadata.source_domain != cctp::solana::DOMAIN
            || metadata.destination_domain != source::DESTINATION_DOMAIN
            || metadata.protocol_version != 2
            || metadata.max_fee.to_string().parse::<u64>().ok().is_none()
            || metadata
                .max_fee
                .to_string()
                .parse::<u64>()
                .unwrap_or(u64::MAX)
                >= amount_atoms
        {
            return Err(invalid(
                "Solana CCTP source request is outside the reviewed route.",
            ));
        }
        let owner = credentials
            .solana_signer(credential_ref)?
            .address()
            .to_owned();
        let rpc = self
            .solana_rpc
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana RPC is not configured."))?;
        let ata = gofin_solana::associated_token_address(
            &owner,
            cctp::solana::USDC_MINT,
            TOKEN_PROGRAM_ID,
        )?;
        let account = rpc
            .request_checked(
                "getAccountInfo",
                serde_json::json!([ata, {"commitment":"finalized","encoding":"base64"}]),
                gofin_solana::RpcReadRequirements {
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await?;
        let value = account
            .get("value")
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                invalid("Enrolled Solana wallet has no finalized USDC token account.")
            })?;
        let program = value
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Solana USDC account owner program is malformed."))?;
        if program != TOKEN_PROGRAM_ID {
            return Err(invalid(
                "Native Solana USDC ATA has an unexpected token program.",
            ));
        }
        let data = decode_rpc_account_data(
            value
                .get("data")
                .ok_or_else(|| invalid("Solana USDC account data is missing."))?,
        )?;
        let parsed = parse_token_account(program, &ata, &data)?;
        if parsed.token_owner != owner
            || parsed.mint != cctp::solana::USDC_MINT
            || parsed.amount_atoms < amount_atoms
            || parsed.state != "initialized"
        {
            return Err(invalid(
                "Finalized Solana USDC account does not cover the exact source burn.",
            ));
        }

        let event_operation = stable_auxiliary_operation_id(submission_id);
        let (event_ref, event_signer) = credentials.create_solana_auxiliary_signer(
            &event_operation,
            &owner,
            "papertrade_cctp_message_sent_event",
        )?;
        let event_address = event_signer.address().to_owned();
        let rent_lamports = self.message_event_account_rent().await?;
        let expected = serde_json::json!({
            "version": 1,
            "submission_id": submission_id,
            "owner": owner,
            "event_signer_ref": event_ref.as_str(),
            "event_address": event_address,
            "event_account_space": 484,
            "event_account_rent_lamports": rent_lamports
        });
        persist_immutable_record(
            credentials,
            &event_signer_record_key(submission_id),
            &expected,
        )?;
        let hook = cctp::hypercore::forward_hook(gofin_evm::address(proxy)?)?;
        Ok(source::BurnRequest {
            owner,
            message_sent_event_data: event_address,
            message_sent_event_rent_lamports: rent_lamports,
            amount_atoms,
            max_fee_atoms: metadata
                .max_fee
                .to_string()
                .parse()
                .map_err(|_| invalid("Solana CCTP maximum fee is malformed."))?,
            min_finality_threshold: metadata.min_finality_threshold,
            hook_data: format!("0x{}", hex::encode(&hook)),
        })
    }

    async fn message_event_account_rent(&self) -> Result<u64, GoFinError> {
        let rpc = self
            .solana_rpc
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana RPC is not configured."))?;
        rpc.request_checked(
            "getMinimumBalanceForRentExemption",
            serde_json::json!([484, {"commitment":"finalized"}]),
            gofin_solana::RpcReadRequirements::default(),
        )
        .await?
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("Solana CCTP event-account rent quote is malformed."))
    }

    #[allow(clippy::too_many_arguments)] // All authority and frozen-plan identities are explicit.
    pub(crate) async fn dispatch_prepared_solana_burn(
        &self,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        credential_ref: &gofin_credentials::CredentialRef,
        leg_submission_id: &str,
        expected_instruction: &gofin_solana::NormalizedInstruction,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<gofin_domain::Submission, GoFinError> {
        let prepared = self
            .prepared_solana
            .lock()
            .map_err(|_| invalid("Solana funding preparation map is poisoned."))?
            .get(leg_submission_id)
            .cloned()
            .ok_or_else(|| invalid("Exact frozen Solana funding transaction is unavailable."))?;
        if prepared.frozen.instructions() != [expected_instruction.clone()] {
            return Err(invalid(
                "Frozen Solana transaction changed its CCTP source instruction.",
            ));
        }
        let event_signer = credentials.generated_solana_signer(&prepared.event_signer_ref)?;
        self.dispatch_solana_burn(
            credentials,
            credential_ref,
            leg_submission_id,
            &prepared.frozen,
            expected_instruction,
            event_signer,
            outer_submission,
            journal,
            validated,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Exact source identity, quote and receipt bounds are explicit.
    pub(crate) async fn observe_evm_burn(
        &self,
        source_chain_id: u64,
        transaction_hash: &str,
        frozen: &FrozenTransaction,
        metadata: &cctp::CctpQuoteMetadata,
        owner: &str,
        amount_atoms: u64,
        proxy: &str,
        maximum_native_fee_atoms: &str,
    ) -> Result<Option<EvmCctpSourceProof>, GoFinError> {
        let rpc = self
            .evm_rpcs
            .get(&source_chain_id)
            .ok_or_else(|| invalid("Inbound CCTP source EVM RPC is not configured."))?;
        let runtime = self
            .evm_runtimes
            .get(&source_chain_id)
            .ok_or_else(|| invalid("Inbound CCTP source EVM runtime is not configured."))?;
        let (confirmations, finalized) = self
            .finality
            .get(&source_chain_id)
            .copied()
            .ok_or_else(|| invalid("Inbound CCTP source finality policy is missing."))?;
        let Some(receipt) = runtime
            .observe_receipt(transaction_hash, confirmations.max(1), finalized)
            .await?
        else {
            return Ok(None);
        };
        let transaction = rpc
            .request(
                "eth_getTransactionByHash",
                serde_json::json!([transaction_hash]),
            )
            .await?;
        if transaction.is_null() {
            return Ok(None);
        }
        verify_evm_source_burn(
            &transaction,
            &receipt,
            frozen,
            metadata,
            source_chain_id,
            owner,
            amount_atoms,
            proxy,
            confirmations,
            maximum_native_fee_atoms,
        )
        .map(Some)
    }

    pub(crate) async fn observe_solana_burn(
        &self,
        signature: &str,
        request: &cctp::solana::hypercore_source::BurnRequest,
        metadata: &cctp::CctpQuoteMetadata,
        proxy: &str,
        maximum_network_fee_lamports: u64,
    ) -> Result<SolanaCctpSourceProof, GoFinError> {
        let rpc = self
            .solana_rpc
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana RPC is not configured."))?;
        let runtime = self
            .solana_runtime
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana runtime is not configured."))?;
        let proof = runtime
            .observe_finalized_proof(signature)
            .await
            .map_err(|_| invalid("Solana CCTP burn has not reached finalized evidence."))?;
        if proof.signature != signature {
            return Err(invalid(
                "Finalized Solana CCTP receipt differs from the persisted source signature.",
            ));
        }
        let account = rpc
            .request_checked(
                "getAccountInfo",
                serde_json::json!([
                    request.message_sent_event_data,
                    {"commitment":"finalized","encoding":"base64"}
                ]),
                gofin_solana::RpcReadRequirements {
                    minimum_context_slot: Some(proof.slot),
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await?;
        verify_solana_source_burn(
            &proof,
            &account,
            request,
            metadata,
            proxy,
            maximum_network_fee_lamports,
        )
    }

    /// Submit one exact EVM source burn under the parent Papertrade
    /// authorization. The nested journal records the child transaction hash
    /// before network dispatch and preserves the parent submission identity.
    pub(crate) async fn dispatch_evm_burn(
        &self,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        leg_submission_id: &str,
        frozen: &FrozenTransaction,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<gofin_domain::Submission, GoFinError> {
        let authorized: gofin_papertrade::FundingBinding = serde_json::from_value(
            validated
                .prepared()
                .binding
                .pointer("/funding_binding")
                .cloned()
                .ok_or_else(|| invalid("Authorized funding operation omits its route binding."))?,
        )
        .map_err(|_| invalid("Authorized funding route binding is malformed."))?;
        if !matches!(frozen.chain_id, 1 | 42161)
            || authorized.transaction.as_ref() != Some(frozen)
            || outer_submission.kind != "papertrade_funding"
            || leg_submission_id.trim().is_empty()
        {
            return Err(invalid(
                "EVM source burn differs from the exact authorized funding binding.",
            ));
        }
        validated.assert_fresh()?;
        let runtime = self
            .evm_runtimes
            .get(&frozen.chain_id)
            .ok_or_else(|| invalid("Inbound CCTP source EVM runtime is not configured."))?;
        let marker = funding_leg_record_key(leg_submission_id, "dispatch_attempted");
        persist_immutable_record(
            credentials.as_ref(),
            &marker,
            &serde_json::json!({
                "version":1,
                "outer_submission":outer_submission,
                "transaction_chain_id":frozen.chain_id,
                "transaction_to":frozen.to,
                "transaction_nonce":frozen.nonce,
                "calldata_hash":format!("{:#x}",gofin_evm::alloy::primitives::keccak256(
                    hex::decode(frozen.calldata.strip_prefix("0x").unwrap_or_default())
                        .map_err(|_| invalid("Frozen CCTP calldata is malformed."))?
                ))
            }),
        )?;
        let nested = crate::papertrade_funding_host::nested_funding_journal(
            credentials,
            outer_submission.clone(),
            leg_submission_id,
            journal,
        );
        runtime
            .submit_nested_revalidated(
                frozen,
                serde_json::json!({"operation":"papertrade.deposit","route":"cctp_source"}),
                nested,
                validated,
                "/funding_binding",
            )
            .await
    }

    /// Submit one exact Solana source burn with the enrolled signer and the
    /// outer candidate's journal authority. The caller must persist the
    /// request/instruction and execution binding before this method runs.
    #[allow(clippy::too_many_arguments)] // All signer, request, submission and nested authority bounds are explicit.
    pub(crate) async fn dispatch_solana_burn(
        &self,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        credential_ref: &gofin_credentials::CredentialRef,
        leg_submission_id: &str,
        frozen: &gofin_solana::FrozenTransaction,
        expected_instruction: &gofin_solana::NormalizedInstruction,
        event_signer: Arc<gofin_solana::SolanaSigner>,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<gofin_domain::Submission, GoFinError> {
        let authorized: gofin_papertrade::FundingBinding = serde_json::from_value(
            validated
                .prepared()
                .binding
                .pointer("/funding_binding")
                .cloned()
                .ok_or_else(|| invalid("Authorized funding operation omits its route binding."))?,
        )
        .map_err(|_| invalid("Authorized funding route binding is malformed."))?;
        let expected = authorized.solana_transaction.as_ref().ok_or_else(|| {
            invalid("Authorized funding binding omits the Solana execution identity.")
        })?;
        if outer_submission.kind != "papertrade_funding"
            || frozen.instructions() != [expected_instruction.clone()]
            || frozen.signer() != expected.signer
            || frozen.fee_payer() != expected.fee_payer
            || frozen.message_sha256() != expected.message_sha256
            || frozen.recent_blockhash() != expected.recent_blockhash
            || frozen.last_valid_block_height() != expected.last_valid_block_height
            || frozen.maximum_network_fee_lamports() != expected.maximum_network_fee_lamports
            || leg_submission_id.trim().is_empty()
        {
            return Err(invalid(
                "Solana source burn differs from the exact authorized funding transaction.",
            ));
        }
        validated.assert_fresh()?;
        let runtime = self
            .solana_runtime
            .as_ref()
            .ok_or_else(|| invalid("Inbound CCTP Solana runtime is not configured."))?;
        let signer = credentials.solana_signer(credential_ref)?;
        if signer.address() != expected.signer {
            return Err(invalid(
                "Protected Solana signer differs from the authorized source wallet.",
            ));
        }
        if frozen.missing_cosigners() != [event_signer.address()]
            || expected_instruction.accounts.get(11).is_none_or(|meta| {
                meta.address != event_signer.address() || !meta.signer || !meta.writable
            })
        {
            return Err(invalid(
                "Protected event-account signer differs from the exact frozen CCTP message.",
            ));
        }
        let marker = funding_leg_record_key(leg_submission_id, "dispatch_attempted");
        persist_immutable_record(
            credentials.as_ref(),
            &marker,
            &serde_json::json!({
                "version":1,
                "outer_submission":outer_submission,
                "message_sha256":frozen.message_sha256(),
                "source_signature_pending":true
            }),
        )?;
        let nested = crate::papertrade_funding_host::nested_funding_journal(
            credentials,
            outer_submission.clone(),
            leg_submission_id,
            journal,
        );
        let sender = gofin_solana::transaction::RpcTransactionSender::new(runtime.rpc().clone());
        runtime
            .dispatch_with_secondary_signers(
                frozen,
                signer.as_ref(),
                &[event_signer],
                validated.prepared(),
                nested,
                &sender,
            )
            .await
    }
}

fn solana_execution_binding(
    frozen: &gofin_solana::FrozenTransaction,
) -> gofin_papertrade::SolanaFundingTransactionBinding {
    gofin_papertrade::SolanaFundingTransactionBinding {
        message_sha256: frozen.message_sha256().to_owned(),
        signer: frozen.signer().to_owned(),
        fee_payer: frozen.fee_payer().to_owned(),
        recent_blockhash: frozen.recent_blockhash().to_owned(),
        last_valid_block_height: frozen.last_valid_block_height(),
        maximum_network_fee_lamports: frozen.maximum_network_fee_lamports(),
    }
}

fn funding_leg_record_key(submission_id: &str, suffix: &str) -> String {
    format!(
        "papertrade-funding-leg:{}:{suffix}",
        hex::encode(submission_id.as_bytes())
    )
}

fn event_signer_record_key(submission_id: &str) -> String {
    funding_leg_record_key(submission_id, "solana_message_sent_event_signer")
}

/// The credential store requires a canonical UUID for internal auxiliary
/// signers. Derive one deterministically from the already stable private leg
/// identity, with a domain separator; no user-controlled purpose or key bytes
/// enter the signer vault.
fn stable_auxiliary_operation_id(submission_id: &str) -> String {
    use sha2::Digest;
    let mut material = b"gofin/papertrade/cctp-v2/message-sent-event/v1\0".to_vec();
    material.extend_from_slice(submission_id.as_bytes());
    let digest = sha2::Sha256::digest(material);
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("fixed digest prefix");
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).hyphenated().to_string()
}

fn persist_immutable_record(
    credentials: &dyn crate::BrokerCredentialProvider,
    key: &str,
    value: &Value,
) -> Result<(), GoFinError> {
    if let Some(existing) = credentials.papertrade_runtime_record(key)? {
        if existing != *value {
            return Err(invalid(
                "Funding source dispatch identity conflicts with its durable record.",
            ));
        }
        return Ok(());
    }
    credentials.put_papertrade_runtime_record(key, value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvmCctpSourceProof {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub confirmations: u64,
    pub source_chain_id: u64,
    pub source_owner: String,
    pub amount_atoms: u64,
    pub native_fee_atoms: String,
    pub cctp_source: cctp::CctpSourceMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SolanaCctpSourceProof {
    pub signature: String,
    pub slot: u64,
    pub source_owner: String,
    pub amount_atoms: u64,
    pub network_fee_lamports: u64,
    pub retained_message_account_rent_lamports: u64,
    pub destination_domain: u32,
    pub hook_data: String,
    pub source_message: gofin_bridge::cctp::solana::hypercore_source::SourceMessageProof,
}

fn invalid(message: &str) -> GoFinError {
    GoFinError::validation(message)
}

fn parse_logs(receipt: &CanonicalReceipt) -> Result<Vec<CanonicalRawLog>, GoFinError> {
    let logs = receipt
        .receipt
        .get("logs")
        .and_then(Value::as_array)
        .filter(|logs| logs.len() <= 512)
        .ok_or_else(|| invalid("CCTP source receipt logs are missing or oversized."))?;
    logs.iter()
        .map(|log| {
            let address = log
                .get("address")
                .and_then(Value::as_str)
                .filter(|value| value.len() == 42 && value.starts_with("0x"))
                .ok_or_else(|| invalid("CCTP source log address is malformed."))?
                .to_owned();
            let topics = log
                .get("topics")
                .and_then(Value::as_array)
                .filter(|topics| !topics.is_empty() && topics.len() <= 4)
                .ok_or_else(|| invalid("CCTP source log topics are malformed."))?
                .iter()
                .map(|topic| {
                    let value = topic
                        .as_str()
                        .filter(|value| value.len() == 66 && value.starts_with("0x"))
                        .ok_or_else(|| invalid("CCTP source log topic is malformed."))?;
                    Ok(value.to_owned())
                })
                .collect::<Result<Vec<_>, GoFinError>>()?;
            let data = log
                .get("data")
                .and_then(Value::as_str)
                .filter(|value| {
                    value.starts_with("0x")
                        && value.len() <= 131_074
                        && (value.len() - 2).is_multiple_of(2)
                })
                .ok_or_else(|| invalid("CCTP source log data is malformed."))?
                .to_owned();
            let log_index = log
                .get("logIndex")
                .and_then(Value::as_str)
                .and_then(|value| u32::from_str_radix(value.strip_prefix("0x")?, 16).ok())
                .ok_or_else(|| invalid("CCTP source log index is malformed."))?;
            Ok(CanonicalRawLog {
                log_index,
                address,
                topics,
                data,
            })
        })
        .collect()
}

fn verify_owner_transfer(
    logs: &[CanonicalRawLog],
    source_chain_id: u64,
    owner: &str,
    amount: &Quantity,
) -> Result<(), GoFinError> {
    let token = cctp::usdc_asset(source_chain_id)?
        .token
        .ok_or_else(|| invalid("CCTP source route has no native USDC token."))?;
    let owner = gofin_evm::address(owner)?;
    let messenger = gofin_evm::address(cctp::TOKEN_MESSENGER)?;
    let transfer_topic = format!(
        "{:#x}",
        gofin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)")
    );
    let mut matches = Vec::new();
    for log in logs {
        if !log.address.eq_ignore_ascii_case(&token)
            || log.topics.first().map(String::as_str) != Some(transfer_topic.as_str())
            || log.topics.len() != 3
        {
            continue;
        }
        let from = gofin_evm::alloy::primitives::B256::from_str(&log.topics[1])
            .map_err(|_| invalid("CCTP source transfer sender topic is malformed."))?;
        let to = gofin_evm::alloy::primitives::B256::from_str(&log.topics[2])
            .map_err(|_| invalid("CCTP source transfer recipient topic is malformed."))?;
        let data = gofin_evm::alloy::primitives::hex::decode(
            log.data
                .strip_prefix("0x")
                .ok_or_else(|| invalid("CCTP source transfer data is malformed."))?,
        )
        .map_err(|_| invalid("CCTP source transfer data is malformed."))?;
        if data.len() != 32 {
            return Err(invalid("CCTP source transfer amount is malformed."));
        }
        if from == owner.into_word() && to == messenger.into_word() {
            matches.push(U256::from_be_slice(&data));
        }
    }
    if matches.len() != 1 || matches[0] != gofin_evm::u256(amount)? {
        return Err(invalid(
            "CCTP source receipt does not prove the exact owner-to-TokenMessenger USDC transfer.",
        ));
    }
    Ok(())
}

/// Prove one exact frozen Ethereum/Arbitrum CCTP burn. The caller must obtain
/// `transaction` and `receipt` by the persisted source hash from the configured
/// read-only RPC, and must pass the approved maximum source native fee.
#[allow(clippy::too_many_arguments)] // Parameters are the complete frozen source-burn proof contract.
pub(crate) fn verify_evm_source_burn(
    transaction: &Value,
    receipt: &CanonicalReceipt,
    frozen: &FrozenTransaction,
    metadata: &cctp::CctpQuoteMetadata,
    source_chain_id: u64,
    owner: &str,
    amount_atoms: u64,
    proxy: &str,
    required_confirmations: u64,
    maximum_native_fee_atoms: &str,
) -> Result<EvmCctpSourceProof, GoFinError> {
    if !matches!(source_chain_id, 1 | 42161)
        || frozen.chain_id != source_chain_id
        || !frozen.sender.eq_ignore_ascii_case(owner)
        || receipt.confirmations < required_confirmations.max(1)
        || receipt.block_number == 0
        || !receipt.success
        || gofin_evm::address(&receipt.transaction_sender)? != gofin_evm::address(owner)?
    {
        return Err(invalid(
            "CCTP source receipt is unsuccessful, below finality, or from another chain/owner.",
        ));
    }
    let tx_hash = transaction
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("CCTP source transaction hash is missing."))?;
    let tx_block_hash = transaction
        .get("blockHash")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("CCTP source transaction block hash is missing."))?;
    let tx_block_number = gofin_evm::rpc_u64(
        transaction
            .get("blockNumber")
            .ok_or_else(|| invalid("CCTP source transaction block number is missing."))?,
    )?;
    let to = transaction
        .get("to")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("CCTP source transaction recipient is missing."))?;
    let input = transaction
        .get("input")
        .or_else(|| transaction.get("data"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("CCTP source transaction calldata is missing."))?;
    let value = transaction
        .get("value")
        .ok_or_else(|| invalid("CCTP source transaction value is missing."))?;
    let fee_cap = U256::from_str_radix(maximum_native_fee_atoms, 10)
        .map_err(|_| invalid("CCTP source native fee cap is malformed."))?;
    let tx_chain_matches = transaction
        .get("chainId")
        .is_none_or(|chain| gofin_evm::rpc_u64(chain).ok() == Some(source_chain_id));
    if gofin_evm::hash(tx_hash)? != receipt.hash
        || gofin_evm::hash(tx_block_hash)? != receipt.block_hash
        || tx_block_number != receipt.block_number
        || gofin_evm::address(
            transaction
                .get("from")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("CCTP source transaction sender is missing."))?,
        )? != gofin_evm::address(owner)?
        || gofin_evm::address(to)? != gofin_evm::address(&frozen.to)?
        || hex::decode(
            input
                .strip_prefix("0x")
                .ok_or_else(|| invalid("CCTP source transaction calldata is not hex encoded."))?,
        )
        .map_err(|_| invalid("CCTP source transaction calldata is malformed."))?
            != hex::decode(
                frozen
                    .calldata
                    .strip_prefix("0x")
                    .ok_or_else(|| invalid("Frozen CCTP calldata is not hex encoded."))?,
            )
            .map_err(|_| invalid("Frozen CCTP calldata is malformed."))?
        || gofin_evm::rpc_u256(value)? != gofin_evm::u256(&frozen.value)?
        || !tx_chain_matches
        || receipt.fee > fee_cap
    {
        return Err(invalid(
            "CCTP source transaction differs from its frozen envelope or fee bound.",
        ));
    }
    let logs = parse_logs(receipt)?;
    let source = cctp::hypercore::verify_source_logs(
        &logs,
        metadata,
        source_chain_id,
        gofin_evm::address(owner)?,
        &Quantity::from_u64(amount_atoms),
        gofin_evm::address(proxy)?,
    )?;
    verify_owner_transfer(
        &logs,
        source_chain_id,
        owner,
        &Quantity::from_u64(amount_atoms),
    )?;
    Ok(EvmCctpSourceProof {
        transaction_hash: format!("{:#x}", receipt.hash),
        block_hash: format!("{:#x}", receipt.block_hash),
        block_number: receipt.block_number,
        confirmations: receipt.confirmations,
        source_chain_id,
        source_owner: format!("{:#x}", gofin_evm::address(owner)?),
        amount_atoms,
        native_fee_atoms: receipt.fee.to_string(),
        cctp_source: source,
    })
}

/// Prove a finalized Solana native-USDC CCTP burn. The route host persists the
/// returned signature before querying Circle and never resubmits on timeout.
pub(crate) fn verify_solana_source_burn(
    proof: &gofin_solana::FinalizedReceiptProof,
    message_account: &Value,
    request: &cctp::solana::hypercore_source::BurnRequest,
    metadata: &cctp::CctpQuoteMetadata,
    proxy: &str,
    maximum_network_fee_lamports: u64,
) -> Result<SolanaCctpSourceProof, GoFinError> {
    if proof.fee_lamports > maximum_network_fee_lamports {
        return Err(invalid(
            "Solana CCTP source network fee exceeds the approved bound.",
        ));
    }
    let burn = cctp::solana::hypercore_source::verify_burn_receipt(
        proof,
        request,
        metadata,
        gofin_evm::address(proxy)?,
    )?;
    let event_account = message_account
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(|| invalid("Finalized Solana CCTP MessageSent account is missing."))?;
    let context_slot = message_account
        .get("context")
        .and_then(|context| context.get("slot"))
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("Solana MessageSent account context slot is missing."))?;
    if context_slot < proof.slot
        || !proof
            .account_keys
            .iter()
            .any(|key| key == &request.message_sent_event_data)
        || event_account.get("owner").and_then(Value::as_str)
            != Some(cctp::solana::MESSAGE_TRANSMITTER)
    {
        return Err(invalid(
            "Solana MessageSent account is not the finalized account created by the exact burn transaction.",
        ));
    }
    let rent_delta = proof
        .lamport_delta(&request.message_sent_event_data)
        .map_err(|_| invalid("Solana MessageSent account rent delta is unavailable."))?;
    if rent_delta.is_negative() || rent_delta.is_zero() {
        return Err(invalid(
            "Solana MessageSent account did not retain a positive owner-funded rent balance.",
        ));
    }
    let data = gofin_solana::token::decode_rpc_account_data(
        event_account
            .get("data")
            .ok_or_else(|| invalid("Solana MessageSent account data is missing."))?,
    )
    .map_err(|_| invalid("Solana MessageSent account data is malformed."))?;
    // Anchor MessageSent account: discriminator, rent payer, created_at i64,
    // then Borsh Vec<u8>. The approved burn instruction creates this account
    // and names its key in the finalized transaction account list.
    const MESSAGE_SENT_DISCRIMINATOR: [u8; 8] = [0x83, 0x64, 0x85, 0x38, 0xa6, 0xe1, 0x97, 0x3c];
    let owner_bytes = bs58::decode(&request.owner)
        .into_vec()
        .ok()
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| invalid("Solana MessageSent rent payer is malformed."))?;
    if data.len() < 52
        || data[..8] != MESSAGE_SENT_DISCRIMINATOR
        || data[8..40] != owner_bytes
        || i64::from_le_bytes(data[40..48].try_into().expect("fixed slice")) < 0
    {
        return Err(invalid(
            "Solana MessageSent account header or rent payer does not match the exact source wallet.",
        ));
    }
    let message_len = u32::from_le_bytes(data[48..52].try_into().expect("fixed slice")) as usize;
    if message_len == 0 || message_len > 4096 || data.len() != 52 + message_len {
        return Err(invalid(
            "Solana MessageSent account message length is invalid.",
        ));
    }
    let raw_message = format!("0x{}", hex::encode(&data[52..]));
    let source_message = cctp::solana::hypercore_source::verify_source_message(
        &raw_message,
        request,
        metadata,
        gofin_evm::address(proxy)?,
    )
    .map_err(|_| invalid("Solana Circle MessageSent bytes differ from the exact source burn."))?;
    let rent_lamports = rent_delta
        .magnitude()
        .to_string()
        .parse::<u64>()
        .map_err(|_| invalid("Solana MessageSent rent balance exceeds supported bounds."))?;
    let actual_account_lamports = event_account
        .get("lamports")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("Finalized Solana MessageSent lamports are malformed."))?;
    let payer_delta = proof
        .lamport_delta(&request.owner)
        .map_err(|_| invalid("Solana CCTP payer lamport delta is unavailable."))?;
    let expected_payer_debit = u128::from(proof.fee_lamports)
        .checked_add(u128::from(rent_lamports))
        .ok_or_else(|| invalid("Solana source native debit overflowed."))?;
    if actual_account_lamports != rent_lamports
        || !payer_delta.is_negative()
        || payer_delta.magnitude().to_string() != expected_payer_debit.to_string()
    {
        return Err(invalid(
            "Finalized Solana payer debit is not exactly network fee plus retained MessageSent rent.",
        ));
    }
    Ok(SolanaCctpSourceProof {
        signature: burn.signature,
        slot: burn.slot,
        source_owner: burn.owner,
        amount_atoms: burn.amount_atoms,
        network_fee_lamports: burn.network_fee_lamports,
        retained_message_account_rent_lamports: rent_lamports,
        destination_domain: burn.destination_domain,
        hook_data: burn.hook_data,
        source_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use gofin_bridge::cctp::solana::hypercore_source::{self, BurnRequest};
    use gofin_bridge::cctp::{CctpQuoteMetadata, CctpTransferMode};
    use gofin_evm::alloy::{sol, sol_types::SolEvent};
    use gofin_solana::FinalizedReceiptProof;
    use serde_json::json;

    sol! {
        event MessageSent(bytes message);
        event DepositForBurn(
            address indexed burnToken,
            uint256 amount,
            address indexed depositor,
            bytes32 mintRecipient,
            uint32 destinationDomain,
            bytes32 destinationTokenMessenger,
            bytes32 destinationCaller,
            uint256 maxFee,
            uint32 indexed minFinalityThreshold,
            bytes hookData
        );
    }

    fn b58(bytes: &[u8]) -> String {
        bs58::encode(bytes).into_string()
    }

    fn canonical_event_log<E: SolEvent>(
        address: &str,
        log_index: u32,
        event: E,
    ) -> CanonicalRawLog {
        CanonicalRawLog {
            log_index,
            address: address.to_owned(),
            topics: event
                .encode_topics()
                .iter()
                .map(|topic| format!("{:#x}", topic.0))
                .collect(),
            data: format!("0x{}", hex::encode(event.encode_data())),
        }
    }

    fn evm_source_burn_fixture() -> (
        Value,
        CanonicalReceipt,
        FrozenTransaction,
        CctpQuoteMetadata,
        String,
        String,
    ) {
        use gofin_bridge::cctp::{self, hypercore};
        use gofin_evm::alloy::primitives::{Bytes, U256};

        let owner = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let amount = 12_345_678u64;
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 3,
            destination_domain: hypercore::DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(12_346),
            min_finality_threshold: 1000,
        };
        let owner_address = gofin_evm::address(owner).unwrap();
        let proxy_address = gofin_evm::address(proxy).unwrap();
        let token =
            gofin_evm::address(cctp::usdc_asset(42161).unwrap().token.as_deref().unwrap()).unwrap();
        let hook = hypercore::forward_hook(proxy_address).unwrap();
        let mut message = Vec::with_capacity(432);
        for value in [1u32, metadata.source_domain, metadata.destination_domain] {
            message.extend_from_slice(&value.to_be_bytes());
        }
        message.extend_from_slice(&[0x77; 32]);
        for address in [
            gofin_evm::address(cctp::TOKEN_MESSENGER).unwrap(),
            gofin_evm::address(cctp::TOKEN_MESSENGER).unwrap(),
            gofin_evm::address(hypercore::FORWARDER).unwrap(),
        ] {
            message.extend_from_slice(&address.into_word().0);
        }
        message.extend_from_slice(&metadata.min_finality_threshold.to_be_bytes());
        message.extend_from_slice(&0u32.to_be_bytes());
        message.extend_from_slice(&1u32.to_be_bytes());
        message.extend_from_slice(&token.into_word().0);
        message.extend_from_slice(
            &gofin_evm::address(hypercore::FORWARDER)
                .unwrap()
                .into_word()
                .0,
        );
        message.extend_from_slice(&U256::from(amount).to_be_bytes::<32>());
        message.extend_from_slice(&owner_address.into_word().0);
        message.extend_from_slice(
            &gofin_evm::u256(&metadata.max_fee)
                .unwrap()
                .to_be_bytes::<32>(),
        );
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        message.extend_from_slice(&hook);
        let logs = [
            canonical_event_log(
                cctp::MESSAGE_TRANSMITTER,
                2,
                MessageSent {
                    message: Bytes::from(message),
                },
            ),
            canonical_event_log(
                cctp::TOKEN_MESSENGER,
                3,
                DepositForBurn {
                    burnToken: token,
                    amount: U256::from(amount),
                    depositor: owner_address,
                    mintRecipient: gofin_evm::address(hypercore::FORWARDER)
                        .unwrap()
                        .into_word(),
                    destinationDomain: hypercore::DOMAIN,
                    destinationTokenMessenger: gofin_evm::address(cctp::TOKEN_MESSENGER)
                        .unwrap()
                        .into_word(),
                    destinationCaller: gofin_evm::address(hypercore::FORWARDER)
                        .unwrap()
                        .into_word(),
                    maxFee: gofin_evm::u256(&metadata.max_fee).unwrap(),
                    minFinalityThreshold: metadata.min_finality_threshold,
                    hookData: hook,
                },
            ),
            canonical_event_log(
                &format!("{token:#x}"),
                4,
                cctp::Token::Transfer {
                    from: owner_address,
                    to: gofin_evm::address(cctp::TOKEN_MESSENGER).unwrap(),
                    value: U256::from(amount),
                },
            ),
        ];
        let logs_value = logs
            .iter()
            .map(|log| json!({"address":log.address,"topics":log.topics,"data":log.data,"logIndex":format!("0x{:x}",log.log_index)}))
            .collect::<Vec<_>>();
        let hash = format!("0x{}", "31".repeat(32));
        let block_hash = format!("0x{}", "32".repeat(32));
        let frozen = FrozenTransaction {
            chain_id: 42161,
            sender: owner.to_owned(),
            to: cctp::TOKEN_MESSENGER.to_owned(),
            value: Quantity::from_u64(0),
            calldata: "0x12345678".into(),
            nonce: 8,
            gas: 200_000,
            max_fee_per_gas: Quantity::from_u64(2),
            max_priority_fee_per_gas: Quantity::from_u64(1),
            rollup_fee: None,
        };
        let receipt = CanonicalReceipt {
            hash: gofin_evm::hash(&hash).unwrap(),
            transaction_sender: owner.to_owned(),
            block_hash: gofin_evm::hash(&block_hash).unwrap(),
            block_number: 77,
            confirmations: 12,
            success: true,
            fee: U256::from(100),
            receipt: json!({"logs":logs_value}),
        };
        let transaction = json!({
            "hash":hash,
            "blockHash":block_hash,
            "blockNumber":"0x4d",
            "from":owner,
            "to":cctp::TOKEN_MESSENGER,
            "input":frozen.calldata,
            "value":"0x0",
            "chainId":"0xa4b1"
        });
        (
            transaction,
            receipt,
            frozen,
            metadata,
            owner.to_owned(),
            proxy.to_owned(),
        )
    }

    fn fixture() -> (
        FinalizedReceiptProof,
        Value,
        BurnRequest,
        CctpQuoteMetadata,
        String,
    ) {
        let owner = b58(&[8; 32]);
        let event_address = b58(&[10; 32]);
        let proxy = "0x1234567890123456789012345678901234567890".to_owned();
        let metadata = CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: cctp::solana::DOMAIN,
            destination_domain: cctp::solana::hypercore_source::DESTINATION_DOMAIN,
            transfer_mode: CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(10),
            min_finality_threshold: 1000,
        };
        let hook = cctp::hypercore::forward_hook(gofin_evm::address(&proxy).unwrap()).unwrap();
        let request = BurnRequest {
            owner: owner.clone(),
            message_sent_event_data: event_address.clone(),
            amount_atoms: 1_000_000,
            max_fee_atoms: 10,
            min_finality_threshold: 1000,
            hook_data: format!("0x{}", hex::encode(&hook)),
            message_sent_event_rent_lamports: 4_200_000,
        };
        let instruction = hypercore_source::burn_instruction(
            &request,
            &metadata,
            gofin_evm::address(&proxy).unwrap(),
        )
        .unwrap();
        let mut keys = Vec::new();
        for account in &instruction.accounts {
            if !keys.contains(&account.address) {
                keys.push(account.address.clone());
            }
        }
        let event_index = keys.iter().position(|key| key == &event_address).unwrap();
        let burn_account = instruction.accounts[3].address.clone();
        let burn_index = keys.iter().position(|key| key == &burn_account).unwrap() as u64;
        let payer_debit = 4_600_000u64;
        let mut pre_balances = vec![100_000_000; keys.len()];
        pre_balances[event_index] = 0;
        let mut post_balances = pre_balances.clone();
        post_balances[0] -= payer_debit;
        post_balances[event_index] = request.message_sent_event_rent_lamports;
        let token_balance = |amount: &str| {
            json!({
                "accountIndex": burn_index,
                "mint": cctp::solana::USDC_MINT,
                "owner": owner,
                "programId": gofin_solana::token::TOKEN_PROGRAM_ID,
                "uiTokenAmount": {"amount": amount, "decimals": 6}
            })
        };
        let proof = FinalizedReceiptProof {
            signature: "5h3SyntheticFinalizedSignature".into(),
            slot: 123,
            fee_lamports: 400_000,
            failed: false,
            message_sha256: "synthetic-message-hash".into(),
            account_keys: keys,
            message_instructions: vec![instruction],
            required_signatures: vec!["synthetic-signature-bytes".into()],
            pre_balances_lamports: pre_balances,
            post_balances_lamports: post_balances,
            pre_token_balances: vec![token_balance("1000000")],
            post_token_balances: vec![token_balance("0")],
            loaded_addresses: json!({}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };

        let remote = &proof.message_instructions[0].accounts[7].address;
        let pubkey_bytes = |address: &str| -> [u8; 32] {
            bs58::decode(address)
                .into_vec()
                .unwrap()
                .try_into()
                .unwrap()
        };
        let mut raw = vec![0u8; 432];
        let put_u32 = |raw: &mut [u8], offset: usize, value: u32| {
            raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        };
        put_u32(&mut raw, 0, 1);
        put_u32(&mut raw, 4, cctp::solana::DOMAIN);
        put_u32(
            &mut raw,
            8,
            cctp::solana::hypercore_source::DESTINATION_DOMAIN,
        );
        raw[44..76].copy_from_slice(&pubkey_bytes(cctp::solana::TOKEN_MESSENGER_MINTER));
        raw[76..108].copy_from_slice(&pubkey_bytes(remote));
        let forwarder = gofin_evm::address(cctp::hypercore::FORWARDER)
            .unwrap()
            .into_word()
            .0;
        raw[108..140].copy_from_slice(&forwarder);
        put_u32(&mut raw, 140, request.min_finality_threshold);
        put_u32(&mut raw, 148, 1);
        raw[152..184].copy_from_slice(&pubkey_bytes(cctp::solana::USDC_MINT));
        raw[184..216].copy_from_slice(&forwarder);
        raw[216..248].copy_from_slice(
            &gofin_evm::alloy::primitives::U256::from(request.amount_atoms).to_be_bytes::<32>(),
        );
        raw[248..280].copy_from_slice(&pubkey_bytes(&owner));
        raw[280..312].copy_from_slice(
            &gofin_evm::alloy::primitives::U256::from(request.max_fee_atoms).to_be_bytes::<32>(),
        );
        raw[376..].copy_from_slice(&hook);

        let mut account_data = vec![0x83, 0x64, 0x85, 0x38, 0xa6, 0xe1, 0x97, 0x3c];
        account_data.extend_from_slice(&pubkey_bytes(&owner));
        account_data.extend_from_slice(&1i64.to_le_bytes());
        account_data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        account_data.extend_from_slice(&raw);
        let event_account = json!({
            "context":{"slot":123},
            "value":{
                "owner":cctp::solana::MESSAGE_TRANSMITTER,
                "lamports":request.message_sent_event_rent_lamports,
                "data":[STANDARD.encode(account_data),"base64"]
            }
        });
        (proof, event_account, request, metadata, proxy)
    }

    #[test]
    fn finalized_solana_source_requires_exact_message_account_and_rent_debit() {
        let (proof, event, request, metadata, proxy) = fixture();
        let verified =
            verify_solana_source_burn(&proof, &event, &request, &metadata, &proxy, 500_000)
                .unwrap();
        assert_eq!(verified.amount_atoms, 1_000_000);
        assert_eq!(verified.network_fee_lamports, 400_000);
        assert_eq!(verified.retained_message_account_rent_lamports, 4_200_000);

        let mut fee_over = proof.clone();
        fee_over.fee_lamports = 500_001;
        assert!(
            verify_solana_source_burn(&fee_over, &event, &request, &metadata, &proxy, 500_000)
                .is_err()
        );

        let mut wrong_owner = event.clone();
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(wrong_owner["value"]["data"][0].as_str().unwrap())
            .unwrap();
        decoded[8] ^= 1;
        wrong_owner["value"]["data"][0] = json!(STANDARD.encode(decoded));
        assert!(
            verify_solana_source_burn(&proof, &wrong_owner, &request, &metadata, &proxy, 500_000)
                .is_err()
        );

        let mut wrong_rent_debit = proof.clone();
        wrong_rent_debit.post_balances_lamports[0] += 1;
        assert!(
            verify_solana_source_burn(
                &wrong_rent_debit,
                &event,
                &request,
                &metadata,
                &proxy,
                500_000,
            )
            .is_err()
        );
    }

    #[test]
    fn finalized_evm_source_burn_binds_frozen_envelope_and_exact_usdc_transfer() {
        let (transaction, receipt, frozen, metadata, owner, proxy) = evm_source_burn_fixture();
        let proof = verify_evm_source_burn(
            &transaction,
            &receipt,
            &frozen,
            &metadata,
            42161,
            &owner,
            12_345_678,
            &proxy,
            12,
            "100000000000000",
        )
        .unwrap();
        assert_eq!(proof.amount_atoms, 12_345_678);
        assert_eq!(proof.source_chain_id, 42161);
        assert_eq!(proof.confirmations, 12);

        let mut wrong_calldata = transaction.clone();
        wrong_calldata["input"] = json!("0xdeadbeef");
        assert!(
            verify_evm_source_burn(
                &wrong_calldata,
                &receipt,
                &frozen,
                &metadata,
                42161,
                &owner,
                12_345_678,
                &proxy,
                12,
                "100000000000000",
            )
            .is_err()
        );

        let mut insufficient_finality = receipt.clone();
        insufficient_finality.confirmations = 11;
        assert!(
            verify_evm_source_burn(
                &transaction,
                &insufficient_finality,
                &frozen,
                &metadata,
                42161,
                &owner,
                12_345_678,
                &proxy,
                12,
                "100000000000000",
            )
            .is_err()
        );
    }
}
