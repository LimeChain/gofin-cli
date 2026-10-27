//! Receipt accounting for provider-trusted spot orders. Nothing in this module
//! inspects a candidate before signing; all inputs are finalized chain evidence.
use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostTreatment, EffectConstraint, FinancialEffect,
    GoFinError, OperationRecord, PositionAnchor, PositionKind, Quantity, ResourceId, Result,
    SignedQuantity, Submission, VerifiedEffects,
};
use gofin_solana::transaction::{FinalizedReceiptProof, NormalizedInstruction};
use serde_json::{Value, json};

use crate::{
    NETWORK,
    swap::{self, SwapIntent},
};

fn incomplete() -> GoFinError {
    GoFinError::new(
        "JUPITER_SETTLEMENT_EFFECT_MISMATCH",
        "reconciliation",
        "The finalized receipt does not establish the swap, costs and refundable account funding within the approved bounds.",
        "Keep the original operation unresolved and reconcile its finalized receipt; do not submit a replacement.",
    )
}

fn atoms(value: &Value) -> Result<u64> {
    value
        .get("uiTokenAmount")
        .and_then(|v| v.get("amount"))
        .and_then(Value::as_str)
        .ok_or_else(incomplete)?
        .parse()
        .map_err(|_| incomplete())
}

fn wallet_rows<'a>(
    rows: &'a [Value],
    wallet: &str,
    proof: &FinalizedReceiptProof,
) -> Result<BTreeMap<usize, &'a Value>> {
    let mut result = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|row| row.get("owner").and_then(Value::as_str) == Some(wallet))
    {
        let index = row
            .get("accountIndex")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(incomplete)?;
        if index >= proof.account_keys.len() || result.insert(index, row).is_some() {
            return Err(incomplete());
        }
    }
    Ok(result)
}

fn reserve(row: &Value, lamports: u64) -> Result<u64> {
    if row.get("mint").and_then(Value::as_str) == Some(swap::WRAPPED_SOL_MINT) {
        lamports.checked_sub(atoms(row)?).ok_or_else(incomplete)
    } else {
        Ok(lamports)
    }
}

