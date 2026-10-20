//! Reusable native loan authority pins identity and risk independently of funds.
use super::{
    adapter::{bound_intent, candidate_from, invalid},
    settlement::position_id,
    types::*,
};
use crate::adapter::{asset, atoms, native_asset, resource};
use gofin_domain::{
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanCumulativeScope, FinancialPlanFundingIdentity, PlanPermission},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    #[serde(default = "zero")]
    pub minimum_amount_atoms: String,
    pub maximum_amount_atoms: String,
    pub maximum_debt_increase_atoms: Option<String>,
    pub maximum_cumulative_borrow_atoms: Option<String>,
    pub maximum_protocol_fee_atoms: String,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
}
fn zero() -> String {
    "0".into()
}
fn fixed(
    intent: &LendingIntent,
    owner: &str,
    reserve: &ReserveInfo,
    obligation: &ObligationInfo,
) -> Value {
    let mut input = json!(intent);
    for key in [
        "amount_atoms",
        "maximum_debt_increase_atoms",
        "maximum_protocol_fee_atoms",
        "maximum_network_fee_lamports",
        "maximum_account_funding_lamports",
    ] {
        input.as_object_mut().expect("intent object").remove(key);
    }
    json!({"intent":input,"owner":owner,"reserve":reserve.address,"market":reserve.market,"program":reserve.program,"token":reserve.token,"collateral_token":reserve.collateral_token,"authority_terms":reserve.protocol_state["authority_terms"],"obligation":obligation.address,"elevation_group":obligation.elevation_group,"tag":obligation.tag,"referrer":obligation.referrer})
}
/// Withdrawals consume collateral units, independently of underlying units.
/// Inspect the immutable permission without changing persisted authorization.
pub(crate) fn finite_withdrawal_source(
    operation: &str,
    spec: &BoundarySpec,
    target: &ResourceAmount,
) -> bool {
    let Ok(bounds) = serde_json::from_value::<Bounds>(spec.parameters.clone()) else {
        return false;
    };
    let owner = spec.fixed["owner"].as_str().unwrap_or_default();
    let Ok(token) =
        serde_json::from_value::<crate::protocol::TokenInfo>(spec.fixed["token"].clone())
    else {
        return false;
    };
    operation == OP_WITHDRAW
        && spec.fixed["intent"]["action"] == "withdraw"
        && !owner.is_empty()
        && target.resource == resource(owner)
        && target.amount.asset == asset(&token)
        && !target.amount.atoms.is_zero()
        && ["market", "reserve", "obligation"].iter().all(|key| {
            spec.fixed[*key]
                .as_str()
                .is_some_and(|value| !value.is_empty())
                && spec.fixed[*key] == spec.fixed["intent"][*key]
        })
        && serde_json::from_value::<crate::protocol::TokenInfo>(
            spec.fixed["collateral_token"].clone(),
        )
        .is_ok()
        && bounds.maximum_debt_increase_atoms.is_none()
        && bounds.maximum_cumulative_borrow_atoms.is_none()
        && bounds.maximum_network_fee_lamports > 0
        && atoms(&bounds.minimum_amount_atoms).is_ok_and(|minimum| {
            atoms(&bounds.maximum_amount_atoms)
                .is_ok_and(|maximum| !maximum.is_zero() && minimum <= maximum)
        })
        && bounds.maximum_amount_atoms.parse::<u64>().is_ok()
}
pub(crate) fn prepare(prepared: &PreparedOperation, raw: &Value) -> Result<BoundarySpec> {
    let intent = bound_intent(prepared)?;
    let candidate = candidate_from(prepared)?;
    if intent.action == LendingAction::Borrow {
        return Err(invalid(
            "Reusable borrowing requires a named plan with gross cumulative debt scope; ordinary boundaries cannot replenish or track debt authority.",
        ));
    }
    Ok(permission(
        &intent,
        prepared.binding["wallet_address"]
            .as_str()
            .ok_or_else(|| invalid("Missing approved wallet."))?,
        &candidate.reserve,
        &candidate.obligation_before,
        raw,
    )?
    .spec)
}
pub(crate) fn permission(
    intent: &LendingIntent,
    owner: &str,
    reserve: &ReserveInfo,
    obligation: &ObligationInfo,
    raw: &Value,
) -> Result<PlanPermission> {
    let bounds: Bounds = serde_json::from_value(raw.clone())
        .map_err(|_| invalid("Invalid native lending bounds; discover the operation schema."))?;
    let minimum = atoms(&bounds.minimum_amount_atoms)?;
    let maximum = atoms(&bounds.maximum_amount_atoms)?;
    let selected = atoms(&intent.amount_atoms)?;
    if minimum > maximum
        || maximum.is_zero()
        || selected < minimum
        || selected > maximum
        || bounds.maximum_network_fee_lamports == 0
        || intent.maximum_network_fee_lamports > bounds.maximum_network_fee_lamports
        || intent.maximum_account_funding_lamports > bounds.maximum_account_funding_lamports
        || atoms(&intent.maximum_protocol_fee_atoms)? > atoms(&bounds.maximum_protocol_fee_atoms)?
        || !reserve.protocol_state["authority_terms"].is_object()
    {
        return Err(invalid(
            "Lending amount or costs exceed finite permission bounds, or canonical authority terms are missing.",
        ));
    }
    let wallet = resource(owner);
    let underlying = asset(&reserve.token);
    let network = CostBound {
        location: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        amount: Amount {
            asset: native_asset(),
            atoms: Quantity::from_u64(bounds.maximum_network_fee_lamports),
        },
        treatment: CostTreatment::AdditionalDebit,
    };
    let mut maximum_costs = vec![network.clone()];
    // Accounting scopes contain positive caps only. A zero fee remains an
    // explicit bound in parameters and is enforced again for each candidate.
    if intent.action == LendingAction::Borrow
        && !atoms(&bounds.maximum_protocol_fee_atoms)?.is_zero()
    {
        maximum_costs.push(CostBound {
            location: AccountingLocation::Position {
                resource: wallet.clone(),
                position: position_id(
                    &intent.market,
                    &intent.obligation,
                    &intent.reserve,
                    LendingSide::Debt,
                ),
                position_kind: PositionKind::VariableDebt,
            },
            amount: Amount {
                asset: underlying.clone(),
                atoms: atoms(&bounds.maximum_protocol_fee_atoms)?,
            },
            treatment: CostTreatment::IncludedInDebit,
        });
    } else if intent.action == LendingAction::Repay {
        maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            amount: Amount {
                asset: underlying.clone(),
                atoms: maximum.clone(),
            },
            treatment: CostTreatment::IncludedInDebit,
        });
    }
    let mut debits = Vec::new();
    if matches!(intent.action, LendingAction::Supply | LendingAction::Repay) {
        debits.push(ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: underlying.clone(),
                atoms: maximum.clone(),
            },
        });
    }
    if bounds.maximum_account_funding_lamports > 0 {
        debits.push(ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: native_asset(),
                atoms: Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        });
    }
    let cumulative_scope = if intent.action == LendingAction::Borrow {
        let gross = atoms(
            bounds
                .maximum_debt_increase_atoms
                .as_deref()
                .ok_or_else(|| {
                    invalid("Borrow permission requires maximum_debt_increase_atoms.")
                })?,
        )?;
        let cumulative=atoms(bounds.maximum_cumulative_borrow_atoms.as_deref().ok_or_else(||invalid("Borrow permission requires maximum_cumulative_borrow_atoms; repayments do not replenish it."))?)?;
        let reviewed = atoms(
            intent
                .maximum_debt_increase_atoms
                .as_deref()
                .ok_or_else(|| invalid("Missing gross debt cap."))?,
        )?;
        if gross < reviewed || gross < maximum || cumulative < gross || intent.risk_limits.is_none()
        {
            return Err(invalid(
                "Borrow permission requires finite gross debt, cumulative debt and fixed risk limits.",
            ));
        }
        Some(FinancialPlanCumulativeScope {
            scope_id: format!(
                "debt:solana:{}:{}:{}:{}",
                reserve.program, owner, intent.obligation, intent.reserve
            ),
            maximum_debits: vec![],
            maximum_costs: maximum_costs.clone(),
            maximum_obligations: vec![ResourceAmount {
                resource: wallet.clone(),
                amount: Amount {
                    asset: underlying.clone(),
                    atoms: cumulative,
                },
            }],
        })
    } else {
        if bounds.maximum_debt_increase_atoms.is_some()
            || bounds.maximum_cumulative_borrow_atoms.is_some()
        {
            return Err(invalid("Only borrowing accepts new-debt authority."));
        }
        None
    };
    let possible_credits = if matches!(
        intent.action,
        LendingAction::Withdraw | LendingAction::Borrow
    ) {
        vec![FinancialPlanFundingIdentity {
            resource: wallet.clone(),
            asset: underlying.clone(),
        }]
    } else {
        vec![]
    };
    let spec = BoundarySpec {
        fixed: fixed(intent, owner, reserve, obligation),
        parameters: json!(bounds),
        maximum_debits: debits,
        maximum_costs,
        summary: format!(
            "KLend {:?}: {}–{} native atoms for reserve {}, obligation {}, wallet {}. Gross new debt cap {}; protocol fee cap {} atoms; network fee cap {} lamports; refundable funding cap {} lamports. Market, assets, destination, fee terms and risk limits remain fixed: {}.",
            intent.action,
            bounds.minimum_amount_atoms,
            bounds.maximum_amount_atoms,
            intent.reserve,
            intent.obligation,
            intent.wallet,
            bounds.maximum_debt_increase_atoms.as_deref().unwrap_or("0"),
            bounds.maximum_protocol_fee_atoms,
            bounds.maximum_network_fee_lamports,
            bounds.maximum_account_funding_lamports,
            json!(intent.risk_limits)
        ) + if intent.action == LendingAction::Supply {
            " Supply amount bounds constrain the native input maximum; actual debit may be smaller because of collateral rounding, leaving an unspent residual."
        } else {
            ""
        },
    };
    Ok(PlanPermission {
        spec,
        recovery_parent: None,
        possible_credits,
        fee_assets: vec![native_asset()],
        deferred_receipt: None,
        authorization_seconds: None,
        cumulative_scope,
    })
}
pub(crate) fn validate(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<()> {
    let intent = bound_intent(prepared)?;
    let candidate = candidate_from(prepared)?;
    let owner = prepared.binding["wallet_address"]
        .as_str()
        .ok_or_else(|| invalid("Missing wallet binding."))?;
    if spec.fixed
        != fixed(
            &intent,
            owner,
            &candidate.reserve,
            &candidate.obligation_before,
        )
    {
        return Err(invalid(
            "Lending candidate changed fixed identity, economic terms or risk limits.",
        ));
    }
    let fresh = permission(
        &intent,
        owner,
        &candidate.reserve,
        &candidate.obligation_before,
        &spec.parameters,
    )?;
    if fresh.spec != *spec {
        return Err(invalid(
            "Lending financial bounds do not match canonical authority.",
        ));
    }
    Ok(())
}
