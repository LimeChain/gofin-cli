use crate::protocol::*;
use crate::{
    ProtocolSettlementEvent, VerifiedPapertradeCall, verify_canonical_call,
    verify_session_revocation, verify_session_revocation_failure,
};
use async_trait::async_trait;
use chrono::Utc;
use gofin_domain::{
    AccountFundingLimit, AccountingLocation, Adapter, Amount, AssetId, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor,
    FinancialEffect, FinancialIntent, GoFinError, OperationKind, OperationRecord, PositionAnchor,
    PositionKind, PreparedOperation, Reconciliation, ResourceAmount, ResourceId,
    SettlementCategory, SignedQuantity, Status, Submission, SubmissionJournal, VerifiedEffects,
};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::ToPrimitive;
use serde::Deserialize;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;

const OP_TRADING_STATE: &str = "papertrade.trading_state";
const OP_ACCOUNT: &str = "papertrade.account";
const OP_POSITIONS: &str = "papertrade.positions";
const OP_HISTORY: &str = "papertrade.history";
const OP_STAKING: &str = "papertrade.staking";
const OP_LIQUIDATION: &str = "papertrade.liquidation";
const OP_OPEN: &str = "papertrade.open";
const OP_CLOSE: &str = "papertrade.close";
const OP_CANCEL: &str = "papertrade.cancel";
const OP_REGISTER: &str = "papertrade.session_register";
const OP_REVOKE: &str = "papertrade.session_revoke";
const OP_STAKE: &str = "papertrade.stake";
const OP_UNSTAKE: &str = "papertrade.unstake";
const OP_CLAIM: &str = "papertrade.claim";
const OP_DEPOSIT: &str = "papertrade.deposit";
const OP_WITHDRAW: &str = "papertrade.withdraw";
const OP_FUNDING_CONTINUE: &str = "papertrade.funding_continue";
const OP_INVALIDATE_CORE_NONCE: &str = "papertrade.deposit.invalidate_nonce";

fn err(code: &str, stage: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        stage,
        message,
        "Inspect the Papertrade account and reconcile any durable submission before retrying.",
    )
}
fn paper_wallet_resource(owner: &str) -> ResourceId {
    ResourceId(format!(
        "papertrade:mainnet:{}:paper_wallet",
        owner.to_ascii_lowercase()
    ))
}
fn paper_staked_resource(owner: &str) -> ResourceId {
    ResourceId(format!(
        "papertrade:mainnet:{}:paper_staked",
        owner.to_ascii_lowercase()
    ))
}
fn position_record(open_intent_id: &str) -> String {
    format!(
        "papertrade:position:{}",
        open_intent_id.to_ascii_lowercase()
    )
}
fn position_resource(owner: &str) -> ResourceId {
    ResourceId(format!(
        "papertrade:mainnet:{}:positions",
        owner.to_ascii_lowercase()
    ))
}
fn payout_queue_resource(owner: &str) -> ResourceId {
    ResourceId(format!(
        "papertrade:mainnet:{}:payout_queue",
        owner.to_ascii_lowercase()
    ))
}
fn valid_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone)]
pub struct PapertradeAdapter<R> {
    runtime: Arc<R>,
    account: PapertradeAccount,
}

impl<R> PapertradeAdapter<R> {
    pub fn new(runtime: Arc<R>, account: PapertradeAccount) -> Result<Self, GoFinError> {
        account.validate()?;
        Ok(Self { runtime, account })
    }
    pub fn account(&self) -> &PapertradeAccount {
        &self.account
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInput {
    instrument_id: u32,
    is_long: bool,
    /// Human decimal string. All conversion to wad is exact; JSON floating-point
    /// numbers are deliberately rejected.
    margin: String,
    leverage: u32,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseInput {
    position_ids: Vec<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    intent_id: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiquidationInput {
    position_id: String,
    #[serde(default)]
    since_block: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PositionSnapshot {
    position_id: String,
    instrument_id: u64,
    is_long: bool,
    use_debt: bool,
    entry_price_raw: String,
    margin_raw: String,
    leverage: u64,
    bust_price_raw: String,
    opened_at_ms: u64,
    intent_id: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PositionsSnapshot {
    owner: String,
    indexed_through_block: String,
    positions: Vec<PositionSnapshot>,
}

fn position_snapshot_key(row: &PositionSnapshot) -> String {
    row.intent_id
        .as_deref()
        .map(position_record)
        .unwrap_or_else(|| format!("papertrade:position_id:{}", row.position_id))
}

fn position_snapshot_anchors(
    owner: &str,
    rows: &[PositionSnapshot],
    block: u64,
    observed_at: &str,
) -> Result<Vec<PositionAnchor>, GoFinError> {
    let resource = position_resource(owner);
    let asset = trading_usdc_asset();
    let mut anchors = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        let margin = row
            .margin_raw
            .parse::<u128>()
            .map_err(|_| GoFinError::validation("Selected position margin is malformed."))?;
        let notional = margin
            .checked_mul(u128::from(row.leverage))
            .ok_or_else(|| {
                GoFinError::validation("Selected position notional exceeds exact range.")
            })?;
        let position = position_snapshot_key(row);
        let (margin_kind, exposure_kind) = if row.is_long {
            (PositionKind::MarginLong, PositionKind::DerivativeLong)
        } else {
            (PositionKind::MarginShort, PositionKind::DerivativeShort)
        };
        let evidence = |kind: &str| {
            json!({
                "protocol":"papertrade",
                "source":"authenticated owner position snapshot",
                "owner":owner.to_ascii_lowercase(),
                "indexed_through_block":block,
                "position_id":row.position_id,
                "opening_intent_id":row.intent_id,
                "kind":kind
            })
        };
        for (kind, amount) in [(margin_kind, margin), (exposure_kind, notional)] {
            let is_margin = kind.is_margin();
            anchors.push(PositionAnchor {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: kind,
                principal: Amount {
                    asset: asset.clone(),
                    atoms: gofin_domain::Quantity::new(amount.to_string())?,
                },
                scaled_receipt: None,
                evidence_version: 1,
                evidence: evidence(if is_margin { "margin" } else { "exposure" }),
                observed_at: observed_at.to_owned(),
            });
        }
    }
    Ok(anchors)
}

/// Reconstruct only the claim principal produced by this exact close batch.
/// Prior queue state is intentionally not read: canonical PositionsClosed
/// rows establish per-position entitlement, while the pinned aggregate cash
/// event establishes the amount actually credited.
fn close_cash_entitlement(
    events: &[ProtocolSettlementEvent],
    rows: &[PositionSnapshot],
) -> Result<u128, GoFinError> {
    let mut total = 0u128;
    for row in rows {
        let Some(ProtocolSettlementEvent::PositionClosed { raw_data, .. }) = events.iter().find(
            |event| matches!(event, ProtocolSettlementEvent::PositionClosed { position_id, .. } if position_id == &row.position_id),
        ) else {
            // Liquidations forfeit the selected isolated margin; they do not
            // create ordinary-close cash entitlement.
            continue;
        };
        let data = hex::decode(raw_data.strip_prefix("0x").unwrap_or(raw_data)).map_err(|_| {
            err(
                "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                "reconciliation",
                "Canonical PositionClosed data is not valid hex.",
            )
        })?;
        if data.len() != 160 {
            return Err(err(
                "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                "reconciliation",
                "Canonical PositionClosed data has an unexpected reviewed ABI layout.",
            ));
        }
        let adjusted = BigInt::from_signed_bytes_be(&data[64..96]);
        let margin = BigInt::from_str(&row.margin_raw).map_err(|_| {
            err(
                "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
                "reconciliation",
                "Selected position margin is malformed.",
            )
        })?;
        let mut entitlement = margin + &adjusted;
        if entitlement.sign() == Sign::Plus && adjusted.sign() == Sign::Plus {
            let fee = BigUint::from_bytes_be(&data[128..160]);
            entitlement -= BigInt::from(fee);
        }
        if entitlement.sign() != Sign::Plus {
            continue;
        }
        let value = entitlement.to_u128().ok_or_else(|| {
            err(
                "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                "reconciliation",
                "Selected close entitlement exceeds exact supported range.",
            )
        })?;
        total = total.checked_add(value).ok_or_else(|| {
            err(
                "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                "reconciliation",
                "Selected close entitlement total exceeds exact supported range.",
            )
        })?;
    }
    Ok(total)
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterInput {
    registration_id: String,
    /// Absolute Unix expiry, bounded by the 30-day protocol maximum.
    expiry: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInput {
    registration_id: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaperAmountInput {
    amount: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryInput {
    #[serde(default)]
    since_ms: Option<u64>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingInput {
    route: FundingRoute,
    amount: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreNonceRecoveryInput {
    parent_operation_id: String,
}

fn parse_input<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, GoFinError> {
    serde_json::from_value(input.clone())
        .map_err(|_| GoFinError::validation("Papertrade operation input has an invalid shape."))
}

fn select_close_positions(
    snapshot: Value,
    owner: &str,
    requested_ids: &[String],
) -> Result<Vec<PositionSnapshot>, GoFinError> {
    let snapshot: PositionsSnapshot = serde_json::from_value(snapshot).map_err(|_| {
        err(
            "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
            "preview",
            "The account position snapshot has an invalid protocol shape.",
        )
    })?;
    if !snapshot.owner.eq_ignore_ascii_case(owner)
        || snapshot.indexed_through_block.parse::<u64>().is_err()
    {
        return Err(err(
            "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
            "preview",
            "The position snapshot owner or indexed block is invalid.",
        ));
    }
    let mut selected = Vec::with_capacity(requested_ids.len());
    for id in requested_ids {
        let mut rows = snapshot
            .positions
            .iter()
            .filter(|position| position.position_id == *id);
        let position = rows.next().ok_or_else(|| {
            err(
                "PAPERTRADE_POSITION_NOT_FOUND",
                "preview",
                "A selected position is not present in the account snapshot.",
            )
        })?;
        if rows.next().is_some()
            || position.use_debt
            || position.instrument_id > 1
            || position.margin_raw.parse::<u128>().is_err()
            || position.entry_price_raw.parse::<u128>().is_err()
            || position.bust_price_raw.parse::<u128>().is_err()
            || position.leverage == 0
            || position.leverage > u64::from(u32::MAX)
            || position.intent_id.as_deref().is_some_and(|intent_id| {
                !intent_id.starts_with("0x")
                    || intent_id.len() != 66
                    || !intent_id[2..].bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err(err(
                "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
                "preview",
                "A selected position has invalid or unsupported cash-only accounting fields.",
            ));
        }
        selected.push(position.clone());
    }
    Ok(selected)
}
fn object_schema(fields: Value) -> Value {
    let required = fields
        .as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    json!({"type":"object","properties":fields,"required":required,"additionalProperties":false})
}
fn object_schema_with_required(fields: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": fields,
        "required": required,
        "additionalProperties": false
    })
}
fn definition(
    name: &str,
    kind: OperationKind,
    description: &str,
    input_schema: Value,
) -> Definition {
    let execution_actor = if matches!(&kind, OperationKind::Read) {
        ExecutionActor::AgentReadable
    } else {
        ExecutionActor::AgentInvocable
    };
    Definition {
        name: name.into(),
        description: description.into(),
        kind,
        execution_actor,
        input_schema,
        output_schema: json!({"type":"object"}),
        error_schema: json!({"type":"object"}),
        examples: Vec::new(),
        coverage: "Official Papertrade HyperEVM BTC/ETH and PAPER staking operations only.".into(),
        completion: "A successful submission is not settlement; canonical receipt reconciliation is required.".into(),
        semantics: json!({"network":"ethereum:999","chain_id":999}),
    }
}
fn read_definitions() -> Vec<Definition> {
    let empty = object_schema(json!({}));
    vec![
        {
            let mut trading = definition(
                OP_TRADING_STATE,
                OperationKind::Read,
                "Read Papertrade markets, trading limits, and the current minimum PAPER stake.",
                empty.clone(),
            );
            trading.semantics["minimum_paper_stake_raw"] =
                json!("intakeControls[1], exact 18-decimal raw units");
            trading
        },
        definition(
            OP_ACCOUNT,
            OperationKind::Read,
            "Read the enrolled Papertrade account.",
            empty.clone(),
        ),
        definition(
            OP_POSITIONS,
            OperationKind::Read,
            "Read the enrolled account's positions.",
            empty.clone(),
        ),
        definition(
            OP_HISTORY,
            OperationKind::Read,
            "Read the enrolled account's recent trade history.",
            object_schema_with_required(json!({"since_ms":{"type":"integer","minimum":0}}), &[]),
        ),
        definition(
            OP_STAKING,
            OperationKind::Read,
            "Read PAPER staking and reward state.",
            empty,
        ),
        definition(
            OP_LIQUIDATION,
            OperationKind::Read,
            "Verify a canonical keeper liquidation and its exact position-correlated PAPER award.",
            object_schema_with_required(
                json!({"position_id":{"type":"string","pattern":"^[0-9]+$"},"since_block":{"type":"integer","minimum":0,"description":"Optional canonical search lower bound. If omitted, the protected runtime uses its bounded recent-history window."}}),
                &["position_id"],
            ),
        ),
    ]
}
fn write_definitions() -> Vec<Definition> {
    let mut definitions = vec![
        definition(
            OP_OPEN,
            OperationKind::Write,
            "Open a BTC or ETH position at the protocol's execution-time market price using internal USDC cash only.",
            object_schema(
                json!({"instrument_id":{"type":"integer","enum":[0,1]},"is_long":{"type":"boolean"},"margin":{"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"},"leverage":{"type":"integer","minimum":1}}),
            ),
        ),
        definition(
            OP_CLOSE,
            OperationKind::Write,
            "Fully close the exact selected Papertrade positions.",
            object_schema(
                json!({"position_ids":{"type":"array","items":{"type":"string","pattern":"^[0-9]+$"},"minItems":1,"maxItems":12}}),
            ),
        ),
        definition(
            OP_CANCEL,
            OperationKind::Write,
            "Cancel an exact pending Papertrade intent; completion requires owner-scoped terminal cancellation evidence.",
            object_schema(json!({"intent_id":{"type":"string","pattern":"^0x[0-9a-fA-F]{64}$"}})),
        ),
        definition(
            OP_STAKE,
            OperationKind::Write,
            "Stake exact PAPER from the enrolled wallet.",
            object_schema(json!({"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"}})),
        ),
        definition(
            OP_UNSTAKE,
            OperationKind::Write,
            "Instantly unstake exact PAPER to the enrolled wallet.",
            object_schema(json!({"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"}})),
        ),
        definition(
            OP_CLAIM,
            OperationKind::Write,
            "Claim available staking rewards into Papertrade Exchange trading USDC.",
            object_schema(json!({})),
        ),
        {
            let mut deposit = definition(
                OP_DEPOSIT,
                OperationKind::Write,
                "Deposit exact six-decimal USDC to Papertrade through a supported fixed route.",
                object_schema(
                    json!({"route":{"type":"string","enum":["core","hyperevm","ethereum","arbitrum","solana"]},"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"}}),
                ),
            );
            deposit.coverage = "USDC deposits from HyperCore, HyperEVM, Ethereum, Arbitrum One, and Solana through native HyperCore/HyperEVM routes or fixed CCTP V2 routes.".into();
            deposit
        },
        {
            let mut withdraw = definition(
                OP_WITHDRAW,
                OperationKind::Write,
                "Withdraw exact six-decimal Papertrade USDC to the enrolled owner through a supported fixed route.",
                object_schema(
                    json!({"route":{"type":"string","enum":["core","hyperevm","ethereum","arbitrum","solana"]},"amount":{"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"}}),
                ),
            );
            withdraw.coverage = "Stage one withdraws to the owner's HyperCore spot USDC; Core withdrawals finish there, while external destinations require separate authorization through papertrade.funding_continue.".into();
            withdraw
        },
        {
            let mut continuation = definition(
                OP_FUNDING_CONTINUE,
                OperationKind::Write,
                "Continue a previously settled Papertrade withdrawal from its exact HyperCore USDC credit to the approved external destination.",
                object_schema(json!({"route_id":{"type":"string","minLength":1}})),
            );
            continuation.coverage = "Separately authorized HyperCore onward transfers to HyperEVM, Ethereum, Arbitrum One, and Solana using fixed native or CCTP V2 routes; HyperEVM requires canonical system-transaction receipt RPC support.".into();
            continuation
        },
    ];
    let mut recovery = definition(
        OP_INVALIDATE_CORE_NONCE,
        OperationKind::Write,
        "Invalidate the exact unresolved Hyperliquid Core nonce for a Papertrade deposit through its protected parent operation.",
        object_schema_with_required(
            json!({"parent_operation_id":{"type":"string","minLength":1}}),
            &["parent_operation_id"],
        ),
    );
    recovery.semantics["recovery_child"] = json!({"version":1});
    definitions.push(recovery);
    definitions
}

fn configuration_definitions() -> Vec<Definition> {
    vec![
        {
            let mut d = definition(
                OP_REGISTER,
                OperationKind::Configuration,
                "Register a broker-generated Papertrade session key for at most 30 days.",
                object_schema(
                    json!({"registration_id":{"type":"string","minLength":1},"expiry":{"type":"integer","minimum":1}}),
                ),
            );
            d.execution_actor = ExecutionActor::HumanInteractive;
            d
        },
        {
            let mut d = definition(
                OP_REVOKE,
                OperationKind::Configuration,
                "Revoke an enrolled Papertrade session key.",
                object_schema(json!({"registration_id":{"type":"string","minLength":1}})),
            );
            d.execution_actor = ExecutionActor::HumanInteractive;
            d
        },
    ]
}

pub fn operation_definitions() -> Vec<Definition> {
    read_definitions()
        .into_iter()
        .chain(write_definitions())
        .chain(configuration_definitions())
        .collect()
}

fn intent_request(
    operation: &str,
    owner: &str,
    input: &Value,
) -> Result<IntentRequest, GoFinError> {
    let owner = owner.to_ascii_lowercase();
    match operation {
        OP_OPEN => {
            let parsed: OpenInput = parse_input(input)?;
            if !matches!(parsed.instrument_id, 0 | 1) {
                return Err(GoFinError::validation(
                    "Papertrade supports BTC (0) and ETH (1) only.",
                ));
            }
            Ok(IntentRequest::Open {
                owner,
                instrument_id: parsed.instrument_id,
                is_long: parsed.is_long,
                margin_wad: wad_from_decimal(&parsed.margin, 18)?,
                leverage: parsed.leverage,
            })
        }
        OP_CLOSE => {
            let parsed: CloseInput = parse_input(input)?;
            Ok(IntentRequest::Close {
                owner,
                position_ids: parsed.position_ids,
            })
        }
        OP_CANCEL => {
            let parsed: CancelInput = parse_input(input)?;
            Ok(IntentRequest::Cancel {
                owner,
                intent_id: parsed.intent_id,
            })
        }
        OP_REGISTER => {
            let parsed: RegisterInput = parse_input(input)?;
            Ok(IntentRequest::RegisterSession {
                owner,
                registration_id: parsed.registration_id,
                expiry: parsed.expiry,
            })
        }
        OP_REVOKE => {
            let parsed: SessionInput = parse_input(input)?;
            Ok(IntentRequest::RevokeSession {
                owner,
                registration_id: parsed.registration_id,
            })
        }
        OP_STAKE | OP_UNSTAKE => {
            let parsed: PaperAmountInput = parse_input(input)?;
            let amount = wad_from_decimal(&parsed.amount, PAPER_DECIMALS)?;
            if operation == OP_STAKE {
                Ok(IntentRequest::Stake { owner, amount })
            } else {
                Ok(IntentRequest::Unstake { owner, amount })
            }
        }
        OP_CLAIM => {
            parse_input::<serde_json::Map<String, Value>>(input)?;
            if input.as_object().is_some_and(|o| o.is_empty()) {
                Ok(IntentRequest::Claim { owner })
            } else {
                Err(GoFinError::validation("Claim takes no parameters."))
            }
        }
        _ => Err(GoFinError::validation(
            "Unsupported Papertrade write operation.",
        )),
    }
}

fn funding_intent(
    operation: &str,
    owner: &str,
    input: &Value,
    operation_id: &str,
) -> Result<FundingIntent, GoFinError> {
    let parsed: FundingInput = parse_input(input)?;
    let amount = wad_from_decimal(&parsed.amount, USDC_DECIMALS)?;
    let amount_usdc_atoms = u64::try_from(amount)
        .map_err(|_| GoFinError::validation("USDC amount exceeds the supported exact range."))?;
    if amount_usdc_atoms == 0 {
        return Err(GoFinError::validation("USDC amount must be positive."));
    }
    Ok(FundingIntent {
        direction: if operation == OP_DEPOSIT {
            FundingDirection::Deposit
        } else {
            FundingDirection::Withdraw
        },
        route: parsed.route,
        owner: owner.to_ascii_lowercase(),
        amount_usdc_atoms,
        request_id: operation_id.to_owned(),
    })
}

async fn prepare_core_nonce_recovery<R: PapertradeRuntime + 'static>(
    adapter: &PapertradeAdapter<R>,
    input: &Value,
    context: &gofin_domain::PreparationContext,
) -> Result<PreparedOperation, GoFinError> {
    let request: CoreNonceRecoveryInput = parse_input(input)?;
    let child_operation_id = context
        .logical_operation_id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let binding = adapter
        .runtime
        .prepare_funding_nonce_recovery(&request.parent_operation_id, &child_operation_id)
        .await?;
    binding.validate()?;
    if binding.child_operation_id != child_operation_id
        || binding.parent_operation_id != request.parent_operation_id
        || binding.parent_submission.reference != binding.route_id
        || binding
            .parent_submission
            .data
            .get("operation_id")
            .and_then(Value::as_str)
            != Some(binding.route_id.as_str())
        || !binding.owner.eq_ignore_ascii_case(&adapter.account.owner)
        || binding.parent_submission.kind != "papertrade_funding"
    {
        return Err(err(
            "PAPERTRADE_NONCE_RECOVERY_BINDING_INVALID",
            "preview",
            "The protected runtime returned a recovery binding for a different parent, owner, or Core route.",
        ));
    }
    let recovery = gofin_domain::RecoveryBinding {
        version: 1,
        parent_operation_id: binding.parent_operation_id.clone(),
        parent_submission: binding.parent_submission.clone(),
    };
    let mut prepared_binding = json!({"core_deposit_nonce_recovery":binding});
    prepared_binding[gofin_domain::RECOVERY_METADATA_KEY] = serde_json::to_value(recovery)
        .map_err(|_| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_BINDING_INVALID",
                "preview",
                "The protected parent link could not be encoded.",
            )
        })?;
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let owner = adapter.account.owner.to_ascii_lowercase();
    let mut intent = FinancialIntent::default();
    intent.locks.push(ResourceId(format!(
        "hyperliquid:mainnet:{owner}:nonce_recovery:{}",
        request.parent_operation_id
    )));
    Ok(PreparedOperation {
        operation: OP_INVALIDATE_CORE_NONCE.into(),
        input: input.clone(),
        binding: prepared_binding,
        summary: format!(
            "Invalidate the protected Core deposit nonce for unresolved operation {}.",
            request.parent_operation_id
        ),
        checks: vec![Check {
            name: "core_nonce_recovery_parent".into(),
            status: CheckStatus::Passed,
            detail: "Parent operation, owner and Core route resolved from the protected funding journal; no nonce is caller supplied.".into(),
        }],
        warnings: vec![
            "Recovery sends only the exact protected Noop for the original Core deposit nonce; uncertain submissions are reconciled without replay.".into(),
        ],
        observed_at: now_iso.clone(),
        expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
        valuation: Some(gofin_domain::Valuation {
            movement_usd_micros: gofin_domain::Quantity::from_u64(0),
            maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
            additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
            observed_at: now_iso,
            source: "hyperliquid:core-deposit-nonce-recovery-noop:v1".into(),
            prices: vec![],
        }),
        intent,
        operation_version: 1,
        action_deadline: None,
    })
}

fn core_nonce_recovery_binding(
    prepared: &PreparedOperation,
) -> Result<CoreDepositNonceRecoveryBinding, GoFinError> {
    let binding: CoreDepositNonceRecoveryBinding = serde_json::from_value(
        prepared
            .binding
            .get("core_deposit_nonce_recovery")
            .cloned()
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_NONCE_RECOVERY_BINDING_MISSING",
                    "validation",
                    "The protected Core-deposit recovery identity is missing.",
                )
            })?,
    )
    .map_err(|_| {
        err(
            "PAPERTRADE_NONCE_RECOVERY_BINDING_INVALID",
            "validation",
            "The protected Core-deposit recovery identity is malformed.",
        )
    })?;
    binding.validate().map_err(|_| {
        err(
            "PAPERTRADE_NONCE_RECOVERY_BINDING_INVALID",
            "validation",
            "The protected Core-deposit recovery identity is incomplete.",
        )
    })?;
    Ok(binding)
}

fn core_nonce_recovery_submission(binding: &CoreDepositNonceRecoveryBinding) -> Submission {
    Submission {
        kind: "papertrade_core_deposit_nonce_recovery".into(),
        reference: format!("{}:{}", binding.route_id, binding.child_operation_id),
        data: json!({
            "parent_operation_id":binding.parent_operation_id,
            "route_id":binding.route_id,
            "owner":binding.owner.to_ascii_lowercase(),
            "plan_fingerprint":binding.plan_fingerprint,
        }),
    }
}

fn unknown_recovery(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    }
}

