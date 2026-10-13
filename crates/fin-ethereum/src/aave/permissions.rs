//! Balance-independent named-plan scopes. Authority never assumes future funds.
use super::super::*;
use fin_domain::{
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanCumulativeScope, FinancialPlanFundingIdentity, PlanPermission},
};
use fin_evm::quantity;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AaveBounds {
    #[serde(default = "zero_decimal")]
    pub min_amount: String,
    pub max_amount: String,
    pub max_fee_eth: String,
    /// Gross new variable debt, in underlying units, across this market/asset's
    /// borrowing steps. Includes finite mint rounding; repayment never refunds.
    #[serde(default)]
    pub max_cumulative_borrow: Option<String>,
}
fn zero_decimal() -> String {
    "0".into()
}
fn fixed_input(input: &Value) -> Value {
    let mut fixed = input.clone();
    if let Some(object) = fixed.as_object_mut() {
        for key in ["request_id", "amount", "max_fee_eth"] {
            object.remove(key);
        }
    }
    fixed
}
impl EthereumAdapter {
    pub(crate) async fn aave_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        raw: &Value,
    ) -> Result<PlanPermission> {
        require(
            matches!(
                operation,
                "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral"
            ),
            "PLAN_PERMISSION_UNSUPPORTED",
            "This Aave operation has no financial plan scope.",
        )?;
        let request = request::parse(operation, input)?;
        let wallet = self.resolve(&request, true)?;
        self.runtime.assert_chain().await?;
        let token = self
            .token(request.token.as_deref().expect("schema"), None)
            .await?;
        let reserve = self.reserve(&token.reference, None).await?;
        self.verify_receipt_identity(&token, &reserve, None).await?;
        let bounds: AaveBounds = serde_json::from_value(raw.clone()).map_err(|_| {
            FinError::validation(
                "Invalid Aave plan bounds; specify min_amount, max_amount and max_fee_eth.",
            )
        })?;
        let minimum = Quantity::from_decimal(&bounds.min_amount, token.decimals)?;
        let maximum = Quantity::from_decimal(&bounds.max_amount, token.decimals)?;
        let fee = Quantity::from_decimal(&bounds.max_fee_eth, 18)?;
        require(
            minimum <= maximum
                && !fee.is_zero()
                && (operation == "aave.collateral" || !maximum.is_zero()),
            "AAVE_BOUNDS_INVALID",
            "Aave quantity interval and fee ceiling must be finite and valid.",
        )?;
        if operation == "aave.collateral" {
            require(
                maximum.is_zero() && minimum.is_zero(),
                "AAVE_BOUNDS_INVALID",
                "Collateral permission has zero asset quantity; its enabled setting is fixed.",
            )?;
        }
        let all = request.amount.as_deref() == Some("all");
        let selected = if operation == "aave.collateral" {
            Quantity::zero()
        } else {
            Quantity::from_decimal(
                if all {
                    request.max_amount.as_deref().expect("schema")
                } else {
                    request.amount.as_deref().expect("schema")
                },
                token.decimals,
            )?
        };
        require(
            selected >= minimum
                && selected <= maximum
                && Quantity::from_decimal(request.max_fee_eth.as_deref().expect("schema"), 18)?
                    <= fee,
            "AAVE_BOUNDS_INVALID",
            "Selected quantity or fee exceeds proposed Aave bounds.",
        )?;
        let resource = resource(self.config.chain_id, address(&wallet.address)?);
        let asset = token.asset(self);
        let debit = ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: maximum.clone(),
            },
        };
        let cost = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: self.native_asset(),
                atoms: fee,
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let maximum_debits = if matches!(operation, "aave.supply" | "aave.repay") {
            vec![debit.clone()]
        } else {
            vec![]
        };
        let mut cumulative_scope = None;
        if operation == "aave.borrow" {
            require(
                request.max_debt_usd.is_some() && request.max_debt_to_collateral_bps.is_some(),
                "AAVE_RISK_BOUNDS_REQUIRED",
                "Variable borrowing plans require max_debt_usd and max_debt_to_collateral_bps in the fixed input.",
            )?;
            let cumulative = Quantity::from_decimal(
                bounds.max_cumulative_borrow.as_deref().ok_or_else(|| {
                    FinError::validation(
                        "Borrow plans require max_cumulative_borrow including mint rounding.",
                    )
                })?,
                token.decimals,
            )?;
            require(
                cumulative >= maximum,
                "AAVE_BOUNDS_INVALID",
                "Cumulative borrowing ceiling cannot be smaller than the per-attempt quantity ceiling.",
            )?;
            cumulative_scope = Some(FinancialPlanCumulativeScope {
                scope_id: format!(
                    "debt:{}:{}:{}:{}",
                    self.config.chain_id,
                    self.aave()?.pool,
                    wallet.address,
                    token.reference
                ),
                maximum_debits: vec![],
                maximum_costs: vec![cost.clone()],
                maximum_obligations: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: cumulative,
                    },
                }],
            });
        } else {
            require(
                bounds.max_cumulative_borrow.is_none(),
                "AAVE_BOUNDS_INVALID",
                "Only borrowing can declare cumulative borrowing authority.",
            )?;
        }
        let mut maximum_costs = vec![cost.clone()];
        if matches!(operation, "aave.supply" | "aave.repay" | "aave.withdraw") {
            maximum_costs.push(CostBound {
                location: if operation == "aave.withdraw" {
                    AccountingLocation::Position {
                        resource: resource.clone(),
                        position: position(&self.aave()?.pool, &token.reference, "supply"),
                        position_kind: PositionKind::Supply,
                    }
                } else {
                    AccountingLocation::Cash {
                        resource: resource.clone(),
                    }
                },
                amount: Amount {
                    asset: asset.clone(),
                    atoms: maximum.clone(),
                },
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        let mut possible_credits = Vec::new();
        let mut receipt_capacity = None;
        if matches!(operation, "aave.borrow" | "aave.withdraw") {
            let recipient = request
                .recipient
                .as_deref()
                .map(address)
                .transpose()?
                .unwrap_or(address(&wallet.address)?);
            let receipt_resource = super::super::resource(self.config.chain_id, recipient);
            possible_credits.push(FinancialPlanFundingIdentity {
                resource: receipt_resource.clone(),
                asset: asset.clone(),
            });
            if operation == "aave.withdraw" {
                // A withdrawal releases an already-held position, so it has
                // no principal debit authority for the generic plan progress
                // check to use. Declare the adapter-bounded receipt capacity;
                // settlement still proves the exact output and full-claim
                // closure independently.
                receipt_capacity = Some(ResourceAmount {
                    resource: receipt_resource,
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: maximum.clone(),
                    },
                });
            }
        }
        Ok(PlanPermission {
            recovery_parent: None,
            spec: BoundarySpec {
                fixed: json!({"operation":operation,"input":fixed_input(input),"pool":self.aave()?.pool,"oracle":self.aave()?.oracle,"signer":wallet.address,"token":token.asset(self),"a_token":reserve.a_token,"debt_token":reserve.variable_debt_token,"all":all,"emode":quantity(self.aave_mode(address(&wallet.address)?,None).await?)}),
                parameters: json!({"minimum":minimum,"maximum":maximum,"max_fee_eth":bounds.max_fee_eth}),
                maximum_debits,
                maximum_costs,
                summary: format!(
                    "{}: {}–{} {}; maximum fee {} ETH. Market, permissions and risk bounds remain fixed; later balances are checked only at dispatch.",
                    operation,
                    bounds.min_amount,
                    bounds.max_amount,
                    crate::amm_permissions::safe_display_label(Some(&token.symbol)),
                    bounds.max_fee_eth
                ),
            },
            possible_credits,
            fee_assets: vec![self.native_asset()],
            deferred_receipt: receipt_capacity,
            authorization_seconds: None,
            cumulative_scope,
        })
    }
    pub(crate) fn validate_aave_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let request = request::parse(&prepared.operation, &prepared.input)?;
        let binding: Binding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| FinError::validation("Invalid Aave binding."))?;
        require(
            spec.fixed["operation"] == json!(prepared.operation)
                && spec.fixed["input"] == fixed_input(&prepared.input)
                && spec.fixed["pool"] == json!(binding.pool)
                && spec.fixed["oracle"] == json!(self.aave()?.oracle)
                && spec.fixed["signer"] == json!(binding.transaction.sender)
                && spec.fixed["token"] == json!(binding.token)
                && spec.fixed["a_token"] == json!(binding.a_token)
                && spec.fixed["debt_token"] == json!(binding.debt_token)
                && spec.fixed["all"] == json!(request.amount.as_deref() == Some("all"))
                && spec.fixed["emode"] == json!(binding.aave_emode),
            "BOUNDARY_VIOLATION",
            "Aave candidate changed fixed market, asset, permissions or risk scope.",
        )?;
        let units = if prepared.operation == "aave.collateral" {
            Quantity::zero()
        } else {
            Quantity::from_decimal(
                if request.amount.as_deref() == Some("all") {
                    request.max_amount.as_deref().expect("schema")
                } else {
                    request.amount.as_deref().expect("schema")
                },
                binding.token.decimals,
            )?
        };
        let minimum: Quantity = serde_json::from_value(spec.parameters["minimum"].clone())
            .map_err(|_| FinError::validation("Invalid Aave minimum."))?;
        let maximum: Quantity = serde_json::from_value(spec.parameters["maximum"].clone())
            .map_err(|_| FinError::validation("Invalid Aave maximum."))?;
        require(
            units >= minimum
                && units <= maximum
                && Quantity::from_decimal(request.max_fee_eth.as_deref().expect("schema"), 18)?
                    <= Quantity::from_decimal(
                        spec.parameters["max_fee_eth"].as_str().unwrap_or(""),
                        18,
                    )?,
            "BOUNDARY_VIOLATION",
            "Aave candidate quantity or fee exceeds approved boundaries.",
        )
    }
}
