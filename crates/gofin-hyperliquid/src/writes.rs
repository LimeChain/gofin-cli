//! Exact preparation and immutable submission binding for Hyperliquid actions.
use crate::{
    account_actions::AccountPreparation,
    account_mode::{AccountMode, AccountModeState, CashWallet},
    identity::cloid,
    profile::*,
    read_profile::{input, native_error},
    types::*,
};
use gofin_domain::*;
use gofin_exchange::ccxt::{
    HyperliquidAction as Action, HyperliquidActionEnvelope as Envelope, HyperliquidBuilderFee,
    HyperliquidInfoRequest as Info, HyperliquidOrder as Order,
    HyperliquidOrderGrouping as Grouping, HyperliquidOrderType as OrderKind,
    HyperliquidTimeInForce as Tif, HyperliquidTpsl as Tpsl,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const SUBMISSION_KIND: &str = "hyperliquid-action-v1";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderScope {
    pub input: LimitOrderInput,
    pub market: NativeMarket,
    pub cloid: String,
    pub min_price: String,
    pub max_price: String,
    pub position_before: String,
    /// Exact native balance/margin capacity used by this preparation. It is
    /// evidence only; the financial intent continues to reserve costs rather
    /// than treating the order notional as a cash debit.
    #[serde(default)]
    pub capacity_before: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionBinding {
    pub account: HyperliquidAccount,
    /// Native mode/signer-role state frozen at preparation. `None` is retained
    /// only for deserializing legacy unresolved records.
    #[serde(default)]
    pub account_mode: Option<AccountModeState>,
    pub action: Action,
    pub orders: Vec<OrderScope>,
    pub authority_before: Option<Value>,
    #[serde(
        default,
        rename = "__fin_replacement",
        skip_serializing_if = "Option::is_none"
    )]
    pub replacement: Option<ReplacementBinding>,
    #[serde(
        default,
        rename = "__fin_recovery",
        skip_serializing_if = "Option::is_none"
    )]
    pub recovery: Option<RecoveryBinding>,
    /// Shared GoFin metadata such as the stable approved USD ceiling. This is
    /// flattened so typed native fields remain closed while reserved GoFin
    /// fields survive exact binding serialization and revalidation.
    #[serde(flatten, default)]
    pub metadata: serde_json::Map<String, Value>,
}

impl ActionBinding {
    pub(crate) fn decode(value: &Value) -> Result<Self> {
        let binding: Self = serde_json::from_value(value.clone())
            .map_err(|_| native_error("The typed action binding is invalid."))?;
        if binding
            .metadata
            .keys()
            .any(|key| !(key.starts_with("_fin_") || key.starts_with("__fin_")))
        {
            return Err(native_error(
                "The action binding contains an unrecognized non-GoFin field.",
            ));
        }
        Ok(binding)
    }
}

const INVALIDATE_NONCE_OPERATION: &str = "hyperliquid.account.invalidate_nonce";
const MAX_NONCE_AGE_MS: u64 = 2 * 24 * 60 * 60 * 1_000;
const MAX_NONCE_FUTURE_MS: u64 = 24 * 60 * 60 * 1_000;

pub(crate) fn nonce_is_current(nonce_ms: u64, now_ms: u64) -> bool {
    nonce_ms > now_ms.saturating_sub(MAX_NONCE_AGE_MS)
        && nonce_ms < now_ms.saturating_add(MAX_NONCE_FUTURE_MS)
}

/// Reconstruct the exact source submission from the durable operation record.
/// This intentionally supports only a standard-owner USDC class transfer.
pub(crate) fn original_class_transfer(
    account: &HyperliquidAccount,
    record: &OperationRecord,
) -> Result<Envelope> {
    if account.execution_scope != HyperliquidExecutionScope::Owner
        || !account.signing_enabled
        || account.credential_ref.is_none()
        || record.operation != "hyperliquid.account.transfer"
        || !matches!(
            record.status,
            Status::Unknown | Status::Pending | Status::Submitted
        )
        || record.effects.is_some()
    {
        return Err(GoFinError::new(
            "HYPERLIQUID_NONCE_RECOVERY_UNSUPPORTED",
            "preview",
            "Nonce recovery is limited to an unresolved owner-signed USDC class transfer.",
            "Reconcile other operations through their original venue evidence; do not invalidate their nonce.",
        ));
    }
    let input: TransferInput = serde_json::from_value(record.input.clone())
        .map_err(|_| GoFinError::validation("The original transfer input is unavailable."))?;
    if input.account != account.name
        || input.asset != "USDC"
        || !matches!(
            input.destination,
            TransferDestination::Spot | TransferDestination::Perpetual
        )
        || input.destination_address.is_some()
        || input.subaccount_address.is_some()
        || input.subaccount_direction.is_some()
    {
        return Err(GoFinError::validation(
            "Nonce recovery requires the original same-owner spot/perpetual USDC class transfer.",
        ));
    }
    let preview = record
        .preview
        .as_ref()
        .ok_or_else(|| GoFinError::validation("The original prepared transfer is unavailable."))?;
    if preview.operation != record.operation || preview.input != record.input {
        return Err(GoFinError::validation(
            "The original transfer input does not match its persisted prepared operation.",
        ));
    }
    let binding = ActionBinding::decode(&preview.binding)?;
    if binding.account != *account
        || binding.recovery.is_some()
        || binding.replacement.is_some()
        || !binding.orders.is_empty()
        || binding.account_mode.as_ref().is_none_or(|mode| {
            mode.owner != account.owner
                || mode.execution_address != account.owner
                || mode.mode != AccountMode::Separate
                || mode.dex_abstraction == Some(true)
        })
    {
        return Err(GoFinError::validation(
            "The original action is not bound to the current protected master account in standard mode.",
        ));
    }
    let submission = record
        .submission
        .as_ref()
        .ok_or_else(|| GoFinError::validation("The original native submission is unavailable."))?;
    if submission.kind != SUBMISSION_KIND
        || submission.data["operation"] != record.operation
        || submission.data["account"] != account.name
        || submission.data["owner"] != account.owner
        || submission.data["vault_address"].as_str().is_some()
    {
        return Err(GoFinError::validation(
            "The original submission does not match the protected owner account.",
        ));
    }
    let envelope: Envelope = serde_json::from_value(submission.data["envelope"].clone())
        .map_err(|_| GoFinError::validation("The original action nonce is unavailable."))?;
    let Action::UsdClassTransfer {
        amount_usdc,
        to_perp,
        nonce,
    } = &envelope.action
    else {
        return Err(GoFinError::validation(
            "Only native class-transfer actions can use nonce recovery.",
        ));
    };
    let input_to_perp = matches!(input.destination, TransferDestination::Perpetual);
    let exact_amount = positive(&input.amount)?;
    let approved_amount = positive(&input.max_amount_usdc)?;
    let approved_fee = q(&input.max_fee_usdc)?;
    let (from, to) = if input_to_perp {
        (
            account.resource("spot", None),
            account.resource("perpetual", None),
        )
    } else {
        (
            account.resource("perpetual", None),
            account.resource("spot", None),
        )
    };
    let intent_matches = preview.intent.debits.as_slice()
        == [ResourceAmount {
            resource: from.clone(),
            amount: Amount {
                asset: usdc(),
                atoms: exact_amount.clone(),
            },
        }]
        && preview.intent.maximum_costs.as_slice()
            == [CostBound {
                location: AccountingLocation::Cash {
                    resource: from.clone(),
                },
                amount: Amount {
                    asset: usdc(),
                    atoms: approved_fee,
                },
                treatment: CostTreatment::AdditionalDebit,
            }]
        && preview.intent.expected.as_slice()
            == [EffectConstraint::Transfer {
                from: from.clone(),
                to: to.clone(),
                debit_asset: usdc(),
                credit_asset: usdc(),
                maximum_debit: exact_amount.clone(),
                minimum_credit: exact_amount.clone(),
            }]
        && preview
            .intent
            .locks
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            == [from.clone(), to.clone()]
                .into_iter()
                .collect::<BTreeSet<_>>();
    if *nonce != envelope.nonce_ms
        || *to_perp != input_to_perp
        || q(amount_usdc)? != exact_amount
        || approved_amount < exact_amount
        || !intent_matches
        || envelope.vault_address.is_some()
    {
        return Err(GoFinError::validation(
            "The original class-transfer action, direction, amount or immutable nonce does not match its saved intent.",
        ));
    }
    let wire = envelope
        .action_wire_json()
        .map_err(|_| GoFinError::validation("The original native action wire is invalid."))?;
    let scoped = submission.data.get("execution_address").is_some();
    let expected_reference = if scoped {
        binding_hash(
            &json!({"version":1,"owner":account.owner,"execution_address":account.execution_address(),"vault_address":account.vault_address(),"operation":record.operation,"wire":wire}),
        )?
    } else {
        binding_hash(
            &json!({"version":1,"owner":account.owner,"operation":record.operation,"wire":wire}),
        )?
    };
    if submission.reference != expected_reference
        || submission.data["wire"] != wire
        || (scoped && submission.data["execution_address"].as_str() != Some(account.owner.as_str()))
    {
        return Err(GoFinError::validation(
            "The original native nonce and wire do not match the immutable submission reference.",
        ));
    }
    Ok(envelope)
}
pub struct ActionRequest {
    pub account: HyperliquidAccount,
    pub envelope: Envelope,
    pub submission: Submission,
}