fn bound(prepared: &PreparedOperation) -> Result<BoundIntent, GoFinError> {
    let value = prepared.binding.get("bound_intent").ok_or_else(|| {
        err(
            "PAPERTRADE_BINDING_MISSING",
            "validation",
            "Prepared Papertrade intent binding is missing.",
        )
    })?;
    serde_json::from_value(value.clone()).map_err(|_| {
        err(
            "PAPERTRADE_BINDING_INVALID",
            "validation",
            "Prepared Papertrade intent binding is invalid.",
        )
    })
}
fn operation_submission(bound: &BoundIntent) -> Submission {
    Submission {
        kind: "papertrade_intent".into(),
        reference: bound.intent_id.clone(),
        data: json!({"intent_id":bound.intent_id,"action":bound.action,"registration_id":bound.registration_id}),
    }
}

fn funding_submission(binding: &FundingBinding) -> Submission {
    Submission {
        kind: "papertrade_funding".into(),
        reference: binding.operation_id.clone(),
        data: json!({
            "operation_id": binding.operation_id,
            "request_id": binding.intent.request_id,
            "direction": binding.intent.direction,
            "route": binding.intent.route,
            "amount_usdc_atoms": binding.intent.amount_usdc_atoms
        }),
    }
}

fn funding_route_identity(route: &FundingRoute) -> &'static str {
    match route {
        FundingRoute::Core => "hyperliquid:mainnet",
        FundingRoute::Hyperevm => "ethereum:999",
        FundingRoute::Ethereum => "ethereum:1",
        FundingRoute::Arbitrum => "ethereum:42161",
        FundingRoute::Solana => "solana:mainnet-beta",
    }
}

fn route_usdc_asset(route: &FundingRoute) -> AssetId {
    let (network, reference, decimals) = match route {
        // Hyperliquid reads normalize every balance to scale-36 atoms. Its
        // human-facing transfer amount remains six-decimal USDC at the route
        // boundary and is converted explicitly when building the effect.
        FundingRoute::Core => ("hyperliquid:mainnet", "USDC", 36),
        FundingRoute::Hyperevm => (
            "ethereum:999",
            "0xb88339cb7199b77e23db6e890353e22632ba630f",
            6,
        ),
        FundingRoute::Ethereum => (
            "ethereum:1",
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            6,
        ),
        FundingRoute::Arbitrum => (
            "ethereum:42161",
            "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
            6,
        ),
        FundingRoute::Solana => (
            "solana:mainnet-beta",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            6,
        ),
    };
    AssetId {
        network: network.into(),
        reference: reference.into(),
        decimals,
    }
}

fn route_atoms(route: &FundingRoute, atoms: u64) -> Result<gofin_domain::Quantity, GoFinError> {
    // Hyperliquid Core exposes USDC at 6 decimals in funding APIs, while the
    // existing profile's canonical USDC ledger uses a 36-decimal scale.
    // Appending the exact scale delta avoids narrowing a valid u64 atom amount
    // through u128 (which would cap ordinary funding amounts around $340).
    let scaled = if matches!(route, FundingRoute::Core) {
        format!("{}{}", atoms, "0".repeat(30))
    } else {
        atoms.to_string()
    };
    gofin_domain::Quantity::new(scaled)
}

fn format_usdc_atoms(atoms: u64) -> String {
    gofin_domain::Quantity::new(atoms.to_string())
        .expect("u64 is a valid exact quantity")
        .to_decimal(6)
}

fn native_fee_asset(route: &FundingRoute, declared: &str) -> Result<AssetId, GoFinError> {
    let (network, reference, decimals) = match route {
        FundingRoute::Core => ("hyperliquid:mainnet", "HYPE", 36),
        FundingRoute::Hyperevm => ("ethereum:999", "native", 18),
        FundingRoute::Ethereum => ("ethereum:1", "native", 18),
        FundingRoute::Arbitrum => ("ethereum:42161", "native", 18),
        FundingRoute::Solana => ("solana:mainnet-beta", "native", 9),
    };
    let expected = format!("{network}:{reference}");
    if declared != expected {
        return Err(GoFinError::validation(
            "Papertrade funding native fee identity does not match the fixed route.",
        ));
    }
    Ok(AssetId {
        network: network.into(),
        reference: reference.into(),
        decimals,
    })
}

/// Converts the route's native fee wire unit into the canonical ledger unit.
/// Hyperliquid's Core action reports HYPE fees in 8 decimals while the
/// canonical account asset is represented at 36 decimals.
fn native_fee_quantity(
    route: &FundingRoute,
    raw_atoms: &str,
) -> Result<gofin_domain::Quantity, GoFinError> {
    if raw_atoms.is_empty() || !raw_atoms.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GoFinError::validation("Native fee quantity is malformed."));
    }
    let scaled = if matches!(route, FundingRoute::Core) {
        format!("{raw_atoms}{}", "0".repeat(28))
    } else {
        raw_atoms.to_owned()
    };
    gofin_domain::Quantity::new(scaled)
}

fn valid_evm_account_identity(route: &FundingRoute, identity: &str, owner: &str) -> bool {
    identity
        == format!(
            "{}:{}",
            funding_route_identity(route),
            owner.to_ascii_lowercase()
        )
}

fn valid_solana_account_identity(identity: &str) -> bool {
    let Some(account) = identity.strip_prefix("solana:mainnet-beta:") else {
        return false;
    };
    (32..=44).contains(&account.len())
        && account.bytes().all(|byte| {
            b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains(&byte)
        })
}

fn valid_funding_identities(
    intent: &FundingIntent,
    source_identity: &str,
    destination_identity: &str,
) -> bool {
    let owner = intent.owner.to_ascii_lowercase();
    let papertrade = format!("papertrade:mainnet:{owner}");
    let route_account_valid = |identity: &str| match intent.route {
        FundingRoute::Core => identity == format!("hyperliquid:mainnet:{owner}:spot:primary"),
        FundingRoute::Hyperevm | FundingRoute::Ethereum | FundingRoute::Arbitrum => {
            valid_evm_account_identity(&intent.route, identity, &owner)
        }
        // Solana is an independently enrolled account. Its base58 identity
        // comes from the protected broker plan and must not be derived from
        // the EVM Papertrade owner.
        FundingRoute::Solana => valid_solana_account_identity(identity),
    };
    match intent.direction {
        FundingDirection::Deposit => {
            route_account_valid(source_identity) && destination_identity == papertrade
        }
        FundingDirection::Withdraw => {
            source_identity == papertrade && route_account_valid(destination_identity)
        }
    }
}

fn funding_identities_match(intent: &FundingIntent, receipt: &FundingReceipt) -> bool {
    if receipt.stage == FundingReceiptStage::CoreCredit {
        let owner = intent.owner.to_ascii_lowercase();
        return intent.direction == FundingDirection::Withdraw
            && intent.route != FundingRoute::Core
            && receipt.source_identity == format!("papertrade:mainnet:{owner}")
            && receipt.destination_identity == format!("hyperliquid:mainnet:{owner}:spot:primary");
    }
    valid_funding_identities(
        intent,
        &receipt.source_identity,
        &receipt.destination_identity,
    )
}

fn funding_receipt_stage_matches(intent: &FundingIntent, receipt: &FundingReceipt) -> bool {
    match receipt.stage {
        FundingReceiptStage::CoreCredit => {
            intent.direction == FundingDirection::Withdraw
                && intent.route != FundingRoute::Core
                && receipt.amount_usdc_atoms == intent.amount_usdc_atoms
                && receipt.source_debit_usdc_atoms == intent.amount_usdc_atoms
                && receipt.destination_credit_usdc_atoms == intent.amount_usdc_atoms
                && receipt.actual_source_fee_usdc_atoms == 0
                && receipt.actual_cctp_fee_usdc_atoms == 0
                && receipt.activation_fee_usdc_atoms == 0
                && receipt.retained_source_credit_usdc_atoms == 0
                && receipt.actual_native_fee_atoms.is_none()
                && receipt.actual_native_fee_asset.is_none()
                && receipt.retained_source_native_atoms.is_none()
                && receipt.retained_source_native_asset.is_none()
        }
        FundingReceiptStage::FinalDestination => {
            intent.route == FundingRoute::Core || intent.direction == FundingDirection::Deposit
        }
    }
}

fn solana_message_sent_position(binding: &FundingBinding) -> String {
    format!(
        "papertrade-cctp-message-sent:papertrade-funding:{}:0",
        binding.operation_id
    )
}

fn funding_retained_native_matches(binding: &FundingBinding, receipt: &FundingReceipt) -> bool {
    let (Some(refundable_cap), Some(total_cap), Some(native_asset), Some(fee_cap)) = (
        binding
            .bounds
            .maximum_refundable_native_funding_atoms
            .as_deref(),
        binding.bounds.maximum_native_source_debit_atoms.as_deref(),
        binding.bounds.native_fee_asset.as_deref(),
        binding.bounds.maximum_native_fee_atoms.as_deref(),
    ) else {
        return receipt.retained_source_native_atoms.is_none()
            && receipt.retained_source_native_asset.is_none();
    };
    if binding.intent.route != FundingRoute::Solana
        || binding.intent.direction != FundingDirection::Deposit
        || binding
            .solana_message_event_account
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return false;
    }
    let (Some(retained_raw), Some(retained_asset), Some(fee_raw), Some(fee_asset)) = (
        receipt.retained_source_native_atoms.as_deref(),
        receipt.retained_source_native_asset.as_deref(),
        receipt.actual_native_fee_atoms.as_deref(),
        receipt.actual_native_fee_asset.as_deref(),
    ) else {
        return false;
    };
    if retained_asset != native_asset || fee_asset != native_asset {
        return false;
    }
    let parsed = (|| {
        let retained = retained_raw.parse::<u128>().ok()?;
        let fee = fee_raw.parse::<u128>().ok()?;
        let max_retained = refundable_cap.parse::<u128>().ok()?;
        let max_fee = fee_cap.parse::<u128>().ok()?;
        let max_total = total_cap.parse::<u128>().ok()?;
        Some((retained, fee, max_retained, max_fee, max_total))
    })();
    let Some((retained, fee, max_retained, max_fee, max_total)) = parsed else {
        return false;
    };
    retained > 0
        && retained <= max_retained
        && fee <= max_fee
        && fee
            .checked_add(retained)
            .is_some_and(|sum| sum <= max_total)
}

fn refundable_native_funding_effects(
    binding: &FundingBinding,
    receipt: &FundingReceipt,
    source_resource: &ResourceId,
) -> Result<Vec<FinancialEffect>, GoFinError> {
    let (Some(retained_raw), Some(asset_id)) = (
        receipt.retained_source_native_atoms.as_deref(),
        receipt.retained_source_native_asset.as_deref(),
    ) else {
        return Ok(Vec::new());
    };
    let declared = binding
        .bounds
        .native_fee_asset
        .as_deref()
        .ok_or_else(|| GoFinError::validation("Unexpected retained native source asset."))?;
    if asset_id != declared || !funding_retained_native_matches(binding, receipt) {
        return Err(GoFinError::validation(
            "Retained native source funding differs from the exact approved route bounds.",
        ));
    }
    let retained = gofin_domain::Quantity::new(retained_raw)?;
    let rent_position = solana_message_sent_position(binding);
    let rent_asset = native_fee_asset(&binding.intent.route, declared)?;
    let rent_account = binding
        .solana_message_event_account
        .as_deref()
        .ok_or_else(|| {
            GoFinError::validation(
                "Solana MessageSent account is missing from the funding binding.",
            )
        })?;
    let anchor = PositionAnchor {
        resource: source_resource.clone(),
        position: rent_position.clone(),
        position_kind: PositionKind::RefundableAccountFunding,
        principal: Amount {
            asset: rent_asset.clone(),
            atoms: retained.clone(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({
            "protocol":"circle_cctp_v2",
            "message_sent_event_account":rent_account,
            "transaction_evidence":receipt.evidence_id,
            "retained_lamports":retained.as_str(),
        }),
        observed_at: Utc::now().to_rfc3339(),
    };
    Ok(vec![
        FinancialEffect::CashDebit {
            resource: source_resource.clone(),
            amount: Amount {
                asset: rent_asset.clone(),
                atoms: retained.clone(),
            },
        },
        FinancialEffect::PositionChange {
            resource: source_resource.clone(),
            position: rent_position.clone(),
            asset: rent_asset.clone(),
            delta: SignedQuantity::positive(&retained),
            scaled_receipt: None,
            anchor: Some(anchor),
        },
        FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            to: AccountingLocation::Position {
                resource: source_resource.clone(),
                position: rent_position,
                position_kind: PositionKind::RefundableAccountFunding,
            },
            debit: Amount {
                asset: rent_asset.clone(),
                atoms: retained.clone(),
            },
            credit: Amount {
                asset: rent_asset,
                atoms: retained,
            },
        },
    ])
}

fn staking_effects(
    intent: &PapertradeTypedIntent,
    preview: &PreparedOperation,
    proof: &VerifiedPapertradeCall,
    submission: Submission,
    account: &PapertradeAccount,
) -> Result<Option<VerifiedEffects>, GoFinError> {
    let paper = paper_asset();
    let amount = match intent {
        PapertradeTypedIntent::Stake { amount, .. }
        | PapertradeTypedIntent::Unstake { amount, .. } => Some(amount.clone()),
        _ => None,
    };
    let (effects, movement) = match intent {
        PapertradeTypedIntent::Stake { .. } => {
            let amount = amount.expect("stake amount");
            let value = gofin_domain::Quantity::new(amount.clone())?;
            (
                vec![FinancialEffect::Transfer {
                    from: paper_wallet_resource(&account.owner),
                    to: paper_staked_resource(&account.owner),
                    debit: Amount {
                        asset: paper.clone(),
                        atoms: value.clone(),
                    },
                    credit: Amount {
                        asset: paper,
                        atoms: value,
                    },
                }],
                amount,
            )
        }
        PapertradeTypedIntent::Unstake { .. } => {
            let amount = amount.expect("unstake amount");
            let value = gofin_domain::Quantity::new(amount.clone())?;
            (
                vec![FinancialEffect::Transfer {
                    from: paper_staked_resource(&account.owner),
                    to: paper_wallet_resource(&account.owner),
                    debit: Amount {
                        asset: paper.clone(),
                        atoms: value.clone(),
                    },
                    credit: Amount {
                        asset: paper,
                        atoms: value,
                    },
                }],
                amount,
            )
        }
        PapertradeTypedIntent::Claim { .. } => {
            let minimum: u128 = preview
                .binding
                .get("claimable_rewards_minimum_raw")
                .and_then(Value::as_str)
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| {
                    err(
                        "PAPERTRADE_BINDING_INVALID",
                        "reconciliation",
                        "Approved claimable reward bound is missing.",
                    )
                })?;
            let amount = proof
                .events
                .iter()
                .find_map(|event| match event {
                    ProtocolSettlementEvent::Claimed { amount, .. } => Some(amount),
                    _ => None,
                })
                .and_then(|value| value.parse::<u128>().ok())
                .or_else(|| {
                    proof
                        .call_return_data
                        .as_deref()
                        .and_then(|value| value.strip_prefix("0x"))
                        .and_then(|value| u128::from_str_radix(value, 16).ok())
                        .filter(|value| *value == 0)
                })
                .ok_or_else(|| {
                    err(
                        "PAPERTRADE_CLAIM_PROOF_INVALID",
                        "reconciliation",
                        "Canonical claim event amount is malformed.",
                    )
                })?;
            if amount < minimum {
                return Ok(None);
            }
            let qty = gofin_domain::Quantity::new(amount.to_string())?;
            let effects = if amount == 0 {
                Vec::new()
            } else {
                vec![FinancialEffect::Settlement {
                    resource: account.trading_cash_resource(),
                    asset: trading_usdc_asset(),
                    category: SettlementCategory::Funding,
                    delta: gofin_domain::SignedQuantity::positive(&qty),
                }]
            };
            (effects, amount.to_string())
        }
        _ => return Ok(None),
    };
    let zero_claim_without_event = matches!(intent, PapertradeTypedIntent::Claim { .. })
        && !proof
            .events
            .iter()
            .any(|event| matches!(event, ProtocolSettlementEvent::Claimed { .. }))
        && proof.call_return_data.as_deref()
            == Some("0x0000000000000000000000000000000000000000000000000000000000000000");
    if (!zero_claim_without_event && proof.event_ids.len() != 1)
        || (zero_claim_without_event && !proof.event_ids.is_empty())
    {
        return Err(err(
            "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
            "reconciliation",
            "Expected one staking event, or the verified zero-reward claim return without an event.",
        ));
    }
    let event_id = proof
        .event_ids
        .first()
        .map(|event| format!("{}:{event}", proof.transaction_hash));
    let proof_events = event_id
        .map(|event_id| {
            vec![json!({
                "eventId":event_id,
                "transactionHash":proof.transaction_hash,
                "blockHash":proof.block_hash,
                "blockNumber":proof.block_number,
                "batchId":proof.batch_id,
                "intentId":proof.intent_id,
                "eventIds":proof.event_ids,
                "callReturnData":proof.call_return_data
            })]
        })
        .unwrap_or_default();
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects,
        evidence: json!({
            "proofEvents":proof_events,
            "amount_raw":movement,
            "protocol_action":proof.action
        }),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(verified))
}

fn open_position_effects(
    intent: &PapertradeTypedIntent,
    proof: &VerifiedPapertradeCall,
    submission: Submission,
    account: &PapertradeAccount,
) -> Result<Option<VerifiedEffects>, GoFinError> {
    let PapertradeTypedIntent::OpenPosition {
        instrument_id,
        is_long,
        size: margin,
        leverage,
        ..
    } = intent
    else {
        return Ok(None);
    };
    if proof.event_ids.len() != 1 {
        return Err(err(
            "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
            "reconciliation",
            "Expected exactly one action-correlated position opening event.",
        ));
    }
    let Some(ProtocolSettlementEvent::PositionOpened {
        position_id,
        entry_price,
        margin: settled_margin,
        ..
    }) = proof
        .events
        .iter()
        .find(|event| matches!(event, ProtocolSettlementEvent::PositionOpened { .. }))
    else {
        return Ok(None);
    };
    let settled_margin = settled_margin.parse::<u128>().map_err(|_| {
        err(
            "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
            "reconciliation",
            "Canonical opening margin is malformed.",
        )
    })?;
    if settled_margin != *margin {
        return Ok(None);
    }
    let notional = settled_margin
        .checked_mul(u128::from(*leverage))
        .ok_or_else(|| {
            err(
                "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                "reconciliation",
                "Canonical derivative notional exceeds the supported exact range.",
            )
        })?;
    let owner = account.owner.to_ascii_lowercase();
    let resource = position_resource(&owner);
    let position = position_record(&proof.intent_id);
    let cash_asset = trading_usdc_asset();
    let margin_qty = gofin_domain::Quantity::new(settled_margin.to_string())?;
    let exposure_qty = gofin_domain::Quantity::new(notional.to_string())?;
    let exposure_kind = if *is_long {
        PositionKind::DerivativeLong
    } else {
        PositionKind::DerivativeShort
    };
    let margin_kind = if *is_long {
        PositionKind::MarginLong
    } else {
        PositionKind::MarginShort
    };
    // Position anchors store an absolute notional magnitude. The kind carries
    // long/short direction, matching the shared derivatives accounting model.
    let exposure_delta = SignedQuantity::positive(&exposure_qty);
    let margin_anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: margin_kind.clone(),
        principal: Amount {
            asset: cash_asset.clone(),
            atoms: margin_qty.clone(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"protocol":"papertrade","position_id":position_id,"instrument_id":instrument_id,"is_long":is_long,"leverage":leverage,"entry_price":entry_price,"margin_wad":settled_margin.to_string(),"transaction_hash":proof.transaction_hash,"event_ids":proof.event_ids}),
        observed_at: Utc::now().to_rfc3339(),
    };
    let derivative_anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: exposure_kind.clone(),
        principal: Amount {
            asset: cash_asset.clone(),
            atoms: exposure_qty.clone(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"protocol":"papertrade","position_id":position_id,"instrument_id":instrument_id,"is_long":is_long,"leverage":leverage,"entry_price":entry_price,"notional_usdc_wad":notional.to_string(),"transaction_hash":proof.transaction_hash,"event_ids":proof.event_ids}),
        observed_at: Utc::now().to_rfc3339(),
    };
    let margin_effect = FinancialEffect::PositionChange {
        resource: resource.clone(),
        position: position.clone(),
        asset: cash_asset.clone(),
        delta: SignedQuantity::positive(&margin_qty),
        scaled_receipt: None,
        anchor: Some(margin_anchor),
    };
    let exposure_effect = FinancialEffect::PositionChange {
        resource: resource.clone(),
        position: position.clone(),
        asset: cash_asset.clone(),
        delta: exposure_delta,
        scaled_receipt: None,
        anchor: Some(derivative_anchor),
    };
    let cash_debit = FinancialEffect::CashDebit {
        resource: account.trading_cash_resource(),
        amount: Amount {
            asset: cash_asset,
            atoms: margin_qty,
        },
    };
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects: vec![cash_debit, margin_effect, exposure_effect],
        evidence: json!({"proofEvents":[{"eventId":format!("{}:{}",proof.transaction_hash,proof.event_ids[0]),"transactionHash":proof.transaction_hash,"blockHash":proof.block_hash,"blockNumber":proof.block_number,"batchId":proof.batch_id,"intentId":proof.intent_id,"positionId":position_id}],"margin_wad":settled_margin.to_string(),"notional_usdc_wad":notional.to_string(),"entry_price":entry_price,"protocol_action":"open_position"}),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(verified))
}

