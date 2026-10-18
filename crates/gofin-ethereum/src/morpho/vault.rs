//! Morpho Vault V2 standard asset flows. Calldata is built locally from the
//! reviewed VaultBundlesV1 ABI; no API transaction is trusted or signed.
use super::{profile, reads::MorphoVaultState};
use crate::abi::Erc20;
use crate::*;
use gofin_evm::alloy::{
    primitives::{B256, Bytes},
    sol,
    sol_types::{SolCall, SolEvent},
};
use gofin_evm::{CanonicalReceipt, rpc_bytes, rpc_u64};

sol! {
    struct TokenPermit { uint8 kind; bytes data; }
    struct SharePermit { uint256 value; uint256 nonce; uint256 deadline; uint8 v; bytes32 r; bytes32 s; }
    interface VaultBundles {
        function vaultBundlesV1Deposit(address vault,uint256 assets,uint256 maxSharePriceE27,TokenPermit assetPermit,uint256 referralFeePct,address referralFeeRecipient,uint256 deadline) external payable;
        function vaultBundlesV1Withdraw(address vault,uint256 assets,uint256 shares,SharePermit sharesPermit,uint256 referralFeePct,address referralFeeRecipient,uint256 deadline) external;
    }
    interface Vault {
        function previewDeposit(uint256 assets) external view returns (uint256);
        function previewWithdraw(uint256 assets) external view returns (uint256);
        function previewRedeem(uint256 shares) external view returns (uint256);
        event Deposit(address indexed sender,address indexed onBehalf,uint256 assets,uint256 shares);
        event Withdraw(address indexed sender,address indexed receiver,address indexed onBehalf,uint256 assets,uint256 shares);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultWriteInput {
    pub wallet: String,
    pub network: String,
    pub chain_id: u64,
    pub vault: String,
    pub request_id: String,
    pub max_fee_eth: String,
    /// Exact underlying units for deposit/withdraw; share units or "all" for redeem.
    pub amount: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_shares: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_shares: Option<String>,
    #[serde(default)]
    pub accept_prevailing_value: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<u64>,
}

impl VaultWriteInput {
    pub(super) fn parse(operation: &str, input: &Value) -> Result<Self> {
        let request: Self = serde_json::from_value(input.clone()).map_err(|_| {
            GoFinError::validation("Invalid Morpho vault input; inspect the operation schema.")
        })?;
        require(
            matches!(
                operation,
                "morpho.deposit" | "morpho.withdraw" | "morpho.redeem"
            ),
            "OPERATION_UNAVAILABLE",
            "Unknown Morpho vault write.",
        )?;
        require(
            !request.wallet.trim().is_empty() && !request.request_id.trim().is_empty(),
            "INVALID_INPUT",
            "Select a wallet and a nonempty request_id.",
        )?;
        require(
            matches!(request.network.as_str(), "evm" | "ethereum"),
            "NETWORK_MISMATCH",
            "Morpho network must identify the EVM family; select the exact chain_id separately.",
        )?;
        address(&request.vault)?;
        match operation {
            "morpho.deposit" => require(
                request.min_shares.is_some()
                    && request.max_shares.is_none()
                    && !request.accept_prevailing_value
                    && request.amount != "all",
                "INVALID_INPUT",
                "Deposit requires exact assets and min_shares only.",
            )?,
            "morpho.withdraw" => require(
                request.max_shares.is_some()
                    && request.min_shares.is_none()
                    && !request.accept_prevailing_value
                    && request.amount != "all",
                "INVALID_INPUT",
                "Withdrawal requires exact assets and max_shares only.",
            )?,
            _ => require(
                request.min_shares.is_none()
                    && request.accept_prevailing_value
                    && (request.amount != "all" || request.max_shares.is_some()),
                "MORPHO_REDEMPTION_CONSENT_REQUIRED",
                "Redemption requires explicit acceptance of the execution-time value; all also requires finite max_shares. No minimum assets are guaranteed.",
            )?,
        }
        Ok(request)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct VaultBinding {
    pub transaction: FrozenTransaction,
    pub resource: ResourceId,
    pub vault: String,
    pub asset: AssetId,
    pub share_asset: AssetId,
    pub bundles: String,
    pub code_identity: String,
    pub profile_identity: String,
    pub assets: Quantity,
    pub shares: Quantity,
    pub minimum_shares: Quantity,
    pub maximum_shares: Quantity,
    pub maximum_share_price_e27: Quantity,
    pub allowance_before: Quantity,
    pub deadline: u64,
    pub before_shares: Quantity,
    pub before_asset_balance: Quantity,
    pub before_anchor: PositionAnchor,
    pub all: bool,
    #[serde(
        default,
        rename = "_gofin_verified_baseline_anchors",
        alias = "_fin_verified_baseline_anchors"
    )]
    pub verified_baseline_anchors: Vec<PositionAnchor>,
    #[serde(
        default,
        rename = "_gofin_usd_ceiling_v1",
        alias = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    value_bounds: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_boundary_authorization",
        alias = "__fin_boundary_authorization",
        skip_serializing_if = "Option::is_none"
    )]
    boundary_authorization: Option<Value>,
    #[serde(
        default,
        rename = "__gofin_financial_plan",
        alias = "__fin_financial_plan",
        skip_serializing_if = "Option::is_none"
    )]
    financial_plan: Option<Value>,
}

pub(super) fn position_id(vault: &str) -> String {
    format!("morpho:v2:{vault}:investment")
}

pub(crate) fn investment_anchor(state: &MorphoVaultState, owner: Address) -> PositionAnchor {
    investment_anchor_observed(state, owner, now())
}

fn investment_anchor_observed(
    state: &MorphoVaultState,
    owner: Address,
    observed_at: String,
) -> PositionAnchor {
    PositionAnchor {
        resource: resource(
            state
                .asset
                .network
                .strip_prefix("ethereum:")
                .and_then(|v| v.parse().ok())
                .expect("verified EVM identity"),
            owner,
        ),
        position: position_id(&state.vault),
        position_kind: PositionKind::Investment,
        principal: Amount {
            asset: state.asset.clone(),
            atoms: state.owner_assets.clone(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"protocol":"morpho-vault-v2","vault":state.vault,"share_asset":state.shares,"share_balance":state.owner_shares,"block":state.block,"block_hash":state.block_hash,"total_assets":state.total_assets,"total_supply":state.total_supply,"virtual_shares":state.virtual_shares,"code_identity":state.code_identity,"profile_identity":state.profile_identity}),
        observed_at,
    }
}

pub(super) fn owner(
    adapter: &EthereumAdapter,
    request: &VaultWriteInput,
    signing: bool,
) -> Result<Address> {
    require(
        request.chain_id == adapter.config.chain_id
            && matches!(request.network.as_str(), "evm" | "ethereum"),
        "NETWORK_MISMATCH",
        "Morpho request targets another configured network.",
    )?;
    profile::profile(request.chain_id)?;
    let matches = adapter
        .wallets
        .iter()
        .filter(|w| w.name == request.wallet || w.address.eq_ignore_ascii_case(&request.wallet))
        .collect::<Vec<_>>();
    let [wallet] = matches.as_slice() else {
        return Err(GoFinError::validation(
            "Select one exact enrolled EVM wallet.",
        ));
    };
    require(
        !signing || wallet.signing_enabled,
        "SIGNING_UNAVAILABLE",
        "This wallet has no protected signing authority.",
    )?;
    address(&wallet.address)
}

