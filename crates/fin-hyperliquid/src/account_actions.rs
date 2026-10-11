//! Typed account and administration actions with proof-bound recovery.
//!
//! This module deliberately has no raw action passthrough. It converts only
//! reviewed operation schemas into the CCXT-owned native action enum and
//! leaves uncertain venue results unresolved until authoritative read evidence
//! proves the exact effect.

use crate::{
    account_mode::{AccountMode, AccountModeState, CashWallet},
    profile::{HyperliquidAccount, HyperliquidProfile, asset, is_canonical_usdc_quote, usdc},
    read_profile::{input, native_error},
    types::*,
    writes::{ActionBinding, positive, q},
};
use async_trait::async_trait;
use fin_domain::*;
use fin_exchange::ccxt::{
    HyperliquidAbstraction, HyperliquidAction as Action, HyperliquidExplorerRequest as Explorer,
    HyperliquidInfoRequest as Info,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BRIDGE2: &str = "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7";

/// Frozen account-action preparation consumed by the normal Fin journal path.
#[derive(Clone, Debug)]
pub struct AccountPreparation {
    pub binding: ActionBinding,
    pub intent: FinancialIntent,
    pub movement: Quantity,
    pub fee: Quantity,
    pub warnings: Vec<String>,
}

/// Exact operation identity passed to the trusted Arbitrum receipt observer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalProofRequest {
    pub owner: String,
    pub destination: String,
    pub token: String,
    /// Approved gross withdrawal amount at Fin's 36-decimal accounting scale.
    pub gross_amount: Quantity,
    /// Native Hyperliquid ledger net amount at Fin's 36-decimal scale.
    pub net_amount: Quantity,
    /// Signed withdraw3 action time in milliseconds.
    pub nonce_ms: u64,
    /// Exact native ledger/Bridge2 nonce in microsecond units.
    pub ledger_nonce: u64,
    /// Arbitrum payout transaction hash from the native withdrawal ledger.
    pub ledger_hash: String,
    /// Explorer L1 transaction hash for the signed withdraw3 action.
    pub native_tx_hash: String,
}

/// Trusted chain evidence. The observer must query an independently pinned
/// Arbitrum RPC, require canonical sequencer inclusion, and bind the transfer log to this exact
/// request. The adapter also matches the native Hyperliquid ledger row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalReceiptProof {
    /// Arbitrum payout transaction hash; must equal the linked ledger hash.
    pub transaction_hash: String,
    pub sender: String,
    pub recipient: String,
    pub token: String,
    /// Exact Arbitrum USDC token atoms at the token's native 6 decimals.
    pub net_amount: Quantity,
    pub sequenced: bool,
    pub block_number: u64,
    pub event_digest: String,
}

#[async_trait]
pub trait HyperliquidWithdrawalProof: Send + Sync {
    async fn prove_sequenced_arbitrum_withdrawal(
        &self,
        request: &WithdrawalProofRequest,
    ) -> Result<Option<WithdrawalReceiptProof>>;
}

fn cost(resource: &ResourceId, maximum: Quantity, treatment: CostTreatment) -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: usdc(),
            atoms: maximum,
        },
        treatment,
    }
}

fn address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn request_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
    {
        return Err(FinError::validation("Invalid Hyperliquid request ID."));
    }
    Ok(())
}

fn as_action(
    account: HyperliquidAccount,
    action: Action,
    authority_before: Option<Value>,
    account_mode: AccountModeState,
) -> ActionBinding {
    ActionBinding {
        account,
        action,
        orders: Vec::new(),
        authority_before,
        replacement: None,
        recovery: None,
        account_mode: Some(account_mode),
        metadata: Default::default(),
    }
}

fn frozen_cash_resource(
    account: &HyperliquidAccount,
    mode: Option<&AccountModeState>,
    wallet: CashWallet,
    dex: Option<&str>,
) -> Result<ResourceId> {
    match mode {
        Some(mode) => mode.cash_resource(account, wallet, dex),
        // Older unresolved operation records predate mode bindings. Preserve
        // their original separate-wallet resource identity during recovery.
        None => Ok(account.resource(
            match wallet {
                CashWallet::Spot => "spot",
                CashWallet::Perpetual => "perpetual",
            },
            dex,
        )),
    }
}

fn decimal_to_micro(value: &str) -> Result<u64> {
    q(value)?;
    if value.starts_with('-') {
        return Err(FinError::validation(
            "Native micro-unit amount cannot be negative.",
        ));
    }
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if fractional.len() > 6 && fractional.as_bytes()[6..].iter().any(|byte| *byte != b'0') {
        return Err(FinError::validation(
            "Hyperliquid account actions support at most six exact decimal places.",
        ));
    }
    let padded = format!("{:0<6}", &fractional[..fractional.len().min(6)]);
    let integer = format!("{whole}{padded}");
    integer
        .parse()
        .map_err(|_| FinError::validation("Amount exceeds the native action range."))
}

fn account_child_resource(owner: &str, scope: &str) -> ResourceId {
    ResourceId(format!(
        "hyperliquid:mainnet:{}:{scope}",
        owner.to_lowercase()
    ))
}

fn arbitrum_usdc() -> AssetId {
    AssetId {
        network: "ethereum:42161".into(),
        reference: "0xaf88d065e77c8cc2239327c5edb3a432268e5831".into(),
        decimals: 6,
    }
}

fn action_nonce_matches(action: &Value, nonce: u64) -> bool {
    action["time"].as_u64() == Some(nonce) || action["nonce"].as_u64() == Some(nonce)
}

pub(crate) async fn unique_user_signed_action_tx(
    profile: &HyperliquidProfile,
    owner: &str,
    nonce: u64,
    expected_type: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Option<Value>> {
    let history = profile
        .transport
        .explorer(Explorer::UserDetails {
            user: owner.to_owned(),
        })
        .await?;
    let Some(txs) = history["txs"].as_array() else {
        return Ok(None);
    };
    let candidates: Vec<_> = txs
        .iter()
        .filter(|tx| {
            tx["user"]
                .as_str()
                .is_some_and(|user| user.eq_ignore_ascii_case(owner))
                && action_nonce_matches(&tx["action"], nonce)
                && tx["action"]["type"].as_str() == Some(expected_type)
                && predicate(&tx["action"])
        })
        .collect();
    if candidates.len() > 32 {
        return Ok(None);
    }
    if candidates.len() > 32 {
        return Ok(None);
    }
    let mut matches = Vec::new();
    for candidate in candidates {
        let Some(hash) = candidate["hash"].as_str() else {
            continue;
        };
        if hash.len() != 66
            || !hash.starts_with("0x")
            || !hash.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
        {
            continue;
        }
        let detail = profile
            .transport
            .explorer(Explorer::TxDetails { hash: hash.into() })
            .await?;
        let tx = &detail["tx"];
        if tx["hash"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(hash))
            && tx["user"]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(owner))
            && action_nonce_matches(&tx["action"], nonce)
            && tx["action"]["type"].as_str() == Some(expected_type)
            && predicate(&tx["action"])
            && tx["error"].is_null()
            && tx["block"].as_u64().is_some_and(|block| block > 0)
        {
            matches.push(tx.clone());
        }
    }
    Ok((matches.len() == 1).then(|| matches.remove(0)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeClassTransferOutcome {
    Accepted(Value),
    Rejected { tx: Value, error: String },
}

/// Resolves only the exact committed native class-transfer action. Explorer's
/// txDetails schema exposes an error string for a failed transaction; this
/// recovery path accepts only the specific class-transfer validation error
/// that proves no cash movement occurred.
pub(crate) async fn exact_class_transfer_tx_outcome(
    profile: &HyperliquidProfile,
    owner: &str,
    nonce: u64,
    expected_action: &Value,
) -> Result<Option<NativeClassTransferOutcome>> {
    let history = profile
        .transport
        .explorer(Explorer::UserDetails {
            user: owner.to_owned(),
        })
        .await?;
    let Some(txs) = history["txs"].as_array() else {
        return Ok(None);
    };
    let candidates: Vec<_> = txs
        .iter()
        .filter(|tx| {
            tx["user"]
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case(owner))
                && action_nonce_matches(&tx["action"], nonce)
        })
        .collect();
    if candidates.len() != 1 {
        return Ok(None);
    }
    let candidate = candidates[0];
    if candidate["action"] != *expected_action {
        return Ok(None);
    }
    let Some(hash) = candidate["hash"].as_str() else {
        return Ok(None);
    };
    if !valid_explorer_hash(hash) {
        return Ok(None);
    }
    let (Some(block), Some(time)) = (candidate["block"].as_u64(), candidate["time"].as_u64())
    else {
        return Ok(None);
    };
    if block == 0 || time == 0 {
        return Ok(None);
    }
    let candidate_error = match candidate.get("error") {
        Some(Value::Null) => None,
        Some(Value::String(error)) if !error.trim().is_empty() => Some(error.as_str()),
        _ => return Ok(None),
    };

    let detail = profile
        .transport
        .explorer(Explorer::TxDetails { hash: hash.into() })
        .await?;
    let tx = &detail["tx"];
    if !tx["hash"]
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case(hash))
        || !tx["user"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(owner))
        || tx["action"] != *expected_action
        || !action_nonce_matches(&tx["action"], nonce)
        || tx["block"].as_u64() != Some(block)
        || tx["time"].as_u64() != Some(time)
    {
        return Ok(None);
    }
    let detail_error = match tx.get("error") {
        Some(Value::Null) => None,
        Some(Value::String(error)) if !error.trim().is_empty() => Some(error.as_str()),
        _ => return Ok(None),
    };
    if detail_error != candidate_error {
        return Ok(None);
    }
    let outcome = match detail_error {
        None => NativeClassTransferOutcome::Accepted(tx.clone()),
        Some(error) => NativeClassTransferOutcome::Rejected {
            tx: tx.clone(),
            error: error.into(),
        },
    };
    Ok(Some(outcome))
}

fn valid_explorer_hash(hash: &str) -> bool {
    hash.len() == 66
        && hash.starts_with("0x")
        && hash.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn response_ok(record: &OperationRecord, submission: &Submission) -> bool {
    record.dispatch_evidence.as_ref().is_some_and(|evidence| {
        evidence["submission_reference"].as_str() == Some(submission.reference.as_str())
            && evidence["response"]["status"].as_str() == Some("ok")
    })
}

fn submission_envelope(submission: &Submission) -> Result<(Value, u64)> {
    let data = &submission.data;
    let envelope = data
        .get("envelope")
        .cloned()
        .ok_or_else(|| native_error("Original signed action envelope is missing."))?;
    let nonce = envelope["nonce_ms"]
        .as_u64()
        .ok_or_else(|| native_error("Original signed action nonce is missing."))?;
    Ok((envelope, nonce))
}

fn verified(
    submission: &Submission,
    effects: Vec<FinancialEffect>,
    evidence: Value,
) -> Result<VerifiedEffects> {
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects,
        evidence,
        observed_at: now(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = fin_domain::verified_effect_digest(&bundle)?;
    Ok(bundle)
}

fn unknown(reason: &str, evidence: Value) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"observations":evidence}),
        verified: None,
    }
}