pub(crate) fn verify_swap_effects(
    record: &OperationRecord,
    submission: &Submission,
    signature: &str,
    slot: u64,
    fee: u64,
    proof: &FinalizedReceiptProof,
) -> Result<VerifiedEffects> {
    let mut input = record.input.clone();
    input
        .as_object_mut()
        .ok_or_else(incomplete)?
        .remove("cluster");
    let intent: SwapIntent = serde_json::from_value(input).map_err(|_| incomplete())?;
    let preview = record.preview.as_ref().ok_or_else(incomplete)?;
    let binding = &preview.binding;
    let wallet = binding
        .get("wallet_address")
        .and_then(Value::as_str)
        .ok_or_else(incomplete)?;
    let (input_asset, output_asset) = preview
        .intent
        .expected
        .iter()
        .find_map(|effect| match effect {
            EffectConstraint::Conversion { input, output, .. } => {
                Some((input.clone(), output.clone()))
            }
            _ => None,
        })
        .ok_or_else(incomplete)?;
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let input_mint = if intent.input_mint == "native" {
        swap::WRAPPED_SOL_MINT
    } else {
        &intent.input_mint
    };
    let output_mint = if intent.output_mint == "native" {
        swap::WRAPPED_SOL_MINT
    } else {
        &intent.output_mint
    };
    let program = |key| {
        binding
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(incomplete)
    };
    let input_delta = proof.token_delta_verified(
        wallet,
        input_mint,
        program("input_token_program")?,
        input_asset.decimals,
    )?;
    let output_delta = proof.token_delta_verified(
        wallet,
        output_mint,
        program("output_token_program")?,
        output_asset.decimals,
    )?;
    for delta in proof.all_owner_token_deltas_verified(wallet)? {
        if delta.mint != input_mint && delta.mint != output_mint && !delta.atoms.is_zero() {
            return Err(incomplete());
        }
    }
    let input: u64 = intent.amount_atoms.parse().map_err(|_| incomplete())?;
    if (intent.input_mint == "native" && !input_delta.is_zero())
        || (intent.input_mint != "native"
            && input_delta != SignedQuantity::negative(&Quantity::from_u64(input)))
        || output_delta.is_negative()
        || (intent.output_mint == "native" && !output_delta.is_zero())
    {
        return Err(incomplete());
    }
    let pre = wallet_rows(&proof.pre_token_balances, wallet, proof)?;
    let post = wallet_rows(&proof.post_token_balances, wallet, proof)?;
    let receipt_instructions = receipt_instructions(proof)?;
    let payments = receipt_instructions
        .iter()
        .filter_map(|instruction| match system_payment(instruction) {
            Ok(Some(payment)) => Some(Ok(payment)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut effects = Vec::new();
    let mut funding = 0u64;
    let mut refunds = 0u64;
    let mut funding_destinations = BTreeSet::new();
    let observed_at = Utc::now().to_rfc3339();
    for index in pre
        .keys()
        .chain(post.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let before = *proof
            .pre_balances_lamports
            .get(index)
            .ok_or_else(incomplete)?;
        let after = *proof
            .post_balances_lamports
            .get(index)
            .ok_or_else(incomplete)?;
        let (row, amount, closing) = match (pre.get(&index), post.get(&index)) {
            (None, Some(row)) if before == 0 && after > 0 => {
                let amount = reserve(row, after)?;
                funding_destinations.insert(proof.account_keys[index].clone());
                // The quote is an estimate, not proof of who funded an account.
                // Attribute principal only from finalized System/CPI payments.
                let account = &proof.account_keys[index];
                let mut total_paid = 0u64;
                let mut wallet_paid = 0u64;
                for (source, destination, paid) in &payments {
                    if destination == account {
                        total_paid = total_paid.checked_add(*paid).ok_or_else(incomplete)?;
                        if source == wallet {
                            wallet_paid = wallet_paid.checked_add(*paid).ok_or_else(incomplete)?;
                        }
                    }
                }
                if total_paid != after || (wallet_paid != 0 && wallet_paid != amount) {
                    return Err(incomplete());
                }
                // Sponsored rent is not a movement of the wallet's principal.
                if wallet_paid == 0 {
                    continue;
                }
                funding = funding.checked_add(amount).ok_or_else(incomplete)?;
                (*row, amount, false)
            }
            (Some(row), None) if before > 0 && after == 0 => {
                let amount = reserve(row, before)?;
                if !closed_to(&receipt_instructions, &proof.account_keys[index], wallet)? {
                    return Err(incomplete());
                }
                refunds = refunds.checked_add(amount).ok_or_else(incomplete)?;
                (*row, amount, true)
            }
            (Some(before_row), Some(after_row)) => {
                if reserve(before_row, before)? != reserve(after_row, after)? {
                    return Err(incomplete());
                }
                continue;
            }
            _ => return Err(incomplete()),
        };
        if amount == 0 {
            continue;
        }
        let account = &proof.account_keys[index];
        let position = format!("account-funding:{account}");
        let quantity = Quantity::from_u64(amount);
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native.clone(),
                atoms: if closing {
                    Quantity::zero()
                } else {
                    quantity.clone()
                },
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"cluster":"mainnet-beta", "account":account, "account_owner":row.get("programId"), "refund_authority":wallet, "rent_lamports":if closing {0} else {amount}, "returned_rent_lamports":if closing {amount} else {0}, "pre_transaction_rent_lamports":if closing {amount} else {0}, "proof_signature":signature,"proof_message_sha256":proof.message_sha256}),
            observed_at: observed_at.clone(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: position.clone(),
            asset: native.clone(),
            delta: if closing {
                SignedQuantity::negative(&quantity)
            } else {
                SignedQuantity::positive(&quantity)
            },
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        let cash = AccountingLocation::Cash {
            resource: resource.clone(),
        };
        let position_location = AccountingLocation::Position {
            resource: resource.clone(),
            position,
            position_kind: PositionKind::RefundableAccountFunding,
        };
        effects.push(FinancialEffect::OriginMovement {
            from: if closing {
                position_location.clone()
            } else {
                cash.clone()
            },
            to: if closing { cash } else { position_location },
            debit: Amount {
                asset: native.clone(),
                atoms: quantity.clone(),
            },
            credit: Amount {
                asset: native.clone(),
                atoms: quantity,
            },
        });
    }
    if funding > intent.maximum_account_funding_lamports {
        return Err(incomplete());
    }
    if funding > 0 {
        effects.push(FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: Quantity::from_u64(funding),
            },
        });
    }
    if refunds > 0 {
        effects.push(FinancialEffect::UsableCredit {
            to: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: Quantity::from_u64(refunds),
            },
        });
    }
    let wallet_delta: i128 = proof
        .lamport_delta(wallet)?
        .as_str()
        .parse()
        .map_err(|_| incomplete())?;
    let network_fee = if proof.account_keys.first().map(String::as_str) == Some(wallet) {
        fee
    } else {
        0
    };
    let (output, actual_cost) = if intent.output_mint == "native" {
        let tips = native_output_extra_cost(proof, wallet, &funding_destinations)?;
        let actual_cost = network_fee.checked_add(tips).ok_or_else(incomplete)?;
        let output = wallet_delta
            .checked_add(i128::from(actual_cost))
            .and_then(|v| v.checked_add(i128::from(funding)))
            .and_then(|v| v.checked_sub(i128::from(refunds)))
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(incomplete)?;
        (output, actual_cost)
    } else {
        let output: u64 = output_delta.as_str().parse().map_err(|_| incomplete())?;
        let native_input = if intent.input_mint == "native" {
            input
        } else {
            0
        };
        let actual_cost = i128::from(refunds)
            .checked_sub(wallet_delta)
            .and_then(|v| v.checked_sub(i128::from(funding)))
            .and_then(|v| v.checked_sub(i128::from(native_input)))
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(incomplete)?;
        if actual_cost < network_fee {
            return Err(incomplete());
        }
        (output, actual_cost)
    };
    let minimum: u64 = intent
        .minimum_output_atoms
        .parse()
        .map_err(|_| incomplete())?;
    if output < minimum || actual_cost > intent.maximum_network_fee_lamports {
        return Err(incomplete());
    }
    effects.insert(
        0,
        FinancialEffect::Conversion {
            resource: resource.clone(),
            spent: Amount {
                asset: input_asset,
                atoms: Quantity::from_u64(input),
            },
            received: Amount {
                asset: output_asset,
                atoms: Quantity::from_u64(output),
            },
        },
    );
    for (cost, category) in [
        (network_fee, "solana_network_fee"),
        (actual_cost - network_fee, "jupiter_execution_cost"),
    ] {
        if cost == 0 {
            continue;
        }
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: native.clone(),
                atoms: Quantity::from_u64(cost),
            },
            category: category.into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: json!({"cluster":"mainnet-beta","signature":signature,"slot":slot,"network_fee_lamports":fee,"wallet_total_cost_lamports":actual_cost,"proof":proof,"provider_request_id":submission.data.get("provider_request_id")}),
        observed_at,
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