pub(crate) fn q(text: &str) -> Result<Quantity> {
    Quantity::from_decimal(text, 36)
}
pub(crate) fn positive(text: &str) -> Result<Quantity> {
    let value = q(text)?;
    if value.is_zero() {
        return Err(GoFinError::validation(
            "A positive exact decimal quantity is required.",
        ));
    }
    Ok(value)
}
pub(crate) fn product(a: &Quantity, b: &Quantity) -> Result<Quantity> {
    let scale = Quantity::new(format!("1{}", "0".repeat(36)))?;
    let numerator = a.integer() * b.integer();
    if &numerator % scale.integer() != num_bigint::BigUint::from(0u8) {
        return Err(GoFinError::validation(
            "The exact product exceeds the exchange accounting precision.",
        ));
    }
    a.mul_div_floor(b, &scale)
}

fn spot_free_balance(
    state: &Value,
    token_index: Option<u32>,
    coin: &str,
) -> Result<(Quantity, Quantity, Quantity)> {
    let index =
        token_index.ok_or_else(|| native_error("Exact spot token index is unavailable."))?;
    let balances = state["balances"]
        .as_array()
        .ok_or_else(|| native_error("Spot balances are missing from the native capacity read."))?;
    let matching: Vec<_> = balances
        .iter()
        .filter(|balance| {
            balance["token"].as_u64() == Some(u64::from(index))
                || balance["coin"].as_str() == Some(coin)
        })
        .collect();
    if matching.len() > 1
        || matching.iter().any(|balance| {
            balance["token"].as_u64() != Some(u64::from(index))
                || balance["coin"].as_str() != Some(coin)
        })
    {
        return Err(native_error(
            "Native spot capacity has duplicate or conflicting token identity.",
        ));
    }
    let Some(balance) = matching.first() else {
        return Ok((Quantity::zero(), Quantity::zero(), Quantity::zero()));
    };
    let total = q(balance["total"]
        .as_str()
        .ok_or_else(|| native_error("Native spot total must be an exact decimal string."))?)?;
    let hold = q(balance["hold"]
        .as_str()
        .ok_or_else(|| native_error("Native spot hold must be an exact decimal string."))?)?;
    let free = total
        .checked_sub(&hold)
        .map_err(|_| native_error("Native spot hold exceeds the total token balance."))?;
    Ok((free, total, hold))
}

fn capacity_decimal(value: &Value, field: &str) -> Result<Quantity> {
    q(value
        .as_str()
        .ok_or_else(|| native_error(&format!("Native {field} must be an exact decimal string.")))?)
}
fn signed_decimal(text: &str) -> Result<SignedQuantity> {
    let value = q(text.strip_prefix('-').unwrap_or(text))?;
    Ok(if text.starts_with('-') {
        SignedQuantity::negative(&value)
    } else {
        SignedQuantity::positive(&value)
    })
}
fn request_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b))
    {
        return Err(GoFinError::validation(
            "Request ID must contain 1–128 letters, digits, hyphens, underscores or colons.",
        ));
    }
    Ok(())
}
fn cost(resource: &ResourceId, reference: &str, maximum: Quantity) -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: asset(reference),
            atoms: maximum,
        },
        treatment: CostTreatment::AdditionalDebit,
    }
}
fn frozen_market(mut market: NativeMarket) -> NativeMarket {
    market.context = None;
    market.observed_at_ms = 0;
    market
}

// Implemented from Ogi's focused identity-helper proposal, with exact native
// owner/coin matching preserved and no identity inference.
pub(crate) fn trading_state_identity_matches(raw: &Value, owner: &str, coin: &str) -> bool {
    raw.get("user")
        .and_then(Value::as_str)
        .is_some_and(|user| user.eq_ignore_ascii_case(owner))
        && raw.get("coin").and_then(Value::as_str) == Some(coin)
}

pub(crate) fn leverage_state(state: &Value, owner: &str, coin: &str) -> Result<String> {
    if !trading_state_identity_matches(state, owner, coin) {
        return Err(native_error(
            "Trading-state response does not identify the exact account and market.",
        ));
    }
    let leverage = &state["leverage"];
    let kind = leverage["type"]
        .as_str()
        .filter(|kind| matches!(*kind, "cross" | "isolated"))
        .ok_or_else(|| native_error("Trading-state margin mode is missing."))?;
    let value = leverage["value"]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| native_error("Trading-state leverage is missing."))?;
    Ok(format!("{kind}:{value}"))
}

fn combine_derivative_expectations(intent: &mut FinancialIntent) -> Result<()> {
    let mut combined: Vec<EffectConstraint> = Vec::new();
    for constraint in std::mem::take(&mut intent.expected) {
        if let EffectConstraint::DerivativePosition {
            resource,
            position,
            asset,
            position_kind,
            minimum_delta,
            maximum_delta,
        } = &constraint
        {
            let prior = combined.iter_mut().find(|prior| matches!(prior,
                EffectConstraint::DerivativePosition { resource:r, position:p, asset:a, position_kind:k, .. }
                if r == resource && p == position && a == asset && k == position_kind));
            if let Some(EffectConstraint::DerivativePosition {
                minimum_delta: minimum,
                maximum_delta: maximum,
                ..
            }) = prior
            {
                *minimum =
                    SignedQuantity::new((minimum.integer() + minimum_delta.integer()).to_string())?;
                *maximum =
                    SignedQuantity::new((maximum.integer() + maximum_delta.integer()).to_string())?;
                continue;
            }
        }
        combined.push(constraint);
    }
    intent.expected = combined;
    Ok(())
}