fn close_position_effects(
    intent: &PapertradeTypedIntent,
    preview: &PreparedOperation,
    proof: &VerifiedPapertradeCall,
    submission: Submission,
    account: &PapertradeAccount,
) -> Result<Option<VerifiedEffects>, GoFinError> {
    let PapertradeTypedIntent::Close { position_ids, .. } = intent else {
        return Ok(None);
    };
    let rows: Vec<PositionSnapshot> =
        serde_json::from_value(preview.binding.get("close_positions").cloned().ok_or_else(
            || {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "reconciliation",
                    "Exact selected position pre-state is unavailable.",
                )
            },
        )?)
        .map_err(|_| {
            err(
                "PAPERTRADE_BINDING_INVALID",
                "reconciliation",
                "Exact selected position pre-state is malformed.",
            )
        })?;
    if rows.len() != position_ids.len()
        || rows
            .iter()
            .any(|row| row.use_debt || !position_ids.contains(&row.position_id))
    {
        return Err(err(
            "PAPERTRADE_BINDING_INVALID",
            "reconciliation",
            "Selected positions differ from the approved cash-only close binding.",
        ));
    }
    let close_event_ids = proof
        .events
        .iter()
        .filter_map(|event| match event {
            ProtocolSettlementEvent::PositionClosed { position_id, .. }
            | ProtocolSettlementEvent::Liquidated { position_id, .. } => Some(position_id.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    if close_event_ids.len() != rows.len()
        || rows
            .iter()
            .any(|row| !close_event_ids.contains(&row.position_id))
    {
        return Ok(None);
    }
    let has_ordinary_close = proof
        .events
        .iter()
        .any(|event| matches!(event, ProtocolSettlementEvent::PositionClosed { .. }));
    let cash_event = proof.events.iter().find_map(|event| match event {
        ProtocolSettlementEvent::PositionsClosedCashSettlement {
            owner,
            amount_wad,
            resulting_balance_wad,
        } => Some((owner, amount_wad, resulting_balance_wad)),
        _ => None,
    });
    let (cash_wad, resulting_balance) = match cash_event {
        Some((cash_owner, cash_wad, resulting_balance)) => {
            if !cash_owner.eq_ignore_ascii_case(&account.owner) || !has_ordinary_close {
                return Ok(None);
            }
            let cash_wad = cash_wad.parse::<u128>().map_err(|_| {
                err(
                    "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                    "reconciliation",
                    "Canonical close cash credit is malformed.",
                )
            })?;
            let resulting_balance = resulting_balance.parse::<u128>().map_err(|_| {
                err(
                    "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                    "reconciliation",
                    "Canonical post-close cash balance is malformed.",
                )
            })?;
            if cash_wad > resulting_balance {
                return Err(err(
                    "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                    "reconciliation",
                    "Canonical close cash credit exceeds the resulting internal balance.",
                ));
            }
            (cash_wad, Some(resulting_balance))
        }
        None if !has_ordinary_close => (0, None),
        None => return Ok(None),
    };
    let entitlement = close_cash_entitlement(&proof.events, &rows)?;
    if cash_wad > entitlement {
        return Err(err(
            "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
            "reconciliation",
            "Canonical cash credit exceeds the code-reviewed cash entitlement of the selected positions.",
        ));
    }
    let queued_claim = entitlement - cash_wad;
    let mut selected = Vec::with_capacity(rows.len());
    for row in &rows {
        let margin = row
            .margin_raw
            .parse::<u128>()
            .map_err(|_| GoFinError::validation("Selected position margin is malformed."))?;
        let leverage = u32::try_from(row.leverage)
            .map_err(|_| GoFinError::validation("Selected position leverage is unsupported."))?;
        let notional = margin.checked_mul(u128::from(leverage)).ok_or_else(|| {
            GoFinError::validation("Selected position notional exceeds exact range.")
        })?;
        selected.push((row, margin, notional));
    }
    let resource = position_resource(&account.owner);
    let asset = trading_usdc_asset();
    let mut effects = Vec::with_capacity(selected.len() * 2 + 2);
    for (row, margin_total, notional_total) in selected {
        let position = position_snapshot_key(row);
        let margin = gofin_domain::Quantity::new(margin_total.to_string())?;
        let notional = gofin_domain::Quantity::new(notional_total.to_string())?;
        let exposure_kind = if row.is_long {
            PositionKind::DerivativeLong
        } else {
            PositionKind::DerivativeShort
        };
        let margin_kind = if row.is_long {
            PositionKind::MarginLong
        } else {
            PositionKind::MarginShort
        };
        let exposure_delta = SignedQuantity::negative(&notional);
        let evidence = json!({"protocol":"papertrade","closed_position_id":row.position_id,"open_intent_id":row.intent_id,"transaction_hash":proof.transaction_hash,"event_ids":proof.event_ids,"settlement":"full close verified by canonical Exchange close event and code-pinned PositionsClosed cash event"});
        let margin_anchor = PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: margin_kind,
            principal: Amount {
                asset: asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(0),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: evidence.clone(),
            observed_at: Utc::now().to_rfc3339(),
        };
        let exposure_anchor = PositionAnchor {
            resource: resource.clone(),
            position: position.clone(),
            position_kind: exposure_kind,
            principal: Amount {
                asset: asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(0),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence,
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position: position.clone(),
            asset: asset.clone(),
            delta: SignedQuantity::negative(&margin),
            scaled_receipt: None,
            anchor: Some(margin_anchor),
        });
        effects.push(FinancialEffect::PositionChange {
            resource: resource.clone(),
            position,
            asset: asset.clone(),
            delta: exposure_delta,
            scaled_receipt: None,
            anchor: Some(exposure_anchor),
        });
    }
    let cash_credit = gofin_domain::Quantity::new(cash_wad.to_string())?;
    effects.push(FinancialEffect::Settlement {
        resource: account.trading_cash_resource(),
        asset: asset.clone(),
        category: SettlementCategory::RealizedPnl,
        delta: SignedQuantity::positive(&cash_credit),
    });
    let queued_claim = gofin_domain::Quantity::new(queued_claim.to_string())?;
    if !queued_claim.is_zero() {
        let position = format!("usdc_win_claim:{}", proof.intent_id);
        let anchor = PositionAnchor {
            resource: payout_queue_resource(&account.owner),
            position: position.clone(),
            position_kind: PositionKind::Investment,
            principal: Amount {
                asset: asset.clone(),
                atoms: queued_claim.clone(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({
                "protocol":"papertrade",
                "intent_id":proof.intent_id,
                "transaction_hash":proof.transaction_hash,
                "event_ids":proof.event_ids,
                "nonspendable":true,
            }),
            observed_at: Utc::now().to_rfc3339(),
        };
        effects.push(FinancialEffect::PositionChange {
            resource: payout_queue_resource(&account.owner),
            position,
            asset: asset.clone(),
            delta: SignedQuantity::positive(&queued_claim),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
    }
    let minted_paper = proof
        .events
        .iter()
        .filter_map(|event| match event {
            ProtocolSettlementEvent::PaperMinted { owner, amount, .. }
                if owner.eq_ignore_ascii_case(&account.owner) =>
            {
                Some(amount)
            }
            _ => None,
        })
        .try_fold(0u128, |total, amount| {
            let amount = amount.parse::<u128>().map_err(|_| {
                err(
                    "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                    "reconciliation",
                    "Canonical PAPER mint amount is malformed.",
                )
            })?;
            total.checked_add(amount).ok_or_else(|| {
                err(
                    "PAPERTRADE_SETTLEMENT_PROOF_INVALID",
                    "reconciliation",
                    "Canonical PAPER mint total exceeds exact range.",
                )
            })
        })?;
    if minted_paper > 0 {
        let amount = gofin_domain::Quantity::new(minted_paper.to_string())?;
        effects.push(FinancialEffect::Settlement {
            resource: paper_wallet_resource(&account.owner),
            asset: paper_asset(),
            category: SettlementCategory::Funding,
            delta: SignedQuantity::positive(&amount),
        });
    }
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects,
        evidence: json!({"proofEvents":[{"eventId":format!("{}:{}",proof.transaction_hash,proof.intent_id),"transactionHash":proof.transaction_hash,"blockHash":proof.block_hash,"blockNumber":proof.block_number,"batchId":proof.batch_id,"intentId":proof.intent_id,"eventIds":proof.event_ids,"closedPositionIds":position_ids}],"cash_credit_wad":cash_wad.to_string(),"code_reviewed_cash_entitlement_wad":entitlement.to_string(),"queued_nonspendable_claim_wad":queued_claim.to_string(),"resulting_cash_balance_wad":resulting_balance.map(|value|value.to_string()),"paper_minted_raw":minted_paper.to_string(),"protocol_action":"close_positions"}),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(verified))
}

fn claimable_rewards_wad(snapshot: &Value) -> Result<u128, GoFinError> {
    let raw = |path: &str| -> Result<BigUint, GoFinError> {
        snapshot
            .pointer(path)
            .and_then(Value::as_str)
            .and_then(|value| BigUint::from_str(value).ok())
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_STAKING_SNAPSHOT_INVALID",
                    "preview",
                    "Staking snapshot is missing exact raw reward fields.",
                )
            })
    };
    let staked = raw("/paper/staked")?;
    let pending_base = raw("/paper/pendingBase")?;
    let reward_debt = raw("/paper/rewardDebt")?;
    let accumulator = raw("/accRewardPerShareRaw")?;
    // The staking contract pays only what remains in its reward reserve and
    // leaves any unpaid amount pending. Bind the realizable amount, not the
    // larger accumulator amount, as the minimum claim settlement.
    let reward_reserve = raw("/rewardReserveRaw")?;
    let accrued = staked * accumulator / BigUint::from(10u128.pow(27));
    let gross = accrued + pending_base;
    if gross < reward_debt {
        return Err(err(
            "PAPERTRADE_STAKING_SNAPSHOT_INVALID",
            "preview",
            "Staking reward debt exceeds accrued reward state.",
        ));
    }
    ((gross - reward_debt).min(reward_reserve))
        .to_u128()
        .ok_or_else(|| {
            err(
                "PAPERTRADE_STAKING_SNAPSHOT_INVALID",
                "preview",
                "Claimable reward amount exceeds the supported exact range.",
            )
        })
}

fn indexed_through_block(snapshot: &Value) -> Result<u64, GoFinError> {
    snapshot
        .get("indexedThroughBlock")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            err(
                "PAPERTRADE_STATE_SNAPSHOT_INVALID",
                "preview",
                "Trading state is missing its exact indexed-through block.",
            )
        })
}

/// Read the exact minimum PAPER stake from the official trading-state schema.
/// The API currently publishes it as intakeControls[1]; the normalized broker
/// runtime may also expose the same value under minimumPaperStakeRaw.
fn minimum_paper_stake_raw(snapshot: &Value) -> Result<u128, GoFinError> {
    let raw = snapshot
        .get("minimumPaperStakeRaw")
        .or_else(|| snapshot.get("minimum_paper_stake_raw"))
        .or_else(|| {
            snapshot
                .get("intakeControls")
                .and_then(Value::as_array)
                .and_then(|v| v.get(1))
        })
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .ok_or_else(|| {
            err(
                "PAPERTRADE_STATE_SNAPSHOT_INVALID",
                "preview",
                "Trading state is missing the exact minimum PAPER stake.",
            )
        })?;
    raw.parse::<u128>().map_err(|_| {
        err(
            "PAPERTRADE_STATE_SNAPSHOT_INVALID",
            "preview",
            "Trading state has a malformed minimum PAPER stake.",
        )
    })
}

fn enforce_minimum_paper_stake(
    request: &IntentRequest,
    snapshot: &Value,
) -> Result<(), GoFinError> {
    if let IntentRequest::Stake { amount, .. } = request {
        let minimum = minimum_paper_stake_raw(snapshot)?;
        if *amount < minimum {
            return Err(err(
                "PAPERTRADE_STAKE_BELOW_MINIMUM",
                "preview",
                "The requested PAPER stake is below the current protocol minimum.",
            ));
        }
    }
    Ok(())
}

fn configuration_verified_effects(
    record: &OperationRecord,
    evidence: Value,
) -> Result<VerifiedEffects, GoFinError> {
    let submission = record.submission.clone().ok_or_else(|| {
        err(
            "PAPERTRADE_SUBMISSION_MISSING",
            "reconciliation",
            "Canonical configuration proof has no durable submission identity.",
        )
    })?;
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects: vec![],
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn session_revocation_verified_effects(
    record: &OperationRecord,
    owner: &str,
    actual_gas_fee_atoms: &str,
    evidence: Value,
) -> Result<VerifiedEffects, GoFinError> {
    let submission = record.submission.clone().ok_or_else(|| {
        err(
            "PAPERTRADE_SUBMISSION_MISSING",
            "reconciliation",
            "Canonical revocation proof has no durable submission identity.",
        )
    })?;
    let fee = gofin_domain::Quantity::new(actual_gas_fee_atoms)?;
    let native = AssetId {
        network: "ethereum:999".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let effects = vec![FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: ResourceId(format!("ethereum:999:{}", owner.to_ascii_lowercase())),
        },
        amount: Amount {
            asset: native,
            atoms: fee.clone(),
        },
        category: "ethereum_gas".into(),
        treatment: CostTreatment::AdditionalDebit,
    }];
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects,
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(verified)
}

fn expired_intent_reconciliation(
    submission: &Submission,
    bound: &BoundIntent,
    proof: crate::PapertradeIntentExpiryProof,
) -> Result<Option<Reconciliation>, GoFinError> {
    if proof.validate_against(bound).is_err() {
        return Ok(None);
    }
    let evidence = json!({
        "canonical_intent_expiry": proof,
        "intent_id": bound.intent_id,
        "terminal": true,
        "reason": "The exact typed intent is past its signed deadline, absent from canonical logs, and its nonce remains unused under reviewed code pins."
    });
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: Vec::new(),
        evidence: evidence.clone(),
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(Reconciliation {
        status: Status::Failed,
        evidence,
        verified: Some(verified),
    }))
}

async fn expired_session_registration_reconciliation<R: PapertradeRuntime + 'static>(
    runtime: &R,
    record: &OperationRecord,
    registration: &SessionRegistrationBinding,
) -> Result<Option<Reconciliation>, GoFinError> {
    let Some(proof) = runtime
        .session_registration_expiry_proof(registration)
        .await?
    else {
        return Ok(None);
    };
    if proof.validate_against(registration).is_err() {
        return Ok(None);
    }
    let evidence = json!({
        "session_registration_expired": {
            "registration_id": proof.registration_id,
            "intent_id": proof.intent_id,
            "owner": proof.owner,
            "deadline": proof.deadline,
            "canonical_block_number": proof.canonical_block_number,
            "canonical_block_hash": proof.canonical_block_hash,
            "canonical_block_timestamp": proof.canonical_block_timestamp,
            "session_key_manager_proxy": proof.session_key_manager_proxy,
            "session_key_manager_implementation": proof.session_key_manager_implementation,
            "session_key_manager_code_hash": proof.session_key_manager_code_hash,
            "nonce_unused": proof.nonce_unused,
            "session_absent": proof.session_absent
        },
        "financial_effects": []
    });
    let verified = configuration_verified_effects(record, evidence.clone())?;
    Ok(Some(Reconciliation {
        status: Status::Failed,
        evidence,
        verified: Some(verified),
    }))
}

async fn prepare_funding_operation<R: PapertradeRuntime + 'static>(
    adapter: &PapertradeAdapter<R>,
    operation: &str,
    input: &Value,
    context: &gofin_domain::PreparationContext,
) -> Result<PreparedOperation, GoFinError> {
    if !adapter.account.signing_enabled {
        return Err(err(
            "SIGNING_DISABLED",
            "preview",
            "Signing is disabled for this Papertrade account.",
        ));
    }
    let operation_id = context
        .logical_operation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let intent = funding_intent(operation, &adapter.account.owner, input, &operation_id)?;
    validate_funding(&intent)?;
    let binding = adapter
        .runtime
        .prepare_funding(intent.clone(), &operation_id)
        .await?;
    if binding.version != 1 || binding.operation_id != operation_id || binding.intent != intent {
        return Err(err(
            "PAPERTRADE_FUNDING_BINDING_INVALID",
            "preview",
            "Broker returned a funding plan for a different owner, route, or exact amount.",
        ));
    }
    if !valid_funding_identities(
        &intent,
        &binding.source_identity,
        &binding.destination_identity,
    ) {
        return Err(err(
            "PAPERTRADE_FUNDING_IDENTITY_INVALID",
            "preview",
            "Broker funding accounts do not match the fixed route and enrolled identities.",
        ));
    }
    validate_funding_bounds(&intent, &binding.bounds).map_err(|_| {
        err(
            "PAPERTRADE_FUNDING_BOUNDS_INVALID",
            "preview",
            "Broker returned incomplete or inconsistent source, destination, fee, or native-gas bounds.",
        )
    })?;
    let internal_wad = u128::from(intent.amount_usdc_atoms)
        .checked_mul(1_000_000_000_000)
        .ok_or_else(|| GoFinError::validation("Internal USDC amount overflow."))?;
    let internal_minimum = u128::from(binding.bounds.destination_min_usdc_atoms)
        .checked_mul(1_000_000_000_000)
        .ok_or_else(|| GoFinError::validation("Internal USDC amount overflow."))?;
    let resource = adapter.account.trading_cash_resource();
    let asset = trading_usdc_asset();
    let stage_one_core_withdrawal =
        intent.direction == FundingDirection::Withdraw && intent.route != FundingRoute::Core;
    let (route_identity, source_route) = match intent.direction {
        FundingDirection::Deposit => (binding.source_identity.clone(), intent.route.clone()),
        FundingDirection::Withdraw if stage_one_core_withdrawal => (
            format!(
                "hyperliquid:mainnet:{}:spot:primary",
                intent.owner.to_ascii_lowercase()
            ),
            FundingRoute::Core,
        ),
        FundingDirection::Withdraw => (binding.destination_identity.clone(), intent.route.clone()),
    };
    let source_resource = ResourceId(route_identity);
    let source_asset = route_usdc_asset(&source_route);
    let mut financial_intent = FinancialIntent::default();
    match intent.direction {
        FundingDirection::Deposit => {
            let source_amount = route_atoms(&intent.route, binding.bounds.source_usdc_atoms)?;
            financial_intent.debits.push(ResourceAmount {
                resource: source_resource.clone(),
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: source_amount.clone(),
                },
            });
            financial_intent.expected.push(EffectConstraint::Debit {
                resource: source_resource.clone(),
                asset: source_asset.clone(),
                maximum_debit: source_amount,
            });
            financial_intent
                .expected
                .push(EffectConstraint::Settlement {
                    resource: resource.clone(),
                    asset: asset.clone(),
                    category: SettlementCategory::Funding,
                    minimum_delta: SignedQuantity::positive(&gofin_domain::Quantity::new(
                        internal_minimum.to_string(),
                    )?),
                    maximum_delta: SignedQuantity::positive(&gofin_domain::Quantity::new(
                        internal_wad.to_string(),
                    )?),
                });
        }
        FundingDirection::Withdraw => {
            let principal_debit = gofin_domain::Quantity::new(internal_wad.to_string())?;
            financial_intent.debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: principal_debit.clone(),
                },
            });
            financial_intent.expected.push(EffectConstraint::Debit {
                resource,
                asset,
                maximum_debit: principal_debit,
            });
            financial_intent.expected.push(EffectConstraint::Credit {
                to: source_resource.clone(),
                asset: source_asset.clone(),
                minimum_credit: route_atoms(
                    &source_route,
                    if stage_one_core_withdrawal {
                        intent.amount_usdc_atoms
                    } else {
                        binding.bounds.destination_min_usdc_atoms
                    },
                )?,
            });
        }
    }
    // Stage one proves only the owner-signed Papertrade cash transfer into
    // Core. Onward-route fees remain bound for the separately authorized
    // continuation and cannot be charged to this operation.
    if !stage_one_core_withdrawal && binding.bounds.maximum_source_fee_usdc_atoms > 0 {
        financial_intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset: source_asset.clone(),
                atoms: route_atoms(&source_route, binding.bounds.maximum_source_fee_usdc_atoms)?,
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let fee_bounds = if stage_one_core_withdrawal {
        [0, 0]
    } else {
        [
            binding.bounds.maximum_cctp_fee_usdc_atoms,
            binding.bounds.maximum_activation_fee_atoms,
        ]
    };
    for fee in fee_bounds {
        if fee > 0 {
            financial_intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource.clone(),
                },
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: route_atoms(&source_route, fee)?,
                },
                treatment: CostTreatment::IncludedInDebit,
            });
        }
    }
    if !stage_one_core_withdrawal
        && let (Some(fee_asset), Some(maximum)) = (
            binding.bounds.native_fee_asset.as_deref(),
            binding.bounds.maximum_native_fee_atoms.as_deref(),
        )
    {
        let fee_asset = native_fee_asset(&source_route, fee_asset)?;
        financial_intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset: fee_asset,
                atoms: native_fee_quantity(&source_route, maximum)?,
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    if let (Some(refundable_cap), Some(total_native_cap), Some(native_asset), Some(maximum_fee)) = (
        binding
            .bounds
            .maximum_refundable_native_funding_atoms
            .as_deref(),
        binding.bounds.maximum_native_source_debit_atoms.as_deref(),
        binding.bounds.native_fee_asset.as_deref(),
        binding.bounds.maximum_native_fee_atoms.as_deref(),
    ) {
        if intent.direction != FundingDirection::Deposit || intent.route != FundingRoute::Solana {
            return Err(err(
                "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                "preview",
                "Refundable native source funding is supported only for Solana CCTP deposits.",
            ));
        }
        let _account = binding
            .solana_message_event_account
            .as_deref()
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                    "preview",
                    "Solana CCTP funding omits its exact MessageSent account identity.",
                )
            })?;
        let asset = native_fee_asset(&intent.route, native_asset)?;
        let refundable = gofin_domain::Quantity::new(refundable_cap)?;
        let total = total_native_cap.parse::<u128>().map_err(|_| {
            err(
                "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                "preview",
                "Solana total native debit cap is malformed.",
            )
        })?;
        let fee = maximum_fee.parse::<u128>().map_err(|_| {
            err(
                "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                "preview",
                "Solana native fee cap is malformed.",
            )
        })?;
        let refundable_atoms = refundable_cap.parse::<u128>().map_err(|_| {
            err(
                "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                "preview",
                "Solana refundable funding cap is malformed.",
            )
        })?;
        if fee
            .checked_add(refundable_atoms)
            .is_none_or(|sum| sum > total)
        {
            return Err(err(
                "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                "preview",
                "Solana network fee and refundable account rent exceed their total native debit bound.",
            ));
        }
        let position = solana_message_sent_position(&binding);
        financial_intent.debits.push(ResourceAmount {
            resource: source_resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: refundable.clone(),
            },
        });
        financial_intent
            .expected
            .push(EffectConstraint::OptionalAccountFunding {
                resource: source_resource.clone(),
                asset,
                accounts: vec![AccountFundingLimit {
                    position,
                    maximum_amount: refundable,
                }],
            });
    }
    Ok(PreparedOperation {
        operation: operation.into(),
        input: input.clone(),
        binding: json!({"funding_binding":binding}),
        summary: format!(
            "Prepare exact {} USDC {:?} through {:?} for Papertrade account {}.",
            format_usdc_atoms(intent.amount_usdc_atoms),
            intent.direction,
            intent.route,
            adapter.account.name
        ),
        checks: vec![Check {
            name: "papertrade_funding_route".into(),
            status: CheckStatus::Passed,
            detail: "Fixed route, owner, exact amount and broker-persisted funding plan validated.".into(),
        }],
        warnings: vec![
            "Settlement requires final destination-side canonical proof; route submission alone does not complete funding.".into(),
        ],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        valuation: adapter.runtime.funding_valuation(&intent).await?,
        intent: financial_intent,
        operation_version: 1,
        action_deadline: None,
    })
}

async fn prepare_funding_continuation<R: PapertradeRuntime + 'static>(
    adapter: &PapertradeAdapter<R>,
    input: &Value,
    context: &gofin_domain::PreparationContext,
) -> Result<PreparedOperation, GoFinError> {
    if !adapter.account.signing_enabled {
        return Err(err(
            "SIGNING_DISABLED",
            "preview",
            "Signing is disabled for this Papertrade account.",
        ));
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        route_id: String,
    }
    let request: Request = parse_input(input)?;
    let operation_id = context
        .logical_operation_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if request.route_id.trim().is_empty() {
        return Err(GoFinError::validation("Funding route ID is required."));
    }
    let binding = adapter
        .runtime
        .prepare_funding_continuation(&request.route_id, &operation_id)
        .await?;
    binding.validate().map_err(|_| {
        err(
            "PAPERTRADE_CONTINUATION_BINDING_INVALID",
            "preview",
            "Broker returned an incomplete or inconsistent funding continuation.",
        )
    })?;
    if binding.operation_id != operation_id
        || binding.route_id != request.route_id
        || !binding
            .intent
            .owner
            .eq_ignore_ascii_case(&adapter.account.owner)
        || !valid_continuation_identities(&binding)
    {
        return Err(err(
            "PAPERTRADE_CONTINUATION_BINDING_INVALID",
            "preview",
            "The continuation does not match the exact enrolled owner and settled route.",
        ));
    }
    let valuation = adapter
        .runtime
        .funding_continuation_valuation(&binding)
        .await?;
    let source_resource = ResourceId(binding.source_identity.clone());
    let source_asset = route_usdc_asset(&FundingRoute::Core);
    let source_principal = route_atoms(&FundingRoute::Core, binding.bounds.source_usdc_atoms)?;
    let destination_resource = ResourceId(binding.destination_identity.clone());
    let destination_asset = route_usdc_asset(&binding.intent.route);
    let destination_minimum = route_atoms(
        &binding.intent.route,
        binding.bounds.destination_min_usdc_atoms,
    )?;
    let mut intent = FinancialIntent::default();
    intent.debits.push(ResourceAmount {
        resource: source_resource.clone(),
        amount: Amount {
            asset: source_asset.clone(),
            atoms: source_principal.clone(),
        },
    });
    intent.expected.push(EffectConstraint::Debit {
        resource: source_resource.clone(),
        asset: source_asset.clone(),
        maximum_debit: source_principal,
    });
    intent.expected.push(EffectConstraint::Credit {
        to: destination_resource.clone(),
        asset: destination_asset.clone(),
        minimum_credit: destination_minimum,
    });
    if binding.bounds.maximum_source_fee_usdc_atoms > 0 {
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset: source_asset.clone(),
                atoms: route_atoms(
                    &FundingRoute::Core,
                    binding.bounds.maximum_source_fee_usdc_atoms,
                )?,
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    for fee in [
        binding.bounds.maximum_cctp_fee_usdc_atoms,
        binding.bounds.maximum_activation_fee_atoms,
    ] {
        if fee > 0 {
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource.clone(),
                },
                amount: Amount {
                    asset: source_asset.clone(),
                    atoms: route_atoms(&FundingRoute::Core, fee)?,
                },
                treatment: CostTreatment::IncludedInDebit,
            });
        }
    }
    if let (Some(fee_asset), Some(maximum)) = (
        binding.bounds.native_fee_asset.as_deref(),
        binding.bounds.maximum_native_fee_atoms.as_deref(),
    ) {
        let asset = native_fee_asset(&FundingRoute::Core, fee_asset)?;
        intent.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset,
                atoms: native_fee_quantity(&FundingRoute::Core, maximum)?,
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    Ok(PreparedOperation {
        operation: OP_FUNDING_CONTINUE.into(),
        input: input.clone(),
        binding: json!({"funding_continuation":binding}),
        summary: format!(
            "Continue the settled Papertrade withdrawal {} from HyperCore to {}.",
            request.route_id, binding.destination_identity
        ),
        checks: vec![Check {
            name: "papertrade_funding_continuation".into(),
            status: CheckStatus::Passed,
            detail: "The protected parent withdrawal and its canonical Core-credit proof are bound to the exact onward route and finite costs.".into(),
        }],
        warnings: vec![
            "This stage debits only the bound HyperCore source and credits the final destination; it does not repeat the Papertrade cash debit or Core credit.".into(),
        ],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        valuation,
        intent,
        operation_version: 1,
        action_deadline: None,
    })
}

