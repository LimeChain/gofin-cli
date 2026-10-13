//! CCTP execution coordination. Protocol parsing and proof interpretation live in fin-bridge.
use super::*;
use fin_bridge::cctp::{self, CctpProvider};

async fn require_unexpired_attestation(
    provider: &CctpProvider,
    chain: u64,
    message: &cctp::CctpMessage,
    stage: &str,
) -> fin_domain::Result<()> {
    if !message.expiration_block.is_zero()
        && message.expiration_block
            <= Quantity::from_u64(provider.expiration_block_number(chain).await?)
    {
        return Err(error(
            "CCTP_ATTESTATION_EXPIRED",
            stage,
            "The CCTP attestation has expired on the destination contract's block clock.",
            "Request bridge.recovery for this original burn, then preview the refreshed mint; do not repeat the burn.",
        ));
    }
    Ok(())
}

fn remaining_gas_balance(
    cap: fin_evm::alloy::primitives::U256,
    spent: fin_evm::alloy::primitives::U256,
    reserved: fin_evm::alloy::primitives::U256,
) -> fin_domain::Result<fin_evm::alloy::primitives::U256> {
    let remaining = cap.checked_sub(spent).ok_or_else(|| {
        FinError::validation("Verified bridge gas costs exceed the approved cap.")
    })?;
    if reserved > remaining {
        return Err(FinError::validation(
            "Unresolved bridge gas reservations exceed the remaining approved cap.",
        ));
    }
    // Reservations may still be wholly unspent onchain; do not release their
    // funding until canonical effects replace them with a verified actual fee.
    Ok(remaining)
}

fn historical_mint_attestation(
    quote: &BridgeQuote,
    locator: &fin_domain::BridgeDestinationLocator,
    receipt: &CanonicalReceipt,
    transaction: &Value,
    source_attestations: &[cctp::CctpAttestation],
) -> fin_domain::Result<cctp::CctpAttestation> {
    let chain = quote.intent.destination_chain_id;
    let sender = fin_evm::address(&quote.intent.source_address)?;
    if !receipt.success
        || fin_evm::hash(transaction["hash"].as_str().unwrap_or_default())? != receipt.hash
        || fin_evm::hash(transaction["blockHash"].as_str().unwrap_or_default())?
            != receipt.block_hash
        || fin_evm::rpc_u64(&transaction["blockNumber"])? != receipt.block_number
        || fin_evm::address(transaction["from"].as_str().unwrap_or_default())? != sender
        || fin_evm::address(&receipt.transaction_sender)? != sender
        || transaction
            .get("chainId")
            .is_some_and(|value| fin_evm::rpc_u64(value).ok() != Some(chain))
        || !fin_evm::rpc_u256(&transaction["value"])?.is_zero()
    {
        return Err(FinError::validation(
            "Historical CCTP mint transaction differs from its canonical receipt or fixed destination signer.",
        ));
    }
    let (message, attestation) = cctp::decode_mint_call(
        chain,
        transaction["to"].as_str().unwrap_or_default(),
        transaction["input"].as_str().unwrap_or_default(),
    )?;
    let nonce = cctp::decode_message(&message)?.nonce;
    // Source MessageSent has a zero nonce. Identical burns therefore have
    // identical immutable payloads; only Circle's source-transaction lookup
    // (or a persisted result of that lookup) links this nonce to this burn.
    let linked = source_attestations
        .iter()
        .find(|attestation| {
            attestation.nonce.eq_ignore_ascii_case(&nonce)
                && validate_attestation(quote, locator, attestation).is_ok()
        })
        .ok_or_else(|| {
            FinError::validation(
                "Historical mint nonce is not independently linked to the original source transaction.",
            )
        })?;
    let result = cctp::CctpAttestation {
        source_transaction_hash: linked.source_transaction_hash.clone(),
        message,
        attestation,
        nonce,
    };
    validate_attestation(quote, locator, &result)?;
    Ok(result)
}

impl BridgeAdapter {
    pub(super) fn cctp_provider(&self) -> fin_domain::Result<CctpProvider> {
        self.provider("cctp")?
            .as_cctp()
            .cloned()
            .ok_or_else(|| FinError::validation("CCTP provider is unavailable."))
    }

    pub(super) async fn validate_cctp_gas(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        alias: &str,
        transfer: &BridgeTransferRecord,
    ) -> fin_domain::Result<()> {
        for chain in [
            quote.intent.source.chain_id,
            quote.intent.destination_chain_id,
        ] {
            let wallet = self.wallet(alias, chain)?;
            let sender = wallet.address.as_deref().ok_or_else(internal_error)?;
            if !wallet.signing_enabled || !sender.eq_ignore_ascii_case(&quote.intent.source_address)
            {
                return Err(FinError::validation(
                    "CCTP requires the same reusable signing wallet on both networks.",
                ));
            }
            let runtime = &self
                .wallet_adapters
                .get(&chain)
                .ok_or_else(internal_error)?
                .runtime;
            let required = self.remaining_cctp_gas_balance(transfer, bounds, chain)?;
            if runtime.balance(fin_evm::address(sender)?, None).await? < required {
                return Err(error(
                    "CCTP_GAS_REQUIRED",
                    "preview",
                    &format!(
                        "{} needs existing ETH covering the remaining approved gas budget.",
                        chain_name(chain)
                    ),
                    "Fund this wallet's gas separately, then resume the original transfer; do not repeat a settled burn.",
                ));
            }
        }
        Ok(())
    }

