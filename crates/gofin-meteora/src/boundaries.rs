//! Finite Meteora permissions. The pool, family, wallet and operation stay
//! fixed; only exact quantities may vary inside explicit caller limits.

use std::{collections::BTreeMap, sync::Arc};

use gofin_domain::{
    AccountingLocation, Amount, AssetId, CostBound, CostTreatment, GoFinError, PreparedOperation,
    ResourceAmount, ResourceId, Result,
    boundaries::{BoundarySpec, BoundaryViolation, violation_error},
};
use gofin_solana::{NETWORK, SolanaWalletAdapter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::{MeteoraProtocol, ProtocolFamily};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bounds {
    #[serde(default)]
    maximum_token_a_atoms: Option<String>,
    #[serde(default)]
    maximum_token_b_atoms: Option<String>,
    #[serde(default)]
    maximum_input_atoms: Option<String>,
    #[serde(default)]
    minimum_output_atoms: Option<String>,
    #[serde(default)]
    minimum_token_a_atoms: Option<String>,
    #[serde(default)]
    minimum_token_b_atoms: Option<String>,
    maximum_slippage_bps: u16,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
}

pub(crate) async fn prepare(
    wallets: &BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocols: &BTreeMap<ProtocolFamily, Arc<dyn MeteoraProtocol>>,
    prepared: &PreparedOperation,
    raw_bounds: &Value,
) -> Result<BoundarySpec> {
    prepare_with_mode(wallets, protocols, prepared, raw_bounds, false).await
}

pub(crate) async fn prepare_plan(
    wallets: &BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocols: &BTreeMap<ProtocolFamily, Arc<dyn MeteoraProtocol>>,
    prepared: &PreparedOperation,
    raw_bounds: &Value,
) -> Result<BoundarySpec> {
    prepare_with_mode(wallets, protocols, prepared, raw_bounds, true).await
}

async fn prepare_with_mode(
    wallets: &BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocols: &BTreeMap<ProtocolFamily, Arc<dyn MeteoraProtocol>>,
    prepared: &PreparedOperation,
    raw_bounds: &Value,
    plan_permission: bool,
) -> Result<BoundarySpec> {
    let bounds: Bounds = serde_json::from_value(raw_bounds.clone())
        .map_err(|_| GoFinError::validation("Invalid Meteora boundary limits."))?;
    let intent = crate::adapter::parse_public_intent(&prepared.operation, &prepared.input)?;
    if bounds.maximum_slippage_bps == 0
        || bounds.maximum_slippage_bps > intent.maximum_slippage_bps
        || bounds.maximum_network_fee_lamports == 0
        || bounds.maximum_network_fee_lamports > intent.maximum_network_fee_lamports
        || bounds.maximum_account_funding_lamports > intent.maximum_account_funding_lamports
    {
        return Err(GoFinError::validation(
            "Meteora boundary limits cannot exceed the reviewed request limits.",
        ));
    }
    for (bound, request) in [
        (
            bounds.maximum_token_a_atoms.as_deref(),
            intent.token_a_atoms.as_deref(),
        ),
        (
            bounds.maximum_token_b_atoms.as_deref(),
            intent.token_b_atoms.as_deref(),
        ),
        (
            bounds.maximum_input_atoms.as_deref(),
            intent.amount_atoms.as_deref(),
        ),
    ] {
        match (bound, request) {
            (Some(bound), Some(request))
                if gofin_domain::Quantity::new(bound)? > gofin_domain::Quantity::new(request)? =>
            {
                return Err(GoFinError::validation(
                    "Meteora approved quantity bounds cannot exceed the reviewed exact request.",
                ));
            }
            (Some(_), None) => {
                return Err(GoFinError::validation(
                    "Meteora quantity bounds must match a reviewed request amount.",
                ));
            }
            _ => {}
        }
    }
    if let (Some(floor), Some(request_floor)) = (
        bounds.minimum_output_atoms.as_deref(),
        intent.minimum_output_atoms.as_deref(),
    ) {
        if gofin_domain::Quantity::new(floor)? < gofin_domain::Quantity::new(request_floor)? {
            return Err(GoFinError::validation(
                "Meteora approved output floor cannot be below the reviewed minimum.",
            ));
        }
    } else if bounds.minimum_output_atoms.is_some() && intent.minimum_output_atoms.is_none() {
        return Err(GoFinError::validation(
            "A Meteora output floor must match a reviewed minimum output.",
        ));
    }
    for (approved, requested) in [
        (
            bounds.minimum_token_a_atoms.as_deref(),
            intent.minimum_token_a_atoms.as_deref(),
        ),
        (
            bounds.minimum_token_b_atoms.as_deref(),
            intent.minimum_token_b_atoms.as_deref(),
        ),
    ] {
        validate_minimum_floor(approved, requested)?;
    }
    let wallet = wallets
        .get(&intent.wallet)
        .ok_or_else(|| GoFinError::validation("The Meteora wallet is no longer enrolled."))?;
    let protocol = protocols
        .get(&intent.family)
        .ok_or_else(|| GoFinError::validation("The Meteora protocol family is unavailable."))?;
    let pool = protocol.pool(&wallet.runtime(), &intent.pool).await?;
    if pool.family != intent.family
        || pool.address != intent.pool
        || !pool.active
        || (intent.action != crate::protocol::MeteoraAction::Swap
            && (intent.token_a_mint.as_deref() != Some(pool.token_a.mint.as_str())
                || intent.token_b_mint.as_deref() != Some(pool.token_b.mint.as_str())))
    {
        return Err(GoFinError::validation(
            "Meteora boundary pool identity is not canonical.",
        ));
    }
    if intent.action == crate::protocol::MeteoraAction::Open
        && prepared.binding["protocol_candidate"]["stable_position_address"]
            .as_str()
            .is_none()
    {
        return Err(GoFinError::validation(
            "Opening a Meteora position requires a stable prepared position identity.",
        ));
    }
    let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
    let mut maximum_debits = Vec::new();
    let (a, b) = (
        AssetId {
            network: NETWORK.into(),
            reference: pool.token_a.mint.clone(),
            decimals: pool.token_a.decimals,
        },
        AssetId {
            network: NETWORK.into(),
            reference: pool.token_b.mint.clone(),
            decimals: pool.token_b.decimals,
        },
    );
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    for (asset, atoms) in [
        (&a, bounds.maximum_token_a_atoms.as_deref()),
        (&b, bounds.maximum_token_b_atoms.as_deref()),
    ] {
        if let Some(atoms) = atoms {
            maximum_debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: gofin_domain::Quantity::new(atoms)?,
                },
            });
        }
    }
    if let Some(atoms) = bounds.maximum_input_atoms.as_deref() {
        let mint = intent
            .input_mint
            .as_deref()
            .ok_or_else(|| GoFinError::validation("A bounded swap requires an input mint."))?;
        let asset = if mint == "native"
            && [a.reference.as_str(), b.reference.as_str()]
                .contains(&"So11111111111111111111111111111111111111112")
        {
            &native
        } else if mint == a.reference {
            &a
        } else if mint == b.reference {
            &b
        } else {
            return Err(GoFinError::validation(
                "The bounded swap input mint is outside the selected pool.",
            ));
        };
        maximum_debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: gofin_domain::Quantity::new(atoms)?,
            },
        });
    }
    let maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: gofin_domain::Quantity::from_u64(bounds.maximum_network_fee_lamports),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    if bounds.maximum_account_funding_lamports > 0 {
        let account_funding =
            gofin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports);
        if let Some(native_debit) = maximum_debits
            .iter_mut()
            .find(|debit| debit.resource == resource && debit.amount.asset == native)
        {
            native_debit.amount.atoms = native_debit.amount.atoms.checked_add(&account_funding)?;
        } else {
            maximum_debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: account_funding,
                },
            });
        }
    }
    let fixed = json!({
        "operation":prepared.operation,
        "wallet":intent.wallet,
        "wallet_address":wallet.address(),
        "family":intent.family,
        "pool_address":pool.address,
        "token_a":{"mint":pool.token_a.mint,"decimals":pool.token_a.decimals,"token_program":pool.token_a.token_program},
        "token_b":{"mint":pool.token_b.mint,"decimals":pool.token_b.decimals,"token_program":pool.token_b.token_program},
        "action":intent.action,
        "position":intent.position,
        "stable_position_address":prepared.binding["protocol_candidate"]["stable_position_address"],
        "input_mint":intent.input_mint,
        "output_mint":intent.output_mint,
        "token_a_mint":intent.token_a_mint,
        "token_b_mint":intent.token_b_mint,
        "bin_lower":intent.bin_lower,
        "bin_upper":intent.bin_upper,
        "liquidity_distribution":intent.liquidity_distribution,
        "remove_share_bps":intent.remove_share_bps,
        "minimum_token_a_atoms":intent.minimum_token_a_atoms,
        "minimum_token_b_atoms":intent.minimum_token_b_atoms,
        "minimum_output_atoms":intent.minimum_output_atoms,
        "approved_receipt_delta_atoms":prepared.binding["protocol_candidate"]["quote"]["receipt_delta_atoms"],
        "plan_permission":plan_permission,
        "network":"solana",
        "cluster":"mainnet-beta"
    });
    Ok(BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds)
            .map_err(|_| GoFinError::validation("Meteora boundary limits are invalid."))?,
        maximum_debits,
        maximum_costs,
        summary: format!(
            "Meteora {:?} {:?} on fixed {} pool {}; max slippage {} bps, network fee {} lamports, account funding {} lamports.",
            intent.family,
            intent.action,
            intent.family.as_str(),
            intent.pool,
            bounds.maximum_slippage_bps,
            bounds.maximum_network_fee_lamports,
            bounds.maximum_account_funding_lamports
        ),
    })
}

