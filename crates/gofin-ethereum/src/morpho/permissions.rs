//! Balance-independent permissions for named Morpho financial plans.
use super::{
    profile,
    vault::{self, VaultWriteInput},
};
use crate::*;
use gofin_domain::{
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MorphoBounds {
    #[serde(default = "zero")]
    pub min_amount: String,
    /// Underlying units for deposits/withdrawals; share units for redemption;
    /// token units for a scoped wallet.approve step.
    pub max_amount: String,
    pub max_fee_eth: String,
    /// Required for named redemption plans, bounding later-step receipt funding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_receipt_assets: Option<String>,
    /// Required only for a wallet.approve permission, identifying the V2 vault
    /// whose underlying or share token may be approved to VaultBundlesV1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
}
fn zero() -> String {
    "0".into()
}
fn fixed_input(input: &Value) -> Value {
    let mut result = input.clone();
    if let Some(object) = result.as_object_mut() {
        for field in ["request_id", "amount", "max_fee_eth", "deadline"] {
            object.remove(field);
        }
    }
    result
}
fn bounds(input: &Value, decimals: u8) -> Result<(MorphoBounds, Quantity, Quantity, Quantity)> {
    let b: MorphoBounds = serde_json::from_value(input.clone()).map_err(|_| {
        GoFinError::validation("Invalid Morpho plan bounds; inspect the boundary schema.")
    })?;
    let minimum = quantity(exact(&b.min_amount, decimals)?);
    let maximum = quantity(exact(&b.max_amount, decimals)?);
    let fee = quantity(exact(&b.max_fee_eth, 18)?);
    require(
        minimum <= maximum && u256(&maximum)? < U256::MAX && !fee.is_zero(),
        "MORPHO_BOUNDS_INVALID",
        "Morpho amount interval and fee ceiling must be finite and valid.",
    )?;
    Ok((b, minimum, maximum, fee))
}
fn fee_cost(adapter: &EthereumAdapter, resource: &ResourceId, fee: Quantity) -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: adapter.native_asset(),
            atoms: fee,
        },
        treatment: CostTreatment::AdditionalDebit,
    }
}