fn extract_ledger_rows(response: &Value) -> &[Value] {
    response
        .as_array()
        .or_else(|| response.get("rows").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn valid_transaction_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn ledger_withdrawal(
    rows: &[Value],
    expected_nonce: u64,
    action_time_ms: u64,
    observed_end_ms: u64,
) -> Option<&Value> {
    if action_time_ms.checked_mul(1000) != Some(expected_nonce) {
        return None;
    }
    let mut matches = rows.iter().filter(|row| {
        row["delta"]["type"].as_str() == Some("withdraw")
            && row["delta"]["nonce"].as_u64() == Some(expected_nonce)
    });
    let first = matches.next()?;
    if matches.next().is_some()
        || first["time"]
            .as_u64()
            .is_none_or(|time_ms| time_ms < action_time_ms || time_ms > observed_end_ms)
        || !first["hash"].as_str().is_some_and(valid_transaction_hash)
    {
        return None;
    }
    Some(first)
}

fn withdrawal_ledger_amounts(
    row: &Value,
    approved_gross: &Quantity,
) -> Option<(Quantity, Quantity)> {
    let net = exact_decimal_field(&row["delta"]["usdc"]).ok()?;
    let fee = exact_decimal_field(&row["delta"]["fee"]).ok()?;
    (!net.is_zero() && net.checked_add(&fee).ok()? == *approved_gross).then_some((net, fee))
}

fn ledger_internal_transfer<'a>(rows: &'a [Value], tx_hash: &str) -> Option<&'a Value> {
    let mut matches = rows.iter().filter(|row| {
        row["delta"]["type"].as_str() == Some("internalTransfer")
            && row["hash"]
                .as_str()
                .is_some_and(|hash| hash.eq_ignore_ascii_case(tx_hash))
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn hyperliquid_perpetual_resource(owner: &str) -> ResourceId {
    ResourceId(format!(
        "hyperliquid:mainnet:{}:perpetual:primary",
        owner.to_ascii_lowercase()
    ))
}

fn authority_transition(
    resource: ResourceId,
    authority: &str,
    before: String,
    allowed_after: Vec<String>,
) -> EffectConstraint {
    EffectConstraint::AuthorityTransition {
        resource,
        authority: authority.to_owned(),
        before,
        allowed_after,
    }
}

impl HyperliquidProfile {
    /// Prepare reviewed account/admin operations. `None` means the operation
    /// belongs to another typed action handler.
    pub async fn prepare_account_action(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<Option<AccountPreparation>> {
        let supported = matches!(
            operation,
            "hyperliquid.account.add_margin"
                | "hyperliquid.account.reduce_margin"
                | "hyperliquid.account.withdraw"
                | "hyperliquid.vault.deposit"
                | "hyperliquid.vault.withdraw"
                | "hyperliquid.vault.create"
                | "hyperliquid.account.reserve_request_weight"
                | "hyperliquid.account.set_abstraction"
                | "hyperliquid.account.set_dex_abstraction"
                | "hyperliquid.account.set_agent_abstraction"
                | "hyperliquid.referral.set"
                | "hyperliquid.builder.approve_fee"
                | "hyperliquid.account.create_subaccount"
                | "hyperliquid.account.transfer"
        );
        if !supported {
            return Ok(None);
        }
        let account_name = raw["account"]
            .as_str()
            .ok_or_else(|| FinError::validation("An enrolled account is required."))?;
        let account = self.account(account_name)?.clone();
        if account.vault_address().is_some()
            && !matches!(
                operation,
                "hyperliquid.account.add_margin" | "hyperliquid.account.reduce_margin"
            )
        {
            return Err(FinError::validation(
                "Delegated Hyperliquid scopes support trading and margin only; account authority, transfers, deposits, withdrawals and administrative actions require the enrolled owner scope.",
            ));
        }
        if !account.signing_enabled {
            return Err(FinError::new(
                "SIGNING_UNAVAILABLE",
                "preview",
                "This Hyperliquid account is read-only.",
                "Enroll a protected owner signer in your own Terminal.",
            ));
        }
        // Every account-action signature is tied to the protected master, not
        // an API wallet or a delegated address. Cash actions also freeze the
        // exact mode that gives their balances meaning.
        let account_mode = self.verified_account_mode(&account).await?;
        let uses_cash_accounting = matches!(
            operation,
            "hyperliquid.account.add_margin"
                | "hyperliquid.account.reduce_margin"
                | "hyperliquid.account.withdraw"
                | "hyperliquid.vault.deposit"
                | "hyperliquid.vault.withdraw"
                | "hyperliquid.vault.create"
                | "hyperliquid.account.reserve_request_weight"
                | "hyperliquid.account.transfer"
        );
        let perps = if uses_cash_accounting {
            account_mode.cash_resource(&account, CashWallet::Perpetual, None)?
        } else {
            account.resource("perpetual", None)
        };
        let id = raw["request_id"]
            .as_str()
            .ok_or_else(|| FinError::validation("request_id is required."))?;
        request_id(id)?;
        let mut intent = FinancialIntent::default();
        let mut movement = Quantity::zero();
        let mut fee = Quantity::zero();
        let authority_before;
        let mut warnings = Vec::new();
        let action = match operation {
            "hyperliquid.account.add_margin" | "hyperliquid.account.reduce_margin" => {
                let request: MarginInput = input(raw)?;
                let kind = if request.dex.is_some() {
                    MarketKind::Hip3Perpetual
                } else {
                    MarketKind::Perpetual
                };
                let mut market = self
                    .market(&request.symbol, Some(kind), request.dex.as_deref())
                    .await?;
                if !is_canonical_usdc_quote(&market) {
                    return Err(FinError::new(
                        "COLLATERAL_PRICE_UNAVAILABLE",
                        "valuation",
                        "Isolated margin requires an exact canonical USDC quote asset.",
                        "Use a primary or HIP-3 USDC-quoted perpetual market.",
                    ));
                }
                let perps = account_mode.cash_resource(
                    &account,
                    CashWallet::Perpetual,
                    market.dex.as_deref(),
                )?;
                // Freeze market identity, not volatile context/freshness data.
                // Revalidation must not turn a stable margin action into a
                // changed preview merely because marks advanced or metadata
                // was observed milliseconds later.
                market.observed_at_ms = 0;
                market.context = None;
                let amount = positive(&request.amount)?;
                let maximum = positive(&request.max_amount_usdc)?;
                if amount > maximum || decimal_to_micro(&request.amount)? == 0 {
                    return Err(FinError::validation(
                        "Margin exceeds its exact approved bound.",
                    ));
                }
                let state_request = if let Some(dex) = &market.dex {
                    Info::ClearinghouseStateForDex {
                        owner: account.execution_address().to_owned(),
                        dex: dex.clone(),
                    }
                } else {
                    Info::ClearinghouseState {
                        owner: account.execution_address().to_owned(),
                    }
                };
                let state = self.transport.info(state_request).await?;
                if state["user"]
                    .as_str()
                    .is_some_and(|owner| !owner.eq_ignore_ascii_case(account.execution_address()))
                {
                    return Err(native_error(
                        "Isolated-margin snapshot belongs to a different account owner.",
                    ));
                }
                let row = state["assetPositions"]
                    .as_array()
                    .ok_or_else(|| native_error("Perpetual position snapshot is missing."))?
                    .iter()
                    .find(|row| row["position"]["coin"].as_str() == Some(&market.native_coin))
                    .ok_or_else(|| {
                        FinError::validation("The selected isolated position does not exist.")
                    })?;
                if row["position"]["leverage"]["type"].as_str() != Some("isolated") {
                    return Err(FinError::validation(
                        "Margin changes require an isolated position.",
                    ));
                }
                let adding = operation.ends_with("add_margin");
                movement = amount.clone();
                fee = q(&request.max_fee_usdc)?;
                let resource =
                    AccountModeState::derivative_resource(&account, market.dex.as_deref());
                let position_name = format!("{}:isolated-collateral", market.symbol);
                intent.debits.push(ResourceAmount {
                    resource: if adding {
                        perps.clone()
                    } else {
                        resource.clone()
                    },
                    amount: Amount {
                        asset: usdc(),
                        atoms: amount.clone(),
                    },
                });
                let signed = if adding {
                    SignedQuantity::positive(&amount)
                } else {
                    SignedQuantity::negative(&amount)
                };
                intent.expected.push(EffectConstraint::OriginMovement {
                    from: if adding {
                        AccountingLocation::Cash {
                            resource: perps.clone(),
                        }
                    } else {
                        AccountingLocation::Position {
                            resource: resource.clone(),
                            position: position_name.clone(),
                            position_kind: PositionKind::LockedCollateral,
                        }
                    },
                    to: if adding {
                        AccountingLocation::Position {
                            resource: resource.clone(),
                            position: position_name,
                            position_kind: PositionKind::LockedCollateral,
                        }
                    } else {
                        AccountingLocation::Cash {
                            resource: perps.clone(),
                        }
                    },
                    debit_asset: usdc(),
                    credit_asset: usdc(),
                    maximum_debit: amount.clone(),
                    minimum_credit: amount.clone(),
                });
                intent.maximum_costs.push(cost(
                    &perps,
                    fee.clone(),
                    CostTreatment::AdditionalDebit,
                ));
                intent.locks.extend([perps.clone(), resource]);
                authority_before = Some(
                    json!({"market":market,"position":row,"state":state,"margin_delta":signed}),
                );
                // Exact native before/after position state is required during reconciliation.
                Action::UpdateIsolatedMargin {
                    asset: market.asset_id,
                    amount_usdc: if adding {
                        request.amount
                    } else {
                        format!("-{}", request.amount)
                    },
                }
            }
            "hyperliquid.account.withdraw" => {
                let request: WithdrawalInput = input(raw)?;
                if request.asset != "USDC" || !address(&request.address) {
                    return Err(FinError::validation(
                        "Withdrawals support only USDC to one exact EVM address.",
                    ));
                }
                let amount = positive(&request.amount)?;
                let maximum = positive(&request.max_amount_usdc)?;
                if amount > maximum {
                    return Err(FinError::validation(
                        "Withdrawal exceeds its approved maximum.",
                    ));
                }
                movement = amount.clone();
                fee = q(&request.max_fee_usdc)?;
                if fee >= amount {
                    return Err(FinError::validation(
                        "Withdrawal fee ceiling must be less than the gross amount.",
                    ));
                }
                let destination =
                    ResourceId(format!("ethereum:42161:{}", request.address.to_lowercase()));
                let minimum_credit_hl = amount
                    .checked_sub(&fee)
                    .unwrap_or_else(|_| Quantity::zero());
                let minimum_credit_arbitrum =
                    Quantity::from_decimal(&minimum_credit_hl.to_decimal(36), 6)?;
                intent.debits.push(ResourceAmount {
                    resource: perps.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: amount.clone(),
                    },
                });
                // This is an exact cross-network transfer between two cash
                // resources. Keeping it typed as Transfer lets the shared
                // engine bind the deducted fee to the proved destination
                // credit while retaining the gross native debit bound.
                intent.expected.push(EffectConstraint::Transfer {
                    from: perps.clone(),
                    to: destination,
                    debit_asset: usdc(),
                    credit_asset: arbitrum_usdc(),
                    maximum_debit: amount.clone(),
                    minimum_credit: minimum_credit_arbitrum,
                });
                intent.maximum_costs.push(cost(
                    &perps,
                    fee.clone(),
                    CostTreatment::IncludedInDebit,
                ));
                intent.locks.push(perps.clone());
                authority_before = Some(
                    json!({"withdrawal":{"owner":account.owner,"destination":request.address,"asset":"USDC","gross_amount":request.amount}}),
                );
                warnings.push("Venue acceptance is not withdrawal finality; settlement waits for the matching Hyperliquid ledger row and sequenced Arbitrum receipt.".into());
                Action::Withdraw {
                    destination: request.address,
                    amount_usdc: request.amount,
                    time_ms: 0,
                }
            }
            "hyperliquid.vault.deposit" | "hyperliquid.vault.withdraw" => {
                let request: VaultActionInput = input(raw)?;
                if !address(&request.vault_address) {
                    return Err(FinError::validation("Select one exact vault address."));
                }
                let amount = positive(&request.amount)?;
                let maximum = positive(&request.max_amount_usdc)?;
                if amount > maximum {
                    return Err(FinError::validation(
                        "Vault amount exceeds its approved maximum.",
                    ));
                }
                let micros = decimal_to_micro(&request.amount)?;
                movement = amount.clone();
                fee = q(&request.max_fee_usdc)?;
                let depositing = operation.ends_with("deposit");
                let vault_resource = account_child_resource(
                    &account.owner,
                    &format!("vault:{}", request.vault_address.to_lowercase()),
                );
                let principal = Amount {
                    asset: usdc(),
                    atoms: amount.clone(),
                };
                if depositing {
                    intent.debits.push(ResourceAmount {
                        resource: perps.clone(),
                        amount: principal.clone(),
                    });
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: AccountingLocation::Cash {
                            resource: perps.clone(),
                        },
                        to: AccountingLocation::Position {
                            resource: vault_resource.clone(),
                            position: request.vault_address.to_lowercase(),
                            position_kind: PositionKind::Investment,
                        },
                        debit_asset: usdc(),
                        credit_asset: usdc(),
                        maximum_debit: amount.clone(),
                        minimum_credit: amount.clone(),
                    });
                } else {
                    intent.expected.push(EffectConstraint::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: vault_resource.clone(),
                            position: request.vault_address.to_lowercase(),
                            position_kind: PositionKind::Investment,
                        },
                        to: AccountingLocation::Cash {
                            resource: perps.clone(),
                        },
                        debit_asset: usdc(),
                        credit_asset: usdc(),
                        maximum_debit: amount.clone(),
                        minimum_credit: amount
                            .checked_sub(&fee)
                            .unwrap_or_else(|_| Quantity::zero()),
                    });
                    intent.maximum_costs.push(cost(
                        &perps,
                        fee.clone(),
                        CostTreatment::DeductedFromCredit,
                    ));
                }
                intent.locks.extend([perps.clone(), vault_resource.clone()]);
                let equities = self
                    .transport
                    .info(Info::UserVaultEquities {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                authority_before = Some(
                    json!({"vault":request.vault_address,"depositing":depositing,"amount":request.amount,"equities":equities}),
                );
                let signed_micro = i64::try_from(micros)
                    .map_err(|_| FinError::validation("Vault amount exceeds native range."))?;
                Action::VaultTransfer {
                    vault_address: request.vault_address,
                    is_deposit: depositing,
                    amount_usd_micro: signed_micro,
                }
            }
            "hyperliquid.vault.create" => {
                let request: CreateVaultInput = input(raw)?;
                if request.name.trim().is_empty()
                    || request.name.len() > 64
                    || request.description.trim().is_empty()
                    || request.description.len() > 1024
                {
                    return Err(FinError::validation(
                        "Vault name and description must be bounded and nonempty.",
                    ));
                }
                let seed = positive(&request.seed_amount_usdc)?;
                let max_fee = q(&request.max_fee_usdc)?;
                if max_fee < positive("100")? {
                    return Err(FinError::validation(
                        "Vault creation requires an approved fee ceiling of at least 100 USDC.",
                    ));
                }
                let micros = decimal_to_micro(&request.seed_amount_usdc)?;
                movement = seed.clone();
                fee = max_fee.clone();
                intent.debits.push(ResourceAmount {
                    resource: perps.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: seed.clone(),
                    },
                });
                intent.maximum_costs.push(cost(
                    &perps,
                    max_fee.clone(),
                    CostTreatment::AdditionalDebit,
                ));
                intent.locks.push(perps.clone());
                authority_before = Some(
                    self.transport
                        .info(Info::UserVaultEquities {
                            owner: account.execution_address().to_owned(),
                        })
                        .await?,
                );
                Action::CreateVault {
                    name: request.name,
                    description: request.description,
                    initial_usd_micro: micros,
                }
            }
            "hyperliquid.account.reserve_request_weight" => {
                let request: ReserveWeightInput = input(raw)?;
                if request.weight == 0 || request.weight > 1_000_000 {
                    return Err(FinError::validation(
                        "Request-weight reservation must be within 1–1,000,000 units.",
                    ));
                }
                let weight = q(&request.weight.to_string())?;
                fee = crate::writes::product(&weight, &q("0.0005")?)?;
                intent.debits.push(ResourceAmount {
                    resource: perps.clone(),
                    amount: Amount {
                        asset: usdc(),
                        atoms: fee.clone(),
                    },
                });
                intent.maximum_costs.push(cost(
                    &perps,
                    fee.clone(),
                    CostTreatment::IncludedInDebit,
                ));
                intent.locks.push(perps.clone());
                let destination_role = if let Some(destination) = request.destination.as_deref() {
                    if !address(destination) || destination.eq_ignore_ascii_case(&account.owner) {
                        return Err(FinError::validation(
                            "An explicit request-weight recipient must be a different exact user address.",
                        ));
                    }
                    let role = self
                        .transport
                        .info(Info::UserRole {
                            owner: destination.to_owned(),
                        })
                        .await?;
                    if role["role"].as_str() != Some("user") || role.get("data").is_some() {
                        return Err(FinError::validation(
                            "Request-weight reservations can be gifted only to an existing Hyperliquid user.",
                        ));
                    }
                    Some(role)
                } else {
                    None
                };
                let rate_limit = self
                    .transport
                    .info(Info::UserRateLimit {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                authority_before = Some(json!({
                    "rate_limit":rate_limit,
                    "destination":request.destination,
                    "destination_role":destination_role,
                    "weight":request.weight,
                    "usdc_cost":fee,
                }));
                Action::ReserveRequestWeight {
                    weight: request.weight,
                    destination: request.destination,
                }
            }
            "hyperliquid.account.set_abstraction" | "hyperliquid.account.set_agent_abstraction" => {
                let request: AbstractionInput = input(raw)?;
                let before = self
                    .transport
                    .info(Info::UserAbstractionState {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                let abstraction = match request.mode {
                    AbstractionMode::Unified => HyperliquidAbstraction::UnifiedAccount,
                    AbstractionMode::Separate => HyperliquidAbstraction::Disabled,
                    AbstractionMode::PortfolioMargin => HyperliquidAbstraction::PortfolioMargin,
                };
                let authority = if operation.ends_with("set_agent_abstraction") {
                    "agent_abstraction"
                } else {
                    "unified_account"
                };
                let expected = match abstraction {
                    HyperliquidAbstraction::Disabled => "disabled",
                    HyperliquidAbstraction::UnifiedAccount => "unifiedAccount",
                    HyperliquidAbstraction::PortfolioMargin => "portfolioMargin",
                };
                let before_state = abstraction_value(&before).ok_or_else(|| {
                    native_error("Account abstraction state has an unknown shape.")
                })?;
                intent.expected.push(authority_transition(
                    account.resource("account", None),
                    authority,
                    before_state.to_owned(),
                    vec![expected.into()],
                ));
                authority_before = Some(
                    json!({"before":before,"expected":expected,"agent":operation.ends_with("set_agent_abstraction")}),
                );
                Action::UserAbstraction {
                    user: account.owner.clone(),
                    abstraction,
                    agent: operation.ends_with("set_agent_abstraction"),
                }
            }
            "hyperliquid.account.set_dex_abstraction" => {
                let request: DexAbstractionInput = input(raw)?;
                let before = self
                    .transport
                    .info(Info::UserDexAbstractionState {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                let before_value = if before.is_null() {
                    "null".to_owned()
                } else {
                    before
                        .as_bool()
                        .ok_or_else(|| {
                            native_error("DEX abstraction state is not boolean or null.")
                        })?
                        .to_string()
                };
                intent.expected.push(EffectConstraint::Authority {
                    resource: account.resource("account", None),
                    authority: "dex_abstraction".into(),
                    before: before_value,
                    after: request.enabled.to_string(),
                });
                authority_before = Some(json!({"before":before,"expected":request.enabled}));
                Action::UserDexAbstraction {
                    user: account.owner.clone(),
                    enabled: request.enabled,
                    agent: false,
                }
            }
            "hyperliquid.referral.set" => {
                let request: ReferralInput = input(raw)?;
                if request.referral_code.trim().is_empty() || request.referral_code.len() > 128 {
                    return Err(FinError::validation("A bounded referral code is required."));
                }
                // A venue read is required both to freeze the current value and
                // later to prove the exact irreversible relationship change.
                let before = self
                    .transport
                    .info(Info::ReferralState {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                let previous_code = before["referredBy"]["code"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                authority_before =
                    Some(json!({"before":before,"expected_code":request.referral_code}));
                intent.expected.push(EffectConstraint::Authority {
                    resource: account.resource("account", None),
                    authority: "referral_code".into(),
                    before: previous_code,
                    after: request.referral_code.clone(),
                });
                Action::SetRef {
                    code: request.referral_code,
                }
            }
            "hyperliquid.builder.approve_fee" => {
                let request: BuilderFeeInput = input(raw)?;
                // Native builder-code limits are 0.1% on perps and 1% on
                // spot. Cap the account-level approval at the highest
                // protocol-supported ceiling: 1000 tenths of a bps = 1%.
                if !address(&request.builder_address) || request.fee_tenths_bps > 1_000 {
                    return Err(FinError::validation(
                        "Builder address or fee ceiling is invalid.",
                    ));
                }
                let before = self
                    .transport
                    .info(Info::MaxBuilderFee {
                        owner: account.execution_address().to_owned(),
                        builder: request.builder_address.clone(),
                    })
                    .await?;
                let rate = tenths_bps_percent(request.fee_tenths_bps);
                intent.expected.push(EffectConstraint::Authority {
                    resource: account.resource("account", None),
                    authority: format!("builder_fee:{}", request.builder_address.to_lowercase()),
                    before: serde_json::to_string(&before).unwrap_or_default(),
                    after: request.fee_tenths_bps.to_string(),
                });
                authority_before = Some(
                    json!({"before":before,"expected_rate":rate,"builder":request.builder_address}),
                );
                Action::ApproveBuilderFee {
                    builder: request.builder_address,
                    max_fee_rate: rate,
                    nonce: 0,
                }
            }
            "hyperliquid.account.create_subaccount" => {
                let request: SubaccountInput = input(raw)?;
                let name = request.name.trim();
                if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                    return Err(FinError::validation(
                        "Subaccount name must be 1–64 printable characters.",
                    ));
                }
                let before = self
                    .transport
                    .info(Info::SubAccounts {
                        owner: account.execution_address().to_owned(),
                    })
                    .await?;
                authority_before = Some(
                    json!({"before":before,"name":name,"expires_after_ms":request.expires_after_ms}),
                );
                intent.expected.push(EffectConstraint::Authority {
                    resource: account.resource("account", None),
                    authority: "subaccount_created".into(),
                    before: "absent".into(),
                    after: name.to_owned(),
                });
                Action::CreateSubAccount {
                    name: name.to_owned(),
                    expires_after_ms: request.expires_after_ms,
                }
            }
            "hyperliquid.account.transfer" => {
                let request: TransferInput = input(raw)?;
                if matches!(&request.destination, TransferDestination::Address) {
                    if request.asset != "USDC"
                        || request.subaccount_address.is_some()
                        || !request.destination_address.as_deref().is_some_and(address)
                    {
                        return Err(FinError::validation(
                            "Address transfers support USDC to one exact Hyperliquid account address.",
                        ));
                    }
                    let target = request.destination_address.as_deref().unwrap();
                    if target.eq_ignore_ascii_case(&account.owner) {
                        return Err(FinError::validation(
                            "A Core USDC transfer must name a different owner address.",
                        ));
                    }
                    let role = self
                        .transport
                        .info(Info::UserRole {
                            owner: target.to_owned(),
                        })
                        .await?;
                    let role_name = role["role"].as_str().unwrap_or("");
                    if !matches!(role_name, "user" | "subAccount") {
                        return Err(FinError::validation(
                            "Address transfers require an existing Hyperliquid user or subaccount; new, agent, vault, and unknown addresses are not supported.",
                        ));
                    }
                    let amount = positive(&request.amount)?;
                    if amount > positive(&request.max_amount_usdc)?
                        || decimal_to_micro(&request.amount)? == 0
                    {
                        return Err(FinError::validation(
                            "Address transfer exceeds its exact approved amount or precision.",
                        ));
                    }
                    fee = q(&request.max_fee_usdc)?;
                    movement = amount.clone();
                    let from = account_mode.cash_resource(&account, CashWallet::Perpetual, None)?;
                    let to = hyperliquid_perpetual_resource(target);
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
                        maximum_debit: amount,
                        minimum_credit: q(&request.amount)?,
                    });
                    intent.maximum_costs.push(cost(
                        &from,
                        fee.clone(),
                        CostTreatment::AdditionalDebit,
                    ));
                    intent.locks.extend([from, to]);
                    authority_before = Some(json!({
                        "transfer": {
                            "owner": account.owner,
                            "destination": target,
                            "asset": "USDC",
                            "source_wallet": "perpetual",
                            "destination_role": role_name,
                            "amount": request.amount,
                        }
                    }));
                    warnings.push(
                        "Venue acknowledgement is not settlement; completion waits for the exact nonce and matching sender and recipient ledger records.".into(),
                    );
                    Action::UsdSend {
                        destination: target.to_owned(),
                        amount_usdc: request.amount,
                        time_ms: 0,
                    }
                } else {
                    if request.destination_address.is_some() {
                        return Err(FinError::validation(
                            "destination_address is only valid when destination is address.",
                        ));
                    }
                    if !matches!(&request.destination, TransferDestination::Subaccount) {
                        return Ok(None);
                    }
                    let target = request
                        .subaccount_address
                        .as_deref()
                        .filter(|value| address(value))
                        .ok_or_else(|| {
                            FinError::validation(
                                "Subaccount transfers require the exact child address.",
                            )
                        })?;
                    let direction = request.subaccount_direction.as_ref().ok_or_else(|| {
                        FinError::validation("Subaccount transfer direction is required.")
                    })?;
                    let children = self
                        .transport
                        .info(Info::SubAccounts {
                            owner: account.execution_address().to_owned(),
                        })
                        .await?;
                    let rows = children
                        .as_array()
                        .or_else(|| children["subAccounts"].as_array())
                        .ok_or_else(|| {
                            native_error("Authoritative child-account list is missing.")
                        })?;
                    let matches: Vec<_> = rows
                        .iter()
                        .filter(|row| {
                            row["subAccountUser"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(target))
                                || row["user"]
                                    .as_str()
                                    .is_some_and(|value| value.eq_ignore_ascii_case(target))
                        })
                        .collect();
                    if matches.len() != 1 {
                        return Err(FinError::validation(
                            "Target is not exactly one subaccount of the enrolled parent.",
                        ));
                    }
                    let amount = positive(&request.amount)?;
                    if amount > positive(&request.max_amount_usdc)? {
                        return Err(FinError::validation(
                            "Subaccount transfer exceeds its approved maximum.",
                        ));
                    }
                    let to_child = matches!(direction, SubaccountTransferDirection::ToSubaccount);
                    let from_child =
                        account_mode.cash_resource(&account, CashWallet::Spot, None)?;
                    let child_scope = account_child_resource(target, "spot");
                    movement = amount.clone();
                    fee = q(&request.max_fee_usdc)?;
                    let usdc_transfer = request.asset == "USDC";
                    let action = if usdc_transfer {
                        let micros = decimal_to_micro(&request.amount)?;
                        Action::SubAccountTransfer {
                            sub_account_user: target.to_owned(),
                            is_deposit: to_child,
                            usd_micro: micros,
                        }
                    } else {
                        let token = request.asset.clone();
                        let market = self
                            .market(&format!("{token}/USDC"), Some(MarketKind::Spot), None)
                            .await?;
                        if market.base_asset.reference != token {
                            return Err(FinError::validation(
                                "Only a uniquely identified token-to-USDC spot market supports valued subaccount transfers.",
                            ));
                        }
                        let px = market
                            .context
                            .as_ref()
                            .and_then(|context| {
                                context.mid_price.as_ref().or(context.mark_price.as_ref())
                            })
                            .ok_or_else(|| {
                                FinError::new(
                                    "COLLATERAL_PRICE_UNAVAILABLE",
                                    "valuation",
                                    "A fresh exact token/USDC spot price is required.",
                                    "Use USDC or retry after the market quote is available.",
                                )
                            })?;
                        movement = crate::writes::product(&amount, &positive(px.as_str())?)?;
                        intent.expected.push(EffectConstraint::Transfer {
                            from: if to_child {
                                from_child.clone()
                            } else {
                                child_scope.clone()
                            },
                            to: if to_child {
                                child_scope.clone()
                            } else {
                                from_child.clone()
                            },
                            debit_asset: asset(&token),
                            credit_asset: asset(&token),
                            maximum_debit: amount.clone(),
                            minimum_credit: amount.clone(),
                        });
                        intent.debits.push(ResourceAmount {
                            resource: if to_child {
                                from_child.clone()
                            } else {
                                child_scope.clone()
                            },
                            amount: Amount {
                                asset: asset(&token),
                                atoms: amount.clone(),
                            },
                        });
                        Action::SubAccountSpotTransfer {
                            sub_account_user: target.to_owned(),
                            is_deposit: to_child,
                            token,
                            amount: request.amount.clone(),
                        }
                    };
                    if usdc_transfer {
                        let from = if to_child {
                            from_child.clone()
                        } else {
                            child_scope.clone()
                        };
                        let to = if to_child {
                            child_scope.clone()
                        } else {
                            from_child.clone()
                        };
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
                            minimum_credit: amount.clone(),
                        });
                    }
                    intent.maximum_costs.push(cost(
                        &from_child,
                        fee.clone(),
                        CostTreatment::AdditionalDebit,
                    ));
                    intent.locks.extend([from_child, child_scope]);
                    authority_before = Some(
                        json!({"parent":account.owner,"child":target,"children":children,"direction":direction,"asset":request.asset,"amount":request.amount}),
                    );
                    action
                }
            }
            _ => unreachable!(),
        };
        intent.locks.sort();
        intent.locks.dedup();
        Ok(Some(AccountPreparation {
            binding: as_action(account, action, authority_before, account_mode),
            intent,
            movement,
            fee,
            warnings,
        }))
    }

    /// Reconcile the immutable account-action submission. Financial moves are
    /// terminal only when venue-native movement and destination proof agree;
    /// otherwise the exact original submission remains unknown.
    pub async fn reconcile_account_action(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        binding: &ActionBinding,
    ) -> Result<Option<Reconciliation>> {
        let supported = matches!(
            record.operation.as_str(),
            "hyperliquid.account.add_margin"
                | "hyperliquid.account.reduce_margin"
                | "hyperliquid.account.withdraw"
                | "hyperliquid.vault.deposit"
                | "hyperliquid.vault.withdraw"
                | "hyperliquid.vault.create"
                | "hyperliquid.account.reserve_request_weight"
                | "hyperliquid.account.set_abstraction"
                | "hyperliquid.account.set_dex_abstraction"
                | "hyperliquid.account.set_agent_abstraction"
                | "hyperliquid.referral.set"
                | "hyperliquid.builder.approve_fee"
                | "hyperliquid.account.create_subaccount"
                | "hyperliquid.account.transfer"
        );
        if !supported {
            return Ok(None);
        }
        // These two native operations have independent nonce-bound explorer
        // and ledger proof paths. A lost HTTP response must not block a later
        // positive proof from those authoritative sources.
        let nonce_bound_action = matches!(
            &binding.action,
            Action::UsdSend { .. } | Action::UsdClassTransfer { .. }
        ) && record.dispatch_evidence.is_none();
        if record.submission.as_ref() != Some(submission)
            || submission.kind != crate::writes::SUBMISSION_KIND
            || (!response_ok(record, submission) && !nonce_bound_action)
        {
            return Ok(Some(unknown(
                "The persisted action submission or dispatch receipt does not match this reconciliation request.",
                json!({"submission_reference":submission.reference}),
            )));
        }
        let (envelope_value, nonce) = match submission_envelope(submission) {
            Ok(value) => value,
            Err(error) => return Ok(Some(unknown(&error.message, Value::Null))),
        };
        let envelope: fin_exchange::ccxt::HyperliquidActionEnvelope =
            match serde_json::from_value(envelope_value.clone()) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(Some(unknown(
                        "Original typed action envelope is malformed.",
                        envelope_value,
                    )));
                }
            };
        let wire = envelope.action_wire_json().ok();
        if envelope.nonce_ms != nonce
            || envelope.action != normalized_action(&binding.action, nonce)
            || submission.data.get("wire") != wire.as_ref()
            || submission.data["owner"].as_str() != Some(binding.account.owner.as_str())
        {
            return Ok(Some(unknown(
                "Persisted action identity differs from the immutable prepared binding.",
                json!({"nonce_ms":nonce,"wire":submission.data["wire"]}),
            )));
        }
        if binding.account != *self.account(&binding.account.name)? {
            return Ok(Some(unknown(
                "Protected account identity changed after submission.",
                Value::Null,
            )));
        }
        let cash_operation = matches!(
            record.operation.as_str(),
            "hyperliquid.account.add_margin"
                | "hyperliquid.account.reduce_margin"
                | "hyperliquid.account.withdraw"
                | "hyperliquid.vault.deposit"
                | "hyperliquid.vault.withdraw"
                | "hyperliquid.vault.create"
                | "hyperliquid.account.reserve_request_weight"
                | "hyperliquid.account.transfer"
        );
        if cash_operation {
            let current_mode = match self.verified_account_mode(&binding.account).await {
                Ok(value) => value,
                Err(error) => {
                    return Ok(Some(unknown(
                        "Current master role and account mode cannot safely map this cash effect.",
                        json!({"mode_error":error.code}),
                    )));
                }
            };
            match binding.account_mode.as_ref() {
                Some(frozen) if frozen == &current_mode => {}
                // Older immutable submissions did not persist mode. Their
                // historical resource identity is safe only in the separate
                // wallet mode that originally defined that identity.
                None if current_mode.mode == AccountMode::Separate
                    && current_mode.dex_abstraction != Some(true) => {}
                _ => {
                    return Ok(Some(unknown(
                        "Current cash-account mode differs from the mode bound to this original action.",
                        json!({"frozen_mode":binding.account_mode,"current_mode":current_mode}),
                    )));
                }
            }
        }

        // Class transfers are settled by the native action nonce plus the
        // matching ledger row in the shared settlement path below this one.
        // This account-action handler owns only subaccount transfers.
        if matches!(envelope.action, Action::UsdClassTransfer { .. }) {
            return Ok(None);
        }

        match &envelope.action {
            Action::UserAbstraction {
                user,
                abstraction,
                agent,
            } => {
                let state = self
                    .transport
                    .info(Info::UserAbstractionState {
                        owner: user.clone(),
                    })
                    .await?;
                let actual = abstraction_value(&state);
                let expected = match abstraction {
                    HyperliquidAbstraction::Disabled => "disabled",
                    HyperliquidAbstraction::UnifiedAccount => "unifiedAccount",
                    HyperliquidAbstraction::PortfolioMargin => "portfolioMargin",
                };
                if actual != Some(expected) || user != &binding.account.owner {
                    return Ok(Some(unknown(
                        "Post-action abstraction state does not equal the exact requested value.",
                        json!({"state":state,"expected":expected}),
                    )));
                }
                let authority = if *agent {
                    "agent_abstraction"
                } else {
                    "unified_account"
                };
                let before = binding
                    .authority_before
                    .as_ref()
                    .and_then(|v| abstraction_value(&v["before"]))
                    .unwrap_or("unknown")
                    .to_owned();
                let effect = FinancialEffect::AuthorityChange {
                    resource: binding.account.resource("account", None),
                    authority: authority.into(),
                    before,
                    after: expected.into(),
                };
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"source":"post-action userAbstractionState","state":state,"agent":agent}),
                    verified: Some(verified(submission, vec![effect], json!({"state":state}))?),
                }))
            }
            Action::UserDexAbstraction {
                user,
                enabled,
                agent,
            } => {
                let state = self
                    .transport
                    .info(Info::UserDexAbstractionState {
                        owner: user.clone(),
                    })
                    .await?;
                if state.as_bool() != Some(*enabled) || user != &binding.account.owner {
                    return Ok(Some(unknown(
                        "Post-action DEX abstraction state does not equal the exact requested value.",
                        json!({"state":state,"expected":enabled}),
                    )));
                }
                let before = binding
                    .authority_before
                    .as_ref()
                    .map(|value| {
                        if value["before"].is_null() {
                            "null".to_owned()
                        } else {
                            value["before"]
                                .as_bool()
                                .map(|enabled| enabled.to_string())
                                .unwrap_or_else(|| "unknown".to_owned())
                        }
                    })
                    .unwrap_or_else(|| "unknown".to_owned());
                let effect = FinancialEffect::AuthorityChange {
                    resource: binding.account.resource("account", None),
                    authority: if *agent {
                        "agent_dex_abstraction"
                    } else {
                        "dex_abstraction"
                    }
                    .into(),
                    before,
                    after: enabled.to_string(),
                };
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"source":"post-action userDexAbstractionState","state":state}),
                    verified: Some(verified(submission, vec![effect], json!({"state":state}))?),
                }))
            }
            Action::SetRef { code } => {
                let state = self
                    .transport
                    .info(Info::ReferralState {
                        owner: binding.account.execution_address().to_owned(),
                    })
                    .await?;
                if state["referredBy"]["code"].as_str() != Some(code.as_str()) {
                    return Ok(Some(unknown(
                        "The referral endpoint does not confirm the requested code.",
                        json!({"state":state,"expected":code}),
                    )));
                }
                let before = binding
                    .authority_before
                    .as_ref()
                    .and_then(|v| v["before"]["referredBy"]["code"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let effect = FinancialEffect::AuthorityChange {
                    resource: binding.account.resource("account", None),
                    authority: "referral_code".into(),
                    before,
                    after: code.clone(),
                };
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"source":"post-action referral info","state":state}),
                    verified: Some(verified(submission, vec![effect], json!({"state":state}))?),
                }))
            }
            Action::ApproveBuilderFee {
                builder,
                max_fee_rate,
                ..
            } => {
                let state = self
                    .transport
                    .info(Info::MaxBuilderFee {
                        owner: binding.account.execution_address().to_owned(),
                        builder: builder.clone(),
                    })
                    .await?;
                let expected = percent_to_tenths_bps(max_fee_rate)?;
                let actual = exact_integer(&state).ok_or_else(|| {
                    native_error("maxBuilderFee response is not an exact integer.")
                })?;
                if actual != expected {
                    return Ok(Some(unknown(
                        "The approved builder fee does not equal the exact requested ceiling.",
                        json!({"state":state,"expected_tenths_bps":expected}),
                    )));
                }
                let before = binding
                    .authority_before
                    .as_ref()
                    .and_then(|v| exact_integer(&v["before"]))
                    .unwrap_or_default()
                    .to_string();
                let effect = FinancialEffect::AuthorityChange {
                    resource: binding.account.resource("account", None),
                    authority: format!("builder_fee:{}", builder.to_lowercase()),
                    before,
                    after: expected.to_string(),
                };
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"source":"post-action maxBuilderFee","builder":builder,"tenths_bps":actual}),
                    verified: Some(verified(
                        submission,
                        vec![effect],
                        json!({"builder":builder,"tenths_bps":actual}),
                    )?),
                }))
            }
            Action::CreateSubAccount { name, .. } => {
                let after = self
                    .transport
                    .info(Info::SubAccounts {
                        owner: binding.account.execution_address().to_owned(),
                    })
                    .await?;
                let rows = after
                    .as_array()
                    .or_else(|| after["subAccounts"].as_array())
                    .ok_or_else(|| {
                        native_error("Subaccount list response has an unknown shape.")
                    })?;
                let before = binding
                    .authority_before
                    .as_ref()
                    .and_then(|v| v.get("before"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let before_rows: &[Value] = before
                    .as_array()
                    .or_else(|| before["subAccounts"].as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let new_rows: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        row["name"].as_str() == Some(name.as_str())
                            && row["subAccountUser"]
                                .as_str()
                                .or(row["user"].as_str())
                                .is_some_and(|user| {
                                    !before_rows.iter().any(|old| {
                                        old["subAccountUser"].as_str().or(old["user"].as_str())
                                            == Some(user)
                                    })
                                })
                    })
                    .collect();
                if new_rows.len() != 1 {
                    return Ok(Some(unknown(
                        "Subaccount post-state does not prove one newly created exact child.",
                        json!({"before":before,"after":after,"name":name}),
                    )));
                }
                let child = new_rows[0]["subAccountUser"]
                    .as_str()
                    .or(new_rows[0]["user"].as_str())
                    .ok_or_else(|| native_error("Created subaccount address is missing."))?;
                if !address(child) {
                    return Ok(Some(unknown(
                        "Created subaccount address is malformed.",
                        new_rows[0].clone(),
                    )));
                }
                let role = self
                    .transport
                    .info(Info::UserRole {
                        owner: child.into(),
                    })
                    .await?;
                if role["role"].as_str() != Some("subAccount")
                    || !role["data"]["master"]
                        .as_str()
                        .is_some_and(|master| master.eq_ignore_ascii_case(&binding.account.owner))
                {
                    return Ok(Some(unknown(
                        "Created child role does not prove the exact parent relationship.",
                        json!({"child":child,"role":role}),
                    )));
                }
                let effect = FinancialEffect::AuthorityChange {
                    resource: binding.account.resource("account", None),
                    authority: "subaccount_created".into(),
                    before: "absent".into(),
                    after: name.clone(),
                };
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"before":before,"after":after,"role":role}),
                    verified: Some(verified(
                        submission,
                        vec![effect],
                        json!({"child":child,"name":name,"role":role}),
                    )?),
                }))
            }
            Action::UpdateIsolatedMargin { amount_usdc, .. } => {
                let amount_text = amount_usdc.trim_start_matches('-');
                let amount = positive(amount_text)?;
                let adding = !amount_usdc.starts_with('-');
                let before = binding
                    .authority_before
                    .as_ref()
                    .ok_or_else(|| native_error("Original isolated-margin snapshot is missing."))?;
                let market = &before["market"];
                let coin = market["native_coin"]
                    .as_str()
                    .ok_or_else(|| native_error("Original isolated-margin coin is missing."))?;
                let dex = market["dex"].as_str();
                let symbol = market["symbol"]
                    .as_str()
                    .ok_or_else(|| native_error("Original isolated-margin symbol is missing."))?;
                let before_row = &before["position"]["position"];
                let before_margin = exact_decimal_field(&before_row["isolatedMargin"])?;
                let state_request = if let Some(dex) = dex {
                    Info::ClearinghouseStateForDex {
                        owner: binding.account.execution_address().to_owned(),
                        dex: dex.to_owned(),
                    }
                } else {
                    Info::ClearinghouseState {
                        owner: binding.account.execution_address().to_owned(),
                    }
                };
                let after_state = self.transport.info(state_request).await?;
                if after_state["user"].as_str().is_some_and(|owner| {
                    !owner.eq_ignore_ascii_case(binding.account.execution_address())
                }) {
                    return Ok(Some(unknown(
                        "Post-action margin state belongs to a different owner.",
                        after_state,
                    )));
                }
                let rows = after_state["assetPositions"]
                    .as_array()
                    .ok_or_else(|| native_error("Post-action position list is missing."))?;
                let matching: Vec<_> = rows
                    .iter()
                    .filter(|row| row["position"]["coin"].as_str() == Some(coin))
                    .collect();
                let [after_row] = matching.as_slice() else {
                    return Ok(Some(unknown(
                        "Post-action margin state does not uniquely identify the original coin.",
                        after_state,
                    )));
                };
                if after_row["position"]["leverage"]["type"].as_str() != Some("isolated") {
                    return Ok(Some(unknown(
                        "Post-action state no longer identifies the exact isolated position.",
                        after_state,
                    )));
                }
                let after_margin = exact_decimal_field(&after_row["position"]["isolatedMargin"])?;
                let expected_margin = if adding {
                    before_margin.checked_add(&amount)?
                } else {
                    before_margin.checked_sub(&amount).map_err(|_| {
                        native_error("Requested margin reduction exceeds the original collateral.")
                    })?
                };
                if after_margin != expected_margin {
                    return Ok(Some(unknown(
                        "Post-action isolated collateral does not equal the exact requested transition.",
                        json!({"before":before_row,"after":after_row,"expected_margin":expected_margin}),
                    )));
                }
                let resource = AccountModeState::derivative_resource(&binding.account, dex);
                let cash = frozen_cash_resource(
                    &binding.account,
                    binding.account_mode.as_ref(),
                    CashWallet::Perpetual,
                    dex,
                )?;
                let position = format!("{symbol}:isolated-collateral");
                let (from, to) = if adding {
                    (
                        AccountingLocation::Cash { resource: cash },
                        AccountingLocation::Position {
                            resource: resource.clone(),
                            position,
                            position_kind: PositionKind::LockedCollateral,
                        },
                    )
                } else {
                    (
                        AccountingLocation::Position {
                            resource: resource.clone(),
                            position,
                            position_kind: PositionKind::LockedCollateral,
                        },
                        AccountingLocation::Cash { resource: cash },
                    )
                };
                let amount = Amount {
                    asset: usdc(),
                    atoms: amount,
                };
                let effect = FinancialEffect::OriginMovement {
                    from,
                    to,
                    debit: amount.clone(),
                    credit: amount,
                };
                let evidence = json!({"source":"exact isolated collateral transition","owner":binding.account.owner,"coin":coin,"before_margin":before_margin,"after_margin":after_margin,"position":after_row});
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: evidence.clone(),
                    verified: Some(verified(submission, vec![effect], evidence)?),
                }))
            }
            Action::Withdraw {
                destination,
                amount_usdc,
                time_ms,
            } => {
                let amount = positive(amount_usdc)?;
                let native_tx = unique_user_signed_action_tx(
                    self,
                    &binding.account.owner,
                    *time_ms,
                    "withdraw3",
                    |action| {
                        action["destination"]
                            .as_str()
                            .is_some_and(|v| v.eq_ignore_ascii_case(destination))
                            && action["amount"].as_str().and_then(|v| q(v).ok()).as_ref()
                                == Some(&amount)
                    },
                )
                .await?;
                let Some(native_tx) = native_tx else {
                    return Ok(Some(unknown(
                        "No unique explorer transaction proves the exact original withdraw3 action.",
                        json!({"owner":binding.account.owner,"destination":destination,"amount":amount,"nonce_ms":time_ms}),
                    )));
                };
                let ledger_nonce = match time_ms.checked_mul(1000) {
                    Some(value) => value,
                    None => {
                        return Ok(Some(unknown(
                            "The signed withdrawal millisecond nonce cannot be represented in native ledger units.",
                            json!({"nonce_ms":time_ms}),
                        )));
                    }
                };
                let observed_end_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let rows = self
                    .transport
                    .info(Info::UserNonFundingLedgerUpdates {
                        owner: binding.account.execution_address().to_owned(),
                        start_time_ms: time_ms.saturating_sub(60_000),
                        end_time_ms: Some(observed_end_ms),
                    })
                    .await?;
                let native_hash = native_tx["hash"]
                    .as_str()
                    .ok_or_else(|| native_error("Explorer transaction hash is missing."))?;
                let row = ledger_withdrawal(
                    extract_ledger_rows(&rows),
                    ledger_nonce,
                    *time_ms,
                    observed_end_ms,
                );
                let Some(row) = row else {
                    return Ok(Some(unknown(
                        "The native ledger does not uniquely identify the exact signed withdrawal nonce.",
                        json!({"ledger":rows,"native_tx":native_tx}),
                    )));
                };
                let ledger_hash = row["hash"]
                    .as_str()
                    .ok_or_else(|| native_error("Native withdrawal ledger hash is missing."))?;
                let Some((ledger_net, fee)) = withdrawal_ledger_amounts(row, &amount) else {
                    return Ok(Some(unknown(
                        "Native withdrawal net amount and fee do not equal the approved gross debit.",
                        json!({"ledger":row,"native_tx":native_tx,"approved_gross":amount}),
                    )));
                };
                let proof_provider = match self.withdrawal_proof.as_ref() {
                    Some(value) => value.clone(),
                    None => {
                        return Ok(Some(unknown(
                            "No trusted Arbitrum sequencer-inclusion observer is configured.",
                            json!({"native_tx":native_tx,"ledger":row}),
                        )));
                    }
                };
                let request = WithdrawalProofRequest {
                    owner: binding.account.execution_address().to_owned(),
                    destination: destination.clone(),
                    token: arbitrum_usdc().reference,
                    gross_amount: amount.clone(),
                    net_amount: ledger_net.clone(),
                    nonce_ms: *time_ms,
                    ledger_nonce,
                    ledger_hash: ledger_hash.to_owned(),
                    native_tx_hash: native_hash.to_owned(),
                };
                let receipt = match proof_provider
                    .prove_sequenced_arbitrum_withdrawal(&request)
                    .await?
                {
                    Some(value) => value,
                    None => {
                        return Ok(Some(unknown(
                            "The exact native withdrawal has no sequenced Arbitrum USDC receipt.",
                            json!({"request":request}),
                        )));
                    }
                };
                // The Hyperliquid ledger is denominated at Fin's 36-decimal
                // accounting scale; the Arbitrum ERC-20 receipt uses the
                // token's native 6-decimal atoms. Compare human quantities at
                // a common scale, while retaining each effect's exact asset
                // units below.
                let net_hl_scale = q(&receipt.net_amount.to_decimal(6))?;
                if !receipt.sequenced
                    || receipt.block_number == 0
                    || receipt.transaction_hash.len() != 66
                    || !receipt.transaction_hash.starts_with("0x")
                    || !receipt.transaction_hash.as_bytes()[2..]
                        .iter()
                        .all(u8::is_ascii_hexdigit)
                    || receipt.event_digest.is_empty()
                    || !receipt.sender.eq_ignore_ascii_case(BRIDGE2)
                    || !receipt.recipient.eq_ignore_ascii_case(destination)
                    || !receipt
                        .token
                        .eq_ignore_ascii_case(&arbitrum_usdc().reference)
                    || !receipt.transaction_hash.eq_ignore_ascii_case(ledger_hash)
                    || ledger_net != net_hl_scale
                    || fee > exact_decimal_field(&record.input["max_fee_usdc"])?
                {
                    return Ok(Some(unknown(
                        "Native ledger and sequenced Arbitrum receipt do not agree on the original withdrawal.",
                        json!({"ledger":row,"receipt":receipt,"gross":amount,"ledger_net":ledger_net,"fee":fee}),
                    )));
                }
                let from = frozen_cash_resource(
                    &binding.account,
                    binding.account_mode.as_ref(),
                    CashWallet::Perpetual,
                    None,
                )?;
                let to = ResourceId(format!("ethereum:42161:{}", destination.to_lowercase()));
                let effects = vec![
                    FinancialEffect::Transfer {
                        from: from.clone(),
                        to: to.clone(),
                        debit: Amount {
                            asset: usdc(),
                            atoms: amount.clone(),
                        },
                        credit: Amount {
                            asset: arbitrum_usdc(),
                            atoms: receipt.net_amount.clone(),
                        },
                    },
                    FinancialEffect::Cost {
                        location: AccountingLocation::Cash { resource: from },
                        amount: Amount {
                            asset: usdc(),
                            atoms: fee.clone(),
                        },
                        category: "hyperliquid_withdrawal_fee".into(),
                        treatment: CostTreatment::IncludedInDebit,
                    },
                ];
                let evidence = json!({"nonce_ms":time_ms,"ledger_nonce":ledger_nonce,"explorer_action_hash":native_hash,"native_ledger":row,"ledger_hash":ledger_hash,"arbitrum_receipt":receipt,"gross_debit":amount,"ledger_net":ledger_net,"net_credit":receipt.net_amount,"actual_fee":fee});
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: evidence.clone(),
                    verified: Some(verified(submission, effects, evidence)?),
                }))
            }
            Action::UsdSend {
                destination,
                amount_usdc,
                time_ms,
            } => {
                let amount = match positive(amount_usdc) {
                    Ok(value) => value,
                    Err(error) => return Ok(Some(unknown(&error.message, Value::Null))),
                };
                let native_tx = unique_user_signed_action_tx(
                    self,
                    &binding.account.owner,
                    *time_ms,
                    "usdSend",
                    |action| {
                        action["destination"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(destination))
                            && action["amount"]
                                .as_str()
                                .is_some_and(|value| q(value).ok() == q(amount_usdc).ok())
                    },
                )
                .await?;
                let Some(native_tx) = native_tx else {
                    return Ok(Some(unknown(
                        "No unique native transaction proves the exact original Core USDC send nonce.",
                        json!({"owner":binding.account.owner,"destination":destination,"amount":amount,"nonce_ms":time_ms}),
                    )));
                };
                let native_hash = native_tx["hash"]
                    .as_str()
                    .ok_or_else(|| native_error("Explorer transaction hash is missing."))?;
                let start_time_ms = time_ms.saturating_sub(60_000);
                let end_time_ms = Some(chrono::Utc::now().timestamp_millis().max(0) as u64);
                let sender_ledger = self
                    .transport
                    .info(Info::UserNonFundingLedgerUpdates {
                        owner: binding.account.owner.clone(),
                        start_time_ms,
                        end_time_ms,
                    })
                    .await?;
                let recipient_ledger = self
                    .transport
                    .info(Info::UserNonFundingLedgerUpdates {
                        owner: destination.clone(),
                        start_time_ms,
                        end_time_ms: Some(chrono::Utc::now().timestamp_millis().max(0) as u64),
                    })
                    .await?;
                let sender_rows = extract_ledger_rows(&sender_ledger);
                let recipient_rows = extract_ledger_rows(&recipient_ledger);
                let activation_fee = sender_rows.iter().chain(recipient_rows).any(|row| {
                    row["delta"]["type"] == "accountActivationGas"
                        && row["hash"]
                            .as_str()
                            .is_some_and(|hash| hash.eq_ignore_ascii_case(native_hash))
                });
                if activation_fee {
                    return Ok(Some(unknown(
                        "The exact send also incurred an account-activation fee whose asset is outside the approved USDC fee bound.",
                        json!({"native_tx":native_tx,"sender_ledger":sender_ledger,"recipient_ledger":recipient_ledger}),
                    )));
                }
                let sender_row = ledger_internal_transfer(sender_rows, native_hash);
                let recipient_row = ledger_internal_transfer(recipient_rows, native_hash);
                let (Some(sender_row), Some(recipient_row)) = (sender_row, recipient_row) else {
                    return Ok(Some(unknown(
                        "The exact native send has no unique sender and recipient ledger records.",
                        json!({"native_tx":native_tx,"sender_ledger":sender_ledger,"recipient_ledger":recipient_ledger}),
                    )));
                };
                let sender_delta = &sender_row["delta"];
                let recipient_delta = &recipient_row["delta"];
                let actual_amount = sender_delta["usdc"]
                    .as_str()
                    .and_then(|value| q(value).ok());
                let received_amount = recipient_delta["usdc"]
                    .as_str()
                    .and_then(|value| q(value).ok());
                let actual_fee = sender_delta["fee"].as_str().and_then(|value| q(value).ok());
                let recipient_fee = recipient_delta["fee"]
                    .as_str()
                    .and_then(|value| q(value).ok());
                let maximum_fee = match q(record.input["max_fee_usdc"].as_str().unwrap_or("")) {
                    Ok(value) => value,
                    Err(_) => {
                        return Ok(Some(unknown(
                            "The original approved fee bound is missing or invalid.",
                            record.input.clone(),
                        )));
                    }
                };
                if !sender_delta["user"]
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case(&binding.account.owner))
                    || !sender_delta["destination"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(destination))
                    || !recipient_delta["user"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(&binding.account.owner))
                    || !recipient_delta["destination"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(destination))
                    || actual_amount.as_ref() != Some(&amount)
                    || received_amount.as_ref() != Some(&amount)
                    || actual_fee.is_none()
                    || actual_fee != recipient_fee
                    || actual_fee
                        .as_ref()
                        .is_some_and(|value| value > &maximum_fee)
                {
                    return Ok(Some(unknown(
                        "Sender and recipient ledger receipts do not prove the exact requested amount, destination and fee bound.",
                        json!({"native_tx":native_tx,"sender_row":sender_row,"recipient_row":recipient_row,"requested_amount":amount,"maximum_fee":maximum_fee}),
                    )));
                }
                let source = frozen_cash_resource(
                    &binding.account,
                    binding.account_mode.as_ref(),
                    CashWallet::Perpetual,
                    None,
                )?;
                let target = hyperliquid_perpetual_resource(destination);
                let mut effects = vec![FinancialEffect::Transfer {
                    from: source.clone(),
                    to: target,
                    debit: Amount {
                        asset: usdc(),
                        atoms: amount.clone(),
                    },
                    credit: Amount {
                        asset: usdc(),
                        atoms: amount.clone(),
                    },
                }];
                let fee = actual_fee.unwrap();
                if !fee.is_zero() {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash { resource: source },
                        amount: Amount {
                            asset: usdc(),
                            atoms: fee.clone(),
                        },
                        category: "hyperliquid_internal_transfer_fee".into(),
                        treatment: CostTreatment::AdditionalDebit,
                    });
                }
                let evidence = json!({"method":"nonce-and-hash-correlated-two-sided-internal-transfer-ledger","native_tx":native_tx,"sender_row":sender_row,"recipient_row":recipient_row,"transfer_amount":amount,"fee":fee});
                Ok(Some(Reconciliation {
                    status: Status::Completed,
                    evidence: evidence.clone(),
                    verified: Some(verified(submission, effects, evidence)?),
                }))
            }
            _ => Ok(Some(unknown(
                "This action requires an authoritative native ledger, exact position transition, or destination proof not yet present in the observation.",
                json!({"dispatch":record.dispatch_evidence,"action":envelope.action}),
            ))),
        }
    }
}