fn positive(value: &str, decimals: u8) -> Result<U256> {
    let result = exact(value, decimals)?;
    require(
        result > U256::ZERO && result < U256::MAX,
        "INVALID_AMOUNT",
        "Use a positive finite amount.",
    )?;
    Ok(result)
}

/// Encode a conservative share floor in the periphery's rounded price bound.
fn deposit_price_ceiling(assets: U256, minimum_shares: U256) -> Result<U256> {
    require(
        !minimum_shares.is_zero(),
        "INVALID_AMOUNT",
        "Deposit minimum shares must be positive.",
    )?;
    let numerator = mul(assets, pow10(27)?)?;
    // ceil(numerator / mintedShares) <= ceiling is equivalent to
    // mintedShares >= ceil(numerator / ceiling). Choose the largest ceiling
    // that still rejects minimum_shares-1. E27 quantization may strengthen
    // the requested floor by a few share atoms; it must never weaken it.
    let ceiling = if minimum_shares == U256::from(1) {
        numerator
    } else {
        sub(numerator, U256::from(1))? / (minimum_shares - U256::from(1))
    };
    require(
        !ceiling.is_zero(),
        "MORPHO_SHARE_FLOOR_PRECISION",
        "The requested share floor exceeds the periphery's E27 price precision.",
    )?;
    Ok(ceiling)
}

fn calldata(operation: &str, binding: &VaultBinding) -> Result<Bytes> {
    let vault = address(&binding.vault)?;
    Ok(if operation == "morpho.deposit" {
        VaultBundles::vaultBundlesV1DepositCall {
            vault,
            assets: u256(&binding.assets)?,
            maxSharePriceE27: u256(&binding.maximum_share_price_e27)?,
            assetPermit: TokenPermit {
                kind: 0,
                data: Bytes::new(),
            },
            referralFeePct: U256::ZERO,
            referralFeeRecipient: Address::ZERO,
            deadline: U256::from(binding.deadline),
        }
        .abi_encode()
        .into()
    } else {
        VaultBundles::vaultBundlesV1WithdrawCall {
            vault,
            assets: if operation == "morpho.withdraw" {
                u256(&binding.assets)?
            } else {
                U256::ZERO
            },
            shares: if operation == "morpho.redeem" {
                u256(&binding.shares)?
            } else {
                U256::ZERO
            },
            sharesPermit: SharePermit {
                value: U256::ZERO,
                nonce: U256::ZERO,
                deadline: U256::ZERO,
                v: 0,
                r: B256::ZERO,
                s: B256::ZERO,
            },
            referralFeePct: U256::ZERO,
            referralFeeRecipient: Address::ZERO,
            deadline: U256::from(binding.deadline),
        }
        .abi_encode()
        .into()
    })
}

fn signed_difference(after: &Quantity, before: &Quantity) -> Result<SignedQuantity> {
    if after >= before {
        SignedQuantity::new(after.checked_sub(before)?.to_string())
    } else {
        SignedQuantity::new(format!("-{}", before.checked_sub(after)?))
    }
}

fn claim_limit() -> Quantity {
    // A successful VaultV2 exit first accrues with newTotalAssets.toUint128()
    // and subtracts assets.toUint128() from _totalAssets. This bounds actual
    // payouts, unlike the view-only totalAssets(), which is uint256.
    // This is solely an effect range: the envelope fixes finite share authority
    // and valuation/plan funding use the preview/caller's independent bounds.
    quantity(U256::from(u128::MAX))
}

pub(super) fn require_write_compatibility(state: &MorphoVaultState) -> Result<()> {
    require(
        state.write_compatibility["supported"] == true,
        "MORPHO_VAULT_WRITE_UNSUPPORTED",
        "The vault adapter or gate configuration is outside the reviewed standard VaultBundlesV1 flow.",
    )
}

fn write_summary(operation: &str, request: &VaultWriteInput, b: &VaultBinding) -> String {
    let action = match operation {
        "morpho.deposit" => format!(
            "Deposit {} units of {} with at least {} vault shares",
            b.assets.to_decimal(b.asset.decimals),
            b.asset.reference,
            b.minimum_shares.to_decimal(b.share_asset.decimals)
        ),
        "morpho.withdraw" => format!(
            "Withdraw exactly {} units of {} using at most {} vault shares",
            b.assets.to_decimal(b.asset.decimals),
            b.asset.reference,
            b.maximum_shares.to_decimal(b.share_asset.decimals)
        ),
        _ => format!(
            "Redeem {}{} vault shares at execution-time value; estimated receipt {} units of {}. No minimum asset payout is guaranteed; the receipt may be zero",
            if b.all { "all selected " } else { "" },
            b.shares.to_decimal(b.share_asset.decimals),
            b.assets.to_decimal(b.asset.decimals),
            b.asset.reference
        ),
    };
    format!(
        "{action}. Morpho V2 vault {} on chain {} using {}. Maximum gas {} ETH.",
        b.vault, request.chain_id, request.wallet, request.max_fee_eth
    )
}

pub(super) async fn valuation(
    adapter: &EthereumAdapter,
    asset: &AssetId,
    movement: &Quantity,
    gas: U256,
) -> Result<Valuation> {
    let quote = |a: AssetId| async move {
        let unit = quantity(pow10(a.decimals)?);
        let (price, source, observed_at) = adapter
            .quote_verified_asset_usd_observed(&a, unit.clone())
            .await?;
        Ok::<_, GoFinError>(AssetPrice {
            asset: a,
            usd_micros_per_unit: price,
            unit_atoms: unit,
            observed_at,
            source,
        })
    };
    let (principal, fee) = tokio::try_join!(quote(asset.clone()), quote(adapter.native_asset()))?;
    let movement_usd =
        movement.mul_div_ceil(&principal.usd_micros_per_unit, &principal.unit_atoms)?;
    let fee_usd = quantity(gas).mul_div_ceil(&fee.usd_micros_per_unit, &fee.unit_atoms)?;
    let source = principal.source.clone();
    let observed_at = [principal.observed_at.clone(), fee.observed_at.clone()]
        .into_iter()
        .min_by_key(|value| {
            chrono::DateTime::parse_from_rfc3339(value).expect("validated price timestamp")
        })
        .expect("two prices");
    let prices = vec![principal, fee];
    Ok(Valuation {
        movement_usd_micros: movement_usd,
        maximum_fee_usd_micros: fee_usd,
        additional_allocation_usd_micros: Quantity::zero(),
        observed_at,
        source,
        prices,
    })
}

