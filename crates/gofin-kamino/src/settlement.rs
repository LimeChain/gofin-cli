//! Finalized vault share accounting; underlying composition is separate evidence.
use crate::{
    adapter::{
        asset, atoms, invalid, native_asset, owned_shares, rent_limits, resource, validate_position,
    },
    protocol::*,
};
use chrono::Utc;
use gofin_domain::*;
use gofin_pricing::UsdPriceSource;
use gofin_solana::{FinalizedReceiptProof, transaction::ExpiredWithoutInclusionProof};
use num_bigint::BigInt;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn position_id(vault: &VaultInfo) -> String {
    format!("kamino:{}:{}:shares", vault.family.as_str(), vault.address)
}
pub(crate) fn position_kind(family: VaultFamily) -> PositionKind {
    match family {
        VaultFamily::Lending => PositionKind::Investment,
        VaultFamily::Liquidity => PositionKind::Liquidity,
    }
}
pub(crate) fn snapshot_position(position: &PositionInfo) -> Result<PositionState> {
    validate_position(position, &position.vault, &position.owner)?;
    let evidence = VaultPositionEvidence {
        version: 1,
        family: position.vault.family.as_str().into(),
        vault: position.vault.address.clone(),
        share_asset: asset(&position.vault.shares),
        share_token_program: position.vault.shares.token_program.clone(),
        wallet_shares_atoms: atoms(&position.wallet_shares_atoms)?,
        staked_shares_atoms: atoms(&position.staked_shares_atoms)?,
        pending_shares_atoms: atoms(&position.pending_shares_atoms)?,
        underlying: position
            .underlying
            .iter()
            .map(|row| {
                Ok(LiquidityCompositionAmount {
                    asset: asset(&row.token),
                    token_program: row.token.token_program.clone(),
                    atoms: row.atoms.as_deref().map(atoms).transpose()?,
                })
            })
            .collect::<Result<_>>()?,
        historical_principal_known: false,
        observed_slot: position.vault.observed_slot,
        observed_at: position.vault.observed_at.clone(),
        backing_valuation: None,
        anchor_position_id: Some(position_id(&position.vault)),
        pre_transaction_shares_atoms: None,
        finalized_submission: None,
        shares_delta_atoms: None,
    };
    let amount = Amount {
        asset: asset(&position.vault.shares),
        atoms: owned_shares(position)?,
    };
    let resource = resource(&position.owner);
    let id = position_id(&position.vault);
    let kind = position_kind(position.vault.family);
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: id.clone(),
        position_kind: kind.clone(),
        principal: amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"version":1,"source":"kamino_position_observation","vault_position":evidence,"historical_principal_known":false,"protocol_state":position.protocol_state}),
        observed_at: position.vault.observed_at.clone(),
    };
    Ok(PositionState {
        resource,
        position: id,
        kind,
        amount,
        scaled_receipt: None,
        anchor: Some(anchor),
    })
}
pub(crate) async fn value_position(
    position: &mut PositionState,
    prices: &dyn UsdPriceSource,
) -> Result<()> {
    let Some(anchor) = &mut position.anchor else {
        return Ok(());
    };
    let mut evidence: VaultPositionEvidence =
        serde_json::from_value(anchor.evidence["vault_position"].clone())
            .map_err(|_| invalid("Invalid vault position evidence."))?;
    let assets: Vec<_> = evidence
        .underlying
        .iter()
        .map(|r| r.asset.clone())
        .collect();
    if evidence.underlying.iter().any(|r| r.atoms.is_none()) {
        return Ok(());
    }
    let Ok(quotes) = prices.quotes(&assets).await else {
        return Ok(());
    };
    if quotes.len() != assets.len() {
        return Ok(());
    }
    let mut total = Quantity::zero();
    let mut sources = BTreeSet::new();
    let mut observed = None;
    for (row, quote) in evidence.underlying.iter().zip(quotes) {
        let Ok(price) = Quantity::from_decimal(&quote.usd_per_unit, 36) else {
            return Ok(());
        };
        if price.is_zero() {
            return Ok(());
        }
        let unit = Quantity::new(format!(
            "1{}",
            "0".repeat(usize::from(row.asset.decimals) + 30)
        ))?;
        total = total.checked_add(
            &row.atoms
                .as_ref()
                .ok_or_else(|| invalid("Unknown backing amount."))?
                .mul_div_ceil(&price, &unit)?,
        )?;
        sources.insert(quote.source);
        observed = Some(observed.map_or(quote.updated_at.clone(), |old: String| {
            old.min(quote.updated_at)
        }));
    }
    let valuation_scope = anchor.evidence["protocol_state"].get("valuation");
    evidence.backing_valuation = Some(VaultBackingValuation {
        total_usd_micros: total,
        observed_at: observed.unwrap_or_else(|| Utc::now().to_rfc3339()),
        source: sources.into_iter().collect::<Vec<_>>().join(";"),
        complete: valuation_scope.is_none_or(|value| value["complete"].as_bool() == Some(true)),
        basis: valuation_scope
            .and_then(|value| value["basis"].as_str())
            .unwrap_or("verified_underlying_exposure")
            .into(),
        excluded_components: valuation_scope
            .and_then(|value| value["excluded"].as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    });
    anchor.evidence["vault_position"] =
        serde_json::to_value(evidence).map_err(|_| invalid("Cannot serialize vault valuation."))?;
    Ok(())
}
fn proof_identity(submission: &Submission, proof: &FinalizedReceiptProof, cap: u64) -> Result<()> {
    if submission.kind != "solana.transaction"
        || submission.reference != format!("solana:message:{}", proof.message_sha256)
        || proof.message_sha256.is_empty()
        || submission.data["message_sha256"].as_str() != Some(proof.message_sha256.as_str())
        || submission.data["wallet_signature"].as_str() != Some(proof.signature.as_str())
        || proof.fee_lamports > cap
    {
        return Err(invalid(
            "Finalized vault proof does not match the immutable submission or fee cap.",
        ));
    }
    Ok(())
}
fn network_cost(owner: &str, fee: u64) -> FinancialEffect {
    FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: resource(owner),
        },
        amount: Amount {
            asset: native_asset(),
            atoms: Quantity::from_u64(fee),
        },
        category: "network_fee".into(),
        treatment: CostTreatment::AdditionalDebit,
    }
}
fn bundle(
    submission: &Submission,
    effects: Vec<FinancialEffect>,
    evidence: Value,
) -> Result<VerifiedEffects> {
    let mut result = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    result.proof_digest = verified_effect_digest(&result)?;
    Ok(result)
}
pub(crate) fn verify_not_dispatched(
    submission: &Submission,
    reason_code: &str,
    reason: &str,
) -> Result<VerifiedEffects> {
    bundle(
        submission,
        vec![],
        json!({"state":"not_dispatched","reason_code":reason_code,"reason":reason,"financial_effects":[]}),
    )
}
pub(crate) fn verify_expired_without_inclusion(
    submission: &Submission,
    signature: &str,
    message_sha256: &str,
    reason: &str,
    proof: &ExpiredWithoutInclusionProof,
) -> Result<VerifiedEffects> {
    if submission.kind != "solana.transaction"
        || submission.reference != format!("solana:message:{message_sha256}")
        || submission.data["message_sha256"].as_str() != Some(message_sha256)
        || submission.data["wallet_signature"].as_str() != Some(signature)
        || submission.data["recent_blockhash"].as_str() != Some(proof.recent_blockhash.as_str())
        || submission.data["last_valid_block_height"].as_u64()
            != Some(proof.last_valid_block_height)
        || proof.evidence_version != 1
        || proof.minimum_rpc_origins_per_block != 2
        || proof.finalized_block_height <= proof.last_valid_block_height
    {
        return Err(invalid(
            "Expired vault evidence does not match the immutable submitted message and lifetime.",
        ));
    }
    // The runtime proved complete finalized block coverage. The transaction
    // may have been sent; this never attests to absence of a network send.
    bundle(
        submission,
        vec![],
        json!({"state":"expired_without_inclusion","signature":signature,"message_sha256":message_sha256,"reason":reason,"proof":proof,"financial_effects":[]}),
    )
}
pub(crate) fn verify_failed(
    submission: &Submission,
    proof: &FinalizedReceiptProof,
    owner: &str,
    cap: u64,
) -> Result<VerifiedEffects> {
    proof_identity(submission, proof, cap)?;
    if !proof.failed {
        return Err(invalid("Expected a finalized failed transaction."));
    }
    bundle(
        submission,
        if proof.fee_lamports == 0 {
            vec![]
        } else {
            vec![network_cost(owner, proof.fee_lamports)]
        },
        json!({"failed_transaction":true,"proof":proof}),
    )
}