impl EthereumAdapter {
    pub(crate) async fn morpho_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        raw_bounds: &Value,
    ) -> Result<PlanPermission> {
        if operation == "wallet.approve" {
            return self
                .morpho_approval_plan_permission(input, raw_bounds)
                .await;
        }
        let r = VaultWriteInput::parse(operation, input)?;
        let owner = vault::owner(self, &r, true)?;
        self.runtime.assert_chain().await?;
        // No balances or allowances are required: a preceding plan step may
        // supply them. Metadata and deployment verification are performed now.
        let state = self.morpho_vault_state(&r.vault, None, None).await?;
        vault::require_write_compatibility(&state)?;
        let decimals = if operation == "morpho.redeem" {
            state.shares.decimals
        } else {
            state.asset.decimals
        };
        let (b, minimum, maximum, fee) = bounds(raw_bounds, decimals)?;
        require(
            b.vault.is_none() && !maximum.is_zero(),
            "MORPHO_BOUNDS_INVALID",
            "Vault writes require positive max_amount; vault is fixed in the operation input.",
        )?;
        let selected = quantity(exact(
            if r.amount == "all" {
                r.max_shares.as_deref().expect("validated")
            } else {
                &r.amount
            },
            decimals,
        )?);
        require(
            selected >= minimum
                && selected <= maximum
                && quantity(exact(&r.max_fee_eth, 18)?) <= fee,
            "MORPHO_BOUNDS_INVALID",
            "Selected amount or gas exceeds the proposed Morpho plan bounds.",
        )?;
        // Parse and fix the protection quantities before approval, even though
        // their attainability and the current allowance are deferred.
        if let Some(value) = &r.min_shares {
            require(
                !exact(value, state.shares.decimals)?.is_zero(),
                "MORPHO_BOUNDS_INVALID",
                "Deposit min_shares must be positive.",
            )?;
        }
        if let Some(value) = &r.max_shares {
            let units = exact(value, state.shares.decimals)?;
            require(
                !units.is_zero() && units < U256::MAX,
                "MORPHO_BOUNDS_INVALID",
                "Share authority requires a finite positive cap.",
            )?;
        }
        let resource = resource(self.config.chain_id, owner);
        let debit = ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: state.asset.clone(),
                atoms: maximum.clone(),
            },
        };
        let depositing = operation == "morpho.deposit";
        let receipt = if operation == "morpho.redeem" {
            let cap=b.max_receipt_assets.as_deref().ok_or_else(|| GoFinError::validation("Redemption plans require finite max_receipt_assets for later-step funding; this is not a minimum payout guarantee."))?;
            let cap = quantity(exact(cap, state.asset.decimals)?);
            require(
                !cap.is_zero() && u256(&cap)? < U256::MAX,
                "MORPHO_BOUNDS_INVALID",
                "Receipt funding cap must be finite and positive.",
            )?;
            cap
        } else {
            require(
                b.max_receipt_assets.is_none(),
                "MORPHO_BOUNDS_INVALID",
                "Only redemption requires a separately denominated receipt cap.",
            )?;
            maximum.clone()
        };
        let protection = match operation {
            "morpho.deposit" => format!(
                "Minimum {} shares per candidate.",
                r.min_shares.as_deref().expect("validated")
            ),
            "morpho.withdraw" => format!(
                "Maximum {} shares per candidate.",
                r.max_shares.as_deref().expect("validated")
            ),
            _ => format!(
                "{} Redemption accepts execution-time value without a minimum payout; assets received may be zero. Receipt capacity for this step is {} underlying units.",
                if r.amount == "all" {
                    format!(
                        "Select all observed shares within the fixed {} share cap.",
                        r.max_shares.as_deref().expect("validated")
                    )
                } else {
                    "Each candidate fixes its exact share quantity.".into()
                },
                receipt.to_decimal(state.asset.decimals)
            ),
        };
        Ok(PlanPermission {
            spec: BoundarySpec {
                fixed: json!({"operation":operation,"input":fixed_input(input),"signer":lower_address(owner),"resource":resource,"vault":state.vault,"asset":state.asset,"share_asset":state.shares,"code_identity":state.code_identity,"profile_identity":state.profile_identity,"bundles":profile::profile(self.config.chain_id)?.vault_bundles,"all":r.amount=="all"}),
                parameters: json!({"minimum":minimum,"maximum":maximum,"maximum_fee":fee,"maximum_receipt":receipt}),
                maximum_debits: if depositing { vec![debit] } else { vec![] },
                maximum_costs: vec![fee_cost(self, &resource, fee)],
                summary: format!(
                    "{operation}: {}–{} {}; maximum gas {} ETH. Vault {} on chain {}, wallet {}, underlying {}. {}",
                    b.min_amount,
                    b.max_amount,
                    if operation == "morpho.redeem" {
                        "shares"
                    } else {
                        "underlying units"
                    },
                    b.max_fee_eth,
                    state.vault,
                    r.chain_id,
                    r.wallet,
                    state.asset.reference,
                    protection
                ),
            },
            recovery_parent: None,
            possible_credits: if depositing {
                vec![]
            } else {
                vec![FinancialPlanFundingIdentity {
                    resource: resource.clone(),
                    asset: state.asset.clone(),
                }]
            },
            fee_assets: vec![self.native_asset()],
            deferred_receipt: if depositing {
                None
            } else {
                Some(ResourceAmount {
                    resource,
                    amount: Amount {
                        asset: state.asset,
                        atoms: receipt,
                    },
                })
            },
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }

    pub(crate) fn morpho_validate_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        if spec.fixed["operation"] == "wallet.approve" {
            return self.morpho_validate_approval_candidate(spec, prepared);
        }
        let r = VaultWriteInput::parse(&prepared.operation, &prepared.input)?;
        let b = vault::binding(prepared)?;
        require(
            spec.fixed["operation"] == json!(prepared.operation)
                && spec.fixed["input"] == fixed_input(&prepared.input)
                && spec.fixed["signer"] == json!(b.transaction.sender)
                && spec.fixed["resource"] == json!(b.resource)
                && spec.fixed["vault"] == json!(b.vault)
                && spec.fixed["asset"] == json!(b.asset)
                && spec.fixed["share_asset"] == json!(b.share_asset)
                && spec.fixed["code_identity"] == json!(b.code_identity)
                && spec.fixed["profile_identity"] == json!(b.profile_identity)
                && address(spec.fixed["bundles"].as_str().unwrap_or(""))? == address(&b.bundles)?
                && spec.fixed["all"] == json!(b.all),
            "BOUNDARY_VIOLATION",
            "Morpho candidate changed a fixed vault, wallet, asset, route, or share protection.",
        )?;
        let selected = if prepared.operation == "morpho.redeem" {
            &b.shares
        } else {
            &b.assets
        };
        let minimum: Quantity = serde_json::from_value(spec.parameters["minimum"].clone())
            .map_err(|_| GoFinError::validation("Malformed Morpho amount minimum."))?;
        let maximum: Quantity = serde_json::from_value(spec.parameters["maximum"].clone())
            .map_err(|_| GoFinError::validation("Malformed Morpho amount maximum."))?;
        let fee: Quantity = serde_json::from_value(spec.parameters["maximum_fee"].clone())
            .map_err(|_| GoFinError::validation("Malformed Morpho gas cap."))?;
        require(
            selected >= &minimum
                && selected <= &maximum
                && quantity(exact(&r.max_fee_eth, 18)?) <= fee,
            "BOUNDARY_VIOLATION",
            "Morpho amount or fee exceeds the approved interval.",
        )?;
        if prepared.operation == "morpho.redeem" {
            let receipt: Quantity =
                serde_json::from_value(spec.parameters["maximum_receipt"].clone())
                    .map_err(|_| GoFinError::validation("Malformed Morpho receipt cap."))?;
            require(
                b.assets <= receipt,
                "BOUNDARY_VIOLATION",
                "Quoted redemption receipt exceeds the approved plan funding cap.",
            )?;
        }
        Ok(())
    }

    /// A finite allowance/reset/revoke step for the same reviewed vault flow.
    /// This does not grant a general-purpose spender permission.
    pub(crate) async fn morpho_approval_plan_permission(
        &self,
        input: &Value,
        raw_bounds: &Value,
    ) -> Result<PlanPermission> {
        let r = request::parse("wallet.approve", input)?;
        let wallet = self.resolve(&r, true)?;
        let owner = address(&wallet.address)?;
        let raw: MorphoBounds = serde_json::from_value(raw_bounds.clone())
            .map_err(|_| GoFinError::validation("Invalid scoped Morpho allowance bounds."))?;
        let vault = raw.vault.as_deref().ok_or_else(|| {
            GoFinError::validation(
                "A Morpho allowance plan must identify its vault in bounds.vault.",
            )
        })?;
        let state = self.morpho_vault_state(vault, None, None).await?;
        vault::require_write_compatibility(&state)?;
        let token = address(r.token.as_deref().expect("wallet schema"))?;
        let asset = if token == address(&state.asset.reference)? {
            state.asset.clone()
        } else if token == address(&state.shares.reference)? {
            state.shares.clone()
        } else {
            return Err(GoFinError::validation(
                "Allowance token must be this V2 vault's underlying or share token.",
            ));
        };
        let spender = address(r.spender.as_deref().expect("wallet schema"))?;
        require(
            spender == address(profile::profile(self.config.chain_id)?.vault_bundles)?,
            "MORPHO_SPENDER_MISMATCH",
            "Morpho allowance must target the reviewed VaultBundlesV1.",
        )?;
        let (b, minimum, maximum, fee) = bounds(raw_bounds, asset.decimals)?;
        require(
            b.max_receipt_assets.is_none(),
            "MORPHO_BOUNDS_INVALID",
            "Allowance permission creates no receipt funding.",
        )?;
        let selected = quantity(exact(
            r.amount.as_deref().expect("wallet schema"),
            asset.decimals,
        )?);
        require(
            selected >= minimum
                && selected <= maximum
                && quantity(exact(r.max_fee_eth.as_deref().expect("wallet schema"), 18)?) <= fee,
            "MORPHO_BOUNDS_INVALID",
            "Allowance amount or gas exceeds proposed finite bounds.",
        )?;
        let resource = resource(self.config.chain_id, owner);
        Ok(PlanPermission {
            spec: BoundarySpec {
                fixed: json!({"operation":"wallet.approve","morpho_allowance":true,"input":fixed_input(input),"signer":lower_address(owner),"resource":resource,"asset":asset,"spender":lower_address(spender),"vault":state.vault,"code_identity":state.code_identity,"profile_identity":state.profile_identity}),
                parameters: json!({"minimum":minimum,"maximum":maximum,"maximum_fee":fee}),
                maximum_debits: vec![],
                maximum_costs: vec![fee_cost(self, &resource, fee)],
                summary: format!(
                    "Set finite {} allowance to Morpho VaultBundlesV1 {} within {}–{} token units; maximum gas {} ETH. Vault {} on chain {}, wallet {}. Zero revokes spending authority.",
                    asset.reference,
                    lower_address(spender),
                    b.min_amount,
                    b.max_amount,
                    b.max_fee_eth,
                    state.vault,
                    self.config.chain_id,
                    wallet.address
                ),
            },
            recovery_parent: None,
            possible_credits: vec![],
            fee_assets: vec![self.native_asset()],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }

    fn morpho_validate_approval_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        require(
            prepared.operation == "wallet.approve" && spec.fixed["morpho_allowance"] == true,
            "BOUNDARY_VIOLATION",
            "Candidate is not a scoped Morpho allowance.",
        )?;
        let r = request::parse("wallet.approve", &prepared.input)?;
        let b: Binding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| GoFinError::validation("Malformed allowance binding."))?;
        require(
            spec.fixed["input"] == fixed_input(&prepared.input)
                && spec.fixed["signer"] == json!(b.transaction.sender)
                && spec.fixed["resource"] == json!(b.resource)
                && spec.fixed["asset"] == json!(b.token)
                && spec.fixed["spender"] == json!(b.spender),
            "BOUNDARY_VIOLATION",
            "Allowance candidate changed its fixed wallet, asset, or spender.",
        )?;
        let minimum: Quantity = serde_json::from_value(spec.parameters["minimum"].clone())
            .map_err(|_| GoFinError::validation("Malformed allowance minimum."))?;
        let maximum: Quantity = serde_json::from_value(spec.parameters["maximum"].clone())
            .map_err(|_| GoFinError::validation("Malformed allowance maximum."))?;
        let fee: Quantity = serde_json::from_value(spec.parameters["maximum_fee"].clone())
            .map_err(|_| GoFinError::validation("Malformed allowance gas cap."))?;
        require(
            b.units >= minimum
                && b.units <= maximum
                && u256(&b.units)? < U256::MAX
                && quantity(exact(r.max_fee_eth.as_deref().expect("wallet schema"), 18)?) <= fee,
            "BOUNDARY_VIOLATION",
            "Allowance amount or fee exceeds its finite plan scope.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_variation_keeps_share_protections_and_consent_fixed() {
        let a = json!({"wallet":"w","vault":"v","amount":"1","max_shares":"2","accept_prevailing_value":true,"deadline":1,"request_id":"a","max_fee_eth":"0.1"});
        let mut b = a.clone();
        b["amount"] = json!("2");
        b["deadline"] = json!(5);
        b["request_id"] = json!("b");
        b["max_fee_eth"] = json!("0.2");
        assert_eq!(fixed_input(&a), fixed_input(&b));
        b["max_shares"] = json!("3");
        assert_ne!(fixed_input(&a), fixed_input(&b));
        b["max_shares"] = json!("2");
        b["accept_prevailing_value"] = json!(false);
        assert_ne!(fixed_input(&a), fixed_input(&b));
    }
    #[test]
    fn invalid_or_unlimited_intervals_are_rejected() {
        assert!(
            bounds(
                &json!({"min_amount":"2","max_amount":"1","max_fee_eth":"0.1"}),
                6
            )
            .is_err()
        );
        assert!(
            bounds(
                &json!({"max_amount":U256::MAX.to_string(),"max_fee_eth":"0.1"}),
                0
            )
            .is_err()
        );
        assert!(bounds(&json!({"max_amount":"0","max_fee_eth":"0.1"}), 6).is_ok());
    }
}