impl EthereumAdapter {
    pub(crate) async fn morpho_prepare(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let r = VaultWriteInput::parse(operation, input)?;
        let owner = owner(self, &r, true)?;
        self.runtime.assert_chain().await?;
        let state = self.morpho_vault_state(&r.vault, Some(owner), None).await?;
        require_write_compatibility(&state)?;
        let p = profile::profile(self.config.chain_id)?;
        let bundles = address(p.vault_bundles)?;
        let vault = address(&state.vault)?;
        let current_time = self.clock.now();
        let current_seconds = u64::try_from(current_time.timestamp())
            .map_err(|_| GoFinError::validation("Invalid observation clock."))?;
        let deadline = r.deadline.unwrap_or(current_seconds.saturating_add(300));
        require(
            deadline > current_seconds,
            "PREVIEW_EXPIRED",
            "Morpho transaction deadline has expired.",
        )?;
        let assets;
        let mut shares = U256::ZERO;
        let mut minimum_shares = U256::ZERO;
        let mut maximum_shares = U256::ZERO;
        let mut price = U256::ZERO;
        match operation {
            "morpho.deposit" => {
                assets = positive(&r.amount, state.asset.decimals)?;
                minimum_shares = positive(
                    r.min_shares.as_deref().expect("validated"),
                    state.shares.decimals,
                )?;
                let quoted = self
                    .runtime
                    .call(
                        vault,
                        &Vault::previewDepositCall { assets },
                        Some(state.block),
                    )
                    .await?;
                require(
                    quoted >= minimum_shares,
                    "MORPHO_SHARE_FLOOR",
                    "Deposit quote is below min_shares.",
                )?;
                price = deposit_price_ceiling(assets, minimum_shares)?;
                let numerator = mul(assets, pow10(27)?)?;
                require(
                    add(
                        numerator / quoted,
                        U256::from((numerator % quoted != U256::ZERO) as u8),
                    )? <= price,
                    "MORPHO_SHARE_FLOOR_PRECISION",
                    "The current share quote cannot satisfy the conservatively rounded minimum; allow a small share margin.",
                )?;
            }
            "morpho.withdraw" => {
                assets = positive(&r.amount, state.asset.decimals)?;
                maximum_shares = positive(
                    r.max_shares.as_deref().expect("validated"),
                    state.shares.decimals,
                )?;
                shares = self
                    .runtime
                    .call(
                        vault,
                        &Vault::previewWithdrawCall { assets },
                        Some(state.block),
                    )
                    .await?;
                require(
                    shares <= maximum_shares && shares <= u256(&state.owner_shares)?,
                    "MORPHO_SHARE_CAP",
                    "Withdrawal exceeds its share cap or current position.",
                )?;
            }
            _ => {
                shares = if r.amount == "all" {
                    u256(&state.owner_shares)?
                } else {
                    positive(&r.amount, state.shares.decimals)?
                };
                maximum_shares = r
                    .max_shares
                    .as_deref()
                    .map(|v| positive(v, state.shares.decimals))
                    .transpose()?
                    .unwrap_or(shares);
                require(
                    !shares.is_zero()
                        && shares < U256::MAX
                        && shares <= maximum_shares
                        && shares <= u256(&state.owner_shares)?,
                    "MORPHO_SHARE_CAP",
                    "Redemption exceeds finite max_shares or available shares.",
                )?;
                assets = self
                    .runtime
                    .call(
                        vault,
                        &Vault::previewRedeemCall { shares },
                        Some(state.block),
                    )
                    .await?;
            }
        }
        let allowance_token = if operation == "morpho.deposit" {
            address(&state.asset.reference)?
        } else {
            vault
        };
        let before_asset_balance = self
            .runtime
            .call(
                address(&state.asset.reference)?,
                &Erc20::balanceOfCall { owner },
                Some(state.block),
            )
            .await?;
        require(
            operation != "morpho.deposit" || before_asset_balance >= assets,
            "BALANCE_INSUFFICIENT",
            "Wallet underlying balance does not cover the exact deposit.",
        )?;
        let allowance = self
            .runtime
            .call(
                allowance_token,
                &Erc20::allowanceCall {
                    owner,
                    spender: bundles,
                },
                Some(state.block),
            )
            .await?;
        let required = if operation == "morpho.deposit" {
            assets
        } else {
            shares
        };
        require(
            allowance >= required
                && allowance < U256::MAX
                && (operation == "morpho.deposit" || allowance <= maximum_shares),
            "ALLOWANCE_INSUFFICIENT",
            "A finite allowance to the reviewed VaultBundlesV1 must cover this action; withdrawal share allowance must not exceed max_shares.",
        )?;
        // The baseline and prepared preview share the same observation instant.
        // Capturing a later wall-clock time here makes the baseline appear to
        // come from the future to the independent engine freshness validator.
        let baseline = investment_anchor_observed(&state, owner, current_time.to_rfc3339());
        let candidates = context
            .positions
            .iter()
            .filter(|a| a.resource == baseline.resource && a.position == baseline.position)
            .collect::<Vec<_>>();
        require(
            candidates.len() <= 1,
            "AMBIGUOUS_POSITION_ANCHOR",
            "Morpho position has multiple prior anchors.",
        )?;
        let before = candidates.first().copied().unwrap_or(&baseline).clone();
        require(
            before.position_kind == PositionKind::Investment
                && before.principal.asset == state.asset
                && before.evidence["share_balance"] == json!(state.owner_shares)
                && before.evidence["vault"] == json!(state.vault),
            "MORPHO_POSITION_CHANGED",
            "Position shares differ from the trusted anchor; inspect external activity before continuing.",
        )?;
        let fee = positive(&r.max_fee_eth, 18)?;
        // A placeholder envelope is replaced before any binding is returned.
        let placeholder = FrozenTransaction {
            chain_id: self.config.chain_id,
            sender: lower_address(owner),
            to: lower_address(bundles),
            nonce: 0,
            gas: 0,
            value: Quantity::zero(),
            calldata: "0x".into(),
            max_fee_per_gas: Quantity::zero(),
            max_priority_fee_per_gas: Quantity::zero(),
            rollup_fee: None,
        };
        let mut b = VaultBinding {
            transaction: placeholder,
            resource: baseline.resource.clone(),
            vault: state.vault.clone(),
            asset: state.asset.clone(),
            share_asset: state.shares.clone(),
            bundles: lower_address(bundles),
            code_identity: state.code_identity.clone(),
            profile_identity: state.profile_identity.clone(),
            assets: quantity(assets),
            shares: quantity(shares),
            minimum_shares: quantity(minimum_shares),
            maximum_shares: quantity(maximum_shares),
            maximum_share_price_e27: quantity(price),
            allowance_before: quantity(allowance),
            deadline,
            before_shares: state.owner_shares.clone(),
            before_asset_balance: quantity(before_asset_balance),
            before_anchor: before,
            all: r.amount == "all",
            verified_baseline_anchors: vec![baseline],
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        };
        let call = self
            .runtime
            .prepare(owner, bundles, U256::ZERO, calldata(operation, &b)?, fee)
            .await?;
        b.transaction = call.transaction;
        let gas = b.transaction.maximum_cost()?;
        require(
            self.runtime.balance(owner, Some(state.block)).await? >= gas,
            "BALANCE_INSUFFICIENT",
            "Wallet native balance does not cover the frozen gas reserve.",
        )?;
        let cash = AccountingLocation::Cash {
            resource: b.resource.clone(),
        };
        let position = AccountingLocation::Position {
            resource: b.resource.clone(),
            position: position_id(&b.vault),
            position_kind: PositionKind::Investment,
        };
        let depositing = operation == "morpho.deposit";
        let maximum = if operation == "morpho.redeem" {
            claim_limit()
        } else {
            b.assets.clone()
        };
        let minimum = if operation == "morpho.redeem" {
            Quantity::zero()
        } else {
            b.assets.clone()
        };
        let mut expected = vec![
            EffectConstraint::OriginMovement {
                from: if depositing {
                    cash.clone()
                } else {
                    position.clone()
                },
                to: if depositing { position } else { cash.clone() },
                debit_asset: b.asset.clone(),
                credit_asset: b.asset.clone(),
                maximum_debit: maximum.clone(),
                minimum_credit: minimum.clone(),
            },
            EffectConstraint::PositionOfKind {
                resource: b.resource.clone(),
                position: position_id(&b.vault),
                asset: b.asset.clone(),
                position_kind: PositionKind::Investment,
                direction: if depositing {
                    Direction::Increase
                } else {
                    Direction::Decrease
                },
                maximum_change: maximum,
            },
            EffectConstraint::Accrual {
                resource: b.resource.clone(),
                position: position_id(&b.vault),
                asset: b.asset.clone(),
                position_kind: PositionKind::Investment,
                maximum_change: claim_limit(),
            },
        ];
        if depositing {
            expected.push(EffectConstraint::Debit {
                resource: b.resource.clone(),
                asset: b.asset.clone(),
                maximum_debit: b.assets.clone(),
            });
        } else {
            expected.push(EffectConstraint::Credit {
                to: b.resource.clone(),
                asset: b.asset.clone(),
                minimum_credit: minimum,
            });
        }
        let intent = FinancialIntent {
            debits: if depositing {
                vec![ResourceAmount {
                    resource: b.resource.clone(),
                    amount: amount(&b.asset, assets),
                }]
            } else {
                vec![]
            },
            maximum_costs: vec![CostBound {
                location: cash,
                amount: amount(&self.native_asset(), gas),
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected,
            locks: vec![b.resource.clone()],
        };
        let mut warnings = vec!["Only standard withdrawals using vault idle assets and its liquidity adapter are supported; insufficient liquidity reverts.".into()];
        if operation == "morpho.redeem" {
            warnings.push("Redeem the fixed approved shares at the execution-time vault value. There is no guaranteed minimum asset receipt; realized losses can reduce the payout to zero.".into());
        }
        let summary = write_summary(operation, &r, &b);
        let expires = current_time + chrono::Duration::seconds(300);
        Ok(PreparedOperation {operation:operation.into(),input:input.clone(),binding:serde_json::to_value(&b).expect("vault binding"),summary,checks:vec![Check {name:"verified Morpho V2 vault, exact allowance and simulated standard flow".into(),status:CheckStatus::Passed,detail:"Factory membership, chain, contract identities, finite shares and local calldata are bound before signing.".into()}],warnings,observed_at:current_time.to_rfc3339(),expires_at:expires.min(chrono::DateTime::from_timestamp(i64::try_from(deadline).map_err(|_| GoFinError::validation("Deadline exceeds timestamp range."))?,0).ok_or_else(|| GoFinError::validation("Invalid deadline."))?).to_rfc3339(),valuation:Some(valuation(self,&b.asset,&b.assets,gas).await?),intent,operation_version:1,action_deadline:None})
    }

    pub(crate) async fn morpho_revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        let r = VaultWriteInput::parse(&prepared.operation, &prepared.input)?;
        let b = binding(prepared)?;
        let owner = owner(self, &r, true)?;
        require(
            chrono::DateTime::parse_from_rfc3339(&prepared.expires_at)
                .is_ok_and(|t| t > self.clock.now())
                && self.clock.now().timestamp() < i64::try_from(b.deadline).unwrap_or(0),
            "PREVIEW_EXPIRED",
            "Morpho preview or contract deadline expired.",
        )?;
        let state = self.morpho_vault_state(&r.vault, Some(owner), None).await?;
        require_write_compatibility(&state)?;
        validate_identity(self, &b, &state, owner)?;
        validate_input(&prepared.operation, &r, &b)?;
        require(
            state.owner_shares == b.before_shares,
            "MORPHO_POSITION_CHANGED",
            "Vault shares changed since preview; refresh the candidate under existing authority.",
        )?;
        require(
            quantity(
                self.runtime
                    .call(
                        address(&b.asset.reference)?,
                        &Erc20::balanceOfCall { owner },
                        None,
                    )
                    .await?,
            ) == b.before_asset_balance,
            "MORPHO_BALANCE_CHANGED",
            "Underlying wallet balance changed since preview; refresh the candidate under existing authority.",
        )?;
        let allowance_token = if prepared.operation == "morpho.deposit" {
            address(&b.asset.reference)?
        } else {
            address(&b.vault)?
        };
        let allowance = self
            .runtime
            .call(
                allowance_token,
                &Erc20::allowanceCall {
                    owner,
                    spender: address(&b.bundles)?,
                },
                None,
            )
            .await?;
        require(
            quantity(allowance) == b.allowance_before,
            "MORPHO_ALLOWANCE_CHANGED",
            "Vault spending allowance changed since preview.",
        )?;
        validate_envelope(&prepared.operation, &b)?;
        self.runtime.revalidate(&b.transaction).await?;
        let mut fresh = prepared.clone();
        fresh.observed_at = self.clock.now().to_rfc3339();
        let current_assets = if prepared.operation == "morpho.redeem" {
            quantity(
                self.runtime
                    .call(
                        address(&b.vault)?,
                        &Vault::previewRedeemCall {
                            shares: u256(&b.shares)?,
                        },
                        Some(state.block),
                    )
                    .await?,
            )
        } else {
            b.assets.clone()
        };
        fresh.valuation = Some(
            valuation(
                self,
                &b.asset,
                &current_assets,
                b.transaction.maximum_cost()?,
            )
            .await?,
        );
        carry_attempt_valuation(prepared, &mut fresh, self.clock.now())?;
        gofin_domain::value_bounds::validate(&fresh)?;
        Ok(fresh)
    }

    pub(crate) async fn morpho_submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let b = binding(prepared)?;
        validate_envelope(&prepared.operation, &b)?;
        self.runtime
            .submit_revalidated(
                &b.transaction,
                json!({"operation":prepared.operation,"resource":b.resource}),
                journal,
                validated,
            )
            .await
    }