/// Native-output swaps need outgoing SOL costs separated from incoming output.
/// Decode only standard System/Token accounting actions in the finalized receipt,
/// including CPI records; Jupiter's route/program payload remains opaque.
fn native_output_extra_cost(
    proof: &FinalizedReceiptProof,
    wallet: &str,
    funded: &BTreeSet<String>,
) -> Result<u64> {
    let instructions = receipt_instructions(proof)?;
    let mut transient = BTreeSet::new();
    for instruction in &instructions {
        if (instruction.program_id == gofin_solana::token::TOKEN_PROGRAM_ID
            || instruction.program_id == gofin_solana::token::TOKEN_2022_PROGRAM_ID)
            && instruction_data(instruction)? == [9]
            && instruction
                .accounts
                .get(1)
                .is_some_and(|a| a.address == wallet)
        {
            let account = instruction
                .accounts
                .first()
                .ok_or_else(incomplete)?
                .address
                .clone();
            let index = proof
                .account_keys
                .iter()
                .position(|key| key == &account)
                .ok_or_else(incomplete)?;
            if proof.pre_balances_lamports.get(index) == Some(&0)
                && proof.post_balances_lamports.get(index) == Some(&0)
            {
                transient.insert(account);
            }
        }
    }
    let mut costs = 0u64;
    for instruction in &instructions {
        let Some((source, destination, lamports)) = system_payment(instruction)? else {
            continue;
        };
        if source != wallet
            || destination == wallet
            || funded.contains(&destination)
            || transient.contains(&destination)
        {
            continue;
        }
        costs = costs.checked_add(lamports).ok_or_else(incomplete)?;
    }
    Ok(costs)
}

