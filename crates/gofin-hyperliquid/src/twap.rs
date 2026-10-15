//! Native Hyperliquid TWAP preparation and exact slice receipt parsing.
//!
//! Hyperliquid applies a relative-price rule to individual slices; the native
//! action has no absolute quote-price bound for the lifetime of the schedule.
//! The GoFin amounts in `TwapInput` are therefore explicit estimates/reservations,
//! not promises that the venue can enforce.

use crate::{
    account_actions::AccountPreparation,
    account_mode::{AccountModeState, CashWallet},
    profile::{HyperliquidProfile, asset, is_canonical_usdc_quote, usdc},
    read_profile::{input, native_error},
    settlement::unresolved,
    types::{MarketKind, MarketType, OrderSide, TwapCancelInput, TwapInput},
    writes::{ActionBinding, positive, product, q},
};
use gofin_domain::*;
use gofin_exchange::ccxt::{
    HyperliquidAction as Action, HyperliquidActionEnvelope as Envelope,
    HyperliquidInfoRequest as Info,
};
use num_bigint::BigInt;
use num_traits::Zero;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_HISTORY_ROWS: usize = 2_000;

/// Exact fill totals from `userTwapSliceFills`; numbers remain decimal strings
/// until converted to GoFin's exact integer-backed quantities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwapSliceTotals {
    pub size: Quantity,
    pub quote: Quantity,
    pub fees: BTreeMap<String, Quantity>,
    pub closed_pnl: SignedQuantity,
    /// Sum of negative per-slice realized PnL before netting profits against losses.
    pub gross_realized_loss: Quantity,
    /// Sum of positive per-slice realized PnL before netting losses against profits.
    pub gross_realized_profit: Quantity,
    /// `tid` mapped to a digest of the immutable native slice receipt.
    pub event_digests: BTreeMap<u64, String>,
}

/// Extract the native identifier from the documented successful
/// `twapOrder` response. A generic `status: ok` is insufficient to bind it.
pub fn twap_id_from_response(response: &Value) -> Result<u64> {
    if response["status"].as_str() != Some("ok")
        || response["response"]["type"].as_str() != Some("twapOrder")
        || response["response"]["data"]["status"]["running"].is_null()
    {
        return Err(native_error(
            "Successful TWAP response did not include a native running TWAP identifier.",
        ));
    }
    response["response"]["data"]["status"]["running"]["twapId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| native_error("Native TWAP identifier is missing or invalid."))
}

/// Validate the documented successful native `twapCancel` action response.
/// This acknowledges the cancel action only; it does not prove terminal state.
pub fn twap_cancel_acknowledged(response: &Value) -> bool {
    response["status"].as_str() == Some("ok")
        && response["response"]["type"].as_str() == Some("twapCancel")
        && response["response"]["data"]["status"].as_str() == Some("success")
}

/// A response that exactly reaches the documented maximum cannot establish
/// completeness because older rows may have been truncated.
pub fn twap_slice_history_is_complete(raw: &Value) -> bool {
    raw.as_array()
        .is_some_and(|rows| rows.len() < MAX_HISTORY_ROWS)
}

fn invalid(message: &str) -> GoFinError {
    GoFinError::validation(message)
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
}

fn is_positive_decimal(value: &str) -> bool {
    q(value).is_ok_and(|quantity| !quantity.is_zero())
}

fn fractional_digits(value: &str) -> usize {
    value
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.trim_end_matches('0').len())
}

fn signed_decimal(value: &str) -> Result<SignedQuantity> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map(|value| (true, value))
        .unwrap_or((false, value));
    let quantity = Quantity::from_decimal(unsigned, 36)?;
    Ok(if negative {
        SignedQuantity::negative(&quantity)
    } else {
        SignedQuantity::positive(&quantity)
    })
}

fn derivative_position_delta_bounds(
    before: &BigInt,
    requested: &Quantity,
    is_buy: bool,
) -> Result<[(PositionKind, SignedQuantity, SignedQuantity); 2]> {
    let size = BigInt::from(requested.integer());
    let after = before + if is_buy { size } else { -size };
    let before_long = before.clone().max(BigInt::zero());
    let before_short = (-before.clone()).max(BigInt::zero());
    let after_long = after.clone().max(BigInt::zero());
    let after_short = (-after).max(BigInt::zero());
    let long_delta = after_long - before_long;
    let short_delta = after_short - before_short;
    let zero = SignedQuantity::new("0")?;
    let signed = |value: BigInt| SignedQuantity::new(value.to_string());
    if is_buy {
        Ok([
            (
                PositionKind::DerivativeLong,
                zero.clone(),
                signed(long_delta)?,
            ),
            (PositionKind::DerivativeShort, signed(short_delta)?, zero),
        ])
    } else {
        Ok([
            (
                PositionKind::DerivativeLong,
                signed(long_delta)?,
                zero.clone(),
            ),
            (PositionKind::DerivativeShort, zero, signed(short_delta)?),
        ])
    }
}

fn validate_request(request: &TwapInput) -> Result<()> {
    if !valid_request_id(&request.request_id) {
        return Err(invalid("Invalid Hyperliquid request ID."));
    }
    if !matches!(&request.market_type, MarketType::Perpetual) {
        return Err(invalid(
            "Native Hyperliquid TWAP actions require a perpetual market.",
        ));
    }
    if request.duration_minutes == 0 {
        return Err(invalid("TWAP duration must be at least one minute."));
    }
    if !request.accept_native_price_risk {
        return Err(invalid(
            "Set accept_native_price_risk after reviewing that native TWAP has no absolute lifetime quote-price ceiling.",
        ));
    }
    if !is_positive_decimal(&request.size) {
        return Err(invalid("TWAP size must be a positive exact decimal."));
    }
    if !is_positive_decimal(&request.estimated_notional_usdc) {
        return Err(invalid(
            "An explicit positive estimated_notional_usdc reservation is required.",
        ));
    }
    q(&request.estimated_fee_reserve_usdc)?;
    q(&request.estimated_realized_loss_reserve_usdc)?;
    q(&request.estimated_realized_profit_usdc)?;
    Ok(())
}

