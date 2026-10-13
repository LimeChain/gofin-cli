//! Bounded Jupiter Perps limit request creation.
//!
//! Creation is an escrow deposit. It stays pending until a keeper fill or a
//! separately approved cancellation proves the terminal request effects.

use super::*;

pub(super) const EXECUTE_ACTION: &str = "create-limit-order";

pub(super) async fn reconcile(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable Perps limit creation submission exists."}),
            verified: None,
        });
    };
    let preview = record.preview.as_ref().ok_or_else(bad_response)?;
    let binding = &preview.binding;
    let wallet = adapter.wallet(&record.input)?;
    match wallet
        .runtime()
        .reconcile_submission(submission, record.dispatch_evidence.as_ref())
        .await?
    {
        ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"confirmed":confirmed,"reason":reason,"request":binding["request"]}),
            verified: None,
        }),
        ReceiptOutcome::NotDispatched {
            reason_code,
            reason,
        } => {
            let verified =
                crate::adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
            Ok(Reconciliation {
                status: Status::Failed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
        ReceiptOutcome::Finalized {
            signature,
            slot,
            fee_lamports,
            failed,
            message_sha256,
            proof,
        } => {
            if failed {
                let verified = crate::adapter::verify_failed_cost(
                    record,
                    submission,
                    &signature,
                    slot,
                    fee_lamports,
                    &proof,
                )?;
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                });
            }
            verify_placement_proof(binding, &proof, &message_sha256, fee_lamports)?;
            let request_address = binding["request"].as_str().ok_or_else(bad_response)?;
            let request_info = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([request_address,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let request_value = request_info.get("value").ok_or_else(bad_response)?;
            if request_value.is_null() {
                return reconcile_limit_keeper_history(
                    adapter,
                    record,
                    submission,
                    &wallet,
                    &signature,
                    slot,
                    &message_sha256,
                    &proof,
                    fee_lamports,
                    request_address,
                )
                .await;
            }
            if request_value.get("owner").and_then(Value::as_str)
                != Some(crate::jlp::PERPS_PROGRAM_ID)
            {
                return Err(receipt_mismatch());
            }
            let request_data = fin_solana::token::decode_rpc_account_data(
                request_value.get("data").ok_or_else(bad_response)?,
            )?;
            let request =
                decode_position_request(wallet.address(), request_address, &request_data)?;
            if request.executed {
                return reconcile_limit_keeper_history(
                    adapter,
                    record,
                    submission,
                    &wallet,
                    &signature,
                    slot,
                    &message_sha256,
                    &proof,
                    fee_lamports,
                    request_address,
                )
                .await;
            }
            let Some(effective_trigger) = effective_request_trigger(
                adapter,
                record,
                &wallet,
                None,
                OP_LIMIT_UPDATE,
                "trigger_price_usd_atoms",
            )
            .await?
            else {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({
                        "request":request_address,
                        "reason":"A linked limit amendment has not reached a verified terminal state."
                    }),
                    verified: None,
                });
            };
            verify_open_request(binding, &request, effective_trigger)?;
            let terminal_or_pending = reconcile_limit_keeper_history(
                adapter,
                record,
                submission,
                &wallet,
                &signature,
                slot,
                &message_sha256,
                &proof,
                fee_lamports,
                request_address,
            )
            .await?;
            if terminal_or_pending.status != Status::Pending
                || terminal_or_pending.verified.is_some()
            {
                return Ok(terminal_or_pending);
            }
            let input_asset = asset(
                binding["funding_mint"].as_str().ok_or_else(bad_response)?,
                binding["funding_decimals"]
                    .as_u64()
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or_else(bad_response)?,
            );
            let request_token = binding["request_token_account"]
                .as_str()
                .ok_or_else(bad_response)?;
            let wallet_token = binding["wallet_token_account"]
                .as_str()
                .ok_or_else(bad_response)?;
            let token_program = binding["token_program"].as_str().ok_or_else(bad_response)?;
            let collateral = parse_binding_u64(binding, "collateral_atoms")?;
            let decimals = input_asset.decimals;
            let deposited = proof.token_account_delta_verified(
                request_token,
                request_address,
                binding["funding_mint"].as_str().ok_or_else(bad_response)?,
                token_program,
                decimals,
            )?;
            let debited = proof.token_account_delta_verified(
                wallet_token,
                wallet.address(),
                binding["funding_mint"].as_str().ok_or_else(bad_response)?,
                token_program,
                decimals,
            )?;
            if deposited.is_negative()
                || deposited.is_zero()
                || deposited.magnitude().as_str() != collateral.to_string()
                || !debited.is_negative()
                || debited.magnitude().as_str() != collateral.to_string()
            {
                return Err(receipt_mismatch());
            }
            let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
            let position_rent = parse_binding_u64(binding, "position_rent_lamports")?;
            if proof.lamport_delta(request_address)?.as_str() != request_rent.to_string()
                || proof
                    .lamport_delta(binding["position"].as_str().ok_or_else(bad_response)?)?
                    .as_str()
                    != position_rent.to_string()
                || fee_lamports > parse_binding_u64(binding, "maximum_network_fee_lamports")?
            {
                return Err(receipt_mismatch());
            }
            let wallet_resource = resource_for_wallet(wallet.address());
            let escrow_resource = ResourceId(format!(
                "solana:{CLUSTER}:jupiter-perps-request:{request_address}"
            ));
            let position_resource = wallet_resource.clone();
            let collateral_position = format!(
                "{}:collateral",
                binding["position"].as_str().ok_or_else(bad_response)?
            );
            let margin_asset = margin_usd_asset();
            let mut effects = vec![limit_escrow_progress(
                wallet_resource.clone(),
                escrow_resource,
                input_asset.clone(),
                collateral,
                collateral,
                0,
                0,
                collateral,
                position_resource.clone(),
                collateral_position,
                fin_domain::PositionKind::LockedCollateral,
                margin_asset,
                0,
                fin_domain::EscrowLifecycleState::Open,
                None,
                0,
                None,
            )];
            if fee_lamports > 0 {
                effects.push(fin_domain::FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: wallet_resource.clone(),
                    },
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: fin_domain::Quantity::from_u64(fee_lamports),
                    },
                    category: "solana_network_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            for spec in account_funding_specs(binding, wallet.address())? {
                let delta = proof.lamport_delta(&spec.account)?;
                if delta.is_negative() || delta.magnitude() != spec.amount {
                    return Err(receipt_mismatch());
                }
                let position_amount = Amount {
                    asset: asset("native", 9),
                    atoms: spec.amount.clone(),
                };
                let anchor = transaction_position_anchor(
                    wallet_resource.clone(),
                    spec.position.clone(),
                    fin_domain::PositionKind::RefundableAccountFunding,
                    position_amount,
                    &signature,
                    slot,
                    &message_sha256,
                    json!({"source":"finalized_jupiter_perps_limit_account_funding","account":spec.account}),
                );
                effects.push(fin_domain::FinancialEffect::AccountFundingProgress {
                    resource: wallet_resource.clone(),
                    position: spec.position,
                    account: spec.account,
                    asset: asset("native", 9),
                    funded: spec.amount.clone(),
                    returned: fin_domain::Quantity::zero(),
                    remaining: spec.amount,
                    state: fin_domain::AccountFundingState::Open,
                    anchor: Some(anchor),
                });
            }
            let evidence = json!({"cluster":CLUSTER,"request":request_address,"position":binding["position"],"signature":signature,"slot":slot,"message_sha256":message_sha256,"request_state":"finalized_open_limit","request_account":request.read_value(),"proof":proof});
            let placement_digest = fin_domain::binding_hash(&json!({
                "signature":signature,
                "slot":slot,
                "message_sha256":message_sha256,
                "proof":proof,
            }))?;
            let settlement_progress = progress_metadata(
                record,
                request_address,
                [(format!("placement:{signature}"), placement_digest)],
            )?;
            let mut evidence = evidence;
            evidence["settlement_progress"] = settlement_progress;
            let mut verified = fin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence,
                observed_at: Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
            Ok(Reconciliation {
                status: Status::Pending,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
    }
}

fn verify_placement_proof(
    binding: &Value,
    proof: &FinalizedReceiptProof,
    message_hash: &str,
    fee: u64,
) -> Result<()> {
    if proof.failed
        || proof.message_sha256 != message_hash
        || binding["message_sha256"].as_str() != Some(message_hash)
        || fee > parse_binding_u64(binding, "maximum_network_fee_lamports")?
    {
        return Err(receipt_mismatch());
    }
    let ix = proof
        .message_instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if ix.len() != 1 {
        return Err(receipt_mismatch());
    }
    let instruction = &ix[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| receipt_mismatch())?;
    let (size, collateral, side, trigger, above, counter, _) = decode_limit_create_params(&data)?;
    if size != parse_binding_u64(binding, "size_usd_atoms")?
        || collateral != parse_binding_u64(binding, "collateral_atoms")?
        || side != binding["side"].as_u64().ok_or_else(bad_response)? as u8
        || trigger != parse_binding_u64(binding, "trigger_price_usd_atoms")?
        || (above == 1)
            != binding["trigger_above_threshold"]
                .as_bool()
                .ok_or_else(bad_response)?
        || counter != parse_binding_u64(binding, "counter")?
        || instruction
            .accounts
            .get(2)
            .is_none_or(|a| a.address != binding["wallet_address"])
        || instruction
            .accounts
            .get(6)
            .is_none_or(|a| a.address != binding["position"])
        || instruction
            .accounts
            .get(7)
            .is_none_or(|a| a.address != binding["request"])
        || instruction
            .accounts
            .get(3)
            .is_none_or(|a| a.address != binding["wallet_token_account"])
        || instruction
            .accounts
            .get(8)
            .is_none_or(|a| a.address != binding["request_token_account"])
    {
        return Err(receipt_mismatch());
    }
    Ok(())
}

fn verify_placement_funding_proof(
    binding: &Value,
    wallet: &SolanaWalletAdapter,
    proof: &FinalizedReceiptProof,
) -> Result<()> {
    let request = binding["request"].as_str().ok_or_else(bad_response)?;
    let position = binding["position"].as_str().ok_or_else(bad_response)?;
    let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
    let position_rent = parse_binding_u64(binding, "position_rent_lamports")?;
    let request_rent_delta = proof
        .lamport_delta(request)
        .map_err(|_| receipt_mismatch_at("original request account rent observation"))?;
    if request_rent_delta.as_str() != request_rent.to_string() {
        return Err(receipt_mismatch_at("original request account rent funding"));
    }
    let position_rent_delta = proof
        .lamport_delta(position)
        .map_err(|_| receipt_mismatch_at("original position account rent observation"))?;
    if position_rent_delta.as_str() != position_rent.to_string() {
        return Err(
            receipt_mismatch_at("original position account rent funding").with_details(json!({
                "expected_lamports": position_rent.to_string(),
                "observed_delta_lamports": position_rent_delta.as_str(),
            })),
        );
    }
    let mint = binding["funding_mint"].as_str().ok_or_else(bad_response)?;
    let token_program = binding["token_program"].as_str().ok_or_else(bad_response)?;
    let decimals = binding["funding_decimals"]
        .as_u64()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(bad_response)?;
    let collateral =
        fin_domain::Quantity::from_u64(parse_binding_u64(binding, "collateral_atoms")?);
    let request_delta = proof
        .token_account_delta_verified(
            binding["request_token_account"]
                .as_str()
                .ok_or_else(bad_response)?,
            request,
            mint,
            token_program,
            decimals,
        )
        .map_err(|_| receipt_mismatch_at("original escrow-token funding observation"))?;
    let wallet_delta = proof
        .token_account_delta_verified(
            binding["wallet_token_account"]
                .as_str()
                .ok_or_else(bad_response)?,
            wallet.address(),
            mint,
            token_program,
            decimals,
        )
        .map_err(|_| receipt_mismatch_at("original wallet-token funding observation"))?;
    if request_delta.is_negative()
        || request_delta.is_zero()
        || request_delta.magnitude() != collateral
        || !wallet_delta.is_negative()
        || wallet_delta.magnitude() != collateral
    {
        return Err(receipt_mismatch_at("original wallet/escrow token funding"));
    }
    for spec in account_funding_specs(binding, wallet.address())? {
        let delta = proof
            .lamport_delta(&spec.account)
            .map_err(|_| receipt_mismatch_at("original refundable account rent observation"))?;
        let rent_delta = if binding["request_token_account"].as_str() == Some(spec.account.as_str())
            && mint == crate::swap::WRAPPED_SOL_MINT
        {
            if delta.is_negative() {
                return Err(receipt_mismatch_at("original wrapped-native ATA funding"));
            }
            delta
                .magnitude()
                .checked_sub(&collateral)
                .map_err(|_| receipt_mismatch_at("original wrapped-native ATA rent funding"))?
        } else {
            if delta.is_negative() {
                return Err(receipt_mismatch_at("original refundable account funding"));
            }
            delta.magnitude()
        };
        if rent_delta != spec.amount {
            return Err(receipt_mismatch_at(
                "original refundable account funding amount",
            ));
        }
    }
    Ok(())
}

struct LimitFundingAccount {
    position: String,
    account: String,
    amount: fin_domain::Quantity,
}

fn account_funding_specs(binding: &Value, wallet: &str) -> Result<Vec<LimitFundingAccount>> {
    let request = binding["request"].as_str().ok_or_else(bad_response)?;
    let position = binding["position"].as_str().ok_or_else(bad_response)?;
    let mut specs = Vec::new();
    for (position_id, account, raw_amount) in [
        (
            format!("account-funding:{request}"),
            request.to_owned(),
            parse_binding_u64(binding, "request_rent_lamports")?,
        ),
        (
            format!("{position}:rent"),
            position.to_owned(),
            parse_binding_u64(binding, "position_rent_lamports")?,
        ),
    ] {
        if raw_amount > 0 {
            specs.push(LimitFundingAccount {
                position: position_id,
                account,
                amount: fin_domain::Quantity::from_u64(raw_amount),
            });
        }
    }
    let closed = binding["closed_wrapped_native_accounts"]
        .as_array()
        .ok_or_else(bad_response)?;
    for funding in binding["funding_accounts"]
        .as_array()
        .ok_or_else(bad_response)?
    {
        let account = funding["address"].as_str().ok_or_else(bad_response)?;
        let payer = funding["payer"].as_str().ok_or_else(bad_response)?;
        let raw_amount = funding["lamports"].as_u64().ok_or_else(bad_response)?;
        if payer == wallet
            && raw_amount > 0
            && !closed.iter().any(|item| item.as_str() == Some(account))
        {
            specs.push(LimitFundingAccount {
                position: format!("account-funding:{account}"),
                account: account.to_owned(),
                amount: fin_domain::Quantity::from_u64(raw_amount),
            });
        }
    }
    Ok(specs)
}

#[allow(clippy::too_many_arguments)]
fn terminal_account_funding_progress(
    record: &OperationRecord,
    binding: &Value,
    wallet: &str,
    request: &str,
    request_signature: &str,
    request_slot: u64,
    request_hash: &str,
    keeper_signature: &str,
    keeper_slot: u64,
    keeper_hash: &str,
    finalized_proof: Option<&FinalizedReceiptProof>,
) -> Result<Vec<fin_domain::FinancialEffect>> {
    let resource = resource_for_wallet(wallet);
    let native = asset("native", 9);
    let previous = record.effects.as_ref();
    let mut effects = Vec::new();
    for spec in account_funding_specs(binding, wallet)? {
        let existing = previous.and_then(|previous| {
            previous.effects.iter().find(|effect| {
                if let fin_domain::FinancialEffect::AccountFundingProgress {
                    resource: actual_resource,
                    position,
                    account,
                    asset: actual_asset,
                    ..
                } = effect
                {
                    actual_resource == &resource
                        && position == &spec.position
                        && account == &spec.account
                        && actual_asset == &native
                } else {
                    false
                }
            })
        });
        if spec.account != request
            && let Some(existing) = existing
        {
            if account_funding_closed_in_proof(finalized_proof, binding, &spec)? {
                // A terminal keeper/cancel may close the request-owned ATA.
                // Preserve its original funded amount, and credit only the
                // exact rent proven returned by this finalized transaction.
            } else {
                effects.push(existing.clone());
                continue;
            }
        }
        let closed = spec.account == request
            || account_funding_closed_in_proof(finalized_proof, binding, &spec)?;
        let (returned, remaining, state, anchor) = if closed {
            (
                spec.amount.clone(),
                fin_domain::Quantity::zero(),
                fin_domain::AccountFundingState::Closed,
                Some(transaction_position_anchor(
                    resource.clone(),
                    spec.position.clone(),
                    fin_domain::PositionKind::RefundableAccountFunding,
                    Amount {
                        asset: native.clone(),
                        atoms: fin_domain::Quantity::zero(),
                    },
                    keeper_signature,
                    keeper_slot,
                    keeper_hash,
                    json!({"source":"finalized_jupiter_perps_account_rent_refund","request":request,"account":spec.account}),
                )),
            )
        } else {
            (
                fin_domain::Quantity::zero(),
                spec.amount.clone(),
                fin_domain::AccountFundingState::Open,
                Some(transaction_position_anchor(
                    resource.clone(),
                    spec.position.clone(),
                    fin_domain::PositionKind::RefundableAccountFunding,
                    Amount {
                        asset: native.clone(),
                        atoms: spec.amount.clone(),
                    },
                    request_signature,
                    request_slot,
                    request_hash,
                    json!({"source":"historical_limit_placement_rent_anchor","account":spec.account}),
                )),
            )
        };
        effects.push(fin_domain::FinancialEffect::AccountFundingProgress {
            resource: resource.clone(),
            position: spec.position,
            account: spec.account,
            asset: native.clone(),
            funded: spec.amount,
            returned,
            remaining,
            state,
            anchor,
        });
    }
    Ok(effects)
}

fn account_funding_closed_in_proof(
    proof: Option<&FinalizedReceiptProof>,
    binding: &Value,
    spec: &LimitFundingAccount,
) -> Result<bool> {
    let Some(proof) = proof else {
        return Ok(false);
    };
    // The keeper/cancel path is only expected to close the request-owned
    // escrow ATA. Other auxiliary accounts remain open unless a separate
    // protocol instruction and exact return contract is added.
    if binding["request_token_account"].as_str() != Some(spec.account.as_str()) {
        return Ok(false);
    }
    if !proof.account_keys.iter().any(|key| key == &spec.account) {
        return Ok(false);
    }
    let delta = proof.lamport_delta(&spec.account)?;
    if delta.is_zero() || !delta.is_negative() {
        return Ok(false);
    }
    let returned = {
        let request = binding["request"].as_str().ok_or_else(bad_response)?;
        let mint = binding["funding_mint"].as_str().ok_or_else(bad_response)?;
        let program = binding["token_program"].as_str().ok_or_else(bad_response)?;
        let decimals = binding["funding_decimals"]
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(bad_response)?;
        let token_delta = proof
            .token_account_delta_verified(&spec.account, request, mint, program, decimals)
            .map_err(|_| receipt_mismatch_at("request-token account refund identity"))?;
        if !token_delta.is_negative()
            || token_delta.magnitude()
                != fin_domain::Quantity::from_u64(parse_binding_u64(binding, "collateral_atoms")?)
        {
            return Err(receipt_mismatch_at("request-token escrow refund amount"));
        }
        if mint == crate::swap::WRAPPED_SOL_MINT {
            delta
                .magnitude()
                .checked_sub(&token_delta.magnitude())
                .map_err(|_| receipt_mismatch_at("wrapped-native ATA rent refund"))?
        } else {
            delta.magnitude()
        }
    };
    if returned == spec.amount {
        Ok(true)
    } else {
        Err(receipt_mismatch_at("request-token ATA rent refund amount"))
    }
}

fn verify_open_request(
    binding: &Value,
    request: &VerifiedRequest,
    effective_trigger: u64,
) -> Result<()> {
    if request.position != binding["position"].as_str().ok_or_else(bad_response)?
        || request.mint != binding["funding_mint"].as_str().ok_or_else(bad_response)?
        || request.size_usd_delta != parse_binding_u64(binding, "size_usd_atoms")?
        || request.collateral_delta != parse_binding_u64(binding, "collateral_atoms")?
        || request.request_change != 1
        || request.request_type != 1
        || request.executed
        || request.side != binding["side"].as_u64().ok_or_else(bad_response)? as u8
        || request.trigger_price != Some(effective_trigger)
        || request.trigger_above != binding["trigger_above_threshold"].as_bool()
        || request.counter != parse_binding_u64(binding, "counter")?
    {
        return Err(receipt_mismatch());
    }
    Ok(())
}

/// Resolve the request's trigger from its durable, finalized amendment chain.
/// Store ordering is intentionally ignored; finalized transaction order is
/// derived from slot and, when tied, the canonical block signature sequence.
pub(super) async fn effective_request_trigger(
    adapter: &PerpsAdapter,
    parent: &OperationRecord,
    wallet: &SolanaWalletAdapter,
    before: Option<(u64, &str)>,
    amendment_operation: &str,
    original_trigger_field: &str,
) -> Result<Option<u64>> {
    let parent_binding = &parent.preview.as_ref().ok_or_else(bad_response)?.binding;
    let original_trigger = parse_binding_u64(parent_binding, original_trigger_field)?;
    let request = parent_binding["request"]
        .as_str()
        .ok_or_else(bad_response)?;
    let wallet_address = parent_binding["wallet_address"]
        .as_str()
        .ok_or_else(bad_response)?;
    let expected_parent_submission =
        serde_json::to_value(parent.submission.as_ref().ok_or_else(bad_response)?)
            .map_err(|_| bad_response())?;
    let mut amendments = Vec::new();
    for child in adapter.parent_amendments(&parent.id, amendment_operation)? {
        if child.operation != amendment_operation {
            continue;
        }
        let Some(preview) = child.preview.as_ref() else {
            return Ok(None);
        };
        let binding = &preview.binding;
        if binding["parent_operation_id"].as_str() != Some(parent.id.as_str())
            || binding["request"].as_str() != Some(request)
            || binding["wallet_address"].as_str() != Some(wallet_address)
            || binding["parent_submission"] != expected_parent_submission
            || binding["__fin_amendment"]["version"].as_u64() != Some(1)
            || binding["__fin_amendment"]["parent_operation_id"].as_str()
                != Some(parent.id.as_str())
            || binding["__fin_amendment"]["request_id"].as_str() != Some(request)
            || binding["__fin_amendment"]["parent_submission"] != expected_parent_submission
        {
            return Err(receipt_mismatch_at(
                "a linked limit amendment does not match the original request identity",
            ));
        }
        if child.status != Status::Completed {
            if child.status.terminal() {
                continue;
            }
            return Ok(None);
        }
        let Some(submission) = child.submission.as_ref() else {
            return Err(receipt_mismatch_at(
                "a completed limit amendment has no durable submission",
            ));
        };
        let Some(verified) = child.effects.as_ref() else {
            return Err(receipt_mismatch_at(
                "a completed limit amendment has no finalized proof",
            ));
        };
        if &verified.submission != submission {
            return Err(receipt_mismatch_at(
                "a completed limit amendment proof is bound to a different submission",
            ));
        }
        let evidence = &verified.evidence;
        let slot = evidence["slot"].as_u64().ok_or_else(receipt_mismatch)?;
        let trigger = parse_binding_u64(binding, "trigger_price_usd_atoms")?;
        let previous = parse_binding_u64(binding, "previous_trigger_price_usd_atoms")?;
        let signature = evidence["signature"]
            .as_str()
            .ok_or_else(receipt_mismatch)?
            .to_owned();
        let submission_signature = submission.data["chain_signature"]
            .as_str()
            .or_else(|| submission.data["wallet_signature"].as_str());
        let proof: FinalizedReceiptProof =
            serde_json::from_value(evidence["proof"].clone()).map_err(|_| receipt_mismatch())?;
        if proof.failed
            || proof.slot != slot
            || proof.signature != signature
            || submission_signature != Some(signature.as_str())
            || proof.message_sha256 != evidence["message_sha256"].as_str().unwrap_or_default()
            || submission
                .reference
                .strip_prefix("solana:message:")
                .is_some_and(|hash| hash != proof.message_sha256)
        {
            return Err(receipt_mismatch_at(
                "a linked limit amendment has inconsistent finalized receipt identity",
            ));
        }
        amendments.push((slot, signature, previous, trigger));
    }

    let mut by_slot = BTreeMap::<u64, Vec<String>>::new();
    for (slot, signature, _, _) in &amendments {
        by_slot.entry(*slot).or_default().push(signature.clone());
    }
    if let Some((slot, signature)) = before {
        by_slot.entry(slot).or_default().push(signature.to_owned());
    }
    let mut transaction_order = BTreeMap::<(u64, String), usize>::new();
    for (slot, signatures) in by_slot {
        let mut unique = signatures.clone();
        unique.sort();
        unique.dedup();
        if unique.len() == 1 {
            transaction_order.insert((slot, unique[0].clone()), 0);
            continue;
        }
        let response = match wallet
            .runtime()
            .rpc()
            .request(
                "getBlock",
                json!([slot,{
                    "commitment":"finalized",
                    "transactionDetails":"signatures",
                    "rewards":false,
                    "maxSupportedTransactionVersion":0
                }]),
            )
            .await
        {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        // Solana's `transactionDetails: "signatures"` response is the
        // ordered top-level `signatures: [string, ...]` array. The `transactions`
        // field is returned only for `transactionDetails: "full"`.
        let Some(signatures_in_block) = response.get("signatures").and_then(Value::as_array) else {
            return Ok(None);
        };
        let Some(indices) = finalized_signature_indices(signatures_in_block) else {
            return Ok(None);
        };
        for signature in unique {
            let Some(index) = indices.get(&signature).copied() else {
                return Ok(None);
            };
            transaction_order.insert((slot, signature), index);
        }
    }
    amendments.sort_by_key(|(slot, signature, _, _)| {
        (
            *slot,
            transaction_order
                .get(&(*slot, signature.clone()))
                .copied()
                .unwrap_or(usize::MAX),
        )
    });
    let mut current = original_trigger;
    let mut prior_order = None;
    for (slot, signature, previous, trigger) in amendments {
        let order = transaction_order
            .get(&(slot, signature))
            .copied()
            .ok_or_else(receipt_mismatch)?;
        if prior_order.is_some_and(|prior| prior >= (slot, order))
            || previous != current
            || trigger == 0
            || trigger == current
        {
            return Err(receipt_mismatch_at(
                "the finalized limit amendment chain is conflicting or discontinuous",
            ));
        }
        current = trigger;
        prior_order = Some((slot, order));
    }
    if let Some((fill_slot, fill_signature)) = before {
        let Some(fill_order) = transaction_order.get(&(fill_slot, fill_signature.to_owned()))
        else {
            return Ok(None);
        };
        if prior_order.is_some_and(|prior| prior >= (fill_slot, *fill_order)) {
            return Err(receipt_mismatch_at(
                "a limit amendment finalized after its keeper fill",
            ));
        }
    }
    Ok(Some(current))
}

fn finalized_signature_indices(signatures_in_block: &[Value]) -> Option<BTreeMap<String, usize>> {
    let mut indices = BTreeMap::new();
    for (index, value) in signatures_in_block.iter().enumerate() {
        let signature = value.as_str()?;
        if indices.insert(signature.to_owned(), index).is_some() {
            return None;
        }
    }
    Some(indices)
}

#[allow(clippy::too_many_arguments)]
fn limit_escrow_progress(
    wallet_resource: ResourceId,
    escrow_resource: ResourceId,
    input_asset: AssetId,
    wallet_debit: u64,
    deposit: u64,
    filled: u64,
    refunded: u64,
    remaining: u64,
    position_resource: ResourceId,
    position: String,
    position_kind: fin_domain::PositionKind,
    position_asset: AssetId,
    position_change: u64,
    state: fin_domain::EscrowLifecycleState,
    position_anchor: Option<fin_domain::PositionAnchor>,
    conditional_change: u64,
    conditional_anchor: Option<fin_domain::PositionAnchor>,
) -> fin_domain::FinancialEffect {
    fin_domain::FinancialEffect::EscrowPositionProgress {
        wallet_resource,
        escrow_resource,
        input_asset,
        wallet_debit: fin_domain::Quantity::from_u64(wallet_debit),
        escrow_deposit: fin_domain::Quantity::from_u64(deposit),
        input_transfer_fee: fin_domain::Quantity::zero(),
        input_filled: fin_domain::Quantity::from_u64(filled),
        input_refunded: fin_domain::Quantity::from_u64(refunded),
        remaining_input: fin_domain::Quantity::from_u64(remaining),
        position_resource,
        position,
        position_kind,
        position_asset,
        position_change: fin_domain::Quantity::from_u64(position_change),
        state,
        position_anchor,
        conditional_position_change: fin_domain::Quantity::from_u64(conditional_change),
        conditional_position_anchor: conditional_anchor,
    }
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_limit_keeper_history(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
    submission: &Submission,
    wallet: &SolanaWalletAdapter,
    request_signature: &str,
    request_slot: u64,
    request_message_hash: &str,
    request_proof: &FinalizedReceiptProof,
    request_fee: u64,
    request: &str,
) -> Result<Reconciliation> {
    if request_proof.message_sha256 != request_message_hash {
        return Err(receipt_mismatch());
    }
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    verify_placement_funding_proof(binding, wallet, request_proof)?;
    let history = wallet
        .runtime()
        .rpc()
        .request(
            "getSignaturesForAddress",
            json!([request,{"commitment":"finalized","limit":100}]),
        )
        .await?;
    let entries = history
        .as_array()
        .filter(|items| items.len() <= 100)
        .ok_or_else(bad_response)?;
    for entry in entries {
        let signature = entry
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let slot = entry
            .get("slot")
            .and_then(Value::as_u64)
            .ok_or_else(bad_response)?;
        if signature == request_signature || slot < request_slot {
            continue;
        }
        let (keeper_fee, keeper_hash, keeper_proof) = match wallet
            .runtime()
            .observe_finalized_transaction(signature)
            .await?
        {
            ReceiptOutcome::Finalized {
                failed: false,
                fee_lamports,
                message_sha256,
                proof,
                ..
            } => (fee_lamports, message_sha256, *proof),
            _ => continue,
        };
        let position = binding["position"].as_str().ok_or_else(bad_response)?;
        if keeper_proof.error.is_some()
            || !keeper_instruction_matches(&keeper_proof, request, position, true)?
        {
            continue;
        }
        let events = keeper_events(&keeper_proof.log_messages)?;
        let side = binding["side"].as_u64().ok_or_else(bad_response)? as u8;
        let size = parse_binding_u64(binding, "size_usd_atoms")?;
        let collateral = parse_binding_u64(binding, "collateral_atoms")?;
        let matched = events
            .iter()
            .filter(|event| {
                keeper_event_matches(
                    event,
                    request,
                    position,
                    wallet.address(),
                    side,
                    true,
                    1,
                    size,
                    collateral,
                )
            })
            .collect::<Vec<_>>();
        if matched.len() != 1 {
            continue;
        }
        if effective_request_trigger(
            adapter,
            record,
            wallet,
            Some((slot, signature)),
            OP_LIMIT_UPDATE,
            "trigger_price_usd_atoms",
        )
        .await?
        .is_none()
        {
            continue;
        }
        let KeeperEvent::Increase {
            position: event_position,
            collateral_delta_usd,
            collateral_delta_token,
            position_size_usd,
            fee_token,
            fee_usd,
            price,
            ..
        } = matched[0]
        else {
            continue;
        };
        let pre_size = parse_binding_u64(binding, "pre_size_usd_atoms")?;
        let pre_collateral = parse_binding_u64(binding, "pre_collateral_usd_atoms")?;
        let expected_size = pre_size.checked_add(size).ok_or_else(receipt_mismatch)?;
        let expected_collateral = pre_collateral
            .checked_add(*collateral_delta_usd)
            .ok_or_else(receipt_mismatch)?;
        let maximum_network = parse_binding_u64(binding, "maximum_network_fee_lamports")?;
        let wallet_pays_keeper = keeper_proof
            .account_keys
            .first()
            .is_some_and(|payer| payer == wallet.address());
        let wallet_network_fee = request_fee
            .checked_add(if wallet_pays_keeper { keeper_fee } else { 0 })
            .ok_or_else(receipt_mismatch)?;
        if *event_position != position
            || *position_size_usd != expected_size
            || *collateral_delta_usd == 0
            || *collateral_delta_usd > size
            || *collateral_delta_token == 0
            || *price == 0
            || wallet_network_fee > maximum_network
        {
            return Err(receipt_mismatch());
        }
        let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
        let request_rent_refund = keeper_proof.lamport_delta(request)?;
        if !request_rent_refund.is_negative()
            || request_rent_refund.magnitude() != fin_domain::Quantity::from_u64(request_rent)
        {
            return Err(receipt_mismatch());
        }
        let request_token = binding["request_token_account"]
            .as_str()
            .ok_or_else(bad_response)?;
        let funding_mint = binding["funding_mint"].as_str().ok_or_else(bad_response)?;
        let token_program = binding["token_program"].as_str().ok_or_else(bad_response)?;
        let decimals = binding["funding_decimals"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok())
            .ok_or_else(bad_response)?;
        let token_out = keeper_proof.token_account_delta_verified(
            request_token,
            request,
            funding_mint,
            token_program,
            decimals,
        )?;
        if !token_out.is_negative()
            || token_out.magnitude() != fin_domain::Quantity::from_u64(collateral)
        {
            return Err(receipt_mismatch());
        }
        // The event plus the immutable pre-request baseline are the historical
        // position anchor. A later live read can corroborate it, but cannot
        // rewrite the state produced by this keeper transaction.
        let resource = resource_for_wallet(wallet.address());
        let margin = margin_usd_asset();
        let collateral_name = format!("{position}:collateral");
        let collateral_anchor = transaction_position_anchor(
            resource.clone(),
            collateral_name.clone(),
            fin_domain::PositionKind::LockedCollateral,
            Amount {
                asset: margin.clone(),
                atoms: fin_domain::Quantity::from_u64(expected_collateral),
            },
            signature,
            slot,
            &keeper_hash,
            json!({"event":matched[0],"pre_collateral_usd_atoms":pre_collateral}),
        );
        let derivative_kind = if side == 1 {
            fin_domain::PositionKind::DerivativeLong
        } else {
            fin_domain::PositionKind::DerivativeShort
        };
        let derivative_asset = crate::pricing::position_usd_asset(
            binding["market_mint"].as_str().ok_or_else(bad_response)?,
        )?;
        let derivative_anchor = transaction_position_anchor(
            resource.clone(),
            position.to_owned(),
            derivative_kind.clone(),
            Amount {
                asset: derivative_asset.clone(),
                atoms: fin_domain::Quantity::from_u64(expected_size),
            },
            signature,
            slot,
            &keeper_hash,
            json!({"event":matched[0],"pre_size_usd_atoms":pre_size}),
        );
        let input_asset = asset(funding_mint, decimals);
        let collateral_change = fin_domain::Quantity::from_u64(*collateral_delta_usd);
        let escrow_resource =
            ResourceId(format!("solana:{CLUSTER}:jupiter-perps-request:{request}"));
        let mut effects = vec![
            limit_escrow_progress(
                resource.clone(),
                escrow_resource,
                input_asset.clone(),
                collateral,
                collateral,
                collateral,
                0,
                0,
                resource.clone(),
                collateral_name.clone(),
                fin_domain::PositionKind::LockedCollateral,
                margin.clone(),
                *collateral_delta_usd,
                fin_domain::EscrowLifecycleState::Filled,
                Some(collateral_anchor.clone()),
                size,
                Some(derivative_anchor.clone()),
            ),
            fin_domain::FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: collateral_name.clone(),
                asset: margin.clone(),
                delta: fin_domain::SignedQuantity::positive(&collateral_change),
                scaled_receipt: None,
                anchor: Some(collateral_anchor),
            },
            fin_domain::FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: position.to_owned(),
                asset: derivative_asset,
                delta: fin_domain::SignedQuantity::positive(&fin_domain::Quantity::from_u64(size)),
                scaled_receipt: None,
                anchor: Some(derivative_anchor),
            },
        ];
        if protocol_fee_is_observed(*fee_usd, *fee_token) {
            let gross_converted = collateral_delta_usd
                .checked_add(*fee_usd)
                .ok_or_else(receipt_mismatch)?;
            let collateral_location = AccountingLocation::Position {
                resource: resource.clone(),
                position: collateral_name.clone(),
                position_kind: fin_domain::PositionKind::LockedCollateral,
            };
            effects.push(fin_domain::FinancialEffect::Cost {
                location: collateral_location.clone(),
                amount: Amount {
                    asset: margin.clone(),
                    atoms: fin_domain::Quantity::from_u64(*fee_usd),
                },
                category: "jupiter_perps_protocol_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            });
            effects.push(fin_domain::FinancialEffect::CostDenomination {
                location: collateral_location.clone(),
                cost_asset: margin.clone(),
                category: "jupiter_perps_protocol_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
                denomination: Amount {
                    asset: asset(
                        binding["collateral_mint"]
                            .as_str()
                            .ok_or_else(bad_response)?,
                        binding["collateral_decimals"]
                            .as_u64()
                            .and_then(|value| u8::try_from(value).ok())
                            .ok_or_else(bad_response)?,
                    ),
                    atoms: fin_domain::Quantity::from_u64(*fee_token),
                },
                evidence_id: format!("perps-limit-fill:{signature}"),
            });
            effects.push(fin_domain::FinancialEffect::IncludedCostConversion {
                location: collateral_location,
                source: fin_domain::IncludedCostSource::Escrow {
                    wallet_resource: resource.clone(),
                    escrow_resource: ResourceId(format!(
                        "solana:{CLUSTER}:jupiter-perps-request:{request}"
                    )),
                },
                gross_debit: Amount {
                    asset: input_asset.clone(),
                    atoms: fin_domain::Quantity::from_u64(collateral),
                },
                gross_converted: Amount {
                    asset: margin.clone(),
                    atoms: fin_domain::Quantity::from_u64(gross_converted),
                },
                net_position: Amount {
                    asset: margin.clone(),
                    atoms: fin_domain::Quantity::from_u64(*collateral_delta_usd),
                },
                included_cost: Amount {
                    asset: margin,
                    atoms: fin_domain::Quantity::from_u64(*fee_usd),
                },
                evidence_id: format!("perps-limit-fill:{signature}"),
            });
        }
        if request_fee > 0 {
            effects.push(fin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: fin_domain::Quantity::from_u64(request_fee),
                },
                category: "solana_network_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        if keeper_fee > 0 && wallet_pays_keeper {
            effects.push(fin_domain::FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: fin_domain::Quantity::from_u64(keeper_fee),
                },
                category: "solana_network_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        effects.extend(terminal_account_funding_progress(
            record,
            binding,
            wallet.address(),
            request,
            request_signature,
            request_slot,
            request_message_hash,
            signature,
            slot,
            &keeper_hash,
            Some(&keeper_proof),
        )?);
        let evidence = json!({"request":request,"position":position,"request_signature":request_signature,"request_slot":request_slot,"request_proof":request_proof,"keeper_signature":signature,"keeper_slot":slot,"keeper_proof":keeper_proof,"keeper_event":matched[0],"request_rent_refund_lamports":request_rent});
        let placement_digest = fin_domain::binding_hash(&json!({
            "signature":request_signature,
            "slot":request_slot,
            "message_sha256":request_message_hash,
            "proof":request_proof,
        }))?;
        let keeper_digest = fin_domain::binding_hash(&json!({
            "signature":signature,
            "slot":slot,
            "message_sha256":keeper_hash,
            "proof":keeper_proof,
            "event":matched[0],
        }))?;
        let settlement_progress = progress_metadata(
            record,
            request,
            [
                (format!("placement:{request_signature}"), placement_digest),
                (format!("keeper:{signature}"), keeper_digest),
            ],
        )?;
        let mut evidence = evidence;
        evidence["settlement_progress"] = settlement_progress;
        let mut verified = fin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence,
            observed_at: Utc::now().to_rfc3339(),
            proof_digest: String::new(),
        };
        verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
        return Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        });
    }
    if let Some(cancelled) = reconcile_cancelled_parent(
        adapter,
        record,
        submission,
        wallet,
        request,
        request_signature,
        request_slot,
        request_message_hash,
        request_proof,
        request_fee,
    )
    .map_err(|error| {
        // Keep a safe category from the inner proof check. Discard arbitrary
        // provider text, but do not collapse every bounded failure into one
        // opaque parent-level mismatch.
        receipt_mismatch_at("parent cancellation reconciliation").with_details(json!({
            "cause_code": error.code,
            "cause_stage": error.stage,
        }))
    })? {
        return Ok(cancelled);
    }
    Ok(Reconciliation {
        status: Status::Pending,
        evidence: json!({"request":request,"request_signature":request_signature,"request_slot":request_slot,"keeper_signatures_checked":entries.len(),"reason":"No matching finalized keeper fill or cancellation history has been proved."}),
        verified: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn reconcile_cancelled_parent(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
    submission: &Submission,
    wallet: &SolanaWalletAdapter,
    request: &str,
    request_signature: &str,
    request_slot: u64,
    request_message_hash: &str,
    request_proof: &FinalizedReceiptProof,
    request_fee: u64,
) -> Result<Option<Reconciliation>> {
    let children = adapter.cancellation_children(&record.id, submission)?;
    let children = children
        .into_iter()
        .filter(|child| child.operation == "jupiter.perps.limit.cancel")
        .collect::<Vec<_>>();
    if children.len() > 1 {
        return Err(receipt_mismatch_at(
            "multiple durable cancellation children target the parent submission",
        ));
    }
    let Some(child) = children.into_iter().next() else {
        return Ok(None);
    };
    let Some(child_submission) = child.submission.as_ref() else {
        return Ok(None);
    };
    if child.operation != "jupiter.perps.limit.cancel"
        || child.status != Status::Completed
        || child.effects.as_ref().is_none_or(|effects| {
            effects.submission != *child_submission
                || fin_domain::verified_effect_digest(effects)
                    .map(|digest| digest != effects.proof_digest)
                    .unwrap_or(true)
        })
    {
        return Ok(None);
    }
    let Some(preview) = child.preview.as_ref() else {
        return Ok(None);
    };
    let child_binding = &preview.binding;
    if child_binding["parent_operation_id"].as_str() != Some(record.id.as_str())
        || child_binding["request"].as_str() != Some(request)
        || child_binding["wallet_address"].as_str() != Some(wallet.address())
        || serde_json::from_value::<Submission>(child_binding["parent_submission"].clone())
            .ok()
            .as_ref()
            != Some(submission)
    {
        return Ok(None);
    }
    let Some(verified_child) = child.effects.as_ref() else {
        return Ok(None);
    };
    if !verified_child.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::SubmissionCancellation { operation_id, submission: cancelled_submission }
            if operation_id == &record.id && cancelled_submission == submission
    )) {
        return Ok(None);
    }
    let child_evidence = &verified_child.evidence;
    let Some(proof_value) = child_evidence.get("proof") else {
        return Ok(None);
    };
    let proof: FinalizedReceiptProof =
        serde_json::from_value(proof_value.clone()).map_err(|_| receipt_mismatch())?;
    let signature = child_evidence["signature"]
        .as_str()
        .ok_or_else(bad_response)?;
    let slot = child_evidence["slot"].as_u64().ok_or_else(bad_response)?;
    let message_hash = child_evidence["message_sha256"]
        .as_str()
        .ok_or_else(bad_response)?;
    if message_hash != proof.message_sha256
        || child_submission.data["message_sha256"].as_str() != Some(message_hash)
        || child_submission.data["chain_signature"].as_str() != Some(signature)
    {
        return Err(receipt_mismatch_at(
            "cancel child proof does not match its immutable submission",
        ));
    }
    let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
    let close_event = &child_evidence["close_event"];
    if close_event["executed"].as_bool() != Some(false)
        || close_event["request"].as_str() != Some(request)
        || close_event["owner"].as_str() != Some(wallet.address())
        || close_event["mint"].as_str() != binding["funding_mint"].as_str()
        || close_event["amount_atoms"].as_str()
            != Some(
                parse_binding_u64(binding, "collateral_atoms")?
                    .to_string()
                    .as_str(),
            )
        || close_event["request_change"] != 1
        || close_event["request_type"] != 1
        || close_event["side"] != binding["side"]
    {
        return Err(receipt_mismatch_at("cancel close-event identity"));
    }
    let request_rent = parse_binding_u64(binding, "request_rent_lamports")?;
    let rent_delta = proof.lamport_delta(request)?;
    if !rent_delta.is_negative()
        || rent_delta.magnitude() != fin_domain::Quantity::from_u64(request_rent)
        || message_hash != proof.message_sha256
    {
        return Err(receipt_mismatch_at(
            "cancel request-rent refund or message proof",
        ));
    }
    let wallet_token = binding["wallet_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request_token = binding["request_token_account"]
        .as_str()
        .ok_or_else(bad_response)?;
    let mint = binding["funding_mint"].as_str().ok_or_else(bad_response)?;
    let program = binding["token_program"].as_str().ok_or_else(bad_response)?;
    let decimals = binding["funding_decimals"]
        .as_u64()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(bad_response)?;
    let collateral = parse_binding_u64(binding, "collateral_atoms")?;
    let wallet_delta = proof.token_account_delta_verified(
        wallet_token,
        wallet.address(),
        mint,
        program,
        decimals,
    )?;
    let escrow_delta =
        proof.token_account_delta_verified(request_token, request, mint, program, decimals)?;
    if wallet_delta.is_negative()
        || wallet_delta.is_zero()
        || wallet_delta.magnitude() != fin_domain::Quantity::from_u64(collateral)
        || !escrow_delta.is_negative()
        || escrow_delta.magnitude() != fin_domain::Quantity::from_u64(collateral)
    {
        return Err(receipt_mismatch_at(
            "cancel wallet-token and escrow-token deltas",
        ));
    }
    let resource = resource_for_wallet(wallet.address());
    let escrow_resource = ResourceId(format!("solana:{CLUSTER}:jupiter-perps-request:{request}"));
    let position = binding["position"].as_str().ok_or_else(bad_response)?;
    let collateral_name = format!("{position}:collateral");
    let mut effects = vec![limit_escrow_progress(
        resource.clone(),
        escrow_resource,
        asset(mint, decimals),
        collateral,
        collateral,
        0,
        collateral,
        0,
        resource.clone(),
        collateral_name,
        fin_domain::PositionKind::LockedCollateral,
        margin_usd_asset(),
        0,
        fin_domain::EscrowLifecycleState::Cancelled,
        None,
        0,
        None,
    )];
    if request_fee > 0 {
        effects.push(fin_domain::FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset("native", 9),
                atoms: fin_domain::Quantity::from_u64(request_fee),
            },
            category: "solana_network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    effects.extend(
        terminal_account_funding_progress(
            record,
            binding,
            wallet.address(),
            request,
            request_signature,
            request_slot,
            request_message_hash,
            signature,
            slot,
            message_hash,
            Some(&proof),
        )
        .map_err(|error| {
            receipt_mismatch_at("cancel account-funding refund proof").with_details(json!({
                "cause_code": error.code,
                "cause_stage": error.stage,
            }))
        })?,
    );
    let mut digests = vec![(
        format!("placement:{request_signature}"),
        fin_domain::binding_hash(&json!({
            "signature":request_signature,"slot":request_slot,"message_sha256":request_message_hash,"proof":request_proof
        }))?,
    )];
    digests.push((
        format!("cancel:{signature}"),
        fin_domain::binding_hash(&json!({
            "signature":signature,"slot":slot,"message_sha256":message_hash,"proof":proof,
            "close_event":child_evidence["close_event"]
        }))?,
    ));
    let settlement_progress = progress_metadata(record, request, digests)
        .map_err(|_| receipt_mismatch_at("cancel settlement progress continuity"))?;
    let mut evidence = json!({
        "request":request,"request_signature":request_signature,"request_slot":request_slot,
        "request_proof":request_proof,"cancel_operation_id":child.id,
        "cancel_signature":signature,"cancel_slot":slot,"cancel_proof":proof,
        "close_event":child_evidence["close_event"],"request_rent_refund_lamports":request_rent
    });
    evidence["settlement_progress"] = settlement_progress;
    let mut verified = fin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = fin_domain::verified_effect_digest(&verified)?;
    Ok(Some(Reconciliation {
        status: Status::Completed,
        evidence: verified.evidence.clone(),
        verified: Some(verified),
    }))
}

pub(super) fn progress_metadata(
    record: &OperationRecord,
    request: &str,
    additions: impl IntoIterator<Item = (String, String)>,
) -> Result<Value> {
    let previous = record
        .effects
        .as_ref()
        .map(|effects| fin_domain::SettlementProgress::from_evidence(&effects.evidence))
        .transpose()?
        .flatten();
    let mut event_digests = previous
        .as_ref()
        .map(|progress| progress.event_digests.clone())
        .unwrap_or_default();
    for (identity, digest) in additions {
        if identity.is_empty() || digest.is_empty() {
            return Err(receipt_mismatch());
        }
        match event_digests.get(&identity) {
            Some(existing) if existing == &digest => {}
            Some(_) => return Err(receipt_mismatch()),
            None => {
                event_digests.insert(identity, digest);
            }
        }
    }
    let sequence = match previous.as_ref() {
        None => 1,
        // Each newly verified observation gets a higher sequence even when
        // the event set is unchanged. This lets the engine distinguish a new
        // cumulative proof from a byte-different rewrite of the old snapshot;
        // event identities and digests remain immutable and effects stay
        // cumulative, so retries cannot apply an economic delta twice.
        Some(progress) => progress
            .sequence
            .checked_add(1)
            .ok_or_else(receipt_mismatch)?,
    };
    Ok(json!({
        "version":1,
        "sequence":sequence,
        "snapshot_id":format!("jupiter-perps-request:{request}:{sequence}"),
        "event_digests":event_digests,
        "position_orders":previous.as_ref().map(|progress| progress.position_orders.clone()).unwrap_or_default(),
    }))
}

pub(super) fn foreign_missing_signers(
    encoded: &str,
    wallet_signer_address: &str,
) -> Result<Vec<String>> {
    Ok(
        fin_solana::transaction::SolanaRuntime::missing_signers(encoded)?
            .into_iter()
            .filter(|address| address.as_str() != wallet_signer_address)
            .collect(),
    )
}

pub(super) fn foreign_required_signers(
    encoded: &str,
    wallet_signer_address: &str,
) -> Result<Vec<String>> {
    Ok(
        fin_solana::transaction::SolanaRuntime::required_signers(encoded)?
            .into_iter()
            .filter(|address| address.as_str() != wallet_signer_address)
            .collect(),
    )
}

pub(super) async fn revalidate(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let encoded = prepared.binding["transaction_base64"]
        .as_str()
        .ok_or_else(bad_response)?;
    let allowed =
        super::approved_missing_signers_from_binding(encoded, signer.address(), &prepared.binding)?;
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            parse_binding_u64(&prepared.binding, "last_valid_block_height")?,
            parse_binding_u64(&prepared.binding, "maximum_network_fee_lamports")?,
            None,
            &allowed,
        )
        .await?;
    if frozen.message_sha256()
        != prepared.binding["message_sha256"]
            .as_str()
            .ok_or_else(bad_response)?
    {
        return Err(unreviewed());
    }
    validate_frozen(adapter, prepared, &frozen).await?;
    let collateral_atoms = required_u64(&prepared.input, "collateral_amount_atoms")?;
    let auxiliary = crate::adapter::validate_auxiliary_instructions_with_authorities(
        &wallet,
        &frozen,
        &[
            prepared.binding["market_mint"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
            prepared.binding["collateral_mint"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
            prepared.binding["funding_mint"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
        ],
        (prepared.binding["funding_mint"].as_str() == Some(crate::swap::WRAPPED_SOL_MINT))
            .then_some(collateral_atoms),
        parse_binding_u64(&prepared.binding, "maximum_account_funding_lamports")?,
        Some(crate::jlp::PERPS_PROGRAM_ID),
        &[
            wallet.address().to_owned(),
            prepared.binding["request"]
                .as_str()
                .ok_or_else(bad_response)?
                .to_owned(),
        ],
    )
    .await?;
    let cpi_rent = cpi_account_rent(
        &wallet,
        prepared.binding["request"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["position"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["market_mint"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["collateral_mint"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["side"].as_u64().ok_or_else(bad_response)? as u8,
    )
    .await?;
    let total_rent = auxiliary
        .account_funding_lamports
        .checked_add(cpi_rent.total()?)
        .ok_or_else(bad_response)?;
    if total_rent != parse_binding_u64(&prepared.binding, "account_rent_lamports")?
        || cpi_rent.request_lamports
            != parse_binding_u64(&prepared.binding, "request_rent_lamports")?
        || cpi_rent.position_lamports
            != parse_binding_u64(&prepared.binding, "position_rent_lamports")?
        || serde_json::to_value(&auxiliary.funding_accounts).map_err(|_| bad_response())?
            != prepared.binding["funding_accounts"]
    {
        return Err(unreviewed_detail(
            "Perps account funding changed after preparation",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

pub(super) async fn validate_frozen(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
    frozen: &FrozenTransaction,
) -> Result<()> {
    let input = &prepared.input;
    let wallet = adapter.wallet(input)?;
    let market = required_market_symbol(input, "market")?;
    let side = match required_text(input, "side")? {
        "long" => 1,
        "short" => 2,
        _ => return Err(unreviewed()),
    };
    let instruction = inspect_create_instruction(
        &wallet,
        frozen,
        market,
        required_funding_symbol(input, side, market)?,
        side,
        required_u64(input, "collateral_amount_atoms")?,
        required_u64(input, "size_usd_atoms")?,
        required_u64(input, "trigger_price_usd_atoms")?,
        input
            .get("trigger_above_threshold")
            .and_then(Value::as_bool)
            .ok_or_else(bad_response)?,
        prepared.binding["position"]
            .as_str()
            .ok_or_else(bad_response)?,
        prepared.binding["request"]
            .as_str()
            .ok_or_else(bad_response)?,
    )
    .await?;
    if instruction.position != prepared.binding["position"]
        || instruction.request != prepared.binding["request"]
        || instruction.counter.to_string()
            != prepared.binding["counter"]
                .as_str()
                .ok_or_else(bad_response)?
    {
        return Err(unreviewed());
    }
    Ok(())
}

pub(super) async fn permission_scope(
    adapter: &PerpsAdapter,
    input: &Value,
    bounds_value: &Value,
) -> Result<PlanPermission> {
    let bounds: PerpsBoundaryBounds = serde_json::from_value(bounds_value.clone())
        .map_err(|_| FinError::validation("Invalid Jupiter Perps limit boundary limits."))?;
    let max_size = fin_domain::Quantity::new(&bounds.maximum_size_usd_atoms)?;
    let max_collateral = fin_domain::Quantity::new(&bounds.maximum_collateral_atoms)?;
    let max_fee_usd = fin_domain::Quantity::new(&bounds.maximum_protocol_fee_usd_atoms)?;
    let max_fee_atoms = fin_domain::Quantity::new(&bounds.maximum_protocol_fee_atoms)?;
    if max_size.is_zero()
        || max_collateral.is_zero()
        || max_fee_usd.is_zero()
        || max_fee_atoms.is_zero()
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_account_funding_lamports == 0
    {
        return Err(FinError::validation(
            "Perps limit permissions require finite size, collateral, protocol fee, network fee and account funding caps.",
        ));
    }
    let wallet_name = required_text(input, "wallet")?.to_owned();
    let wallet = adapter.wallet(input)?;
    let market = required_market_symbol(input, "market")?;
    let side = match required_text(input, "side")? {
        "long" => 1u8,
        "short" => 2u8,
        _ => return Err(FinError::validation("Choose long or short Perps side.")),
    };
    let funding_symbol = required_funding_symbol(input, side, market)?;
    let market_mint = market_mint_for_symbol(market)?.to_owned();
    let collateral_mint = collateral_mint_for_market(market, side)?.to_owned();
    let funding_mint = token_mint_for_symbol(funding_symbol)?.to_owned();
    let position = derive_position_address(wallet.address(), &market_mint, &collateral_mint, side)?;
    let resource = resource_for_wallet(wallet.address());
    let native = asset("native", 9);
    let funding_decimals = crate::adapter::token_decimals(&wallet.runtime(), &funding_mint).await?;
    let collateral_decimals =
        crate::adapter::token_decimals(&wallet.runtime(), &collateral_mint).await?;
    let collateral_symbol = token_symbol_for_mint(&collateral_mint).ok_or_else(|| {
        FinError::validation("Perps collateral mint has no verified token label.")
    })?;
    let maximum_size_atoms = bounds
        .maximum_size_usd_atoms
        .parse::<u64>()
        .map_err(|_| FinError::validation("Perps size bound must be integer atoms."))?;
    let maximum_collateral_atoms = bounds
        .maximum_collateral_atoms
        .parse::<u64>()
        .map_err(|_| FinError::validation("Perps collateral bound must be integer atoms."))?;
    let maximum_fee_usd_atoms = bounds
        .maximum_protocol_fee_usd_atoms
        .parse::<u64>()
        .map_err(|_| FinError::validation("Perps fee bound must be integer atoms."))?;
    let maximum_fee_token_atoms = bounds
        .maximum_protocol_fee_atoms
        .parse::<u64>()
        .map_err(|_| FinError::validation("Perps token fee bound must be integer atoms."))?;
    let funding_asset = asset(&funding_mint, funding_decimals);
    let fixed = json!({"operation":OP_LIMIT_CREATE,"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":wallet_name,"wallet_address":wallet.address(),"market":market,"market_mint":market_mint,"collateral_mint":collateral_mint,"funding_mint":funding_mint,"side":side,"position":position,"trigger_price_usd_atoms":required_text(input,"trigger_price_usd_atoms")?,"trigger_above_threshold":input["trigger_above_threshold"]});
    let spec = BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| FinError::validation("Invalid Perps boundary limits."))?,
        maximum_debits: vec![
            ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: funding_asset.clone(),
                    atoms: max_collateral.clone(),
                },
            },
            ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: fin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
                },
            },
        ],
        maximum_costs: vec![
            CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: native.clone(),
                    atoms: fin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
                },
                treatment: CostTreatment::AdditionalDebit,
            },
            CostBound {
                location: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: format!("{position}:collateral"),
                    position_kind: fin_domain::PositionKind::LockedCollateral,
                },
                amount: Amount {
                    asset: margin_usd_asset(),
                    atoms: max_fee_usd.clone(),
                },
                treatment: CostTreatment::IncludedInDebit,
            },
        ],
        summary: limit_permission_summary(
            market,
            side,
            maximum_size_atoms,
            maximum_collateral_atoms,
            funding_symbol,
            funding_decimals,
            collateral_symbol,
            maximum_fee_usd_atoms,
            maximum_fee_token_atoms,
            collateral_decimals,
            bounds.maximum_network_fee_lamports,
            bounds.maximum_account_funding_lamports,
        ),
    };
    Ok(PlanPermission {
        recovery_parent: None,
        spec,
        possible_credits: vec![],
        fee_assets: vec![native, funding_asset, margin_usd_asset()],
        deferred_receipt: None,
        authorization_seconds: Some(600),
        cumulative_scope: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn limit_permission_summary(
    market: &str,
    side: u8,
    maximum_size_atoms: u64,
    maximum_funding_atoms: u64,
    funding_symbol: &str,
    funding_decimals: u8,
    collateral_symbol: &str,
    maximum_fee_usd_atoms: u64,
    maximum_fee_token_atoms: u64,
    collateral_decimals: u8,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
) -> String {
    format!(
        "Authorize Jupiter Perps {market} {} limit requests up to {} notional, locking up to {} {funding_symbol} from {collateral_symbol}. Protocol fees are bounded at {} and {} {collateral_symbol}; reserve up to {} SOL for network fees and {} SOL for refundable account funding. Trigger price and direction stay fixed to this request.",
        if side == 1 { "long" } else { "short" },
        format_usd_micros(maximum_size_atoms),
        format_token_atoms(maximum_funding_atoms, funding_decimals),
        format_usd_micros(maximum_fee_usd_atoms),
        format_token_atoms(maximum_fee_token_atoms, collateral_decimals),
        format_token_atoms(maximum_network_fee_lamports, 9),
        format_token_atoms(maximum_account_funding_lamports, 9),
    )
}

pub(super) fn validate_permission_candidate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let bounds: PerpsBoundaryBounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| FinError::validation("Invalid Jupiter Perps limit permission."))?;
    let b = &prepared.binding;
    for field in [
        "operation",
        "network",
        "cluster",
        "program",
        "wallet_name",
        "wallet_address",
        "market",
        "market_mint",
        "collateral_mint",
        "funding_mint",
        "side",
        "position",
        "trigger_price_usd_atoms",
        "trigger_above_threshold",
    ] {
        if spec.fixed.get(field)
            != Some(&match field {
                "operation" => json!(prepared.operation),
                "network" => json!(NETWORK),
                "cluster" => json!(CLUSTER),
                "program" => json!(crate::jlp::PERPS_PROGRAM_ID),
                "wallet_name" => prepared.input["wallet"].clone(),
                "wallet_address" => b["wallet_address"].clone(),
                "market" => prepared.input["market"].clone(),
                "market_mint" => b["market_mint"].clone(),
                "collateral_mint" => b["collateral_mint"].clone(),
                "funding_mint" => b["funding_mint"].clone(),
                "side" => b["side"].clone(),
                "position" => b["position"].clone(),
                "trigger_price_usd_atoms" => prepared.input["trigger_price_usd_atoms"].clone(),
                "trigger_above_threshold" => prepared.input["trigger_above_threshold"].clone(),
                _ => Value::Null,
            })
        {
            return Err(FinError::validation(
                "The Perps limit request differs from the fixed authorized wallet, market, side or trigger.",
            ));
        }
    }
    if parse_binding_u64(b, "size_usd_atoms")?
        > bounds
            .maximum_size_usd_atoms
            .parse::<u64>()
            .map_err(|_| bad_response())?
        || parse_binding_u64(b, "collateral_atoms")?
            > bounds
                .maximum_collateral_atoms
                .parse::<u64>()
                .map_err(|_| bad_response())?
        || parse_binding_u64(b, "maximum_protocol_fee_usd_atoms")?
            > bounds
                .maximum_protocol_fee_usd_atoms
                .parse::<u64>()
                .map_err(|_| bad_response())?
        || parse_binding_u64(b, "maximum_protocol_fee_atoms")?
            > bounds
                .maximum_protocol_fee_atoms
                .parse::<u64>()
                .map_err(|_| bad_response())?
        || parse_binding_u64(b, "maximum_network_fee_lamports")?
            > bounds.maximum_network_fee_lamports
        || parse_binding_u64(b, "maximum_account_funding_lamports")?
            > bounds.maximum_account_funding_lamports
    {
        return Err(FinError::validation(
            "The Perps limit request exceeds its authorized size, collateral, cost or funding bound.",
        ));
    }
    Ok(())
}

pub(super) async fn prepare(adapter: &PerpsAdapter, input: &Value) -> Result<PreparedOperation> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let signer = wallet.signer().ok_or_else(|| {
        FinError::new(
            "SOLANA_SIGNING_UNAVAILABLE",
            "prepare",
            "The selected wallet has no protected signing capability.",
            "Enroll a protected signer before creating a Jupiter Perps limit request.",
        )
    })?;
    let market = required_market_symbol(input, "market")?;
    let side = match required_text(input, "side")? {
        "long" => 1u8,
        "short" => 2u8,
        _ => return Err(FinError::validation("Choose long or short Perps side.")),
    };
    let funding_symbol = required_funding_symbol(input, side, market)?;
    let collateral_atoms = required_u64(input, "collateral_amount_atoms")?;
    let size_usd_atoms = required_u64(input, "size_usd_atoms")?;
    let trigger_price_usd_atoms = required_u64(input, "trigger_price_usd_atoms")?;
    let trigger_above_threshold = input
        .get("trigger_above_threshold")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            FinError::validation("Choose whether the limit trigger is above or below its price.")
        })?;
    let maximum_network_fee_lamports = required_u64(input, "maximum_network_fee_lamports")?;
    let maximum_account_funding_lamports = required_u64(input, "maximum_account_funding_lamports")?;
    let maximum_protocol_fee_usd_atoms = required_u64(input, "maximum_protocol_fee_usd_atoms")?;
    let maximum_protocol_fee_atoms = required_u64(input, "maximum_protocol_fee_atoms")?;
    if collateral_atoms == 0
        || size_usd_atoms == 0
        || trigger_price_usd_atoms == 0
        || maximum_network_fee_lamports == 0
        || maximum_account_funding_lamports == 0
        || maximum_protocol_fee_usd_atoms == 0
        || maximum_protocol_fee_atoms == 0
    {
        return Err(FinError::validation(
            "Perps limit collateral, size, trigger, fee and funding limits must be positive.",
        ));
    }
    let response = adapter
        .api
        .request(
            JupiterHost::Perps,
            Method::POST,
            "/orders/limit",
            Some(json!({
                "asset":market,
                "inputToken":funding_symbol,
                "inputTokenAmount":collateral_atoms.to_string(),
                "sizeUsdDelta":size_usd_atoms.to_string(),
                "side":if side == 1 {"long"} else {"short"},
                "triggerPrice":trigger_price_usd_atoms.to_string(),
                "walletAddress":wallet.address()
            })),
            RateBucket::Main,
            RequestPriority::Financial,
            DEADLINE,
        )
        .await?;
    if response
        .get("requireKeeperSignature")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(unreviewed_detail(
            "Perps limit transaction is missing its pinned keeper-signature requirement",
        ));
    }
    let quote = response
        .get("quote")
        .filter(|value| value.is_object())
        .ok_or_else(bad_response)?;
    if parse_u64_text(quote, "sizeUsdDelta")? != size_usd_atoms
        || quote.get("side").and_then(Value::as_str)
            != Some(if side == 1 { "long" } else { "short" })
        || parse_u64_text(quote, "openFeeUsd")? > maximum_protocol_fee_usd_atoms
    {
        return Err(FinError::validation(
            "The Perps limit quote exceeds the approved notional, side, or protocol-fee bound.",
        ));
    }
    let request = response
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?
        .to_owned();
    let position = response
        .get("positionPubkey")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?
        .to_owned();
    fin_solana::parse_address(&request)?;
    fin_solana::parse_address(&position)?;
    let encoded = response
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(bad_response)?;
    let metadata = response
        .get("txMetadata")
        .filter(|v| v.is_object())
        .ok_or_else(bad_response)?;
    let last_valid_block_height = parse_u64_text(metadata, "lastValidBlockHeight")?;
    // Provider rent metadata is diagnostic only. The amount authorized below
    // comes from account sizes, existence checks, and finalized RPC rent quotes.
    let _provider_rent_estimate = parse_u64_text(metadata, "accountRentLamports")?;
    // `missing_signers` reports every zero signature slot, including the
    // enrolled wallet's own slot. The provider may require exactly the two
    // read-only keeper roles in addition to that wallet signer.
    let allowed_cosigners = foreign_missing_signers(encoded, signer.address())?;
    if allowed_cosigners.len() > 2 {
        return Err(unreviewed_detail("unexpected Perps limit co-signer count"));
    }
    let frozen = wallet
        .runtime()
        .freeze_transaction(
            encoded,
            &signer,
            last_valid_block_height,
            maximum_network_fee_lamports,
            None,
            &allowed_cosigners,
        )
        .await?;
    let instruction = inspect_create_instruction(
        &wallet,
        &frozen,
        market,
        funding_symbol,
        side,
        collateral_atoms,
        size_usd_atoms,
        trigger_price_usd_atoms,
        trigger_above_threshold,
        &position,
        &request,
    )
    .await?;
    if instruction.position != position || instruction.request != request {
        return Err(unreviewed_detail(
            "limit order response PDA differs from its pinned instruction",
        ));
    }
    let collateral_decimals =
        crate::adapter::token_decimals(&wallet.runtime(), &instruction.collateral_mint).await?;
    let pre_position = adapter.existing_position(&wallet, &position).await?;
    if pre_position.as_ref().is_some_and(|existing| {
        existing.market_mint != instruction.market_mint
            || existing.side
                != if side == 1 {
                    fin_domain::PositionKind::DerivativeLong
                } else {
                    fin_domain::PositionKind::DerivativeShort
                }
    }) {
        return Err(invalid_account_set());
    }
    let auxiliary = crate::adapter::validate_auxiliary_instructions_with_authorities(
        &wallet,
        &frozen,
        &[
            instruction.market_mint.clone(),
            instruction.collateral_mint.clone(),
            instruction.funding_mint.clone(),
        ],
        (instruction.funding_mint == crate::swap::WRAPPED_SOL_MINT).then_some(collateral_atoms),
        maximum_account_funding_lamports,
        Some(crate::jlp::PERPS_PROGRAM_ID),
        &[wallet.address().to_owned(), request.clone()],
    )
    .await?;
    let cpi_rent = cpi_account_rent(
        &wallet,
        &request,
        &position,
        &instruction.market_mint,
        &instruction.collateral_mint,
        side,
    )
    .await?;
    let account_funding_lamports = auxiliary
        .account_funding_lamports
        .checked_add(cpi_rent.total()?)
        .ok_or_else(bad_response)?;
    if account_funding_lamports > maximum_account_funding_lamports {
        return Err(unreviewed_detail(
            "verified account rent exceeds the authorized funding limit",
        ));
    }
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;

    let resource = resource_for_wallet(wallet.address());
    let escrow_resource = ResourceId(format!(
        "solana:mainnet-beta:jupiter-perps-request:{request}"
    ));
    let funding_asset = asset(&instruction.funding_mint, instruction.funding_decimals);
    let margin_asset = margin_usd_asset();
    let network_asset = asset("native", 9);
    let fee_bound = CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: network_asset.clone(),
            atoms: fin_domain::Quantity::from_u64(maximum_network_fee_lamports),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let (mut valuation, _) = crate::adapter::valuation(
        adapter.prices.clone(),
        margin_asset,
        size_usd_atoms,
        funding_asset.clone(),
        collateral_atoms,
        maximum_network_fee_lamports,
        wallet.address(),
    )
    .await?;
    let native_asset = asset("native", 9);
    let native_price = valuation
        .prices
        .iter()
        .find(|price| price.asset == native_asset)
        .ok_or_else(bad_response)?;
    valuation.additional_allocation_usd_micros =
        fin_domain::Quantity::from_u64(account_funding_lamports)
            .mul_div_ceil(&native_price.usd_micros_per_unit, &native_price.unit_atoms)?;
    let rent_amount = fin_domain::Quantity::from_u64(account_funding_lamports);
    let mut debits = vec![ResourceAmount {
        resource: resource.clone(),
        amount: Amount {
            asset: funding_asset,
            atoms: fin_domain::Quantity::from_u64(collateral_atoms),
        },
    }];
    if !rent_amount.is_zero() {
        debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: network_asset,
                atoms: rent_amount.clone(),
            },
        });
    }
    let binding = json!({
        "operation":OP_LIMIT_CREATE,"wallet_address":wallet.address(), "market":market, "side":side,
        "position":position,"request":request,
        "market_mint":instruction.market_mint,"collateral_mint":instruction.collateral_mint,
        "collateral_decimals":collateral_decimals,
        "funding_mint":instruction.funding_mint,"funding_decimals":instruction.funding_decimals,
        "collateral_atoms":collateral_atoms.to_string(),"size_usd_atoms":size_usd_atoms.to_string(),
        "trigger_price_usd_atoms":trigger_price_usd_atoms.to_string(),
        "trigger_above_threshold":trigger_above_threshold,
        "counter":instruction.counter.to_string(),"account_rent_lamports":account_funding_lamports.to_string(),
        "pre_size_usd_atoms":pre_position.as_ref().map(|position|position.size_usd_atoms).unwrap_or(0).to_string(),
        "pre_collateral_usd_atoms":pre_position.as_ref().map(|position|position.collateral_usd_atoms).unwrap_or(0).to_string(),
        "wallet_token_account":instruction.wallet_token_account,
        "request_token_account":instruction.request_token_account,
        "token_program":instruction.token_program,
        "request_rent_lamports":cpi_rent.request_lamports.to_string(),
        "position_rent_lamports":cpi_rent.position_lamports.to_string(),
        "fee_payer":frozen.fee_payer(),"message_sha256":frozen.message_sha256(),
        "last_valid_block_height":last_valid_block_height,"recent_blockhash":frozen.recent_blockhash(),
        "transaction_base64":encoded,"maximum_network_fee_lamports":maximum_network_fee_lamports.to_string(),
        "approved_external_cosigners":fin_solana::transaction::SolanaRuntime::required_signers(encoded)?.into_iter().filter(|address| address.as_str() != signer.address()).collect::<Vec<_>>(),
        "maximum_account_funding_lamports":maximum_account_funding_lamports.to_string(),
        "maximum_protocol_fee_usd_atoms":maximum_protocol_fee_usd_atoms.to_string(),
        "maximum_protocol_fee_atoms":maximum_protocol_fee_atoms.to_string(),
        "funding_accounts":auxiliary.funding_accounts,
        "created_token_accounts":auxiliary.created_token_accounts.iter().map(|created|json!({"address":created.address,"mint":created.mint,"token_program":created.token_program})).collect::<Vec<_>>(),
        "closed_wrapped_native_accounts":auxiliary.closed_wrapped_native_accounts,
        "closed_wrapped_native_refunds":auxiliary.closed_wrapped_native_refunds,
        "wrapped_native_lamports":auxiliary.wrapped_native_lamports.to_string()
    });
    let lifecycle_accounts = account_funding_specs(&binding, wallet.address())?
        .into_iter()
        .map(|spec| fin_domain::RefundableFundingLimit {
            position: spec.position,
            account: spec.account,
            maximum_amount: spec.amount,
        })
        .collect::<Vec<_>>();
    Ok(PreparedOperation {
        operation: OP_LIMIT_CREATE.into(), input: input.clone(), binding,
        summary: format!("Place a {market} {} Perps limit request for {}, locking {} {funding_symbol} at trigger {}.", if side == 1 {"long"} else {"short"}, format_usd_micros(size_usd_atoms), format_token_atoms(collateral_atoms, instruction.funding_decimals), format_usd_micros(trigger_price_usd_atoms)),
        checks: vec![check("Pinned instantCreateLimitOrder instruction and signer roles"),check("Exact market, position and PositionRequest PDAs"),check("Bounded collateral, protocol fee, network fee and rent"),check("Transaction simulation")],
        warnings: vec!["Placement locks collateral in a keeper request; it is not a completed position. Fill or cancellation must be reconciled against the same request.".into()],
        observed_at:Utc::now().to_rfc3339(), expires_at:(Utc::now()+ChronoDuration::seconds(20)).to_rfc3339(), valuation:Some(valuation),
        intent:FinancialIntent {
            debits,
            maximum_costs:{
                let mut costs=vec![fee_bound];
                if maximum_protocol_fee_usd_atoms>0 {
                    costs.push(CostBound {
                        location:AccountingLocation::Position {
                            resource:resource.clone(),position:format!("{position}:collateral"),
                            position_kind:fin_domain::PositionKind::LockedCollateral,
                        },
                        amount:Amount{asset:margin_usd_asset(),atoms:fin_domain::Quantity::from_u64(maximum_protocol_fee_usd_atoms)},
                        // The keeper fee is withheld from the gross escrowed
                        // collateral. The finalized IncludedCostConversion
                        // proves it once without charging a second debit.
                        treatment:CostTreatment::IncludedInDebit,
                    });
                }
                costs
            },
            expected:{let mut expected=vec![EffectConstraint::EscrowToPosition {
                wallet_resource:resource.clone(), escrow_resource,
                input_asset:asset(&instruction.funding_mint, instruction.funding_decimals),
                position_resource:resource.clone(),
                position:format!("{position}:collateral"),
                position_kind:fin_domain::PositionKind::LockedCollateral,
                position_asset:margin_usd_asset(),
                maximum_wallet_debit:fin_domain::Quantity::from_u64(collateral_atoms),
                maximum_position_change:fin_domain::Quantity::from_u64(size_usd_atoms),
                minimum_position_per_input_numerator:fin_domain::Quantity::zero(),
                minimum_position_per_input_denominator:fin_domain::Quantity::from_u64(collateral_atoms),
                conditional_position:Some(fin_domain::EscrowConditionalPosition {
                    resource:resource.clone(), position:position.clone(),
                    position_kind:if side==1 {fin_domain::PositionKind::DerivativeLong} else {fin_domain::PositionKind::DerivativeShort},
                    asset:crate::pricing::position_usd_asset(&instruction.market_mint)?,
                    maximum_change:fin_domain::Quantity::from_u64(size_usd_atoms),
                }),
            }, EffectConstraint::CostDenomination {
                location:AccountingLocation::Position {
                    resource:resource.clone(),position:format!("{position}:collateral"),
                    position_kind:fin_domain::PositionKind::LockedCollateral,
                },
                cost_asset:margin_usd_asset(),
                category:"jupiter_perps_protocol_fee".into(),
                treatment:CostTreatment::IncludedInDebit,
                denomination_asset:asset(&instruction.collateral_mint,collateral_decimals),
                maximum_amount:fin_domain::Quantity::from_u64(maximum_protocol_fee_atoms),
            }];
                if !lifecycle_accounts.is_empty() {
                    expected.push(EffectConstraint::AccountFundingLifecycle {
                        resource: resource.clone(),
                        asset: native_asset.clone(),
                        accounts: lifecycle_accounts,
                    });
                }
                for refund in auxiliary.closed_wrapped_native_refunds.iter().filter(|refund| !auxiliary.funding_accounts.iter().any(|funding| funding.address==refund.address)) {
                    let position=format!("account-funding:{}",refund.address); let amount=fin_domain::Quantity::from_u64(refund.lamports);
                    expected.push(EffectConstraint::PositionOfKind{resource:resource.clone(),position:position.clone(),asset:native_asset.clone(),position_kind:fin_domain::PositionKind::RefundableAccountFunding,direction:fin_domain::Direction::Decrease,maximum_change:amount.clone()});
                    expected.push(EffectConstraint::OriginMovement{from:AccountingLocation::Position{resource:resource.clone(),position,position_kind:fin_domain::PositionKind::RefundableAccountFunding},to:AccountingLocation::Cash{resource:resource.clone()},debit_asset:native_asset.clone(),credit_asset:native_asset.clone(),maximum_debit:amount.clone(),minimum_credit:amount.clone()});
                    expected.push(EffectConstraint::Credit{to:resource.clone(),asset:native_asset.clone(),minimum_credit:amount});
                }
                expected
            }, locks:vec![resource,ResourceId(format!("solana:mainnet-beta:jupiter-perps-request:{request}")),super::position_lock_resource(&position)]
        }, action_deadline:None, operation_version:1
    })
}