fn valid_continuation_identities(binding: &FundingContinuationBinding) -> bool {
    let owner = binding.intent.owner.to_ascii_lowercase();
    binding.source_identity == format!("hyperliquid:mainnet:{owner}:spot:primary")
        && match binding.intent.route {
            FundingRoute::Core => false,
            FundingRoute::Hyperevm | FundingRoute::Ethereum | FundingRoute::Arbitrum => {
                valid_evm_account_identity(
                    &binding.intent.route,
                    &binding.destination_identity,
                    &owner,
                )
            }
            FundingRoute::Solana => valid_solana_account_identity(&binding.destination_identity),
        }
}

fn funding_continuation_submission(binding: &FundingContinuationBinding) -> Submission {
    Submission {
        kind: "papertrade_funding_continuation".into(),
        reference: binding.operation_id.clone(),
        data: json!({
            "operation_id":binding.operation_id,
            "route_id":binding.route_id,
            "plan_fingerprint":binding.plan_fingerprint,
            "stage1_proof_fingerprint":binding.stage1_proof_fingerprint
        }),
    }
}

/// Build only stage-two accounting effects. The stage-one Core credit already
/// exists in the source balance, so any reserve left after this debit remains
/// there naturally and must not be represented as a second credit.
fn funding_continuation_effects(
    binding: &FundingContinuationBinding,
    receipt: &FundingReceipt,
) -> Result<Option<Vec<FinancialEffect>>, GoFinError> {
    let conserved = receipt
        .destination_credit_usdc_atoms
        .checked_add(receipt.actual_cctp_fee_usdc_atoms)
        .and_then(|x| x.checked_add(receipt.activation_fee_usdc_atoms));
    if receipt.status != "settled"
        || receipt.stage != FundingReceiptStage::FinalDestination
        || receipt.amount_usdc_atoms != receipt.destination_credit_usdc_atoms
        || receipt.source_debit_usdc_atoms != binding.bounds.source_usdc_atoms
        || receipt.actual_source_fee_usdc_atoms > binding.bounds.maximum_source_fee_usdc_atoms
        || receipt.actual_cctp_fee_usdc_atoms > binding.bounds.maximum_cctp_fee_usdc_atoms
        || receipt.activation_fee_usdc_atoms > binding.bounds.maximum_activation_fee_atoms
        || receipt.destination_credit_usdc_atoms < binding.bounds.destination_min_usdc_atoms
        || receipt.destination_credit_usdc_atoms > binding.bounds.source_usdc_atoms
        || conserved != Some(binding.bounds.source_usdc_atoms)
        || receipt.retained_source_credit_usdc_atoms != 0
        || receipt.evidence_id.trim().is_empty()
        || receipt.source_identity != binding.source_identity
        || receipt.destination_identity != binding.destination_identity
        || !valid_continuation_identities(binding)
    {
        return Ok(None);
    }

    let mut effects = vec![
        FinancialEffect::CashDebit {
            resource: ResourceId(binding.source_identity.clone()),
            amount: Amount {
                asset: route_usdc_asset(&FundingRoute::Core),
                atoms: route_atoms(&FundingRoute::Core, receipt.source_debit_usdc_atoms)?,
            },
        },
        FinancialEffect::UsableCredit {
            to: ResourceId(binding.destination_identity.clone()),
            amount: Amount {
                asset: route_usdc_asset(&binding.intent.route),
                atoms: route_atoms(&binding.intent.route, receipt.destination_credit_usdc_atoms)?,
            },
        },
    ];
    if receipt.actual_source_fee_usdc_atoms > 0 {
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: ResourceId(binding.source_identity.clone()),
            },
            amount: Amount {
                asset: route_usdc_asset(&FundingRoute::Core),
                atoms: route_atoms(&FundingRoute::Core, receipt.actual_source_fee_usdc_atoms)?,
            },
            treatment: CostTreatment::AdditionalDebit,
            category: "papertrade_funding_continuation_source_fee".into(),
        });
    }
    let included_usdc_fee = receipt
        .actual_cctp_fee_usdc_atoms
        .checked_add(receipt.activation_fee_usdc_atoms)
        .ok_or_else(|| GoFinError::validation("Continuation fee total overflow."))?;
    if included_usdc_fee > 0 {
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: ResourceId(binding.source_identity.clone()),
            },
            amount: Amount {
                asset: route_usdc_asset(&FundingRoute::Core),
                atoms: route_atoms(&FundingRoute::Core, included_usdc_fee)?,
            },
            treatment: CostTreatment::IncludedInDebit,
            category: "papertrade_funding_continuation_protocol_fee".into(),
        });
    }
    match (
        receipt.actual_native_fee_atoms.as_deref(),
        receipt.actual_native_fee_asset.as_deref(),
    ) {
        (None, None) => {}
        (Some(raw), Some(asset)) if asset == "hyperliquid:mainnet:HYPE" => {
            let actual = raw
                .parse::<u128>()
                .map_err(|_| GoFinError::validation("Continuation native fee is malformed."))?;
            let maximum = binding
                .bounds
                .maximum_native_fee_atoms
                .as_deref()
                .and_then(|x| x.parse::<u128>().ok())
                .unwrap_or(0);
            if actual > maximum {
                return Ok(None);
            }
            if actual > 0 {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: ResourceId(binding.source_identity.clone()),
                    },
                    amount: Amount {
                        asset: native_fee_asset(&FundingRoute::Core, asset)?,
                        atoms: native_fee_quantity(&FundingRoute::Core, raw)?,
                    },
                    treatment: CostTreatment::AdditionalDebit,
                    category: "papertrade_funding_continuation_native_fee".into(),
                });
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(effects))
}

async fn reconcile_funding_continuation_operation<R: PapertradeRuntime + 'static>(
    adapter: &PapertradeAdapter<R>,
    record: &OperationRecord,
    preview: &PreparedOperation,
) -> Result<Reconciliation, GoFinError> {
    let Some(submission) = record.submission.clone() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable funding continuation submission is present."}),
            verified: None,
        });
    };
    let binding: FundingContinuationBinding = serde_json::from_value(
        preview
            .binding
            .get("funding_continuation")
            .cloned()
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_CONTINUATION_BINDING_MISSING",
                    "reconciliation",
                    "The protected continuation identity is missing.",
                )
            })?,
    )
    .map_err(|_| {
        err(
            "PAPERTRADE_CONTINUATION_BINDING_INVALID",
            "reconciliation",
            "The protected continuation identity is malformed.",
        )
    })?;
    if binding.validate().is_err()
        || !valid_continuation_identities(&binding)
        || binding.operation_id != submission.reference
        || funding_continuation_submission(&binding) != submission
    {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"The continuation binding does not match its durable submission."}),
            verified: None,
        });
    }
    let progress = adapter
        .runtime
        .reconcile_funding_continuation(&binding)
        .await?;
    match progress.status {
        FundingProgressStatus::Pending => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"continuation_status":"pending","route_id":binding.route_id}),
            verified: None,
        }),
        FundingProgressStatus::Unknown => Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"continuation_status":"unknown","route_id":binding.route_id}),
            verified: None,
        }),
        FundingProgressStatus::Rejected => Ok(Reconciliation {
            status: Status::Rejected,
            evidence: json!({"continuation_status":"rejected","route_id":binding.route_id,"evidence":progress.evidence}),
            verified: None,
        }),
        FundingProgressStatus::Settled => {
            let Some(receipt) = progress.receipt else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Continuation reported settlement without a final destination receipt."}),
                    verified: None,
                });
            };
            let Some(effects) = funding_continuation_effects(&binding, &receipt)? else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Stage-two destination proof does not match the immutable Core source, destination and finite bounds."}),
                    verified: None,
                });
            };
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence: json!({
                    "continuation_route_id":binding.route_id,
                    "parent_submission":binding.parent_submission,
                    "plan_fingerprint":binding.plan_fingerprint,
                    "stage1_proof_fingerprint":binding.stage1_proof_fingerprint,
                    "proofEvents":[{"eventId":receipt.evidence_id,"source":receipt.source_identity,"destination":receipt.destination_identity}],
                    "amount_usdc_atoms":receipt.destination_credit_usdc_atoms
                }),
                observed_at: Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            Ok(Reconciliation {
                status: Status::Completed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
    }
}