impl HyperliquidProfile {
    /// Prepare a native `twapOrder` or exact-ID `twapCancel`. The returned
    /// preparation enters the regular immutable action/journal path.
    pub async fn prepare_twap(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<Option<AccountPreparation>> {
        match operation {
            "hyperliquid.orders.twap" => {
                let request: TwapInput = input(raw)?;
                validate_request(&request)?;
                let account = self.account(&request.account)?.clone();
                if !account.signing_enabled {
                    return Err(GoFinError::new(
                        "SIGNING_UNAVAILABLE",
                        "preview",
                        "This Hyperliquid account is read-only.",
                        "Enroll a protected owner signer in your own Terminal.",
                    ));
                }
                let account_mode = self.verified_account_mode(&account).await?;
                account_mode.ensure_supported_cash_accounting()?;
                let dex = request.dex.as_deref();
                let kind = if dex.is_some() {
                    MarketKind::Hip3Perpetual
                } else {
                    MarketKind::Perpetual
                };
                let market = self.market(&request.symbol, Some(kind), dex).await?;
                if !is_canonical_usdc_quote(&market) {
                    return Err(invalid(
                        "TWAP preparation requires an exact USDC-quoted perpetual market.",
                    ));
                }
                if fractional_digits(&request.size) > market.sz_decimals as usize {
                    return Err(invalid("TWAP size exceeds native szDecimals."));
                }
                let reference_price = market
                    .context
                    .as_ref()
                    .and_then(|context| context.mid_price.as_ref().or(context.mark_price.as_ref()))
                    .ok_or_else(|| {
                        GoFinError::new(
                            "MARKET_REFERENCE_UNAVAILABLE",
                            "preview",
                            "A current market reference is required to estimate TWAP notional.",
                            "Retry after Hyperliquid market metadata includes a mid or mark price.",
                        )
                    })?;
                let estimate = product(
                    &positive(&request.size)?,
                    &positive(reference_price.as_str())?,
                )?;
                let reservation = positive(&request.estimated_notional_usdc)?;
                if reservation < estimate {
                    return Err(invalid(
                        "Estimated notional reservation is below the current reference-price estimate.",
                    ));
                }

                let state_request = if let Some(dex) = dex {
                    Info::ClearinghouseStateForDex {
                        owner: account.execution_address().to_owned(),
                        dex: dex.to_owned(),
                    }
                } else {
                    Info::ClearinghouseState {
                        owner: account.execution_address().to_owned(),
                    }
                };
                let state = self.transport.info(state_request).await?;
                let positions = state["assetPositions"]
                    .as_array()
                    .ok_or_else(|| native_error("Perpetual position snapshot is missing."))?;
                let position_before = positions
                    .iter()
                    .find(|row| row["position"]["coin"].as_str() == Some(&market.native_coin))
                    .and_then(|row| row["position"]["szi"].as_str())
                    .unwrap_or("0")
                    .to_owned();
                signed_decimal(&position_before)?;

                let resource = AccountModeState::derivative_resource(&account, dex);
                let cash_resource =
                    account_mode.cash_resource(&account, CashWallet::Perpetual, dex)?;
                let before = signed_decimal(&position_before)?;
                let is_buy = matches!(request.side, OrderSide::Buy);
                let mut intent = FinancialIntent {
                    locks: vec![resource.clone(), cash_resource.clone()],
                    ..FinancialIntent::default()
                };
                let base = asset(&market.base_asset.reference);
                for (kind, minimum_delta, maximum_delta) in derivative_position_delta_bounds(
                    &before.integer(),
                    &positive(&request.size)?,
                    is_buy,
                )? {
                    let suffix = if kind == PositionKind::DerivativeLong {
                        "long"
                    } else {
                        "short"
                    };
                    intent.expected.push(EffectConstraint::DerivativePosition {
                        resource: resource.clone(),
                        position: format!("{}:{suffix}", market.symbol),
                        asset: base.clone(),
                        position_kind: kind,
                        minimum_delta,
                        maximum_delta,
                    });
                }
                let realized_loss_reserve = q(&request.estimated_realized_loss_reserve_usdc)?;
                let realized_profit_estimate = q(&request.estimated_realized_profit_usdc)?;
                intent.debits.push(ResourceAmount {
                    resource: cash_resource.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: realized_loss_reserve.clone(),
                    },
                });
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: cash_resource.clone(),
                    },
                    amount: Amount {
                        asset: usdc(),
                        atoms: q(&request.estimated_fee_reserve_usdc)?,
                    },
                    treatment: CostTreatment::AdditionalDebit,
                });
                intent.maximum_costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: cash_resource.clone(),
                    },
                    amount: Amount {
                        asset: usdc(),
                        atoms: realized_loss_reserve.clone(),
                    },
                    treatment: CostTreatment::IncludedInDebit,
                });
                if !realized_loss_reserve.is_zero() || !realized_profit_estimate.is_zero() {
                    intent.expected.push(EffectConstraint::Settlement {
                        resource: cash_resource.clone(),
                        asset: usdc(),
                        category: SettlementCategory::RealizedPnl,
                        minimum_delta: SignedQuantity::negative(&realized_loss_reserve),
                        maximum_delta: SignedQuantity::positive(&realized_profit_estimate),
                    });
                }
                let warnings = vec![
                    "Hyperliquid TWAP uses native relative slice pricing and has no absolute lifetime quote-price ceiling; notional, fee, realized-loss, and realized-profit amounts are GoFin estimates/reservations, not venue-enforced maximums.".into(),
                    "The schedule continues at Hyperliquid after GoFin records the accepted action; cancel it through its exact native TWAP ID if you want it stopped.".into(),
                ];
                let binding = ActionBinding {
                    replacement: None,
                    recovery: None,
                    metadata: Default::default(),
                    account,
                    account_mode: Some(account_mode),
                    action: Action::TwapOrder {
                        asset: market.asset_id,
                        is_buy,
                        size: request.size.clone(),
                        reduce_only: request.reduce_only,
                        duration_minutes: request.duration_minutes,
                        randomize: request.randomize,
                    },
                    orders: Vec::new(),
                    authority_before: Some(json!({
                        "twap": {
                            "symbol": market.symbol,
                            "native_coin": market.native_coin,
                            "base_asset": market.base_asset.reference,
                            "asset_id": market.asset_id,
                            "dex": market.dex,
                            "side": request.side,
                            "size": request.size,
                            "duration_minutes": request.duration_minutes,
                            "randomize": request.randomize,
                            "reduce_only": request.reduce_only,
                            "reference_price": reference_price.as_str(),
                            "estimated_notional_usdc": request.estimated_notional_usdc,
                            "estimated_fee_reserve_usdc": request.estimated_fee_reserve_usdc,
                            "estimated_realized_loss_reserve_usdc": request.estimated_realized_loss_reserve_usdc,
                            "estimated_realized_profit_usdc": request.estimated_realized_profit_usdc,
                            "accept_native_price_risk": true
                        },
                        "position_before": position_before
                    })),
                };
                Ok(Some(AccountPreparation {
                    binding,
                    intent,
                    movement: reservation,
                    fee: q(&request.estimated_fee_reserve_usdc)?,
                    warnings,
                }))
            }
            "hyperliquid.orders.twap_cancel" => {
                let request: TwapCancelInput = input(raw)?;
                if !valid_request_id(&request.request_id) {
                    return Err(invalid("Invalid Hyperliquid request ID."));
                }
                let twap_id = request
                    .twap_id
                    .parse::<u64>()
                    .map_err(|_| invalid("twap_id must be an exact unsigned native ID."))?;
                if twap_id == 0 || request.twap_id != twap_id.to_string() {
                    return Err(invalid(
                        "twap_id must be a canonical positive native identifier.",
                    ));
                }
                if !matches!(&request.market_type, MarketType::Perpetual) {
                    return Err(invalid("TWAP cancellation requires a perpetual market."));
                }
                let account = self.account(&request.account)?.clone();
                if !account.signing_enabled {
                    return Err(GoFinError::new(
                        "SIGNING_UNAVAILABLE",
                        "preview",
                        "This Hyperliquid account is read-only.",
                        "Enroll a protected owner signer in your own Terminal.",
                    ));
                }
                let account_mode = self.verified_account_mode(&account).await?;
                let market = self
                    .market(
                        &request.symbol,
                        Some(match &request.market_type {
                            MarketType::Perpetual if request.dex.is_some() => {
                                MarketKind::Hip3Perpetual
                            }
                            MarketType::Perpetual => MarketKind::Perpetual,
                            MarketType::Spot => MarketKind::Spot,
                        }),
                        request.dex.as_deref(),
                    )
                    .await?;
                let resource =
                    AccountModeState::derivative_resource(&account, market.dex.as_deref());
                Ok(Some(AccountPreparation {
                    binding: ActionBinding {
                        account,
                        account_mode: Some(account_mode),
                        action: Action::TwapCancel {
                            asset: market.asset_id,
                            twap_id,
                        },
                        orders: Vec::new(),
                        authority_before: Some(json!({
                            "twap_cancel": {"twap_id":twap_id,"market":market}
                        })),
                        replacement: None,
                        recovery: None,
                        metadata: Default::default(),
                    },
                    intent: FinancialIntent {
                        locks: vec![resource],
                        ..FinancialIntent::default()
                    },
                    movement: Quantity::zero(),
                    fee: Quantity::zero(),
                    warnings: vec![
                        "Cancellation does not undo slices already executed; verify native status and receipts after dispatch.".into(),
                    ],
                }))
            }
            _ => Ok(None),
        }
    }

    /// Reconcile one immutable TWAP action. Completed fills are accumulated
    /// from native slice receipts. A full requested quantity proves natural
    /// completion because the venue cannot execute more than the fixed size;
    /// partial fills remain Pending. The REST API exposes no terminal TWAP
    /// state, so an accepted cancellation also remains Pending here.
    pub async fn reconcile_twap(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &ActionBinding,
    ) -> Result<Option<Reconciliation>> {
        if !matches!(
            record.operation.as_str(),
            "hyperliquid.orders.twap" | "hyperliquid.orders.twap_cancel"
        ) {
            return Ok(None);
        }
        if self.account(&binding.account.name)? != &binding.account {
            return Ok(Some(unresolved(
                Status::Unknown,
                "Protected Hyperliquid account identity changed after TWAP preparation.",
                json!({"account":binding.account.name,"owner":binding.account.owner}),
            )));
        }
        {
            let current_mode = match self.verified_account_mode(&binding.account).await {
                Ok(value) => value,
                Err(error) => {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "Native account mode could not be revalidated for TWAP settlement.",
                        json!({"error":error.code}),
                    )));
                }
            };
            let mode_matches = binding.account_mode.as_ref().map_or_else(
                || {
                    current_mode.mode == crate::account_mode::AccountMode::Separate
                        && current_mode.dex_abstraction != Some(true)
                },
                |frozen_mode| &current_mode == frozen_mode,
            );
            if !mode_matches {
                return Ok(Some(unresolved(
                    Status::Unknown,
                    "Hyperliquid account mode or signer role changed after TWAP preparation.",
                    json!({"prepared_mode":binding.account_mode,"current_mode":current_mode}),
                )));
            }
        }
        let Some(dispatch) = record.dispatch_evidence.as_ref() else {
            return Ok(Some(unresolved(
                Status::Unknown,
                "The original TWAP action has no immutable dispatch receipt; it was not resubmitted.",
                json!({"submission_reference":submission.reference}),
            )));
        };
        if submission.kind != crate::writes::SUBMISSION_KIND
            || record.submission.as_ref() != Some(submission)
            || dispatch["submission_reference"].as_str() != Some(submission.reference.as_str())
            || dispatch["response"]["status"].as_str() != Some("ok")
        {
            return Ok(Some(unresolved(
                Status::Unknown,
                "The persisted TWAP submission and successful venue receipt do not match.",
                dispatch.clone(),
            )));
        }
        let envelope_value = submission
            .data
            .get("envelope")
            .cloned()
            .unwrap_or(Value::Null);
        let envelope: Envelope = match serde_json::from_value(envelope_value.clone()) {
            Ok(value) => value,
            Err(_) => {
                return Ok(Some(unresolved(
                    Status::Unknown,
                    "The original native TWAP envelope is malformed.",
                    envelope_value,
                )));
            }
        };
        let wire = envelope
            .action_wire_json()
            .map_err(|_| native_error("Original TWAP action cannot be reconstructed."))?;
        let scoped_identity = submission.data.get("execution_address").is_some();
        if envelope.vault_address.as_deref() != binding.account.vault_address()
            || (scoped_identity
                && submission.data["execution_address"].as_str()
                    != Some(binding.account.execution_address()))
            || (!scoped_identity && binding.account.vault_address().is_some())
        {
            return Ok(Some(unresolved(
                Status::Unknown,
                "TWAP submission execution scope does not match the protected account.",
                json!({}),
            )));
        }
        let expected_reference = if scoped_identity {
            binding_hash(
                &json!({"version":1,"owner":binding.account.owner,"execution_address":binding.account.execution_address(),"vault_address":binding.account.vault_address(),"operation":record.operation,"wire":wire}),
            )?
        } else {
            binding_hash(
                &json!({"version":1,"owner":binding.account.owner,"operation":record.operation,"wire":wire}),
            )?
        };
        if submission.reference != expected_reference
            || submission.data["wire"] != wire
            || submission.data["owner"] != binding.account.owner
            || envelope.action != binding.action
            || envelope.nonce_ms == 0
        {
            return Ok(Some(unresolved(
                Status::Unknown,
                "The immutable TWAP nonce/action differs from the prepared account binding.",
                json!({"envelope":envelope,"binding":binding}),
            )));
        }

        match &envelope.action {
            Action::TwapCancel {
                twap_id,
                asset: action_asset,
            } => {
                let response = &dispatch["response"];
                if !twap_cancel_acknowledged(response) {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "The exact TWAP cancellation has no successful native action acknowledgement.",
                        response.clone(),
                    )));
                }
                Ok(Some(unresolved(
                    Status::Pending,
                    "Hyperliquid acknowledged the exact TWAP cancel, but documented REST reads do not expose terminal schedule state; executed slices remain unresolved until a terminal observation is available.",
                    json!({"twap_id":twap_id,"asset":action_asset,"cancel_ack":response,"nonce_ms":envelope.nonce_ms}),
                )))
            }
            Action::TwapOrder {
                asset: action_asset,
                is_buy,
                size,
                reduce_only,
                duration_minutes,
                randomize,
            } => {
                if record.operation != "hyperliquid.orders.twap" {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "A TWAP create action is bound to a different operation name.",
                        json!({"action":envelope.action}),
                    )));
                }
                let native_twap_id = match twap_id_from_response(&dispatch["response"]) {
                    Ok(value) => value,
                    Err(error) => {
                        return Ok(Some(unresolved(
                            Status::Unknown,
                            "The action was acknowledged without a parseable native TWAP identity; no similar order was inferred or resubmitted.",
                            json!({"response":dispatch["response"],"detail":error.message}),
                        )));
                    }
                };
                let request: TwapInput = match input(&record.input) {
                    Ok(value) => value,
                    Err(_) => {
                        return Ok(Some(unresolved(
                            Status::Unknown,
                            "The exact original TWAP request is unavailable.",
                            record.input.clone(),
                        )));
                    }
                };
                if request.account != binding.account.name
                    || request.size != *size
                    || request.duration_minutes != *duration_minutes
                    || request.randomize != *randomize
                    || request.reduce_only != *reduce_only
                    || matches!(request.side, OrderSide::Buy) != *is_buy
                {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "The original TWAP request differs from the persisted signed action.",
                        json!({"request":request,"action":envelope.action}),
                    )));
                }
                let authority = binding
                    .authority_before
                    .as_ref()
                    .ok_or_else(|| native_error("Prepared TWAP context is missing."))?;
                let twap_meta = &authority["twap"];
                if twap_meta["asset_id"].as_u64() != Some(*action_asset as u64) {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "The signed TWAP asset index differs from the exact prepared native market.",
                        json!({"action_asset":action_asset,"prepared_market":twap_meta}),
                    )));
                }
                let native_coin = twap_meta["native_coin"]
                    .as_str()
                    .ok_or_else(|| native_error("Bound native TWAP market is missing."))?;
                let base_asset = twap_meta["base_asset"]
                    .as_str()
                    .ok_or_else(|| native_error("Bound native TWAP asset is missing."))?;
                let history = self
                    .transport
                    .info(Info::UserTwapSliceFills {
                        owner: binding.account.execution_address().to_owned(),
                    })
                    .await?;
                let current_rows = history
                    .as_array()
                    .ok_or_else(|| native_error("Native TWAP slice history is not an array."))?;
                if !twap_slice_history_is_complete(&history) {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "Native TWAP slice history reached its 2000-row limit; complete slice coverage cannot be established.",
                        json!({"twap_id":native_twap_id,"rows_observed":current_rows.len()}),
                    )));
                }
                let mut merged = BTreeMap::<u64, (Vec<u8>, Value)>::new();
                if let Some(previous_rows) = record
                    .effects
                    .as_ref()
                    .and_then(|effect| effect.evidence["twap_slice_rows"].as_array())
                {
                    for row in previous_rows {
                        if row["twapId"].as_u64() != Some(native_twap_id) {
                            return Ok(Some(unresolved(
                                Status::Unknown,
                                "Persisted TWAP slice evidence contains a different native TWAP ID.",
                                row.clone(),
                            )));
                        }
                        let Some(tid) = row["fill"]["tid"].as_u64() else {
                            return Ok(Some(unresolved(
                                Status::Unknown,
                                "Persisted TWAP slice evidence has no native trade identity.",
                                row.clone(),
                            )));
                        };
                        let canonical = serde_json::to_vec(row).map_err(|_| {
                            native_error("Persisted TWAP slice cannot be canonicalized.")
                        })?;
                        merged.insert(tid, (canonical, row.clone()));
                    }
                }
                for row in current_rows {
                    if row["twapId"].as_u64() != Some(native_twap_id) {
                        continue;
                    }
                    let Some(tid) = row["fill"]["tid"].as_u64() else {
                        return Ok(Some(unresolved(
                            Status::Unknown,
                            "Native TWAP slice is missing its trade identity.",
                            row.clone(),
                        )));
                    };
                    let canonical = serde_json::to_vec(row)
                        .map_err(|_| native_error("Native TWAP slice cannot be canonicalized."))?;
                    if let Some((previous, _)) = merged.get(&tid) {
                        if previous != &canonical {
                            return Ok(Some(unresolved(
                                Status::Unknown,
                                "A repeated TWAP trade identity changed its native receipt.",
                                row.clone(),
                            )));
                        }
                    } else {
                        merged.insert(tid, (canonical, row.clone()));
                    }
                }
                let all_rows: Vec<_> = merged.values().map(|(_, row)| row.clone()).collect();
                let totals = match aggregate_twap_slice_fills(
                    &Value::Array(all_rows.clone()),
                    native_twap_id,
                    native_coin,
                    if *is_buy {
                        OrderSide::Buy
                    } else {
                        OrderSide::Sell
                    },
                    size,
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        return Ok(Some(unresolved(
                            Status::Unknown,
                            "Native TWAP slice receipts do not prove the exact signed schedule.",
                            json!({"twap_id":native_twap_id,"detail":error.message}),
                        )));
                    }
                };
                if totals.size.is_zero() {
                    return Ok(Some(unresolved(
                        Status::Pending,
                        "The native TWAP is accepted and has a native ID, but no exact slice fill is visible yet.",
                        json!({"twap_id":native_twap_id,"history":history}),
                    )));
                }

                let reservation = positive(&request.estimated_notional_usdc)?;
                let fee_reserve = q(&request.estimated_fee_reserve_usdc)?;
                let loss_reserve = q(&request.estimated_realized_loss_reserve_usdc)?;
                let profit_estimate = q(&request.estimated_realized_profit_usdc)?;
                let actual_fee = totals
                    .fees
                    .get("USDC")
                    .cloned()
                    .unwrap_or_else(Quantity::zero);
                if totals.quote > reservation
                    || actual_fee > fee_reserve
                    || totals.gross_realized_loss > loss_reserve
                    || totals.gross_realized_profit > profit_estimate
                {
                    return Ok(Some(unresolved(
                        Status::Unknown,
                        "Observed TWAP turnover, fees, or gross realized loss exceeded its estimate reservation; no terminal effects were inferred, and the original schedule was not resubmitted.",
                        json!({"twap_id":native_twap_id,"estimated_notional_usdc":reservation,"observed_notional_usdc":totals.quote,"estimated_fee_reserve_usdc":fee_reserve,"observed_fee_usdc":actual_fee,"estimated_realized_loss_reserve_usdc":loss_reserve,"observed_gross_realized_loss_usdc":totals.gross_realized_loss,"estimated_realized_profit_usdc":profit_estimate,"observed_gross_realized_profit_usdc":totals.gross_realized_profit,"slice_rows":all_rows}),
                    )));
                }

                let resource =
                    AccountModeState::derivative_resource(&binding.account, request.dex.as_deref());
                let cash_resource = match binding.account_mode.as_ref() {
                    Some(mode) => mode.cash_resource(
                        &binding.account,
                        CashWallet::Perpetual,
                        request.dex.as_deref(),
                    )?,
                    None => binding
                        .account
                        .resource("perpetual", request.dex.as_deref()),
                };
                let before = signed_decimal(authority["position_before"].as_str().unwrap_or("0"))?;
                let before_integer = before.integer();
                let movement = BigInt::from(totals.size.integer());
                let after = &before_integer + if *is_buy { movement } else { -movement };
                let mut effects = Vec::new();
                for (suffix, kind, old, new) in [
                    (
                        "long",
                        PositionKind::DerivativeLong,
                        before_integer.clone().max(BigInt::zero()),
                        after.clone().max(BigInt::zero()),
                    ),
                    (
                        "short",
                        PositionKind::DerivativeShort,
                        (-before_integer.clone()).max(BigInt::zero()),
                        (-after.clone()).max(BigInt::zero()),
                    ),
                ] {
                    let position = format!(
                        "{}:{suffix}",
                        twap_meta["symbol"].as_str().unwrap_or(native_coin)
                    );
                    let principal = Amount {
                        asset: asset(base_asset),
                        atoms: Quantity::new(new.to_string())?,
                    };
                    effects.push(FinancialEffect::PositionChange {
                        resource: resource.clone(),
                        position: position.clone(),
                        asset: principal.asset.clone(),
                        delta: SignedQuantity::new((&new - &old).to_string())?,
                        scaled_receipt: None,
                        anchor: Some(PositionAnchor {
                            resource: resource.clone(),
                            position,
                            position_kind: kind,
                            principal,
                            scaled_receipt: None,
                            evidence_version: 1,
                            evidence: json!({"twap_id":native_twap_id,"position_before":before.as_str(),"position_after":after.to_string(),"slice_trade_ids":totals.event_digests.keys().collect::<Vec<_>>()}),
                            observed_at: now(),
                        }),
                    });
                }
                // Keep loss and profit legs separate and cumulative. Later
                // profitable slices must not erase losses already consumed or
                // regress the IncludedInDebit boundary cost.
                if !loss_reserve.is_zero() {
                    effects.push(FinancialEffect::Settlement {
                        resource: cash_resource.clone(),
                        asset: usdc(),
                        category: SettlementCategory::RealizedPnl,
                        delta: SignedQuantity::negative(&totals.gross_realized_loss),
                    });
                }
                if !profit_estimate.is_zero() {
                    effects.push(FinancialEffect::Settlement {
                        resource: cash_resource.clone(),
                        asset: usdc(),
                        category: SettlementCategory::RealizedPnl,
                        delta: SignedQuantity::positive(&totals.gross_realized_profit),
                    });
                }
                if !totals.gross_realized_loss.is_zero() {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: cash_resource.clone(),
                        },
                        amount: Amount {
                            asset: usdc(),
                            atoms: totals.gross_realized_loss.clone(),
                        },
                        category: "realized_loss".into(),
                        treatment: CostTreatment::IncludedInDebit,
                    });
                }
                for (fee_token, fee) in &totals.fees {
                    if fee.is_zero() {
                        continue;
                    }
                    if fee_token != "USDC" {
                        return Ok(Some(unresolved(
                            Status::Unknown,
                            "A TWAP fee uses a non-USDC token without an exact approved valuation.",
                            json!({"twap_id":native_twap_id,"fee_token":fee_token,"fee":fee}),
                        )));
                    }
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: cash_resource.clone(),
                        },
                        amount: Amount {
                            asset: usdc(),
                            atoms: fee.clone(),
                        },
                        category: "trading_fee".into(),
                        treatment: CostTreatment::AdditionalDebit,
                    });
                }
                let terminal = totals.size == positive(size)?;
                let status = if terminal {
                    Status::Completed
                } else {
                    Status::Pending
                };
                let event_digests: BTreeMap<_, _> = totals
                    .event_digests
                    .iter()
                    .map(|(tid, digest)| (format!("{native_twap_id}:{tid}"), digest.clone()))
                    .collect();
                let evidence = json!({
                    "method":"owner-scoped native TWAP slice receipts",
                    "twap_id":native_twap_id,
                    "coin":native_coin,
                    "side":if *is_buy {"B"} else {"A"},
                    "requested_size":size,
                    "filled_size":totals.size,
                    "executed_notional_usdc":totals.quote,
                    "fees":totals.fees,
                    "closed_pnl":totals.closed_pnl,
                    "gross_realized_loss_usdc":totals.gross_realized_loss,
                    "gross_realized_profit_usdc":totals.gross_realized_profit,
                    "slice_rows":all_rows,
                    "slice_trade_ids":event_digests.keys().collect::<Vec<_>>(),
                    "native_price_risk_acknowledged":request.accept_native_price_risk,
                    "estimated_notional_usdc":reservation,
                    "estimated_fee_reserve_usdc":fee_reserve,
                    "schedule_terminal_by_exact_quantity":terminal
                });
                Ok(Some(self.verified_result(
                    record,
                    submission,
                    status,
                    effects,
                    evidence,
                    event_digests,
                )?))
            }
            _ => Ok(Some(unresolved(
                Status::Unknown,
                "The action binding is not a native TWAP action.",
                json!({"action":envelope.action}),
            ))),
        }
    }
}