    fn remaining_cctp_gas_balance(
        &self,
        transfer: &BridgeTransferRecord,
        bounds: &BridgeBounds,
        chain: u64,
    ) -> fin_domain::Result<fin_evm::alloy::primitives::U256> {
        let asset = EvmAsset {
            chain_id: chain,
            token: None,
            decimals: 18,
        }
        .asset_id();
        let (spent, reserved) = self.transfer_gas_usage(transfer, None, &asset)?;
        remaining_gas_balance(
            fin_evm::u256(&maximum_gas_cap(bounds, chain)?)?,
            spent,
            reserved,
        )
    }

    pub(super) async fn prepare_cctp_stage(
        &self,
        input: &Value,
        record: &BridgeTransferRecord,
        saved: &SavedQuote,
        mut step: BridgeStep,
        link: &BridgeStepLink,
        requested: Option<BridgeBounds>,
    ) -> fin_domain::Result<PreparedOperation> {
        if saved.quote.provider != "cctp"
            || link.kind != BridgeStepKind::Mint
            || record.grant_id.is_some()
        {
            return Err(FinError::validation(
                "CCTP mint requires its original transfer under exact or named-plan authority.",
            ));
        }
        let locator = cctp_locator(record)?;
        let bounds: BridgeBounds =
            serde_json::from_value(record.approved_bounds.clone().ok_or_else(internal_error)?)
                .map_err(|_| internal_error())?;
        if requested.is_some() && normalize_authorization_bounds(requested, saved)? != bounds {
            return Err(FinError::validation(
                "Mint must retain the original approved bounds and consumed fees.",
            ));
        }
        let alias = record.metadata["wallet_alias"]
            .as_str()
            .ok_or_else(internal_error)?;
        let chain = saved.quote.intent.destination_chain_id;
        let wallet = self.wallet(alias, chain)?;
        let sender = wallet.address.as_deref().ok_or_else(internal_error)?;
        if !wallet.signing_enabled
            || !sender.eq_ignore_ascii_case(&saved.quote.intent.source_address)
        {
            return Err(FinError::validation(
                "CCTP mint requires the approved destination signing wallet.",
            ));
        }
        let attestation = self
            .cctp_attestation(&saved.quote, locator)
            .await?
            .ok_or_else(|| {
                error(
                    "CCTP_ATTESTATION_PENDING",
                    "preview",
                    "Circle's attestation is not ready.",
                    "Poll bridge.status; retain the original burn identity.",
                )
            })?;
        let provider = self.cctp_provider()?;
        let message = cctp::decode_message(&attestation.message)?;
        require_unexpired_attestation(&provider, chain, &message, "preview").await?;
        if provider.nonce_used(chain, &message.nonce).await? {
            return Err(error(
                "CCTP_ALREADY_PROCESSED",
                "preview",
                "This CCTP message has already been processed.",
                "Reconcile bridge.status to verify the original destination receipt; do not mint again.",
            ));
        }
        provider
            .verify_attestation(chain, &attestation.message, &attestation.attestation)
            .await?;
        step.call = cctp::mint_call(chain, &attestation.message, &attestation.attestation)?;
        let runtime = &self
            .wallet_adapters
            .get(&chain)
            .ok_or_else(internal_error)?
            .runtime;
        let transaction = runtime
            .prepare(
                fin_evm::address(sender)?,
                fin_evm::address(&step.call.to)?,
                fin_evm::alloy::primitives::U256::ZERO,
                fin_evm::rpc_bytes(&json!(step.call.data))?,
                fin_evm::u256(&maximum_gas_cap(&bounds, chain)?)?,
            )
            .await?
            .transaction;
        let gas_asset = EvmAsset {
            chain_id: chain,
            token: None,
            decimals: 18,
        }
        .asset_id();
        self.validate_frozen_gas_cap(
            &bounds,
            &gas_asset,
            &fin_evm::quantity(transaction.maximum_cost()?),
        )
        .await?;
        self.validate_transfer_gas_budget(
            record,
            &link.step_id,
            &bounds,
            &gas_asset,
            transaction.maximum_cost()?,
        )?;
        let resource = wallet.resource;
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: FinAmount {
                    asset: gas_asset,
                    atoms: fin_evm::quantity(transaction.maximum_cost()?),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![resource.clone()],
        };
        let valuation = self.value_intent(&intent).await?;
        let mut plan = saved.plan.clone();
        plan.bounds = bounds.clone();
        plan.fixed_scope_digest = cctp::scope_digest(&saved.quote, &bounds, sender)?;
        let mut prepared = PreparedOperation {
            action_deadline:None,operation:"bridge.transfer".into(),input:input.clone(),
            binding:json!({"resource":resource,"transaction":transaction,"transfer_id":record.id,"step_id":link.step_id,"step_kind":link.kind,"step_dependencies":link.depends_on,"grant_id":null,"quote_id":saved.quote.quote_id,"route_id":saved.quote.route_id,"quote":saved.quote,"intent":saved.quote.intent,"plan_digest":plan.fixed_scope_digest,"plan":plan,"step_ids":saved.step_ids,"step":step,"bounds":bounds,"source_locator":locator,"cctp_attestation":attestation,
                (fin_domain::BRIDGE_STEP_METADATA_KEY):{"transfer_id":record.id,"grant_id":null,"step_id":link.step_id,"kind":"mint","bounds":bounds,"settlement_expectation":record.settlement_expectation}}),
            summary:self.summary(saved,&link.kind,alias), checks:vec![Check{name:"CCTP source and attestation".into(),status:CheckStatus::Passed,detail:"Verified original source message, attester signatures, exact destination call and gas bounds.".into()}], warnings:vec!["Mint submission is intermediate; transfer completion requires verified destination receipt.".into()], observed_at:self.clock.now().to_rfc3339(),expires_at:(self.clock.now()+chrono::Duration::seconds(90)).to_rfc3339(),valuation:Some(valuation),intent,operation_version:1,
        };
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    pub(super) async fn revalidate_cctp_mint(
        &self,
        prepared: &PreparedOperation,
        transaction: &FrozenTransaction,
        quote: &BridgeQuote,
    ) -> fin_domain::Result<PreparedOperation> {
        let record = self
            .state()?
            .transfer(
                prepared.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let locator = cctp_locator(&record)?;
        if quote.provider != "cctp"
            || json!(locator) != prepared.binding["source_locator"]
            || record.grant_id.is_some()
        {
            return Err(FinError::validation(
                "Mint source identity changed before dispatch.",
            ));
        }
        let attestation: cctp::CctpAttestation =
            serde_json::from_value(prepared.binding["cctp_attestation"].clone())
                .map_err(|_| internal_error())?;
        validate_attestation(quote, locator, &attestation)?;
        let provider = self.cctp_provider()?;
        provider
            .verify_attestation(
                quote.intent.destination_chain_id,
                &attestation.message,
                &attestation.attestation,
            )
            .await?;
        let message = cctp::decode_message(&attestation.message)?;
        require_unexpired_attestation(
            &provider,
            quote.intent.destination_chain_id,
            &message,
            "submission",
        )
        .await?;
        if provider
            .nonce_used(quote.intent.destination_chain_id, &message.nonce)
            .await?
        {
            return Err(error(
                "CCTP_ALREADY_PROCESSED",
                "submission",
                "CCTP message was already processed.",
                "Reconcile the original transfer; do not dispatch again.",
            ));
        }
        let call = cctp::mint_call(
            quote.intent.destination_chain_id,
            &attestation.message,
            &attestation.attestation,
        )?;
        if transaction.chain_id != call.chain_id
            || !transaction.to.eq_ignore_ascii_case(&call.to)
            || transaction.value != call.value
            || !transaction.calldata.eq_ignore_ascii_case(&call.data)
        {
            return Err(FinError::validation(
                "Frozen mint differs from its validated attestation.",
            ));
        }
        let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
            .map_err(|_| internal_error())?;
        let gas = EvmAsset {
            chain_id: call.chain_id,
            token: None,
            decimals: 18,
        }
        .asset_id();
        self.validate_frozen_gas_cap(
            &bounds,
            &gas,
            &fin_evm::quantity(transaction.maximum_cost()?),
        )
        .await?;
        self.validate_transfer_gas_budget(
            &record,
            prepared.binding["step_id"]
                .as_str()
                .ok_or_else(internal_error)?,
            &bounds,
            &gas,
            transaction.maximum_cost()?,
        )?;
        let mut fresh = prepared.clone();
        fresh.valuation = Some(self.value_intent(&fresh.intent).await?);
        fin_domain::value_bounds::carry_at(prepared, &mut fresh, self.clock.now())?;
        Ok(fresh)
    }
}

fn cctp_locator(
    record: &BridgeTransferRecord,
) -> fin_domain::Result<&fin_domain::BridgeDestinationLocator> {
    record
        .transit
        .as_ref()
        .and_then(|transit| transit.destination_locator.as_ref())
        .filter(|locator| locator.scheme == "cctp_v2_message")
        .ok_or_else(|| {
            error(
                "CCTP_SOURCE_PROOF_REQUIRED",
                "preview",
                "The original CCTP burn has not been verified.",
                "Reconcile the original burn operation before minting.",
            )
        })
}

fn validate_attestation(
    quote: &BridgeQuote,
    locator: &fin_domain::BridgeDestinationLocator,
    attestation: &cctp::CctpAttestation,
) -> fin_domain::Result<cctp::CctpMessage> {
    let source: cctp::CctpSourceMessage =
        serde_json::from_value(locator.proof["source"].clone())
            .map_err(|_| FinError::validation("CCTP source proof is malformed."))?;
    let metadata = quote.cctp.as_ref().ok_or_else(internal_error)?;
    if locator.destination_chain_id != quote.intent.destination_chain_id
        || locator.message_hash.as_deref() != Some(source.immutable_message_hash.as_str())
        || cctp::immutable_message_hash(&source.raw_message)? != source.immutable_message_hash
        || locator.proof["source_transaction_hash"]
            .as_str()
            .is_none_or(|hash| !hash.eq_ignore_ascii_case(&attestation.source_transaction_hash))
    {
        return Err(FinError::validation(
            "Attestation does not belong to the saved source transaction and message.",
        ));
    }
    let result =
        cctp::validate_attested_message(&source.raw_message, metadata, &attestation.message)?;
    if !result.nonce.eq_ignore_ascii_case(&attestation.nonce) {
        return Err(FinError::validation(
            "Iris nonce differs from its signed message.",
        ));
    }
    Ok(result)
}

pub(super) fn verify_source(
    quote: &BridgeQuote,
    receipt: &CanonicalReceipt,
    transfer_id: &str,
) -> fin_domain::Result<(Vec<FinancialEffect>, Option<VerifiedSourceProof>)> {
    let tx_hash = format!("{:#x}", receipt.hash);
    if !receipt.success || !receipt_logs_canonical(&receipt.receipt, receipt, &tx_hash)? {
        return Err(FinError::validation("CCTP source logs are not canonical."));
    }
    let logs = receipt.receipt["logs"]
        .as_array()
        .ok_or_else(internal_error)?
        .iter()
        .map(canonical_raw_log)
        .collect::<fin_domain::Result<Vec<_>>>()?;
    let source = cctp::verify_source_logs(
        &logs,
        &quote.intent,
        quote.cctp.as_ref().ok_or_else(internal_error)?,
    )?;
    let evidence_id = format!(
        "ethereum:{}:{}:{}",
        quote.intent.source.chain_id, tx_hash, source.log_index
    );
    let locator = fin_domain::BridgeDestinationLocator {
        scheme: "cctp_v2_message".into(),
        source_evidence_id: evidence_id.clone(),
        destination_chain_id: quote.intent.destination_chain_id,
        transaction_hash: String::new(),
        message_hash: Some(source.immutable_message_hash.clone()),
        proof: json!({"source":source,"source_transaction_hash":tx_hash,"source_block_number":receipt.block_number,"source_block_hash":format!("{:#x}",receipt.block_hash)}),
    };
    Ok((
        vec![
            FinancialEffect::CashDebit {
                resource: quote.intent.wallet.clone(),
                amount: quote.gross_debit.clone(),
            },
            FinancialEffect::TransitPrincipal {
                transfer_id: transfer_id.into(),
                amount: quote.gross_debit.clone(),
            },
        ],
        Some(VerifiedSourceProof {
            evidence_id,
            log_index: u64::from(source.log_index),
            correlation: source.immutable_message_hash,
            destination_locator: Some(locator),
        }),
    ))
}

pub(super) fn verify_mint_step(
    binding: &Value,
    receipt: &CanonicalReceipt,
) -> fin_domain::Result<()> {
    let quote: BridgeQuote =
        serde_json::from_value(binding["quote"].clone()).map_err(|_| internal_error())?;
    let locator: fin_domain::BridgeDestinationLocator =
        serde_json::from_value(binding["source_locator"].clone()).map_err(|_| internal_error())?;
    let attestation: cctp::CctpAttestation =
        serde_json::from_value(binding["cctp_attestation"].clone())
            .map_err(|_| internal_error())?;
    validate_attestation(&quote, &locator, &attestation)?;
    if !receipt_logs_canonical(&receipt.receipt, receipt, &format!("{:#x}", receipt.hash))? {
        return Err(FinError::validation("Mint logs are not canonical."));
    }
    let logs = receipt.receipt["logs"]
        .as_array()
        .ok_or_else(internal_error)?
        .iter()
        .map(canonical_raw_log)
        .collect::<fin_domain::Result<Vec<_>>>()?;
    cctp::verify_destination_logs(&logs, &attestation.message)?;
    Ok(())
}

impl BridgeAdapter {
    async fn cctp_attestation(
        &self,
        quote: &BridgeQuote,
        locator: &fin_domain::BridgeDestinationLocator,
    ) -> fin_domain::Result<Option<cctp::CctpAttestation>> {
        let metadata = quote.cctp.as_ref().ok_or_else(internal_error)?;
        let tx = locator.proof["source_transaction_hash"]
            .as_str()
            .ok_or_else(internal_error)?;
        match self
            .cctp_provider()?
            .fetch_attestation(metadata.source_domain, tx)
            .await?
        {
            cctp::CctpAttestationStatus::Pending => Ok(None),
            cctp::CctpAttestationStatus::Ready { messages } => {
                // Direct Fin burns emit exactly one message. Ambiguity is never resolved by picking the first row.
                if messages.len() != 1 {
                    return Err(FinError::validation(
                        "Iris returned ambiguous messages for a direct CCTP burn.",
                    ));
                }
                let attestation = messages.into_iter().next().ok_or_else(internal_error)?;
                validate_attestation(quote, locator, &attestation)?;
                Ok(Some(attestation))
            }
        }
    }

    async fn reconcile_cctp_candidates(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
        candidates: &[String],
        saved_attestations: &[cctp::CctpAttestation],
    ) -> fin_domain::Result<Option<Value>> {
        let locator = cctp_locator(transfer)?;
        let chain = saved.quote.intent.destination_chain_id;
        let runtime = self.observers.get(&chain).ok_or_else(internal_error)?;
        let (confirmations, finalized) = self
            .confirmation_rules
            .get(&chain)
            .copied()
            .ok_or_else(internal_error)?;
        for candidate in candidates {
            let Some(receipt) = runtime
                .observe_receipt(candidate, confirmations, finalized)
                .await?
            else {
                continue;
            };
            if !receipt.success || !receipt_logs_canonical(&receipt.receipt, &receipt, candidate)? {
                continue;
            }
            let logs = receipt.receipt["logs"]
                .as_array()
                .ok_or_else(internal_error)?
                .iter()
                .map(canonical_raw_log)
                .collect::<fin_domain::Result<Vec<_>>>()?;
            let matched = saved_attestations.iter().find_map(|attestation| {
                cctp::verify_destination_logs(&logs, &attestation.message)
                    .ok()
                    .map(|index| (attestation.clone(), index))
            });
            let (attestation, log_index) = if let Some(matched) = matched {
                matched
            } else {
                // Iris may now report a different generation, or be unavailable.
                // Decode the bytes actually executed in this canonical receipt.
                let tx = runtime
                    .rpc
                    .request("eth_getTransactionByHash", json!([candidate]))
                    .await?;
                let Ok(historical) = historical_mint_attestation(
                    &saved.quote,
                    locator,
                    &receipt,
                    &tx,
                    saved_attestations,
                ) else {
                    continue;
                };
                let Ok(index) = cctp::verify_destination_logs(&logs, &historical.message) else {
                    continue;
                };
                (historical, index)
            };
            let message = validate_attestation(&saved.quote, locator, &attestation)?;
            let resource = fin_domain::ResourceId(format!(
                "ethereum:{chain}:{}",
                saved.quote.intent.recipient.to_ascii_lowercase()
            ));
            let amount = FinAmount {
                asset: saved.quote.expected_receipt.asset.clone(),
                atoms: message.amount.clone(),
            };
            let evidence_id = format!(
                "ethereum:{chain}:{}:{log_index}",
                candidate.to_ascii_lowercase()
            );
            let evidence = json!({"transfer_id":transfer.id,"log_index":log_index,"correlation":message.nonce,"canonical":true,"success":true,"confirmations":receipt.confirmations,"recipient":saved.quote.intent.recipient,"provider":"cctp","chain_id":chain,"transaction_hash":candidate,"message_hash":locator.message_hash,"attested_message_hash":format!("{:#x}",fin_evm::alloy::primitives::keccak256(fin_evm::rpc_bytes(&json!(attestation.message))?)),"source_evidence_id":locator.source_evidence_id,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"recipient_resource":resource,"asset":amount.asset,"amount_atoms":amount.atoms,"net_amount_atoms":message.amount.checked_sub(&message.fee_executed)?,"fee_executed":message.fee_executed,"evidence_id":evidence_id,"proof_method":"cctp_v2_message_and_native_usdc_mint"});
            let mut effects = vec![FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount,
            }];
            if !message.fee_executed.is_zero() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash { resource },
                    amount: FinAmount {
                        asset: saved.quote.expected_receipt.asset.clone(),
                        atoms: message.fee_executed.clone(),
                    },
                    category: "cctp_protocol_fee".into(),
                    treatment: CostTreatment::DeductedFromCredit,
                });
            }
            let mut proof = fin_domain::BridgeSettlementProof {
                transfer_id: transfer.id.clone(),
                outcome: fin_domain::BridgeSettlementOutcome::Destination,
                evidence_id: evidence_id.clone(),
                effects,
                evidence,
                observed_at: self.clock.now().to_rfc3339(),
                proof_digest: String::new(),
            };
            proof.proof_digest = fin_domain::bridge_settlement_digest(&proof)?;
            engine.commit_bridge_external_settlement(proof)?;
            return Ok(Some(
                json!({"provider":"cctp","settled":true,"status":"destination_settled","evidence_id":evidence_id,"transaction_hash":candidate}),
            ));
        }
        Ok(None)
    }

    pub(super) async fn reconcile_cctp_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> fin_domain::Result<Value> {
        let locator = cctp_locator(transfer)?;
        let chain = saved.quote.intent.destination_chain_id;
        let runtime = self.observers.get(&chain).ok_or_else(internal_error)?;
        let provider = self.cctp_provider()?;
        let operations = engine.operations()?;
        let mut candidates = Vec::new();
        let mut attestation = None;
        let mut saved_attestations = Vec::new();
        let mut uncertain = false;
        let mut attestation_error = None;
        for link in transfer
            .steps
            .iter()
            .filter(|link| link.kind == BridgeStepKind::Mint)
        {
            if let Some(operation) = operations
                .iter()
                .find(|op| Some(&op.id) == link.operation_id.as_ref())
                && let Some(submission) = &operation.submission
            {
                candidates.push(submission.reference.clone());
                uncertain |= !operation.status.terminal();
                if let Some(preview) = &operation.preview {
                    let saved_attestation: cctp::CctpAttestation =
                        serde_json::from_value(preview.binding["cctp_attestation"].clone())
                            .map_err(|_| internal_error())?;
                    validate_attestation(&saved.quote, locator, &saved_attestation)?;
                    saved_attestations.push(saved_attestation.clone());
                    attestation = Some(saved_attestation);
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        // Persisted submission identities are the strongest locators. Prove
        // them first without depending on Iris or a broad destination log scan.
        let known_candidates = if !saved_attestations.is_empty() {
            if let Some(settled) = self
                .reconcile_cctp_candidates(
                    engine,
                    transfer,
                    saved,
                    &candidates,
                    &saved_attestations,
                )
                .await?
            {
                return Ok(settled);
            }
            std::mem::take(&mut candidates)
        } else {
            // A newly available Iris response can supply the missing source
            // nonce linkage, so retain these candidates for the second pass.
            Vec::new()
        };
        if attestation.is_none() || !uncertain {
            match self.cctp_attestation(&saved.quote, locator).await {
                Ok(Some(current)) => attestation = Some(current),
                Ok(None) => {}
                Err(error) if attestation.is_none() => attestation_error = Some(error),
                Err(_) => {} // Saved signed submissions still reconcile during Iris outages.
            }
        }
        let message = attestation
            .as_ref()
            .map(|value| validate_attestation(&saved.quote, locator, value))
            .transpose()?;
        // A used nonce is only a locator hint. Exact receipt proof is still required.
        let used = if let Some(message) = &message {
            provider.nonce_used(chain, &message.nonce).await?
        } else {
            false
        };
        if used {
            let head = fin_evm::rpc_u64(&runtime.rpc.request("eth_blockNumber", json!([])).await?)?;
            let topic = fin_evm::alloy::primitives::keccak256(
                "MessageReceived(address,uint32,bytes32,bytes32,uint32,bytes)",
            );
            // Never discover by caller/immutable payload alone: another burn
            // with identical terms has the same source message bytes.
            let caller = format!(
                "{:#x}",
                fin_evm::address(&saved.quote.intent.source_address)?.into_word()
            );
            let nonce = message.as_ref().map(|message| message.nonce.clone());
            let logs=runtime.rpc.request("eth_getLogs",json!([{"address":cctp::MESSAGE_TRANSMITTER,"fromBlock":format!("{:#x}",head.saturating_sub(10_000)),"toBlock":format!("{head:#x}"),"topics":[format!("{topic:#x}"),caller,nonce]}])).await?;
            if let Some(logs) = logs.as_array().filter(|logs| logs.len() <= 16) {
                for log in logs {
                    if let Some(hash) = log["transactionHash"].as_str() {
                        fin_evm::hash(hash)?;
                        candidates.push(hash.to_owned());
                    }
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        candidates.retain(|candidate| !known_candidates.contains(candidate));
        if let Some(attestation) = &attestation {
            saved_attestations.push(attestation.clone());
        }
        if let Some(settled) = self
            .reconcile_cctp_candidates(engine, transfer, saved, &candidates, &saved_attestations)
            .await?
        {
            return Ok(settled);
        }
        if used {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"processed_receipt_pending","locator_coverage":"Fin submission identities and latest 10,000 destination blocks; external discovery may be incomplete","next_action":"The nonce was processed but exact destination credit is not yet proven. Preserve the transfer and reconcile; do not mint again."}),
            );
        }
        if uncertain {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"mint_submission_uncertain","next_action":"Reconcile the original mint operation before considering any further write."}),
            );
        }
        let Some(message) = message else {
            if let Some(error) = attestation_error {
                return Err(error);
            }
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"attestation_pending","locator_coverage":"Destination discovery requires a nonce independently linked to the original source transaction by Circle.","next_action":"Wait for Circle attestation and poll bridge.status; preserve the original burn identity."}),
            );
        };
        if !message.expiration_block.is_zero()
            && message.expiration_block
                <= Quantity::from_u64(provider.expiration_block_number(chain).await?)
        {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"attestation_expired","next_action":"Request bridge.recovery to refresh this same burn's attestation, then explicitly prepare mint under current authority."}),
            );
        }
        if !transfer.steps.iter().any(|link| {
            link.kind == BridgeStepKind::Mint && link.state == BridgeStepState::Unsubmitted
        }) {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"mint_recovery_required","next_action":"Request bridge.recovery for the proven failed mint, then review the new attempt under current authority."}),
            );
        }
        let alias = transfer.metadata["wallet_alias"]
            .as_str()
            .ok_or_else(internal_error)?;
        let bounds: BridgeBounds = serde_json::from_value(
            transfer
                .approved_bounds
                .clone()
                .ok_or_else(internal_error)?,
        )
        .map_err(|_| internal_error())?;
        let wallet = self.wallet(alias, chain)?;
        if runtime
            .balance(
                fin_evm::address(wallet.address.as_deref().ok_or_else(internal_error)?)?,
                None,
            )
            .await?
            < self.remaining_cctp_gas_balance(transfer, &bounds, chain)?
        {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"destination_gas_required","next_action":"Fund ETH on the destination separately, then resume the saved mint; do not repeat the burn."}),
            );
        }
        Ok(
            json!({"provider":"cctp","settled":false,"status":"mint_ready","requested_mode":saved.quote.cctp.as_ref().map(|m|m.transfer_mode),"executed_finality":message.finality_threshold_executed,"next_action":"Preview and explicitly execute the saved mint stage under current plan authority."}),
        )
    }
}