fn validate_share_limits(intent: &VaultIntent, verified: &VerifiedVaultEffects) -> Result<()> {
    let native_delta = verified
        .native_shares_delta
        .as_ref()
        .unwrap_or(&verified.shares_delta);
    match intent.action {
        VaultAction::Deposit => {
            if native_delta.is_negative()
                || native_delta.is_zero()
                || intent
                    .minimum_shares_atoms
                    .as_deref()
                    .map(atoms)
                    .transpose()?
                    .is_some_and(|floor| native_delta.magnitude() < floor)
            {
                return Err(invalid(
                    "Finalized native share receipt is invalid or below the approved minimum.",
                ));
            }
            if verified.shares_delta.is_negative() || verified.shares_delta.is_zero() {
                return Err(invalid(
                    "Finalized deposit did not prove positive redeemable share ownership.",
                ));
            }
        }
        VaultAction::Withdraw => {
            let maximum = atoms(
                intent
                    .shares_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Withdrawal share quantity missing."))?,
            )?;
            if !native_delta.is_negative() || native_delta.magnitude() != maximum {
                return Err(invalid(
                    "Finalized native redeemed shares differ from the approved exact quantity.",
                ));
            }
            if !verified.shares_delta.is_negative() || verified.shares_delta.magnitude() > maximum {
                return Err(GoFinError::new(
                    "KAMINO_SHARE_ENTITLEMENT_PENDING",
                    "reconciliation",
                    "The finalized farm entitlement change is outside the reviewed share reduction.",
                    "Keep the immutable submission pending and inspect the farm entitlement evidence; do not replay the withdrawal.",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn verify(
    record: &OperationRecord,
    intent: &VaultIntent,
    candidate: &VaultCandidate,
    verified: &VerifiedVaultEffects,
    proof: &FinalizedReceiptProof,
) -> Result<VerifiedEffects> {
    let submission = record
        .submission
        .as_ref()
        .ok_or_else(|| invalid("Missing durable vault submission."))?;
    proof_identity(submission, proof, intent.maximum_network_fee_lamports)?;
    if proof.failed {
        return Err(invalid("A failed transaction cannot settle vault shares."));
    }
    let prepared = record
        .preview
        .as_ref()
        .ok_or_else(|| invalid("Missing durable preview."))?;
    let owner = prepared.binding["wallet_address"]
        .as_str()
        .ok_or_else(|| invalid("Missing wallet identity."))?;
    if proof.required_signatures.len() != 1
        || proof.required_signatures.first() != Some(&proof.signature)
        || proof.account_keys.first().map(String::as_str) != Some(owner)
        || prepared.binding["message_sha256"].as_str() != Some(proof.message_sha256.as_str())
        || !gofin_solana::transaction::normalized_instruction_graph_matches(
            &candidate.instructions,
            &proof.message_instructions,
            owner,
        )
    {
        return Err(invalid(
            "Finalized signer or instruction graph differs from the approved native vault transaction.",
        ));
    }
    let mut effects = Vec::new();
    let resource = resource(owner);
    let native = native_asset();
    let observed = proof.all_owner_token_deltas_verified(owner)?;
    let mut reported = BTreeMap::new();
    for row in &verified.token_deltas {
        if !candidate.vault.assets.contains(&row.token)
            || reported.insert(row.token.mint.clone(), row).is_some()
        {
            return Err(invalid(
                "Protocol proof contains duplicate or unapproved underlying assets.",
            ));
        }
    }
    for row in &observed {
        if row.mint == candidate.vault.shares.mint {
            if row.decimals != candidate.vault.shares.decimals
                || row.token_program != candidate.vault.shares.token_program
            {
                return Err(invalid("Finalized share mint identity mismatch."));
            }
            continue;
        }
        if row.atoms.is_zero() {
            continue;
        }
        let token = candidate
            .vault
            .assets
            .iter()
            .find(|t| {
                t.mint == row.mint
                    && t.decimals == row.decimals
                    && t.token_program == row.token_program
            })
            .ok_or_else(|| invalid("Finalized transaction changed an unapproved wallet token."))?;
        let claimed = reported
            .get(&token.mint)
            .ok_or_else(|| invalid("Protocol proof omitted a finalized wallet cashflow."))?;
        if claimed.atoms != row.atoms {
            return Err(invalid(
                "Protocol cashflow differs from finalized owner token balances.",
            ));
        }
    }
    for token in &candidate.vault.assets {
        let delta =
            proof.token_delta_verified(owner, &token.mint, &token.token_program, token.decimals)?;
        if reported
            .get(&token.mint)
            .is_some_and(|row| row.atoms != delta)
            || (!delta.is_zero() && !reported.contains_key(&token.mint))
        {
            return Err(invalid(
                "Reported underlying cashflows do not match the receipt.",
            ));
        }
        match intent.action {
            VaultAction::Deposit => {
                if !delta.is_negative() && !delta.is_zero() {
                    return Err(invalid(
                        "A native deposit unexpectedly credited an underlying wallet asset.",
                    ));
                }
                let maximum = intent
                    .amounts
                    .iter()
                    .find(|r| r.token == *token)
                    .and_then(|r| r.atoms.as_deref())
                    .ok_or_else(|| invalid("Missing deposit cap."))?;
                if delta.magnitude() > atoms(maximum)? {
                    return Err(invalid("Finalized underlying debit exceeded approval."));
                }
                if !delta.is_zero() {
                    effects.push(FinancialEffect::CashDebit {
                        resource: resource.clone(),
                        amount: Amount {
                            asset: asset(token),
                            atoms: delta.magnitude(),
                        },
                    });
                }
            }
            VaultAction::Withdraw => {
                if delta.is_negative() {
                    return Err(invalid(
                        "A native withdrawal debited underlying wallet assets.",
                    ));
                }
                if !delta.is_zero() {
                    effects.push(FinancialEffect::UsableCredit {
                        to: resource.clone(),
                        amount: Amount {
                            asset: asset(token),
                            atoms: delta.magnitude(),
                        },
                    });
                }
            }
        }
    }
    validate_share_limits(intent, verified)?;
    let before = atoms(
        candidate.metadata["position_shares_before_atoms"]
            .as_str()
            .ok_or_else(|| invalid("Share prestate missing."))?,
    )?;
    let after = if verified.shares_delta.is_negative() {
        before.checked_sub(&verified.shares_delta.magnitude())?
    } else {
        before.checked_add(&verified.shares_delta.magnitude())?
    };
    let position = verified.position.as_ref().ok_or_else(|| {
        invalid("Finalized share settlement requires a complete position observation.")
    })?;
    validate_position(position, &candidate.vault, owner)?;
    if owned_shares(position)? != after
        || position
            .vault
            .observed_slot
            .is_none_or(|slot| slot < proof.slot)
    {
        return Err(invalid(
            "Post-transaction position evidence does not match the proven share change.",
        ));
    }
    let mut state = snapshot_position(position)?;
    let anchor = state
        .anchor
        .as_mut()
        .ok_or_else(|| invalid("Missing share position anchor."))?;
    let mut typed: VaultPositionEvidence =
        serde_json::from_value(anchor.evidence["vault_position"].clone())
            .map_err(|_| invalid("Invalid share evidence."))?;
    typed.pre_transaction_shares_atoms = Some(before);
    typed.finalized_submission = Some(submission.clone());
    typed.shares_delta_atoms = Some(verified.shares_delta.clone());
    anchor.evidence["vault_position"] = json!(typed);
    anchor.evidence["source"] = json!("finalized_kamino_share_delta");
    anchor.evidence["protocol_event"] = verified.evidence.clone();
    anchor.evidence["native_shares_delta"] = json!(verified.native_shares_delta);
    anchor.evidence["signature"] = json!(proof.signature);
    anchor.evidence["slot"] = json!(proof.slot);
    effects.push(FinancialEffect::PositionChange {
        resource: resource.clone(),
        position: state.position,
        asset: asset(&candidate.vault.shares),
        delta: verified.shares_delta.clone(),
        scaled_receipt: None,
        anchor: state.anchor,
    });
    let limits = rent_limits(candidate)?;
    let mut seen = BTreeSet::new();
    let mut rent = BigInt::from(0);
    let mut funding = Quantity::zero();
    for row in &verified.rent_accounts {
        gofin_solana::parse_address(&row.account)?;
        if !seen.insert(&row.account) || row.atoms.is_negative() {
            return Err(invalid(
                "Unapproved or duplicate rent refund; native vault operations only authorize reviewed account creation.",
            ));
        }
        let id = format!("account-funding:{}", row.account);
        let cap = limits
            .iter()
            .find(|cap| cap.position == id)
            .ok_or_else(|| invalid("Unapproved rent account."))?;
        if row.atoms.magnitude() > cap.maximum_amount
            || proof.lamport_delta(&row.account)? != row.atoms
        {
            return Err(invalid(
                "Rent account funding does not match the finalized proof and cap.",
            ));
        }
        rent += row.atoms.integer();
        funding = funding.checked_add(&row.atoms.magnitude())?;
        if row.atoms.is_zero() {
            continue;
        }
        let amount = Amount {
            asset: native.clone(),
            atoms: row.atoms.magnitude(),
        };
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: id.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
            principal: amount.clone(),
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"version":1,"source":"finalized_kamino_account_funding","account":row.account,"signature":proof.signature,"slot":proof.slot}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: id.clone(),
            asset: native.clone(),
            delta: row.atoms.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        effects.push(FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            to: AccountingLocation::Position {
                resource: resource.clone(),
                position: id,
                position_kind: PositionKind::RefundableAccountFunding,
            },
            debit: amount.clone(),
            credit: amount,
        });
    }
    if rent != verified.rent_lamports_delta.integer()
        || funding > Quantity::from_u64(intent.maximum_account_funding_lamports)
        || proof.lamport_delta(owner)?.integer() != -BigInt::from(proof.fee_lamports) - rent
    {
        return Err(invalid(
            "Finalized native balance change is not explained by the bounded fee and refundable account funding.",
        ));
    }
    if !funding.is_zero() {
        effects.push(FinancialEffect::CashDebit {
            resource,
            amount: Amount {
                asset: native,
                atoms: funding,
            },
        });
    }
    if proof.fee_lamports > 0 {
        effects.push(network_cost(owner, proof.fee_lamports));
    }
    bundle(
        submission,
        effects,
        json!({"source":"finalized_kamino_vault_transaction","proof":proof,"protocol_evidence":verified.evidence}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_non_inclusion_binds_original_message_and_lifetime_without_claiming_no_send() {
        let hash = "a".repeat(64);
        let signature = bs58::encode([1u8; 64]).into_string();
        let recent = bs58::encode([42u8; 32]).into_string();
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: format!("solana:message:{hash}"),
            data: json!({"schema_version":2,"message_sha256":hash,"wallet_signature":signature,"recent_blockhash":recent,"last_valid_block_height":250}),
        };
        let proof = ExpiredWithoutInclusionProof {
            evidence_version: 1,
            minimum_rpc_origins_per_block: 2,
            recent_blockhash: recent,
            last_valid_block_height: 250,
            finalized_context_slot: 1151,
            finalized_block_height: 251,
            first_slot: 1000,
            last_slot: 1151,
            first_block_height: 100,
            last_block_height: 251,
            checked_blocks: 152,
            block_coverage_sha256: "b".repeat(64),
        };
        let verified = verify_expired_without_inclusion(
            &submission,
            &signature,
            &hash,
            "Complete finalized history excludes this signature.",
            &proof,
        )
        .unwrap();
        assert!(verified.effects.is_empty());
        assert_eq!(verified.submission, submission);
        assert_eq!(
            verified.proof_digest,
            verified_effect_digest(&verified).unwrap()
        );
        assert_eq!(verified.evidence["state"], "expired_without_inclusion");
        assert!(verified.evidence.get("no_network_send").is_none());
        for (field, value) in [
            ("message_sha256", json!("other")),
            ("wallet_signature", json!("other")),
            ("recent_blockhash", json!("other")),
            ("last_valid_block_height", json!(251)),
        ] {
            let mut changed = submission.clone();
            changed.data[field] = value;
            assert!(
                verify_expired_without_inclusion(&changed, &signature, &hash, "synthetic", &proof)
                    .is_err()
            );
        }
        let mut early = proof;
        early.minimum_rpc_origins_per_block = 1;
        assert!(
            verify_expired_without_inclusion(&submission, &signature, &hash, "synthetic", &early)
                .is_err()
        );
        early.minimum_rpc_origins_per_block = 2;
        early.finalized_block_height = 250;
        assert!(
            verify_expired_without_inclusion(&submission, &signature, &hash, "synthetic", &early)
                .is_err()
        );
    }
    struct Prices(Option<&'static str>);
    #[async_trait::async_trait]
    impl UsdPriceSource for Prices {
        async fn quote(&self, _: &AssetId) -> Result<gofin_pricing::UsdQuote> {
            Ok(gofin_pricing::UsdQuote {
                usd_per_unit: self
                    .0
                    .ok_or_else(|| invalid("Missing controlled price."))?
                    .into(),
                updated_at: "2026-10-18T12:00:00Z".into(),
                observed_at: "2026-10-18T12:00:01Z".into(),
                source: "controlled exact price".into(),
            })
        }
    }
    fn position() -> PositionInfo {
        let token = |mint: &str| TokenInfo {
            mint: mint.into(),
            decimals: 6,
            token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        };
        let underlying = token("So11111111111111111111111111111111111111112");
        PositionInfo {
            vault: VaultInfo {
                family: VaultFamily::Lending,
                address: "Stake11111111111111111111111111111111111111".into(),
                program: "11111111111111111111111111111111".into(),
                name: None,
                backend: None,
                assets: vec![underlying.clone()],
                shares: token("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
                farm: Some("Vote111111111111111111111111111111111111111".into()),
                deposits_available: true,
                withdrawals_available: true,
                restrictions: vec![],
                observed_slot: Some(10),
                observed_at: Utc::now().to_rfc3339(),
                metrics: json!({}),
                protocol_state: json!({}),
            },
            owner: "11111111111111111111111111111111".into(),
            wallet_shares_atoms: "20".into(),
            staked_shares_atoms: "30".into(),
            pending_shares_atoms: "5".into(),
            underlying: vec![TokenAmount {
                token: underlying,
                atoms: None,
            }],
            rewards: vec![],
            withdrawable_shares_atoms: Some("50".into()),
            complete: false,
            warnings: vec!["Backing observation missing".into()],
            protocol_state: json!({}),
        }
    }
    #[test]
    fn imported_share_custody_is_one_position_with_unknown_backing_and_basis() {
        let state = snapshot_position(&position()).unwrap();
        assert_eq!(state.amount.atoms, Quantity::from_u64(55));
        assert_eq!(state.kind, PositionKind::Investment);
        let evidence: VaultPositionEvidence =
            serde_json::from_value(state.anchor.unwrap().evidence["vault_position"].clone())
                .unwrap();
        assert!(!evidence.historical_principal_known);
        assert_eq!(evidence.underlying[0].atoms, None);
        assert_eq!(evidence.wallet_shares_atoms, Quantity::from_u64(20));
        assert_eq!(evidence.staked_shares_atoms, Quantity::from_u64(30));
        assert_eq!(evidence.pending_shares_atoms, Quantity::from_u64(5));
        assert!(evidence.backing_valuation.is_none());
    }
    #[tokio::test]
    async fn backing_valuation_preserves_exact_fractional_usd_and_missing_prices() {
        let mut observed = position();
        observed.underlying[0].atoms = Some("1000001".into());
        let mut state = snapshot_position(&observed).unwrap();
        value_position(&mut state, &Prices(Some("1.25")))
            .await
            .unwrap();
        let value = &state.anchor.as_ref().unwrap().evidence["vault_position"]["backing_valuation"];
        assert_eq!(value["total_usd_micros"], "1250002");
        assert_eq!(value["observed_at"], "2026-10-18T12:00:00Z");
        assert_eq!(value["basis"], "verified_underlying_exposure");
        assert_eq!(value["complete"], true);
        assert_eq!(state.amount.atoms, Quantity::from_u64(55));
        for price in [Prices(None), Prices(Some("0")), Prices(Some("invalid"))] {
            let mut missing = snapshot_position(&observed).unwrap();
            value_position(&mut missing, &price).await.unwrap();
            assert!(
                missing.anchor.as_ref().unwrap().evidence["vault_position"]["backing_valuation"]
                    .is_null()
            );
            assert_eq!(missing.amount.atoms, Quantity::from_u64(55));
        }
    }
    #[tokio::test]
    async fn partial_backing_value_retains_protocol_scope_and_excluded_components() {
        let mut observed = position();
        observed.underlying[0].atoms = Some("1000001".into());
        observed.protocol_state = json!({"valuation":{"basis":"underlying_principal_only","complete":false,"excluded":["unclaimed_fees","rewards","fractional_farm_entitlement"]}});
        let mut state = snapshot_position(&observed).unwrap();
        value_position(&mut state, &Prices(Some("1.25")))
            .await
            .unwrap();
        let value = &state.anchor.as_ref().unwrap().evidence["vault_position"]["backing_valuation"];
        assert_eq!(value["total_usd_micros"], "1250002");
        assert_eq!(value["complete"], false);
        assert_eq!(value["basis"], "underlying_principal_only");
        assert_eq!(
            value["excluded_components"],
            json!(["unclaimed_fees", "rewards", "fractional_farm_entitlement"])
        );
    }
    #[test]
    fn duplicate_or_foreign_backing_and_excess_withdrawal_availability_fail_closed() {
        let mut p = position();
        p.withdrawable_shares_atoms = Some("56".into());
        assert!(snapshot_position(&p).is_err());
        p = position();
        p.underlying[0].token.mint = p.vault.shares.mint.clone();
        assert!(snapshot_position(&p).is_err());
    }
    #[test]
    fn native_share_guards_do_not_fabricate_rounded_farm_entitlement() {
        let mut intent = VaultIntent {
            wallet: "wallet".into(),
            family: VaultFamily::Lending,
            action: VaultAction::Deposit,
            vault: position().vault.address,
            amounts: vec![],
            shares_atoms: None,
            minimum_shares_atoms: Some("10".into()),
            maximum_network_fee_lamports: 5000,
            maximum_account_funding_lamports: 0,
            stake: true,
        };
        let mut proof = VerifiedVaultEffects {
            token_deltas: vec![],
            shares_delta: SignedQuantity::new("9").unwrap(),
            native_shares_delta: Some(SignedQuantity::new("10").unwrap()),
            rent_lamports_delta: SignedQuantity::new("0").unwrap(),
            rent_accounts: vec![],
            position: None,
            evidence: json!({"farm_entitlement_proof":"controlled"}),
        };
        validate_share_limits(&intent, &proof).unwrap();
        assert_eq!(proof.shares_delta.as_str(), "9");
        proof.native_shares_delta = Some(SignedQuantity::new("9").unwrap());
        assert!(validate_share_limits(&intent, &proof).is_err());
        intent.action = VaultAction::Withdraw;
        intent.stake = false;
        intent.minimum_shares_atoms = None;
        intent.shares_atoms = Some("5".into());
        proof.native_shares_delta = Some(SignedQuantity::new("-5").unwrap());
        proof.shares_delta = SignedQuantity::new("-4").unwrap();
        validate_share_limits(&intent, &proof).unwrap();
        proof.shares_delta = SignedQuantity::new("-6").unwrap();
        assert_eq!(
            validate_share_limits(&intent, &proof).unwrap_err().code,
            "KAMINO_SHARE_ENTITLEMENT_PENDING"
        );
        proof.shares_delta = SignedQuantity::new("-5").unwrap();
        proof.native_shares_delta = Some(SignedQuantity::new("-6").unwrap());
        assert!(validate_share_limits(&intent, &proof).is_err());
        let mut legacy = serde_json::to_value(&proof).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("native_shares_delta");
        let legacy: VerifiedVaultEffects = serde_json::from_value(legacy).unwrap();
        validate_share_limits(&intent, &legacy).unwrap();
    }
}
