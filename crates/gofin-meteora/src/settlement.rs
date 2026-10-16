//! Two-asset Meteora position accounting from transaction-scoped proof data.

use chrono::Utc;
use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostTreatment, FinancialEffect, GoFinError,
    LiquidityAssetDelta, LiquidityCompositionAmount, LiquidityPositionEvent,
    LiquidityPositionEvidence, OperationRecord, PositionAnchor, PositionKind, PositionState,
    Quantity, ResourceId, Result, SignedQuantity, Submission, VerifiedEffects,
    verified_effect_digest,
};
use gofin_solana::FinalizedReceiptProof;
use num_bigint::BigInt;
use serde_json::{Value, json};

use crate::protocol::{
    MeteoraAction, MeteoraIntent, PoolInfo, PositionInfo, ProtocolCandidate, ProtocolFamily,
    VerifiedProtocolEffects,
};

pub fn snapshot_anchors(
    _wallet_name: &str,
    wallet_address: &str,
    position: &PositionInfo,
) -> Result<Vec<PositionState>> {
    if position.owner != wallet_address
        || position.pool.trim().is_empty()
        || position.address.trim().is_empty()
    {
        return Err(invalid_proof_check("submission_identity"));
    }
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
    let observed_at = Utc::now().to_rfc3339();
    let position_id = liquidity_position_id(position.family.as_str(), &position.address);
    let units_known = position.liquidity_units_atoms.is_some();
    let units = Quantity::new(position.liquidity_units_atoms.as_deref().unwrap_or("0"))?;
    let asset = liquidity_asset(position.family.as_str(), &position.address);
    let typed = LiquidityPositionEvidence {
        version: 1,
        family: position.family.as_str().to_owned(),
        pool: position.pool.clone(),
        position: position.address.clone(),
        anchor_position_id: Some(position_id.clone()),
        observed_slot: position.observation_slot.or_else(|| {
            position
                .protocol_state
                .get("observed_slot")
                .and_then(Value::as_u64)
        }),
        pre_transaction_units_atoms: None,
        liquidity_units_atoms: position
            .liquidity_units_atoms
            .as_deref()
            .map(Quantity::new)
            .transpose()?,
        liquidity_units_known: units_known,
        historical_principal_known: false,
        token_a: LiquidityCompositionAmount {
            asset: AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: position.token_a.mint.clone(),
                decimals: position.token_a.decimals,
            },
            token_program: position.token_a.token_program.clone(),
            atoms: position
                .token_a_atoms
                .as_deref()
                .map(Quantity::new)
                .transpose()?,
        },
        token_b: LiquidityCompositionAmount {
            asset: AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: position.token_b.mint.clone(),
                decimals: position.token_b.decimals,
            },
            token_program: position.token_b.token_program.clone(),
            atoms: position
                .token_b_atoms
                .as_deref()
                .map(Quantity::new)
                .transpose()?,
        },
        backing_valuation: None,
        history: Vec::new(),
    };
    let evidence = json!({
        "version":3,
        "source":"meteora_position_observation",
        "basis":"stable_protocol_liquidity_units",
        "liquidity_units_known":units_known,
        "historical_principal_known":false,
        "liquidity_position":typed,
        "owner":position.owner,
        "receipt_mint":position.receipt_mint,
        "receipt_atoms":position.receipt_atoms,
        "bin_lower":position.bin_lower,
        "bin_upper":position.bin_upper,
        "protocol_state":position.protocol_state,
    });
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position_id.clone(),
        position_kind: PositionKind::Liquidity,
        principal: Amount {
            asset: asset.clone(),
            atoms: units,
        },
        scaled_receipt: None,
        evidence_version: 3,
        evidence,
        observed_at: observed_at.clone(),
    };
    let mut positions = vec![PositionState {
        resource,
        position: position_id,
        kind: PositionKind::Liquidity,
        amount: anchor.principal.clone(),
        scaled_receipt: None,
        anchor: Some(anchor),
    }];
    let mut rent_accounts = std::collections::BTreeSet::new();
    for rent in &position.rent_accounts {
        let address = gofin_solana::parse_address(&rent.account).map_err(|_| invalid_proof())?;
        if !rent_accounts.insert(address.clone()) {
            return Err(invalid_proof());
        }
        let amount = Amount {
            asset: AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: Quantity::from_u64(rent.lamports),
        };
        let id = format!("account-funding:{address}");
        let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: id.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
            principal: amount.clone(),
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"version":1,"source":"meteora_position_rent_observation","account":address,"observed_slot":position.observation_slot}),
            observed_at: observed_at.clone(),
        };
        positions.push(PositionState {
            resource,
            position: id,
            kind: PositionKind::RefundableAccountFunding,
            amount,
            scaled_receipt: None,
            anchor: Some(anchor),
        });
    }
    Ok(positions)
}

pub fn liquidity_asset(family: &str, position: &str) -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: format!("meteora:liquidity:{family}:{position}"),
        decimals: 0,
    }
}
fn token_asset(pool: &PoolInfo, a: bool) -> AssetId {
    let token = if a { &pool.token_a } else { &pool.token_b };
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: token.mint.clone(),
        decimals: token.decimals,
    }
}
pub fn liquidity_position_id(family: &str, position: &str) -> String {
    format!("meteora:{family}:{position}:liquidity_units")
}

pub fn zero_open_baselines(
    resource: &ResourceId,
    pool: &PoolInfo,
    position: &str,
) -> Vec<PositionAnchor> {
    let observed_at = Utc::now().to_rfc3339();
    let id = liquidity_position_id(pool.family.as_str(), position);
    vec![PositionAnchor {
        resource: resource.clone(),
        position: id,
        position_kind: PositionKind::Liquidity,
        principal: Amount {
            asset: liquidity_asset(pool.family.as_str(), position),
            atoms: Quantity::zero(),
        },
        scaled_receipt: None,
        evidence_version: 3,
        evidence: json!({"version":3,"source":"verified_zero_before_position_open","family":pool.family,"pool":pool.address,"position":position,"basis":"stable_protocol_liquidity_units","liquidity_units_known":true,"historical_principal_known":true,"liquidity_position":{"version":1,"family":pool.family.as_str(),"pool":pool.address,"position":position,"observed_slot":null,"liquidity_units_atoms":"0","liquidity_units_known":true,"historical_principal_known":true,"token_a":{"asset":{"network":gofin_solana::NETWORK,"reference":pool.token_a.mint,"decimals":pool.token_a.decimals},"token_program":pool.token_a.token_program,"atoms":"0"},"token_b":{"asset":{"network":gofin_solana::NETWORK,"reference":pool.token_b.mint,"decimals":pool.token_b.decimals},"token_program":pool.token_b.token_program,"atoms":"0"},"history":[]}}),
        observed_at,
    }]
}