#[async_trait]
impl<R: PapertradeRuntime + 'static> Adapter for PapertradeAdapter<R> {
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        self.runtime.attach_execution_state(state)
    }

    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
    }
    fn uses_rpc_reads(&self, operation: &str, _input: &Value) -> bool {
        matches!(
            operation,
            OP_STAKING
                | OP_LIQUIDATION
                | OP_OPEN
                | OP_CLOSE
                | OP_REGISTER
                | OP_REVOKE
                | OP_STAKE
                | OP_UNSTAKE
                | OP_CLAIM
                | OP_DEPOSIT
                | OP_WITHDRAW
                | OP_FUNDING_CONTINUE
        )
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        vec![self.account.metadata()]
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value, GoFinError> {
        if operation == OP_LIQUIDATION {
            let request = parse_input::<LiquidationInput>(input)?;
            if request.position_id.is_empty()
                || request.position_id.len() > 78
                || request.position_id.parse::<BigUint>().is_err()
            {
                return Err(GoFinError::validation(
                    "Position ID must be a bounded decimal uint256 string.",
                ));
            }
            let Some(tx) = self
                .runtime
                .liquidation_transaction(
                    &self.account.owner.to_ascii_lowercase(),
                    &request.position_id,
                    request.since_block,
                )
                .await?
            else {
                return Ok(
                    json!({"owner":self.account.owner.to_ascii_lowercase(),"position_id":request.position_id,"state":"not_observed"}),
                );
            };
            let proof = crate::verify_liquidation_receipt(
                &tx,
                &self.account.owner,
                &request.position_id,
            ).map_err(|_| err(
                "PAPERTRADE_LIQUIDATION_PROOF_INVALID",
                "read",
                "Canonical receipt does not prove an exact owner and position-correlated liquidation award.",
            ))?;
            return Ok(json!({
                "owner":proof.owner,
                "position_id":proof.position_id,
                "state":"liquidated",
                "paper_minted_raw":proof.paper_minted,
                "transaction_hash":proof.transaction_hash,
                "block_hash":proof.block_hash,
                "block_number":proof.block_number,
                "event_ids":proof.event_ids
            }));
        }
        let request = match operation {
            OP_TRADING_STATE => {
                parse_input::<serde_json::Map<String, Value>>(input)?;
                if !input.as_object().is_some_and(|x| x.is_empty()) {
                    return Err(GoFinError::validation("Trading state takes no parameters."));
                }
                ReadRequest::TradingState
            }
            OP_ACCOUNT => {
                parse_input::<serde_json::Map<String, Value>>(input)?;
                if !input.as_object().is_some_and(|x| x.is_empty()) {
                    return Err(GoFinError::validation("Account read takes no parameters."));
                }
                ReadRequest::AccountState {
                    owner: self.account.owner.to_ascii_lowercase(),
                }
            }
            OP_POSITIONS => {
                parse_input::<serde_json::Map<String, Value>>(input)?;
                if !input.as_object().is_some_and(|x| x.is_empty()) {
                    return Err(GoFinError::validation(
                        "Positions read takes no parameters.",
                    ));
                }
                ReadRequest::Positions {
                    owner: self.account.owner.to_ascii_lowercase(),
                }
            }
            OP_HISTORY => {
                let history: HistoryInput = parse_input(input)?;
                ReadRequest::History {
                    owner: self.account.owner.to_ascii_lowercase(),
                    since_ms: history.since_ms,
                }
            }
            OP_STAKING => {
                parse_input::<serde_json::Map<String, Value>>(input)?;
                if !input.as_object().is_some_and(|x| x.is_empty()) {
                    return Err(GoFinError::validation("Staking read takes no parameters."));
                }
                ReadRequest::StakingState {
                    owner: self.account.owner.to_ascii_lowercase(),
                }
            }
            _ => {
                return Err(GoFinError::validation(
                    "Unsupported Papertrade read operation.",
                ));
            }
        };
        let mut result = self.runtime.read(request).await?;
        if operation == OP_TRADING_STATE {
            let minimum = minimum_paper_stake_raw(&result)?;
            let object = result.as_object_mut().ok_or_else(|| {
                err(
                    "PAPERTRADE_STATE_SNAPSHOT_INVALID",
                    "read",
                    "Trading state response is not an object.",
                )
            })?;
            object.insert("minimum_paper_stake_raw".into(), json!(minimum.to_string()));
        }
        Ok(result)
    }
    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare_with_context(operation, input, &Default::default())
            .await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &gofin_domain::PreparationContext,
    ) -> Result<PreparedOperation, GoFinError> {
        if operation == OP_INVALIDATE_CORE_NONCE {
            return prepare_core_nonce_recovery(self, input, context).await;
        }
        if operation == OP_FUNDING_CONTINUE {
            return prepare_funding_continuation(self, input, context).await;
        }
        if matches!(operation, OP_DEPOSIT | OP_WITHDRAW) {
            return prepare_funding_operation(self, operation, input, context).await;
        }
        if !write_definitions().iter().any(|d| d.name == operation) {
            return Err(GoFinError::validation(
                "Unsupported Papertrade write operation.",
            ));
        }
        if !self.account.signing_enabled {
            return Err(err(
                "SIGNING_DISABLED",
                "preview",
                "Signing is disabled for this Papertrade account.",
            ));
        }
        let now = Utc::now().timestamp().max(0) as u64;
        let request = intent_request(operation, &self.account.owner, input)?;
        request.validate(now)?;
        let valuation = self.runtime.valuation(&request).await?;
        let operation_id = context
            .logical_operation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let trading_state = self.runtime.read(ReadRequest::TradingState).await?;
        let canonical_start_block = indexed_through_block(&trading_state)?;
        enforce_minimum_paper_stake(&request, &trading_state)?;
        let mut close_positions_block = None;
        let close_positions = if let IntentRequest::Close { position_ids, .. } = &request {
            let snapshot = self
                .runtime
                .read(ReadRequest::Positions {
                    owner: self.account.owner.to_ascii_lowercase(),
                })
                .await?;
            close_positions_block = snapshot
                .get("indexedThroughBlock")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<u64>().ok());
            if close_positions_block.is_none() {
                return Err(err(
                    "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
                    "preview",
                    "The owner position snapshot omitted its exact indexed block.",
                ));
            }
            Some(select_close_positions(
                snapshot,
                &self.account.owner,
                position_ids,
            )?)
        } else {
            None
        };
        let bound = self
            .runtime
            .prepare_intent(request.clone(), &operation_id)
            .await?;
        bound.typed.validate(now)?;
        if !bound.matches_request(&request) {
            return Err(err(
                "PAPERTRADE_BINDING_INVALID",
                "preview",
                "Broker returned an invalid intent identity.",
            ));
        }
        let claimable = if matches!(request, IntentRequest::Claim { .. }) {
            let snapshot = self
                .runtime
                .read(ReadRequest::StakingState {
                    owner: self.account.owner.to_ascii_lowercase(),
                })
                .await?;
            let amount = claimable_rewards_wad(&snapshot)?;
            Some(amount)
        } else {
            None
        };
        let baseline_rows = close_positions.clone();
        let mut binding = json!({"bound_intent":bound,"intent_request":request,"operation_id":operation_id,"canonical_start_block":canonical_start_block,"claimable_rewards_minimum_raw":claimable.map(|_|"0"),"claimable_rewards_estimate_raw":claimable.map(|amount|amount.to_string()),"close_positions":close_positions});
        let mut intent = FinancialIntent::default();
        let resource = self.account.trading_cash_resource();
        match &request {
            IntentRequest::Open {
                instrument_id: _,
                is_long,
                margin_wad,
                leverage,
                ..
            } => {
                let margin = gofin_domain::Quantity::new(margin_wad.to_string())?;
                let notional = margin_wad
                    .checked_mul(u128::from(*leverage))
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "Papertrade notional exceeds the supported exact range.",
                        )
                    })?;
                let notional = gofin_domain::Quantity::new(notional.to_string())?;
                // Bind this exposure to the opening signed intent so multiple
                // positions in the same market remain independently settleable.
                let position = position_record(&bound.intent_id);
                let exposure_kind = if *is_long {
                    PositionKind::DerivativeLong
                } else {
                    PositionKind::DerivativeShort
                };
                let exposure_delta = SignedQuantity::positive(&notional);
                let resource = position_resource(&self.account.owner);
                intent.debits.push(ResourceAmount {
                    resource: self.account.trading_cash_resource(),
                    amount: Amount {
                        asset: trading_usdc_asset(),
                        atoms: margin.clone(),
                    },
                });
                intent.expected.push(EffectConstraint::Debit {
                    resource: self.account.trading_cash_resource(),
                    asset: trading_usdc_asset(),
                    maximum_debit: margin.clone(),
                });
                intent.expected.push(EffectConstraint::MarginPosition {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: trading_usdc_asset(),
                    position_kind: if *is_long {
                        PositionKind::MarginLong
                    } else {
                        PositionKind::MarginShort
                    },
                    minimum_delta: SignedQuantity::positive(&margin),
                    maximum_delta: SignedQuantity::positive(&margin),
                });
                intent.expected.push(EffectConstraint::DerivativePosition {
                    resource,
                    position,
                    asset: trading_usdc_asset(),
                    position_kind: exposure_kind,
                    minimum_delta: exposure_delta.clone(),
                    maximum_delta: exposure_delta,
                });
            }
            IntentRequest::Stake { amount, .. } => {
                intent.debits.push(ResourceAmount {
                    resource: paper_wallet_resource(&self.account.owner),
                    amount: Amount {
                        asset: paper_asset(),
                        atoms: gofin_domain::Quantity::new(amount.to_string())?,
                    },
                });
                intent.expected.push(EffectConstraint::Transfer {
                    from: paper_wallet_resource(&self.account.owner),
                    to: paper_staked_resource(&self.account.owner),
                    debit_asset: paper_asset(),
                    credit_asset: paper_asset(),
                    maximum_debit: gofin_domain::Quantity::new(amount.to_string())?,
                    minimum_credit: gofin_domain::Quantity::new(amount.to_string())?,
                });
            }
            IntentRequest::Unstake { amount, .. } => {
                intent.debits.push(ResourceAmount {
                    resource: paper_staked_resource(&self.account.owner),
                    amount: Amount {
                        asset: paper_asset(),
                        atoms: gofin_domain::Quantity::new(amount.to_string())?,
                    },
                });
                intent.expected.push(EffectConstraint::Transfer {
                    from: paper_staked_resource(&self.account.owner),
                    to: paper_wallet_resource(&self.account.owner),
                    debit_asset: paper_asset(),
                    credit_asset: paper_asset(),
                    maximum_debit: gofin_domain::Quantity::new(amount.to_string())?,
                    minimum_credit: gofin_domain::Quantity::new(amount.to_string())?,
                });
            }
            IntentRequest::Close { position_ids, .. } => {
                intent.locks.extend(position_ids.iter().map(|id| {
                    ResourceId(format!(
                        "papertrade:mainnet:{}:position:{}",
                        self.account.owner.to_ascii_lowercase(),
                        id
                    ))
                }));
                let positions = close_positions.as_ref().ok_or_else(|| {
                    err(
                        "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
                        "preview",
                        "Exact selected position pre-state is unavailable.",
                    )
                })?;
                let position_resource = position_resource(&self.account.owner);
                for row in positions {
                    let margin = row.margin_raw.parse::<u128>().map_err(|_| {
                        GoFinError::validation("Selected position margin is malformed.")
                    })?;
                    let leverage = u32::try_from(row.leverage).map_err(|_| {
                        GoFinError::validation("Selected position leverage is unsupported.")
                    })?;
                    let notional = margin.checked_mul(u128::from(leverage)).ok_or_else(|| {
                        GoFinError::validation("Selected position notional exceeds exact range.")
                    })?;
                    let position = position_snapshot_key(row);
                    let margin = gofin_domain::Quantity::new(margin.to_string())?;
                    let notional = gofin_domain::Quantity::new(notional.to_string())?;
                    let exposure_kind = if row.is_long {
                        PositionKind::DerivativeLong
                    } else {
                        PositionKind::DerivativeShort
                    };
                    let exposure_delta = SignedQuantity::negative(&notional);
                    intent.expected.push(EffectConstraint::MarginPosition {
                        resource: position_resource.clone(),
                        position: position.clone(),
                        asset: trading_usdc_asset(),
                        position_kind: if row.is_long {
                            PositionKind::MarginLong
                        } else {
                            PositionKind::MarginShort
                        },
                        minimum_delta: SignedQuantity::negative(&margin),
                        maximum_delta: SignedQuantity::negative(&margin),
                    });
                    intent.expected.push(EffectConstraint::DerivativePosition {
                        resource: position_resource.clone(),
                        position,
                        asset: trading_usdc_asset(),
                        position_kind: exposure_kind,
                        minimum_delta: exposure_delta.clone(),
                        maximum_delta: exposure_delta,
                    });
                }
                intent.expected.push(EffectConstraint::Settlement {
                    resource: self.account.trading_cash_resource(),
                    asset: trading_usdc_asset(),
                    category: SettlementCategory::RealizedPnl,
                    minimum_delta: SignedQuantity::positive(&gofin_domain::Quantity::from_u64(0)),
                    maximum_delta: SignedQuantity::positive(&gofin_domain::Quantity::new(
                        u128::MAX.to_string(),
                    )?),
                });
                intent.expected.push(EffectConstraint::Credit {
                    to: paper_wallet_resource(&self.account.owner),
                    asset: paper_asset(),
                    minimum_credit: gofin_domain::Quantity::from_u64(0),
                });
                intent
                    .expected
                    .push(EffectConstraint::OptionalPositionOfKind {
                        resource: payout_queue_resource(&self.account.owner),
                        position: format!("usdc_win_claim:{}", bound.intent_id),
                        asset: trading_usdc_asset(),
                        position_kind: PositionKind::Investment,
                        maximum_change: gofin_domain::Quantity::new(u128::MAX.to_string())?,
                    });
            }
            IntentRequest::Claim { .. } if claimable.is_some_and(|amount| amount > 0) => {
                intent.locks.push(resource.clone())
            }
            IntentRequest::Claim { .. } => {}
            _ => {}
        }
        if claimable.is_some_and(|amount| amount > 0) {
            intent.expected.push(EffectConstraint::Settlement {
                resource,
                asset: trading_usdc_asset(),
                category: SettlementCategory::Funding,
                minimum_delta: SignedQuantity::positive(&gofin_domain::Quantity::from_u64(0)),
                maximum_delta: SignedQuantity::positive(&gofin_domain::Quantity::new(
                    u128::MAX.to_string(),
                )?),
            });
        }
        let warnings = match request {
            IntentRequest::Open { .. } => vec!["Execution uses the protocol's market price when the intent executes; there is no price-limit signature field.".into()],
            IntentRequest::Close { .. } => vec![gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_WARNING.into()],
            IntentRequest::Stake { .. } | IntentRequest::Unstake { .. } => vec![gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_WARNING.into()],
            IntentRequest::RegisterSession { .. } => vec!["The broker generates and protects the session key; permissions are limited to bounded BTC/ETH trading intents.".into()],
            IntentRequest::Claim { .. } => vec!["Claim preview is an estimate capped by the available reward reserve; settlement requires the canonical Claimed amount, or the pinned staking contract's verified zero-return with no event.".into()],
            _ => Vec::new(),
        };
        let observed_at = Utc::now().to_rfc3339();
        if let Some(rows) = baseline_rows.as_deref() {
            let block = close_positions_block.ok_or_else(|| {
                err(
                    "PAPERTRADE_POSITION_SNAPSHOT_INVALID",
                    "preview",
                    "The owner position snapshot omitted its exact indexed block.",
                )
            })?;
            binding[gofin_domain::BASELINE_ANCHORS_KEY] = json!(position_snapshot_anchors(
                &self.account.owner,
                rows,
                block,
                &observed_at
            )?);
        }
        let typed_deadline = binding
            .get("bound_intent")
            .and_then(|b| b.get("typed"))
            .and_then(|t| t.get("deadline"))
            .and_then(|d| d.as_u64().or_else(|| d.as_str()?.parse().ok()));
        let expiry = typed_deadline
            .map(|d| d.min(now.saturating_add(5 * 60)))
            .unwrap_or_else(|| now.saturating_add(5 * 60));
        let expires_at = chrono::DateTime::<Utc>::from_timestamp(expiry as i64, 0)
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_DEADLINE_INVALID",
                    "preview",
                    "Bound Papertrade deadline is invalid.",
                )
            })?
            .to_rfc3339();
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding,
            summary: format!(
                "Prepare {operation} for Papertrade account {}.",
                self.account.name
            ),
            checks: vec![Check {
                name: "papertrade_request".into(),
                status: CheckStatus::Passed,
                detail: "Closed protocol request shape and exact quantities validated.".into(),
            }],
            warnings,
            observed_at,
            expires_at,
            valuation,
            intent,
            operation_version: 1,
            action_deadline: None,
        })
    }
    async fn prepare_configuration(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        if !configuration_definitions()
            .iter()
            .any(|d| d.name == operation)
        {
            return Err(GoFinError::validation(
                "Unsupported Papertrade configuration operation.",
            ));
        }
        if !self.account.signing_enabled {
            return Err(err(
                "SIGNING_DISABLED",
                "configuration",
                "Signing is disabled for this Papertrade account.",
            ));
        }
        let now = Utc::now().timestamp().max(0) as u64;
        let mut protected_revocation = None;
        let binding = match operation {
            OP_REGISTER => {
                let req: RegisterInput = parse_input(input)?;
                if req.registration_id.trim().is_empty()
                    || req.expiry <= now
                    || req.expiry - now > MAX_SESSION_SECONDS
                {
                    return Err(GoFinError::validation(
                        "Session expiry must be within the next 30 days.",
                    ));
                }
                let intent = self
                    .runtime
                    .prepare_session_registration(
                        &self.account.owner.to_ascii_lowercase(),
                        &req.registration_id,
                        req.expiry,
                        &req.registration_id,
                    )
                    .await?;
                intent.typed.validate(now)?;
                if !matches!(&intent.typed,PapertradeTypedIntent::RegisterSessionKey{user,key,expiry,..} if user.eq_ignore_ascii_case(&self.account.owner)&&valid_address(key)&&*expiry==req.expiry)
                {
                    return Err(err(
                        "PAPERTRADE_BINDING_INVALID",
                        "configuration",
                        "Broker returned a mismatched session registration binding.",
                    ));
                }
                let registration =
                    SessionRegistrationBinding::from_bound(req.registration_id, &intent).map_err(
                        |_| {
                            err(
                                "PAPERTRADE_BINDING_INVALID",
                                "configuration",
                                "Session registration public identity is invalid.",
                            )
                        },
                    )?;
                serde_json::to_value(SessionConfigurationBinding::Register { registration })
                    .map_err(|_| {
                        err(
                            "PAPERTRADE_BINDING_INVALID",
                            "configuration",
                            "Session registration binding could not be encoded.",
                        )
                    })?
            }
            OP_REVOKE => {
                let req: SessionInput = parse_input(input)?;
                let revoke = self
                    .runtime
                    .prepare_session_revocation(
                        &self.account.owner.to_ascii_lowercase(),
                        &req.registration_id,
                        &req.registration_id,
                    )
                    .await?;
                revoke.validate()?;
                if !revoke.owner.eq_ignore_ascii_case(&self.account.owner)
                    || revoke.registration_id != req.registration_id
                {
                    return Err(err(
                        "PAPERTRADE_BINDING_INVALID",
                        "configuration",
                        "Broker returned a mismatched session revocation transaction.",
                    ));
                }
                protected_revocation = Some(revoke.clone());
                let revocation = SessionRevocationBinding {
                    owner: revoke.owner,
                    registration_id: revoke.registration_id,
                    submission_id: revoke.submission_id,
                };
                serde_json::to_value(SessionConfigurationBinding::Revoke { revocation }).map_err(
                    |_| {
                        err(
                            "PAPERTRADE_BINDING_INVALID",
                            "configuration",
                            "Session revocation binding could not be encoded.",
                        )
                    },
                )?
            }
            _ => {
                return Err(GoFinError::validation(
                    "Unsupported Papertrade configuration operation.",
                ));
            }
        };
        let now_iso = Utc::now().to_rfc3339();
        // Registration is submitted through Papertrade's relayer with a zero
        // fee address; the owner does not pay an on-chain fee. Persist an
        // explicit zero valuation so the engine can account for the durable
        // configuration operation without inventing a priced movement.
        // Revocation is a direct owner-signed EVM transaction and must carry
        // a separately bounded, priced native-gas cost instead.
        let (valuation, intent) = if operation == OP_REGISTER {
            (
                Some(gofin_domain::Valuation {
                    movement_usd_micros: gofin_domain::Quantity::from_u64(0),
                    maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
                    additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
                    observed_at: now_iso.clone(),
                    source: "papertrade:relayed-session-registration-no-owner-fee:v1".into(),
                    prices: vec![],
                }),
                FinancialIntent::default(),
            )
        } else {
            let revocation = protected_revocation.as_ref().ok_or_else(|| {
                err("PAPERTRADE_REVOCATION_BOUND_TRANSACTION_MISSING", "configuration", "The immutable owner-signed revocation transaction is unavailable for fee accounting.")
            })?;
            let maximum_cost = revocation.transaction.maximum_cost()?.to_string();
            let asset = AssetId {
                network: "ethereum:999".into(),
                reference: "native".into(),
                decimals: 18,
            };
            let valuation = self.runtime.session_revocation_valuation(revocation).await?
                .ok_or_else(|| err("PAPERTRADE_REVOCATION_GAS_PRICE_UNAVAILABLE", "pricing", "A trusted native-asset price is required to approve the owner-paid session revocation gas bound."))?;
            let price = valuation
                .prices
                .iter()
                .find(|price| price.asset == asset)
                .ok_or_else(|| {
                    err(
                        "PAPERTRADE_REVOCATION_GAS_PRICE_INVALID",
                        "pricing",
                        "The revocation valuation does not price the exact HyperEVM native asset.",
                    )
                })?;
            let fee_usd = gofin_domain::Quantity::new(maximum_cost.clone())?
                .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?;
            if price.unit_atoms.is_zero()
                || price.usd_micros_per_unit.is_zero()
                || valuation.maximum_fee_usd_micros < fee_usd
            {
                return Err(err(
                    "PAPERTRADE_REVOCATION_GAS_PRICE_INVALID",
                    "pricing",
                    "The native price does not cover the exact frozen revocation gas bound.",
                ));
            }
            let mut intent = FinancialIntent::default();
            intent.maximum_costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: ResourceId(format!(
                        "ethereum:999:{}",
                        revocation.owner.to_ascii_lowercase()
                    )),
                },
                amount: Amount {
                    asset,
                    atoms: gofin_domain::Quantity::new(maximum_cost)?,
                },
                treatment: CostTreatment::AdditionalDebit,
            });
            (Some(valuation), intent)
        };
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding,
            summary: format!(
                "Prepare human-approved {operation} for Papertrade account {}.",
                self.account.name
            ),
            checks: vec![Check {
                name: "papertrade_configuration".into(),
                status: CheckStatus::Passed,
                detail: "Session authority and immutable transaction binding validated.".into(),
            }],
            warnings: vec![
                "Session authority changes require human-only configuration approval.".into(),
            ],
            observed_at: now_iso,
            expires_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            valuation,
            intent,
            operation_version: 1,
            action_deadline: None,
        })
    }
    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        if prepared.operation == OP_INVALIDATE_CORE_NONCE {
            let binding = core_nonce_recovery_binding(prepared)?;
            let request: CoreNonceRecoveryInput = parse_input(&prepared.input)?;
            if request.parent_operation_id != binding.parent_operation_id
                || binding.child_operation_id.trim().is_empty()
                || !binding.owner.eq_ignore_ascii_case(&self.account.owner)
                || binding.parent_submission.reference != binding.route_id
                || binding
                    .parent_submission
                    .data
                    .get("operation_id")
                    .and_then(Value::as_str)
                    != Some(binding.route_id.as_str())
            {
                return Err(err(
                    "PAPERTRADE_NONCE_RECOVERY_BINDING_CHANGED",
                    "revalidation",
                    "The recovery child no longer matches its protected parent and Core route.",
                ));
            }
            self.runtime
                .revalidate_funding_nonce_recovery(&binding)
                .await?;
            return Ok(prepared.clone());
        }
        if prepared.operation == OP_FUNDING_CONTINUE {
            let binding: FundingContinuationBinding = serde_json::from_value(
                prepared
                    .binding
                    .get("funding_continuation")
                    .cloned()
                    .ok_or_else(|| {
                        err(
                            "PAPERTRADE_CONTINUATION_BINDING_MISSING",
                            "revalidation",
                            "The continuation identity is missing.",
                        )
                    })?,
            )
            .map_err(|_| {
                err(
                    "PAPERTRADE_CONTINUATION_BINDING_INVALID",
                    "revalidation",
                    "The continuation identity is malformed.",
                )
            })?;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Request {
                route_id: String,
            }
            let request: Request = parse_input(&prepared.input)?;
            if binding.validate().is_err()
                || binding.operation_id.trim().is_empty()
                || binding.route_id != request.route_id
                || !binding
                    .intent
                    .owner
                    .eq_ignore_ascii_case(&self.account.owner)
                || !valid_continuation_identities(&binding)
            {
                return Err(err(
                    "PAPERTRADE_CONTINUATION_BINDING_CHANGED",
                    "revalidation",
                    "The continuation no longer matches the exact parent route and enrolled source/destination.",
                ));
            }
            self.runtime
                .revalidate_funding_continuation(&binding)
                .await?;
            let mut refreshed = prepared.clone();
            refreshed.valuation = self
                .runtime
                .funding_continuation_valuation(&binding)
                .await?;
            refreshed.observed_at = Utc::now().to_rfc3339();
            return Ok(refreshed);
        }
        if matches!(prepared.operation.as_str(), OP_DEPOSIT | OP_WITHDRAW) {
            let binding: FundingBinding = serde_json::from_value(
                prepared
                    .binding
                    .get("funding_binding")
                    .cloned()
                    .ok_or_else(|| {
                        err(
                            "PAPERTRADE_FUNDING_BINDING_MISSING",
                            "revalidation",
                            "The approved funding route identity is missing.",
                        )
                    })?,
            )
            .map_err(|_| {
                err(
                    "PAPERTRADE_FUNDING_BINDING_INVALID",
                    "revalidation",
                    "The approved funding route identity is malformed.",
                )
            })?;
            if binding.version != 1
                || binding.intent.owner != self.account.owner.to_ascii_lowercase()
                || binding.intent.request_id != binding.operation_id
                || binding.operation_id.trim().is_empty()
                || binding.intent.direction
                    != if prepared.operation == OP_DEPOSIT {
                        FundingDirection::Deposit
                    } else {
                        FundingDirection::Withdraw
                    }
                || !valid_funding_identities(
                    &binding.intent,
                    &binding.source_identity,
                    &binding.destination_identity,
                )
            {
                return Err(err(
                    "PAPERTRADE_FUNDING_BINDING_CHANGED",
                    "revalidation",
                    "The approved funding route no longer matches the enrolled account.",
                ));
            }
            validate_funding(&binding.intent)?;
            validate_funding_bounds(&binding.intent, &binding.bounds).map_err(|_| {
                err(
                    "PAPERTRADE_FUNDING_BOUNDS_INVALID",
                    "revalidation",
                    "The approved funding fee and net-credit bounds are malformed.",
                )
            })?;
            let mut refreshed = prepared.clone();
            refreshed.valuation = self.runtime.funding_valuation(&binding.intent).await?;
            refreshed.observed_at = Utc::now().to_rfc3339();
            return Ok(refreshed);
        }
        // Preserve the exact nonce, intent identity, and execution fields frozen
        // by the original approved binding. Revalidation only refreshes named
        // read state; it must not silently allocate a different operation.
        let mut refreshed = prepared.clone();
        let request: IntentRequest =
            serde_json::from_value(prepared.binding.get("intent_request").cloned().ok_or_else(
                || {
                    err(
                        "PAPERTRADE_BINDING_MISSING",
                        "revalidation",
                        "The approved request binding is missing.",
                    )
                },
            )?)
            .map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "revalidation",
                    "The approved request binding is invalid.",
                )
            })?;
        request.validate(Utc::now().timestamp().max(0) as u64)?;
        if matches!(request, IntentRequest::Stake { .. }) {
            let trading_state = self.runtime.read(ReadRequest::TradingState).await?;
            enforce_minimum_paper_stake(&request, &trading_state).map_err(|_| {
                err(
                    "PAPERTRADE_STAKE_MINIMUM_CHANGED",
                    "revalidation",
                    "The requested PAPER stake is below the current protocol minimum.",
                )
            })?;
        }
        if matches!(request, IntentRequest::Claim { .. }) {
            let snapshot = self
                .runtime
                .read(ReadRequest::StakingState {
                    owner: self.account.owner.to_ascii_lowercase(),
                })
                .await?;
            // Claim has no user-specified minimum. The preview amount is an
            // estimate capped by the observed reserve; the canonical Claimed
            // event establishes the actual paid amount, including zero.
            let _estimate = claimable_rewards_wad(&snapshot)?;
        }
        if let IntentRequest::Close { position_ids, .. } = &request {
            let approved: Vec<PositionSnapshot> = serde_json::from_value(
                prepared
                    .binding
                    .get("close_positions")
                    .cloned()
                    .ok_or_else(|| {
                        err(
                            "PAPERTRADE_BINDING_INVALID",
                            "revalidation",
                            "Exact close-position snapshot is missing.",
                        )
                    })?,
            )
            .map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "revalidation",
                    "Exact close-position snapshot is malformed.",
                )
            })?;
            let current = self
                .runtime
                .read(ReadRequest::Positions {
                    owner: self.account.owner.to_ascii_lowercase(),
                })
                .await?;
            if select_close_positions(current, &self.account.owner, position_ids)? != approved {
                return Err(err(
                    "PAPERTRADE_POSITION_CHANGED",
                    "revalidation",
                    "A selected position changed after the close preview.",
                ));
            }
        }
        let bound = if let Some(encoded) = prepared.binding.get("bound_intent") {
            let b: BoundIntent = serde_json::from_value(encoded.clone()).map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "revalidation",
                    "The approved intent binding is invalid.",
                )
            })?;
            b.typed.validate(Utc::now().timestamp().max(0) as u64)?;
            if !b
                .typed
                .user_address()
                .eq_ignore_ascii_case(&self.account.owner)
            {
                return Err(err(
                    "PAPERTRADE_OWNER_CHANGED",
                    "revalidation",
                    "The prepared intent owner no longer matches the enrolled account.",
                ));
            }
            if !b.matches_request(&request) {
                return Err(err(
                    "PAPERTRADE_BINDING_CHANGED",
                    "revalidation",
                    "Prepared signed fields no longer match the approved request.",
                ));
            }
            Some(b)
        } else {
            None
        };
        self.runtime
            .revalidate_intent(&request, bound.as_ref())
            .await?;
        refreshed.observed_at = Utc::now().to_rfc3339();
        Ok(refreshed)
    }

    fn validate_recovery_child(
        &self,
        prepared: &PreparedOperation,
        parent: &OperationRecord,
    ) -> Result<Option<gofin_domain::RecoveryBinding>, GoFinError> {
        if prepared.operation != OP_INVALIDATE_CORE_NONCE {
            return Ok(None);
        }
        let binding = core_nonce_recovery_binding(prepared)?;
        let link = gofin_domain::RecoveryBinding::from_prepared_binding(&prepared.binding)?;
        let Some(link) = link else {
            return Ok(None);
        };
        let parent_funding = parent
            .preview
            .as_ref()
            .filter(|preview| preview.operation == OP_DEPOSIT)
            .and_then(|preview| preview.binding.get("funding_binding"))
            .cloned()
            .and_then(|value| serde_json::from_value::<FundingBinding>(value).ok());
        let Some(parent_funding) = parent_funding else {
            return Ok(None);
        };
        if parent.operation != OP_DEPOSIT
            || parent_funding.intent.direction != FundingDirection::Deposit
            || parent_funding.intent.route != FundingRoute::Core
            || parent_funding.intent.owner != self.account.owner.to_ascii_lowercase()
            || binding.parent_operation_id != parent.id
            || binding.owner != self.account.owner.to_ascii_lowercase()
            || parent.submission.as_ref() != Some(&binding.parent_submission)
            || binding.route_id != parent_funding.operation_id
            || binding.route_id != binding.parent_submission.reference
            || binding
                .parent_submission
                .data
                .get("operation_id")
                .and_then(Value::as_str)
                != Some(binding.route_id.as_str())
            || link.parent_operation_id != parent.id
            || link.parent_submission != binding.parent_submission
            || !prepared.intent.debits.is_empty()
            || !prepared.intent.maximum_costs.is_empty()
            || !prepared.intent.expected.is_empty()
            || prepared.intent.locks.len() != 1
        {
            return Ok(None);
        }
        Ok(Some(link))
    }
    async fn revalidate_configuration(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        let binding: SessionConfigurationBinding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "configuration",
                    "Session configuration binding is invalid.",
                )
            })?;
        self.runtime
            .revalidate_session_configuration(&binding)
            .await?;
        let mut refreshed = prepared.clone();
        refreshed.observed_at = Utc::now().to_rfc3339();
        Ok(refreshed)
    }
    async fn submit_configuration(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        journal.assert_candidate(prepared)?;
        let binding: SessionConfigurationBinding = serde_json::from_value(prepared.binding.clone())
            .map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "submission",
                    "Session configuration binding is invalid.",
                )
            })?;
        let submission = match &binding {
            SessionConfigurationBinding::Register { registration } => Submission {
                kind: "papertrade_session_register".into(),
                reference: registration.intent_id.clone(),
                data: json!({"intent_id":registration.intent_id,"registration_id":registration.registration_id,"owner":registration.owner,"expiry":registration.expiry}),
            },
            SessionConfigurationBinding::Revoke { revocation } => Submission {
                kind: "papertrade_session_revoke".into(),
                reference: revocation.submission_id.clone(),
                data: json!({"registration_id":revocation.registration_id,"owner":revocation.owner,"submission_id":revocation.submission_id}),
            },
        };
        journal.persist_submission(&submission)?;
        journal.assert_candidate(prepared)?;
        let receipt = match &binding {
            SessionConfigurationBinding::Register { registration } => {
                self.runtime
                    .dispatch_session_registration(registration)
                    .await?
            }
            SessionConfigurationBinding::Revoke { revocation } => {
                self.runtime
                    .dispatch_session_revocation(revocation, journal.clone())
                    .await?
            }
        };
        journal.persist_dispatch_evidence(&submission, &json!({"receipt":receipt}))?;
        Ok(submission)
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        if prepared.operation == OP_INVALIDATE_CORE_NONCE {
            let binding = core_nonce_recovery_binding(prepared)?;
            let submission = core_nonce_recovery_submission(&binding);
            journal.persist_submission(&submission)?;
            journal.assert_candidate(prepared)?;
            let progress = self
                .runtime
                .dispatch_funding_nonce_recovery(&binding, &submission, validated, journal.clone())
                .await?;
            journal.persist_dispatch_evidence(
                &submission,
                &json!({"recovery_status":progress.status,"evidence":progress.evidence}),
            )?;
            return Ok(submission);
        }
        if prepared.operation == OP_FUNDING_CONTINUE {
            let binding: FundingContinuationBinding = serde_json::from_value(
                prepared
                    .binding
                    .get("funding_continuation")
                    .cloned()
                    .ok_or_else(|| {
                        err(
                            "PAPERTRADE_CONTINUATION_BINDING_MISSING",
                            "submission",
                            "The continuation identity is missing.",
                        )
                    })?,
            )
            .map_err(|_| {
                err(
                    "PAPERTRADE_CONTINUATION_BINDING_INVALID",
                    "submission",
                    "The continuation identity is malformed.",
                )
            })?;
            binding.validate()?;
            let submission = funding_continuation_submission(&binding);
            journal.persist_submission(&submission)?;
            journal.assert_candidate(prepared)?;
            let progress = self
                .runtime
                .dispatch_funding_continuation(&binding, &submission, validated, journal.clone())
                .await?;
            journal.persist_dispatch_evidence(
                &submission,
                &json!({"continuation_status":progress.status,"evidence":progress.evidence}),
            )?;
            return Ok(submission);
        }
        if !matches!(prepared.operation.as_str(), OP_DEPOSIT | OP_WITHDRAW) {
            return self.submit(prepared, journal).await;
        }
        let binding: FundingBinding = serde_json::from_value(
            prepared
                .binding
                .get("funding_binding")
                .cloned()
                .ok_or_else(|| {
                    err(
                        "PAPERTRADE_FUNDING_BINDING_MISSING",
                        "submission",
                        "The prepared funding plan identity is missing.",
                    )
                })?,
        )
        .map_err(|_| {
            err(
                "PAPERTRADE_FUNDING_BINDING_INVALID",
                "submission",
                "The prepared funding plan identity is invalid.",
            )
        })?;
        if binding.version != 1
            || binding.operation_id.trim().is_empty()
            || binding.intent.owner != self.account.owner.to_ascii_lowercase()
            || binding.intent.request_id != binding.operation_id
        {
            return Err(err(
                "PAPERTRADE_FUNDING_BINDING_INVALID",
                "submission",
                "The funding plan no longer matches the enrolled account.",
            ));
        }
        let submission = funding_submission(&binding);
        journal.persist_submission(&submission)?;
        journal.assert_candidate(prepared)?;
        let progress = self
            .runtime
            .dispatch_funding(&binding, &submission, validated, journal.clone())
            .await?;
        journal.persist_dispatch_evidence(
            &submission,
            &json!({"funding_status":progress.status,"evidence_id":progress.receipt.as_ref().map(|r|r.evidence_id.as_str())}),
        )?;
        Ok(submission)
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        journal.assert_candidate(prepared)?;
        if matches!(
            prepared.operation.as_str(),
            OP_DEPOSIT | OP_WITHDRAW | OP_FUNDING_CONTINUE
        ) {
            return Err(err(
                "PAPERTRADE_FUNDING_REVALIDATION_REQUIRED",
                "submission",
                "Funding routes must be submitted through the final revalidated broker authority.",
            ));
        }
        let request: IntentRequest =
            serde_json::from_value(prepared.binding.get("intent_request").cloned().ok_or_else(
                || {
                    err(
                        "PAPERTRADE_BINDING_MISSING",
                        "submission",
                        "Prepared intent request is missing.",
                    )
                },
            )?)
            .map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "submission",
                    "Prepared intent request is invalid.",
                )
            })?;
        if matches!(
            request,
            IntentRequest::RevokeSession { .. } | IntentRequest::RegisterSession { .. }
        ) {
            return Err(err(
                "WRONG_OPERATION_KIND",
                "submission",
                "Session keys must use the human-only configuration lifecycle.",
            ));
        }
        let submission = operation_submission(&bound(prepared)?);
        journal.persist_submission(&submission)?;
        journal.assert_candidate(prepared)?;
        let receipt = if let Some(b) = prepared.binding.get("bound_intent") {
            let b: BoundIntent = serde_json::from_value(b.clone()).map_err(|_| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "submission",
                    "Prepared intent binding is invalid.",
                )
            })?;
            self.runtime.dispatch(&b).await?
        } else {
            return Err(err(
                "PAPERTRADE_BINDING_INVALID",
                "submission",
                "Prepared Papertrade write has no dispatchable binding.",
            ));
        };
        journal.persist_dispatch_evidence(&submission, &json!({"receipt":receipt}))?;
        Ok(submission)
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, GoFinError> {
        if record.operation == OP_FUNDING_CONTINUE {
            let Some(preview) = record.preview.as_ref() else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Continuation preview is unavailable."}),
                    verified: None,
                });
            };
            return reconcile_funding_continuation_operation(self, record, preview).await;
        }
        if record.operation == OP_INVALIDATE_CORE_NONCE {
            let Some(preview) = record.preview.as_ref() else {
                return Ok(unknown_recovery("The recovery preview is unavailable."));
            };
            let binding = core_nonce_recovery_binding(preview)?;
            let Some(submission) = record.submission.as_ref() else {
                return Ok(unknown_recovery(
                    "The exact recovery child submission is unavailable.",
                ));
            };
            let progress = self
                .runtime
                .reconcile_funding_nonce_recovery(&binding, Some(submission))
                .await?;
            return match progress.status {
                CoreDepositNonceRecoveryStatus::Pending => Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: progress.evidence,
                    verified: None,
                }),
                CoreDepositNonceRecoveryStatus::Unknown => Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: progress.evidence,
                    verified: None,
                }),
                CoreDepositNonceRecoveryStatus::Rejected => Ok(Reconciliation {
                    status: Status::Rejected,
                    evidence: progress.evidence,
                    verified: None,
                }),
                CoreDepositNonceRecoveryStatus::Settled => {
                    let Some(proof) = progress.proof else {
                        return Ok(unknown_recovery(
                            "The runtime reported a settled Noop without its linked protected proof.",
                        ));
                    };
                    if binding.validate().is_err()
                        || proof.validate_against(&binding).is_err()
                        || proof.child_submission != *submission
                        || proof.child_record_id != record.id
                    {
                        return Ok(unknown_recovery(
                            "The successful Noop proof does not match the exact durable recovery child.",
                        ));
                    }
                    let mut verified = VerifiedEffects {
                        evidence_version: 1,
                        submission: submission.clone(),
                        effects: Vec::new(),
                        evidence: json!({"core_deposit_nonce_recovery":proof}),
                        observed_at: Utc::now().to_rfc3339(),
                        proof_digest: String::new(),
                    };
                    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                    Ok(Reconciliation {
                        status: Status::Completed,
                        evidence: verified.evidence.clone(),
                        verified: Some(verified),
                    })
                }
            };
        }
        if let Some(preview) = record
            .preview
            .as_ref()
            .filter(|preview| matches!(preview.operation.as_str(), OP_DEPOSIT | OP_WITHDRAW))
        {
            return reconcile_funding_operation(self, record, preview).await;
        }
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Papertrade submission identity is present."}),
                verified: None,
            });
        };
        let Some(preview) = record.preview.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"The original prepared typed intent is unavailable."}),
                verified: None,
            });
        };
        let Some(encoded) = preview.binding.get("bound_intent") else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"The exact typed intent binding is unavailable."}),
                verified: None,
            });
        };
        let bound: BoundIntent = serde_json::from_value(encoded.clone()).map_err(|_| {
            err(
                "PAPERTRADE_BINDING_INVALID",
                "reconciliation",
                "Durable typed intent binding is malformed.",
            )
        })?;
        let start_block = preview
            .binding
            .get("canonical_start_block")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                err(
                    "PAPERTRADE_BINDING_INVALID",
                    "reconciliation",
                    "Canonical intent search start block is missing.",
                )
            })?;
        if let PapertradeTypedIntent::CancelIntent {
            user, intent_id, ..
        } = &bound.typed
        {
            let observation = self
                .runtime
                .cancel_status(user, intent_id, start_block)
                .await?;
            let Some(observation) = observation else {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"reason":"No exact target-intent terminal cancellation state has been observed yet.","target_intent_id":intent_id}),
                    verified: None,
                });
            };
            if !observation.owner.eq_ignore_ascii_case(&self.account.owner)
                || !observation.target_intent_id.eq_ignore_ascii_case(intent_id)
                || bound.cancel_target_action.as_deref() != Some(observation.action.as_str())
                || !is_cancellable_target_action(&observation.action)
                || (matches!(observation.source, CancelStatusSource::WalletSse)
                    && (observation.account_revision.is_none()
                        || observation
                            .indexed_through_block
                            .is_none_or(|block| block < start_block)))
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Cancel status observation does not bind the enrolled owner, exact target, action, and source freshness."}),
                    verified: None,
                });
            }
            match observation.state {
                CancelIntentState::Cancelled => {
                    let mut verified = VerifiedEffects {
                        evidence_version: 1,
                        submission: submission.clone(),
                        effects: Vec::new(),
                        evidence: json!({"cancel_status":observation,"signed_cancel_intent_id":bound.intent_id}),
                        observed_at: Utc::now().to_rfc3339(),
                        proof_digest: String::new(),
                    };
                    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                    return Ok(Reconciliation {
                        status: Status::Completed,
                        evidence: json!({"cancel_status":observation,"terminal":true}),
                        verified: Some(verified),
                    });
                }
                CancelIntentState::Succeeded
                | CancelIntentState::Failed
                | CancelIntentState::Expired => {
                    let evidence = json!({
                        "cancel_status":observation,
                        "terminal":true,
                        "reason":"The target reached a terminal state before cancellation completed."
                    });
                    let mut verified = VerifiedEffects {
                        evidence_version: 1,
                        submission: submission.clone(),
                        effects: Vec::new(),
                        evidence: evidence.clone(),
                        observed_at: Utc::now().to_rfc3339(),
                        proof_digest: String::new(),
                    };
                    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                    return Ok(Reconciliation {
                        status: Status::Failed,
                        evidence,
                        verified: Some(verified),
                    });
                }
                CancelIntentState::Pending | CancelIntentState::Unknown => {
                    return Ok(Reconciliation {
                        status: Status::Pending,
                        evidence: json!({"cancel_status":observation,"terminal":false}),
                        verified: None,
                    });
                }
            }
        }
        let expected_tx_hash = record
            .dispatch_evidence
            .as_ref()
            .and_then(|v| v.pointer("/receipt/transaction_hash"))
            .and_then(Value::as_str);
        let tx = match self
            .runtime
            .canonical_intent_transaction(&bound.intent_id, start_block)
            .await
        {
            Ok(Some(tx)) => tx,
            Ok(None) => {
                if let Some(proof) = self.runtime.intent_expiry_proof(&bound).await?
                    && let Some(expired) = expired_intent_reconciliation(submission, &bound, proof)?
                {
                    return Ok(expired);
                }
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"reason":"No canonical BatchExecutor execution has been found for this durable intent ID yet.","intent_id":bound.intent_id,"from_block":start_block}),
                    verified: None,
                });
            }
            Err(lookup_error) => {
                if let Ok(Some(proof)) = self.runtime.intent_expiry_proof(&bound).await
                    && let Ok(Some(expired)) =
                        expired_intent_reconciliation(submission, &bound, proof)
                {
                    return Ok(expired);
                }
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({
                        "reason":"Canonical intent lookup failed and no exact expiry proof was available.",
                        "intent_id":bound.intent_id,
                        "from_block":start_block,
                        "lookup_error_code":lookup_error.code
                    }),
                    verified: None,
                });
            }
        };
        if expected_tx_hash.is_some_and(|hash| !tx.transaction_hash.eq_ignore_ascii_case(hash)) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"Canonical intent lookup found a transaction different from the durable dispatch receipt."}),
                verified: None,
            });
        }
        match verify_canonical_call(&tx, &bound.typed, &bound.intent_id) {
            Ok(proof) => {
                let verified = staking_effects(
                    &bound.typed,
                    preview,
                    &proof,
                    submission.clone(),
                    &self.account,
                )?
                .or(open_position_effects(
                    &bound.typed,
                    &proof,
                    submission.clone(),
                    &self.account,
                )?)
                .or(close_position_effects(
                    &bound.typed,
                    preview,
                    &proof,
                    submission.clone(),
                    &self.account,
                )?);
                if let Some(verified) = verified {
                    Ok(Reconciliation {
                        status: Status::Completed,
                        evidence: json!({"canonical_call":proof,"settlement":"exact protocol accounting effect proved"}),
                        verified: Some(verified),
                    })
                } else {
                    Ok(Reconciliation {
                        status: Status::Pending,
                        evidence: json!({"canonical_call":proof,"settlement":"canonical call/event verified; accounting effects remain unproved"}),
                        verified: None,
                    })
                }
            }
            Err(_) => match crate::verify_rejected_call(&tx, &bound.typed, &bound.intent_id) {
                Ok(Some(rejected)) => Ok(Reconciliation {
                    status: Status::Rejected,
                    evidence: json!({
                        "terminal_call_outcome":rejected.kind,
                        "transaction_hash":rejected.transaction_hash,
                        "block_hash":rejected.block_hash,
                        "block_number":rejected.block_number,
                        "batch_id":rejected.batch_id,
                        "batch_item_index":rejected.batch_item_index,
                        "intent_id":rejected.intent_id,
                        "event_id":rejected.event_id,
                        "financial_effects":false
                    }),
                    verified: None,
                }),
                Ok(None) | Err(_) => Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"The canonical receipt did not prove this exact typed intent or a pinned terminal no-fill outcome; signed payload and raw logs are withheld."}),
                    verified: None,
                }),
            },
        }
    }

    async fn reconcile_configuration(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation, GoFinError> {
        let Some(_submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable configuration submission identity is present."}),
                verified: None,
            });
        };
        let binding: SessionConfigurationBinding = serde_json::from_value(
            record
                .preview
                .as_ref()
                .ok_or_else(|| {
                    err(
                        "PAPERTRADE_BINDING_MISSING",
                        "reconciliation",
                        "Configuration preview is missing.",
                    )
                })?
                .binding
                .clone(),
        )
        .map_err(|_| {
            err(
                "PAPERTRADE_BINDING_INVALID",
                "reconciliation",
                "Configuration binding is invalid.",
            )
        })?;
        let tx_hash = record
            .dispatch_evidence
            .as_ref()
            .and_then(|v| v.pointer("/receipt/transaction_hash"))
            .and_then(Value::as_str);
        let tx = if let Some(tx_hash) = tx_hash {
            self.runtime.canonical_transaction(tx_hash).await?
        } else if let SessionConfigurationBinding::Register { registration } = &binding {
            let Some(intent) = self
                .runtime
                .session_registration_intent(registration)
                .await?
            else {
                if let Some(expired) = expired_session_registration_reconciliation(
                    self.runtime.as_ref(),
                    record,
                    registration,
                )
                .await?
                {
                    return Ok(expired);
                }
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Protected session registration intent is unavailable for canonical receipt discovery."}),
                    verified: None,
                });
            };
            let Some(from_block) = self
                .runtime
                .session_registration_from_block(registration)
                .await?
            else {
                if let Some(expired) = expired_session_registration_reconciliation(
                    self.runtime.as_ref(),
                    record,
                    registration,
                )
                .await?
                {
                    return Ok(expired);
                }
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"No durable registration dispatch block is available for bounded canonical receipt discovery."}),
                    verified: None,
                });
            };
            self.runtime
                .canonical_intent_transaction(&intent.intent_id, from_block)
                .await?
        } else {
            None
        };
        let Some(tx) = tx else {
            if let SessionConfigurationBinding::Register { registration } = &binding
                && let Some(expired) = expired_session_registration_reconciliation(
                    self.runtime.as_ref(),
                    record,
                    registration,
                )
                .await?
            {
                return Ok(expired);
            }
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"reason":"Configuration transaction is not yet discoverable as canonical.","transaction_hash":tx_hash}),
                verified: None,
            });
        };
        if tx_hash.is_some_and(|hash| !tx.transaction_hash.eq_ignore_ascii_case(hash)) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"Canonical RPC provider returned a different transaction hash."}),
                verified: None,
            });
        }
        match binding {
            SessionConfigurationBinding::Register { registration } => {
                let Some(intent) = self
                    .runtime
                    .session_registration_intent(&registration)
                    .await?
                else {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Protected session registration binding is unavailable."}),
                        verified: None,
                    });
                };
                if intent.intent_id != registration.intent_id {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Protected registration identity does not match the durable submission."}),
                        verified: None,
                    });
                }
                match verify_canonical_call(&tx, &intent.typed, &intent.intent_id) {
                    Ok(proof) => {
                        if proof.owner != registration.owner.to_ascii_lowercase()
                            || proof.intent_id != registration.intent_id.to_ascii_lowercase()
                        {
                            return Ok(Reconciliation {
                                status: Status::Unknown,
                                evidence: json!({"reason":"Canonical registration proof differs from the durable public identity."}),
                                verified: None,
                            });
                        }
                        self.runtime
                            .activate_session_registration(&registration, &proof)
                            .await?;
                        let evidence = json!({"canonical_session_registration":{
                            "transaction_hash":proof.transaction_hash,
                            "block_hash":proof.block_hash,
                            "block_number":proof.block_number,
                            "intent_id":proof.intent_id,
                            "event_ids":proof.event_ids
                        }});
                        let verified = configuration_verified_effects(record, evidence.clone())?;
                        Ok(Reconciliation {
                            status: Status::Completed,
                            evidence,
                            verified: Some(verified),
                        })
                    }
                    Err(_) => {
                        if let Some(expired) = expired_session_registration_reconciliation(
                            self.runtime.as_ref(),
                            record,
                            &registration,
                        )
                        .await?
                        {
                            return Ok(expired);
                        }
                        Ok(Reconciliation {
                            status: Status::Unknown,
                            evidence: json!({"reason":"Canonical receipt did not prove exact session registration."}),
                            verified: None,
                        })
                    }
                }
            }
            SessionConfigurationBinding::Revoke { revocation } => {
                let Some(revoke) = self
                    .runtime
                    .session_revocation_transaction(&revocation)
                    .await?
                else {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Protected frozen revocation transaction is unavailable."}),
                        verified: None,
                    });
                };
                if !revoke.owner.eq_ignore_ascii_case(&revocation.owner)
                    || revoke.registration_id != revocation.registration_id
                    || revoke.submission_id != revocation.submission_id
                {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Protected revocation transaction differs from its durable public identity."}),
                        verified: None,
                    });
                }
                match verify_session_revocation(&tx, &revoke) {
                    Ok(proof) => {
                        self.runtime
                            .deactivate_session_registration(&revocation, &proof)
                            .await?;
                        let evidence = json!({"canonical_session_revocation":{
                            "transaction_hash":proof.transaction_hash,
                            "block_hash":proof.block_hash,
                            "block_number":proof.block_number,
                            "owner":proof.owner,
                            "event_id":proof.event_id,
                            "actual_gas_fee_atoms":proof.actual_gas_fee_atoms,
                            "gas_used":tx.gas_used,
                            "effective_gas_price":tx.effective_gas_price
                        }});
                        let verified = session_revocation_verified_effects(
                            record,
                            &proof.owner,
                            &proof.actual_gas_fee_atoms,
                            evidence.clone(),
                        )?;
                        Ok(Reconciliation {
                            status: Status::Completed,
                            evidence,
                            verified: Some(verified),
                        })
                    }
                    Err(_) => match verify_session_revocation_failure(&tx, &revoke) {
                        Ok(proof) => {
                            let evidence = json!({"canonical_session_revocation_failure":{
                                "transaction_hash":proof.transaction_hash,
                                "block_hash":proof.block_hash,
                                "block_number":proof.block_number,
                                "owner":proof.owner,
                                "actual_gas_fee_atoms":proof.actual_gas_fee_atoms,
                                "gas_used":tx.gas_used,
                                "effective_gas_price":tx.effective_gas_price
                            }});
                            let verified = session_revocation_verified_effects(
                                record,
                                &proof.owner,
                                &proof.actual_gas_fee_atoms,
                                evidence.clone(),
                            )?;
                            Ok(Reconciliation {
                                status: Status::Failed,
                                evidence,
                                verified: Some(verified),
                            })
                        }
                        Err(_) => Ok(Reconciliation {
                            status: Status::Unknown,
                            evidence: json!({"reason":"Canonical receipt did not prove exact session revocation success or a matching reverted owner transaction."}),
                            verified: None,
                        }),
                    },
                }
            }
        }
    }
}

