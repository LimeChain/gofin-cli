//! Finite native-vault permissions. Swap composition belongs to named plans.
use crate::{
    adapter::{
        asset, atoms, bound_intent, candidate_from, farm_custody_summary, invalid, native_asset,
        resource, staking_farm,
    },
    protocol::*,
    types::PublicAmount,
};
pub use gofin_domain::boundaries::BoundarySpec;
use gofin_domain::{
    AccountingLocation, Amount, CostBound, CostTreatment, PreparedOperation, Quantity,
    ResourceAmount, Result,
    boundaries::{BoundaryViolation, violation_error},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Bounds {
    #[serde(default)]
    maximum_amounts: Vec<PublicAmount>,
    #[serde(default)]
    maximum_shares_atoms: Option<String>,
    #[serde(default)]
    maximum_stake_shares_atoms: Option<String>,
    #[serde(default)]
    minimum_shares_atoms: Option<String>,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
}
/// A share cap is finite source authority, never an underlying receipt cap.
/// Recognize the already-approved shape so pre-upgrade plans remain usable.
pub(crate) fn finite_withdrawal_source(
    operation: &str,
    spec: &BoundarySpec,
    target: &ResourceAmount,
) -> bool {
    let Ok(bounds) = serde_json::from_value::<Bounds>(spec.parameters.clone()) else {
        return false;
    };
    let owner = spec.fixed["owner"].as_str().unwrap_or_default();
    let Ok(tokens) = serde_json::from_value::<Vec<TokenInfo>>(spec.fixed["assets"].clone()) else {
        return false;
    };
    let family = spec.fixed["family"].as_str();
    operation == crate::types::OP_WITHDRAW
        && spec.fixed["operation"] == operation
        && spec.fixed["action"] == "withdraw"
        && matches!(family, Some("lending" | "liquidity"))
        && tokens.len() == if family == Some("lending") { 1 } else { 2 }
        && !owner.is_empty()
        && target.resource == resource(owner)
        && tokens
            .iter()
            .any(|token| asset(token) == target.amount.asset)
        && !target.amount.atoms.is_zero()
        && spec.fixed["vault"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && serde_json::from_value::<TokenInfo>(spec.fixed["shares"].clone()).is_ok()
        && bounds.maximum_amounts.is_empty()
        && bounds.minimum_shares_atoms.is_none()
        && bounds.maximum_stake_shares_atoms.is_none()
        && spec.fixed["stake"] == false
        && bounds.maximum_network_fee_lamports > 0
        && bounds.maximum_shares_atoms.as_deref().is_some_and(|cap| {
            atoms(cap).is_ok_and(|quantity| !quantity.is_zero()) && cap.parse::<u64>().is_ok()
        })
}
pub(crate) fn prepare(prepared: &PreparedOperation, raw: &Value) -> Result<BoundarySpec> {
    let intent = bound_intent(prepared)?;
    let candidate = candidate_from(prepared)?;
    let farm = intent.stake.then(|| staking_farm(&candidate)).transpose()?;
    prepare_intent(
        &prepared.operation,
        &intent,
        &candidate.vault,
        prepared.binding["wallet_address"]
            .as_str()
            .ok_or_else(|| invalid("Prepared wallet identity missing."))?,
        farm.as_ref(),
        raw,
    )
}
pub(crate) fn prepare_intent(
    operation: &str,
    intent: &VaultIntent,
    vault: &VaultInfo,
    owner: &str,
    farm: Option<&crate::farms::FarmInfo>,
    raw: &Value,
) -> Result<BoundarySpec> {
    if intent.stake != farm.is_some()
        || farm.is_some_and(|farm| {
            vault.farm.as_deref() != Some(farm.address.as_str())
                || farm.token != vault.shares
                || farm.time_unit > 1
        })
    {
        return Err(invalid(
            "The permission requires exact canonical staking farm terms.",
        ));
    }
    let bounds:Bounds=serde_json::from_value(raw.clone()).map_err(|_|invalid("Invalid Kamino bounds; use maximum_amounts or maximum_shares_atoms and finite cost caps."))?;
    if bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_network_fee_lamports > intent.maximum_network_fee_lamports
        || bounds.maximum_account_funding_lamports > intent.maximum_account_funding_lamports
    {
        return Err(invalid(
            "Boundary cost caps cannot exceed the reviewed limits.",
        ));
    }
    match (intent.stake, bounds.maximum_stake_shares_atoms.as_deref()) {
        (true, Some(cap)) if !atoms(cap)?.is_zero() => {}
        (false, None) => {}
        _ => {
            return Err(invalid(
                "Staking permissions require a positive finite maximum_stake_shares_atoms; non-staking permissions must omit it.",
            ));
        }
    }
    let mut maximum_debits = Vec::new();
    let resource = resource(owner);
    match intent.action {
        VaultAction::Deposit => {
            if bounds.maximum_shares_atoms.is_some()
                || bounds.maximum_amounts.len() != intent.amounts.len()
            {
                return Err(invalid(
                    "A deposit permission requires a finite cap for each native input asset.",
                ));
            }
            let mut seen = BTreeSet::new();
            for cap in &bounds.maximum_amounts {
                if !seen.insert(&cap.mint) {
                    return Err(invalid("Duplicate bounded input asset."));
                }
                let reviewed = intent
                    .amounts
                    .iter()
                    .find(|row| row.token.mint == cap.mint)
                    .ok_or_else(|| {
                        invalid("Bounded deposit mint is outside the reviewed request.")
                    })?;
                let n = atoms(&cap.atoms)?;
                if n > atoms(
                    reviewed
                        .atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Reviewed input amount is missing."))?,
                )? {
                    return Err(invalid(
                        "Deposit bounds cannot exceed reviewed input amounts.",
                    ));
                }
                maximum_debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset(&reviewed.token),
                        atoms: n,
                    },
                });
            }
            if let Some(floor) = &bounds.minimum_shares_atoms {
                let floor = atoms(floor)?;
                let reviewed=intent.minimum_shares_atoms.as_deref().map(atoms).transpose()?.ok_or_else(||invalid("An approved share minimum must be present in the reviewed native request."))?;
                if floor < reviewed {
                    return Err(invalid(
                        "An approved share minimum cannot weaken the reviewed minimum.",
                    ));
                }
            } else if intent.minimum_shares_atoms.is_some() {
                return Err(invalid(
                    "The permission must retain its reviewed share minimum.",
                ));
            }
        }
        VaultAction::Withdraw => {
            if !bounds.maximum_amounts.is_empty() || bounds.minimum_shares_atoms.is_some() {
                return Err(invalid(
                    "Withdrawal permission bounds only native shares and costs.",
                ));
            }
            let cap =
                atoms(bounds.maximum_shares_atoms.as_deref().ok_or_else(|| {
                    invalid("Withdrawal permissions require maximum_shares_atoms.")
                })?)?;
            if cap.is_zero()
                || cap
                    > atoms(
                        intent
                            .shares_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Reviewed concrete shares missing."))?,
                    )?
            {
                return Err(invalid(
                    "The share cap must be positive and no greater than the reviewed request.",
                ));
            }
        }
    }
    if bounds.maximum_account_funding_lamports > 0 {
        maximum_debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native_asset(),
                atoms: Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        });
    }
    let quantity_summary = match intent.action {
        VaultAction::Deposit => bounds
            .maximum_amounts
            .iter()
            .map(|amount| format!("up to {} atoms of {}", amount.atoms, amount.mint))
            .collect::<Vec<_>>()
            .join(", "),
        VaultAction::Withdraw => format!(
            "up to {} share atoms of {}",
            bounds.maximum_shares_atoms.as_deref().unwrap_or("unknown"),
            vault.shares.mint
        ),
    };
    let custody_summary = farm
        .map(|farm| {
            format!(
                " Stake up to {} share atoms in farm {}.{}",
                bounds
                    .maximum_stake_shares_atoms
                    .as_deref()
                    .unwrap_or("unknown"),
                farm.address,
                farm_custody_summary(farm)
            )
        })
        .unwrap_or_default();
    let minimum_summary = bounds
        .minimum_shares_atoms
        .as_ref()
        .map(|atoms| {
            format!(" Native mint minimum {atoms} share atoms before farm entitlement rounding.")
        })
        .unwrap_or_default();
    Ok(BoundarySpec {
        fixed: fixed(operation, intent, vault, owner, farm),
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| invalid("Could not encode Kamino bounds."))?,
        maximum_debits,
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash { resource },
            amount: Amount {
                asset: native_asset(),
                atoms: Quantity::from_u64(bounds.maximum_network_fee_lamports),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Kamino {:?} {} vault {}, wallet {}: {quantity_summary}.{minimum_summary} Network fee up to {} lamports; refundable account funding up to {} lamports.{custody_summary}",
            intent.action,
            intent.family.as_str(),
            intent.vault,
            intent.wallet,
            bounds.maximum_network_fee_lamports,
            bounds.maximum_account_funding_lamports
        ),
    })
}
fn fixed(
    operation: &str,
    intent: &VaultIntent,
    vault: &VaultInfo,
    owner: &str,
    farm: Option<&crate::farms::FarmInfo>,
) -> Value {
    let fee_terms = match vault.family {
        VaultFamily::Lending => vault.metrics["fees"].clone(),
        VaultFamily::Liquidity => {
            let strategy = &vault.protocol_state["strategy"];
            let global = &vault.protocol_state["global_configuration"];
            json!({"withdraw_fee_bps":vault.protocol_state["withdraw_fee_bps"],"performance_fee_bps":vault.protocol_state["performance_fee_bps"],"withdrawFee":strategy["withdrawFee"],"feesFee":strategy["feesFee"],"reward0Fee":strategy["reward0Fee"],"reward1Fee":strategy["reward1Fee"],"reward2Fee":strategy["reward2Fee"],"minWithdrawalFeeBps":global["minWithdrawalFeeBps"],"minPerformanceFeeBps":global["minPerformanceFeeBps"]})
        }
    };
    let farm_terms = farm.map(|f| json!({"address":f.address,"withdraw_authority":f.withdraw_authority,"token":f.token,"vault":f.vault,"authority":f.authority,"scope_prices":f.scope_prices,"frozen":f.frozen,"delegated":f.delegated,"time_unit":f.time_unit,"warmup":f.warmup,"cooldown":f.cooldown,"locking_mode":f.locking_mode,"locking_start":f.locking_start,"locking_duration":f.locking_duration,"early_penalty_bps":f.early_penalty_bps}));
    json!({"operation":operation,"wallet":intent.wallet,"owner":owner,"family":intent.family,"action":intent.action,"vault":vault.address,"program":vault.program,"assets":vault.assets,"shares":vault.shares,"farm":vault.farm,"farm_terms":farm_terms,"fee_terms":fee_terms,"backend":vault.backend,"stake":intent.stake,"network":"solana","cluster":"mainnet-beta"})
}
pub(crate) fn validate_candidate(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<()> {
    let intent = bound_intent(prepared)?;
    let candidate = candidate_from(prepared)?;
    crate::adapter::validate_candidate(&intent, &candidate)?;
    let farm = intent.stake.then(|| staking_farm(&candidate)).transpose()?;
    let approved: Bounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| invalid("Invalid approved Kamino boundaries."))?;
    let actual = fixed(
        &prepared.operation,
        &intent,
        &candidate.vault,
        prepared.binding["wallet_address"]
            .as_str()
            .ok_or_else(|| invalid("Missing wallet identity."))?,
        farm.as_ref(),
    );
    if actual != spec.fixed {
        return Err(violation(
            "fixed_vault_wallet_and_assets",
            &spec.fixed,
            &actual,
        ));
    }
    let actual_stake = candidate.metadata["stake_atoms"]
        .as_str()
        .map(atoms)
        .transpose()?;
    let quote_stakes = ["stake_atoms", "farm_stake_atoms"]
        .into_iter()
        .filter_map(|field| candidate.quote.details.get(field))
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid("Invalid quoted farm custody quantity."))
                .and_then(atoms)
        })
        .collect::<Result<Vec<_>>>()?;
    let stake_disagreement = quote_stakes
        .iter()
        .any(|quoted| actual_stake.as_ref() != Some(quoted));
    let stake_outside = match (
        intent.stake,
        approved.maximum_stake_shares_atoms.as_deref(),
        actual_stake,
    ) {
        (true, Some(cap), Some(actual)) => actual.is_zero() || actual > atoms(cap)?,
        (false, None, None) => false,
        (false, None, Some(actual)) => !actual.is_zero(),
        _ => true,
    };
    let mut outside = stake_outside
        || stake_disagreement
        || intent.maximum_network_fee_lamports > approved.maximum_network_fee_lamports
        || intent.maximum_account_funding_lamports > approved.maximum_account_funding_lamports
        || candidate.quote.network_fee_lamports > approved.maximum_network_fee_lamports
        || candidate.quote.rent_lamports > approved.maximum_account_funding_lamports;
    for row in intent.amounts.iter().chain(
        candidate
            .quote
            .debits
            .iter()
            .filter(|_| intent.action == VaultAction::Deposit),
    ) {
        let cap = approved
            .maximum_amounts
            .iter()
            .find(|r| r.mint == row.token.mint)
            .ok_or_else(|| {
                violation("unapproved_input_asset", &spec.parameters, &prepared.input)
            })?;
        outside |= atoms(
            row.atoms
                .as_deref()
                .ok_or_else(|| invalid("Unknown candidate debit."))?,
        )? > atoms(&cap.atoms)?;
    }
    if intent.action == VaultAction::Withdraw {
        outside |= atoms(
            intent
                .shares_atoms
                .as_deref()
                .ok_or_else(|| invalid("Missing withdrawal shares."))?,
        )? > atoms(
            approved
                .maximum_shares_atoms
                .as_deref()
                .ok_or_else(|| invalid("Missing approved share cap."))?,
        )?;
    }
    if let Some(floor) = approved.minimum_shares_atoms.as_deref() {
        outside |= !candidate.quote.minimum_shares_enforced
            || intent
                .minimum_shares_atoms
                .as_deref()
                .map(atoms)
                .transpose()?
                .is_none_or(|n| n < atoms(floor).unwrap_or_else(|_| Quantity::zero()))
            || atoms(&candidate.quote.shares_atoms)? < atoms(floor)?;
    }
    if outside {
        return Err(violation(
            "native_quantity_and_cost_caps",
            &spec.parameters,
            &prepared.input,
        ));
    }
    Ok(())
}
fn violation(name: &str, approved: &Value, candidate: &Value) -> gofin_domain::GoFinError {
    violation_error(
        "The Kamino candidate falls outside approved native vault authority.",
        BoundaryViolation {
            constraint: name.into(),
            approved_bound: approved.clone(),
            candidate_value: candidate.clone(),
            evidence_time: None,
            reauthorization_required: true,
            reconciliation_required: false,
        },
    )
}