#[expect(
    clippy::too_many_arguments,
    reason = "Finalized settlement independently binds operation, submission, request, protocol proof, slot, and fee evidence."
)]
pub fn verify(
    record: &OperationRecord,
    submission: &Submission,
    intent: &MeteoraIntent,
    candidate: &ProtocolCandidate,
    verified: &VerifiedProtocolEffects,
    proof: &FinalizedReceiptProof,
    slot: u64,
    fee_lamports: u64,
) -> Result<VerifiedEffects> {
    if proof.failed
        || proof.signature.is_empty()
        || proof.slot != slot
        || proof.message_sha256.is_empty()
        || proof.message_sha256
            != record
                .preview
                .as_ref()
                .and_then(|p| p.binding["message_sha256"].as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(invalid_proof)?
        || submission.kind != "solana.transaction"
        || submission.data["message_sha256"].as_str() != Some(proof.message_sha256.as_str())
        || !submission.data["wallet_signature"]
            .as_str()
            .is_some_and(|signature| {
                signature == proof.signature
                    || submission.data["chain_signature"].as_str() == Some(&proof.signature)
            })
    {
        return Err(invalid_proof_check("submission_identity"));
    }
    if fee_lamports > intent.maximum_network_fee_lamports {
        return Err(GoFinError::validation(
            "Finalized Meteora network fee exceeded the approved maximum.",
        ));
    }
    if verified.position_asset_deltas.len() > 2
        || verified.token_deltas.len() > 16
        || verified.rent_accounts.len() > 32
        || verified.collected_fees.len() > 16
        || verified.collected_rewards.len() > 16
    {
        return Err(invalid_proof_check("protocol_delta_item_limits"));
    }
    let preview = record.preview.as_ref().ok_or_else(invalid_proof)?;
    let pool: PoolInfo =
        serde_json::from_value(preview.binding["pool"].clone()).map_err(|_| invalid_proof())?;
    let resource = ResourceId(format!(
        "solana:mainnet-beta:{}",
        preview.binding["wallet_address"]
            .as_str()
            .ok_or_else(invalid_proof)?
    ));
    let wallet_address = resource
        .0
        .strip_prefix("solana:mainnet-beta:")
        .ok_or_else(invalid_proof)?;
    let receipt = proof.all_owner_token_deltas_verified(wallet_address)?;
    let position_mint = if intent.family == ProtocolFamily::DammV2 {
        candidate
            .metadata
            .get("position_mint")
            .and_then(Value::as_str)
    } else {
        None
    };
    if let Some(position_mint) = position_mint {
        let expected_nft_delta = match intent.action {
            MeteoraAction::Open => 1,
            MeteoraAction::Close => -1,
            _ => 0,
        };
        if !position_nft_receipt_delta_matches(
            &receipt,
            position_mint,
            gofin_solana::token::TOKEN_2022_PROGRAM_ID,
            0,
            expected_nft_delta,
        ) || !reported_position_nft_delta_matches(
            &verified.token_deltas,
            position_mint,
            gofin_solana::token::TOKEN_2022_PROGRAM_ID,
            0,
            expected_nft_delta,
        ) {
            return Err(invalid_proof_check("position_nft_receipt_delta"));
        }
    }
    let nonzero_receipt = receipt
        .into_iter()
        .filter(|delta| !delta.atoms.is_zero() && Some(delta.mint.as_str()) != position_mint)
        .collect::<Vec<_>>();
    if !owner_token_deltas_match(&nonzero_receipt, &verified.token_deltas, position_mint) {
        return Err(invalid_proof_check("owner_token_delta_match"));
    }
    if matches!(
        intent.action,
        MeteoraAction::Open | MeteoraAction::Add | MeteoraAction::Remove
    ) && !position_asset_deltas_match_wallet_cashflow(
        &verified.position_asset_deltas,
        &verified.token_deltas,
        [
            (
                pool.token_a.mint.as_str(),
                pool.token_a.decimals,
                pool.token_a.token_program.as_str(),
            ),
            (
                pool.token_b.mint.as_str(),
                pool.token_b.decimals,
                pool.token_b.token_program.as_str(),
            ),
        ],
    ) {
        return Err(invalid_proof_check("position_asset_wallet_cashflow"));
    }
    let mut effects = Vec::new();
    let fee_asset = AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    if fee_lamports > 0 {
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: fee_asset.clone(),
                atoms: Quantity::from_u64(fee_lamports),
            },
            category: "network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let nonrefundable_rows = candidate
        .metadata
        .get("nonrefundable_account_costs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut nonrefundable_accounts = std::collections::BTreeSet::new();
    let mut nonrefundable_total = BigInt::from(0);
    for row in nonrefundable_rows {
        let account = row
            .get("account")
            .and_then(Value::as_str)
            .ok_or_else(invalid_proof)?;
        let maximum = row
            .get("maximum_lamports")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| value.as_u64())
            })
            .ok_or_else(invalid_proof)?;
        let delta = proof.lamport_delta(account)?.integer();
        if !nonrefundable_accounts.insert(account.to_owned())
            || delta.sign() != num_bigint::Sign::Plus
            || delta > BigInt::from(maximum)
        {
            return Err(invalid_proof_check("nonrefundable_account_cost"));
        }
        let amount = Quantity::new(delta.to_string())?;
        nonrefundable_total += &delta;
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: fee_asset.clone(),
                atoms: amount,
            },
            category: "persistent_account_funding".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let mut delta_mints = std::collections::BTreeSet::new();
    for delta in &verified.token_deltas {
        if !delta_mints.insert(delta.mint.as_str()) {
            return Err(invalid_proof_check("duplicate_token_mint"));
        }
        if let Some(token) = token_by_mint(&pool, &delta.mint) {
            if token.decimals != delta.decimals || token.token_program != delta.token_program {
                return Err(invalid_proof_check("pool_token_identity"));
            }
        } else if position_mint == Some(delta.mint.as_str()) {
            if delta.atoms.is_negative() && intent.action != MeteoraAction::Close {
                return Err(invalid_proof_check("position_nft_delta"));
            }
            // Position NFTs are proof-only identity. Their balance is not a
            // fungible token credit or liquidity-unit balance.
            continue;
        } else if !verified
            .collected_rewards
            .iter()
            .chain(&verified.collected_fees)
            .any(|reward| {
                reward.mint == delta.mint
                    && reward.decimals == delta.decimals
                    && reward.token_program == delta.token_program
            })
        {
            return Err(invalid_proof_check("unexpected_token_mint"));
        }
        if delta.atoms.is_negative() {
            let amount = Amount {
                asset: AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: delta.mint.clone(),
                    decimals: delta.decimals,
                },
                atoms: delta.atoms.magnitude(),
            };
            if intent.action == MeteoraAction::Swap {
                if intent.input_mint.as_deref() != Some(delta.mint.as_str())
                    || delta.atoms.magnitude()
                        > Quantity::new(intent.amount_atoms.as_deref().ok_or_else(invalid_proof)?)?
                {
                    return Err(invalid_proof_check("authorized_swap_input"));
                }
            } else if matches!(intent.action, MeteoraAction::Open | MeteoraAction::Add) {
                let cap = if delta.mint == pool.token_a.mint {
                    intent.token_a_atoms.as_deref()
                } else if delta.mint == pool.token_b.mint {
                    intent.token_b_atoms.as_deref()
                } else {
                    None
                }
                .ok_or_else(invalid_proof)?;
                if delta.atoms.magnitude() > Quantity::new(cap)? {
                    return Err(invalid_proof_check("approved_liquidity_debit"));
                }
            } else {
                // Remove/collect/close only release value to the wallet.
                return Err(invalid_proof_check("unexpected_position_token_debit"));
            }
            if intent.action != MeteoraAction::Swap {
                effects.push(FinancialEffect::CashDebit {
                    resource: resource.clone(),
                    amount,
                });
            }
        } else if !delta.atoms.is_zero() && intent.action != MeteoraAction::Swap {
            effects.push(FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: delta.mint.clone(),
                        decimals: delta.decimals,
                    },
                    atoms: delta.atoms.magnitude(),
                },
            });
        }
    }
    claim_credit_totals(
        &verified.collected_fees,
        &verified.collected_rewards,
        &verified.token_deltas,
        intent.action == MeteoraAction::Collect,
    )?;
    if intent.action == MeteoraAction::Swap {
        const WSOL: &str = "So11111111111111111111111111111111111111112";
        let input = intent.input_mint.as_deref().ok_or_else(invalid_proof)?;
        let input_token = if input == "native" {
            if pool.token_a.mint == WSOL {
                pool.token_a.mint.as_str()
            } else if pool.token_b.mint == WSOL {
                pool.token_b.mint.as_str()
            } else {
                return Err(invalid_proof_check("native_pool_mint"));
            }
        } else {
            input
        };
        let output_token = if input_token == pool.token_a.mint {
            pool.token_b.mint.as_str()
        } else {
            pool.token_a.mint.as_str()
        };
        if token_by_mint(&pool, input_token).is_none() {
            return Err(invalid_proof_check("swap_input_mint"));
        }
        if intent.output_mint.as_deref() == Some("native") && output_token != WSOL {
            return Err(invalid_proof_check("native_output_mint"));
        }
        let (spent_asset, spent_atoms) = if input == "native" {
            (
                AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                verified
                    .native_cash_delta
                    .as_ref()
                    .filter(|d| d.is_negative())
                    .ok_or_else(invalid_proof)?
                    .magnitude(),
            )
        } else {
            let delta = verified
                .token_deltas
                .iter()
                .find(|delta| delta.mint == input_token)
                .filter(|delta| delta.atoms.is_negative())
                .ok_or_else(invalid_proof)?;
            (
                AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: input.into(),
                    decimals: delta.decimals,
                },
                delta.atoms.magnitude(),
            )
        };
        let (received_asset, received_atoms) = if intent.output_mint.as_deref() == Some("native") {
            (
                AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                verified
                    .native_cash_delta
                    .as_ref()
                    .filter(|d| !d.is_negative() && !d.is_zero())
                    .ok_or_else(invalid_proof)?
                    .magnitude(),
            )
        } else {
            let delta = verified
                .token_deltas
                .iter()
                .find(|delta| delta.mint == output_token)
                .filter(|delta| !delta.atoms.is_negative() && !delta.atoms.is_zero())
                .ok_or_else(invalid_proof)?;
            (
                AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: output_token.into(),
                    decimals: delta.decimals,
                },
                delta.atoms.magnitude(),
            )
        };
        if spent_atoms > Quantity::new(intent.amount_atoms.as_deref().ok_or_else(invalid_proof)?)?
            || received_atoms
                < Quantity::new(
                    intent
                        .minimum_output_atoms
                        .as_deref()
                        .ok_or_else(invalid_proof)?,
                )?
        {
            return Err(invalid_proof_check("swap_bound"));
        }
        effects.push(FinancialEffect::Conversion {
            resource: resource.clone(),
            spent: Amount {
                asset: spent_asset,
                atoms: spent_atoms,
            },
            received: Amount {
                asset: received_asset,
                atoms: received_atoms,
            },
        });
    }
    let pinned_rent_accounts = candidate
        .metadata
        .get("refundable_rent_accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut rent_account_ids = std::collections::BTreeSet::new();
    let mut rent_total = BigInt::from(0);
    let mut refundable_funding_total = BigInt::from(0);
    for delta in &verified.rent_accounts {
        let address = gofin_solana::parse_address(&delta.account).map_err(|_| invalid_proof())?;
        let row = pinned_rent_accounts
            .iter()
            .find(|row| row.get("account").and_then(Value::as_str) == Some(delta.account.as_str()))
            .ok_or_else(invalid_proof)?;
        let declared_maximum = row
            .get("maximum_lamports")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| value.as_u64())
            })
            .ok_or_else(invalid_proof)?;
        let quoted_delta = candidate
            .quote
            .protocol_details
            .get("rent_account_deltas")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|quote| {
                quote.get("account").and_then(Value::as_str) == Some(delta.account.as_str())
            })
            .and_then(|quote| {
                quote.get("lamports_delta").and_then(|value| {
                    value
                        .as_str()
                        .and_then(|s| s.parse::<i128>().ok())
                        .or_else(|| value.as_i64().map(i128::from))
                })
            })
            .and_then(|delta| u64::try_from(delta.saturating_abs()).ok())
            .unwrap_or(0);
        let maximum_lamports = declared_maximum.max(quoted_delta);
        if !rent_account_ids.insert(delta.account.clone())
            || row.get("position").and_then(Value::as_str) != Some(delta.position.as_str())
            || delta.position != format!("account-funding:{}", address)
            || (delta.atoms.is_negative()
                && delta.atoms.magnitude() > Quantity::from_u64(maximum_lamports))
            || (!delta.atoms.is_negative()
                && delta.atoms.magnitude() > Quantity::from_u64(maximum_lamports))
            || proof.lamport_delta(&delta.account)? != delta.atoms
        {
            return Err(invalid_proof_check("refundable_account_proof"));
        }
        if delta.atoms.is_negative() && intent.action != MeteoraAction::Close {
            return Err(invalid_proof_check("rent_debit_policy"));
        }
        let amount = Amount {
            asset: AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            },
            atoms: delta.atoms.magnitude(),
        };
        rent_total += delta.atoms.integer();
        refundable_funding_total += account_funding_debit(&delta.atoms.integer());
        let id = delta.position.clone();
        let baseline = record.position_baseline.as_ref().and_then(|baseline| {
            baseline.anchors.iter().find(|anchor| {
                anchor.position == id
                    && anchor.position_kind == PositionKind::RefundableAccountFunding
            })
        });
        let old = baseline
            .map(|anchor| anchor.principal.atoms.clone())
            .unwrap_or_else(Quantity::zero);
        let new = if delta.atoms.is_negative() {
            old.checked_sub(&delta.atoms.magnitude())
                .map_err(|_| invalid_proof())?
        } else {
            old.checked_add(&delta.atoms.magnitude())?
        };
        if delta.atoms.is_negative() && baseline.is_none() {
            return Err(invalid_proof());
        }
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: id.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: amount.asset.clone(),
                atoms: new,
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"version":1,"source":"finalized_meteora_rent_account_delta","account":delta.account,"signature":proof.signature,"slot":proof.slot}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: id.clone(),
            asset: amount.asset.clone(),
            delta: delta.atoms.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        if delta.atoms.is_negative() {
            effects.push(FinancialEffect::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: id,
                    position_kind: PositionKind::RefundableAccountFunding,
                },
                to: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                debit: amount.clone(),
                credit: amount,
            });
        } else {
            effects.push(FinancialEffect::CashDebit {
                resource: resource.clone(),
                amount: amount.clone(),
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
    }
    if intent.action == MeteoraAction::Close {
        let refund_credit = refundable_rent_credit_total(&verified.rent_accounts)?;
        if !refund_credit.is_zero() {
            effects.push(FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: "native".into(),
                        decimals: 9,
                    },
                    atoms: refund_credit,
                },
            });
        }
    }
    let native_delta = verified
        .native_cash_delta
        .as_ref()
        .map(SignedQuantity::integer)
        .unwrap_or_else(|| BigInt::from(0));
    if intent.action == MeteoraAction::Swap
        && ((intent.input_mint.as_deref() == Some("native")
            && native_delta.sign() != num_bigint::Sign::Minus)
            || (intent.output_mint.as_deref() == Some("native")
                && (native_delta.sign() == num_bigint::Sign::Minus
                    || native_delta == BigInt::from(0)))
            || (intent.input_mint.as_deref() != Some("native")
                && intent.output_mint.as_deref() != Some("native")
                && native_delta != BigInt::from(0)))
    {
        return Err(invalid_proof_check("native_cash_delta_identity"));
    }
    if intent.action != MeteoraAction::Swap
        && verified
            .native_cash_delta
            .as_ref()
            .is_some_and(|delta| !delta.is_zero())
    {
        return Err(invalid_proof_check("unexpected_native_delta_for_action"));
    }
    let expected_wallet_lamports = SignedQuantity::new(
        (native_delta - BigInt::from(fee_lamports) - &rent_total - &nonrefundable_total)
            .to_string(),
    )?;
    let wallet_delta = proof.lamport_delta(wallet_address)?;
    if verified.rent_lamports_delta != wallet_delta
        || wallet_delta != expected_wallet_lamports
        || !account_funding_within_cap(
            &refundable_funding_total,
            &nonrefundable_total,
            intent.maximum_account_funding_lamports,
        )
    {
        return Err(invalid_proof_check("native_wallet_lamport_equation"));
    }
    let baseline = record
        .position_baseline
        .as_ref()
        .map(|baseline| baseline.anchors.as_slice())
        .unwrap_or(&[]);
    let position = candidate
        .stable_position_address
        .as_deref()
        .or(intent.position.as_deref());
    let liquidity_actions = is_liquidity_action(intent.action);
    if liquidity_actions {
        let position = position.ok_or_else(invalid_proof)?;
        let id = liquidity_position_id(intent.family.as_str(), position);
        let anchor_before = baseline.iter().find(|anchor| {
            anchor.position == id && anchor.position_kind == PositionKind::Liquidity
        });
        let units_before =
            candidate_position_units_before(candidate, intent.action, anchor_before)?;
        let delta = verified
            .receipt_delta_atoms
            .as_ref()
            .ok_or_else(invalid_proof)?;
        if intent.action == MeteoraAction::Open || intent.action == MeteoraAction::Add {
            if delta.is_negative() || delta.is_zero() {
                return Err(invalid_proof_check("positive_liquidity_units"));
            }
        } else if intent.action == MeteoraAction::Remove {
            if !delta.is_negative() {
                return Err(invalid_proof_check("negative_remove_units"));
            }
        } else if !delta.is_zero() {
            return Err(invalid_proof_check("unexpected_receipt_delta"));
        }
        let units_after = if delta.is_negative() {
            units_before
                .checked_sub(&delta.magnitude())
                .map_err(|_| invalid_proof())?
        } else {
            units_before.checked_add(&delta.magnitude())?
        };
        if intent.action == MeteoraAction::Close && !units_after.is_zero() {
            return Err(invalid_proof_check("close_residual_units"));
        }
        if intent.action != MeteoraAction::Close {
            let observed = verified
                .position
                .as_ref()
                .ok_or_else(|| invalid_proof_check("position_observation_missing"))?;
            let observed_slot = observed.observation_slot.or_else(|| {
                observed
                    .protocol_state
                    .get("observed_slot")
                    .and_then(Value::as_u64)
            });
            if !observed_slot.is_some_and(|observed_slot| observed_slot >= proof.slot) {
                return Err(invalid_proof_check("position_observation_slot"));
            }
            let observed_units = observed
                .liquidity_units_atoms
                .as_deref()
                .ok_or_else(|| invalid_proof_check("position_observed_units_missing"))?;
            if Quantity::new(observed_units)? != units_after {
                return Err(invalid_proof_check("position_observed_units_mismatch"));
            }
            if observed.token_a.mint != pool.token_a.mint
                || observed.token_a.token_program != pool.token_a.token_program
                || observed.token_a.decimals != pool.token_a.decimals
                || observed.token_b.mint != pool.token_b.mint
                || observed.token_b.token_program != pool.token_b.token_program
                || observed.token_b.decimals != pool.token_b.decimals
            {
                return Err(invalid_proof_check("position_observation_assets"));
            }
        }
        let asset = liquidity_asset(intent.family.as_str(), position);
        let old_evidence = anchor_before
            .and_then(|anchor| anchor.evidence.get("liquidity_position"))
            .and_then(|value| {
                serde_json::from_value::<LiquidityPositionEvidence>(value.clone()).ok()
            });
        let mut typed = if let Some(observed) = &verified.position {
            if observed.address != position
                || observed.pool != intent.pool
                || observed.owner != wallet_address
                || observed.family != intent.family
            {
                return Err(invalid_proof_check("position_observation_identity"));
            }
            LiquidityPositionEvidence {
                version: 1,
                family: observed.family.as_str().to_owned(),
                pool: observed.pool.clone(),
                position: observed.address.clone(),
                anchor_position_id: Some(liquidity_position_id(
                    observed.family.as_str(),
                    &observed.address,
                )),
                observed_slot: observed.observation_slot.or_else(|| {
                    observed
                        .protocol_state
                        .get("observed_slot")
                        .and_then(Value::as_u64)
                }),
                pre_transaction_units_atoms: Some(units_before.clone()),
                liquidity_units_atoms: Some(units_after.clone()),
                liquidity_units_known: true,
                historical_principal_known: old_evidence.as_ref().is_some_and(|evidence| {
                    liquidity_history_basis_matches(evidence, anchor_before, &units_before)
                }) || intent.action == MeteoraAction::Open,
                token_a: LiquidityCompositionAmount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: observed.token_a.mint.clone(),
                        decimals: observed.token_a.decimals,
                    },
                    token_program: observed.token_a.token_program.clone(),
                    atoms: observed
                        .token_a_atoms
                        .as_deref()
                        .map(Quantity::new)
                        .transpose()?,
                },
                token_b: LiquidityCompositionAmount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: observed.token_b.mint.clone(),
                        decimals: observed.token_b.decimals,
                    },
                    token_program: observed.token_b.token_program.clone(),
                    atoms: observed
                        .token_b_atoms
                        .as_deref()
                        .map(Quantity::new)
                        .transpose()?,
                },
                // This transaction changes the position state. A previous
                // snapshot's USD estimate must be refreshed from current
                // composition and prices before it is presented as value.
                backing_valuation: None,
                history: old_evidence
                    .as_ref()
                    .map(|evidence| evidence.history.clone())
                    .unwrap_or_default(),
            }
        } else if intent.action == MeteoraAction::Close {
            let mut evidence = old_evidence
                .clone()
                .unwrap_or_else(|| LiquidityPositionEvidence {
                    version: 1,
                    family: intent.family.as_str().to_owned(),
                    pool: intent.pool.clone(),
                    position: position.to_owned(),
                    anchor_position_id: Some(liquidity_position_id(
                        intent.family.as_str(),
                        position,
                    )),
                    observed_slot: None,
                    pre_transaction_units_atoms: Some(units_before.clone()),
                    liquidity_units_atoms: Some(units_after.clone()),
                    liquidity_units_known: true,
                    historical_principal_known: false,
                    token_a: LiquidityCompositionAmount {
                        asset: token_asset(&pool, true),
                        token_program: pool.token_a.token_program.clone(),
                        atoms: None,
                    },
                    token_b: LiquidityCompositionAmount {
                        asset: token_asset(&pool, false),
                        token_program: pool.token_b.token_program.clone(),
                        atoms: None,
                    },
                    backing_valuation: None,
                    history: Vec::new(),
                });
            evidence.observed_slot = None;
            evidence.token_a.atoms = None;
            evidence.token_b.atoms = None;
            evidence.backing_valuation = None;
            evidence
        } else {
            return Err(invalid_proof());
        };
        typed.liquidity_units_atoms = Some(units_after.clone());
        typed.liquidity_units_known = true;
        typed.pre_transaction_units_atoms = Some(units_before.clone());
        if verified.position.is_some() {
            typed.observed_slot = verified.position.as_ref().and_then(|observed| {
                observed.observation_slot.or_else(|| {
                    observed
                        .protocol_state
                        .get("observed_slot")
                        .and_then(Value::as_u64)
                })
            });
        }
        typed.history.push(LiquidityPositionEvent {
            signature: proof.signature.clone(),
            submission: Some(submission.clone()),
            slot,
            action: format!("{:?}", intent.action).to_ascii_lowercase(),
            liquidity_delta_atoms: delta.clone(),
            token_deltas: verified
                .position_asset_deltas
                .iter()
                .map(|row| LiquidityAssetDelta {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: row.mint.clone(),
                        decimals: row.decimals,
                    },
                    token_program: row.token_program.clone(),
                    delta: row.atoms.clone(),
                })
                .collect(),
            fees: verified
                .collected_fees
                .iter()
                .map(|row| Amount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: row.mint.clone(),
                        decimals: row.decimals,
                    },
                    atoms: row.atoms.magnitude().clone(),
                })
                .collect(),
            rewards: verified
                .collected_rewards
                .iter()
                .map(|row| Amount {
                    asset: AssetId {
                        network: gofin_solana::NETWORK.into(),
                        reference: row.mint.clone(),
                        decimals: row.decimals,
                    },
                    atoms: row.atoms.magnitude().clone(),
                })
                .collect(),
        });
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: id.clone(),
            position_kind: PositionKind::Liquidity,
            principal: Amount {
                asset: asset.clone(),
                atoms: units_after,
            },
            scaled_receipt: None,
            evidence_version: 3,
            evidence: json!({"version":3,"source":"finalized_meteora_liquidity_unit_delta","basis":"stable_protocol_liquidity_units","liquidity_units_known":true,"historical_principal_known":typed.historical_principal_known,"liquidity_position":typed,"protocol_event":verified.evidence,"signature":proof.signature,"slot":proof.slot}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: id,
            asset,
            delta: delta.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
    } else if verified
        .receipt_delta_atoms
        .as_ref()
        .is_some_and(|delta| !delta.is_zero())
    {
        return Err(invalid_proof());
    }
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: json!({"signature":proof.signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":proof.message_sha256,"protocol":verified.evidence,"proof":proof,"position_observation":verified.position,"settlement_source":"finalized_receipt_plus_validated_protocol_event"}),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = verified_effect_digest(&bundle)?;
    Ok(bundle)
}

fn account_funding_within_cap(
    refundable_funding: &BigInt,
    nonrefundable: &BigInt,
    cap: u64,
) -> bool {
    refundable_funding + nonrefundable <= BigInt::from(cap)
}

fn account_funding_debit(signed_rent_delta: &BigInt) -> BigInt {
    if signed_rent_delta.sign() == num_bigint::Sign::Plus {
        signed_rent_delta.clone()
    } else {
        BigInt::from(0)
    }
}

fn refundable_rent_credit_total(
    deltas: &[crate::protocol::VerifiedRentAccountDelta],
) -> Result<Quantity> {
    let total = deltas.iter().try_fold(BigInt::from(0), |total, delta| {
        if delta.atoms.is_negative() {
            total
                .checked_add(&BigInt::from(delta.atoms.magnitude().integer()))
                .ok_or_else(invalid_proof)
        } else {
            Ok(total)
        }
    })?;
    Quantity::new(total.to_string())
}

pub fn verify_failed(
    submission: &Submission,
    proof: &FinalizedReceiptProof,
    wallet_address: &str,
    maximum_network_fee_lamports: u64,
    fee_lamports: u64,
    slot: u64,
) -> Result<VerifiedEffects> {
    if !proof.failed
        || proof.slot != slot
        || proof.signature.is_empty()
        || fee_lamports > maximum_network_fee_lamports
        || submission.kind != "solana.transaction"
        || proof.message_sha256.is_empty()
        || submission.data["message_sha256"].as_str() != Some(proof.message_sha256.as_str())
        || !submission.data["wallet_signature"]
            .as_str()
            .is_some_and(|signature| {
                signature == proof.signature
                    || submission.data["chain_signature"].as_str() == Some(&proof.signature)
            })
    {
        return Err(invalid_proof());
    }
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
    let mut effects = Vec::new();
    if fee_lamports > 0 {
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash { resource },
            amount: Amount {
                asset: AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                atoms: Quantity::from_u64(fee_lamports),
            },
            category: "network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence: json!({"signature":proof.signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":proof.message_sha256,"failed_transaction":true,"proof":proof}),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = verified_effect_digest(&bundle)?;
    Ok(bundle)
}