async fn reconcile_funding_operation<R: PapertradeRuntime + 'static>(
    adapter: &PapertradeAdapter<R>,
    record: &OperationRecord,
    preview: &PreparedOperation,
) -> Result<Reconciliation, GoFinError> {
    let Some(submission) = record.submission.clone() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable Papertrade funding submission identity is present."}),
            verified: None,
        });
    };
    let binding: FundingBinding =
        serde_json::from_value(preview.binding.get("funding_binding").cloned().ok_or_else(
            || {
                err(
                    "PAPERTRADE_FUNDING_BINDING_MISSING",
                    "reconciliation",
                    "The funding plan identity is missing.",
                )
            },
        )?)
        .map_err(|_| {
            err(
                "PAPERTRADE_FUNDING_BINDING_INVALID",
                "reconciliation",
                "The funding plan identity is invalid.",
            )
        })?;
    if funding_submission(&binding) != submission {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"The durable funding submission does not match the persisted route identity."}),
            verified: None,
        });
    }
    // Core WithdrawToCore has an independent terminal-expiry proof. Ask for
    // it before the funding host performs a potentially expensive multi-leg
    // canonical scan. Wall-clock expiry only enables this proof lookup; only
    // the proof's canonical post-deadline block, unused nonce, pinned code and
    // absent intent can terminalize the operation.
    if let Some(bound) = binding.papertrade_intent.as_ref()
        && binding.intent.direction == FundingDirection::Withdraw
        && core_withdrawal_binding_matches(&binding, bound)
        && core_withdrawal_deadline(bound)
            .is_some_and(|deadline| Utc::now().timestamp().max(0) as u64 > deadline)
        && let Ok(Some(proof)) = adapter.runtime.core_withdrawal_expiry_proof(bound).await
        && proof.validate_against(bound).is_ok()
    {
        let evidence = json!({
            "core_withdrawal_expired": {
                "intent_id": proof.intent_id,
                "owner": proof.owner,
                "deadline": proof.deadline,
                "canonical_block_number": proof.canonical_block_number,
                "canonical_block_hash": proof.canonical_block_hash,
                "canonical_block_timestamp": proof.canonical_block_timestamp,
                "exchange_proxy": proof.exchange_proxy,
                "exchange_implementation": proof.exchange_implementation,
                "exchange_code_hash": proof.exchange_code_hash,
                "session_key_manager_proxy": proof.session_key_manager_proxy,
                "session_key_manager_implementation": proof.session_key_manager_implementation,
                "session_key_manager_code_hash": proof.session_key_manager_code_hash,
                "nonce_unused": proof.nonce_unused,
                "intent_absent": proof.intent_absent
            },
            "financial_effects": []
        });
        let verified = configuration_verified_effects(record, evidence)?;
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        });
    }
    let progress = adapter.runtime.reconcile_funding(&binding).await?;
    if progress.status == FundingProgressStatus::Rejected
        && let Some(terminal) = progress
            .evidence
            .get("core_deposit_nonce_recovery_terminal")
        && let Some(verified) =
            verified_core_nonce_invalidation_failure(record, &submission, &binding, terminal)?
    {
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        });
    }
    if proven_local_funding_key_failure(
        record,
        &progress.evidence,
        &binding.operation_id,
        expected_unsubmitted_funding_leg_count(&binding),
    ) {
        // The development provider rejected an overlong immutable journal key
        // before the host wrote its attempt marker. Every funding host writes
        // that marker before dispatch and reconciliation reads the protected
        // markers for each frozen leg. Together with the exact persisted local
        // validation error, this proves no external write was attempted.
        let evidence = json!({
            "state":"not_dispatched",
            "reason":"bounded_runtime_record_key_rejected_before_any_leg_attempt_marker",
            "operation_id":binding.operation_id,
            "original_error_code":"INVALID_INPUT",
            "legs":progress.evidence["legs"],
            "financial_effects":[]
        });
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![],
            evidence: evidence.clone(),
            observed_at: Utc::now().to_rfc3339(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
        return Ok(Reconciliation {
            status: Status::Failed,
            evidence,
            verified: Some(verified),
        });
    }
    match progress.status {
        FundingProgressStatus::Pending => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"funding_status":"pending","route":binding.intent.route,"direction":binding.intent.direction}),
            verified: None,
        }),
        FundingProgressStatus::Unknown => Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"funding_status":"unknown","route":binding.intent.route,"direction":binding.intent.direction}),
            verified: None,
        }),
        FundingProgressStatus::Rejected => Ok(Reconciliation {
            status: Status::Rejected,
            evidence: json!({"funding_status":"rejected","evidence_id":progress.receipt.map(|r|r.evidence_id)}),
            verified: None,
        }),
        FundingProgressStatus::Settled => {
            let receipt = progress.receipt.ok_or_else(|| {
                err(
                    "PAPERTRADE_FUNDING_PROOF_MISSING",
                    "reconciliation",
                    "Funding runtime reported settlement without destination proof.",
                )
            })?;
            let expected_source_debit = match binding.intent.direction {
                FundingDirection::Deposit => binding
                    .bounds
                    .source_usdc_atoms
                    .checked_add(receipt.actual_source_fee_usdc_atoms),
                FundingDirection::Withdraw => Some(binding.bounds.source_usdc_atoms),
            };
            let deposit_components = receipt
                .destination_credit_usdc_atoms
                .checked_add(receipt.actual_cctp_fee_usdc_atoms)
                .and_then(|value| value.checked_add(receipt.activation_fee_usdc_atoms));
            let withdrawal_components = receipt
                .destination_credit_usdc_atoms
                .checked_add(receipt.actual_source_fee_usdc_atoms)
                .and_then(|value| value.checked_add(receipt.actual_cctp_fee_usdc_atoms))
                .and_then(|value| value.checked_add(receipt.activation_fee_usdc_atoms))
                .and_then(|value| value.checked_add(receipt.retained_source_credit_usdc_atoms));
            let is_core_credit_stage = receipt.stage == FundingReceiptStage::CoreCredit;
            if receipt.status != "settled"
                || !funding_receipt_stage_matches(&binding.intent, &receipt)
                || receipt.amount_usdc_atoms != receipt.destination_credit_usdc_atoms
                || receipt.source_debit_usdc_atoms != expected_source_debit.unwrap_or(u64::MAX)
                || receipt.actual_source_fee_usdc_atoms
                    > binding.bounds.maximum_source_fee_usdc_atoms
                || receipt.actual_cctp_fee_usdc_atoms > binding.bounds.maximum_cctp_fee_usdc_atoms
                || receipt.activation_fee_usdc_atoms > binding.bounds.maximum_activation_fee_atoms
                || receipt.destination_credit_usdc_atoms < binding.bounds.destination_min_usdc_atoms
                || receipt.destination_credit_usdc_atoms > binding.bounds.source_usdc_atoms
                || match binding.intent.direction {
                    FundingDirection::Deposit => {
                        deposit_components != Some(binding.bounds.source_usdc_atoms)
                            || receipt.retained_source_credit_usdc_atoms != 0
                    }
                    FundingDirection::Withdraw => {
                        withdrawal_components != Some(binding.bounds.source_usdc_atoms)
                    }
                }
                || receipt.evidence_id.trim().is_empty()
                || !funding_identities_match(&binding.intent, &receipt)
                || !funding_retained_native_matches(&binding, &receipt)
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Destination proof does not bind the exact funding amount and route."}),
                    verified: None,
                });
            }
            let wad = u128::from(receipt.destination_credit_usdc_atoms)
                .checked_mul(1_000_000_000_000)
                .ok_or_else(|| GoFinError::validation("Internal USDC amount overflow."))?;
            let qty = gofin_domain::Quantity::new(wad.to_string())?;
            if !valid_funding_identities(
                &binding.intent,
                &binding.source_identity,
                &binding.destination_identity,
            ) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Funding binding account identities are invalid for the fixed route."}),
                    verified: None,
                });
            }
            let route_identity = if is_core_credit_stage {
                // The parent withdrawal's final route remains in the binding
                // for the separately authorized continuation. This receipt
                // credits only the proven first-stage Core balance.
                format!(
                    "hyperliquid:mainnet:{}:spot:primary",
                    binding.intent.owner.to_ascii_lowercase()
                )
            } else {
                match binding.intent.direction {
                    FundingDirection::Deposit => binding.source_identity.clone(),
                    FundingDirection::Withdraw => binding.destination_identity.clone(),
                }
            };
            let source_resource = ResourceId(route_identity.clone());
            let source_route = if is_core_credit_stage {
                FundingRoute::Core
            } else {
                binding.intent.route.clone()
            };
            let source_asset = route_usdc_asset(&source_route);
            let mut effects = Vec::new();
            match binding.intent.direction {
                FundingDirection::Deposit => {
                    effects.push(FinancialEffect::CashDebit {
                        resource: source_resource.clone(),
                        amount: Amount {
                            asset: source_asset.clone(),
                            atoms: route_atoms(
                                &binding.intent.route,
                                binding.bounds.source_usdc_atoms,
                            )?,
                        },
                    });
                    effects.push(FinancialEffect::Settlement {
                        resource: adapter.account.trading_cash_resource(),
                        asset: trading_usdc_asset(),
                        category: SettlementCategory::Funding,
                        delta: SignedQuantity::positive(&qty),
                    });
                }
                FundingDirection::Withdraw => {
                    // The approved Papertrade withdrawal principal is the
                    // exact internal cash debit. Any separately proven
                    // source-side fee is represented by its own Cost effect
                    // below, so using the receipt's gross debit here would
                    // count that fee twice.
                    let debit = u128::from(receipt.source_debit_usdc_atoms)
                        .checked_mul(1_000_000_000_000)
                        .ok_or_else(|| GoFinError::validation("Internal USDC amount overflow."))?;
                    effects.push(FinancialEffect::CashDebit {
                        resource: adapter.account.trading_cash_resource(),
                        amount: Amount {
                            asset: trading_usdc_asset(),
                            atoms: gofin_domain::Quantity::new(debit.to_string())?,
                        },
                    });
                    effects.push(FinancialEffect::UsableCredit {
                        to: source_resource.clone(),
                        amount: Amount {
                            asset: source_asset.clone(),
                            atoms: route_atoms(
                                &source_route,
                                receipt.destination_credit_usdc_atoms,
                            )?,
                        },
                    });
                    if receipt.retained_source_credit_usdc_atoms > 0 {
                        let core_owner = ResourceId(format!(
                            "hyperliquid:mainnet:{}:spot:primary",
                            binding.intent.owner.to_ascii_lowercase()
                        ));
                        effects.push(FinancialEffect::UsableCredit {
                            to: core_owner,
                            amount: Amount {
                                asset: route_usdc_asset(&FundingRoute::Core),
                                atoms: route_atoms(
                                    &FundingRoute::Core,
                                    receipt.retained_source_credit_usdc_atoms,
                                )?,
                            },
                        });
                    }
                }
            }
            if receipt.actual_source_fee_usdc_atoms > 0 {
                let (fee_resource, fee_asset, fee_amount) = match binding.intent.direction {
                    FundingDirection::Deposit => (
                        source_resource.clone(),
                        source_asset.clone(),
                        route_atoms(&binding.intent.route, receipt.actual_source_fee_usdc_atoms)?,
                    ),
                    FundingDirection::Withdraw => (
                        ResourceId(format!(
                            "hyperliquid:mainnet:{}:spot:primary",
                            binding.intent.owner.to_ascii_lowercase()
                        )),
                        route_usdc_asset(&FundingRoute::Core),
                        route_atoms(&FundingRoute::Core, receipt.actual_source_fee_usdc_atoms)?,
                    ),
                };
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: fee_resource,
                    },
                    amount: Amount {
                        asset: fee_asset,
                        atoms: fee_amount,
                    },
                    treatment: if binding.intent.direction == FundingDirection::Deposit {
                        CostTreatment::AdditionalDebit
                    } else {
                        CostTreatment::IncludedInDebit
                    },
                    category: "papertrade_funding_source_fee".into(),
                });
            }
            let protocol_fee_atoms = receipt
                .actual_cctp_fee_usdc_atoms
                .checked_add(receipt.activation_fee_usdc_atoms)
                .ok_or_else(|| GoFinError::validation("Funding fee total overflow."))?;
            if protocol_fee_atoms > 0 {
                let (fee_resource, fee_asset, fee_amount) = match binding.intent.direction {
                    FundingDirection::Deposit => (
                        source_resource.clone(),
                        source_asset.clone(),
                        route_atoms(&binding.intent.route, protocol_fee_atoms)?,
                    ),
                    FundingDirection::Withdraw => (
                        adapter.account.trading_cash_resource(),
                        trading_usdc_asset(),
                        gofin_domain::Quantity::new(
                            (u128::from(protocol_fee_atoms) * 1_000_000_000_000).to_string(),
                        )?,
                    ),
                };
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: fee_resource,
                    },
                    amount: Amount {
                        asset: fee_asset,
                        atoms: fee_amount,
                    },
                    treatment: CostTreatment::IncludedInDebit,
                    category: "papertrade_funding_protocol_fee".into(),
                });
            }
            if let (Some(native), Some(asset_id)) = (
                receipt.actual_native_fee_atoms.as_deref(),
                receipt.actual_native_fee_asset.as_deref(),
            ) {
                let declared = binding.bounds.native_fee_asset.as_deref().ok_or_else(|| {
                    GoFinError::validation("Unexpected native fee asset in funding proof.")
                })?;
                if asset_id != declared {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Native fee proof identity differs from the approved route."}),
                        verified: None,
                    });
                }
                let actual = native.parse::<u128>().map_err(|_| {
                    GoFinError::validation("Native fee proof quantity is malformed.")
                })?;
                let maximum = binding
                    .bounds
                    .maximum_native_fee_atoms
                    .as_deref()
                    .and_then(|v| v.parse::<u128>().ok())
                    .unwrap_or(0);
                if actual > maximum {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"Native fee exceeds its approved exact asset bound."}),
                        verified: None,
                    });
                }
                if actual > 0 {
                    effects.push(FinancialEffect::Cost {
                        location: AccountingLocation::Cash {
                            resource: source_resource.clone(),
                        },
                        amount: Amount {
                            asset: native_fee_asset(&binding.intent.route, declared)?,
                            atoms: native_fee_quantity(&binding.intent.route, native)?,
                        },
                        treatment: CostTreatment::AdditionalDebit,
                        category: "papertrade_funding_native_fee".into(),
                    });
                }
            } else if receipt.actual_native_fee_atoms.is_some()
                || receipt.actual_native_fee_asset.is_some()
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"Native fee proof is missing one side of its typed asset/amount pair."}),
                    verified: None,
                });
            }
            effects.extend(refundable_native_funding_effects(
                &binding,
                &receipt,
                &source_resource,
            )?);
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence: json!({
                        "proofEvents":[{"eventId":receipt.evidence_id,"source":receipt.source_identity,"destination":receipt.destination_identity}],
                    "amount_usdc_atoms":receipt.amount_usdc_atoms,
                    "route":binding.intent.route,
                    "direction":binding.intent.direction
                }),
                observed_at: Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            Ok(Reconciliation {
                status: Status::Completed,
                evidence: json!({"funding_status":"settled","evidence_id":receipt.evidence_id}),
                verified: Some(verified),
            })
        }
    }
}

