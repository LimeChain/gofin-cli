//! Exact collateral units and native debt evidence; valuation is observational.
use super::{
    adapter::{
        closing_rent_baselines, complete_before, fraction, invalid, rent_limits, target_side,
        validate_obligation,
    },
    types::*,
};
use crate::adapter::{asset, atoms, native_asset, resource};
use chrono::Utc;
use gofin_domain::*;
use gofin_pricing::UsdPriceSource;
use gofin_solana::FinalizedReceiptProof;
use num_bigint::{BigInt, BigUint};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn position_id(
    market: &str,
    obligation: &str,
    reserve: &str,
    side: LendingSide,
) -> String {
    format!(
        "kamino:lending:{market}:{obligation}:{reserve}:{}",
        if side == LendingSide::Supply {
            "supply"
        } else {
            "debt"
        }
    )
}
pub(crate) fn position_asset(row: &LendingPosition) -> Result<AssetId> {
    if row.side == LendingSide::Supply {
        Ok(asset(row.collateral_token.as_ref().ok_or_else(|| {
            invalid("Supply collateral mint missing.")
        })?))
    } else {
        Ok(asset(&row.token))
    }
}
pub(crate) fn position_atoms(row: &LendingPosition) -> Result<Quantity> {
    if row.side == LendingSide::Supply {
        atoms(&row.native_units)
    } else {
        let raw = fraction(&row.native_units)?.integer();
        let scale = BigUint::from(1u64) << 60_usize;
        let rounded = (&raw + &scale - BigUint::from(1u8)) / scale;
        let amount = Quantity::new(rounded.to_string())?;
        if row
            .underlying_atoms
            .as_deref()
            .is_some_and(|n| n != amount.as_str())
        {
            return Err(invalid(
                "Debt ceil atoms disagree with exact native debt fraction.",
            ));
        }
        Ok(amount)
    }
}
pub(crate) fn snapshot_positions(obligation: &ObligationInfo) -> Result<Vec<PositionState>> {
    let mut states = Vec::new();
    for row in &obligation.positions {
        if row.side == LendingSide::Debt && row.protocol_state["debt_kind"] != "floating" {
            continue;
        }
        let id = position_id(
            &obligation.market,
            &obligation.address,
            &row.reserve,
            row.side,
        );
        let kind = if row.side == LendingSide::Supply {
            PositionKind::Supply
        } else {
            PositionKind::VariableDebt
        };
        let amount = Amount {
            asset: position_asset(row)?,
            atoms: position_atoms(row)?,
        };
        let mut evidence = json!({"version":1,"source":"klend_obligation_observation","market":obligation.market,"obligation":obligation.address,"reserve":row.reserve,"owner":obligation.owner,"side":row.side,"native_units":row.native_units,"cumulative_borrow_rate_bsf":row.cumulative_borrow_rate_bsf,"observed_slot":obligation.observed_slot,"historical_principal_known":false,"protocol_state":row.protocol_state,"known_unpaid_interest_atoms":"0"});
        if row.side == LendingSide::Supply {
            let token = row
                .collateral_token
                .as_ref()
                .ok_or_else(|| invalid("Missing collateral identity."))?;
            evidence["lending_position"] = json!(LendingPositionEvidence {
                version: 1,
                market: obligation.market.clone(),
                obligation: obligation.address.clone(),
                reserve: row.reserve.clone(),
                owner: obligation.owner.clone(),
                collateral_asset: asset(token),
                collateral_token_program: token.token_program.clone(),
                collateral_atoms: atoms(&row.native_units)?,
                underlying: LiquidityCompositionAmount {
                    asset: asset(&row.token),
                    token_program: row.token.token_program.clone(),
                    atoms: row.underlying_atoms.as_deref().map(atoms).transpose()?
                },
                observed_slot: obligation.observed_slot,
                observed_at: obligation.observed_at.clone(),
                historical_principal_known: false,
                backing_valuation: None,
                pre_transaction_collateral_atoms: None,
                finalized_submission: None,
                collateral_delta_atoms: None
            });
        }
        let scaled = (row.side == LendingSide::Debt)
            .then(|| fraction(&row.native_units))
            .transpose()?;
        let anchor = PositionAnchor {
            resource: resource(&obligation.owner),
            position: id.clone(),
            position_kind: kind.clone(),
            principal: amount.clone(),
            scaled_receipt: scaled.clone(),
            evidence_version: 1,
            evidence,
            observed_at: obligation.observed_at.clone(),
        };
        states.push(PositionState {
            resource: resource(&obligation.owner),
            position: id,
            kind,
            amount,
            scaled_receipt: scaled,
            anchor: Some(anchor),
        });
    }
    Ok(states)
}
pub(crate) async fn value_position(
    state: &mut PositionState,
    prices: &dyn UsdPriceSource,
) -> Result<()> {
    let Some(anchor) = state.anchor.as_mut() else {
        return Ok(());
    };
    if state.kind != PositionKind::Supply {
        return Ok(());
    }
    let mut typed: LendingPositionEvidence =
        serde_json::from_value(anchor.evidence["lending_position"].clone())
            .map_err(|_| invalid("Invalid collateral backing evidence."))?;
    if let Some(units) = &typed.underlying.atoms
        && let Ok(quote) = prices.quote(&typed.underlying.asset).await
        && let Ok(price) = Quantity::from_decimal(&quote.usd_per_unit, 36)
        && !price.is_zero()
    {
        let unit = Quantity::new(format!(
            "1{}",
            "0".repeat(usize::from(typed.underlying.asset.decimals) + 30)
        ))?;
        typed.backing_valuation = Some(VaultBackingValuation {
            total_usd_micros: units.mul_div_ceil(&price, &unit)?,
            observed_at: quote.updated_at,
            source: quote.source,
            complete: false,
            basis: "observed_underlying_principal_only".into(),
            excluded_components: vec!["unclaimed rewards".into()],
        });
    }
    anchor.evidence["lending_position"] = json!(typed);
    Ok(())
}
pub(crate) async fn public_obligation(
    obligation: &ObligationInfo,
    prices: &dyn UsdPriceSource,
) -> Result<Value> {
    let mut value = json!(obligation);
    let mut collateral = Quantity::zero();
    let mut debt = Quantity::zero();
    let mut missing = Vec::new();
    let mut collateral_complete = obligation.complete;
    let mut debt_complete = obligation.complete;
    if !obligation.complete {
        missing.push("obligation inventory is incomplete".into());
    }
    let mut sources = BTreeSet::new();
    for row in &obligation.positions {
        let side_complete = if row.side == LendingSide::Supply {
            &mut collateral_complete
        } else {
            &mut debt_complete
        };
        let Some(units) = row.underlying_atoms.as_deref() else {
            *side_complete = false;
            missing.push(format!("{} {:?} underlying amount", row.reserve, row.side));
            continue;
        };
        let token = asset(&row.token);
        let Ok(quote) = prices.quote(&token).await else {
            *side_complete = false;
            missing.push(format!("price for {}", token.reference));
            continue;
        };
        let Ok(price) = Quantity::from_decimal(&quote.usd_per_unit, 36) else {
            *side_complete = false;
            missing.push(format!("price for {}", token.reference));
            continue;
        };
        if price.is_zero() {
            *side_complete = false;
            missing.push(format!("price for {}", token.reference));
            continue;
        }
        let unit = Quantity::new(format!("1{}", "0".repeat(usize::from(token.decimals) + 30)))?;
        let amount = atoms(units)?.mul_div_ceil(&price, &unit)?;
        if row.side == LendingSide::Supply {
            collateral = collateral.checked_add(&amount)?;
        } else {
            debt = debt.checked_add(&amount)?;
        }
        sources.insert(quote.source);
    }
    let underlying_complete = obligation.complete && missing.is_empty();
    let excluded = [
        "unclaimed rewards",
        "interest after the observed obligation/reserve state",
    ];
    value["current_value"] = json!({"collateral_usd_micros":collateral_complete.then_some(&collateral),"debt_usd_micros":debt_complete.then_some(&debt),"net_usd_micros":underlying_complete.then(||(BigInt::from(collateral.integer())-BigInt::from(debt.integer())).to_string()),"known_collateral_subtotal_usd_micros":collateral,"known_debt_subtotal_usd_micros":debt,"complete":false,"underlying_valuation_complete":underlying_complete,"basis":"observed_underlying_principal_and_stored_debt","excluded_components":excluded,"missing_data":missing,"sources":sources,"observed_at":obligation.observed_at,"observed_slot":obligation.observed_slot});
    Ok(value)
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
fn shift(before: &Quantity, delta: &SignedQuantity) -> Result<Quantity> {
    if delta.is_negative() {
        before.checked_sub(&delta.magnitude())
    } else {
        before.checked_add(&delta.magnitude())
    }
}
fn zero_after(before: &LendingPosition) -> LendingPosition {
    let mut row = before.clone();
    row.native_units = "0".into();
    row.underlying_atoms = Some("0".into());
    row.cumulative_borrow_rate_bsf = None;
    row
}

pub(crate) fn verify(
    record: &OperationRecord,
    intent: &LendingIntent,
    candidate: &LendingCandidate,
    verified: &VerifiedLendingEffects,
    proof: &FinalizedReceiptProof,
) -> Result<VerifiedEffects> {
    let submission = record
        .submission
        .as_ref()
        .ok_or_else(|| invalid("Missing lending submission."))?;
    let prepared = record
        .preview
        .as_ref()
        .ok_or_else(|| invalid("Missing lending approval preview."))?;
    let owner = prepared.binding["wallet_address"]
        .as_str()
        .ok_or_else(|| invalid("Missing lending wallet."))?;
    if proof.failed
        || proof.message_sha256 != submission.data["message_sha256"].as_str().unwrap_or("")
        || submission.reference != format!("solana:message:{}", proof.message_sha256)
        || submission.data["wallet_signature"].as_str() != Some(proof.signature.as_str())
        || proof.fee_lamports > intent.maximum_network_fee_lamports
        || proof.required_signatures != [proof.signature.clone()]
        || proof.account_keys.first().map(String::as_str) != Some(owner)
        || !gofin_solana::transaction::normalized_instruction_graph_matches(
            &candidate.instructions,
            &proof.message_instructions,
            owner,
        )
    {
        return Err(invalid(
            "Finalized lending identity, message, signer or fee differs from approved submission.",
        ));
    }
    validate_obligation(
        &verified.obligation_after,
        owner,
        &intent.market,
        &intent.obligation,
    )?;
    if !verified.obligation_after.complete
        || verified.obligation_after.observed_slot < proof.slot
        || !verified.evidence.is_object()
    {
        return Err(invalid(
            "Attributable finalized obligation evidence is incomplete.",
        ));
    }
    let wallet = resource(owner);
    let underlying = asset(&intent.token);
    let cash = AccountingLocation::Cash {
        resource: wallet.clone(),
    };
    let mut effects = Vec::new();
    let delta = proof.token_delta_verified(
        owner,
        &intent.token.mint,
        &intent.token.token_program,
        intent.token.decimals,
    )?;
    let observed = proof.all_owner_token_deltas_verified(owner)?;
    if observed.iter().any(|row| {
        !row.atoms.is_zero()
            && (row.mint != intent.token.mint
                || row.decimals != intent.token.decimals
                || row.token_program != intent.token.token_program)
    }) {
        return Err(invalid(
            "Lending transaction changed an unapproved wallet token.",
        ));
    }
    if verified
        .token_deltas
        .iter()
        .any(|r| r.token != intent.token || r.atoms != delta)
        || verified.token_deltas.len() > 1
        || (!delta.is_zero() && verified.token_deltas.is_empty())
    {
        return Err(invalid(
            "Protocol cashflow differs from finalized token evidence.",
        ));
    }
    let amount = atoms(&intent.amount_atoms)?;
    match intent.action {
        LendingAction::Supply
            if !delta.is_negative() || delta.is_zero() || delta.magnitude() > amount =>
        {
            return Err(invalid(
                "Supply debit is not positive or exceeds the approved input maximum.",
            ));
        }
        LendingAction::Repay if !delta.is_negative() || delta.magnitude() > amount => {
            return Err(invalid("Repayment debit exceeds finite approval."));
        }
        LendingAction::Borrow if delta.is_negative() || delta.magnitude() != amount => {
            return Err(invalid("Borrow receipt differs from native exact amount."));
        }
        LendingAction::Withdraw
            if delta.is_negative()
                || delta.magnitude()
                    < atoms(
                        intent
                            .minimum_receive_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Missing withdrawal receipt floor."))?,
                    )? =>
        {
            return Err(invalid("Withdrawal receipt is below the reviewed floor."));
        }
        _ => {}
    }
    if !delta.is_zero() {
        let actual = Amount {
            asset: underlying.clone(),
            atoms: delta.magnitude(),
        };
        effects.push(if delta.is_negative() {
            FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount: actual,
            }
        } else {
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount: actual,
            }
        });
    }
    let before = complete_before(candidate, intent.action);
    let prior: Vec<PositionAnchor> =
        serde_json::from_value(prepared.binding["prior_anchors"].clone())
            .map_err(|_| invalid("Missing protected lending anchor context."))?;
    let mut seen = BTreeSet::new();
    let mut selected_delta = None;
    for transition in &verified.transitions {
        if !seen.insert((&transition.reserve, transition.side)) {
            return Err(invalid("Duplicate finalized lending transition."));
        }
        let before_row = before
            .positions
            .iter()
            .find(|p| p.reserve == transition.reserve && p.side == transition.side)
            .ok_or_else(|| invalid("Finalized transition has no approved native prestate."))?;
        let after_row = verified
            .obligation_after
            .positions
            .iter()
            .find(|p| p.reserve == transition.reserve && p.side == transition.side)
            .cloned()
            .unwrap_or_else(|| zero_after(before_row));
        if transition.native_units_before != before_row.native_units
            || transition.native_units_after != after_row.native_units
            || asset(&transition.token) != position_asset(before_row)?
            || position_asset(&after_row)? != position_asset(before_row)?
        {
            return Err(invalid(
                "Native transition changed exact reserve, unit or asset identity.",
            ));
        }
        let selected =
            transition.reserve == intent.reserve && transition.side == target_side(intent.action);
        if !selected && !transition.action_delta_atoms.is_zero() {
            return Err(invalid(
                "Unapproved principal movement in another obligation reserve.",
            ));
        }
        if selected {
            selected_delta = Some(transition.action_delta_atoms.clone());
        }
        let id = position_id(
            &intent.market,
            &intent.obligation,
            &transition.reserve,
            transition.side,
        );
        let mut post = verified.obligation_after.clone();
        post.positions = vec![after_row.clone()];
        let mut state = snapshot_positions(&post)?.remove(0);
        let anchor = state
            .anchor
            .as_mut()
            .ok_or_else(|| invalid("Missing finalized anchor."))?;
        anchor.evidence["source"] = json!("finalized_klend_transaction");
        anchor.evidence["protocol_event"] = transition.evidence.clone();
        anchor.evidence["signature"] = json!(proof.signature);
        if transition.side == LendingSide::Supply {
            if !selected
                || transition
                    .accrual_delta_atoms
                    .as_ref()
                    .is_some_and(|a| !a.is_zero())
                || shift(
                    &atoms(&before_row.native_units)?,
                    &transition.action_delta_atoms,
                )? != atoms(&after_row.native_units)?
            {
                return Err(invalid(
                    "Collateral accounting must prove exact native units without fabricated underlying accrual.",
                ));
            }
            let mut typed: LendingPositionEvidence =
                serde_json::from_value(anchor.evidence["lending_position"].clone())
                    .map_err(|_| invalid("Invalid collateral anchor."))?;
            typed.pre_transaction_collateral_atoms = Some(atoms(&before_row.native_units)?);
            typed.finalized_submission = Some(submission.clone());
            typed.collateral_delta_atoms = Some(transition.action_delta_atoms.clone());
            anchor.evidence["lending_position"] = json!(typed);
            effects.push(FinancialEffect::PositionChange {
                resource: wallet.clone(),
                position: id,
                asset: state.amount.asset,
                delta: transition.action_delta_atoms.clone(),
                scaled_receipt: None,
                anchor: state.anchor,
            });
        } else {
            let accrued = transition.accrual_delta_atoms.clone().ok_or_else(|| {
                invalid("Debt settlement requires exact atom-level interest evidence.")
            })?;
            if accrued.is_negative() {
                return Err(invalid(
                    "Negative debt accrual requires an independently reviewed native rounding rule.",
                ));
            }
            let base = position_atoms(before_row)?;
            let after = position_atoms(&after_row)?;
            if shift(&shift(&base, &accrued)?, &transition.action_delta_atoms)? != after {
                return Err(invalid(
                    "Debt action and accrued interest do not explain the exact post debt.",
                ));
            }
            let old = prior.iter().find(|a| {
                a.position == id && a.resource == wallet && a.principal.asset == state.amount.asset
            });
            let accrued_before = prior_accrual(old, before_row, &base)?;
            let total_interest = accrued_before.checked_add(&accrued.magnitude())?;
            let known = old
                .and_then(|a| a.evidence["known_unpaid_interest_atoms"].as_str())
                .map(atoms)
                .transpose()?
                .unwrap_or_default()
                .checked_add(&total_interest)?;
            let paid_interest = if selected && intent.action == LendingAction::Repay {
                known.clone().min(delta.magnitude())
            } else {
                Quantity::zero()
            };
            anchor.evidence["known_unpaid_interest_atoms"] =
                json!(known.checked_sub(&paid_interest)?);
            {
                effects.push(FinancialEffect::Accrual {
                    resource: wallet.clone(),
                    position: id.clone(),
                    asset: state.amount.asset.clone(),
                    position_kind: PositionKind::VariableDebt,
                    delta: SignedQuantity::positive(&total_interest),
                });
            }
            effects.push(FinancialEffect::DebtChange {
                resource: wallet.clone(),
                debt: id,
                asset: state.amount.asset,
                delta: transition.action_delta_atoms.clone(),
                anchor: state.anchor,
            });
            if !paid_interest.is_zero() {
                effects.push(FinancialEffect::Cost {
                    location: cash.clone(),
                    amount: Amount {
                        asset: underlying.clone(),
                        atoms: paid_interest,
                    },
                    category: "klend_verified_variable_interest".into(),
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
        }
    }
    for row in &before.positions {
        if row.side == LendingSide::Supply
            && !(row.reserve == intent.reserve && target_side(intent.action) == LendingSide::Supply)
        {
            let post = verified
                .obligation_after
                .positions
                .iter()
                .find(|p| p.reserve == row.reserve && p.side == row.side)
                .ok_or_else(|| invalid("Unselected collateral disappeared from the obligation."))?;
            if post.native_units != row.native_units
                || position_asset(post)? != position_asset(row)?
                || post.token != row.token
            {
                return Err(invalid("Unselected collateral units or identity changed."));
            }
        }
        if (row.side == LendingSide::Debt
            || (row.reserve == intent.reserve && row.side == target_side(intent.action)))
            && !seen.contains(&(&row.reserve, row.side))
        {
            return Err(invalid(
                "Finalized evidence omitted a reviewed debt or selected collateral transition.",
            ));
        }
    }
    for row in &verified.obligation_after.positions {
        if !before
            .positions
            .iter()
            .any(|p| p.reserve == row.reserve && p.side == row.side)
        {
            return Err(invalid(
                "Finalized obligation contains an unapproved new reserve position.",
            ));
        }
    }
    let selected = selected_delta
        .ok_or_else(|| invalid("Finalized evidence omitted the approved principal transition."))?;
    let target = position_id(
        &intent.market,
        &intent.obligation,
        &intent.reserve,
        target_side(intent.action),
    );
    let position = AccountingLocation::Position {
        resource: wallet.clone(),
        position: target,
        position_kind: if target_side(intent.action) == LendingSide::Supply {
            PositionKind::Supply
        } else {
            PositionKind::VariableDebt
        },
    };
    let position_asset = if target_side(intent.action) == LendingSide::Supply {
        asset(&candidate.reserve.collateral_token)
    } else {
        underlying.clone()
    };
    let position_amount = Amount {
        asset: position_asset,
        atoms: selected.magnitude(),
    };
    let cash_amount = Amount {
        asset: underlying.clone(),
        atoms: delta.magnitude(),
    };
    match intent.action {
        LendingAction::Supply if selected.is_negative() || selected.is_zero() => {
            return Err(invalid("Supply did not produce positive collateral."));
        }
        LendingAction::Withdraw if !selected.is_negative() || selected.magnitude() != amount => {
            return Err(invalid(
                "Withdrawal did not burn the exact approved collateral units.",
            ));
        }
        LendingAction::Borrow
            if selected.is_negative()
                || selected.magnitude()
                    > atoms(
                        intent
                            .maximum_debt_increase_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Missing debt cap."))?,
                    )? =>
        {
            return Err(invalid("Gross new debt exceeded the approved cap."));
        }
        LendingAction::Repay
            if !selected.is_negative() || selected.magnitude() != delta.magnitude() =>
        {
            return Err(invalid(
                "Repayment debt reduction differs from proven native payment.",
            ));
        }
        _ => {}
    }
    let (from, to, debit, credit) =
        if matches!(intent.action, LendingAction::Supply | LendingAction::Repay) {
            (cash.clone(), position.clone(), cash_amount, position_amount)
        } else {
            (position.clone(), cash.clone(), position_amount, cash_amount)
        };
    effects.push(FinancialEffect::OriginMovement {
        from,
        to,
        debit,
        credit,
    });
    let mut fees = Quantity::zero();
    for fee in &verified.protocol_fees {
        if fee.token != intent.token {
            return Err(invalid("Protocol fee identity changed."));
        }
        fees = fees.checked_add(&atoms(
            fee.atoms
                .as_deref()
                .ok_or_else(|| invalid("Unknown finalized protocol fee."))?,
        )?)?;
    }
    if fees > atoms(&intent.maximum_protocol_fee_atoms)?
        || (intent.action == LendingAction::Borrow
            && selected.magnitude() != delta.magnitude().checked_add(&fees)?)
        || (intent.action != LendingAction::Borrow && !fees.is_zero())
    {
        return Err(invalid(
            "Finalized origination fee differs from gross debt minus net proceeds or approved cap.",
        ));
    }
    if !fees.is_zero() {
        effects.push(FinancialEffect::Cost {
            location: position,
            amount: Amount {
                asset: underlying,
                atoms: fees,
            },
            category: "klend_origination_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
        });
    }
    if intent.all
        && intent.action == LendingAction::Repay
        && verified.obligation_after.positions.iter().any(|p| {
            p.reserve == intent.reserve && p.side == LendingSide::Debt && p.native_units != "0"
        })
    {
        return Err(invalid("Repay all did not prove zero remaining debt."));
    }
    append_rent_effects(&mut effects, intent, candidate, verified, proof, owner)?;
    if proof.fee_lamports > 0 {
        effects.push(FinancialEffect::Cost {
            location: cash,
            amount: Amount {
                asset: native_asset(),
                atoms: Quantity::from_u64(proof.fee_lamports),
            },
            category: "network_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    bundle(
        submission,
        effects,
        json!({"source":"finalized_klend_obligation_transaction","proof":proof,"protocol_evidence":verified.evidence,"supply_input":if intent.action==LendingAction::Supply {json!({"maximum_atoms":intent.amount_atoms,"actual_debit_atoms":delta.magnitude(),"unspent_input_atoms":amount.checked_sub(&delta.magnitude())?})} else {Value::Null}}),
    )
}

fn prior_accrual(
    prior: Option<&PositionAnchor>,
    before: &LendingPosition,
    current: &Quantity,
) -> Result<Quantity> {
    let Some(prior) = prior else {
        return Ok(Quantity::zero());
    };
    let old_raw = prior.evidence["native_units"]
        .as_str()
        .ok_or_else(|| invalid("Prior debt anchor lacks native fraction."))?;
    if old_raw != before.native_units {
        let old = fraction(old_raw)?.integer();
        let old_index = fraction(
            prior.evidence["cumulative_borrow_rate_bsf"]
                .as_str()
                .ok_or_else(|| invalid("Prior debt index missing."))?,
        )?
        .integer();
        let current_index = fraction(
            before
                .cumulative_borrow_rate_bsf
                .as_deref()
                .ok_or_else(|| invalid("Current debt index missing."))?,
        )?
        .integer();
        if old_index == BigUint::from(0u8)
            || current_index < old_index
            || old * current_index / old_index != fraction(&before.native_units)?.integer()
        {
            return Err(invalid(
                "An external debt principal change cannot be attributed as interest to this action.",
            ));
        }
    }
    current.checked_sub(&prior.principal.atoms)
}

fn append_rent_effects(
    effects: &mut Vec<FinancialEffect>,
    intent: &LendingIntent,
    candidate: &LendingCandidate,
    verified: &VerifiedLendingEffects,
    proof: &FinalizedReceiptProof,
    owner: &str,
) -> Result<()> {
    let limits = rent_limits(candidate)?;
    let closing = closing_rent_baselines(candidate)?;
    let wallet = resource(owner);
    let cash = AccountingLocation::Cash {
        resource: wallet.clone(),
    };
    let mut seen = BTreeSet::new();
    let mut net = BigInt::from(0);
    let mut funded = Quantity::zero();
    for row in &verified.rent_accounts {
        let id = format!("account-funding:{}", row.account);
        if !seen.insert(row.account.clone()) || proof.lamport_delta(&row.account)? != row.atoms {
            return Err(invalid("Duplicate or unproven rent account change."));
        }
        let principal = if row.atoms.is_negative() {
            let baseline = closing
                .iter()
                .find(|a| a.position == id)
                .ok_or_else(|| invalid("Unapproved rent refund."))?;
            let index = proof
                .account_keys
                .iter()
                .position(|a| a == &row.account)
                .ok_or_else(|| invalid("Refund account missing from receipt."))?;
            if row.atoms.magnitude() != baseline.principal.atoms
                || proof.post_balances_lamports.get(index) != Some(&0)
                || verified.obligation_after.exists
            {
                return Err(invalid(
                    "Rent refund must prove exact reviewed account closure.",
                ));
            }
            Quantity::zero()
        } else {
            let limit = limits
                .iter()
                .find(|a| a.position == id)
                .ok_or_else(|| invalid("Unapproved account funding."))?;
            if row.atoms.magnitude() > limit.maximum_amount {
                return Err(invalid("Account funding exceeds reviewed cap."));
            }
            funded = funded.checked_add(&row.atoms.magnitude())?;
            row.atoms.magnitude()
        };
        net += row.atoms.integer();
        if row.atoms.is_zero() {
            continue;
        }
        let amount = Amount {
            asset: native_asset(),
            atoms: row.atoms.magnitude(),
        };
        let location = AccountingLocation::Position {
            resource: wallet.clone(),
            position: id.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
        };
        let anchor = PositionAnchor {
            resource: wallet.clone(),
            position: id.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
            principal: Amount {
                asset: native_asset(),
                atoms: principal,
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"version":1,"source":"finalized_klend_account_funding","account":row.account,"signature":proof.signature,"slot":proof.slot}),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position: id,
            asset: native_asset(),
            delta: row.atoms.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
        let (from, to) = if row.atoms.is_negative() {
            (location, cash.clone())
        } else {
            (cash.clone(), location)
        };
        effects.push(FinancialEffect::OriginMovement {
            from,
            to,
            debit: amount.clone(),
            credit: amount.clone(),
        });
        effects.push(if row.atoms.is_negative() {
            FinancialEffect::UsableCredit {
                to: wallet.clone(),
                amount,
            }
        } else {
            FinancialEffect::CashDebit {
                resource: wallet.clone(),
                amount,
            }
        });
    }
    if funded > Quantity::from_u64(intent.maximum_account_funding_lamports)
        || proof.lamport_delta(owner)?.integer() != -BigInt::from(proof.fee_lamports) - net
    {
        return Err(invalid(
            "Finalized native wallet balance is not explained by reviewed rent and network fee.",
        ));
    }
    Ok(())
}