fn token_by_mint<'a>(pool: &'a PoolInfo, mint: &str) -> Option<&'a crate::protocol::TokenInfo> {
    if pool.token_a.mint == mint {
        Some(&pool.token_a)
    } else if pool.token_b.mint == mint {
        Some(&pool.token_b)
    } else {
        None
    }
}

fn claim_credit_totals(
    fees: &[crate::protocol::VerifiedTokenDelta],
    rewards: &[crate::protocol::VerifiedTokenDelta],
    actual_deltas: &[crate::protocol::VerifiedTokenDelta],
    require_exact: bool,
) -> Result<std::collections::BTreeMap<(String, u8, String), BigInt>> {
    let mut classified = std::collections::BTreeMap::<(String, u8, String), BigInt>::new();
    for row in fees.iter().chain(rewards) {
        if row.atoms.is_negative() || row.atoms.is_zero() {
            return Err(invalid_proof());
        }
        *classified
            .entry((row.mint.clone(), row.decimals, row.token_program.clone()))
            .or_default() += row.atoms.integer();
    }
    for ((mint, decimals, token_program), amount) in &classified {
        let actual = actual_deltas
            .iter()
            .find(|delta| delta.mint == *mint)
            .ok_or_else(invalid_proof)?;
        if actual.decimals != *decimals
            || actual.token_program != *token_program
            || actual.atoms.is_negative()
            || actual.atoms.integer() < *amount
        {
            return Err(invalid_proof());
        }
    }
    if require_exact {
        for actual in actual_deltas {
            if actual.atoms.is_negative()
                || actual.atoms.integer()
                    != classified
                        .get(&(
                            actual.mint.clone(),
                            actual.decimals,
                            actual.token_program.clone(),
                        ))
                        .cloned()
                        .unwrap_or_default()
            {
                return Err(invalid_proof());
            }
        }
    }
    Ok(classified)
}