impl BridgeAdapter {
    pub(super) async fn bridge_recovery(&self, input: &Value) -> fin_domain::Result<Value> {
        let request: BridgeStatusInput = serde_json::from_value(input.clone())
            .map_err(|_| FinError::validation("Use bridge.recovery transferId."))?;
        let state = self.state()?;
        let record = state
            .transfer(&request.transfer_id)
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        let saved = self.saved_quote(&record)?;
        if saved.quote.provider != "cctp" {
            return self.base_recovery(input).await;
        }
        if record.grant_id.is_some()
            || !matches!(
                record.state,
                BridgeTransferState::InTransit
                    | BridgeTransferState::DestinationPending
                    | BridgeTransferState::NeedsAttention
            )
        {
            return Err(FinError::validation(
                "CCTP recovery requires the original unresolved transfer.",
            ));
        }
        let locator = cctp_locator(&record)?;
        let engine = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(internal_error)?;
        let operations = engine.operations()?;
        for link in record
            .steps
            .iter()
            .filter(|link| link.kind == BridgeStepKind::Mint)
        {
            if link.state == BridgeStepState::Submitted
                || link.operation_id.as_ref().is_some_and(|id| {
                    operations
                        .iter()
                        .find(|op| &op.id == id)
                        .is_none_or(|op| !op.status.terminal())
                })
            {
                return Err(error(
                    "BRIDGE_RECOVERY_ATTEMPT_UNRESOLVED",
                    "recovery",
                    "A mint attempt is still unresolved.",
                    "Reconcile its original operation before declaring recovery.",
                ));
            }
        }
        let mut refreshed = false;
        if let Some(attestation) = self.cctp_attestation(&saved.quote, locator).await? {
            let message = validate_attestation(&saved.quote, locator, &attestation)?;
            let provider = self.cctp_provider()?;
            let chain = saved.quote.intent.destination_chain_id;
            if provider.nonce_used(chain, &message.nonce).await? {
                return Err(error(
                    "CCTP_ALREADY_PROCESSED",
                    "recovery",
                    "The message has already been processed.",
                    "Use bridge.status to verify destination credit.",
                ));
            }
            if !message.expiration_block.is_zero()
                && message.expiration_block
                    <= Quantity::from_u64(provider.expiration_block_number(chain).await?)
            {
                provider
                    .verify_attestation(chain, &attestation.message, &attestation.attestation)
                    .await?;
                provider.reattest(&message.nonce).await?;
                refreshed = true;
            }
        }
        if let Some(link) = record.steps.iter().rev().find(|link| {
            link.kind == BridgeStepKind::Mint
                && link.state == BridgeStepState::Unsubmitted
                && link.operation_id.is_none()
        }) {
            return Ok(
                json!({"transfer_id":record.id,"message_hash":locator.message_hash,"steps":[link],"created":false,"attestation_refresh_requested":refreshed,"next_action":"Poll bridge.status, then explicitly submit the saved mint under current authority."}),
            );
        }
        let parent = record
            .steps
            .iter()
            .rev()
            .find(|link| {
                link.kind == BridgeStepKind::Mint
                    && link.state == BridgeStepState::NeedsAttention
                    && link.operation_id.as_ref().is_some_and(|id| {
                        operations
                            .iter()
                            .any(|op| &op.id == id && op.status == Status::Failed)
                    })
            })
            .ok_or_else(|| {
                FinError::validation("No canonically failed CCTP mint requires a new attempt.")
            })?;
        let deposit = record
            .steps
            .iter()
            .find(|link| {
                link.kind == BridgeStepKind::Deposit && link.state == BridgeStepState::Settled
            })
            .ok_or_else(internal_error)?;
        let deposit_ordinal = *saved
            .step_ids
            .iter()
            .find(|(_, id)| *id == &deposit.step_id)
            .ok_or_else(internal_error)?
            .0;
        let mut plan = saved.plan.clone();
        let ordinal = u8::try_from(plan.steps.len())
            .map_err(|_| FinError::validation("CCTP recovery history reached its finite bound."))?;
        let mut step = plan
            .steps
            .iter()
            .find(|step| step.kind == ProviderStepKind::Mint)
            .cloned()
            .ok_or_else(internal_error)?;
        step.ordinal = ordinal;
        step.dependencies = vec![deposit_ordinal];
        plan.steps.push(step);
        let step_id = uuid::Uuid::new_v4().to_string();
        let mut ids = saved.step_ids.clone();
        ids.insert(ordinal, step_id.clone());
        let mut updated = record.clone();
        let link = BridgeStepLink {
            transfer_id: record.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: step_id.clone(),
            operation_id: None,
            kind: BridgeStepKind::Mint,
            depends_on: vec![deposit.step_id.clone()],
            state: BridgeStepState::Unsubmitted,
        };
        updated.steps.push(link.clone());
        let prior = record.quote_revisions.last().ok_or_else(internal_error)?;
        let mut payload = prior.payload.clone();
        payload["plan"] = json!(plan);
        payload["step_ids"] = json!(ids);
        payload["cctp_recovery"] = json!({"message_hash":locator.message_hash,"reason":"stage_reverted","parent_step_id":parent.step_id,"parent_operation_id":parent.operation_id,"step_ids":[step_id]});
        updated.quote_revisions.push(BridgeQuoteRevision {
            revision: prior.revision.checked_add(1).ok_or_else(internal_error)?,
            provider: prior.provider.clone(),
            route_id: prior.route_id.clone(),
            order_id: prior.order_id.clone(),
            quote_hash: binding_hash(&payload)?,
            payload,
            observed_at: now(),
            valid_until: prior.valid_until.clone(),
        });
        updated.version = record.version.checked_add(1).ok_or_else(internal_error)?;
        updated.state = BridgeTransferState::DestinationPending;
        state
            .append_cctp_recovery(&record, &updated)
            .map_err(store_error)?;
        Ok(
            json!({"transfer_id":record.id,"message_hash":locator.message_hash,"steps":[link],"created":true,"attestation_refresh_requested":refreshed,"next_action":"Review this new mint identity and revise the same named plan, or obtain exact approval. Original burn, consumed fees and prior attempts are preserved."}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_evm::alloy::primitives::{B256, U256, keccak256};

    #[test]
    fn gas_balance_deducts_only_proven_spend_and_preserves_uncertain_reservations() {
        assert_eq!(
            remaining_gas_balance(U256::from(100), U256::from(60), U256::from(10)).unwrap(),
            U256::from(40)
        );
        assert_eq!(
            remaining_gas_balance(U256::from(100), U256::ZERO, U256::from(80)).unwrap(),
            U256::from(100)
        );
        assert!(remaining_gas_balance(U256::from(100), U256::from(101), U256::ZERO).is_err());
        assert!(remaining_gas_balance(U256::from(100), U256::from(60), U256::from(41)).is_err());
    }

    fn historical_fixture() -> (
        BridgeQuote,
        fin_domain::BridgeDestinationLocator,
        CanonicalReceipt,
        Value,
    ) {
        let sender = "0x1111111111111111111111111111111111111111";
        let recipient = "0x2222222222222222222222222222222222222222";
        let source = cctp::usdc_asset(1).unwrap();
        let destination = cctp::usdc_asset(8453).unwrap();
        let metadata = cctp::CctpQuoteMetadata {
            protocol_version: 2,
            source_domain: 0,
            destination_domain: 6,
            transfer_mode: cctp::CctpTransferMode::Fast,
            max_fee: Quantity::from_u64(100),
            min_finality_threshold: 1000,
        };
        let intent = BridgeIntent {
            wallet: fin_domain::ResourceId("wallet".into()),
            source_address: sender.into(),
            source: source.clone(),
            destination_chain_id: 8453,
            destination_assets: vec![destination.clone()],
            recipient: recipient.into(),
            refund_to: sender.into(),
            allowed_refund_assets: vec![],
            amount: Quantity::from_u64(1_000_000),
        };
        let quote = BridgeQuote {
            quote_id: "fixture".into(),
            provider: "cctp".into(),
            route_id: "cctp-v2:1:8453".into(),
            gross_debit: source.amount(intent.amount.clone()),
            expected_receipt: destination.amount(Quantity::from_u64(999_900)),
            minimum_receipt: destination.amount(Quantity::from_u64(999_900)),
            intent,
            fees: vec![],
            approval: None,
            protocol_steps: vec![],
            estimated_arrival_seconds: None,
            observed_at: now(),
            valid_until: now(),
            class: fin_bridge::QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
            cctp: Some(metadata),
        };
        let mut raw = vec![0u8; 376];
        for (offset, value) in [(0, 1u32), (8, 6), (140, 1000), (148, 1)] {
            raw[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        for (offset, value) in [
            (44, cctp::TOKEN_MESSENGER),
            (76, cctp::TOKEN_MESSENGER),
            (108, sender),
            (152, source.token.as_deref().unwrap()),
            (184, recipient),
            (248, sender),
        ] {
            raw[offset..offset + 32]
                .copy_from_slice(fin_evm::address(value).unwrap().into_word().as_slice());
        }
        for (offset, value) in [(216, 1_000_000u64), (280, 100)] {
            raw[offset..offset + 32].copy_from_slice(&U256::from(value).to_be_bytes::<32>());
        }
        let raw_source = fin_evm::hex_bytes(&raw);
        let immutable = cctp::immutable_message_hash(&raw_source).unwrap();
        let source_tx = format!("{:#x}", B256::repeat_byte(1));
        let source_proof = cctp::CctpSourceMessage {
            raw_message: raw_source,
            message_hash: format!("{:#x}", keccak256(&raw)),
            immutable_message_hash: immutable.clone(),
            log_index: 3,
        };
        let locator = fin_domain::BridgeDestinationLocator {
            scheme: "cctp_v2_message".into(),
            source_evidence_id: "source-proof".into(),
            destination_chain_id: 8453,
            transaction_hash: String::new(),
            message_hash: Some(immutable),
            proof: json!({"source":source_proof,"source_transaction_hash":source_tx}),
        };
        raw[12..44].fill(7);
        raw[144..148].copy_from_slice(&1000u32.to_be_bytes());
        raw[312..344].copy_from_slice(&U256::from(80).to_be_bytes::<32>());
        raw[344..376].copy_from_slice(&U256::from(100).to_be_bytes::<32>());
        let call = cctp::mint_call(
            8453,
            &fin_evm::hex_bytes(&raw),
            &fin_evm::hex_bytes(&[1u8; 65]),
        )
        .unwrap();
        let receipt = CanonicalReceipt {
            hash: B256::repeat_byte(2),
            transaction_sender: sender.into(),
            block_hash: B256::repeat_byte(3),
            block_number: 10,
            confirmations: 5,
            success: true,
            fee: U256::from(1),
            receipt: json!({}),
        };
        let tx = json!({"hash":format!("{:#x}",receipt.hash),"blockHash":format!("{:#x}",receipt.block_hash),"blockNumber":"0xa","chainId":"0x2105","from":sender,"to":cctp::MESSAGE_TRANSMITTER,"input":call.data,"value":"0x0"});
        (quote, locator, receipt, tx)
    }

    #[test]
    fn external_historical_mint_binds_canonical_transaction_and_original_burn() {
        let (quote, locator, receipt, tx) = historical_fixture();
        let (message, attestation) =
            cctp::decode_mint_calldata(tx["input"].as_str().unwrap()).unwrap();
        let mut refreshed = fin_evm::rpc_bytes(&json!(message)).unwrap().to_vec();
        refreshed[312..344].copy_from_slice(&U256::from(90).to_be_bytes::<32>());
        refreshed[344..376].copy_from_slice(&U256::from(200).to_be_bytes::<32>());
        let linked = cctp::CctpAttestation {
            source_transaction_hash: locator.proof["source_transaction_hash"]
                .as_str()
                .unwrap()
                .into(),
            nonce: cctp::decode_message(&message).unwrap().nonce,
            message: fin_evm::hex_bytes(&refreshed),
            attestation,
        };
        let source_attestations = [linked];
        assert!(historical_mint_attestation(&quote, &locator, &receipt, &tx, &[]).is_err());
        let mut unrelated = source_attestations.clone();
        let mut wrong_nonce = refreshed;
        wrong_nonce[12..44].fill(8);
        unrelated[0].message = fin_evm::hex_bytes(&wrong_nonce);
        unrelated[0].nonce = cctp::decode_message(&unrelated[0].message).unwrap().nonce;
        assert!(historical_mint_attestation(&quote, &locator, &receipt, &tx, &unrelated).is_err());
        let actual =
            historical_mint_attestation(&quote, &locator, &receipt, &tx, &source_attestations)
                .unwrap();
        let decoded = cctp::decode_message(&actual.message).unwrap();
        assert_eq!(decoded.fee_executed, Quantity::from_u64(80));
        assert_eq!(decoded.expiration_block, Quantity::from_u64(100));
        // A later Iris generation may carry a different expiry, but settlement
        // must retain the actual bytes and fee proven by this historical call.
        for (field, value) in [
            ("hash", json!(format!("{:#x}", B256::repeat_byte(9)))),
            ("blockHash", json!(format!("{:#x}", B256::repeat_byte(9)))),
            ("blockNumber", json!("0xb")),
            ("chainId", json!("0x1")),
            ("from", json!(quote.intent.recipient)),
            ("to", json!(cctp::TOKEN_MESSENGER)),
            ("value", json!("0x1")),
        ] {
            let mut changed = tx.clone();
            changed[field] = value;
            assert!(
                historical_mint_attestation(
                    &quote,
                    &locator,
                    &receipt,
                    &changed,
                    &source_attestations
                )
                .is_err(),
                "{field}"
            );
        }
        let mut raw = fin_evm::rpc_bytes(&json!(actual.message)).unwrap().to_vec();
        raw[247] ^= 1;
        let wrong = cctp::mint_call(8453, &fin_evm::hex_bytes(&raw), &actual.attestation).unwrap();
        let mut changed = tx;
        changed["input"] = json!(wrong.data);
        assert!(
            historical_mint_attestation(&quote, &locator, &receipt, &changed, &source_attestations)
                .is_err()
        );
    }
}