struct LimitCreateInstruction {
    position: String,
    request: String,
    market_mint: String,
    collateral_mint: String,
    funding_mint: String,
    funding_decimals: u8,
    wallet_token_account: String,
    request_token_account: String,
    token_program: String,
    counter: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CpiRent {
    pub(super) request_lamports: u64,
    pub(super) position_lamports: u64,
}

impl CpiRent {
    pub(super) fn total(self) -> Result<u64> {
        self.request_lamports
            .checked_add(self.position_lamports)
            .ok_or_else(bad_response)
    }
}

pub(super) async fn cpi_account_rent(
    wallet: &SolanaWalletAdapter,
    request: &str,
    position: &str,
    market_mint: &str,
    collateral_mint: &str,
    side: u8,
) -> Result<CpiRent> {
    let request_info = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([request,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let request_value = request_info.get("value").ok_or_else(bad_response)?;
    if !request_value.is_null() {
        return Err(FinError::validation(
            "This Perps limit request address already exists; prepare a fresh request instead.",
        ));
    }
    let position_info = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([position,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let position_value = position_info.get("value").ok_or_else(bad_response)?;
    let position_lamports = if !position_value.is_null() {
        let owner = position_value["owner"]
            .as_str()
            .ok_or_else(invalid_account_set)?;
        if owner != crate::jlp::PERPS_PROGRAM_ID {
            return Err(invalid_account_set());
        }
        let data = fin_solana::token::decode_rpc_account_data(
            position_value.get("data").ok_or_else(invalid_account_set)?,
        )?;
        let fields = decode_position_fields(wallet.address(), &data)?;
        let pool = main_pool_address(
            &crate::jlp::PERPS_PROGRAM_ID
                .parse::<solana_pubkey3::Pubkey>()
                .map_err(|_| invalid_account_set())?,
        )
        .to_string();
        let expected_market_custody = custody_address(&pool, market_mint)?;
        let expected_collateral_custody = custody_address(&pool, collateral_mint)?;
        if fields.pool != pool
            || fields.custody != expected_market_custody
            || fields.collateral_custody != expected_collateral_custody
            || fields.side
                != if side == 1 {
                    fin_domain::PositionKind::DerivativeLong
                } else {
                    fin_domain::PositionKind::DerivativeShort
                }
            || derive_position_address(wallet.address(), market_mint, collateral_mint, side)?
                != position
        {
            return Err(invalid_account_set());
        }
        0
    } else {
        minimum_account_rent(wallet, POSITION_ACCOUNT_SPACE).await?
    };
    let request_lamports = minimum_account_rent(wallet, POSITION_REQUEST_ACCOUNT_SPACE).await?;
    Ok(CpiRent {
        request_lamports,
        position_lamports,
    })
}

fn custody_address(pool: &str, mint: &str) -> Result<String> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let pool = pool
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let mint = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool.as_ref(), mint.as_ref()],
        &program,
    )
    .0
    .to_string())
}

async fn minimum_account_rent(wallet: &SolanaWalletAdapter, space: usize) -> Result<u64> {
    wallet
        .runtime()
        .rpc()
        .request(
            "getMinimumBalanceForRentExemption",
            json!([space,{"commitment":"finalized"}]),
        )
        .await?
        .as_u64()
        .ok_or_else(bad_response)
}

// This verifier compares the complete decoded request against independently
// derived inputs; keeping those inputs explicit avoids hiding identity checks.
#[allow(clippy::too_many_arguments)]
async fn inspect_create_instruction(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    market: &str,
    funding_symbol: &str,
    side: u8,
    collateral_atoms: u64,
    size_usd_atoms: u64,
    trigger_price: u64,
    trigger_above: bool,
    expected_position: &str,
    expected_request: &str,
) -> Result<LimitCreateInstruction> {
    if frozen.fee_payer() != wallet.address() {
        return Err(unreviewed());
    }
    let instructions = frozen.instructions();
    let perps = instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if perps.len() != 1
        || instructions.iter().any(|ix| {
            ix.program_id != crate::jlp::PERPS_PROGRAM_ID
                && ix.program_id != "ComputeBudget111111111111111111111111111111"
                && ix.program_id != "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
                && ix.program_id != fin_solana::token::TOKEN_PROGRAM_ID
                && ix.program_id != fin_solana::token::TOKEN_2022_PROGRAM_ID
                && ix.program_id != "11111111111111111111111111111111"
        })
    {
        return Err(unreviewed());
    }
    let ix = perps[0];
    if ix.accounts.len() != 20 {
        return Err(unreviewed_detail("limit create pinned account count"));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(&ix.data_base64)
        .map_err(|_| bad_response())?;
    let (size, collateral, actual_side, price, above, counter, requested_at) =
        decode_limit_create_params(&data)?;
    if size != size_usd_atoms
        || collateral != collateral_atoms
        || actual_side != side
        || price != trigger_price
        || above > 1
        || (above == 1) != trigger_above
        || counter == 0
        || requested_at <= 0
    {
        return Err(unreviewed_detail(
            "limit create parameters differ from approved input",
        ));
    }
    let a = &ix.accounts;
    let market_mint = market_mint_for_symbol(market)?.to_owned();
    let collateral_mint = collateral_mint_for_market(market, side)?.to_owned();
    let funding_mint = token_mint_for_symbol(funding_symbol)?.to_owned();
    let pool = main_pool_address(
        &crate::jlp::PERPS_PROGRAM_ID
            .parse()
            .map_err(|_| invalid_account_set())?,
    )
    .to_string();
    let position = derive_position_address(wallet.address(), &market_mint, &collateral_mint, side)?;
    let request = position_request_pda(&position, counter, 1)?;
    if position != expected_position || request != expected_request {
        return Err(unreviewed_detail(
            "limit create response PDA differs from the pinned instruction",
        ));
    }
    let market_custody = derive_custody_address(&pool, &market_mint)?;
    let collateral_custody = derive_custody_address(&pool, &collateral_mint)?;
    let token_program = crate::adapter::token_program_for(&wallet.runtime(), &funding_mint).await?;
    let wallet_ata =
        fin_solana::associated_token_address(wallet.address(), &funding_mint, &token_program)?;
    let request_ata =
        fin_solana::associated_token_address(&request, &funding_mint, &token_program)?;
    let required_signers = frozen.required_signers();
    if !limit_create_signer_roles_valid(required_signers, a, wallet.address())
        || !limit_create_compiled_privileges_valid(a, frozen.fee_payer())
        || a[2].address != wallet.address()
        || a[3].address != wallet_ata
        || a[4].address != pda(&[b"perpetuals"])?
        || a[5].address != pool
        || a[6].address != position
        || a[7].address != request
        || a[8].address != request_ata
        || a[9].address != market_custody
        || a[12].address != collateral_custody
        || a[13].address != funding_mint
        || a[15].address != token_program
        || a[16].address != "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        || a[17].address != "11111111111111111111111111111111"
        || a[18].address != pda(&[b"__event_authority"])?.as_str()
        || a[19].address != crate::jlp::PERPS_PROGRAM_ID
    {
        return Err(unreviewed_detail(
            "limit create signer or account identity/privilege",
        ));
    }
    let pool_data = account_data(wallet, &pool).await?;
    let (pool_name, name_end) = decode_pool_name(&pool_data)?;
    if pool_name.as_bytes() != MAIN_POOL_NAME || pool_name_address(&pool_name)? != pool {
        return Err(invalid_account_set());
    }
    let pool_count = read_u32(&pool_data, name_end)? as usize;
    let pool_end = name_end + 4 + pool_count * 32;
    if pool_count == 0
        || pool_count > 32
        || pool_end > pool_data.len()
        || !pool_data[name_end + 4..pool_end]
            .chunks_exact(32)
            .any(|k| bs58::encode(k).into_string() == market_custody)
        || !pool_data[name_end + 4..pool_end]
            .chunks_exact(32)
            .any(|k| bs58::encode(k).into_string() == collateral_custody)
    {
        return Err(invalid_account_set());
    }
    let mdata = account_data(wallet, &market_custody).await?;
    let cdata = account_data(wallet, &collateral_custody).await?;
    // The pinned Custody layout stores OracleParams.oracleAccount at offset
    // 106 and dovesOracle at offset 320 (including Anchor's discriminator).
    if address_field(&mdata, 8)? != pool
        || address_field(&cdata, 8)? != pool
        || address_field(&mdata, 40)? != market_mint
        || address_field(&cdata, 40)? != collateral_mint
        || address_field(&mdata, 106)? != a[11].address
        || address_field(&mdata, 320)? != a[10].address
        || a[14].address != "11111111111111111111111111111111"
        || a[10].signer
        || a[10].writable
        || a[11].signer
        || a[11].writable
        || a[14].signer
        || a[14].writable
    {
        return Err(invalid_account_set());
    }
    let account_decimals = crate::adapter::token_decimals(&wallet.runtime(), &funding_mint).await?;
    if !frozen
        .missing_cosigners()
        .iter()
        .all(|cosigner| cosigner == &a[0].address || cosigner == &a[1].address)
        || a.iter().skip(3).any(|account| account.signer)
    {
        return Err(unreviewed());
    }
    Ok(LimitCreateInstruction {
        position,
        request,
        market_mint,
        collateral_mint,
        funding_mint,
        funding_decimals: account_decimals,
        wallet_token_account: a[3].address.clone(),
        request_token_account: a[8].address.clone(),
        token_program,
        counter,
    })
}

fn decode_limit_create_params(data: &[u8]) -> Result<(u64, u64, u8, u64, u8, u64, i64)> {
    if data.get(..8) != Some(instruction_discriminator("instantCreateLimitOrder").as_slice()) {
        return Err(unreviewed());
    }
    let mut cursor = BorshCursor::new(data.get(8..).ok_or_else(bad_response)?);
    let size = cursor.u64()?;
    let collateral = cursor.u64()?;
    let side = cursor.u8()?;
    let price = cursor.u64()?;
    let above = cursor.u8()?;
    let counter = cursor.u64()?;
    let requested_at = i64::from_le_bytes(cursor.take(8)?.try_into().map_err(|_| bad_response())?);
    cursor.finish()?;
    Ok((size, collateral, side, price, above, counter, requested_at))
}

pub(super) fn limit_create_signer_roles_valid(
    required_signers: &[String],
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
    owner: &str,
) -> bool {
    if required_signers.len() != 3 || accounts.len() != 20 {
        return false;
    }
    let keeper = &accounts[0].address;
    let api_keeper = &accounts[1].address;
    keeper != owner
        && api_keeper != owner
        && keeper != api_keeper
        && accounts[0].signer
        && accounts[1].signer
        && accounts[2].signer
        && accounts[2].address == owner
        && accounts.iter().all(|account| {
            !account.signer
                || account.address == *keeper
                || account.address == *api_keeper
                || account.address == owner
        })
        && required_signers.iter().any(|signer| signer == owner)
        && required_signers.iter().any(|signer| signer == keeper)
        && required_signers.iter().any(|signer| signer == api_keeper)
        && required_signers
            .iter()
            .all(|signer| signer == owner || signer == keeper || signer == api_keeper)
}

#[cfg(test)]
fn limit_create_privileges_valid(
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
) -> bool {
    if accounts.len() != 20 {
        return false;
    }
    const WRITABLE: [usize; 5] = [2, 3, 6, 7, 8];
    accounts
        .iter()
        .enumerate()
        .all(|(index, account)| account.writable == WRITABLE.contains(&index))
}

pub(super) fn limit_create_compiled_privileges_valid(
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
    fee_payer: &str,
) -> bool {
    compiled_privileges_valid(accounts, fee_payer, &[2, 3, 6, 7, 8])
}

pub(super) fn compiled_privileges_valid(
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
    fee_payer: &str,
    writable_indices: &[usize],
) -> bool {
    compiled_account_privileges_valid(accounts, fee_payer, &[0, 1, 2], writable_indices)
}

pub(super) fn compiled_account_privileges_valid(
    accounts: &[fin_solana::transaction::NormalizedAccountMeta],
    fee_payer: &str,
    signer_indices: &[usize],
    writable_indices: &[usize],
) -> bool {
    if accounts.is_empty()
        || signer_indices
            .iter()
            .chain(writable_indices)
            .any(|index| *index >= accounts.len())
        || !signer_indices
            .iter()
            .any(|index| accounts[*index].address == fee_payer)
    {
        return false;
    }
    accounts.iter().all(|account| {
        let inherited_signer = signer_indices
            .iter()
            .any(|signer_index| accounts[*signer_index].address == account.address);
        let inherited_writable = account.address == fee_payer
            || writable_indices
                .iter()
                .any(|writable_index| accounts[*writable_index].address == account.address);
        account.signer == inherited_signer && account.writable == inherited_writable
    })
}

pub(super) fn derive_custody_address(pool: &str, mint: &str) -> Result<String> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let pool = pool
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let mint = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool.as_ref(), mint.as_ref()],
        &program,
    )
    .0
    .to_string())
}