fn instruction_data(instruction: &NormalizedInstruction) -> Result<Vec<u8>> {
    base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &instruction.data_base64,
    )
    .map_err(|_| incomplete())
}

/// Historical prestate for a closed account, authenticated by the finalized
/// receipt. The engine only imports this when no prior anchor exists.
pub(crate) fn position_baselines(verified: &VerifiedEffects) -> Result<Vec<PositionAnchor>> {
    verified
        .effects
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::PositionChange {
                delta,
                anchor: Some(anchor),
                ..
            } if delta.is_negative()
                && anchor.position_kind == PositionKind::RefundableAccountFunding =>
            {
                Some((delta, anchor))
            }
            _ => None,
        })
        .map(|(delta, anchor)| {
            let rent = anchor
                .evidence
                .get("pre_transaction_rent_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(incomplete)?;
            if Quantity::from_u64(rent) != delta.magnitude() {
                return Err(incomplete());
            }
            let mut baseline = anchor.clone();
            baseline.principal.atoms = Quantity::from_u64(rent);
            baseline.evidence["rent_lamports"] = json!(rent);
            baseline.evidence["historical_prestate"] = json!(true);
            Ok(baseline)
        })
        .collect()
}

fn receipt_instructions(proof: &FinalizedReceiptProof) -> Result<Vec<NormalizedInstruction>> {
    let mut instructions = proof.message_instructions.clone();
    for group in &proof.inner_instructions {
        for instruction in group
            .get("instructions")
            .and_then(Value::as_array)
            .ok_or_else(incomplete)?
        {
            let index = instruction
                .get("programIdIndex")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(incomplete)?;
            let program_id = proof
                .account_keys
                .get(index)
                .cloned()
                .ok_or_else(incomplete)?;
            let accounts = instruction
                .get("accounts")
                .and_then(Value::as_array)
                .ok_or_else(incomplete)?
                .iter()
                .map(|index| {
                    let index = index
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(incomplete)?;
                    Ok(gofin_solana::transaction::NormalizedAccountMeta {
                        address: proof
                            .account_keys
                            .get(index)
                            .cloned()
                            .ok_or_else(incomplete)?,
                        signer: false,
                        writable: false,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            instructions.push(NormalizedInstruction {
                program_id,
                accounts,
                data_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    bs58::decode(
                        instruction
                            .get("data")
                            .and_then(Value::as_str)
                            .ok_or_else(incomplete)?,
                    )
                    .into_vec()
                    .map_err(|_| incomplete())?,
                ),
            });
        }
    }
    Ok(instructions)
}

fn system_payment(instruction: &NormalizedInstruction) -> Result<Option<(String, String, u64)>> {
    if instruction.program_id != swap::SYSTEM_PROGRAM {
        return Ok(None);
    }
    let data = instruction_data(instruction)?;
    let tag = data
        .get(..4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(incomplete)?;
    let (destination_index, amount_offset) = match tag {
        0 | 2 | 5 => (1, 4),
        3 => {
            let seed_length = data
                .get(36..44)
                .and_then(|v| v.try_into().ok())
                .map(u64::from_le_bytes)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(incomplete)?;
            (1, 44usize.checked_add(seed_length).ok_or_else(incomplete)?)
        }
        11 => (2, 4),
        1 | 4 | 6..=10 | 12 => return Ok(None),
        _ => return Err(incomplete()),
    };
    let source = instruction
        .accounts
        .first()
        .ok_or_else(incomplete)?
        .address
        .clone();
    let destination = instruction
        .accounts
        .get(destination_index)
        .ok_or_else(incomplete)?
        .address
        .clone();
    let amount = data
        .get(amount_offset..amount_offset.checked_add(8).ok_or_else(incomplete)?)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(incomplete)?;
    Ok(Some((source, destination, amount)))
}

fn closed_to(
    instructions: &[NormalizedInstruction],
    account: &str,
    destination: &str,
) -> Result<bool> {
    let mut matches = 0;
    for instruction in instructions {
        if (instruction.program_id == gofin_solana::token::TOKEN_PROGRAM_ID
            || instruction.program_id == gofin_solana::token::TOKEN_2022_PROGRAM_ID)
            && instruction
                .accounts
                .first()
                .is_some_and(|a| a.address == account)
            && instruction_data(instruction)? == [9]
        {
            if instruction
                .accounts
                .get(1)
                .is_none_or(|a| a.address != destination)
            {
                return Err(incomplete());
            }
            matches += 1;
        }
    }
    Ok(matches == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_solana::transaction::NormalizedAccountMeta;

    fn proof() -> FinalizedReceiptProof {
        FinalizedReceiptProof {
            signature: "receipt".into(),
            slot: 10,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "hash".into(),
            account_keys: vec![
                "wallet".into(),
                "tip".into(),
                "temporary".into(),
                "funded".into(),
            ],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![100_000, 0, 0, 0],
            post_balances_lamports: vec![120_000, 1000, 0, 2000],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        }
    }
    fn ix(program: &str, accounts: &[&str], data: Vec<u8>) -> NormalizedInstruction {
        NormalizedInstruction {
            program_id: program.into(),
            accounts: accounts
                .iter()
                .map(|a| NormalizedAccountMeta {
                    address: (*a).into(),
                    signer: false,
                    writable: true,
                })
                .collect(),
            data_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data),
        }
    }
    fn transfer(destination: &str, amount: u64) -> NormalizedInstruction {
        ix(
            swap::SYSTEM_PROGRAM,
            &["wallet", destination],
            [
                2u32.to_le_bytes().as_slice(),
                amount.to_le_bytes().as_slice(),
            ]
            .concat(),
        )
    }
    #[test]
    fn native_output_cost_excludes_persistent_and_closed_temporary_rent() {
        let mut receipt = proof();
        receipt.message_instructions = vec![
            transfer("tip", 1000),
            transfer("funded", 2000),
            transfer("temporary", 3000),
            ix(
                gofin_solana::token::TOKEN_PROGRAM_ID,
                &["temporary", "wallet", "wallet"],
                vec![9],
            ),
        ];
        assert_eq!(
            native_output_extra_cost(&receipt, "wallet", &BTreeSet::from(["funded".into()]))
                .unwrap(),
            1000
        );
    }
    #[test]
    fn native_output_cost_includes_cpi_transfers() {
        let mut receipt = proof();
        receipt.account_keys.push(swap::SYSTEM_PROGRAM.into());
        receipt.inner_instructions = vec![
            json!({"index":0,"instructions":[{"programIdIndex":4,"accounts":[0,1],"data":bs58::encode([2u32.to_le_bytes().as_slice(),700u64.to_le_bytes().as_slice()].concat()).into_string()}]}),
        ];
        assert_eq!(
            native_output_extra_cost(&receipt, "wallet", &BTreeSet::new()).unwrap(),
            700
        );
    }
    #[test]
    fn unknown_wallet_system_movements_remain_unresolved() {
        let mut receipt = proof();
        receipt.message_instructions.push(ix(
            swap::SYSTEM_PROGRAM,
            &["wallet", "tip"],
            99u32.to_le_bytes().to_vec(),
        ));
        assert!(native_output_extra_cost(&receipt, "wallet", &BTreeSet::new()).is_err());
    }
    fn native_output_fixture() -> (OperationRecord, Submission, FinalizedReceiptProof) {
        let token = gofin_solana::token::TOKEN_PROGRAM_ID;
        let input = json!({"wallet":"test","network":"solana","cluster":"mainnet-beta","input_mint":"usdc","output_mint":"native","amount_atoms":"100","minimum_output_atoms":"10000","maximum_slippage_bps":100,"maximum_platform_fee_bps":0,"maximum_network_fee_lamports":5000,"maximum_account_funding_lamports":2000});
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let input_asset = AssetId {
            network: NETWORK.into(),
            reference: "usdc".into(),
            decimals: 6,
        };
        let resource = ResourceId("solana:mainnet-beta:wallet".into());
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: "synthetic".into(),
            data: json!({"fee_payer":"wallet"}),
        };
        let preview = gofin_domain::PreparedOperation {
            operation: "jupiter.swap".into(),
            input: input.clone(),
            binding: json!({"wallet_address":"wallet","input_token_program":token,"output_token_program":token,"order_response":{"rentFeePayer":"wallet","rentFeeLamports":2000}}),
            summary: "test".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: Utc::now().to_rfc3339(),
            expires_at: Utc::now().to_rfc3339(),
            valuation: None,
            intent: gofin_domain::FinancialIntent {
                expected: vec![EffectConstraint::Conversion {
                    resource,
                    input: input_asset,
                    maximum_input: Quantity::from_u64(100),
                    output: native,
                    minimum_output: Quantity::from_u64(10000),
                }],
                ..Default::default()
            },
            action_deadline: None,
            operation_version: 1,
        };
        let record = OperationRecord {
            id: "test".into(),
            request_id: "test".into(),
            fingerprint: "test".into(),
            fingerprint_version: 1,
            operation: "jupiter.swap".into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            preview: Some(preview),
            approval: None,
            authorization_source: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        };
        let mut receipt = proof();
        receipt.account_keys = vec![
            "wallet".into(),
            "input-ata".into(),
            "new-ata".into(),
            "sponsor".into(),
        ];
        receipt.pre_balances_lamports = vec![1000000, 2000, 0, 1000000];
        receipt.post_balances_lamports = vec![1005000, 2000, 2000, 998000];
        let row = |index, amount| json!({"accountIndex":index,"mint":"usdc","owner":"wallet","programId":token,"uiTokenAmount":{"amount":amount,"decimals":6}});
        receipt.pre_token_balances = vec![row(1, "100")];
        receipt.post_token_balances = vec![row(1, "0"), row(2, "0")];
        receipt.message_instructions = vec![ix(
            swap::SYSTEM_PROGRAM,
            &["sponsor", "new-ata"],
            [
                2u32.to_le_bytes().as_slice(),
                2000u64.to_le_bytes().as_slice(),
            ]
            .concat(),
        )];
        (record, submission, receipt)
    }

    #[test]
    fn sponsored_rent_cannot_inflate_native_output_despite_quoted_wallet_payer() {
        let (mut record, submission, receipt) = native_output_fixture();
        let verified =
            verify_swap_effects(&record, &submission, "receipt", 10, 5000, &receipt).unwrap();
        assert!(
            matches!(&verified.effects[0],FinancialEffect::Conversion{received,..} if received.atoms==Quantity::from_u64(10000))
        );
        assert!(!verified.effects.iter().any(|e| matches!(
            e,
            FinancialEffect::PositionChange { .. } | FinancialEffect::CashDebit { .. }
        )));
        record.input["minimum_output_atoms"] = json!("11000");
        assert!(verify_swap_effects(&record, &submission, "receipt", 10, 5000, &receipt).is_err());
    }

    #[test]
    fn rent_refund_to_other_account_remains_unresolved() {
        let (record, submission, mut receipt) = native_output_fixture();
        receipt.pre_balances_lamports[2] = 2000;
        receipt.post_balances_lamports[2] = 0;
        receipt
            .pre_token_balances
            .push(receipt.post_token_balances.pop().unwrap());
        receipt.message_instructions = vec![ix(
            gofin_solana::token::TOKEN_PROGRAM_ID,
            &["new-ata", "sponsor", "wallet"],
            vec![9],
        )];
        assert!(verify_swap_effects(&record, &submission, "receipt", 10, 5000, &receipt).is_err());
    }
}