fn is_liquidity_action(action: MeteoraAction) -> bool {
    matches!(
        action,
        MeteoraAction::Open
            | MeteoraAction::Add
            | MeteoraAction::Remove
            | MeteoraAction::Collect
            | MeteoraAction::Close
    )
}

fn candidate_position_units_before(
    candidate: &ProtocolCandidate,
    action: MeteoraAction,
    anchor_before: Option<&PositionAnchor>,
) -> Result<Quantity> {
    let before = candidate
        .metadata
        .get("position_units_before_atoms")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_proof_check("position_pre_state_missing"))
        .and_then(Quantity::new)?;
    if action == MeteoraAction::Open {
        if !before.is_zero() {
            return Err(invalid_proof_check("open_position_pre_state"));
        }
        if let Some(anchor) = anchor_before {
            let known_zero = anchor.principal.atoms.is_zero()
                && anchor.evidence.get("source").and_then(Value::as_str)
                    == Some("verified_zero_before_position_open");
            if !known_zero {
                return Err(invalid_proof_check("open_position_baseline"));
            }
        }
        return Ok(Quantity::zero());
    }
    // The anchor remains required for position identity and history, but its
    // unit count may predate an external position change. The exact candidate
    // pre-state is bound to its finalized post-state proof below.
    anchor_before.ok_or_else(invalid_proof)?;
    Ok(before)
}