    pub(crate) async fn morpho_verified_position_baselines(
        &self,
        record: &OperationRecord,
    ) -> Result<Vec<PositionAnchor>> {
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Original Morpho preview is missing."))?;
        let b = binding(prepared)?;
        let mut verified = Vec::new();
        for anchor in &b.verified_baseline_anchors {
            let block = anchor.evidence["block"]
                .as_u64()
                .ok_or_else(|| GoFinError::validation("Malformed Morpho baseline block."))?;
            let state = self
                .morpho_vault_state(&b.vault, Some(address(&b.transaction.sender)?), Some(block))
                .await?;
            let observed = investment_anchor(&state, address(&b.transaction.sender)?);
            require(
                observed.resource == anchor.resource
                    && observed.position == anchor.position
                    && observed.principal == anchor.principal
                    && observed.evidence == anchor.evidence
                    && observed.position_kind == anchor.position_kind
                    && anchor.scaled_receipt.is_none(),
                "MORPHO_BASELINE_MISMATCH",
                "Historical Morpho share backing does not match the approved baseline.",
            )?;
            verified.push(anchor.clone());
        }
        Ok(verified)
    }

    pub(crate) async fn morpho_reconcile(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Original Morpho preview is missing."))?;
        let submission = record
            .submission
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Original Morpho submission is missing."))?;
        require(
            prepared.operation == record.operation && prepared.input == record.input,
            "BINDING_IDENTITY_MISMATCH",
            "Morpho record differs from its original preview.",
        )?;
        let b = binding(prepared)?;
        let request = VaultWriteInput::parse(&record.operation, &prepared.input)?;
        validate_input(&record.operation, &request, &b)?;
        validate_envelope(&record.operation, &b)?;
        require(
            submission.data["transaction"] == json!(b.transaction)
                && submission.data["metadata"]["operation"] == json!(record.operation)
                && submission.data["metadata"]["resource"] == json!(b.resource),
            "SUBMISSION_IDENTITY_MISMATCH",
            "Morpho durable submission differs from its approved envelope.",
        )?;
        let Some(receipt) = self
            .morpho_canonical_receipt(submission, &b.transaction)
            .await?
        else {
            return Ok(pending(submission));
        };
        let mut effects = vec![FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: b.resource.clone(),
            },
            amount: amount(&self.native_asset(), receipt.fee),
            category: "ethereum_gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        }];
        let mut details = json!({"original_hash":submission.reference,"block":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"success":receipt.success,"finality":self.config.finality_label(),"fee_wei":quantity(receipt.fee)});
        if receipt.success {
            let owner = address(&b.transaction.sender)?;
            let vault = address(&b.vault)?;
            let bundles = address(&b.bundles)?;
            let asset = address(&b.asset.reference)?;
            let depositing = record.operation == "morpho.deposit";
            let (assets, shares) = if depositing {
                let events = events::<Vault::Deposit>(&receipt, vault)?
                    .into_iter()
                    .filter(|e| e.sender == bundles && e.onBehalf == owner)
                    .collect::<Vec<_>>();
                let [e] = events.as_slice() else {
                    return Err(GoFinError::validation(
                        "Canonical Morpho deposit event is missing or ambiguous.",
                    ));
                };
                require(
                    quantity(e.assets) == b.assets && quantity(e.shares) >= b.minimum_shares,
                    "MORPHO_RECEIPT_MISMATCH",
                    "Deposit assets or shares violate the approved input.",
                )?;
                require(
                    transfers(&receipt, asset, owner, bundles)? == e.assets
                        && transfers(&receipt, asset, bundles, vault)? == e.assets
                        && transfers(&receipt, vault, Address::ZERO, owner)? >= e.shares,
                    "MORPHO_RECEIPT_MISMATCH",
                    "Deposit lacks its exact asset transfers and share mint.",
                )?;
                (e.assets, e.shares)
            } else {
                let events = events::<Vault::Withdraw>(&receipt, vault)?
                    .into_iter()
                    .filter(|e| e.sender == bundles && e.receiver == bundles && e.onBehalf == owner)
                    .collect::<Vec<_>>();
                let [e] = events.as_slice() else {
                    return Err(GoFinError::validation(
                        "Canonical Morpho withdrawal event is missing or ambiguous.",
                    ));
                };
                require(
                    (record.operation != "morpho.withdraw" || quantity(e.assets) == b.assets)
                        && (record.operation != "morpho.redeem" || quantity(e.shares) == b.shares)
                        && quantity(e.shares) <= b.maximum_shares,
                    "MORPHO_RECEIPT_MISMATCH",
                    "Withdrawal exceeds its fixed asset or share scope.",
                )?;
                require(
                    transfers(&receipt, asset, vault, bundles)? == e.assets
                        && transfers(&receipt, asset, bundles, owner)? == e.assets
                        && transfers(&receipt, vault, owner, Address::ZERO)? == e.shares,
                    "MORPHO_RECEIPT_MISMATCH",
                    "Withdrawal lacks its exact asset transfers and share burn.",
                )?;
                (e.assets, e.shares)
            };
            // Exact routes alone must not hide additional wallet movements in
            // this transaction. Other transactions in the inclusion block are
            // independent cash observations, not effects of this operation.
            let (wallet_debits, wallet_credits) = events::<Erc20::Transfer>(&receipt, asset)?
                .into_iter()
                .try_fold((U256::ZERO, U256::ZERO), |(debits, credits), event| {
                    Ok::<_, GoFinError>((
                        if event.from == owner {
                            add(debits, event.value)?
                        } else {
                            debits
                        },
                        if event.to == owner {
                            add(credits, event.value)?
                        } else {
                            credits
                        },
                    ))
                })?;
            require(
                if depositing {
                    wallet_debits == assets && wallet_credits.is_zero()
                } else {
                    wallet_credits == assets && wallet_debits.is_zero()
                },
                "MORPHO_RECEIPT_MISMATCH",
                "Canonical Morpho receipt contains unexpected underlying wallet transfers.",
            )?;
            let state = self
                .morpho_vault_state(&b.vault, Some(owner), Some(receipt.block_number))
                .await?;
            validate_identity(self, &b, &state, owner)?;
            let mut expected_shares = u256(&b.before_shares)?;
            for event in events::<Erc20::Transfer>(&receipt, vault)? {
                require(
                    (event.from != owner || (!depositing && event.to == Address::ZERO))
                        && (event.to != owner || event.from == Address::ZERO),
                    "MORPHO_RECEIPT_MISMATCH",
                    "Canonical Morpho receipt contains an unexpected wallet share transfer.",
                )?;
                if event.from == owner {
                    expected_shares = sub(expected_shares, event.value)?;
                }
                if event.to == owner {
                    expected_shares = add(expected_shares, event.value)?;
                }
            }
            // A different transaction can transfer shares during the same
            // block. Settle only the shares attributable to this operation;
            // importing the full observed wallet balance here would classify
            // external contributions/withdrawals as vault earnings or losses.
            let mut anchor = investment_anchor(&state, owner);
            if quantity(expected_shares) != state.owner_shares {
                let attributed_assets = self
                    .runtime
                    .call(
                        vault,
                        &Vault::previewRedeemCall {
                            shares: expected_shares,
                        },
                        Some(receipt.block_number),
                    )
                    .await?;
                anchor.evidence_version = 2;
                anchor.principal.atoms = quantity(attributed_assets);
                anchor.evidence["share_balance"] = json!(quantity(expected_shares));
                anchor.evidence["share_scope"] = json!("transaction_attributed");
                anchor.evidence["transaction_hash"] = json!(submission.reference);
                anchor.evidence["observed_wallet_share_balance"] = json!(state.owner_shares);
                anchor.evidence["observed_wallet_underlying_value"] = json!(state.owner_assets);
                anchor.evidence["external_share_difference"] = json!(signed_difference(
                    &state.owner_shares,
                    &quantity(expected_shares)
                )?);
                anchor.evidence["recovery_note"] = json!(
                    "External share movements are excluded from this operation's accounting. Later vault writes remain unavailable while observed shares differ from the journal; automatic adoption of external share transfers is unsupported."
                );
                details["share_observation"] = anchor.evidence.clone();
            }
            let wallet_assets = self
                .runtime
                .call(
                    asset,
                    &Erc20::balanceOfCall { owner },
                    Some(receipt.block_number),
                )
                .await?;
            let expected_assets = if depositing {
                sub(u256(&b.before_asset_balance)?, assets)?
            } else {
                add(u256(&b.before_asset_balance)?, assets)?
            };
            details["underlying_balance_observation"] = json!({
                "source": "canonical_rpc",
                "block": receipt.block_number,
                "before_preview": b.before_asset_balance,
                "projected_from_this_transaction": quantity(expected_assets),
                "observed_at_inclusion_block": quantity(wallet_assets),
                "difference_not_attributed_to_this_transaction": signed_difference(&quantity(wallet_assets), &quantity(expected_assets))?,
                "note": "Only this transaction's exact canonical token transfers are accounted as its cash effects; independent balance changes are excluded."
            });
            let token = if depositing { asset } else { vault };
            let allowance_after = self
                .runtime
                .call(
                    token,
                    &Erc20::allowanceCall {
                        owner,
                        spender: bundles,
                    },
                    Some(receipt.block_number),
                )
                .await?;
            require(
                allowance_after
                    == sub(
                        u256(&b.allowance_before)?,
                        if depositing { assets } else { shares },
                    )?,
                "MORPHO_ALLOWANCE_PROOF_MISMATCH",
                "Inclusion-block allowance differs from the exact reviewed flow.",
            )?;
            let delta = if depositing {
                SignedQuantity::new(quantity(assets).to_string())?
            } else {
                SignedQuantity::new(format!("-{}", quantity(assets)))?
            };
            let intermediate = if depositing {
                b.before_anchor
                    .principal
                    .atoms
                    .checked_add(&quantity(assets))?
            } else {
                // Signed arithmetic below handles withdrawn yield exceeding the prior principal.
                Quantity::zero()
            };
            let accrual = if depositing {
                signed_difference(&anchor.principal.atoms, &intermediate)?
            } else {
                signed_difference(
                    &anchor.principal.atoms.checked_add(&quantity(assets))?,
                    &b.before_anchor.principal.atoms,
                )?
            };
            let position = AccountingLocation::Position {
                resource: b.resource.clone(),
                position: position_id(&b.vault),
                position_kind: PositionKind::Investment,
            };
            let cash = AccountingLocation::Cash {
                resource: b.resource.clone(),
            };
            effects.push(FinancialEffect::OriginMovement {
                from: if depositing { cash } else { position.clone() },
                to: if depositing {
                    position
                } else {
                    AccountingLocation::Cash {
                        resource: b.resource.clone(),
                    }
                },
                debit: amount(&b.asset, assets),
                credit: amount(&b.asset, assets),
            });
            if !assets.is_zero() {
                if depositing {
                    effects.push(FinancialEffect::CashDebit {
                        resource: b.resource.clone(),
                        amount: amount(&b.asset, assets),
                    });
                } else {
                    effects.push(FinancialEffect::UsableCredit {
                        to: b.resource.clone(),
                        amount: amount(&b.asset, assets),
                    });
                }
            }
            effects.push(FinancialEffect::PositionChange {
                resource: b.resource.clone(),
                position: position_id(&b.vault),
                asset: b.asset.clone(),
                delta,
                scaled_receipt: None,
                anchor: Some(anchor),
            });
            effects.push(FinancialEffect::Accrual {
                resource: b.resource.clone(),
                position: position_id(&b.vault),
                asset: b.asset.clone(),
                position_kind: PositionKind::Investment,
                delta: accrual,
            });
            details["assets"] = json!(quantity(assets));
            details["shares"] = json!(quantity(shares));
            details["remaining_shares"] = json!(state.owner_shares);
            details["prevailing_value"] = json!(record.operation == "morpho.redeem");
        }
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(pending(submission));
        }
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: details.clone(),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: if receipt.success {
                Status::Completed
            } else {
                Status::Failed
            },
            evidence: details,
            verified: Some(verified),
        })
    }

    pub(super) async fn morpho_canonical_receipt(
        &self,
        submission: &Submission,
        tx: &FrozenTransaction,
    ) -> Result<Option<CanonicalReceipt>> {
        let Some(receipt) = self
            .runtime
            .receipt(submission, tx, self.config.confirmations)
            .await?
        else {
            return Ok(None);
        };
        if self.config.requires_rpc_finalized() {
            let header = self
                .runtime
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await?;
            if header.is_null() || rpc_u64(&header["number"])? < receipt.block_number {
                return Ok(None);
            }
            gofin_evm::hash(header["hash"].as_str().unwrap_or(""))?;
        }
        if !self.inclusion_still_canonical(&receipt).await? {
            return Ok(None);
        }
        Ok(Some(receipt))
    }
}