/// Parse exact native TWAP slice receipts for one owner-scoped TWAP. Fill rows
/// for other `twapId`s are ignored before their fields are inspected.
pub fn aggregate_twap_slice_fills(
    raw: &Value,
    twap_id: u64,
    native_coin: &str,
    side: OrderSide,
    max_size: &str,
) -> Result<TwapSliceTotals> {
    let rows = raw
        .as_array()
        .ok_or_else(|| native_error("TWAP slice history must be an array."))?;
    let max_size = positive(max_size)?;
    let expected_side = if matches!(side, OrderSide::Buy) {
        "B"
    } else {
        "A"
    };
    let mut size = Quantity::zero();
    let mut quote = Quantity::zero();
    let mut fees = BTreeMap::<String, Quantity>::new();
    let mut pnl = SignedQuantity::positive(&Quantity::zero());
    let mut gross_realized_loss = Quantity::zero();
    let mut gross_realized_profit = Quantity::zero();
    let mut digests = BTreeMap::<u64, String>::new();
    let mut canonical_by_tid = BTreeMap::<u64, Vec<u8>>::new();

    for row in rows {
        if row["twapId"].as_u64() != Some(twap_id) {
            continue;
        }
        let fill = &row["fill"];
        if fill["coin"].as_str() != Some(native_coin)
            || fill["side"].as_str() != Some(expected_side)
        {
            return Err(invalid(
                "Matching TWAP slice changed its bound market or side.",
            ));
        }
        let tid = fill["tid"]
            .as_u64()
            .ok_or_else(|| native_error("TWAP slice trade ID is missing."))?;
        if fill["oid"].as_u64().is_none() || fill["time"].as_u64().is_none() {
            return Err(native_error(
                "TWAP slice order ID or execution timestamp is missing.",
            ));
        }
        if fill["hash"].as_str()
            != Some("0x0000000000000000000000000000000000000000000000000000000000000000")
        {
            return Err(invalid(
                "Native TWAP slice hash does not match the documented zero-hash receipt form.",
            ));
        }
        let canonical = serde_json::to_vec(row)
            .map_err(|_| native_error("TWAP slice receipt cannot be canonicalized."))?;
        if let Some(previous) = canonical_by_tid.get(&tid) {
            if previous != &canonical {
                return Err(invalid(
                    "A repeated TWAP slice trade ID has conflicting receipt contents.",
                ));
            }
            continue;
        }
        let fill_size = positive(
            fill["sz"]
                .as_str()
                .ok_or_else(|| native_error("TWAP slice size is missing."))?,
        )?;
        let fill_price = positive(
            fill["px"]
                .as_str()
                .ok_or_else(|| native_error("TWAP slice price is missing."))?,
        )?;
        let next_size = size.checked_add(&fill_size)?;
        if next_size > max_size {
            return Err(invalid(
                "Native TWAP slice receipts exceed the exact approved size.",
            ));
        }
        size = next_size;
        quote = quote.checked_add(&product(&fill_size, &fill_price)?)?;
        let fee_token = fill["feeToken"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| native_error("TWAP slice fee token is missing."))?;
        let fee_text = fill["fee"]
            .as_str()
            .ok_or_else(|| native_error("TWAP slice fee is missing."))?;
        if fee_text.starts_with('-') {
            return Err(invalid(
                "A negative TWAP fee rebate needs separate typed credit proof.",
            ));
        }
        let fee = q(fee_text)?;
        let accumulated = fees
            .get(fee_token)
            .cloned()
            .unwrap_or_else(Quantity::zero)
            .checked_add(&fee)?;
        fees.insert(fee_token.to_owned(), accumulated);
        let pnl_text = fill["closedPnl"]
            .as_str()
            .ok_or_else(|| native_error("TWAP slice realized PnL is missing."))?;
        let slice_pnl = signed_decimal(pnl_text)?;
        if slice_pnl.is_negative() {
            gross_realized_loss = gross_realized_loss.checked_add(&slice_pnl.magnitude())?;
        } else {
            gross_realized_profit = gross_realized_profit.checked_add(&slice_pnl.magnitude())?;
        }
        pnl = SignedQuantity::new((pnl.integer() + slice_pnl.integer()).to_string())?;
        let digest = hex::encode(Sha256::digest(&canonical));
        canonical_by_tid.insert(tid, canonical);
        digests.insert(tid, digest);
    }

    Ok(TwapSliceTotals {
        size,
        quote,
        fees,
        closed_pnl: pnl,
        gross_realized_loss,
        gross_realized_profit,
        event_digests: digests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> TwapInput {
        TwapInput {
            request_id: "twap-1".into(),
            account: "main".into(),
            symbol: "ETH/USDC:USDC".into(),
            market_type: MarketType::Perpetual,
            dex: None,
            side: OrderSide::Buy,
            size: "0.25".into(),
            duration_minutes: 15,
            estimated_notional_usdc: "1000".into(),
            estimated_fee_reserve_usdc: "2".into(),
            estimated_realized_loss_reserve_usdc: "50".into(),
            estimated_realized_profit_usdc: "1000".into(),
            accept_native_price_risk: true,
            randomize: true,
            reduce_only: false,
        }
    }

    #[test]
    fn native_price_risk_must_be_explicit_and_fields_are_estimates() {
        let mut request = request();
        request.accept_native_price_risk = false;
        assert!(validate_request(&request).is_err());
        request.accept_native_price_risk = true;
        request.estimated_notional_usdc = "0".into();
        assert!(validate_request(&request).is_err());
        request.estimated_notional_usdc = "1000".into();
        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn input_requires_named_estimates_and_explicit_native_risk_acknowledgement() {
        let raw = json!({
            "request_id":"twap-1",
            "account":"main",
            "symbol":"ETH/USDC:USDC",
            "market_type":"perpetual",
            "side":"buy",
            "size":"0.25",
            "duration_minutes":15,
            "estimated_notional_usdc":"1000",
            "estimated_fee_reserve_usdc":"2",
            "estimated_realized_loss_reserve_usdc":"50",
            "estimated_realized_profit_usdc":"1000",
            "accept_native_price_risk":true,
            "randomize":true,
            "reduce_only":false
        });
        assert!(serde_json::from_value::<TwapInput>(raw.clone()).is_ok());
        let mut missing_ack = raw.clone();
        missing_ack
            .as_object_mut()
            .unwrap()
            .remove("accept_native_price_risk");
        assert!(serde_json::from_value::<TwapInput>(missing_ack).is_err());
        let mut old_hard_bound = raw;
        old_hard_bound["max_notional_usdc"] = json!("1000");
        assert!(serde_json::from_value::<TwapInput>(old_hard_bound).is_err());
    }

    #[test]
    fn long_short_position_bounds_include_partial_crossing_of_zero() {
        let before = signed_decimal("-0.5").unwrap().integer();
        let size = positive("1").unwrap();
        let bounds = derivative_position_delta_bounds(&before, &size, true).unwrap();
        assert_eq!(bounds[0].1.as_str(), "0");
        assert_eq!(
            bounds[0].2,
            SignedQuantity::positive(&positive("0.5").unwrap())
        );
        assert_eq!(
            bounds[1].1,
            SignedQuantity::negative(&positive("0.5").unwrap())
        );
        assert_eq!(bounds[1].2.as_str(), "0");
    }

    #[test]
    fn only_documented_running_response_binds_native_id() {
        assert_eq!(
            twap_id_from_response(&json!({
                "status":"ok",
                "response":{"type":"twapOrder","data":{"status":{"running":{"twapId":77}}}}
            }))
            .unwrap(),
            77
        );
        assert!(twap_id_from_response(&json!({"status":"ok"})).is_err());
        assert!(
            twap_id_from_response(&json!({
                "status":"ok",
                "response":{"type":"twapOrder","data":{"status":{"running":{"twapId":0}}}}
            }))
            .is_err()
        );
    }

    #[test]
    fn cancellation_ack_is_not_a_terminal_state_claim() {
        assert!(twap_cancel_acknowledged(&json!({
            "status":"ok",
            "response":{"type":"twapCancel","data":{"status":"success"}}
        })));
        assert!(!twap_cancel_acknowledged(&json!({
            "status":"ok",
            "response":{"type":"twapCancel","data":{"status":"error"}}
        })));
    }

    #[test]
    fn aggregates_exact_native_slice_rows_and_deduplicates_tid() {
        let row = json!({"twapId":77,"fill":{"coin":"ETH","side":"B","oid":900,"tid":901,"time":1700000000000u64,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000","sz":"0.1","px":"2000","fee":"0.2","feeToken":"USDC","closedPnl":"0.0"}});
        let totals = aggregate_twap_slice_fills(
            &json!([row.clone(), row]),
            77,
            "ETH",
            OrderSide::Buy,
            "0.25",
        )
        .unwrap();
        assert_eq!(totals.size, q("0.1").unwrap());
        assert_eq!(totals.quote, q("200").unwrap());
        assert_eq!(totals.fees["USDC"], q("0.2").unwrap());
        assert_eq!(totals.event_digests.len(), 1);
    }

    #[test]
    fn aggregates_gross_loss_and_profit_before_netting() {
        let row = |tid, pnl: &str| json!({"twapId":77,"fill":{"coin":"ETH","side":"B","oid":900+tid,"tid":tid,"time":1700000000000u64,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000","sz":"0.1","px":"2000","fee":"0.01","feeToken":"USDC","closedPnl":pnl}});
        let totals = aggregate_twap_slice_fills(
            &json!([row(901, "-0.4"), row(902, "0.35")]),
            77,
            "ETH",
            OrderSide::Buy,
            "0.25",
        )
        .unwrap();
        assert_eq!(totals.gross_realized_loss, q("0.4").unwrap());
        assert_eq!(totals.gross_realized_profit, q("0.35").unwrap());
        assert_eq!(totals.closed_pnl, signed_decimal("-0.05").unwrap());
    }

    #[test]
    fn rejects_conflicting_duplicate_and_size_overrun() {
        let a = json!({"twapId":77,"fill":{"coin":"ETH","side":"B","oid":900,"tid":901,"time":1700000000000u64,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000","sz":"0.2","px":"2000","fee":"0.2","feeToken":"USDC","closedPnl":"0.0"}});
        let mut conflict = a.clone();
        conflict["fill"]["px"] = json!("2001");
        assert!(
            aggregate_twap_slice_fills(
                &json!([a.clone(), conflict]),
                77,
                "ETH",
                OrderSide::Buy,
                "0.25"
            )
            .is_err()
        );
        assert!(aggregate_twap_slice_fills(&json!([a]), 77, "ETH", OrderSide::Buy, "0.1").is_err());
    }

    #[test]
    fn ignores_rows_for_other_twap_before_validating_them() {
        let totals = aggregate_twap_slice_fills(
            &json!([{"twapId":76,"fill":"malformed"}]),
            77,
            "ETH",
            OrderSide::Buy,
            "0.25",
        )
        .unwrap();
        assert!(totals.size.is_zero());
    }

    #[test]
    fn refuses_history_at_native_response_limit() {
        let rows = vec![json!({"twapId":1,"fill":{}}); MAX_HISTORY_ROWS];
        assert!(!twap_slice_history_is_complete(&json!(rows)));
        assert!(twap_slice_history_is_complete(&json!([])));
    }
}