pub(crate) fn validate_candidate(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<()> {
    let intent = crate::adapter::parse_public_intent(&prepared.operation, &prepared.input)?;
    let approved: Bounds = serde_json::from_value(spec.parameters.clone())
        .map_err(|_| GoFinError::validation("The approved Meteora bounds are invalid."))?;
    let candidate: crate::protocol::ProtocolCandidate =
        serde_json::from_value(prepared.binding["protocol_candidate"].clone()).map_err(|_| {
            GoFinError::validation("The prepared Meteora protocol quote is invalid.")
        })?;
    let candidate_fixed = json!({
        "operation":prepared.operation,
        "wallet":intent.wallet,
        "wallet_address":prepared.binding["wallet_address"],
        "family":intent.family,
        "pool_address":prepared.binding["pool"]["address"],
        "token_a":{"mint":prepared.binding["pool"]["token_a"]["mint"],"decimals":prepared.binding["pool"]["token_a"]["decimals"],"token_program":prepared.binding["pool"]["token_a"]["token_program"]},
        "token_b":{"mint":prepared.binding["pool"]["token_b"]["mint"],"decimals":prepared.binding["pool"]["token_b"]["decimals"],"token_program":prepared.binding["pool"]["token_b"]["token_program"]},
        "action":intent.action,
        "position":intent.position,
        "stable_position_address":candidate.stable_position_address,
        "input_mint":intent.input_mint,
        "output_mint":intent.output_mint,
        "token_a_mint":intent.token_a_mint,
        "token_b_mint":intent.token_b_mint,
        "bin_lower":intent.bin_lower,
        "bin_upper":intent.bin_upper,
        "liquidity_distribution":intent.liquidity_distribution,
        "remove_share_bps":intent.remove_share_bps,
        "minimum_token_a_atoms":intent.minimum_token_a_atoms,
        "minimum_token_b_atoms":intent.minimum_token_b_atoms,
        "minimum_output_atoms":intent.minimum_output_atoms,
        "approved_receipt_delta_atoms":spec.fixed["approved_receipt_delta_atoms"],
        "plan_permission":spec.fixed["plan_permission"],
        "network":"solana",
        "cluster":"mainnet-beta"
    });
    if candidate_fixed != spec.fixed {
        return Err(violation(
            "fixed_pool_wallet_action",
            &spec.fixed,
            &candidate_fixed,
        ));
    }
    let exceeded = |candidate: Option<&str>, maximum: Option<&str>| -> Result<bool> {
        match (candidate, maximum) {
            (None, _) => Ok(false),
            (Some(_), None) => Ok(true),
            (Some(candidate), Some(maximum)) => {
                Ok(gofin_domain::Quantity::new(candidate)? > gofin_domain::Quantity::new(maximum)?)
            }
        }
    };
    let pool_a = prepared.binding["pool"]["token_a"]["mint"]
        .as_str()
        .ok_or_else(|| GoFinError::validation("The prepared Meteora token identity is invalid."))?;
    let (quoted_debit, quoted_credit, debit_cap, credit_floor) =
        if intent.action == crate::protocol::MeteoraAction::Swap {
            let input = intent
                .input_mint
                .as_deref()
                .ok_or_else(|| GoFinError::validation("The exact swap input mint is missing."))?;
            let input_is_a = if input == "native" {
                pool_a == "So11111111111111111111111111111111111111112"
            } else {
                input == pool_a
            };
            let (debit, credit) = if input_is_a {
                (
                    candidate.quote.token_a_debit_atoms.as_deref(),
                    candidate.quote.token_b_credit_atoms.as_deref(),
                )
            } else {
                (
                    candidate.quote.token_b_debit_atoms.as_deref(),
                    candidate.quote.token_a_credit_atoms.as_deref(),
                )
            };
            (
                debit,
                credit,
                approved.maximum_input_atoms.as_deref(),
                approved.minimum_output_atoms.as_deref(),
            )
        } else {
            (None, None, None, None)
        };
    let native_input = intent.action == crate::protocol::MeteoraAction::Swap
        && intent.input_mint.as_deref() == Some("native");
    let native_output = intent.action == crate::protocol::MeteoraAction::Swap
        && intent.output_mint.as_deref() == Some("native");
    let native_cash_delta = candidate.quote.native_cash_delta.as_deref();
    let debit_outside = if native_input {
        native_debit_outside(native_cash_delta, debit_cap)?
    } else {
        exceeded(quoted_debit, debit_cap)?
    };
    let credit_outside = if native_output {
        native_credit_outside(native_cash_delta, credit_floor)?
    } else {
        match (quoted_credit, credit_floor) {
            (Some(actual), Some(floor)) => {
                gofin_domain::Quantity::new(actual)? < gofin_domain::Quantity::new(floor)?
            }
            (None, Some(_)) => true,
            _ => false,
        }
    };
    let quoted_swap_outside = debit_outside || credit_outside;
    let liquidity_quote_outside = if matches!(
        intent.action,
        crate::protocol::MeteoraAction::Open | crate::protocol::MeteoraAction::Add
    ) {
        exceeded(
            candidate.quote.token_a_debit_atoms.as_deref(),
            approved.maximum_token_a_atoms.as_deref(),
        )? || exceeded(
            candidate.quote.token_b_debit_atoms.as_deref(),
            approved.maximum_token_b_atoms.as_deref(),
        )?
    } else {
        false
    };
    let below_floor = |actual: Option<&str>, floor: Option<&str>| -> Result<bool> {
        match (actual, floor) {
            (_, None) => Ok(false),
            (None, Some(_)) => Ok(true),
            (Some(actual), Some(floor)) => {
                Ok(gofin_domain::Quantity::new(actual)? < gofin_domain::Quantity::new(floor)?)
            }
        }
    };
    let liquidity_credit_below_floor = matches!(
        intent.action,
        crate::protocol::MeteoraAction::Remove
            | crate::protocol::MeteoraAction::Close
            | crate::protocol::MeteoraAction::Collect
    ) && (below_floor(
        candidate.quote.token_a_credit_atoms.as_deref(),
        approved.minimum_token_a_atoms.as_deref(),
    )? || below_floor(
        candidate.quote.token_b_credit_atoms.as_deref(),
        approved.minimum_token_b_atoms.as_deref(),
    )?);
    let receipt_delta_outside = if spec.fixed["plan_permission"] == true {
        false
    } else {
        receipt_delta_outside(
            intent.family,
            intent.action,
            spec.fixed["approved_receipt_delta_atoms"].as_str(),
            candidate.quote.receipt_delta_atoms.as_deref(),
        )?
    };
    let outside = quoted_swap_outside
        || liquidity_quote_outside
        || liquidity_credit_below_floor
        || receipt_delta_outside
        || exceeded(
            intent.token_a_atoms.as_deref(),
            approved.maximum_token_a_atoms.as_deref(),
        )?
        || exceeded(
            intent.token_b_atoms.as_deref(),
            approved.maximum_token_b_atoms.as_deref(),
        )?
        || exceeded(
            intent.amount_atoms.as_deref(),
            approved.maximum_input_atoms.as_deref(),
        )?
        || intent.maximum_slippage_bps > approved.maximum_slippage_bps
        || intent.maximum_network_fee_lamports > approved.maximum_network_fee_lamports
        || intent.maximum_account_funding_lamports > approved.maximum_account_funding_lamports
        || match (
            intent.minimum_output_atoms.as_deref(),
            approved.minimum_output_atoms.as_deref(),
        ) {
            (Some(actual), Some(floor)) => {
                gofin_domain::Quantity::new(actual)? < gofin_domain::Quantity::new(floor)?
            }
            (Some(_), None) => true,
            (None, Some(_)) => true,
            _ => false,
        };
    if outside {
        return Err(violation(
            "amount_slippage_and_cost_caps",
            &spec.parameters,
            &prepared.input,
        ));
    }
    Ok(())
}

fn native_debit_outside(delta: Option<&str>, cap: Option<&str>) -> Result<bool> {
    let Some(delta) = delta else {
        return Ok(true);
    };
    let delta = gofin_domain::SignedQuantity::new(delta)?;
    if !delta.is_negative() {
        return Ok(true);
    }
    match cap {
        Some(cap) => Ok(delta.magnitude() > gofin_domain::Quantity::new(cap)?),
        None => Ok(true),
    }
}

fn native_credit_outside(delta: Option<&str>, floor: Option<&str>) -> Result<bool> {
    let Some(delta) = delta else {
        return Ok(true);
    };
    let delta = gofin_domain::SignedQuantity::new(delta)?;
    if delta.is_negative() {
        return Ok(true);
    }
    match floor {
        Some(floor) => Ok(delta.magnitude() < gofin_domain::Quantity::new(floor)?),
        None => Ok(false),
    }
}

fn validate_minimum_floor(approved: Option<&str>, requested: Option<&str>) -> Result<()> {
    match (approved, requested) {
        (Some(approved), Some(requested))
            if gofin_domain::Quantity::new(approved)? < gofin_domain::Quantity::new(requested)? =>
        {
            Err(GoFinError::validation(
                "Meteora approved token output floors cannot be below the reviewed request floors.",
            ))
        }
        (Some(_), None) => Err(GoFinError::validation(
            "A Meteora token output floor must match a reviewed request floor.",
        )),
        (None, Some(_)) => Err(GoFinError::validation(
            "Meteora reviewed token output floors must be included in the approved bounds.",
        )),
        _ => Ok(()),
    }
}

fn receipt_delta_outside(
    family: crate::protocol::ProtocolFamily,
    action: crate::protocol::MeteoraAction,
    approved: Option<&str>,
    actual: Option<&str>,
) -> Result<bool> {
    Ok(match action {
        crate::protocol::MeteoraAction::Open | crate::protocol::MeteoraAction::Add => {
            match actual {
                Some(actual) => {
                    let actual = gofin_domain::SignedQuantity::new(actual)?;
                    if actual.is_negative() || actual.is_zero() {
                        true
                    } else if family == crate::protocol::ProtocolFamily::DammV2 {
                        let Some(approved) = approved else {
                            return Ok(true);
                        };
                        gofin_domain::SignedQuantity::new(approved)? != actual
                    } else {
                        false
                    }
                }
                None => true,
            }
        }
        crate::protocol::MeteoraAction::Remove => match (approved, actual) {
            (Some(approved), Some(actual)) if family == crate::protocol::ProtocolFamily::DammV2 => {
                gofin_domain::SignedQuantity::new(approved)?
                    != gofin_domain::SignedQuantity::new(actual)?
            }
            (Some(approved), Some(actual)) => {
                let approved = gofin_domain::SignedQuantity::new(approved)?;
                let actual = gofin_domain::SignedQuantity::new(actual)?;
                !approved.is_negative()
                    || !actual.is_negative()
                    || actual.magnitude() > approved.magnitude()
            }
            _ => true,
        },
        crate::protocol::MeteoraAction::Close => actual != Some("0") || approved != Some("0"),
        crate::protocol::MeteoraAction::Swap | crate::protocol::MeteoraAction::Collect => false,
    })
}

fn violation(constraint: &str, approved: &Value, candidate: &Value) -> GoFinError {
    violation_error(
        "The Meteora candidate exceeds its approved fixed pool or quantity limits.",
        BoundaryViolation {
            constraint: constraint.into(),
            approved_bound: approved.clone(),
            candidate_value: candidate.clone(),
            evidence_time: None,
            reauthorization_required: true,
            reconciliation_required: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_liquidity_floors_cannot_be_lowered_or_omitted() {
        assert!(validate_minimum_floor(Some("89"), Some("90")).is_err());
        assert!(validate_minimum_floor(None, Some("90")).is_err());
        assert!(validate_minimum_floor(Some("90"), None).is_err());
        assert!(validate_minimum_floor(Some("90"), Some("90")).is_ok());
        assert!(validate_minimum_floor(Some("91"), Some("90")).is_ok());
    }

    #[test]
    fn native_swap_bounds_use_fee_and_rent_adjusted_native_cash_delta() {
        // Native input/output is wrapped through a temporary WSOL account. The
        // SPL-token quote field is therefore not the wallet's native cashflow.
        assert!(!native_debit_outside(Some("-10000"), Some("10000")).unwrap());
        assert!(native_debit_outside(Some("-10001"), Some("10000")).unwrap());
        assert!(native_debit_outside(Some("10000"), Some("10000")).unwrap());
        assert!(native_debit_outside(None, Some("10000")).unwrap());

        assert!(!native_credit_outside(Some("10000"), Some("9950")).unwrap());
        assert!(native_credit_outside(Some("9949"), Some("9950")).unwrap());
        assert!(native_credit_outside(Some("-10000"), Some("9950")).unwrap());
        assert!(native_credit_outside(None, Some("9950")).unwrap());
    }

    #[test]
    fn receipt_unit_requotes_cannot_increase_remove_or_reduce_add() {
        use crate::protocol::MeteoraAction;
        use crate::protocol::ProtocolFamily;
        assert!(
            !receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Add,
                Some("100"),
                Some("100")
            )
            .unwrap()
        );
        assert!(
            receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Add,
                Some("100"),
                Some("101")
            )
            .unwrap()
        );
        assert!(
            !receipt_delta_outside(
                ProtocolFamily::Dlmm,
                MeteoraAction::Add,
                Some("100"),
                Some("101")
            )
            .unwrap()
        );
        assert!(
            !receipt_delta_outside(ProtocolFamily::Dlmm, MeteoraAction::Add, None, Some("1"))
                .unwrap()
        );
        assert!(
            receipt_delta_outside(ProtocolFamily::Dlmm, MeteoraAction::Add, None, Some("0"))
                .unwrap()
        );
        assert!(
            receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Remove,
                Some("-100"),
                Some("-99")
            )
            .unwrap()
        );
        assert!(
            !receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Remove,
                Some("-100"),
                Some("-100")
            )
            .unwrap()
        );
        assert!(
            receipt_delta_outside(
                ProtocolFamily::Dlmm,
                MeteoraAction::Remove,
                Some("-100"),
                Some("-101")
            )
            .unwrap()
        );
        assert!(
            !receipt_delta_outside(
                ProtocolFamily::Dlmm,
                MeteoraAction::Remove,
                Some("-100"),
                Some("-99")
            )
            .unwrap()
        );
        assert!(
            !receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Close,
                Some("0"),
                Some("0")
            )
            .unwrap()
        );
        assert!(
            receipt_delta_outside(
                ProtocolFamily::DammV2,
                MeteoraAction::Close,
                Some("0"),
                None
            )
            .unwrap()
        );
    }

    #[test]
    fn public_input_parser_accepts_network_fields_and_fixes_write_action() {
        let input = json!({
            "wallet":"main",
            "network":"solana",
            "cluster":"mainnet-beta",
            "family":"damm_v2",
            "pool":"Pool111111111111111111111111111111111111111",
            "action":"open",
            "token_a_mint":"MintA11111111111111111111111111111111111111",
            "token_b_mint":"MintB11111111111111111111111111111111111111",
            "token_a_atoms":"100",
            "token_b_atoms":"200",
            "maximum_slippage_bps":50,
            "maximum_network_fee_lamports":10000,
            "maximum_account_funding_lamports":1000000
        });
        let intent = crate::adapter::parse_public_intent(crate::types::OP_OPEN, &input).unwrap();
        assert_eq!(intent.action, crate::protocol::MeteoraAction::Open);
        assert_eq!(
            intent.token_a_mint.as_deref(),
            Some("MintA11111111111111111111111111111111111111")
        );
        assert_eq!(
            intent.token_b_mint.as_deref(),
            Some("MintB11111111111111111111111111111111111111")
        );
    }

    #[test]
    fn public_input_parser_rejects_action_mismatch_and_unpinned_lp_assets() {
        let mut input = json!({
            "wallet":"main","network":"solana","cluster":"mainnet-beta",
            "family":"dlmm","pool":"Pool111111111111111111111111111111111111111",
            "action":"close","token_a_mint":"A","token_b_mint":"B",
            "position":"Position11111111111111111111111111111111111",
            "maximum_slippage_bps":50,"maximum_network_fee_lamports":10000,
            "maximum_account_funding_lamports":0
        });
        assert!(crate::adapter::parse_public_intent(crate::types::OP_OPEN, &input).is_err());
        input["action"] = json!("open");
        input.as_object_mut().unwrap().remove("token_b_mint");
        assert!(crate::adapter::parse_public_intent(crate::types::OP_OPEN, &input).is_err());
    }
}