pub(super) fn binding(prepared: &PreparedOperation) -> Result<VaultBinding> {
    serde_json::from_value(prepared.binding.clone())
        .map_err(|_| GoFinError::validation("Malformed original Morpho vault binding."))
}
fn validate_identity(
    adapter: &EthereumAdapter,
    b: &VaultBinding,
    state: &MorphoVaultState,
    owner: Address,
) -> Result<()> {
    require(
        b.vault == state.vault
            && b.asset == state.asset
            && b.share_asset == state.shares
            && b.code_identity == state.code_identity
            && b.profile_identity == state.profile_identity
            && b.resource == resource(adapter.config.chain_id, owner)
            && b.bundles
                == lower_address(address(
                    profile::profile(adapter.config.chain_id)?.vault_bundles,
                )?),
        "MORPHO_IDENTITY_CHANGED",
        "Vault, underlying, deployment or wallet differs from the approved identity.",
    )
}
fn validate_input(operation: &str, r: &VaultWriteInput, b: &VaultBinding) -> Result<()> {
    require(
        address(&r.vault)? == address(&b.vault)?
            && r.deadline.is_none_or(|d| d == b.deadline)
            && exact(&r.max_fee_eth, 18)? >= b.transaction.maximum_cost()?,
        "BINDING_CHANGED",
        "Morpho input or fee differs from its frozen binding.",
    )?;
    let matches = match operation {
        "morpho.deposit" => {
            quantity(positive(&r.amount, b.asset.decimals)?) == b.assets
                && quantity(positive(
                    r.min_shares.as_deref().expect("validated"),
                    b.share_asset.decimals,
                )?) == b.minimum_shares
                && quantity(deposit_price_ceiling(
                    u256(&b.assets)?,
                    u256(&b.minimum_shares)?,
                )?) == b.maximum_share_price_e27
        }
        "morpho.withdraw" => {
            quantity(positive(&r.amount, b.asset.decimals)?) == b.assets
                && quantity(positive(
                    r.max_shares.as_deref().expect("validated"),
                    b.share_asset.decimals,
                )?) == b.maximum_shares
        }
        _ => {
            (if r.amount == "all" {
                b.all && b.shares == b.before_shares
            } else {
                !b.all && quantity(positive(&r.amount, b.share_asset.decimals)?) == b.shares
            }) && r
                .max_shares
                .as_deref()
                .map(|v| positive(v, b.share_asset.decimals).map(quantity))
                .transpose()?
                .unwrap_or(b.shares.clone())
                == b.maximum_shares
        }
    };
    require(
        matches,
        "BINDING_CHANGED",
        "Morpho finite asset/share bounds differ from approved input.",
    )
}
fn validate_envelope(operation: &str, b: &VaultBinding) -> Result<()> {
    require(
        b.transaction.to == b.bundles
            && b.transaction.value.is_zero()
            && b.transaction.chain_id
                == b.asset
                    .network
                    .strip_prefix("ethereum:")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            && b.resource == resource(b.transaction.chain_id, address(&b.transaction.sender)?)
            && rpc_bytes(&json!(b.transaction.calldata))? == calldata(operation, b)?,
        "BINDING_CHANGED",
        "Frozen Morpho calldata is not the exact zero-referral, empty-permit standard flow.",
    )
}
pub(super) fn pending(submission: &Submission) -> Reconciliation {
    Reconciliation {
        status: Status::Pending,
        evidence: json!({"original_hash":submission.reference,"canonical_confirmed":false}),
        verified: None,
    }
}
pub(super) fn events<E: SolEvent>(receipt: &CanonicalReceipt, emitter: Address) -> Result<Vec<E>> {
    let mut result = Vec::new();
    let logs = receipt.receipt["logs"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Canonical receipt has no logs."))?;
    for log in logs {
        if address(log["address"].as_str().unwrap_or(""))? != emitter {
            continue;
        }
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| GoFinError::validation("Malformed event topics."))?
            .iter()
            .map(|v| gofin_evm::hash(v.as_str().unwrap_or("")))
            .collect::<Result<Vec<_>>>()?;
        if topics.first() != Some(&E::SIGNATURE_HASH) {
            continue;
        }
        require(
            log["removed"].as_bool() != Some(true)
                && log["transactionHash"] == receipt.receipt["transactionHash"]
                && log["blockHash"] == receipt.receipt["blockHash"]
                && log["blockNumber"] == receipt.receipt["blockNumber"],
            "RECEIPT_LOG_IDENTITY",
            "Morpho event is not in the original canonical transaction.",
        )?;
        result.push(
            E::decode_raw_log_validate(topics.iter().copied(), &rpc_bytes(&log["data"])?)
                .map_err(|_| GoFinError::validation("Malformed reviewed Morpho event."))?,
        );
    }
    Ok(result)
}
pub(super) fn transfers(
    receipt: &CanonicalReceipt,
    token: Address,
    from: Address,
    to: Address,
) -> Result<U256> {
    events::<Erc20::Transfer>(receipt, token)?
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .try_fold(U256::ZERO, |sum, e| add(sum, e.value))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    fn fixture_binding(operation: &str) -> VaultBinding {
        let asset = AssetId {
            network: "ethereum:1".into(),
            reference: lower_address(addr(2)),
            decimals: 6,
        };
        let share_asset = AssetId {
            network: "ethereum:1".into(),
            reference: lower_address(addr(3)),
            decimals: 18,
        };
        let resource = resource(1, addr(1));
        let mut b = VaultBinding {
            transaction: FrozenTransaction {
                chain_id: 1,
                sender: lower_address(addr(1)),
                to: lower_address(addr(4)),
                value: Quantity::zero(),
                calldata: "0x".into(),
                nonce: 8,
                gas: 100_000,
                max_fee_per_gas: Quantity::new("10").unwrap(),
                max_priority_fee_per_gas: Quantity::new("1").unwrap(),
                rollup_fee: None,
            },
            resource: resource.clone(),
            vault: share_asset.reference.clone(),
            asset: asset.clone(),
            share_asset,
            bundles: lower_address(addr(4)),
            code_identity: "reviewed".into(),
            profile_identity: "profile".into(),
            assets: Quantity::new("1000000").unwrap(),
            shares: Quantity::new("2000000000000000000").unwrap(),
            minimum_shares: Quantity::new("1900000000000000000").unwrap(),
            maximum_shares: Quantity::new("2100000000000000000").unwrap(),
            maximum_share_price_e27: Quantity::new("526315789473685").unwrap(),
            allowance_before: Quantity::new("2100000000000000000").unwrap(),
            deadline: 2000000000,
            before_shares: Quantity::new("2000000000000000000").unwrap(),
            before_asset_balance: Quantity::new("10000000").unwrap(),
            before_anchor: PositionAnchor {
                resource,
                position: position_id(&lower_address(addr(3))),
                position_kind: PositionKind::Investment,
                principal: amount(&asset, U256::from(1_000_000)),
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({}),
                observed_at: now(),
            },
            all: false,
            verified_baseline_anchors: vec![],
            value_bounds: None,
            boundary_authorization: None,
            financial_plan: None,
        };
        b.transaction.calldata = gofin_evm::hex_bytes(&calldata(operation, &b).unwrap());
        b
    }
    fn receipt_with<E: SolEvent>(emitter: Address, event: E) -> CanonicalReceipt {
        let hash = B256::repeat_byte(0xaa);
        let block_hash = B256::repeat_byte(0xbb);
        let data = event.encode_log_data();
        let log = json!({"address":lower_address(emitter),"topics":data.topics().iter().map(|t|format!("{t:#x}")).collect::<Vec<_>>(),"data":gofin_evm::hex_bytes(&data.data),"removed":false,"transactionHash":format!("{hash:#x}"),"blockHash":format!("{block_hash:#x}"),"blockNumber":"0x14"});
        CanonicalReceipt {
            hash,
            transaction_sender: lower_address(addr(1)),
            block_hash,
            block_number: 20,
            confirmations: 2,
            success: true,
            fee: U256::from(10),
            receipt: json!({"transactionHash":format!("{hash:#x}"),"blockHash":format!("{block_hash:#x}"),"blockNumber":"0x14","logs":[log]}),
        }
    }
    #[test]
    fn share_floor_rejects_the_preceding_atom() {
        for assets in 1..100u64 {
            for shares in 1..100u64 {
                let ray = pow10(27).unwrap();
                let ceiling =
                    deposit_price_ceiling(U256::from(assets), U256::from(shares)).unwrap();
                assert!(
                    (U256::from(assets) * ray + U256::from(shares - 1)) / U256::from(shares)
                        <= ceiling
                );
                if shares > 1 {
                    let bad_price = (U256::from(assets) * ray + U256::from(shares - 2))
                        / U256::from(shares - 1);
                    assert!(bad_price > ceiling);
                }
            }
        }
    }
    #[test]
    fn nonrepresentable_share_atom_floor_fails_closed() {
        assert!(deposit_price_ceiling(U256::from(1), pow10(28).unwrap()).is_err());
        assert!(deposit_price_ceiling(U256::MAX, U256::from(1)).is_err());
    }
    #[test]
    fn ordinary_usdc_deposit_enforces_share_floor_despite_ray_quantization() {
        let assets = U256::from(1_000_000);
        let minimum = U256::from(990_000_000_000_000_000u64);
        let quoted = pow10(18).unwrap();
        let price = deposit_price_ceiling(assets, minimum).unwrap();
        let numerator = assets * pow10(27).unwrap();
        assert!(numerator / quoted <= price);
        assert!((numerator + minimum - U256::from(2)) / (minimum - U256::from(1)) > price);
        let accepted = (numerator + price - U256::from(1)) / price;
        assert!(accepted >= minimum && accepted <= quoted);
    }
    #[test]
    fn envelopes_have_only_reviewed_standard_flow_and_no_referral_or_permits() {
        for operation in ["morpho.deposit", "morpho.withdraw", "morpho.redeem"] {
            let mut b = fixture_binding(operation);
            validate_envelope(operation, &b).unwrap();
            let data = calldata(operation, &b).unwrap();
            if operation == "morpho.deposit" {
                let call =
                    VaultBundles::vaultBundlesV1DepositCall::abi_decode_validate(&data).unwrap();
                assert_eq!(call.vault, addr(3));
                assert_eq!(call.assets, U256::from(1_000_000));
                assert_eq!(call.assetPermit.kind, 0);
                assert!(call.assetPermit.data.is_empty());
                assert!(call.referralFeePct.is_zero());
                assert_eq!(call.referralFeeRecipient, Address::ZERO);
            } else {
                let call =
                    VaultBundles::vaultBundlesV1WithdrawCall::abi_decode_validate(&data).unwrap();
                assert_eq!(call.vault, addr(3));
                assert_eq!(
                    call.assets,
                    if operation == "morpho.withdraw" {
                        U256::from(1_000_000)
                    } else {
                        U256::ZERO
                    }
                );
                assert_eq!(
                    call.shares,
                    if operation == "morpho.redeem" {
                        u256(&b.shares).unwrap()
                    } else {
                        U256::ZERO
                    }
                );
                assert!(call.sharesPermit.value.is_zero() && call.sharesPermit.deadline.is_zero());
                assert_eq!(call.sharesPermit.v, 0);
                assert_eq!(call.sharesPermit.r, B256::ZERO);
                assert_eq!(call.sharesPermit.s, B256::ZERO);
                assert!(call.referralFeePct.is_zero());
                assert_eq!(call.referralFeeRecipient, Address::ZERO);
            }
            b.transaction.value = Quantity::new("1").unwrap();
            assert!(validate_envelope(operation, &b).is_err());
            b.transaction.value = Quantity::zero();
            b.transaction.to = lower_address(addr(9));
            assert!(validate_envelope(operation, &b).is_err());
            b.transaction.to = b.bundles.clone();
            b.deadline += 1;
            assert!(validate_envelope(operation, &b).is_err());
        }
    }
    #[test]
    fn receipt_transfer_requires_exact_token_route_and_canonical_identity() {
        let original = receipt_with(
            addr(2),
            Erc20::Transfer {
                from: addr(1),
                to: addr(4),
                value: U256::from(123),
            },
        );
        assert_eq!(
            transfers(&original, addr(2), addr(1), addr(4)).unwrap(),
            U256::from(123)
        );
        assert_eq!(
            transfers(&original, addr(3), addr(1), addr(4)).unwrap(),
            U256::ZERO
        );
        assert_eq!(
            transfers(&original, addr(2), addr(4), addr(1)).unwrap(),
            U256::ZERO
        );
        for (field, value) in [
            ("transactionHash", json!(format!("{:#x}", B256::ZERO))),
            ("blockHash", json!(format!("{:#x}", B256::ZERO))),
            ("blockNumber", json!("0x15")),
            ("removed", json!(true)),
        ] {
            let mut changed = original.clone();
            changed.receipt["logs"][0][field] = value;
            assert!(transfers(&changed, addr(2), addr(1), addr(4)).is_err());
        }
    }
    #[test]
    fn human_approval_discloses_exact_frozen_all_shares_and_unprotected_payout() {
        let mut b = fixture_binding("morpho.redeem");
        b.all = true;
        let r=VaultWriteInput::parse("morpho.redeem",&json!({"wallet":"test","network":"ethereum","chain_id":1,"vault":b.vault,"request_id":"redeem","max_fee_eth":"0.001","amount":"all","max_shares":"2.1","accept_prevailing_value":true})).unwrap();
        let summary = write_summary("morpho.redeem", &r, &b);
        assert!(
            summary.contains("all selected 2 vault shares")
                && summary.contains("No minimum asset payout is guaranteed")
                && summary.contains("may be zero")
        );
    }
    #[test]
    fn redemption_requires_explicit_semantics_and_a_finite_all_cap() {
        let mut input = json!({"wallet":"test","network":"ethereum","chain_id":8453,"vault":"0x1111111111111111111111111111111111111111","request_id":"redeem","max_fee_eth":"0.001","amount":"all"});
        assert!(VaultWriteInput::parse("morpho.redeem", &input).is_err());
        input["accept_prevailing_value"] = json!(true);
        assert!(VaultWriteInput::parse("morpho.redeem", &input).is_err());
        input["max_shares"] = json!("10");
        assert!(VaultWriteInput::parse("morpho.redeem", &input).is_ok());
        input["min_shares"] = json!("1");
        assert!(VaultWriteInput::parse("morpho.redeem", &input).is_err());
    }
    #[test]
    fn exact_backing_changes_preserve_loss_and_gain() {
        assert_eq!(
            signed_difference(&Quantity::new("7").unwrap(), &Quantity::new("9").unwrap())
                .unwrap()
                .as_str(),
            "-2"
        );
        assert_eq!(
            signed_difference(&Quantity::new("11").unwrap(), &Quantity::new("9").unwrap())
                .unwrap()
                .as_str(),
            "2"
        );
    }

    #[test]
    fn baseline_uses_captured_preview_observation_without_restamping() {
        let b = fixture_binding("morpho.deposit");
        let state:MorphoVaultState=serde_json::from_value(json!({"vault":b.vault,"asset":b.asset,"shares":b.share_asset,"block":20,"block_hash":format!("{:#x}",B256::repeat_byte(0xbb)),"block_timestamp":1_700_000_000,"total_assets":"1000000","total_supply":"2000000000000000000","virtual_shares":"1000000000000","owner_shares":b.before_shares,"owner_assets":"1000000","idle_assets":"1000000","liquidity_adapter":lower_address(Address::ZERO),"code_identity":"reviewed","profile_identity":"profile","gates":{},"adapters":[],"write_compatibility":{"supported":true},"capacity":{},"fees":{}})).unwrap();
        let preview_observed = "2023-11-14T22:13:20+00:00".to_owned();
        let anchor = investment_anchor_observed(&state, addr(1), preview_observed.clone());
        assert_eq!(anchor.observed_at, preview_observed);
        assert_eq!(anchor.principal.atoms, Quantity::new("1000000").unwrap());
        assert_eq!(anchor.evidence["share_balance"], json!(b.before_shares));
    }
}
