//! Read-only reconciliation of the original CCTP burn and Solana mint identity.
use super::cctp_lifecycle::{cctp_locator, validate_attestation};
use super::*;
use gofin_bridge::cctp::{self, solana as protocol};
use gofin_domain::{OperationRecord, Reconciliation};
use gofin_solana::{FinalizedReceiptProof, ReceiptOutcome};

fn sol_cost(resource: &gofin_domain::ResourceId, atoms: u64, category: &str) -> FinancialEffect {
    FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: GoFinAmount {
            asset: gofin_domain::AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: Quantity::from_u64(atoms),
        },
        category: category.into(),
        treatment: CostTreatment::AdditionalDebit,
    }
}

fn verify_bound_receipt(
    record: &OperationRecord,
    proof: &FinalizedReceiptProof,
) -> gofin_domain::Result<(BridgeQuote, cctp::CctpAttestation)> {
    let preview = record.preview.as_ref().ok_or_else(internal_error)?;
    let binding = &preview.binding;
    let quote: BridgeQuote =
        serde_json::from_value(binding["quote"].clone()).map_err(|_| internal_error())?;
    let destination = quote
        .intent
        .solana_destination
        .as_ref()
        .ok_or_else(internal_error)?;
    protocol::validate_destination(destination)?;
    let instructions: Vec<gofin_solana::NormalizedInstruction> =
        serde_json::from_value(binding["solana_instructions"].clone())
            .map_err(|_| internal_error())?;
    if record.operation != "bridge.transfer"
        || preview.operation != record.operation
        || preview.input != record.input
        || quote.provider != "cctp"
        || binding["step_kind"] != "mint"
        || binding["resource"] != json!(destination.wallet_resource)
        || binding["solana_message_sha256"].as_str() != Some(proof.message_sha256.as_str())
        || proof.account_keys.first() != Some(&destination.wallet_address)
        || proof.message_instructions != instructions
        || proof.fee_lamports
            > binding["solana_maximum_network_fee_lamports"]
                .as_u64()
                .ok_or_else(internal_error)?
    {
        return Err(GoFinError::validation(
            "Finalized CCTP Solana receipt differs from its original bound operation.",
        ));
    }
    let locator: gofin_domain::BridgeDestinationLocator =
        serde_json::from_value(binding["source_locator"].clone()).map_err(|_| internal_error())?;
    let attestation: cctp::CctpAttestation =
        serde_json::from_value(binding["cctp_attestation"].clone())
            .map_err(|_| internal_error())?;
    validate_attestation(&quote, &locator, &attestation)?;
    Ok((quote, attestation))
}

fn verify_failed_receipt(
    proof: &FinalizedReceiptProof,
    destination: &gofin_bridge::SolanaDestination,
) -> gofin_domain::Result<()> {
    let expected = gofin_domain::SignedQuantity::negative(&Quantity::from_u64(proof.fee_lamports));
    if !proof.failed || proof.lamport_delta(&destination.wallet_address)? != expected {
        return Err(GoFinError::validation(
            "Failed Solana mint does not prove fee-only atomic rollback.",
        ));
    }
    let mut mints = std::collections::BTreeSet::new();
    for row in proof
        .pre_token_balances
        .iter()
        .chain(&proof.post_token_balances)
    {
        if row["owner"].as_str() == Some(destination.wallet_address.as_str()) {
            mints.insert(row["mint"].as_str().ok_or_else(internal_error)?);
        }
    }
    for mint in mints {
        if !proof
            .token_delta(&destination.wallet_address, mint)?
            .is_zero()
        {
            return Err(GoFinError::validation(
                "Failed Solana mint unexpectedly changed wallet tokens.",
            ));
        }
    }
    Ok(())
}