fn normalized_action(action: &Action, nonce: u64) -> Action {
    let mut action = action.clone();
    match &mut action {
        Action::UsdClassTransfer { nonce: value, .. }
        | Action::ApproveBuilderFee { nonce: value, .. } => *value = nonce,
        Action::UsdSend { time_ms, .. } | Action::Withdraw { time_ms, .. } => *time_ms = nonce,
        _ => {}
    }
    action
}

fn abstraction_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("abstraction").and_then(Value::as_str))
}

fn exact_integer(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
}

fn percent_to_tenths_bps(value: &str) -> Result<u64> {
    let value = value.strip_suffix('%').unwrap_or(value);
    let micros = decimal_to_micro(value)?;
    if micros % 1000 != 0 {
        return Err(FinError::validation(
            "Builder fee must be an exact tenths-of-basis-point value.",
        ));
    }
    Ok(micros / 1000)
}

fn exact_decimal_field(value: &Value) -> Result<Quantity> {
    let text = value
        .as_str()
        .ok_or_else(|| native_error("Exact ledger decimal is missing."))?;
    Quantity::from_decimal(text, 36).map_err(|_| native_error("Ledger decimal is invalid."))
}

fn tenths_bps_percent(value: u16) -> String {
    let whole = value / 1000;
    let fractional = value % 1000;
    if fractional == 0 {
        format!("{whole}%")
    } else {
        format!("{whole}.{fractional:03}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_micro_conversion_is_exact_and_bounded() {
        assert_eq!(decimal_to_micro("12.340001").unwrap(), 12_340_001);
        assert_eq!(decimal_to_micro("1.2300000").unwrap(), 1_230_000);
        assert!(decimal_to_micro("0.0000001").is_err());
        assert!(decimal_to_micro("-1").is_err());
        assert!(decimal_to_micro("1e-6").is_err());
    }

    #[test]
    fn fee_rate_conversion_uses_tenths_of_a_basis_point_exactly() {
        assert_eq!(tenths_bps_percent(1), "0.001%");
        assert_eq!(tenths_bps_percent(10), "0.010%");
        assert_eq!(tenths_bps_percent(1_000), "1%");
        assert_eq!(percent_to_tenths_bps("0.001%").unwrap(), 1);
        assert_eq!(percent_to_tenths_bps("0.010%").unwrap(), 10);
        assert!(percent_to_tenths_bps("0.0001%").is_err());
    }

    #[test]
    fn withdrawal_ledger_match_uses_micro_nonce_and_independent_ledger_hash() {
        let nonce_ms = 1_791_697_542_589u64;
        let ledger_nonce = nonce_ms * 1000;
        let ledger_hash = format!("0x{}", "b6".repeat(32));
        let explorer_action_hash = format!("0x{}", "b5".repeat(32));
        let row = json!({
            "time": 1_791_697_790_712u64,
            "hash": ledger_hash,
            "delta": {"type":"withdraw","usdc":"4.0","nonce":ledger_nonce,"fee":"1.0"}
        });
        let rows = vec![row.clone()];
        assert_eq!(
            ledger_withdrawal(&rows, ledger_nonce, nonce_ms, 1_791_697_900_000),
            Some(&row)
        );
        assert_ne!(row["hash"], explorer_action_hash);
        let (net, fee) = withdrawal_ledger_amounts(&row, &q("5").unwrap()).unwrap();
        assert_eq!(net, q("4").unwrap());
        assert_eq!(fee, q("1").unwrap());
        let zero_fee = json!({
            "time": nonce_ms,
            "hash": format!("0x{}", "b8".repeat(32)),
            "delta": {"type":"withdraw","usdc":"5.0","nonce":ledger_nonce,"fee":"0.0"}
        });
        assert_eq!(
            withdrawal_ledger_amounts(&zero_fee, &q("5").unwrap()).unwrap(),
            (q("5").unwrap(), q("0").unwrap())
        );
        let zero_net = json!({
            "time": nonce_ms,
            "hash": format!("0x{}", "b9".repeat(32)),
            "delta": {"type":"withdraw","usdc":"0.0","nonce":ledger_nonce,"fee":"5.0"}
        });
        assert!(withdrawal_ledger_amounts(&zero_net, &q("5").unwrap()).is_none());

        // The ledger's nonce and net-plus-fee amounts are independently
        // binding; the Explorer transaction hash is not the ledger hash.
        assert!(ledger_withdrawal(&rows, ledger_nonce + 1, nonce_ms, 1_791_697_900_000).is_none());
        assert!(ledger_withdrawal(&rows, ledger_nonce, nonce_ms + 1, 1_791_697_900_000).is_none());
        assert!(ledger_withdrawal(&rows, ledger_nonce, nonce_ms, 1_791_697_700_000).is_none());
        let mut pre_action = row.clone();
        pre_action["time"] = json!(nonce_ms - 1);
        assert!(
            ledger_withdrawal(&[pre_action], ledger_nonce, nonce_ms, 1_791_697_900_000).is_none()
        );
        assert!(withdrawal_ledger_amounts(&row, &q("4").unwrap()).is_none());

        let wrong_fee = json!({
            "time": 1_791_697_790_712u64,
            "hash": format!("0x{}", "b7".repeat(32)),
            "delta": {"type":"withdraw","usdc":"4.0","nonce":ledger_nonce,"fee":"-1.0"}
        });
        assert!(withdrawal_ledger_amounts(&wrong_fee, &q("5").unwrap()).is_none());

        let duplicated = vec![row.clone(), row];
        assert!(
            ledger_withdrawal(&duplicated, ledger_nonce, nonce_ms, 1_791_697_900_000).is_none()
        );

        let mut conflicting = duplicated[0].clone();
        conflicting["hash"] = json!(format!("0x{}", "c8".repeat(32)));
        let rows = vec![duplicated[0].clone(), conflicting];
        assert!(ledger_withdrawal(&rows, ledger_nonce, nonce_ms, 1_791_697_900_000).is_none());
    }

    #[test]
    fn native_action_nonce_requires_an_explicit_signed_action_field() {
        let nonce = 1_700_000_000_000;
        assert!(action_nonce_matches(&json!({"time":nonce}), nonce));
        assert!(action_nonce_matches(&json!({"nonce":nonce}), nonce));
        assert!(!action_nonce_matches(
            &json!({"type":"updateIsolatedMargin"}),
            nonce
        ));
        // Explorer's outer block timestamp is not the signed action nonce.
        let tx = json!({"time":nonce,"action":{"type":"updateIsolatedMargin"}});
        assert!(!action_nonce_matches(&tx["action"], nonce));
    }
}