fn liquidity_history_basis_matches(
    evidence: &LiquidityPositionEvidence,
    anchor_before: Option<&PositionAnchor>,
    candidate_units_before: &Quantity,
) -> bool {
    evidence.historical_principal_known
        && anchor_before.is_some_and(|anchor| &anchor.principal.atoms == candidate_units_before)
}

fn invalid_proof() -> GoFinError {
    GoFinError::new(
        "METEORA_FINALIZED_PROOF_INVALID",
        "reconciliation",
        "The finalized Meteora transaction did not prove the exact approved two-token position effects.",
        "Keep the operation unresolved and reconcile the same submission identity.",
    )
}

fn invalid_proof_check(check: &'static str) -> GoFinError {
    let mut error = invalid_proof();
    error.details = Some(Box::new(json!({"check": check})));
    error
}

fn owner_token_deltas_match(
    actual: &[gofin_solana::transaction::OwnerTokenDelta],
    reported: &[crate::protocol::VerifiedTokenDelta],
    position_mint: Option<&str>,
) -> bool {
    let actual = actual
        .iter()
        .filter(|delta| !delta.atoms.is_zero() && Some(delta.mint.as_str()) != position_mint)
        .collect::<Vec<_>>();
    let reported = reported
        .iter()
        .filter(|delta| !delta.atoms.is_zero() && Some(delta.mint.as_str()) != position_mint)
        .collect::<Vec<_>>();
    let actual_mints = actual
        .iter()
        .map(|delta| delta.mint.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let reported_mints = reported
        .iter()
        .map(|delta| delta.mint.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    actual.len() == actual_mints.len()
        && reported.len() == reported_mints.len()
        && actual.len() == reported.len()
        && actual.iter().all(|actual| {
            reported.iter().any(|reported| {
                reported.mint == actual.mint
                    && reported.decimals == actual.decimals
                    && reported.token_program == actual.token_program
                    && reported.atoms == actual.atoms
            })
        })
}

fn position_nft_receipt_delta_matches(
    receipt: &[gofin_solana::transaction::OwnerTokenDelta],
    position_mint: &str,
    token_program: &str,
    decimals: u8,
    expected_delta: i128,
) -> bool {
    let mut matches = receipt.iter().filter(|delta| delta.mint == position_mint);
    let row = matches.next();
    let actual = row.and_then(|delta| delta.atoms.as_str().parse::<i128>().ok());
    matches.next().is_none()
        && match row {
            Some(delta) => {
                delta.token_program == token_program
                    && delta.decimals == decimals
                    && actual == Some(expected_delta)
            }
            None => expected_delta == 0,
        }
}

fn reported_position_nft_delta_matches(
    reported: &[crate::protocol::VerifiedTokenDelta],
    position_mint: &str,
    token_program: &str,
    decimals: u8,
    expected_delta: i128,
) -> bool {
    let mut matches = reported.iter().filter(|delta| delta.mint == position_mint);
    let row = matches.next();
    let actual = row.and_then(|delta| delta.atoms.as_str().parse::<i128>().ok());
    matches.next().is_none()
        && match row {
            Some(delta) => {
                delta.token_program == token_program
                    && delta.decimals == decimals
                    && actual == Some(expected_delta)
            }
            None => true,
        }
}

fn position_asset_deltas_match_wallet_cashflow(
    position_deltas: &[crate::protocol::VerifiedTokenDelta],
    wallet_deltas: &[crate::protocol::VerifiedTokenDelta],
    pool_tokens: [(&str, u8, &str); 2],
) -> bool {
    let mut expected = std::collections::BTreeMap::<String, (u8, String, BigInt)>::new();
    let mut reported = std::collections::BTreeMap::<String, (u8, String, BigInt)>::new();
    let mut seen_position_mints = std::collections::BTreeSet::new();

    for row in wallet_deltas {
        let Some((_, decimals, token_program)) =
            pool_tokens.iter().find(|(mint, _, _)| *mint == row.mint)
        else {
            continue;
        };
        if row.decimals != *decimals || row.token_program != *token_program {
            return false;
        }
        let inverse = -row.atoms.integer();
        if inverse != BigInt::from(0)
            && expected
                .insert(
                    row.mint.clone(),
                    (*decimals, (*token_program).to_owned(), inverse),
                )
                .is_some()
        {
            return false;
        }
    }
    for row in position_deltas {
        if !seen_position_mints.insert(row.mint.as_str()) {
            return false;
        }
        let Some((_, decimals, token_program)) =
            pool_tokens.iter().find(|(mint, _, _)| *mint == row.mint)
        else {
            return false;
        };
        if row.decimals != *decimals || row.token_program != *token_program {
            return false;
        }
        if !row.atoms.is_zero()
            && reported
                .insert(
                    row.mint.clone(),
                    (*decimals, (*token_program).to_owned(), row.atoms.integer()),
                )
                .is_some()
        {
            return false;
        }
    }
    expected == reported
}

#[cfg(test)]
mod settlement_accounting_tests {
    use super::*;

    #[test]
    fn close_refund_is_not_limited_by_new_account_funding_cap() {
        let rent_delta = BigInt::from(-2_100_000_000i64);
        let funding = account_funding_debit(&rent_delta);
        assert_eq!(funding, BigInt::from(0));
        assert!(account_funding_within_cap(
            &funding,
            &BigInt::from(0),
            50_000_000
        ));
    }

    #[test]
    fn close_rent_refunds_produce_one_aggregate_wallet_credit() {
        use crate::protocol::VerifiedRentAccountDelta;

        let deltas = [
            VerifiedRentAccountDelta {
                account: "receipt".into(),
                position: "account-funding:receipt".into(),
                atoms: SignedQuantity::new("-4127280").unwrap(),
            },
            VerifiedRentAccountDelta {
                account: "position".into(),
                position: "account-funding:position".into(),
                atoms: SignedQuantity::new("-3730560").unwrap(),
            },
            VerifiedRentAccountDelta {
                account: "ata".into(),
                position: "account-funding:ata".into(),
                atoms: SignedQuantity::new("-2039280").unwrap(),
            },
        ];

        assert_eq!(
            refundable_rent_credit_total(&deltas).unwrap(),
            Quantity::new("9897120").unwrap()
        );
    }

    #[test]
    fn owner_token_proof_ignores_zero_temporary_wsol_but_rejects_unexplained_debit() {
        use gofin_solana::transaction::OwnerTokenDelta;
        let actual = vec![OwnerTokenDelta {
            mint: "usdc".into(),
            decimals: 6,
            token_program: "token".into(),
            atoms: SignedQuantity::new("100").unwrap(),
        }];
        let reported = vec![
            crate::protocol::VerifiedTokenDelta {
                mint: "wsol".into(),
                decimals: 9,
                token_program: "token".into(),
                atoms: SignedQuantity::new("0").unwrap(),
            },
            crate::protocol::VerifiedTokenDelta {
                mint: "usdc".into(),
                decimals: 6,
                token_program: "token".into(),
                atoms: SignedQuantity::new("100").unwrap(),
            },
        ];
        assert!(owner_token_deltas_match(&actual, &reported, None));
        assert!(owner_token_deltas_match(&[], &[reported[0].clone()], None));

        let mut unexplained = reported;
        unexplained[0].atoms = SignedQuantity::new("-1").unwrap();
        assert!(!owner_token_deltas_match(&actual, &unexplained, None));
    }

    #[test]
    fn position_nft_receipt_is_bound_and_excluded_from_fungible_deltas() {
        use gofin_domain::SignedQuantity;
        use gofin_solana::transaction::OwnerTokenDelta;
        let token_program = gofin_solana::token::TOKEN_2022_PROGRAM_ID;

        let open_receipt = vec![OwnerTokenDelta {
            mint: "position-mint".into(),
            decimals: 0,
            token_program: token_program.into(),
            atoms: SignedQuantity::new("1").unwrap(),
        }];
        assert!(position_nft_receipt_delta_matches(
            &open_receipt,
            "position-mint",
            token_program,
            0,
            1
        ));
        assert!(!position_nft_receipt_delta_matches(
            &open_receipt,
            "position-mint",
            token_program,
            0,
            0
        ));
        assert!(position_nft_receipt_delta_matches(
            &[],
            "position-mint",
            token_program,
            0,
            0
        ));
        assert!(!position_nft_receipt_delta_matches(
            &[],
            "position-mint",
            token_program,
            0,
            -1
        ));
        assert!(!position_nft_receipt_delta_matches(
            &open_receipt,
            "position-mint",
            gofin_solana::token::TOKEN_PROGRAM_ID,
            0,
            1
        ));
        assert!(reported_position_nft_delta_matches(
            &[],
            "position-mint",
            token_program,
            0,
            0
        ));
        assert!(reported_position_nft_delta_matches(
            &[],
            "position-mint",
            token_program,
            0,
            1
        ));
        let reported = [crate::protocol::VerifiedTokenDelta {
            mint: "position-mint".into(),
            decimals: 0,
            token_program: token_program.into(),
            atoms: SignedQuantity::new("1").unwrap(),
        }];
        assert!(reported_position_nft_delta_matches(
            &reported,
            "position-mint",
            token_program,
            0,
            1
        ));
        assert!(!reported_position_nft_delta_matches(
            &reported,
            "position-mint",
            token_program,
            0,
            0
        ));
    }

    fn delta(mint: &str, atoms: &str) -> crate::protocol::VerifiedTokenDelta {
        crate::protocol::VerifiedTokenDelta {
            mint: mint.into(),
            decimals: 6,
            token_program: "TokenProgram".into(),
            atoms: SignedQuantity::new(atoms).unwrap(),
        }
    }

    fn cashflow_delta(
        mint: &str,
        decimals: u8,
        token_program: &str,
        atoms: &str,
    ) -> crate::protocol::VerifiedTokenDelta {
        crate::protocol::VerifiedTokenDelta {
            mint: mint.into(),
            decimals,
            token_program: token_program.into(),
            atoms: SignedQuantity::new(atoms).unwrap(),
        }
    }

    #[test]
    fn liquidity_history_matches_inverse_wallet_cashflow_not_total_pool_reserves() {
        let pool = [("mint-a", 9, "token-a"), ("mint-b", 6, "token-b")];
        let wallet_contribution = vec![
            cashflow_delta("mint-a", 9, "token-a", "-100"),
            cashflow_delta("mint-b", 6, "token-b", "-20"),
        ];
        let contribution_history = vec![
            cashflow_delta("mint-a", 9, "token-a", "100"),
            cashflow_delta("mint-b", 6, "token-b", "20"),
        ];
        assert!(position_asset_deltas_match_wallet_cashflow(
            &contribution_history,
            &wallet_contribution,
            pool,
        ));

        let wallet_withdrawal = vec![
            cashflow_delta("mint-a", 9, "token-a", "50"),
            cashflow_delta("mint-b", 6, "token-b", "10"),
        ];
        let withdrawal_history = vec![
            cashflow_delta("mint-a", 9, "token-a", "-50"),
            cashflow_delta("mint-b", 6, "token-b", "-10"),
        ];
        assert!(position_asset_deltas_match_wallet_cashflow(
            &withdrawal_history,
            &wallet_withdrawal,
            pool,
        ));

        // DAMM's total pool reserve fields are much larger than this user's
        // finalized transaction flow and must never be accepted as principal.
        let reserve_totals = vec![
            cashflow_delta("mint-a", 9, "token-a", "900000000000"),
            cashflow_delta("mint-b", 6, "token-b", "800000000000"),
        ];
        assert!(!position_asset_deltas_match_wallet_cashflow(
            &reserve_totals,
            &wallet_contribution,
            pool,
        ));
    }

    #[test]
    fn liquidity_history_cashflow_rejects_identity_gaps_and_duplicates() {
        let pool = [("mint-a", 9, "token-a"), ("mint-b", 6, "token-b")];
        let wallet = vec![cashflow_delta("mint-a", 9, "token-a", "-100")];
        assert!(!position_asset_deltas_match_wallet_cashflow(
            &[],
            &wallet,
            pool,
        ));
        assert!(!position_asset_deltas_match_wallet_cashflow(
            &[cashflow_delta("mint-a", 8, "token-a", "100")],
            &wallet,
            pool,
        ));
        assert!(!position_asset_deltas_match_wallet_cashflow(
            &[cashflow_delta("mint-a", 9, "foreign-token", "100")],
            &wallet,
            pool,
        ));
        assert!(!position_asset_deltas_match_wallet_cashflow(
            &[
                cashflow_delta("mint-a", 9, "token-a", "100"),
                cashflow_delta("mint-a", 9, "token-a", "0"),
            ],
            &wallet,
            pool,
        ));
    }

    #[test]
    fn fee_and_reward_categories_for_same_mint_sum_before_reconciling_wallet_credit() {
        let fee = delta("reward-mint", "3");
        let reward = delta("reward-mint", "4");
        let actual = delta("reward-mint", "7");
        let totals = claim_credit_totals(
            std::slice::from_ref(&fee),
            std::slice::from_ref(&reward),
            std::slice::from_ref(&actual),
            true,
        )
        .unwrap();
        assert_eq!(
            totals[&("reward-mint".into(), 6, "TokenProgram".into())],
            BigInt::from(7)
        );

        assert!(
            claim_credit_totals(&[fee], &[reward], &[delta("reward-mint", "6")], true).is_err()
        );
    }

    #[test]
    fn collect_requires_all_wallet_credit_to_be_classified() {
        let fee = delta("pool-mint", "5");
        assert!(
            claim_credit_totals(
                std::slice::from_ref(&fee),
                &[],
                &[delta("pool-mint", "5")],
                true
            )
            .is_ok()
        );
        assert!(
            claim_credit_totals(
                &[fee],
                &[],
                &[delta("pool-mint", "5"), delta("unclassified", "1")],
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn collect_is_recorded_as_a_liquidity_position_event() {
        assert!(is_liquidity_action(MeteoraAction::Collect));
        assert!(!is_liquidity_action(MeteoraAction::Swap));
    }

    #[test]
    fn settlement_uses_candidate_prestate_when_snapshot_units_are_stale() {
        let candidate: ProtocolCandidate = serde_json::from_value(json!({
            "quote": {
                "family":"dlmm",
                "action":"add",
                "pool":"pool",
                "expires_at":"2026-10-14T00:00:30Z"
            },
            "instructions":[],
            "blockhash":"blockhash",
            "last_valid_block_height":42,
            "metadata":{"position_units_before_atoms":"12"}
        }))
        .unwrap();
        let asset = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "meteora:liquidity:dlmm:position".into(),
            decimals: 0,
        };
        let anchor = PositionAnchor {
            resource: ResourceId("solana:mainnet-beta:wallet".into()),
            position: "meteora:liquidity:dlmm:position".into(),
            position_kind: PositionKind::Liquidity,
            principal: Amount {
                asset: asset.clone(),
                atoms: Quantity::new("7").unwrap(),
            },
            scaled_receipt: None,
            evidence_version: 3,
            evidence: json!({"historical_principal_known":true}),
            observed_at: "2026-10-14T00:00:00Z".into(),
        };
        let before =
            candidate_position_units_before(&candidate, MeteoraAction::Add, Some(&anchor)).unwrap();
        assert_eq!(before.as_str(), "12");
        let old_evidence = LiquidityPositionEvidence {
            version: 1,
            family: "dlmm".into(),
            pool: "pool".into(),
            position: "position".into(),
            anchor_position_id: Some("meteora:liquidity:dlmm:position".into()),
            observed_slot: Some(10),
            pre_transaction_units_atoms: Some(Quantity::new("7").unwrap()),
            liquidity_units_atoms: Some(Quantity::new("7").unwrap()),
            liquidity_units_known: true,
            historical_principal_known: true,
            token_a: LiquidityCompositionAmount {
                asset: asset.clone(),
                token_program: "token".into(),
                atoms: None,
            },
            token_b: LiquidityCompositionAmount {
                asset,
                token_program: "token".into(),
                atoms: None,
            },
            backing_valuation: None,
            history: Vec::new(),
        };
        assert!(!liquidity_history_basis_matches(
            &old_evidence,
            Some(&anchor),
            &before
        ));

        let mut observed_anchor = anchor;
        observed_anchor.principal.atoms = Quantity::new("12").unwrap();
        assert!(liquidity_history_basis_matches(
            &old_evidence,
            Some(&observed_anchor),
            &before
        ));
    }

    #[test]
    fn unknown_position_composition_survives_serialization_as_unknown() {
        let observation = LiquidityCompositionAmount {
            asset: AssetId {
                network: "solana".into(),
                reference: "mint".into(),
                decimals: 6,
            },
            token_program: "TokenProgram".into(),
            atoms: None,
        };
        let value = serde_json::to_value(&observation).unwrap();
        assert!(value["atoms"].is_null());
        let restored: LiquidityCompositionAmount = serde_json::from_value(value).unwrap();
        assert_eq!(restored.atoms, None);
    }

    #[test]
    fn imported_position_snapshot_preserves_unknown_basis_and_refundable_rent() {
        use crate::protocol::{PositionRentAccount, ProtocolFamily, TokenInfo};
        let address = "11111111111111111111111111111111";
        let position = PositionInfo {
            family: ProtocolFamily::DammV2,
            address: address.into(),
            pool: "Stake11111111111111111111111111111111111111".into(),
            owner: address.into(),
            observation_slot: Some(42),
            rent_accounts: vec![PositionRentAccount {
                account: address.into(),
                lamports: 2_039_280,
            }],
            token_a: TokenInfo {
                mint: "So11111111111111111111111111111111111111112".into(),
                decimals: 9,
                token_program: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
                decimals: 6,
                token_program: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into(),
                transfer_fee: None,
            },
            receipt_mint: Some(address.into()),
            receipt_atoms: Some("1".into()),
            liquidity_units_atoms: Some("12".into()),
            token_a_atoms: None,
            token_b_atoms: Some("5".into()),
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            bin_lower: None,
            bin_upper: None,
            protocol_state: json!({}),
        };
        let states = snapshot_anchors("main", address, &position).unwrap();
        assert_eq!(states.len(), 2);
        let liquidity = states
            .iter()
            .find(|state| state.kind == PositionKind::Liquidity)
            .unwrap();
        let evidence: LiquidityPositionEvidence = serde_json::from_value(
            liquidity.anchor.as_ref().unwrap().evidence["liquidity_position"].clone(),
        )
        .unwrap();
        assert!(!evidence.historical_principal_known);
        assert_eq!(evidence.observed_slot, Some(42));
        assert_eq!(evidence.token_a.atoms, None);
        assert_eq!(evidence.token_b.atoms, Some(Quantity::from_u64(5)));
        let rent = states
            .iter()
            .find(|state| state.kind == PositionKind::RefundableAccountFunding)
            .unwrap();
        assert_eq!(rent.amount.atoms, Quantity::from_u64(2_039_280));
    }

    #[test]
    fn failed_finalized_transaction_charges_only_the_proven_network_fee() {
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: "finalized-signature".into(),
            data: json!({
                "message_sha256":"message-hash",
                "wallet_signature":"finalized-signature"
            }),
        };
        let proof = FinalizedReceiptProof {
            signature: "finalized-signature".into(),
            slot: 42,
            fee_lamports: 5_000,
            failed: true,
            message_sha256: "message-hash".into(),
            account_keys: vec![],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![],
            post_balances_lamports: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: Some(json!({"InstructionError":[0,"Custom"]})),
        };
        let verified =
            verify_failed(&submission, &proof, "wallet-address", 10_000, 5_000, 42).unwrap();
        assert_eq!(verified.effects.len(), 1);
        assert!(matches!(
            &verified.effects[0],
            FinancialEffect::Cost { amount, category, .. }
                if amount.asset.reference == "native"
                    && amount.atoms == Quantity::from_u64(5_000)
                    && category == "network_fee"
        ));
        assert!(verify_failed(&submission, &proof, "wallet-address", 4_999, 5_000, 42,).is_err());
    }
}