fn action_summary(binding: &ActionBinding) -> String {
    let mut summary = match &binding.action {
        Action::Noop {} => format!(
            "Consume the native nonce for unresolved transfer {}",
            binding
                .recovery
                .as_ref()
                .map(|recovery| recovery.parent_operation_id.as_str())
                .unwrap_or("unknown operation")
        ),
        Action::Order { .. } | Action::Amend { .. } => binding
            .orders
            .iter()
            .map(|scope| {
                format!(
                    "{} {} {} ({:?}, price {}–{} USDC{})",
                    if matches!(scope.input.side, OrderSide::Buy) {
                        "Buy"
                    } else {
                        "Sell"
                    },
                    scope.input.size,
                    scope.market.symbol,
                    scope.input.order_type,
                    scope.min_price,
                    scope.max_price,
                    if scope.input.reduce_only {
                        ", reduce only"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        Action::Cancel { cancels } => format!("Cancel {} reviewed order(s)", cancels.len()),
        Action::UpdateLeverage {
            leverage,
            is_cross,
            asset,
        } => format!(
            "Set market {asset} to {leverage}× {} margin",
            if *is_cross { "cross" } else { "isolated" }
        ),
        Action::UpdateIsolatedMargin { amount_usdc, asset } => {
            format!("Adjust market {asset} isolated margin by {amount_usdc} USDC")
        }
        Action::TwapOrder {
            is_buy,
            size,
            duration_minutes,
            reduce_only,
            ..
        } => format!(
            "Native TWAP {} {size} {} over {duration_minutes} minutes{}; venue-controlled slice prices",
            if *is_buy { "buy" } else { "sell" },
            binding
                .authority_before
                .as_ref()
                .and_then(|value| value["twap"]["symbol"].as_str())
                .unwrap_or("units"),
            if *reduce_only { ", reduce only" } else { "" }
        ),
        Action::TwapCancel { twap_id, .. } => format!("Cancel native TWAP {twap_id}"),
        Action::ScheduledCancel { time_ms } => time_ms.map_or_else(
            || "Remove scheduled account-wide order cancellation".into(),
            |time| format!("Schedule account-wide order cancellation at {time} ms UTC"),
        ),
        Action::UsdClassTransfer {
            amount_usdc,
            to_perp,
            ..
        } => format!(
            "Transfer {amount_usdc} USDC {}",
            if *to_perp {
                "from spot to perpetual collateral"
            } else {
                "from perpetual collateral to spot"
            }
        ),
        Action::UsdSend {
            destination,
            amount_usdc,
            ..
        } => format!("Send {amount_usdc} USDC to Hyperliquid account {destination}"),
        Action::Withdraw {
            destination,
            amount_usdc,
            ..
        } => format!("Withdraw {amount_usdc} gross USDC to Arbitrum destination {destination}"),
        Action::VaultTransfer {
            vault_address,
            is_deposit,
            amount_usd_micro,
        } => format!(
            "{} {} USDC {} vault {vault_address}",
            if *is_deposit { "Deposit" } else { "Redeem" },
            Quantity::new(amount_usd_micro.unsigned_abs().to_string())
                .expect("integer")
                .to_decimal(6),
            if *is_deposit { "into" } else { "from" }
        ),
        Action::CreateVault {
            name,
            initial_usd_micro,
            ..
        } => format!(
            "Create vault {name} with {} USDC initial allocation",
            Quantity::new(initial_usd_micro.to_string())
                .expect("integer")
                .to_decimal(6)
        ),
        Action::SubAccountTransfer {
            sub_account_user,
            is_deposit,
            usd_micro,
        } => format!(
            "Transfer {} USDC {} subaccount {sub_account_user}",
            Quantity::new(usd_micro.to_string())
                .expect("integer")
                .to_decimal(6),
            if *is_deposit { "to" } else { "from" }
        ),
        Action::SubAccountSpotTransfer {
            sub_account_user,
            is_deposit,
            token,
            amount,
        } => format!(
            "Transfer {amount} of token {token} {} subaccount {sub_account_user}",
            if *is_deposit { "to" } else { "from" }
        ),
        Action::CreateSubAccount { name, .. } => format!("Create subaccount {name}"),
        Action::ReserveRequestWeight {
            weight,
            destination,
        } => {
            let cost = request_weight_cost(*weight);
            match destination {
                Some(destination) => format!(
                    "Purchase {weight} native request-weight units for {destination}, costing {cost} USDC"
                ),
                None => {
                    format!("Purchase {weight} native request-weight units, costing {cost} USDC")
                }
            }
        }
        Action::SetRef { code } => format!("Set referral code {code}"),
        Action::ApproveBuilderFee {
            builder,
            max_fee_rate,
            ..
        } => format!("Authorize builder {builder} fee rate up to {max_fee_rate}"),
        Action::UserAbstraction {
            user,
            abstraction,
            agent,
        } => format!(
            "Set {} abstraction for {user} to {abstraction:?}",
            if *agent { "agent" } else { "user" }
        ),
        Action::UserDexAbstraction {
            user,
            enabled,
            agent,
        } => format!(
            "{} {} DEX abstraction for {user}",
            if *enabled { "Enable" } else { "Disable" },
            if *agent { "agent" } else { "user" }
        ),
    };
    if let Action::Order {
        builder_fee: Some(builder),
        ..
    } = &binding.action
    {
        summary.push_str(&format!(
            "; builder {} at {}%",
            builder.address,
            Quantity::new(builder.fee_tenths_bps.to_string())
                .expect("integer")
                .to_decimal(3)
        ));
    }
    summary
}

fn request_weight_cost(weight: u32) -> String {
    let ten_thousandths = u64::from(weight) * 5;
    let value = format!(
        "{}.{:04}",
        ten_thousandths / 10_000,
        ten_thousandths % 10_000
    );
    value.trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl HyperliquidProfile {
    /// Unified spot balances share collateral with every perpetual DEX. The
    /// native spot `hold` does not include isolated margin or resting perp
    /// orders, so unknown cross-market commitments fail closed for spot spends.
    async fn ensure_unified_spot_cash_unencumbered(
        &self,
        account: &HyperliquidAccount,
    ) -> Result<()> {
        let raw_dexes = self.transport.info(Info::PerpDexs).await?;
        let dexes = raw_dexes
            .as_array()
            .filter(|rows| rows.first().is_some_and(Value::is_null))
            .ok_or_else(|| {
                native_error("Perpetual DEX coverage is unavailable for unified cash.")
            })?;
        for (index, dex) in dexes.iter().enumerate() {
            let name = if index == 0 {
                None
            } else {
                Some(
                    dex["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| native_error("Perpetual DEX identity is incomplete."))?,
                )
            };
            let state = match name {
                Some(dex) => {
                    self.transport
                        .info(Info::ClearinghouseStateForDex {
                            owner: account.execution_address().to_owned(),
                            dex: dex.to_owned(),
                        })
                        .await?
                }
                None => {
                    self.transport
                        .info(Info::ClearinghouseState {
                            owner: account.execution_address().to_owned(),
                        })
                        .await?
                }
            };
            let positions = state["assetPositions"]
                .as_array()
                .ok_or_else(|| native_error("Unified perpetual positions are incomplete."))?;
            for row in positions {
                let position = &row["position"];
                let size = position["szi"]
                    .as_str()
                    .ok_or_else(|| native_error("Unified position size is missing."))?;
                let margin = position["marginUsed"]
                    .as_str()
                    .ok_or_else(|| native_error("Unified position margin is missing."))?;
                let margin = q(margin)?;
                let size_is_zero = signed_decimal(size)?.is_zero();
                let leverage_type = position["leverage"]["type"]
                    .as_str()
                    .ok_or_else(|| native_error("Unified position margin type is missing."))?;
                if leverage_type == "isolated" && (!size_is_zero || !margin.is_zero()) {
                    return Err(GoFinError::new(
                        "UNIFIED_CASH_CAPACITY_UNAVAILABLE",
                        "preflight",
                        "Unified spot funds also collateralize an isolated perpetual position; its shared cash cannot be safely reduced by this spot order.",
                        "Close or reduce isolated positions in your own Terminal before placing a spot order.",
                    ));
                }
                if !matches!(leverage_type, "cross" | "isolated") {
                    return Err(native_error("Unified position margin type is unknown."));
                }
                if !size_is_zero {
                    return Err(GoFinError::new(
                        "UNIFIED_CASH_CAPACITY_UNAVAILABLE",
                        "preflight",
                        "A live perpetual position shares the unified spot collateral pool; free spot balance alone cannot prove spendable cash.",
                        "Reduce or close perpetual exposure before placing a spot order.",
                    ));
                }
            }
            let orders = match name {
                Some(dex) => {
                    self.transport
                        .info(Info::FrontendOpenOrdersForDex {
                            owner: account.execution_address().to_owned(),
                            dex: dex.to_owned(),
                        })
                        .await?
                }
                None => {
                    self.transport
                        .info(Info::FrontendOpenOrders {
                            owner: account.execution_address().to_owned(),
                        })
                        .await?
                }
            };
            let orders = orders
                .as_array()
                .ok_or_else(|| native_error("Unified perpetual order coverage is incomplete."))?;
            if !orders.is_empty() {
                return Err(GoFinError::new(
                    "UNIFIED_CASH_CAPACITY_UNAVAILABLE",
                    "preflight",
                    "Resting perpetual orders may reserve shared unified collateral outside the spot hold field.",
                    "Cancel or resolve perpetual orders in your own Terminal before placing a spot order.",
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn order_scope(
        &self,
        operation: &str,
        request: LimitOrderInput,
        ordinal: u32,
        account_mode: &AccountModeState,
    ) -> Result<(OrderScope, Order, FinancialIntent, Quantity, Quantity)> {
        request_id(&request.request_id)?;
        let account = self.account(&request.account)?;
        let kind = match request.market_type {
            MarketType::Spot => MarketKind::Spot,
            MarketType::Perpetual if request.dex.is_some() => MarketKind::Hip3Perpetual,
            MarketType::Perpetual => MarketKind::Perpetual,
        };
        let market = self
            .market(&request.symbol, Some(kind), request.dex.as_deref())
            .await?;
        account_mode.ensure_supported_cash_accounting()?;
        if !is_canonical_usdc_quote(&market) {
            return Err(GoFinError::new(
                "COLLATERAL_PRICE_UNAVAILABLE",
                "valuation",
                "This market requires an exact quote-asset valuation.",
                "Use a market with verified collateral pricing.",
            ));
        }
        if market.is_delisted {
            return Err(GoFinError::validation(
                "The selected native market is delisted.",
            ));
        }
        if !matches!(request.order_type, OrderType::Trigger)
            && (request.trigger_price.is_some()
                || request.trigger_kind.is_some()
                || request.trigger_is_market.is_some())
        {
            return Err(GoFinError::validation(
                "Trigger parameters require order_type=trigger.",
            ));
        }
        let buy = matches!(request.side, OrderSide::Buy);
        let size = positive(&request.size)?;
        let max_notional = positive(&request.max_notional_usdc)?;
        let max_fee = q(&request.max_fee_usdc)?;
        if let Some(builder) = &request.builder_fee {
            let native_maximum = if matches!(request.market_type, MarketType::Spot) {
                1000
            } else {
                100
            };
            if !valid_address(&builder.address)
                || builder.address == format!("0x{}", "0".repeat(40))
                || builder.fee_tenths_bps == 0
                || builder.fee_tenths_bps > native_maximum
            {
                return Err(GoFinError::validation(
                    "Builder address or fee exceeds this market's native bound.",
                ));
            }
            let minimum_builder_fee = max_notional.mul_div_ceil(
                &Quantity::new(builder.fee_tenths_bps.to_string())?,
                &Quantity::new("100000")?,
            )?;
            if minimum_builder_fee > max_fee {
                return Err(GoFinError::validation(
                    "The explicit fee reserve does not cover the requested builder fee.",
                ));
            }
            let approved = self
                .transport
                .info(Info::MaxBuilderFee {
                    owner: account.execution_address().to_owned(),
                    builder: builder.address.to_ascii_lowercase(),
                })
                .await?;
            if approved
                .as_u64()
                .is_none_or(|rate| rate < u64::from(builder.fee_tenths_bps))
            {
                return Err(GoFinError::new(
                    "BUILDER_FEE_APPROVAL_REQUIRED",
                    "permissions",
                    "The account has not approved this builder's requested fee rate.",
                    "Review hyperliquid.builder.approve_fee in your own Terminal before preparing the order.",
                ));
            }
        }
        let tif = match request.time_in_force.as_ref().unwrap_or(&TimeInForce::Gtc) {
            TimeInForce::Gtc => Tif::Gtc,
            TimeInForce::Ioc => Tif::Ioc,
            TimeInForce::Alo => Tif::Alo,
        };
        let (price, min_price, max_price, order_type) =
            match request.order_type {
                OrderType::Limit => {
                    let price = request.limit_price.as_deref().ok_or_else(|| {
                        GoFinError::validation("A limit order requires limit_price.")
                    })?;
                    positive(price)?;
                    let min = request.min_price.clone().unwrap_or_else(|| {
                        if buy {
                            "0.000000000000000000000000000000000001".into()
                        } else {
                            price.into()
                        }
                    });
                    let max = request.max_price.clone().unwrap_or_else(|| {
                        if buy {
                            price.into()
                        } else {
                            max_notional
                                .mul_div_floor(
                                    &Quantity::from_decimal("1", 36).expect("fixed scale"),
                                    &size,
                                )
                                .map(|x| x.to_decimal(36))
                                .unwrap_or_default()
                        }
                    });
                    (
                        price.to_owned(),
                        min,
                        max,
                        OrderKind::Limit { time_in_force: tif },
                    )
                }
                OrderType::Market => {
                    if request.limit_price.is_some()
                        || request
                            .time_in_force
                            .as_ref()
                            .is_some_and(|t| !matches!(t, TimeInForce::Ioc))
                    {
                        return Err(GoFinError::validation(
                            "Market orders use an explicit bounded IOC price.",
                        ));
                    }
                    let min = request.min_price.clone().ok_or_else(|| {
                        GoFinError::validation("Market orders require min_price.")
                    })?;
                    let max = request.max_price.clone().ok_or_else(|| {
                        GoFinError::validation("Market orders require max_price.")
                    })?;
                    let price = if buy { max.clone() } else { min.clone() };
                    (
                        price,
                        min,
                        max,
                        OrderKind::Limit {
                            time_in_force: Tif::Ioc,
                        },
                    )
                }
                OrderType::Trigger => {
                    let trigger = request.trigger_price.clone().ok_or_else(|| {
                        GoFinError::validation("A trigger order requires trigger_price.")
                    })?;
                    positive(&trigger)?;
                    let min = request.min_price.clone().ok_or_else(|| {
                        GoFinError::validation("Trigger orders require exact min_price.")
                    })?;
                    let max = request.max_price.clone().ok_or_else(|| {
                        GoFinError::validation("Trigger orders require exact max_price.")
                    })?;
                    let price = request
                        .limit_price
                        .clone()
                        .unwrap_or_else(|| if buy { max.clone() } else { min.clone() });
                    let tpsl = match request
                        .trigger_kind
                        .as_ref()
                        .ok_or_else(|| GoFinError::validation("Trigger kind is required."))?
                    {
                        TriggerKind::TakeProfit => Tpsl::TakeProfit,
                        TriggerKind::StopLoss => Tpsl::StopLoss,
                    };
                    (
                        price,
                        min,
                        max,
                        OrderKind::Trigger {
                            trigger_price: trigger,
                            is_market: request.trigger_is_market.unwrap_or(false),
                            tpsl,
                        },
                    )
                }
            };
        let min = positive(&min_price)?;
        let max = positive(&max_price)?;
        let submission_price = positive(&price)?;
        if min > max
            || submission_price < min
            || submission_price > max
            || product(&size, &max)? > max_notional
        {
            return Err(GoFinError::validation(
                "Order price/quantity exceeds its explicit maximum notional.",
            ));
        }
        let order_notional = product(&size, &submission_price)?;
        if !request.reduce_only && order_notional < q("10")? {
            return Err(GoFinError::validation(
                "Opening spot and perpetual orders must meet the native 10 USDC minimum notional.",
            ));
        }
        crate::reads::validate_order_precision(&market, &request.size, &price)
            .map_err(GoFinError::validation)?;
        if let OrderKind::Trigger { trigger_price, .. } = &order_type {
            crate::reads::validate_order_precision(&market, &request.size, trigger_price)
                .map_err(GoFinError::validation)?;
        }
        let identifier = cloid(
            account.execution_address(),
            operation,
            &request.request_id,
            ordinal,
        );
        let spot = matches!(request.market_type, MarketType::Spot);
        let cash_resource = account_mode.cash_resource(
            account,
            if spot {
                CashWallet::Spot
            } else {
                CashWallet::Perpetual
            },
            request.dex.as_deref(),
        )?;
        let resource = if spot {
            cash_resource.clone()
        } else {
            AccountModeState::derivative_resource(account, request.dex.as_deref())
        };
        let mut intent = FinancialIntent::default();
        let mut before = SignedQuantity::positive(&Quantity::zero());
        let mut perpetual_state: Option<Value> = None;
        let capacity_before;
        if matches!(request.market_type, MarketType::Spot) {
            if request.reduce_only {
                return Err(GoFinError::validation(
                    "Spot orders do not support reduce_only.",
                ));
            }
            let base = asset(&market.base_asset.reference);
            let (spent, received) = if buy {
                (
                    Amount {
                        asset: usdc(),
                        atoms: max_notional.clone(),
                    },
                    Amount {
                        asset: base.clone(),
                        atoms: size.clone(),
                    },
                )
            } else {
                (
                    Amount {
                        asset: base.clone(),
                        atoms: size.clone(),
                    },
                    Amount {
                        asset: usdc(),
                        atoms: max_notional.clone(),
                    },
                )
            };
            intent.debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: spent.clone(),
            });
            intent.expected.push(EffectConstraint::Conversion {
                resource: resource.clone(),
                input: spent.asset,
                maximum_input: spent.atoms,
                output: received.asset,
                minimum_output: Quantity::zero(),
            });
            // The native venue can charge spot fees in the base or quote asset.
            // Both exact identities are covered; their total economic fee is
            // independently checked against max_fee_usdc during reconciliation.
            let base_fee = max_fee.mul_div_ceil(&q("1")?, &min)?.min(size.clone());
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: base,
                    atoms: base_fee,
                },
                treatment: if buy {
                    CostTreatment::DeductedFromCredit
                } else {
                    CostTreatment::AdditionalDebit
                },
            });
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: usdc(),
                    atoms: max_fee.clone(),
                },
                treatment: if buy {
                    CostTreatment::AdditionalDebit
                } else {
                    CostTreatment::DeductedFromCredit
                },
            });
        } else {
            let query = if let Some(dex) = &request.dex {
                Info::ClearinghouseStateForDex {
                    owner: account.execution_address().to_owned(),
                    dex: dex.clone(),
                }
            } else {
                Info::ClearinghouseState {
                    owner: account.execution_address().to_owned(),
                }
            };
            let state = self.transport.info(query).await?;
            perpetual_state = Some(state.clone());
            for row in state["assetPositions"]
                .as_array()
                .ok_or_else(|| native_error("Perpetual positions missing."))?
            {
                if row["position"]["coin"] == market.native_coin {
                    before = signed_decimal(
                        row["position"]["szi"]
                            .as_str()
                            .ok_or_else(|| native_error("Position quantity is missing."))?,
                    )?;
                }
            }
            if request.reduce_only
                && (before.is_zero()
                    || (buy && !before.is_negative())
                    || (!buy && before.is_negative())
                    || size > before.magnitude())
            {
                return Err(GoFinError::validation(
                    "A reduce-only order must reduce the exact existing position without crossing zero.",
                ));
            }
            for (suffix, kind) in [
                ("long", PositionKind::DerivativeLong),
                ("short", PositionKind::DerivativeShort),
            ] {
                intent.expected.push(EffectConstraint::DerivativePosition {
                    resource: resource.clone(),
                    position: format!("{}:{suffix}", market.symbol),
                    asset: asset(&market.base_asset.reference),
                    position_kind: kind,
                    minimum_delta: SignedQuantity::negative(&size),
                    maximum_delta: SignedQuantity::positive(&size),
                });
            }
            let max_loss = q(request.max_realized_loss_usdc.as_deref().unwrap_or("0"))?;
            // Realized losses are actual collateral debits, even though opening
            // derivative exposure does not spend the full order notional.
            intent.debits.push(ResourceAmount {
                resource: cash_resource.clone(),
                amount: Amount {
                    asset: usdc(),
                    atoms: max_loss.clone(),
                },
            });
            intent.expected.push(EffectConstraint::Settlement {
                resource: cash_resource.clone(),
                asset: usdc(),
                category: SettlementCategory::RealizedPnl,
                minimum_delta: SignedQuantity::negative(&max_loss),
                maximum_delta: SignedQuantity::positive(&Quantity::zero()),
            });
            intent.expected.push(EffectConstraint::Settlement {
                resource: cash_resource.clone(),
                asset: usdc(),
                category: SettlementCategory::RealizedPnl,
                minimum_delta: SignedQuantity::positive(&Quantity::zero()),
                maximum_delta: SignedQuantity::positive(&max_notional),
            });
            intent
                .maximum_costs
                .push(cost(&cash_resource, "USDC", max_fee.clone()));
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: cash_resource.clone(),
                },
                amount: Amount {
                    asset: usdc(),
                    atoms: max_loss,
                },
                treatment: CostTreatment::IncludedInDebit,
            });
        }
        if spot {
            let state = self
                .transport
                .info(Info::SpotClearinghouseState {
                    owner: account.execution_address().to_owned(),
                })
                .await?;
            let (cash_asset, required, token_index, coin) = if buy {
                (
                    &market.quote_asset,
                    max_notional.checked_add(&max_fee)?,
                    market.quote_asset.token_index,
                    market.quote_asset.symbol.as_str(),
                )
            } else {
                let base_fee = max_fee
                    .mul_div_ceil(&Quantity::from_decimal("1", 36)?, &min)?
                    .min(size.clone());
                (
                    &market.base_asset,
                    size.checked_add(&base_fee)?,
                    market.base_asset.token_index,
                    market.base_asset.symbol.as_str(),
                )
            };
            let (free, _total, _hold) = spot_free_balance(&state, token_index, coin)?;
            if account_mode.mode == AccountMode::Unified {
                self.ensure_unified_spot_cash_unencumbered(account).await?;
            }
            if free < required {
                return Err(GoFinError::new(
                    "INSUFFICIENT_NATIVE_CAPACITY",
                    "preflight",
                    "Native spot free balance after existing order holds does not cover the exact order and fee reserve.",
                    "Cancel or reduce other orders, or choose a smaller order and fee reserve; no automatic transfer or margin action was attempted.",
                ));
            }
            capacity_before = Some(json!({
                "kind":"spot_free_balance",
                "owner":account.execution_address(),
                "coin":cash_asset.symbol,
                "token_index":token_index,
                "required":required,
                "side":if buy {"buy"} else {"sell"}
            }));
        } else if !request.reduce_only {
            let active = self
                .transport
                .info(Info::ActiveAssetData {
                    owner: account.execution_address().to_owned(),
                    coin: market.native_coin.clone(),
                })
                .await?;
            if active["user"].as_str() != Some(account.execution_address())
                || active["coin"].as_str() != Some(market.native_coin.as_str())
            {
                return Err(native_error(
                    "Active asset capacity response is bound to a different user or market.",
                ));
            }
            let leverage_type = active["leverage"]["type"]
                .as_str()
                .filter(|kind| matches!(*kind, "cross" | "isolated"))
                .ok_or_else(|| native_error("Active asset margin type is missing or invalid."))?;
            let leverage = active["leverage"]["value"]
                .as_u64()
                .filter(|value| *value > 0 && *value <= u64::from(market.max_leverage.unwrap_or(0)))
                .ok_or_else(|| {
                    native_error("Active asset leverage is missing or outside market metadata.")
                })?;
            let available_rows = active["availableToTrade"]
                .as_array()
                .filter(|rows| rows.len() == 2)
                .ok_or_else(|| {
                    native_error(
                        "Active available-to-trade capacity must contain buy and sell values.",
                    )
                })?;
            let size_rows = active["maxTradeSzs"]
                .as_array()
                .filter(|rows| rows.len() == 2)
                .ok_or_else(|| {
                    native_error("Active maximum trade size must contain buy and sell values.")
                })?;
            let side_index = usize::from(!buy);
            let side_capacity = capacity_decimal(&available_rows[side_index], "availableToTrade")?;
            let side_max_size = capacity_decimal(&size_rows[side_index], "maxTradeSzs")?;
            let max_loss = q(request.max_realized_loss_usdc.as_deref().unwrap_or("0"))?;
            let required = order_notional
                .checked_add(&max_fee)?
                .checked_add(&max_loss)?;
            let leverage_quantity = Quantity::new(leverage.to_string())?;
            let one_x_capacity =
                side_capacity.mul_div_floor(&Quantity::from_u64(1), &leverage_quantity)?;
            if side_max_size < size || side_capacity < required || one_x_capacity < required {
                return Err(GoFinError::new(
                    "INSUFFICIENT_NATIVE_CAPACITY",
                    "preflight",
                    "Native perps size or margin capacity does not cover this order at the current leverage.",
                    "Reduce the order, free collateral, or review leverage in your own Terminal; GoFin will not change margin automatically.",
                ));
            }
            let free_cash = if account_mode.mode == AccountMode::Unified {
                let balances = self
                    .transport
                    .info(Info::SpotClearinghouseState {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                spot_free_balance(
                    &balances,
                    Some(crate::profile::NATIVE_USDC_TOKEN_INDEX),
                    "USDC",
                )?
                .0
            } else {
                let state = perpetual_state
                    .as_ref()
                    .ok_or_else(|| native_error("Perpetual margin state is unavailable."))?;
                capacity_decimal(&state["withdrawable"], "withdrawable")?
            };
            if free_cash < required {
                return Err(GoFinError::new(
                    "INSUFFICIENT_NATIVE_CAPACITY",
                    "preflight",
                    "Free native collateral does not cover the order at one-times notional plus approved fees and realized-loss reserve.",
                    "Free collateral or lower the order and reserves; no automatic margin transfer was attempted.",
                ));
            }
            capacity_before = Some(json!({
                "kind":"active_asset_capacity",
                "owner":account.execution_address(),
                "coin":market.native_coin,
                "side":if buy {"buy"} else {"sell"},
                "leverage_type":leverage_type,
                "leverage":leverage,
                "required":required
            }));
        } else {
            capacity_before = Some(json!({
                "kind":"reduce_only_position",
                "owner":account.execution_address(),
                "coin":market.native_coin,
                "position_before":before.as_str(),
                "size":size
            }));
        }
        // Rebates are separate native credits, never reductions of previously
        // consumed fee budget. The receipt verifier binds their token and fill.
        let mut rebate_assets = vec![usdc()];
        if matches!(request.market_type, MarketType::Spot) {
            rebate_assets.push(asset(&market.base_asset.reference));
        }
        for rebate_asset in rebate_assets {
            intent.expected.push(EffectConstraint::Credit {
                to: cash_resource.clone(),
                asset: rebate_asset,
                minimum_credit: Quantity::zero(),
            });
        }
        intent.locks.push(resource);
        if !spot {
            intent.locks.push(cash_resource);
        }
        let order = Order {
            asset: market.asset_id,
            is_buy: buy,
            price,
            size: request.size.clone(),
            reduce_only: request.reduce_only,
            order_type,
            client_order_id: Some(identifier.clone()),
        };
        Ok((
            OrderScope {
                input: request,
                market: frozen_market(market),
                cloid: identifier,
                min_price,
                max_price,
                position_before: before.as_str().to_owned(),
                capacity_before,
            },
            order,
            intent,
            max_notional,
            max_fee,
        ))
    }

    pub async fn prepare_action(&self, operation: &str, raw: &Value) -> Result<PreparedOperation> {
        let selected = raw["account"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("An enrolled account is required."))?;
        let selected_account = self.account(selected)?;
        self.verify_account_scope(selected_account).await?;
        let account_mode = self.verified_account_mode(selected_account).await?;
        if operation == INVALIDATE_NONCE_OPERATION {
            return self
                .prepare_nonce_invalidation(raw, selected_account, account_mode)
                .await;
        }
        if selected_account.vault_address().is_some()
            && !matches!(
                operation,
                "hyperliquid.orders.place"
                    | "hyperliquid.orders.batch"
                    | "hyperliquid.orders.edit"
                    | "hyperliquid.orders.edit_batch"
                    | "hyperliquid.orders.cancel"
                    | "hyperliquid.orders.cancel_batch"
                    | "hyperliquid.orders.twap"
                    | "hyperliquid.orders.twap_cancel"
                    | "hyperliquid.orders.schedule_cancel"
                    | "hyperliquid.account.set_leverage"
                    | "hyperliquid.account.add_margin"
                    | "hyperliquid.account.reduce_margin"
            )
        {
            return Err(GoFinError::validation(
                "This operation requires the master owner's account; delegated scope supports reviewed L1 trading only.",
            ));
        }
        if let Some(mut extra) = self.prepare_amend(operation, raw).await? {
            extra.binding.replacement = extra.replacement;
            return self
                .finish_preparation(
                    operation,
                    raw,
                    AccountPreparation {
                        binding: extra.binding,
                        intent: extra.intent,
                        movement: extra.movement,
                        fee: extra.fee,
                        warnings: extra.warnings,
                    },
                )
                .await;
        }
        if let Some(extra) = self.prepare_twap(operation, raw).await? {
            return self.finish_preparation(operation, raw, extra).await;
        }
        if let Some(extra) = self.prepare_account_action(operation, raw).await? {
            return self.finish_preparation(operation, raw, extra).await;
        }
        let account_name = raw["account"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("An enrolled account is required."))?;
        let account = self.account(account_name)?.clone();
        if !account.signing_enabled {
            return Err(GoFinError::new(
                "SIGNING_UNAVAILABLE",
                "preview",
                "This Hyperliquid account is read-only.",
                "Enroll a protected owner signer in your own Terminal.",
            ));
        }
        let id = raw["request_id"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("request_id is required."))?;
        request_id(id)?;
        let mut intent = FinancialIntent::default();
        let mut scopes = Vec::new();
        let mut authority_before = None;
        let mut movement = Quantity::zero();
        let mut fee = Quantity::zero();
        let action = match operation {
            "hyperliquid.orders.place" | "hyperliquid.orders.batch" => {
                let orders = if operation.ends_with(".batch") {
                    let request: BatchOrdersInput = input(raw)?;
                    if request.orders.is_empty() || request.orders.len() > 100 {
                        return Err(GoFinError::validation(
                            "Batch must contain 1–100 explicitly bounded orders.",
                        ));
                    }
                    request.orders
                } else {
                    vec![input::<LimitOrderInput>(raw)?]
                };
                let mut native = Vec::new();
                let mut group = None;
                let mut native_builder: Option<OrderBuilderFeeInput> = None;
                for (ordinal, request) in orders.into_iter().enumerate() {
                    if request.account != account_name {
                        return Err(GoFinError::validation(
                            "Every batch child must use its parent account.",
                        ));
                    }
                    let requested = match request.grouping.as_ref().unwrap_or(&OrderGrouping::Na) {
                        OrderGrouping::Na => Grouping::Na,
                        OrderGrouping::NormalTpsl => Grouping::NormalTpsl,
                        OrderGrouping::PositionTpsl => Grouping::PositionTpsl,
                    };
                    if group.is_some_and(|g| g != requested) {
                        return Err(GoFinError::validation(
                            "A native batch has one explicit order grouping.",
                        ));
                    }
                    group = Some(requested);
                    if ordinal == 0 {
                        native_builder = request.builder_fee.clone();
                    } else if native_builder != request.builder_fee {
                        return Err(GoFinError::validation(
                            "A native batch requires one identical builder fee across all children.",
                        ));
                    }
                    let (scope, order, child, notional, child_fee) = self
                        .order_scope(operation, request, ordinal as u32, &account_mode)
                        .await?;
                    movement = movement.checked_add(&notional)?;
                    fee = fee.checked_add(&child_fee)?;
                    intent.debits.extend(child.debits);
                    intent.maximum_costs.extend(child.maximum_costs);
                    intent.expected.extend(child.expected);
                    intent.locks.extend(child.locks);
                    scopes.push(scope);
                    native.push(order);
                }
                Action::Order {
                    builder_fee: native_builder.map(|builder| HyperliquidBuilderFee {
                        address: builder.address.to_ascii_lowercase(),
                        fee_tenths_bps: builder.fee_tenths_bps,
                    }),
                    orders: native,
                    grouping: group.unwrap_or(Grouping::Na),
                }
            }
            "hyperliquid.orders.cancel" | "hyperliquid.orders.cancel_batch" => {
                let requests = if operation.ends_with("cancel_batch") {
                    input::<CancelOrdersInput>(raw)?.orders
                } else {
                    vec![input::<CancelOrderInput>(raw)?]
                };
                if requests.is_empty() || requests.len() > 100 {
                    return Err(GoFinError::validation(
                        "Cancellation batch must contain 1–100 exact orders.",
                    ));
                }
                let mut cancels = Vec::new();
                let mut originals = Vec::new();
                for request in requests {
                    if request.account != account_name {
                        return Err(GoFinError::validation(
                            "Cancellation account does not match parent.",
                        ));
                    }
                    let market = self.market(&request.symbol, None, None).await?;
                    let oid = request.order_id.parse().map_err(|_| {
                        GoFinError::validation("Cancellation requires a numeric native order ID.")
                    })?;
                    let original = self
                        .transport
                        .info(Info::OrderStatus {
                            owner: account.execution_address().to_owned(),
                            order_id: oid,
                        })
                        .await?;
                    if original["order"]["order"]["coin"] != market.native_coin {
                        return Err(GoFinError::validation(
                            "Order does not belong to the selected account and market.",
                        ));
                    }
                    originals.push(json!({"oid":oid,"coin":market.native_coin,"cloid":original["order"]["order"]["cloid"]}));
                    cancels.push(gofin_exchange::ccxt::HyperliquidCancel {
                        asset: market.asset_id,
                        order_id: oid,
                    });
                }
                authority_before = Some(json!(originals));
                Action::Cancel { cancels }
            }
            "hyperliquid.account.transfer" => {
                let request: TransferInput = input(raw)?;
                if request.asset != "USDC"
                    || matches!(request.destination, TransferDestination::Address)
                {
                    return Err(GoFinError::validation(
                        "Class transfers require USDC and exact spot/perpetual custody.",
                    ));
                }
                let amount = positive(&request.amount)?;
                if amount > positive(&request.max_amount_usdc)? {
                    return Err(GoFinError::validation(
                        "Transfer exceeds its approved amount.",
                    ));
                }
                movement = amount.clone();
                fee = q(&request.max_fee_usdc)?;
                let to_perp = matches!(request.destination, TransferDestination::Perpetual);
                let from = account.resource(if to_perp { "spot" } else { "perpetual" }, None);
                let to = account.resource(if to_perp { "perpetual" } else { "spot" }, None);
                intent.debits.push(ResourceAmount {
                    resource: from.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: amount.clone(),
                    },
                });
                intent.expected.push(EffectConstraint::Transfer {
                    from: from.clone(),
                    to: to.clone(),
                    debit_asset: usdc(),
                    credit_asset: usdc(),
                    maximum_debit: amount.clone(),
                    minimum_credit: amount,
                });
                intent.maximum_costs.push(cost(&from, "USDC", fee.clone()));
                intent.locks.extend([from, to]);
                Action::UsdClassTransfer {
                    amount_usdc: request.amount,
                    to_perp,
                    nonce: 0,
                }
            }
            "hyperliquid.account.set_leverage" => {
                let request: LeverageInput = input(raw)?;
                let kind = if request.dex.is_some() {
                    MarketKind::Hip3Perpetual
                } else {
                    MarketKind::Perpetual
                };
                let market = self
                    .market(&request.symbol, Some(kind), request.dex.as_deref())
                    .await?;
                if request.leverage == 0
                    || market
                        .max_leverage
                        .is_none_or(|max| u32::from(request.leverage) > max)
                {
                    return Err(GoFinError::validation(
                        "Leverage must be within the exact market maximum.",
                    ));
                }
                let state = self
                    .transport
                    .info(Info::ActiveAssetData {
                        owner: account.execution_address().to_owned(),
                        coin: market.native_coin.clone(),
                    })
                    .await?;
                let before =
                    leverage_state(&state, account.execution_address(), &market.native_coin)?;
                let after = format!(
                    "{}:{}",
                    if request.cross { "cross" } else { "isolated" },
                    request.leverage
                );
                let resource = account.resource("perpetual", market.dex.as_deref());
                let authority = format!("leverage:{}", market.native_coin);
                intent.expected.push(EffectConstraint::Authority {
                    resource: resource.clone(),
                    authority,
                    before: before.clone(),
                    after,
                });
                intent.locks.push(resource);
                authority_before = Some(
                    json!({"asset":market.asset_id,"native_coin":market.native_coin,"dex":market.dex,"leverage_before":before}),
                );
                Action::UpdateLeverage {
                    asset: market.asset_id,
                    leverage: u32::from(request.leverage),
                    is_cross: request.cross,
                }
            }
            "hyperliquid.orders.schedule_cancel" => {
                let request: ScheduleCancelInput = input(raw)?;
                let time = request.after_ms;
                if time != 0
                    && time < (chrono::Utc::now().timestamp_millis() as u64).saturating_add(5000)
                {
                    return Err(GoFinError::validation(
                        "Scheduled cancellation must be at least five seconds ahead; zero explicitly disables it.",
                    ));
                }
                Action::ScheduledCancel {
                    time_ms: if time == 0 { None } else { Some(time) },
                }
            }
            _ => {
                return Err(GoFinError::new(
                    "HYPERLIQUID_ACTION_UNAVAILABLE",
                    "preview",
                    "This typed action has not completed its verified execution implementation.",
                    "Inspect the capability matrix; do not bypass GoFin with raw requests.",
                ));
            }
        };
        intent.locks.sort();
        intent.locks.dedup();
        let binding = ActionBinding {
            account: account.clone(),
            account_mode: Some(account_mode),
            action,
            orders: scopes,
            authority_before,
            replacement: None,
            recovery: None,
            metadata: Default::default(),
        };
        self.finish_preparation(
            operation,
            raw,
            AccountPreparation {
                binding,
                intent,
                movement,
                fee,
                warnings: Vec::new(),
            },
        )
        .await
    }

    async fn prepare_nonce_invalidation(
        &self,
        raw: &Value,
        account: &HyperliquidAccount,
        account_mode: AccountModeState,
    ) -> Result<PreparedOperation> {
        let request: InvalidateNonceInput = input(raw)?;
        request_id(&request.request_id)?;
        if request.account != account.name
            || account.execution_scope != HyperliquidExecutionScope::Owner
            || account_mode.mode != AccountMode::Separate
            || account_mode.dex_abstraction == Some(true)
        {
            return Err(GoFinError::validation(
                "Nonce invalidation requires the protected master account in standard separate mode.",
            ));
        }
        if request.original_operation_id.trim().is_empty()
            || request.original_operation_id.len() > 128
        {
            return Err(GoFinError::validation(
                "An exact original GoFin operation ID is required.",
            ));
        }
        let parent = self
            .nonces
            .operation(&request.original_operation_id)?
            .ok_or_else(|| {
                GoFinError::new(
                    "HYPERLIQUID_NONCE_RECOVERY_PARENT_MISSING",
                    "preview",
                    "The original GoFin operation is not in the protected journal.",
                    "Use the original operation ID from GoFin status; a raw nonce cannot be supplied.",
                )
            })?;
        if parent.id != request.original_operation_id {
            return Err(GoFinError::validation(
                "The journal returned a different original operation identity.",
            ));
        }
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let parent_envelope = original_class_transfer(account, &parent)?;
        if !nonce_is_current(parent_envelope.nonce_ms, now_ms) {
            return Err(GoFinError::validation(
                "The original nonce is outside Hyperliquid's native validity window.",
            ));
        }
        let parent_submission = parent
            .submission
            .clone()
            .ok_or_else(|| GoFinError::validation("The original submission is unavailable."))?;
        let recovery = RecoveryBinding {
            version: 1,
            parent_operation_id: parent.id.clone(),
            parent_submission: parent_submission.clone(),
        };
        let binding = ActionBinding {
            account: account.clone(),
            account_mode: Some(account_mode),
            action: Action::Noop {},
            orders: Vec::new(),
            authority_before: Some(json!({
                "original_operation_id": parent.id,
                "original_submission_reference": parent_submission.reference,
                "original_nonce_ms": parent_envelope.nonce_ms,
                "original_wire": parent_submission.data["wire"],
                "scope": "owner-signed spot/perpetual USDC class transfer",
                "resolution": "an exact successful Noop acknowledgement proves the native nonce was consumed by this child only; rejection or missing acknowledgement leaves the parent unresolved"
            })),
            replacement: None,
            recovery: Some(recovery),
            metadata: Default::default(),
        };
        let nonce_lock = ResourceId(format!(
            "hyperliquid:mainnet:nonce:{}:{}",
            account.owner.to_ascii_lowercase(),
            parent_envelope.nonce_ms
        ));
        let intent = FinancialIntent {
            locks: vec![nonce_lock],
            ..FinancialIntent::default()
        };
        let prepared = PreparedOperation {
            operation: INVALIDATE_NONCE_OPERATION.into(),
            input: raw.clone(),
            binding: serde_json::to_value(binding)
                .map_err(|_| native_error("Nonce recovery binding serialization failed."))?,
            summary: format!(
                "Consume the original nonce for unresolved GoFin operation {} on {}",
                parent.id, account.name
            ),
            checks: vec![Check {
                name: "Exact unresolved native class transfer".into(),
                status: CheckStatus::Passed,
                detail: "The same protected master signer, original immutable wire and native nonce are bound; no financial effects are declared.".into(),
            }],
            warnings: vec![
                "Only the exact successful native Noop acknowledgement can prove the nonce was consumed. Rejection, timeout or lost response leaves both this recovery and the original transfer unresolved; the transfer will never be replayed.".into(),
            ],
            observed_at: now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(Valuation {
                movement_usd_micros: Quantity::zero(),
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: now(),
                source: "Noop nonce recovery has no financial valuation.".into(),
                prices: Vec::new(),
            }),
            intent,
            operation_version: 1,
            action_deadline: None,
        };
        if let Some(child) = self.nonces.recovery_child(&parent.id)? {
            let same_awaiting_candidate = child.operation == INVALIDATE_NONCE_OPERATION
                && child.input == prepared.input
                && child.status == Status::AwaitingApproval
                && child.submission.is_none()
                && child.effects.is_none()
                && child.dispatch_evidence.is_none()
                && child.run_id.is_none()
                && child.preview.as_ref().is_some_and(|preview| {
                    preview.operation == prepared.operation
                        && preview.input == prepared.input
                        && preview.binding == prepared.binding
                        && preview.intent == prepared.intent
                });
            let previous_attempt_never_submitted = matches!(
                child.status,
                Status::Failed | Status::Rejected | Status::Expired
            ) && child.operation
                == INVALIDATE_NONCE_OPERATION
                && child.submission.is_none()
                && child.effects.is_none()
                && child.dispatch_evidence.is_none();
            if !same_awaiting_candidate && !previous_attempt_never_submitted {
                return Err(GoFinError::new(
                    "HYPERLIQUID_NONCE_RECOVERY_ALREADY_ATTEMPTED",
                    "preview",
                    "A durable nonce-recovery child already exists for this original operation.",
                    "Reconcile that child; do not create another Noop action for the same nonce.",
                ));
            }
        }
        Ok(prepared)
    }

    async fn finish_preparation(
        &self,
        operation: &str,
        raw: &Value,
        preparation: AccountPreparation,
    ) -> Result<PreparedOperation> {
        let AccountPreparation {
            binding,
            mut intent,
            movement,
            fee,
            mut warnings,
        } = preparation;
        combine_derivative_expectations(&mut intent)?;
        intent.locks.sort();
        intent.locks.dedup();
        let price = self.quote_usdc().await?;
        let mut prices = vec![price.clone()];
        for scope in &binding.orders {
            let base = asset(&scope.market.base_asset.reference);
            if !prices.iter().any(|p| p.asset == base) {
                prices.push(AssetPrice {
                    asset: base,
                    usd_micros_per_unit: Self::value_usdc(&q(&scope.max_price)?, &price)?,
                    unit_atoms: q("1")?,
                    observed_at: price.observed_at.clone(),
                    source: format!("Hyperliquid exact order price ceiling; {}", price.source),
                });
            }
        }
        let mut maximum_cost = Quantity::zero();
        for bound in &intent.maximum_costs {
            let quote = prices
                .iter()
                .find(|p| p.asset == bound.amount.asset)
                .ok_or_else(|| {
                    GoFinError::new(
                        "PRICE_DATA_INVALID",
                        "valuation",
                        "A declared cost asset lacks exact USD valuation.",
                        "Use a supported exact asset price before authorizing this operation.",
                    )
                })?;
            maximum_cost = maximum_cost.checked_add(
                &bound
                    .amount
                    .atoms
                    .mul_div_ceil(&quote.usd_micros_per_unit, &quote.unit_atoms)?,
            )?;
        }
        let declared = Self::value_usdc(&fee, &price)?;
        let valuation = Valuation {
            movement_usd_micros: Self::value_usdc(&movement, &price)?,
            maximum_fee_usd_micros: maximum_cost.max(declared),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: price.observed_at.clone(),
            source: price.source.clone(),
            prices,
        };
        if matches!(binding.action, Action::ScheduledCancel { .. }) {
            warnings.push("Scheduled cancellation applies to every open order in this execution account, including orders created by another client. Existing GoFin reservations remain until each original order is reconciled.".into());
        }
        warnings.push("Venue acceptance is not financial settlement; reconcile the original identity after uncertainty.".into());
        let account = &binding.account;
        Ok(PreparedOperation {
            operation: operation.into(),
            input: raw.clone(),
            binding: serde_json::to_value(&binding)
                .map_err(|_| native_error("Action binding serialization failed."))?,
            summary: format!(
                "{} using {}; {} movement {} USD; {} costs {} USD",
                action_summary(&binding),
                account.name,
                if matches!(binding.action, Action::TwapOrder { .. }) {
                    "estimated"
                } else {
                    "maximum"
                },
                valuation.movement_usd_micros.to_decimal(6),
                if matches!(binding.action, Action::TwapOrder { .. }) {
                    "estimated"
                } else {
                    "maximum"
                },
                valuation.maximum_fee_usd_micros.to_decimal(6)
            ),
            checks: vec![Check {
                name: "Exact account and native action".into(),
                status: CheckStatus::Passed,
                detail:
                    "Owner, market identities, decimal quantities and enforceable bounds verified."
                        .into(),
            }],
            warnings,
            observed_at: now(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            operation_version: 1,
            action_deadline: None,
        })
    }
}