fn core_withdrawal_deadline(bound: &BoundIntent) -> Option<u64> {
    match &bound.typed {
        PapertradeTypedIntent::WithdrawToCore { deadline, .. } => Some(*deadline),
        _ => None,
    }
}

fn core_withdrawal_binding_matches(binding: &FundingBinding, bound: &BoundIntent) -> bool {
    bound.signer.eq_ignore_ascii_case(&binding.intent.owner)
        && matches!(
            &bound.typed,
            PapertradeTypedIntent::WithdrawToCore { user, amount, .. }
                if user.eq_ignore_ascii_case(&binding.intent.owner)
                    && amount == &binding.intent.amount_usdc_atoms.to_string()
        )
}

fn verified_core_nonce_invalidation_failure(
    record: &OperationRecord,
    submission: &Submission,
    funding: &FundingBinding,
    terminal: &Value,
) -> Result<Option<VerifiedEffects>, GoFinError> {
    let parse = |field: &str| -> Result<Value, GoFinError> {
        terminal.get(field).cloned().ok_or_else(|| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_PROOF_INVALID",
                "reconciliation",
                "The linked nonce-recovery proof is incomplete.",
            )
        })
    };
    let recovery: CoreDepositNonceRecoveryBinding =
        serde_json::from_value(parse("recovery_binding")?).map_err(|_| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_PROOF_INVALID",
                "reconciliation",
                "The recovery binding is malformed.",
            )
        })?;
    let proof: CoreDepositNonceRecoveryProof =
        serde_json::from_value(parse("proof")?).map_err(|_| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_PROOF_INVALID",
                "reconciliation",
                "The recovery proof is malformed.",
            )
        })?;
    let parent_submission: Submission = serde_json::from_value(parse("parent_submission")?)
        .map_err(|_| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_PROOF_INVALID",
                "reconciliation",
                "The parent submission proof is malformed.",
            )
        })?;
    let child_submission: Submission =
        serde_json::from_value(parse("child_submission")?).map_err(|_| {
            err(
                "PAPERTRADE_NONCE_RECOVERY_PROOF_INVALID",
                "reconciliation",
                "The child submission proof is malformed.",
            )
        })?;
    let host_proof = parse("host_proof")?;
    let parent_record_id = terminal["parent_record_id"].as_str().unwrap_or_default();
    let child_record_id = terminal["child_record_id"].as_str().unwrap_or_default();
    if terminal["version"].as_u64() != Some(1)
        || terminal["child_completed"] != true
        || terminal["plan_id"].as_str() != Some(funding.operation_id.as_str())
        || parent_record_id != record.id
        || child_record_id != proof.child_record_id
        || parent_submission != *submission
        || recovery.parent_operation_id != record.id
        || recovery.parent_submission != *submission
        || recovery.route_id != funding.operation_id
        || recovery.owner != funding.intent.owner.to_ascii_lowercase()
        || proof.parent_operation_id != record.id
        || proof.parent_submission != *submission
        || proof.child_submission != child_submission
        || proof.child_record_id != child_record_id
        || proof.validate_against(&recovery).is_err()
        || child_submission.kind.trim().is_empty()
        || host_proof["kind"] != "hyperliquid_core_deposit_nonce_invalidated"
        || host_proof["parent_operation_id"].as_str() != Some(record.id.as_str())
        || host_proof["funding_plan_id"].as_str() != Some(funding.operation_id.as_str())
        || host_proof["owner"]
            .as_str()
            .is_none_or(|owner| !owner.eq_ignore_ascii_case(&funding.intent.owner))
        || host_proof["child_operation_id"].as_str() != Some(proof.child_operation_id.as_str())
        || host_proof["child_submission_id"].as_str() != Some(child_submission.reference.as_str())
        || host_proof["action_fingerprint"]
            .as_str()
            .is_none_or(|value| value.len() != 66 || !value.starts_with("0x"))
    {
        return Ok(None);
    }
    let evidence = json!({
        "state":"core_deposit_nonce_consumed",
        "reason":"A linked completed recovery child received the exact successful Hyperliquid Noop acknowledgement; the original Core action is terminally rejected without financial effects.",
        "recovery":terminal,
        "financial_effects":[]
    });
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: submission.clone(),
        effects: vec![],
        evidence,
        observed_at: Utc::now().to_rfc3339(),
        proof_digest: String::new(),
    };
    verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
    Ok(Some(verified))
}

fn exact_local_funding_key_failure(record: &OperationRecord) -> bool {
    record.error.as_ref().is_some_and(|error| {
        error.code == "INVALID_INPUT"
            && error.stage == "validation"
            && error.message == "Invalid development Papertrade runtime key."
    })
}

fn proven_local_funding_key_failure(
    record: &OperationRecord,
    progress_evidence: &Value,
    operation_id: &str,
    expected_leg_count: Option<usize>,
) -> bool {
    exact_local_funding_key_failure(record)
        && record.dispatch_evidence.is_none()
        && expected_leg_count.is_some_and(|expected| {
            all_funding_legs_proven_not_submitted(progress_evidence, operation_id, expected)
        })
}

fn expected_unsubmitted_funding_leg_count(binding: &FundingBinding) -> Option<usize> {
    // The source-stage deposit routes are each exactly one immutable source
    // action. Other route/direction combinations have different staged leg
    // counts and are not eligible for this narrowly scoped local failure.
    (binding.intent.direction == FundingDirection::Deposit).then_some(1)
}

fn all_funding_legs_proven_not_submitted(
    evidence: &Value,
    operation_id: &str,
    expected_leg_count: usize,
) -> bool {
    let Some(legs) = evidence
        .get("legs")
        .and_then(Value::as_array)
        .filter(|legs| legs.len() == expected_leg_count && expected_leg_count > 0)
    else {
        return false;
    };
    evidence.get("operation_id").and_then(Value::as_str) == Some(operation_id)
        && legs.iter().enumerate().all(|(ordinal, leg)| {
            leg.get("ordinal").and_then(Value::as_u64) == Some(ordinal as u64)
                && leg.get("status").and_then(Value::as_str) == Some("not_submitted")
                && leg
                    .get("submission_id")
                    .and_then(Value::as_str)
                    .is_none_or(|submission_id| {
                        submission_id == format!("papertrade-funding:{operation_id}:{ordinal}")
                    })
        })
}