pub(super) fn pool_name_address(name: &str) -> Result<String> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    Ok(
        solana_pubkey3::Pubkey::find_program_address(&[b"pool", name.as_bytes()], &program)
            .0
            .to_string(),
    )
}

pub(super) fn operation_definition() -> Definition {
    Definition {
        name:OP_LIMIT_CREATE.into(),description:"Create a bounded Jupiter Perps long or short limit request; collateral remains in request escrow until keeper fill or cancellation.".into(),kind:OperationKind::Write,execution_actor:ExecutionActor::AgentInvocable,
        input_schema:json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string"},"market":{"enum":["SOL","ETH","BTC"]},"side":{"enum":["long","short"]},"funding_token":{"enum":["SOL","ETH","BTC","USDC"]},"collateral_amount_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"size_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"trigger_price_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"trigger_above_threshold":{"type":"boolean"},"maximum_protocol_fee_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_account_funding_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"}},"required":["network","cluster","wallet","market","side","funding_token","collateral_amount_atoms","size_usd_atoms","trigger_price_usd_atoms","trigger_above_threshold","maximum_protocol_fee_atoms","maximum_protocol_fee_usd_atoms","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false}),
        output_schema:json!({"type":"object","properties":{"state":{"enum":["request_submitted","keeper_pending","settled"]},"request":{"type":"string"},"position":{"type":"string"},"signature":{"type":"string"},"size_usd_atoms":{"type":"string"},"collateral_atoms":{"type":"string"}},"required":["state","request","position","size_usd_atoms","collateral_atoms"],"additionalProperties":false}),
        error_schema:json!({"type":"object","required":["code","stage","message","next_action"]}),
        examples:vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet","market":"SOL","side":"long","funding_token":"SOL","collateral_amount_atoms":"100000000","size_usd_atoms":"11000000","trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,"maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"100000","maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})],
        coverage:"The exact provider transaction is checked against pinned instantCreateLimitOrder accounts and arguments, simulated, durably identified before one execution request, and reconciled from finalized transaction plus PositionRequest evidence.".into(),
        completion:"A finalized request creation proves escrow placement only. The original operation remains pending until exact keeper fill or refund/cancellation evidence settles its escrow lifecycle.".into(),
        semantics:json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"instantCreateLimitOrder","escrowed_not_filled":true,"execute_endpoint":"/transaction/execute","provider_request_id":null,"uncertain_submission":"reconcile_saved_message_never_retry"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_permission_summary_uses_readable_asset_units() {
        assert_eq!(
            limit_permission_summary(
                "SOL",
                2,
                12_100_000,
                100_000_000,
                "SOL",
                9,
                "USDC",
                1_000,
                1_000,
                6,
                15_000,
                2_881_440,
            ),
            "Authorize Jupiter Perps SOL short limit requests up to $12.10 notional, locking up to 0.1 SOL from USDC. Protocol fees are bounded at $0.001 and 0.001 USDC; reserve up to 0.000015 SOL for network fees and 0.00288144 SOL for refundable account funding. Trigger price and direction stay fixed to this request."
        );
    }

    #[test]
    fn finalized_block_order_requires_unique_real_signatures() {
        // Agave `ConfirmedBlock::encode_with_options` emits `signatures` as a
        // top-level ordered Vec<String> for TransactionDetails::Signatures;
        // see https://github.com/anza-xyz/agave/blob/master/transaction-status/src/lib.rs.
        let signatures = vec![json!("placement"), json!("update"), json!("keeper-fill")];
        let indices = finalized_signature_indices(&signatures).unwrap();
        assert!(indices["placement"] < indices["update"]);
        assert!(indices["update"] < indices["keeper-fill"]);

        let duplicated = vec![json!("update"), json!("update")];
        assert!(finalized_signature_indices(&duplicated).is_none());
        assert!(finalized_signature_indices(&[json!(3)]).is_none());
    }

    fn fixture() -> (
        Vec<String>,
        Vec<fin_solana::transaction::NormalizedAccountMeta>,
    ) {
        let required = vec!["owner".into(), "api-keeper".into(), "keeper".into()];
        let writable = [2usize, 3, 6, 7, 8];
        let accounts = (0..20)
            .map(|index| fin_solana::transaction::NormalizedAccountMeta {
                address: match index {
                    0 => "keeper".into(),
                    1 => "api-keeper".into(),
                    2 => "owner".into(),
                    _ => format!("account-{index}"),
                },
                signer: index < 3,
                writable: writable.contains(&index),
            })
            .collect();
        (required, accounts)
    }

    #[test]
    fn limit_create_accepts_instruction_roles_independent_of_signer_order() {
        let (required, accounts) = fixture();
        assert!(limit_create_signer_roles_valid(
            &required, &accounts, "owner"
        ));
        assert!(limit_create_privileges_valid(&accounts));
        assert!(limit_create_compiled_privileges_valid(&accounts, "owner"));
    }

    #[test]
    fn compiled_privileges_allow_only_exact_payer_and_duplicate_key_union() {
        let (required, mut accounts) = fixture();
        accounts[4].address = "owner".into();
        accounts[4].signer = true;
        accounts[4].writable = true;
        assert!(limit_create_signer_roles_valid(
            &required, &accounts, "owner"
        ));
        assert!(limit_create_compiled_privileges_valid(&accounts, "owner"));

        accounts[9].writable = true;
        assert!(!limit_create_compiled_privileges_valid(&accounts, "owner"));
        accounts[9].writable = false;
        assert!(!limit_create_compiled_privileges_valid(
            &accounts,
            "unexpected-payer"
        ));
    }

    #[test]
    fn compiled_privilege_profiles_are_instruction_specific() {
        let (_, mut accounts) = fixture();
        for (index, account) in accounts.iter_mut().enumerate() {
            account.writable = [2, 3, 5, 7, 8].contains(&index);
        }
        assert!(compiled_privileges_valid(
            &accounts,
            "owner",
            &[2, 3, 5, 7, 8]
        ));
        assert!(!limit_create_compiled_privileges_valid(&accounts, "owner"));
        accounts[5].writable = false;
        accounts[6].writable = true;
        assert!(!compiled_privileges_valid(
            &accounts,
            "owner",
            &[2, 3, 5, 7, 8]
        ));
    }

    #[test]
    fn limit_create_rejects_extra_signers_wallet_alias_and_privilege_widening() {
        let (mut required, accounts) = fixture();
        required.push("unexpected".into());
        assert!(!limit_create_signer_roles_valid(
            &required, &accounts, "owner"
        ));

        let (required, mut accounts) = fixture();
        accounts[1].address = "owner".into();
        assert!(!limit_create_signer_roles_valid(
            &required, &accounts, "owner"
        ));

        let (_, mut accounts) = fixture();
        accounts[9].writable = true;
        assert!(!limit_create_privileges_valid(&accounts));
    }

    #[test]
    fn pinned_create_instruction_decodes_exact_economic_fields() {
        let mut data = instruction_discriminator("instantCreateLimitOrder").to_vec();
        data.extend_from_slice(&12_100_000u64.to_le_bytes());
        data.extend_from_slice(&100_000_000u64.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&150_000_000u64.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&7u64.to_le_bytes());
        data.extend_from_slice(&1_800_000_000i64.to_le_bytes());
        assert_eq!(
            decode_limit_create_params(&data).unwrap(),
            (12_100_000, 100_000_000, 1, 150_000_000, 1, 7, 1_800_000_000)
        );

        data[8..16].copy_from_slice(&12_100_001u64.to_le_bytes());
        assert_ne!(decode_limit_create_params(&data).unwrap().0, 12_100_000);
        data[0] ^= 1;
        assert!(decode_limit_create_params(&data).is_err());
    }

    #[test]
    fn limit_escrow_progress_transitions_from_pending_to_anchored_fill() {
        let wallet = resource_for_wallet("synthetic-wallet");
        let escrow =
            ResourceId("solana:mainnet-beta:jupiter-perps-request:synthetic-request".into());
        let asset = asset("synthetic-usdc", 6);
        let margin = margin_usd_asset();
        let expected_margin = margin.clone();
        let position = String::from("synthetic-position:collateral");
        let open = limit_escrow_progress(
            wallet.clone(),
            escrow.clone(),
            asset.clone(),
            11_000_000,
            11_000_000,
            0,
            0,
            11_000_000,
            wallet.clone(),
            position.clone(),
            fin_domain::PositionKind::LockedCollateral,
            margin.clone(),
            0,
            fin_domain::EscrowLifecycleState::Open,
            None,
            0,
            None,
        );
        let anchor = fin_domain::PositionAnchor {
            resource: wallet.clone(),
            position: "synthetic-position".into(),
            position_kind: fin_domain::PositionKind::DerivativeLong,
            principal: Amount {
                asset: crate::pricing::position_usd_asset(market_mint_for_symbol("SOL").unwrap())
                    .unwrap(),
                atoms: fin_domain::Quantity::from_u64(12_100_000),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"source":"synthetic_finalized_keeper"}),
            observed_at: Utc::now().to_rfc3339(),
        };
        let collateral_anchor = fin_domain::PositionAnchor {
            resource: wallet.clone(),
            position: position.clone(),
            position_kind: fin_domain::PositionKind::LockedCollateral,
            principal: Amount {
                asset: margin.clone(),
                atoms: fin_domain::Quantity::from_u64(10_999_000),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"source":"synthetic_finalized_keeper"}),
            observed_at: Utc::now().to_rfc3339(),
        };
        let filled = limit_escrow_progress(
            wallet.clone(),
            escrow,
            asset,
            11_000_000,
            11_000_000,
            11_000_000,
            0,
            0,
            wallet,
            position,
            fin_domain::PositionKind::LockedCollateral,
            margin,
            10_999_000,
            fin_domain::EscrowLifecycleState::Filled,
            Some(collateral_anchor),
            12_100_000,
            Some(anchor),
        );
        let fin_domain::FinancialEffect::EscrowPositionProgress {
            escrow_deposit,
            input_filled,
            input_refunded,
            remaining_input,
            state,
            position,
            position_asset,
            position_change,
            conditional_position_change,
            conditional_position_anchor,
            ..
        } = open
        else {
            panic!("open escrow effect")
        };
        assert_eq!(escrow_deposit.as_str(), "11000000");
        assert_eq!(input_filled.as_str(), "0");
        assert_eq!(input_refunded.as_str(), "0");
        assert_eq!(remaining_input.as_str(), "11000000");
        assert_eq!(state, fin_domain::EscrowLifecycleState::Open);
        assert_eq!(position, "synthetic-position:collateral");
        assert_eq!(position_asset, expected_margin);
        assert!(
            position_change.is_zero()
                && conditional_position_change.is_zero()
                && conditional_position_anchor.is_none()
        );
        let fin_domain::FinancialEffect::EscrowPositionProgress {
            escrow_deposit,
            input_filled,
            input_refunded,
            remaining_input,
            state,
            position,
            position_asset,
            position_change,
            conditional_position_change,
            conditional_position_anchor,
            position_anchor,
            ..
        } = filled
        else {
            panic!("filled escrow effect")
        };
        assert_eq!(escrow_deposit.as_str(), "11000000");
        assert_eq!(input_filled.as_str(), "11000000");
        assert_eq!(input_refunded.as_str(), "0");
        assert_eq!(remaining_input.as_str(), "0");
        assert_eq!(state, fin_domain::EscrowLifecycleState::Filled);
        assert_eq!(position, "synthetic-position:collateral");
        assert_eq!(position_asset, expected_margin);
        assert_eq!(position_change.as_str(), "10999000");
        assert_eq!(conditional_position_change.as_str(), "12100000");
        assert_eq!(
            position_anchor.unwrap().principal.atoms.as_str(),
            "10999000"
        );
        assert_eq!(
            conditional_position_anchor
                .unwrap()
                .principal
                .atoms
                .as_str(),
            "12100000"
        );
    }

    #[test]
    fn no_fill_cancel_refunds_original_escrow_without_position_credit() {
        let wallet = resource_for_wallet("synthetic-wallet");
        let effect = limit_escrow_progress(
            wallet.clone(),
            ResourceId("solana:mainnet-beta:jupiter-perps-request:synthetic-request".into()),
            asset("synthetic-usdc", 6),
            11_000_000,
            11_000_000,
            0,
            11_000_000,
            0,
            wallet,
            "synthetic-position:collateral".into(),
            fin_domain::PositionKind::LockedCollateral,
            margin_usd_asset(),
            0,
            fin_domain::EscrowLifecycleState::Cancelled,
            None,
            0,
            None,
        );
        let fin_domain::FinancialEffect::EscrowPositionProgress {
            escrow_deposit,
            input_filled,
            input_refunded,
            remaining_input,
            state,
            position_change,
            conditional_position_change,
            position_anchor,
            conditional_position_anchor,
            ..
        } = effect
        else {
            panic!("cancel escrow effect")
        };
        assert_eq!(escrow_deposit.as_str(), "11000000");
        assert_eq!(input_filled.as_str(), "0");
        assert_eq!(input_refunded.as_str(), "11000000");
        assert_eq!(remaining_input.as_str(), "0");
        assert_eq!(state, fin_domain::EscrowLifecycleState::Cancelled);
        assert!(position_change.is_zero() && conditional_position_change.is_zero());
        assert!(position_anchor.is_none() && conditional_position_anchor.is_none());
    }

    #[test]
    fn first_terminal_observation_preserves_exact_request_and_position_rent() {
        let binding = json!({
            "request":"request-pda",
            "position":"position-pda",
            "request_rent_lamports":"2881440",
            "position_rent_lamports":"2352480",
            "funding_accounts":[{"address":"aux-account","payer":"wallet","lamports":2039280}],
            "closed_wrapped_native_accounts":[]
        });
        let record = OperationRecord {
            id: "create-operation".into(),
            request_id: "request-id".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_LIMIT_CREATE.into(),
            input: json!({}),
            status: Status::Pending,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let effects = terminal_account_funding_progress(
            &record,
            &binding,
            "wallet",
            "request-pda",
            "placement-signature",
            7,
            "placement-message-hash",
            "cancel-signature",
            8,
            "cancel-message-hash",
            None,
        )
        .unwrap();
        assert_eq!(effects.len(), 3);
        let mut rows = effects
            .iter()
            .map(|effect| match effect {
                fin_domain::FinancialEffect::AccountFundingProgress {
                    position,
                    account,
                    funded,
                    returned,
                    remaining,
                    state,
                    anchor,
                    ..
                } => (
                    position.as_str(),
                    account.as_str(),
                    funded.as_str(),
                    returned.as_str(),
                    remaining.as_str(),
                    *state,
                    anchor.as_ref().map(|value| value.principal.atoms.as_str()),
                ),
                _ => panic!("expected cumulative account-funding effect"),
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.1);
        assert_eq!(
            rows[0],
            (
                "account-funding:aux-account",
                "aux-account",
                "2039280",
                "0",
                "2039280",
                fin_domain::AccountFundingState::Open,
                Some("2039280")
            )
        );
        assert_eq!(
            rows[1],
            (
                "position-pda:rent",
                "position-pda",
                "2352480",
                "0",
                "2352480",
                fin_domain::AccountFundingState::Open,
                Some("2352480")
            )
        );
        assert_eq!(
            rows[2],
            (
                "account-funding:request-pda",
                "request-pda",
                "2881440",
                "2881440",
                "0",
                fin_domain::AccountFundingState::Closed,
                Some("0")
            )
        );
    }

    #[test]
    fn native_request_ata_refund_proof_separates_rent_from_wrapped_sol() {
        let ata = "request-ata";
        let owner = "request-pda";
        let amount = 100_000_000u64;
        let rent = 2_039_280u64;
        let mint = crate::swap::WRAPPED_SOL_MINT;
        let proof = FinalizedReceiptProof {
            signature: "synthetic-cancel-signature".into(),
            slot: 9,
            fee_lamports: 0,
            failed: false,
            message_sha256: "11".repeat(32),
            account_keys: vec![ata.into()],
            message_instructions: Vec::new(),
            required_signatures: Vec::new(),
            pre_balances_lamports: vec![amount + rent],
            post_balances_lamports: vec![0],
            pre_token_balances: vec![
                json!({"accountIndex":0,"mint":mint,"owner":owner,"programId":fin_solana::token::TOKEN_PROGRAM_ID,"uiTokenAmount":{"amount":amount.to_string(),"decimals":9}}),
            ],
            post_token_balances: Vec::new(),
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: Vec::new(),
            log_messages: Vec::new(),
            error: None,
        };
        let binding = json!({
            "request":"request-pda",
            "request_token_account":ata,
            "funding_mint":mint,
            "token_program":fin_solana::token::TOKEN_PROGRAM_ID,
            "funding_decimals":9,
            "collateral_atoms":amount.to_string()
        });
        let spec = LimitFundingAccount {
            position: format!("account-funding:{ata}"),
            account: ata.into(),
            amount: fin_domain::Quantity::from_u64(rent),
        };
        assert!(account_funding_closed_in_proof(Some(&proof), &binding, &spec).unwrap());

        let mut wrong = proof;
        wrong.post_balances_lamports[0] = rent;
        assert!(account_funding_closed_in_proof(Some(&wrong), &binding, &spec).is_err());
    }
}