impl BridgeAdapter {
    pub(super) async fn reconcile_cctp_solana_step(
        &self,
        record: &OperationRecord,
    ) -> gofin_domain::Result<Reconciliation> {
        let submission = record.submission.as_ref().ok_or_else(internal_error)?;
        let preview = record.preview.as_ref().ok_or_else(internal_error)?;
        let quote: BridgeQuote = serde_json::from_value(preview.binding["quote"].clone())
            .map_err(|_| internal_error())?;
        let destination = quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet_alias = destination.wallet.as_ref().ok_or_else(|| {
            GoFinError::validation("CCTP Solana requires its enrolled destination wallet.")
        })?;
        let wallet = self.solana_wallets.get(wallet_alias).ok_or_else(|| {
            GoFinError::validation("Original Solana destination wallet is unavailable.")
        })?;
        if wallet.address() != destination.wallet_address {
            return Err(GoFinError::validation(
                "Original Solana wallet identity changed.",
            ));
        }
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::ExpiredWithoutInclusion { reason, .. } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"reason":reason,"settlement_verifier":"expired_without_inclusion_not_supported"}),
                verified: None,
            }),
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason,"submission":submission}),
                verified: None,
            }),
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let evidence = json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"financial_effects":[]});
                Ok(Reconciliation {
                    status: Status::Failed,
                    verified: Some(verified_bundle(
                        record,
                        submission.clone(),
                        vec![],
                        evidence.clone(),
                    )?),
                    evidence,
                })
            }
            ReceiptOutcome::Finalized { proof, .. } => {
                let (quote, attestation) = verify_bound_receipt(record, &proof)?;
                let destination = quote
                    .intent
                    .solana_destination
                    .as_ref()
                    .ok_or_else(internal_error)?;
                let mut effects = Vec::new();
                if proof.fee_lamports > 0 {
                    effects.push(sol_cost(
                        &destination.wallet_resource,
                        proof.fee_lamports,
                        "gas",
                    ));
                }
                if proof.failed {
                    verify_failed_receipt(&proof, destination)?;
                } else {
                    let verified =
                        protocol::verify_solana_receipt(&proof, destination, &attestation)?;
                    let rent_cap = preview.binding["solana_account_funding_lamports"]
                        .as_u64()
                        .ok_or_else(internal_error)?;
                    if verified.actual_nonce_rent_lamports > rent_cap {
                        return Err(GoFinError::validation(
                            "Solana nonce account funding exceeds the original cap.",
                        ));
                    }
                    let spent = proof
                        .fee_lamports
                        .checked_add(verified.actual_nonce_rent_lamports)
                        .ok_or_else(internal_error)?;
                    if proof.lamport_delta(&destination.wallet_address)?
                        != gofin_domain::SignedQuantity::negative(&Quantity::from_u64(spent))
                    {
                        return Err(GoFinError::validation(
                            "Solana mint payer debit differs from verified fee and nonce rent.",
                        ));
                    }
                    if verified.actual_nonce_rent_lamports > 0 {
                        effects.push(sol_cost(
                            &destination.wallet_resource,
                            verified.actual_nonce_rent_lamports,
                            "cctp_nonce_rent",
                        ));
                    }
                }
                let evidence = json!({"network":gofin_solana::NETWORK,"transaction_hash":proof.signature,"signature":proof.signature,"slot":proof.slot,"canonical":true,"commitment":"finalized","success":!proof.failed,"transfer_id":preview.binding["transfer_id"],"step_id":preview.binding["step_id"],"proof":proof});
                Ok(Reconciliation {
                    status: if proof.failed {
                        Status::Failed
                    } else {
                        Status::Completed
                    },
                    verified: Some(verified_bundle(
                        record,
                        submission.clone(),
                        effects,
                        evidence.clone(),
                    )?),
                    evidence,
                })
            }
        }
    }

    fn commit_solana_cctp_receipt(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
        attestation: &cctp::CctpAttestation,
        proof: &FinalizedReceiptProof,
    ) -> gofin_domain::Result<Value> {
        let locator = cctp_locator(transfer)?;
        let message = validate_attestation(&saved.quote, locator, attestation)?;
        let destination = saved
            .quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let verified = protocol::verify_solana_receipt(proof, destination, attestation)?;
        let operations = engine.operations()?;
        let known = operations.iter().find(|operation| {
            operation.submission.as_ref().is_some_and(|submission| {
                submission.kind == "solana.transaction"
                    && (submission.reference == proof.signature
                        || submission.data["message_sha256"].as_str()
                            == Some(proof.message_sha256.as_str()))
            })
        });
        let native_cost_accounting = if let Some(operation) = known {
            if operation
                .preview
                .as_ref()
                .is_none_or(|preview| preview.binding["transfer_id"] != transfer.id)
            {
                return Err(GoFinError::validation(
                    "Solana mint submission belongs to a different GoFin transfer.",
                ));
            }
            if operation.status != Status::Completed || operation.effects.is_none() {
                return Ok(
                    json!({"provider":"cctp","settled":false,"status":"mint_cost_reconciliation_pending","operation_id":operation.id,"next_action":"Reconcile the original mint operation so its verified fee and rent are committed before destination settlement."}),
                );
            }
            json!({"kind":"gofin_operation","operation_id":operation.id,"count_once":"Native costs are already in this operation's verified effects."})
        } else {
            json!({"kind":"external_observation","count_once":"Observed external transaction costs; not an additional GoFin plan dispatch or authority consumption."})
        };
        let native_costs = vec![
            sol_cost(&destination.wallet_resource, proof.fee_lamports, "gas"),
            sol_cost(
                &destination.wallet_resource,
                verified.actual_nonce_rent_lamports,
                "cctp_nonce_rent",
            ),
        ];
        let evidence_id = format!(
            "{}:{}:{}",
            gofin_solana::NETWORK,
            proof.signature,
            verified.receive_instruction_index
        );
        let amount = GoFinAmount {
            asset: destination.asset.clone(),
            atoms: message.amount,
        };
        let mut effects = vec![FinancialEffect::UsableCredit {
            to: destination.wallet_resource.clone(),
            amount: amount.clone(),
        }];
        if !verified.fee_executed.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: destination.wallet_resource.clone(),
                },
                amount: GoFinAmount {
                    asset: destination.asset.clone(),
                    atoms: verified.fee_executed.clone(),
                },
                category: "cctp_protocol_fee".into(),
                treatment: CostTreatment::DeductedFromCredit,
            });
        }
        let evidence = json!({"transfer_id":transfer.id,"network":gofin_solana::NETWORK,"transaction_hash":proof.signature,"slot":proof.slot,"canonical":true,"commitment":"finalized","success":true,"provider":"cctp","message_hash":locator.message_hash,"source_evidence_id":locator.source_evidence_id,"correlation":verified.nonce,"recipient":destination.recipient,"recipient_token_account":destination.recipient_token_account,"recipient_resource":destination.wallet_resource,"asset":amount.asset,"amount_atoms":amount.atoms,"net_amount_atoms":verified.net_amount,"fee_executed":verified.fee_executed,"evidence_id":evidence_id,"proof_method":"cctp_v2_solana_message_and_native_usdc_receipt","receipt_scope":"instruction","instruction_index":verified.receive_instruction_index,"native_costs":native_costs,"native_cost_accounting":native_cost_accounting,"proof":proof});
        let mut settlement = gofin_domain::BridgeSettlementProof {
            transfer_id: transfer.id.clone(),
            outcome: gofin_domain::BridgeSettlementOutcome::Destination,
            evidence_id: evidence_id.clone(),
            effects,
            evidence,
            observed_at: self.clock.now().to_rfc3339(),
            proof_digest: String::new(),
        };
        settlement.proof_digest = gofin_domain::bridge_settlement_digest(&settlement)?;
        engine.commit_bridge_external_settlement(settlement)?;
        Ok(
            json!({"provider":"cctp","settled":true,"status":"destination_settled","network":gofin_solana::NETWORK,"evidence_id":evidence_id,"transaction_hash":proof.signature,"net_amount_atoms":verified.net_amount,"native_costs":native_costs,"native_cost_accounting":native_cost_accounting}),
        )
    }

    pub(super) async fn reconcile_cctp_solana_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> gofin_domain::Result<Value> {
        let locator = cctp_locator(transfer)?;
        let destination = saved
            .quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let wallet_alias = destination.wallet.as_ref().ok_or_else(|| {
            GoFinError::validation("CCTP Solana requires its enrolled destination wallet.")
        })?;
        let wallet = self
            .solana_wallets
            .get(wallet_alias)
            .ok_or_else(|| GoFinError::validation("Original Solana observer is unavailable."))?;
        if wallet.address() != destination.wallet_address {
            return Err(GoFinError::validation(
                "Solana destination wallet identity changed.",
            ));
        }
        let runtime = wallet.runtime();
        let operations = engine.operations()?;
        let mut uncertain = false;
        let mut saved_attestations = Vec::new();
        for link in transfer
            .steps
            .iter()
            .filter(|link| link.kind == BridgeStepKind::Mint)
        {
            let Some(operation) = operations
                .iter()
                .find(|operation| Some(&operation.id) == link.operation_id.as_ref())
            else {
                continue;
            };
            let Some(submission) = &operation.submission else {
                continue;
            };
            uncertain |= !operation.status.terminal();
            let preview = operation.preview.as_ref().ok_or_else(internal_error)?;
            let attestation: cctp::CctpAttestation =
                serde_json::from_value(preview.binding["cctp_attestation"].clone())
                    .map_err(|_| internal_error())?;
            validate_attestation(&saved.quote, locator, &attestation)?;
            if let ReceiptOutcome::Finalized {
                proof,
                failed: false,
                ..
            } = runtime
                .reconcile_submission(submission, operation.dispatch_evidence.as_ref())
                .await?
            {
                verify_bound_receipt(operation, &proof)?;
                return self.commit_solana_cctp_receipt(
                    engine,
                    transfer,
                    saved,
                    &attestation,
                    &proof,
                );
            }
            saved_attestations.push(attestation);
        }
        let fresh = self.cctp_attestation(&saved.quote, locator).await;
        let attestation = match fresh {
            Ok(Some(attestation)) => attestation,
            Ok(None) => match saved_attestations.last() {
                Some(attestation) => attestation.clone(),
                None => {
                    return Ok(
                        json!({"provider":"cctp","settled":false,"status":"attestation_pending","next_action":"Wait for Circle attestation of the original burn; do not repeat it."}),
                    );
                }
            },
            Err(error) => match saved_attestations.last() {
                Some(attestation) => attestation.clone(),
                None => return Err(error),
            },
        };
        let message = validate_attestation(&saved.quote, locator, &attestation)?;
        let provider = self.cctp_provider()?;
        // Historical finalized proof does not depend on today's ATA/program
        // readiness. The source-linked nonce is sufficient to discover candidates.
        {
            let nonce_address = protocol::used_nonce_address(&message.nonce)?;
            let rows = runtime
                .rpc()
                .request(
                    "getSignaturesForAddress",
                    json!([nonce_address,{"commitment":"finalized","limit":100}]),
                )
                .await?;
            for row in rows.as_array().ok_or_else(internal_error)? {
                if !row["err"].is_null() {
                    continue;
                }
                let Some(signature) = row["signature"].as_str() else {
                    continue;
                };
                if let ReceiptOutcome::Finalized {
                    proof,
                    failed: false,
                    ..
                } = runtime.observe_finalized_transaction(signature).await?
                {
                    for candidate in std::iter::once(&attestation).chain(saved_attestations.iter())
                    {
                        if protocol::verify_solana_receipt(&proof, destination, candidate).is_ok() {
                            return self.commit_solana_cctp_receipt(
                                engine, transfer, saved, candidate, &proof,
                            );
                        }
                    }
                    // A refreshed attestation can have different fee/expiry bytes.
                    // Its historical nonce must still be linked to this burn by Iris.
                    for instruction in &proof.message_instructions {
                        let Ok((message_bytes, signature_bytes)) =
                            protocol::decode_receive_instruction(instruction)
                        else {
                            continue;
                        };
                        let decoded = cctp::decode_message(&message_bytes)?;
                        let linked = std::iter::once(&attestation)
                            .chain(saved_attestations.iter())
                            .find(|candidate| candidate.nonce == decoded.nonce);
                        let Some(linked) = linked else {
                            continue;
                        };
                        let historical = cctp::CctpAttestation {
                            source_transaction_hash: linked.source_transaction_hash.clone(),
                            message: message_bytes,
                            attestation: signature_bytes,
                            nonce: decoded.nonce,
                            forward_tx_hash: None,
                        };
                        if validate_attestation(&saved.quote, locator, &historical).is_ok()
                            && protocol::verify_solana_receipt(&proof, destination, &historical)
                                .is_ok()
                        {
                            return self.commit_solana_cctp_receipt(
                                engine,
                                transfer,
                                saved,
                                &historical,
                                &proof,
                            );
                        }
                    }
                }
            }
        }
        let state = provider
            .solana_state(
                saved.quote.intent.evm_source()?.chain_id,
                destination,
                Some(&message.nonce),
            )
            .await?;
        if state.used_nonce {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"processed_receipt_pending","locator_coverage":"Original GoFin submissions and latest 100 finalized nonce-account signatures","next_action":"Nonce processing is observed, but exact destination credit is not verified. Keep reconciling the original transfer."}),
            );
        }
        if uncertain {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"mint_submission_uncertain","next_action":"Reconcile the original Solana mint submission; do not create a new attempt."}),
            );
        }
        if !message.expiration_block.is_zero()
            && message.expiration_block <= Quantity::from_u64(state.slot)
        {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"attestation_expired","next_action":"Explicitly request bridge.recovery to refresh this original burn's attestation."}),
            );
        }
        if transfer.steps.iter().any(|step| {
            step.kind == BridgeStepKind::Mint && step.state == BridgeStepState::NeedsAttention
        }) && !transfer.steps.iter().any(|step| {
            step.kind == BridgeStepKind::Mint && step.state == BridgeStepState::Unsubmitted
        }) {
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"mint_recovery_required","next_action":"Use bridge.recovery for a new mint attempt after proven failure; retain the original burn."}),
            );
        }
        let bounds: BridgeBounds = serde_json::from_value(
            transfer
                .approved_bounds
                .clone()
                .ok_or_else(internal_error)?,
        )
        .map_err(|_| internal_error())?;
        if let Err(failure) = self
            .validate_cctp_solana_gas(&saved.quote, &bounds, transfer)
            .await
        {
            if failure.code != "CCTP_GAS_REQUIRED" {
                return Err(failure);
            }
            return Ok(
                json!({"provider":"cctp","settled":false,"status":"destination_gas_required","error":failure,"next_action":"Fund the approved Solana wallet separately, then resume the original mint."}),
            );
        }
        Ok(
            json!({"provider":"cctp","settled":false,"status":"mint_ready","requested_mode":saved.quote.cctp.as_ref().map(|m|m.transfer_mode),"executed_finality":message.finality_threshold_executed,"next_action":"Explicitly execute the saved Solana mint step under current plan authority."}),
        )
    }

    pub(super) async fn refresh_cctp_solana_attestation(
        &self,
        quote: &BridgeQuote,
        locator: &gofin_domain::BridgeDestinationLocator,
    ) -> gofin_domain::Result<bool> {
        let Some(attestation) = self.cctp_attestation(quote, locator).await? else {
            return Ok(false);
        };
        let message = validate_attestation(quote, locator, &attestation)?;
        let destination = quote
            .intent
            .solana_destination
            .as_ref()
            .ok_or_else(internal_error)?;
        let provider = self.cctp_provider()?;
        let state = provider
            .solana_state(
                quote.intent.evm_source()?.chain_id,
                destination,
                Some(&message.nonce),
            )
            .await?;
        if state.used_nonce {
            return Err(error(
                "CCTP_ALREADY_PROCESSED",
                "recovery",
                "The Solana CCTP nonce is already processed.",
                "Use bridge.status to verify its exact receipt.",
            ));
        }
        if !message.expiration_block.is_zero()
            && message.expiration_block <= Quantity::from_u64(state.slot)
        {
            protocol::verify_solana_attestation(
                &attestation.message,
                &attestation.attestation,
                &state,
            )?;
            provider.reattest(&message.nonce).await?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (FinalizedReceiptProof, gofin_bridge::SolanaDestination) {
        let owner = "11111111111111111111111111111111";
        let destination = gofin_bridge::SolanaDestination {
            network: gofin_solana::NETWORK.into(),
            wallet: Some("SYNTHETIC-wallet".into()),
            wallet_resource: gofin_domain::ResourceId(format!("{}:{owner}", gofin_solana::NETWORK)),
            wallet_address: owner.into(),
            recipient: owner.into(),
            asset: protocol::asset(),
            recipient_token_account: gofin_solana::associated_token_address(
                owner,
                protocol::USDC_MINT,
                gofin_solana::token::TOKEN_PROGRAM_ID,
            )
            .unwrap(),
            destination_caller: owner.into(),
        };
        let token = json!({"accountIndex":1,"mint":protocol::USDC_MINT,"owner":owner,"programId":gofin_solana::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":"17","decimals":6}});
        let proof = FinalizedReceiptProof {
            signature: "SYNTHETIC-signature".into(),
            slot: 123,
            fee_lamports: 5_000,
            failed: true,
            message_sha256: "SYNTHETIC-hash".into(),
            account_keys: vec![owner.into(), destination.recipient_token_account.clone()],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![1_000_000, 2_039_280],
            post_balances_lamports: vec![995_000, 2_039_280],
            pre_token_balances: vec![token.clone()],
            post_token_balances: vec![token],
            loaded_addresses: json!({}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: Some(json!({"InstructionError":[0,"SYNTHETIC"]})),
        };
        (proof, destination)
    }
    #[test]
    fn reverted_mint_requires_fee_only_lamport_debit_and_atomic_token_rollback() {
        let (proof, destination) = fixture();
        verify_failed_receipt(&proof, &destination).unwrap();
        let mut extra = proof.clone();
        extra.post_balances_lamports[0] -= 1;
        assert!(verify_failed_receipt(&extra, &destination).is_err());
        let mut token = proof.clone();
        token.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("18");
        assert!(verify_failed_receipt(&token, &destination).is_err());
        let mut success = proof;
        success.failed = false;
        assert!(verify_failed_receipt(&success, &destination).is_err());
    }
}