impl PapertradeTypedIntent {
    fn user_address(&self) -> &str {
        match self {
            Self::RegisterSessionKey { user, .. }
            | Self::OpenPosition { user, .. }
            | Self::Close { user, .. }
            | Self::WithdrawToCore { user, .. }
            | Self::Stake { user, .. }
            | Self::Unstake { user, .. }
            | Self::Claim { user, .. }
            | Self::CancelIntent { user, .. } => user,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_continuation_debits_stage_two_once_and_leaves_core_reserve_uncredited() {
        let owner = "0x1111111111111111111111111111111111111111";
        let route_id = "plan-execution:withdraw:parent";
        let binding = FundingContinuationBinding {
            version: 1,
            operation_id: "execution:continue-withdraw".into(),
            route_id: route_id.into(),
            parent_submission: Submission {
                kind: "papertrade_funding".into(),
                reference: route_id.into(),
                data: json!({"operation_id":route_id}),
            },
            plan_fingerprint: format!("0x{}", "a".repeat(64)),
            stage1_proof_fingerprint: format!("0x{}", "b".repeat(64)),
            intent: FundingIntent {
                direction: FundingDirection::Withdraw,
                route: FundingRoute::Ethereum,
                owner: owner.into(),
                amount_usdc_atoms: 319_920_698,
                request_id: route_id.into(),
            },
            source_identity: format!("hyperliquid:mainnet:{owner}:spot:primary"),
            destination_identity: format!("ethereum:1:{owner}"),
            // The stage-two principal excludes the 0.1 USDC reserve already
            // left in the Core balance by the stage-one withdrawal.
            bounds: FundingFinancialBounds {
                source_usdc_atoms: 319_820_698,
                maximum_source_fee_usdc_atoms: 100_000,
                destination_min_usdc_atoms: 319_720_698,
                maximum_cctp_fee_usdc_atoms: 100_000,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
        };
        binding.validate().unwrap();
        let receipt = FundingReceipt {
            stage: FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: 319_720_698,
            source_debit_usdc_atoms: 319_820_698,
            destination_credit_usdc_atoms: 319_720_698,
            actual_source_fee_usdc_atoms: 3_645,
            actual_cctp_fee_usdc_atoms: 100_000,
            retained_source_credit_usdc_atoms: 0,
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            activation_fee_usdc_atoms: 0,
            actual_native_fee_atoms: None,
            actual_native_fee_asset: None,
            source_identity: binding.source_identity.clone(),
            destination_identity: binding.destination_identity.clone(),
            evidence_id: "fixture:canonical-stage-two-receipt".into(),
        };
        let effects = funding_continuation_effects(&binding, &receipt)
            .unwrap()
            .unwrap();
        let core_asset = route_usdc_asset(&FundingRoute::Core);
        let core_principal = route_atoms(&FundingRoute::Core, 319_820_698).unwrap();
        let core_source_fee = route_atoms(&FundingRoute::Core, 3_645).unwrap();
        let core_protocol_fee = route_atoms(&FundingRoute::Core, 100_000).unwrap();
        assert!(effects.iter().any(|effect| matches!(effect,
            FinancialEffect::CashDebit { resource, amount }
                if resource.0 == binding.source_identity && amount.asset == core_asset && amount.atoms == core_principal
        )));
        assert!(effects.iter().any(|effect| matches!(effect,
            FinancialEffect::Cost { location: AccountingLocation::Cash { resource }, amount, treatment: CostTreatment::AdditionalDebit, .. }
                if resource.0 == binding.source_identity && amount.atoms == core_source_fee
        )));
        assert!(effects.iter().any(|effect| matches!(effect,
            FinancialEffect::Cost { location: AccountingLocation::Cash { resource }, amount, treatment: CostTreatment::IncludedInDebit, .. }
                if resource.0 == binding.source_identity && amount.atoms == core_protocol_fee
        )));
        assert!(effects.iter().any(|effect| matches!(effect,
            FinancialEffect::UsableCredit { to, amount }
                if to.0 == binding.destination_identity && amount.atoms == gofin_domain::Quantity::new("319720698").unwrap()
        )));
        assert!(!effects.iter().any(|effect| matches!(effect,
            FinancialEffect::UsableCredit { to, .. } if to.0 == binding.source_identity
        )));
        // Stage one credited 319.920698 USDC. Stage two consumes its exact
        // principal plus the separately charged source fee, leaving 0.096355
        // USDC in Core. No synthetic reserve credit is added by stage two.
        assert_eq!(319_920_698u64 - 319_820_698 - 3_645, 96_355);
    }

    fn failure_record(message: &str) -> OperationRecord {
        OperationRecord {
            id: "test-operation".into(),
            request_id: "test-request".into(),
            fingerprint: "test-fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_DEPOSIT.into(),
            input: json!({}),
            status: Status::Unknown,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            preview: None,
            approval: None,
            authorization_source: None,
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: Some(GoFinError::validation(message)),
            run_id: None,
        }
    }

    #[test]
    fn session_revocation_reconciliation_books_only_canonical_actual_native_gas() {
        let mut record = failure_record("fixture");
        record.submission = Some(Submission {
            kind: "papertrade_session_revoke".into(),
            reference: "revoke-fixture".into(),
            data: json!({"owner":"0x1111111111111111111111111111111111111111"}),
        });
        let verified = session_revocation_verified_effects(
            &record,
            "0x1111111111111111111111111111111111111111",
            "42000",
            json!({"canonical_session_revocation_failure":{"actual_gas_fee_atoms":"42000"}}),
        )
        .unwrap();
        assert_eq!(verified.effects.len(), 1);
        match &verified.effects[0] {
            FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount,
                category,
                treatment,
            } => {
                assert_eq!(
                    resource.0,
                    "ethereum:999:0x1111111111111111111111111111111111111111"
                );
                assert_eq!(amount.asset.network, "ethereum:999");
                assert_eq!(amount.asset.reference, "native");
                assert_eq!(amount.asset.decimals, 18);
                assert_eq!(amount.atoms.as_str(), "42000");
                assert_eq!(category, "ethereum_gas");
                assert_eq!(*treatment, CostTreatment::AdditionalDebit);
            }
            other => panic!("unexpected revocation effect: {other:?}"),
        }
        assert_eq!(
            verified.proof_digest,
            gofin_domain::verified_effect_digest(&verified).unwrap()
        );
    }

    #[test]
    fn core_nonce_recovery_binds_route_reference_separately_from_parent_record_id() {
        let parent_id = "e479b28a-4475-4e5d-bc25-6c0f824c0011";
        let route_id = "plan-execution:deposit:9ae41e7b";
        let parent_submission = Submission {
            kind: "papertrade_funding".into(),
            reference: route_id.into(),
            data: json!({"operation_id":route_id}),
        };
        let recovery = CoreDepositNonceRecoveryBinding {
            version: 1,
            child_operation_id: "execution:recover-deposit".into(),
            parent_operation_id: parent_id.into(),
            parent_submission: parent_submission.clone(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            route_id: route_id.into(),
            plan_fingerprint: format!("0x{}", "a".repeat(64)),
        };
        recovery.validate().unwrap();
        assert_ne!(recovery.parent_operation_id, recovery.route_id);
        let prepared = PreparedOperation {
            operation: OP_INVALIDATE_CORE_NONCE.into(),
            input: json!({"parent_operation_id":parent_id}),
            binding: json!({"core_deposit_nonce_recovery":recovery}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let decoded = core_nonce_recovery_binding(&prepared).unwrap();
        assert_eq!(decoded.parent_operation_id, parent_id);
        assert_eq!(decoded.parent_submission.reference, route_id);
        assert_eq!(decoded.parent_submission.data["operation_id"], route_id);
        let child_submission = core_nonce_recovery_submission(&decoded);
        assert!(child_submission.data.get("nonce").is_none());
    }

    #[test]
    fn local_runtime_key_failure_is_terminal_only_with_exact_all_leg_no_dispatch_proof() {
        let record = failure_record("Invalid development Papertrade runtime key.");
        assert!(exact_local_funding_key_failure(&record));
        let legs = json!({
            "operation_id":"route-1",
            "legs":[
                {"ordinal":0,"status":"not_submitted","submission_id":"papertrade-funding:route-1:0"}
            ]
        });
        assert!(all_funding_legs_proven_not_submitted(&legs, "route-1", 1));
        assert!(!all_funding_legs_proven_not_submitted(&legs, "route-1", 2));
        assert!(!all_funding_legs_proven_not_submitted(&legs, "route-2", 1));

        let mut ambiguous = legs.clone();
        ambiguous["legs"][0]["status"] = json!("unknown");
        assert!(proven_local_funding_key_failure(
            &record,
            &legs,
            "route-1",
            Some(1)
        ));
        let mut attempted = record.clone();
        attempted.dispatch_evidence = Some(json!({"attempted":true}));
        assert!(!proven_local_funding_key_failure(
            &attempted,
            &legs,
            "route-1",
            Some(1)
        ));
        assert!(!proven_local_funding_key_failure(
            &record,
            &ambiguous,
            "route-1",
            Some(1)
        ));
        assert!(!proven_local_funding_key_failure(
            &record, &legs, "route-1", None
        ));
        let mut wrong_identity = legs.clone();
        wrong_identity["legs"][0]["submission_id"] = json!("papertrade-funding:other:0");
        assert!(!all_funding_legs_proven_not_submitted(
            &wrong_identity,
            "route-1",
            1
        ));
        let mut other_error = failure_record("Other input was invalid.");
        assert!(!exact_local_funding_key_failure(&other_error));
        other_error.error = Some(GoFinError::new(
            "INVALID_INPUT",
            "submission",
            "Invalid development Papertrade runtime key.",
            "different stage is not accepted",
        ));
        assert!(!exact_local_funding_key_failure(&other_error));
    }

    #[test]
    fn funding_summary_formats_six_decimal_wire_atoms_as_usdc_units() {
        assert_eq!(format_usdc_atoms(19_000_000), "19");
        assert_eq!(format_usdc_atoms(19_250_000), "19.25");
        assert_eq!(format_usdc_atoms(1), "0.000001");
    }

    #[test]
    fn staking_claimable_uses_exact_1e27_floor_formula() {
        let snapshot = json!({
            "paper": {
                "staked": "2000000000000000000",
                "pendingBase": "3000000000000000000",
                "rewardDebt": "1000000000000000000"
            },
            "accRewardPerShareRaw": "1000000000000000000000000000",
            "rewardReserveRaw": "4000000000000000000"
        });
        assert_eq!(
            claimable_rewards_wad(&snapshot).unwrap(),
            4_000_000_000_000_000_000
        );
        let fractional = json!({
            "paper": {"staked":"3", "pendingBase":"0", "rewardDebt":"0"},
            "accRewardPerShareRaw":"333333333333333333333333333",
            "rewardReserveRaw":"1000000000000000000000000000"
        });
        assert_eq!(claimable_rewards_wad(&fractional).unwrap(), 0);
        let capped = json!({
            "paper": {"staked":"2000000000000000000", "pendingBase":"3000000000000000000", "rewardDebt":"1000000000000000000"},
            "accRewardPerShareRaw":"1000000000000000000000000000",
            "rewardReserveRaw":"700000000000000000"
        });
        assert_eq!(
            claimable_rewards_wad(&capped).unwrap(),
            700_000_000_000_000_000
        );
    }

    #[test]
    fn core_usdc_atom_conversion_uses_36dp_without_u128_narrowing() {
        let thousand_usdc = route_atoms(&FundingRoute::Core, 1_000_000_000).unwrap();
        assert_eq!(
            thousand_usdc.as_str(),
            "1000000000000000000000000000000000000000"
        );
        assert_eq!(
            route_usdc_asset(&FundingRoute::Core).decimals,
            36,
            "Core's funding wire precision is 6 decimals but ledger identity uses the profile's 36-decimal asset"
        );
    }

    #[test]
    fn core_hype_fee_atoms_scale_to_canonical_36dp_asset() {
        let asset = native_fee_asset(&FundingRoute::Core, "hyperliquid:mainnet:HYPE").unwrap();
        assert_eq!(asset.reference, "HYPE");
        assert_eq!(asset.decimals, 36);
        assert_eq!(
            native_fee_quantity(&FundingRoute::Core, "12345678")
                .unwrap()
                .as_str(),
            "12345678".to_owned() + &"0".repeat(28)
        );
        assert!(native_fee_asset(&FundingRoute::Core, "hyperliquid:mainnet:native").is_err());
    }

    #[test]
    fn legacy_position_gets_exact_native_id_baseline_anchor() {
        let row = PositionSnapshot {
            position_id: "42".into(),
            instrument_id: 0,
            is_long: true,
            use_debt: false,
            entry_price_raw: "50000000".into(),
            margin_raw: "10000000000000000000".into(),
            leverage: 100,
            bust_price_raw: "45000000".into(),
            opened_at_ms: 1_800_000_000_000,
            intent_id: None,
        };
        assert_eq!(position_snapshot_key(&row), "papertrade:position_id:42");
        let anchors = position_snapshot_anchors(
            "0x1111111111111111111111111111111111111111",
            &[row],
            123,
            "2026-10-16T00:00:00Z",
        )
        .unwrap();
        assert_eq!(anchors.len(), 2);
        assert!(
            anchors
                .iter()
                .any(|anchor| anchor.position_kind == PositionKind::MarginLong
                    && anchor.principal.atoms.as_str() == "10000000000000000000")
        );
        assert!(anchors.iter().any(
            |anchor| anchor.position_kind == PositionKind::DerivativeLong
                && anchor.principal.atoms.as_str() == "1000000000000000000000"
        ));
        assert!(anchors.iter().all(|anchor| {
            anchor.position == "papertrade:position_id:42"
                && anchor.evidence["indexed_through_block"] == 123
        }));
    }

    #[test]
    fn core_funding_receipt_identity_matches_exact_spot_resource() {
        let owner = "0x1111111111111111111111111111111111111111";
        let intent = FundingIntent {
            direction: FundingDirection::Deposit,
            route: FundingRoute::Core,
            owner: owner.into(),
            amount_usdc_atoms: 10_000_000,
            request_id: "req-1".into(),
        };
        // This is the exact normalized shape returned by the funding host:
        // Core source spot identity and the Papertrade internal cash account.
        let receipt = FundingReceipt {
            stage: FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: 9_000_000,
            source_debit_usdc_atoms: 10_000_000,
            destination_credit_usdc_atoms: 9_000_000,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: 0,
            retained_source_credit_usdc_atoms: 0,
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            activation_fee_usdc_atoms: 1_000_000,
            actual_native_fee_atoms: None,
            actual_native_fee_asset: None,
            source_identity: format!("hyperliquid:mainnet:{owner}:spot:primary"),
            destination_identity: format!("papertrade:mainnet:{owner}"),
            evidence_id: "0xabc:0xdef".into(),
        };
        assert!(funding_identities_match(&intent, &receipt));
    }

    #[test]
    fn non_core_withdrawal_stage_one_proves_only_exact_core_credit() {
        let owner = "0x1111111111111111111111111111111111111111";
        let amount = 19_000_000;
        let intent = FundingIntent {
            direction: FundingDirection::Withdraw,
            route: FundingRoute::Hyperevm,
            owner: owner.into(),
            amount_usdc_atoms: amount,
            request_id: "withdraw-request".into(),
        };
        let receipt = FundingReceipt {
            stage: FundingReceiptStage::CoreCredit,
            status: "settled".into(),
            amount_usdc_atoms: amount,
            source_debit_usdc_atoms: amount,
            destination_credit_usdc_atoms: amount,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: 0,
            retained_source_credit_usdc_atoms: 0,
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            activation_fee_usdc_atoms: 0,
            actual_native_fee_atoms: None,
            actual_native_fee_asset: None,
            source_identity: format!("papertrade:mainnet:{owner}"),
            destination_identity: format!("hyperliquid:mainnet:{owner}:spot:primary"),
            evidence_id: "canonical-papertrade-and-core-credit".into(),
        };
        assert!(funding_receipt_stage_matches(&intent, &receipt));
        assert!(funding_identities_match(&intent, &receipt));

        let mut incomplete = receipt.clone();
        incomplete.stage = FundingReceiptStage::FinalDestination;
        incomplete.destination_identity = format!("ethereum:999:{owner}");
        assert!(!funding_receipt_stage_matches(&intent, &incomplete));

        let mut fee = receipt.clone();
        fee.actual_source_fee_usdc_atoms = 1;
        assert!(!funding_receipt_stage_matches(&intent, &fee));

        let mut short_credit = receipt;
        short_credit.destination_credit_usdc_atoms -= 1;
        assert!(!funding_receipt_stage_matches(&intent, &short_credit));
    }

    #[test]
    fn solana_message_rent_is_bounded_separately_from_network_fee() {
        let binding = FundingBinding {
            version: 1,
            operation_id: "funding-op-1".into(),
            intent: FundingIntent {
                direction: FundingDirection::Deposit,
                route: FundingRoute::Solana,
                owner: "0x1111111111111111111111111111111111111111".into(),
                amount_usdc_atoms: 20_000_000,
                request_id: "funding-op-1".into(),
            },
            source_identity: format!("solana:mainnet-beta:{}", "1".repeat(32)),
            destination_identity: "papertrade:mainnet:0x1111111111111111111111111111111111111111"
                .into(),
            bounds: FundingFinancialBounds {
                source_usdc_atoms: 20_000_000,
                maximum_source_fee_usdc_atoms: 0,
                destination_min_usdc_atoms: 19_000_000,
                maximum_cctp_fee_usdc_atoms: 1_000_000,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: Some("solana:mainnet-beta:native".into()),
                maximum_native_fee_atoms: Some("500000".into()),
                maximum_refundable_native_funding_atoms: Some("4200000".into()),
                maximum_native_source_debit_atoms: Some("5000000".into()),
            },
            transaction: None,
            solana_transaction: None,
            solana_message_event_account: Some("11111111111111111111111111111111".into()),
            papertrade_intent: None,
        };
        let receipt = FundingReceipt {
            stage: FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: 19_000_000,
            source_debit_usdc_atoms: 20_000_000,
            destination_credit_usdc_atoms: 19_000_000,
            actual_source_fee_usdc_atoms: 0,
            actual_cctp_fee_usdc_atoms: 1_000_000,
            retained_source_credit_usdc_atoms: 0,
            activation_fee_usdc_atoms: 0,
            actual_native_fee_atoms: Some("400000".into()),
            actual_native_fee_asset: Some("solana:mainnet-beta:native".into()),
            retained_source_native_atoms: Some("4200000".into()),
            retained_source_native_asset: Some("solana:mainnet-beta:native".into()),
            source_identity: binding.source_identity.clone(),
            destination_identity: binding.destination_identity.clone(),
            evidence_id: "source:destination:sweep".into(),
        };
        assert!(funding_retained_native_matches(&binding, &receipt));
        let source_resource = ResourceId(binding.source_identity.clone());
        let rent_effects =
            refundable_native_funding_effects(&binding, &receipt, &source_resource).unwrap();
        assert_eq!(rent_effects.len(), 3);
        assert!(matches!(
            &rent_effects[0],
            FinancialEffect::CashDebit { resource, amount }
                if resource == &source_resource
                    && amount.asset.reference == "native"
                    && amount.atoms.as_str() == "4200000"
        ));
        assert!(matches!(
            &rent_effects[1],
            FinancialEffect::PositionChange {
                resource,
                position,
                asset,
                delta,
                anchor: Some(anchor),
                ..
            }
                if resource == &source_resource
                    && position == "papertrade-cctp-message-sent:papertrade-funding:funding-op-1:0"
                    && asset.reference == "native"
                    && delta.magnitude().as_str() == "4200000"
                    && anchor.position_kind == PositionKind::RefundableAccountFunding
                    && anchor.evidence["message_sent_event_account"] == "11111111111111111111111111111111"
        ));
        assert!(matches!(
            &rent_effects[2],
            FinancialEffect::OriginMovement { debit, credit, .. }
                if debit.atoms.as_str() == "4200000"
                    && credit.atoms.as_str() == "4200000"
        ));

        let mut fee_over = receipt.clone();
        fee_over.actual_native_fee_atoms = Some("500001".into());
        assert!(!funding_retained_native_matches(&binding, &fee_over));
        let mut rent_over = receipt.clone();
        rent_over.retained_source_native_atoms = Some("4200001".into());
        assert!(!funding_retained_native_matches(&binding, &rent_over));
        let mut combined_over = binding.clone();
        combined_over.bounds.maximum_native_source_debit_atoms = Some("4599999".into());
        assert!(!funding_retained_native_matches(&combined_over, &receipt));
        let mut wrong_asset = receipt.clone();
        wrong_asset.retained_source_native_asset = Some("ethereum:1:native".into());
        assert!(!funding_retained_native_matches(&binding, &wrong_asset));
        let mut missing_rent = receipt;
        missing_rent.retained_source_native_atoms = None;
        assert!(!funding_retained_native_matches(&binding, &missing_rent));
    }

    #[test]
    fn solana_funding_identity_uses_enrolled_base58_account_not_evm_owner() {
        let intent = FundingIntent {
            direction: FundingDirection::Deposit,
            route: FundingRoute::Solana,
            owner: "0x1111111111111111111111111111111111111111".into(),
            amount_usdc_atoms: 10_000_000,
            request_id: "req-solana".into(),
        };
        let enrolled = format!("solana:mainnet-beta:{}", "1".repeat(32));
        assert!(valid_funding_identities(
            &intent,
            &enrolled,
            "papertrade:mainnet:0x1111111111111111111111111111111111111111"
        ));
        assert!(!valid_funding_identities(
            &intent,
            "solana:mainnet-beta:0x1111111111111111111111111111111111111111",
            "papertrade:mainnet:0x1111111111111111111111111111111111111111"
        ));
    }

    #[test]
    fn open_position_effects_debit_margin_and_record_leveraged_short_exactly() {
        let account = PapertradeAccount {
            name: "paper-main".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: true,
        };
        let intent = PapertradeTypedIntent::OpenPosition {
            user: account.owner.clone(),
            instrument_id: 0,
            is_long: false,
            size: 2_000_000_000_000_000_000,
            leverage: 5,
            use_debt: false,
            nonce: "7".into(),
            deadline: 1_800_000_000,
        };
        let proof = VerifiedPapertradeCall {
            transaction_hash: format!("0x{}", "a".repeat(64)),
            block_hash: format!("0x{}", "b".repeat(64)),
            block_number: 123,
            batch_id: format!("0x{}", "c".repeat(64)),
            batch_item_index: 0,
            intent_id: format!("0x{}", "d".repeat(64)),
            action: "open_position".into(),
            owner: account.owner.clone(),
            target: EXCHANGE.into(),
            selector: "ec6783f0".into(),
            call_return_data: None,
            event_ids: vec!["19".into()],
            events: vec![ProtocolSettlementEvent::PositionOpened {
                position_id: "42".into(),
                owner: account.owner.clone(),
                instrument_id: 0,
                is_long: false,
                use_debt: false,
                entry_price: "1000000000000".into(),
                margin: "2000000000000000000".into(),
                leverage: 5,
            }],
        };
        let verified = open_position_effects(
            &intent,
            &proof,
            Submission {
                kind: "papertrade_intent".into(),
                reference: proof.intent_id.clone(),
                data: json!({"intent_id":proof.intent_id}),
            },
            &account,
        )
        .unwrap()
        .unwrap();
        assert_eq!(verified.effects.len(), 3);
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::CashDebit { amount, .. }
                if amount.atoms == gofin_domain::Quantity::new("2000000000000000000").unwrap()
        )));
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::PositionChange { position, delta, anchor: Some(anchor), .. }
                if position == &position_record(&proof.intent_id)
                    && delta == &SignedQuantity::positive(&gofin_domain::Quantity::new("10000000000000000000").unwrap())
                    && anchor.position_kind == PositionKind::DerivativeShort
        )));
        let mut second_open = proof.clone();
        second_open.intent_id = format!("0x{}", "e".repeat(64));
        let second = open_position_effects(
            &intent,
            &second_open,
            Submission {
                kind: "papertrade_intent".into(),
                reference: second_open.intent_id.clone(),
                data: json!({"intent_id":second_open.intent_id}),
            },
            &account,
        )
        .unwrap()
        .unwrap();
        let position_keys = [&verified, &second]
            .into_iter()
            .flat_map(|effects| effects.effects.iter())
            .filter_map(|effect| match effect {
                FinancialEffect::PositionChange {
                    position,
                    anchor: Some(anchor),
                    ..
                } if anchor.position_kind == PositionKind::DerivativeShort => Some(position),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(position_keys.len(), 2);
        assert_ne!(position_keys[0], position_keys[1]);
    }

    #[test]
    fn close_effects_release_exact_cash_margin_and_exposure() {
        let account = PapertradeAccount {
            name: "paper-main".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: true,
        };
        let row = PositionSnapshot {
            position_id: "42".into(),
            instrument_id: 0,
            is_long: true,
            use_debt: false,
            entry_price_raw: "1000000000000".into(),
            margin_raw: "2000000000000000000".into(),
            leverage: 5,
            bust_price_raw: "900000000000".into(),
            opened_at_ms: 1_800_000_000_000,
            intent_id: Some(format!("0x{}", "a".repeat(64))),
        };
        let preview = PreparedOperation {
            operation: OP_CLOSE.into(),
            input: json!({"position_ids":["42"]}),
            binding: json!({"close_positions":[row]}),
            summary: "close".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:05:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let proof = VerifiedPapertradeCall {
            transaction_hash: format!("0x{}", "b".repeat(64)),
            block_hash: format!("0x{}", "c".repeat(64)),
            block_number: 123,
            batch_id: format!("0x{}", "d".repeat(64)),
            batch_item_index: 0,
            intent_id: format!("0x{}", "e".repeat(64)),
            action: "closePositions".into(),
            owner: account.owner.clone(),
            target: EXCHANGE.into(),
            selector: "2792e706".into(),
            call_return_data: None,
            event_ids: vec!["19".into(), "20".into()],
            events: vec![
                ProtocolSettlementEvent::PositionClosed {
                    position_id: "42".into(),
                    owner: account.owner.clone(),
                    raw_data: format!("0x{}", "00".repeat(160)),
                },
                ProtocolSettlementEvent::PositionsClosedCashSettlement {
                    owner: account.owner.clone(),
                    amount_wad: "1500000000000000000".into(),
                    resulting_balance_wad: "5500000000000000000".into(),
                },
            ],
        };
        let intent = PapertradeTypedIntent::Close {
            user: account.owner.clone(),
            position_ids: vec!["42".into()],
            nonce: "8".into(),
            deadline: 1_800_000_300,
        };
        let verified = close_position_effects(
            &intent,
            &preview,
            &proof,
            Submission {
                kind: "papertrade_intent".into(),
                reference: proof.intent_id.clone(),
                data: json!({"intent_id":proof.intent_id}),
            },
            &account,
        )
        .unwrap()
        .unwrap();
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::PositionChange { position, delta, anchor: Some(anchor), .. }
                if position == &position_record(row.intent_id.as_deref().unwrap())
                    && delta == &SignedQuantity::negative(&gofin_domain::Quantity::new("2000000000000000000").unwrap())
                    && anchor.position_kind == PositionKind::MarginLong
        )));
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::PositionChange { position, delta, anchor: Some(anchor), .. }
                if position == &position_record(row.intent_id.as_deref().unwrap())
                    && delta == &SignedQuantity::negative(&gofin_domain::Quantity::new("10000000000000000000").unwrap())
                    && anchor.position_kind == PositionKind::DerivativeLong
        )));
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::Settlement { delta, .. }
                if delta == &SignedQuantity::positive(&gofin_domain::Quantity::new("1500000000000000000").unwrap())
        )));
        assert!(verified.effects.iter().any(|effect| matches!(
            effect,
            FinancialEffect::PositionChange { resource, position, delta, anchor: Some(anchor), .. }
                if resource == &payout_queue_resource(&account.owner)
                    && position == &format!("usdc_win_claim:{}", proof.intent_id)
                    && anchor.position_kind == PositionKind::Investment
                    && delta == &SignedQuantity::positive(&gofin_domain::Quantity::new("500000000000000000").unwrap())
        )));
        assert!(verified.evidence["queued_nonspendable_claim_wad"] == "500000000000000000");
    }

    #[test]
    fn mixed_close_entitlement_subtracts_fee_only_from_positive_adjusted_pnl() {
        fn word_u128(value: u128) -> [u8; 32] {
            let mut out = [0u8; 32];
            out[16..].copy_from_slice(&value.to_be_bytes());
            out
        }
        fn word_i128(value: i128) -> [u8; 32] {
            let bytes = value.to_be_bytes();
            let mut out = if value < 0 { [0xff; 32] } else { [0; 32] };
            out[16..].copy_from_slice(&bytes);
            out
        }
        fn close_event(position_id: &str, adjusted: i128, fee: u128) -> ProtocolSettlementEvent {
            let mut data = Vec::new();
            data.extend_from_slice(&word_u128(0)); // exit price
            data.extend_from_slice(&word_i128(0)); // raw PnL
            data.extend_from_slice(&word_i128(adjusted)); // adjusted PnL
            data.extend_from_slice(&word_u128(0)); // verified bool field
            data.extend_from_slice(&word_u128(fee)); // userPaidFee
            ProtocolSettlementEvent::PositionClosed {
                position_id: position_id.into(),
                owner: "0x1111111111111111111111111111111111111111".into(),
                raw_data: format!("0x{}", hex::encode(data)),
            }
        }
        let rows = vec![
            PositionSnapshot {
                position_id: "1".into(),
                instrument_id: 0,
                is_long: true,
                use_debt: false,
                entry_price_raw: "1".into(),
                margin_raw: "1000".into(),
                leverage: 1,
                bust_price_raw: "1".into(),
                opened_at_ms: 1,
                intent_id: Some(format!("0x{}", "11".repeat(32))),
            },
            PositionSnapshot {
                position_id: "2".into(),
                instrument_id: 1,
                is_long: false,
                use_debt: false,
                entry_price_raw: "1".into(),
                margin_raw: "1000".into(),
                leverage: 1,
                bust_price_raw: "1".into(),
                opened_at_ms: 1,
                intent_id: Some(format!("0x{}", "22".repeat(32))),
            },
        ];
        let events = vec![close_event("1", 600, 100), close_event("2", -700, 999)];
        // Winner: 1000 + 600 - 100 = 1500. Loser: 1000 - 700 = 300;
        // its reported LP-side fee is not an additional owner debit.
        assert_eq!(close_cash_entitlement(&events, &rows).unwrap(), 1800);
    }

    #[test]
    fn selected_close_positions_require_exact_owner_and_cash_only_rows() {
        let owner = "0x1111111111111111111111111111111111111111";
        let snapshot = json!({
            "owner": owner,
            "indexedThroughBlock": "123",
            "positions": [{
                "positionId":"42", "instrumentId":0, "isLong":true, "useDebt":false,
                "entryPriceRaw":"100", "marginRaw":"200", "leverage":5,
                "bustPriceRaw":"90", "openedAtMs":1000, "intentId":format!("0x{}", "a".repeat(64))
            }]
        });
        let rows = select_close_positions(snapshot.clone(), owner, &["42".into()]).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(select_close_positions(snapshot.clone(), owner, &["999".into()]).is_err());
        let mut debt_snapshot = snapshot;
        debt_snapshot["positions"][0]["useDebt"] = json!(true);
        assert!(select_close_positions(debt_snapshot, owner, &["42".into()]).is_err());
    }

    #[test]
    fn zero_reward_claim_settles_from_exact_call_return_without_event() {
        let account = PapertradeAccount {
            name: "paper-main".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: true,
        };
        let intent = PapertradeTypedIntent::Claim {
            user: account.owner.clone(),
            nonce: "9".into(),
            deadline: 1_800_000_000,
        };
        let preview = PreparedOperation {
            operation: OP_CLAIM.into(),
            input: json!({}),
            binding: json!({"claimable_rewards_minimum_raw":"0","claimable_rewards_estimate_raw":"0"}),
            summary: "claim".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-01-01T00:05:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let proof = VerifiedPapertradeCall {
            transaction_hash: format!("0x{}", "a".repeat(64)),
            block_hash: format!("0x{}", "b".repeat(64)),
            block_number: 123,
            batch_id: format!("0x{}", "c".repeat(64)),
            batch_item_index: 0,
            intent_id: format!("0x{}", "d".repeat(64)),
            action: "paperClaim".into(),
            owner: account.owner.clone(),
            target: PAPER_STAKING.into(),
            selector: "9c96d4a1".into(),
            call_return_data: Some(format!("0x{}", "00".repeat(32))),
            event_ids: Vec::new(),
            events: Vec::new(),
        };
        let verified = staking_effects(
            &intent,
            &preview,
            &proof,
            Submission {
                kind: "papertrade_intent".into(),
                reference: proof.intent_id.clone(),
                data: json!({"intent_id":proof.intent_id}),
            },
            &account,
        )
        .unwrap()
        .unwrap();
        assert!(
            verified.effects.is_empty(),
            "a zero reward payout is not a cash movement"
        );
        let evidence = verified.evidence;
        assert!(evidence["proofEvents"].as_array().is_none_or(Vec::is_empty));
    }
    #[test]
    fn expired_intent_requires_exact_pinned_empty_nonce_proof() {
        let owner = "0x1111111111111111111111111111111111111111";
        let bound = BoundIntent {
            typed: PapertradeTypedIntent::Stake {
                user: owner.into(),
                amount: "1000000000000000000".into(),
                nonce: "7".into(),
                deadline: 100,
            },
            intent_id: format!("0x{}", "a".repeat(64)),
            signer: owner.into(),
            action: Some("paperStake".into()),
            registration_id: None,
            cancel_target_action: None,
        };
        let proof = crate::PapertradeIntentExpiryProof {
            chain_id: CHAIN_ID,
            owner: owner.into(),
            intent_id: bound.intent_id.clone(),
            nonce: "7".into(),
            deadline: 100,
            canonical_block_number: 123,
            canonical_block_hash: format!("0x{}", "b".repeat(64)),
            canonical_block_timestamp: 101,
            target_proxy: PAPER_STAKING.into(),
            target_implementation: PAPER_STAKING_IMPLEMENTATION.into(),
            target_code_hash: PAPER_STAKING_CODE_HASH.into(),
            nonce_proxy: PAPER_STAKING.into(),
            nonce_implementation: PAPER_STAKING_IMPLEMENTATION.into(),
            nonce_code_hash: PAPER_STAKING_CODE_HASH.into(),
            batch_executor_proxy: BATCH_EXECUTOR.into(),
            batch_executor_implementation: BATCH_EXECUTOR_IMPLEMENTATION.into(),
            batch_executor_code_hash: BATCH_EXECUTOR_CODE_HASH.into(),
            nonce_unused: true,
            intent_absent: true,
        };
        let submission = Submission {
            kind: "papertrade_intent".into(),
            reference: bound.intent_id.clone(),
            data: json!({"intent_id":bound.intent_id}),
        };
        let expired = expired_intent_reconciliation(&submission, &bound, proof.clone())
            .unwrap()
            .unwrap();
        assert_eq!(expired.status, Status::Failed);
        let verified = expired.verified.unwrap();
        assert!(verified.effects.is_empty());
        assert_eq!(verified.submission, submission);
        assert_eq!(
            verified.proof_digest,
            gofin_domain::verified_effect_digest(&verified).unwrap()
        );

        let mut invalid = proof.clone();
        invalid.nonce_unused = false;
        assert!(
            expired_intent_reconciliation(&submission, &bound, invalid)
                .unwrap()
                .is_none()
        );
        let mut wrong_target = bound.clone();
        wrong_target.typed = PapertradeTypedIntent::CancelIntent {
            user: owner.into(),
            intent_id: format!("0x{}", "c".repeat(64)),
            deadline: "101".into(),
        };
        assert!(
            expired_intent_reconciliation(&submission, &wrong_target, proof)
                .unwrap()
                .is_none()
        );
    }
}
