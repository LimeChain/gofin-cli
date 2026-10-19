//! Jupiter Perps request lifecycle and position observer.
//!
//! Position candidates are obtained from the program-owned account set and
//! decoded against the pinned Perps IDL. Market requests remain pending until
//! their exact request PDA, keeper instruction, event, and effects reconcile.

use gofin_domain::product_names::MetadataValueExt;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, Check, CheckStatus, ConnectionMetadata,
    CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor, FinancialIntent,
    FinancialState, GoFinError, OperationKind, OperationRecord, PreparedOperation, Reconciliation,
    ResourceAmount, ResourceId, Result, SnapshotScope, StateSnapshot, Status, Submission,
    SubmissionJournal,
    boundaries::BoundarySpec,
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
};
use gofin_pricing::UsdPriceSource;
use gofin_solana::transaction::{FinalizedReceiptProof, FrozenTransaction, ReceiptOutcome};
use gofin_solana::{CLUSTER, NETWORK, SolanaWalletAdapter};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};

mod cancel;
mod collateral;
mod limit_create;
mod tpsl;

const POSITION_ACCOUNT: &str = "Position";
const POSITION_REQUEST_ACCOUNT: &str = "PositionRequest";
const MAIN_POOL_NAME: &[u8] = b"Pool";
const OP_POSITIONS: &str = "jupiter.perps.positions";
const OP_MARKET_STATS: &str = "jupiter.perps.market_stats";
const OP_COLLATERAL_LIMITS: &str = "jupiter.perps.collateral.limits";
const OP_INCREASE: &str = "jupiter.perps.increase_market";
const OP_DECREASE: &str = "jupiter.perps.decrease_market";
pub(super) const OP_COLLATERAL_ADD: &str = "jupiter.perps.collateral.add";
pub(super) const OP_COLLATERAL_REMOVE: &str = "jupiter.perps.collateral.remove";
const OP_LIMIT_UPDATE: &str = "jupiter.perps.limit.update";
pub(super) const OP_LIMIT_CREATE: &str = "jupiter.perps.limit.create";
pub(super) const OP_LIMIT_CANCEL: &str = "jupiter.perps.limit.cancel";
const OP_TPSL_CREATE: &str = tpsl::OP_TPSL_CREATE;
const OP_TPSL_UPDATE: &str = tpsl::OP_TPSL_UPDATE;
const OP_TPSL_CANCEL: &str = tpsl::OP_TPSL_CANCEL;
const DEADLINE: Duration = Duration::from_secs(20);
const MAX_POSITIONS: usize = 200;
const MAX_TOKEN_RENT_ACCOUNTS: usize = 512;
// Space includes Anchor's 8-byte account discriminator. These sizes are
// derived from the tracked `perps-published-idl.json` field layouts.
const POSITION_REQUEST_ACCOUNT_SPACE: usize = 286;
const POSITION_ACCOUNT_SPACE: usize = 210;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PerpsBoundaryBounds {
    maximum_size_usd_atoms: String,
    #[serde(default)]
    maximum_collateral_atoms: String,
    #[serde(default)]
    minimum_receive_per_size_numerator: Option<String>,
    #[serde(default)]
    minimum_receive_per_size_denominator: Option<String>,
    maximum_protocol_fee_usd_atoms: String,
    #[serde(default)]
    maximum_protocol_fee_atoms: String,
    maximum_slippage_bps: u16,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
}

pub struct PerpsAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    api: Arc<dyn JupiterApi>,
    prices: Arc<dyn UsdPriceSource>,
    execution_state: Arc<PerpsExecutionState>,
}

#[derive(Default)]
pub(super) struct PerpsExecutionState(
    std::sync::Mutex<Option<Arc<dyn gofin_domain::execution_state::ExecutionState>>>,
);

impl PerpsExecutionState {
    fn get(&self) -> Result<Arc<dyn gofin_domain::execution_state::ExecutionState>> {
        self.0
            .lock()
            .map_err(|_| GoFinError::validation("Jupiter Perps execution-state lock failed."))?
            .clone()
            .ok_or_else(|| {
                GoFinError::new(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "journal",
                    "Jupiter Perps is not attached to its durable execution state.",
                    "Start the matching GoFin broker before recovering a limit request.",
                )
            })
    }
}

pub(super) struct PerpsExecuteSender {
    api: Arc<dyn JupiterApi>,
    action: &'static str,
}

impl PerpsAdapter {
    pub(super) fn execute_sender(&self, action: &'static str) -> PerpsExecuteSender {
        PerpsExecuteSender {
            api: self.api.clone(),
            action,
        }
    }
}

#[async_trait]
impl gofin_solana::transaction::TransactionSender for PerpsExecuteSender {
    fn submission_reference(
        &self,
        frozen: &FrozenTransaction,
        _first_signature: Option<&str>,
    ) -> Result<String> {
        // `/transaction/execute` has no provider request ID. The runtime has
        // already durably keyed this immutable message before calling send.
        Ok(format!(
            "jupiter:perps:{}:{}",
            self.action,
            frozen.message_sha256()
        ))
    }

    async fn send_signed(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
    ) -> Result<String> {
        self.send_checked(signed_transaction_base64, frozen, &|| Ok(()), None)
            .await
    }

    async fn send_signed_with_guard(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
        guard: &dyn gofin_solana::transaction::AsyncDispatchGuard,
    ) -> Result<String> {
        self.send_checked(signed_transaction_base64, frozen, &|| Ok(()), Some(guard))
            .await
    }
}

impl PerpsExecuteSender {
    async fn send_checked(
        &self,
        signed_transaction_base64: &str,
        frozen: &FrozenTransaction,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
        dispatch_guard: Option<&dyn gofin_solana::transaction::AsyncDispatchGuard>,
    ) -> Result<String> {
        before_send()?;
        let first_signature = first_nonzero_signature_from_wire(signed_transaction_base64)?;
        if first_signature.is_none()
            && (frozen.fee_payer() == frozen.signer()
                || !frozen
                    .missing_cosigners()
                    .iter()
                    .any(|signer| signer == frozen.fee_payer()))
        {
            return Err(execute_uncertain());
        }
        let response = self
            .api
            .request_checked(
                JupiterHost::Perps,
                Method::POST,
                "/transaction/execute",
                Some(json!({"action":self.action,"serializedTxBase64":signed_transaction_base64})),
                RateBucket::Main,
                RequestPriority::Financial,
                DEADLINE,
                before_send,
                dispatch_guard,
            )
            .await
            .map_err(|_| execute_uncertain())?;
        if response.get("action").and_then(Value::as_str) != Some(self.action) {
            return Err(execute_uncertain());
        }
        let returned_txid = response
            .get("txid")
            .and_then(Value::as_str)
            .ok_or_else(execute_uncertain)?;
        validate_execute_txid(
            first_signature.as_deref(),
            frozen.fee_payer(),
            frozen.signer(),
            frozen.missing_cosigners(),
            returned_txid,
        )?;
        Ok(returned_txid.to_owned())
    }
}

fn valid_signature_string(signature: &str) -> bool {
    bs58::decode(signature)
        .into_vec()
        .is_ok_and(|value| value.len() == 64 && value.iter().any(|byte| *byte != 0))
}

fn validate_execute_txid(
    first_signature: Option<&str>,
    fee_payer: &str,
    wallet_signer: &str,
    missing_cosigners: &[String],
    returned_txid: &str,
) -> Result<()> {
    if !valid_signature_string(returned_txid) {
        return Err(execute_uncertain());
    }
    match first_signature {
        Some(expected) if expected == returned_txid => Ok(()),
        Some(_) => Err(execute_uncertain()),
        None if fee_payer != wallet_signer
            && missing_cosigners.iter().any(|signer| signer == fee_payer) =>
        {
            // The external fee payer may sign the already-frozen message in
            // `/transaction/execute`. Finalized proof must still bind its hash.
            Ok(())
        }
        None => Err(execute_uncertain()),
    }
}

fn first_nonzero_signature_from_wire(encoded: &str) -> Result<Option<String>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| execute_uncertain())?;
    let (count, offset) = decode_shortvec(&bytes)?;
    if count == 0 || offset + 64 > bytes.len() {
        return Err(execute_uncertain());
    }
    let signature = &bytes[offset..offset + 64];
    if signature.iter().all(|byte| *byte == 0) {
        Ok(None)
    } else {
        let encoded = bs58::encode(signature).into_string();
        valid_signature_string(&encoded)
            .then_some(Some(encoded))
            .ok_or_else(execute_uncertain)
    }
}

#[cfg(test)]
fn first_signature_from_wire(encoded: &str) -> Result<String> {
    first_nonzero_signature_from_wire(encoded)?.ok_or_else(execute_uncertain)
}

fn decode_shortvec(bytes: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0;
    for (index, byte) in bytes.iter().copied().take(3).enumerate() {
        value |= usize::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or_else(execute_uncertain)?;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
        shift += 7;
    }
    Err(execute_uncertain())
}

fn execute_uncertain() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_EXECUTE_UNCERTAIN",
        "submission",
        "Jupiter Perps did not return a transaction identity matching the signed message.",
        "Reconcile the original saved message with finalized Solana evidence; never retry execute.",
    )
}

impl PerpsAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
    ) -> Result<Self> {
        let rpc = wallets
            .values()
            .next()
            .ok_or_else(|| {
                GoFinError::validation("Jupiter Perps needs at least one enrolled Solana wallet.")
            })?
            .runtime()
            .rpc()
            .clone();
        let prices: Arc<dyn UsdPriceSource> =
            Arc::new(crate::JupiterUsdPrices::new(api.clone(), rpc));
        Self::with_price_source(wallets, api, prices)
    }

    pub fn with_price_source(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        api: Arc<dyn JupiterApi>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        if wallets.is_empty()
            || wallets
                .iter()
                .any(|(name, wallet)| name.trim().is_empty() || wallet.address().is_empty())
        {
            return Err(GoFinError::validation(
                "Jupiter Perps needs at least one enrolled Solana wallet.",
            ));
        }
        Ok(Self {
            wallets,
            api,
            prices,
            execution_state: Arc::new(PerpsExecutionState::default()),
        })
    }

    pub(super) fn parent_operation(&self, operation_id: &str) -> Result<Option<OperationRecord>> {
        self.execution_state.get()?.operation(operation_id)
    }

    pub(super) fn recovery_child(&self, operation_id: &str) -> Result<Option<OperationRecord>> {
        self.execution_state.get()?.recovery_child(operation_id)
    }

    pub(super) fn cancellation_children(
        &self,
        operation_id: &str,
        submission: &Submission,
    ) -> Result<Vec<OperationRecord>> {
        self.execution_state
            .get()?
            .cancellation_children(operation_id, submission)
    }

    pub(super) fn parent_amendments(
        &self,
        operation_id: &str,
        operation_name: &str,
    ) -> Result<Vec<OperationRecord>> {
        self.execution_state
            .get()?
            .amendment_children(operation_id, operation_name)
    }

    pub(super) fn protection_candidates(
        &self,
        resource: &ResourceId,
        position: &str,
    ) -> Result<Vec<OperationRecord>> {
        self.execution_state
            .get()?
            .protection_candidates(resource, position)
    }

    fn limit_create_parent(
        &self,
        operation_id: &str,
        wallet: &str,
        request: &str,
    ) -> Result<(OperationRecord, Submission)> {
        let parent = self.parent_operation(operation_id)?.ok_or_else(|| {
            unreviewed_detail(
                "the original Perps limit creation is absent from the durable journal",
            )
        })?;
        let preview = parent.preview.as_ref().ok_or_else(|| {
            unreviewed_detail("the original Perps limit creation has no immutable prepared binding")
        })?;
        let submission = parent.submission.clone().ok_or_else(|| {
            unreviewed_detail("the original Perps limit creation has no durable submission")
        })?;
        if parent.operation != OP_LIMIT_CREATE
            || parent.status.terminal()
            || !matches!(
                &parent.status,
                Status::Submitted | Status::Pending | Status::Unknown
            )
            || preview.binding["wallet_address"].as_str() != Some(wallet)
            || preview.binding["request"].as_str() != Some(request)
            || submission.kind.trim().is_empty()
            || submission.reference.trim().is_empty()
        {
            return Err(unreviewed_detail(
                "the durable parent is not the unresolved creation of this wallet's exact request",
            ));
        }
        if let Some(effects) = parent.effects.as_ref() {
            let terminal = effects.effects.iter().any(|effect| {
                matches!(effect,
                    gofin_domain::FinancialEffect::EscrowPositionProgress {
                        escrow_resource,
                        state: gofin_domain::EscrowLifecycleState::Filled
                            | gofin_domain::EscrowLifecycleState::Cancelled
                            | gofin_domain::EscrowLifecycleState::Expired,
                        ..
                    } if escrow_resource.0.ends_with(request)
                )
            });
            if terminal {
                return Err(unreviewed_detail(
                    "the original limit request already has a terminal keeper or cancellation proof",
                ));
            }
        }
        if self
            .recovery_child(operation_id)?
            .is_some_and(|child| !child.status.terminal() && child.operation == OP_LIMIT_CANCEL)
        {
            return Err(unreviewed_detail(
                "a cancellation of this exact limit request is already unresolved",
            ));
        }
        Ok((parent, submission))
    }

    fn wallet(&self, input: &Value) -> Result<Arc<SolanaWalletAdapter>> {
        let name = input
            .get("wallet")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select an enrolled Solana wallet."))?;
        self.wallets.get(name).cloned().ok_or_else(|| {
            GoFinError::new(
                "JUPITER_WALLET_UNKNOWN",
                "connection",
                "The selected Solana wallet is not enrolled for Jupiter Perps.",
                "Choose an enrolled wallet from connection discovery.",
            )
        })
    }

    async fn permission_scope(
        &self,
        operation: &str,
        input: &Value,
        bounds_value: &Value,
    ) -> Result<PlanPermission> {
        validate_network(input)?;
        if operation == OP_LIMIT_CREATE {
            return limit_create::permission_scope(self, input, bounds_value).await;
        }
        if operation == OP_TPSL_CREATE {
            return tpsl::permission_scope(self, operation, input, bounds_value).await;
        }
        if operation == OP_TPSL_UPDATE || operation == OP_TPSL_CANCEL {
            return tpsl::permission_scope(self, operation, input, bounds_value).await;
        }
        if operation == OP_LIMIT_UPDATE {
            return permission_limit_update(self, input, bounds_value).await;
        }
        if operation == OP_LIMIT_CANCEL {
            return cancel::permission_scope(self, input, bounds_value).await;
        }
        if !is_market_write_operation(operation) {
            return Err(permission_unsupported());
        }
        let bounds: PerpsBoundaryBounds = serde_json::from_value(bounds_value.clone())
            .map_err(|_| GoFinError::validation("Invalid Jupiter Perps boundary limits."))?;
        let max_size = gofin_domain::Quantity::new(&bounds.maximum_size_usd_atoms)?;
        let max_collateral = gofin_domain::Quantity::new(&bounds.maximum_collateral_atoms)?;
        let max_fee_usd = gofin_domain::Quantity::new(&bounds.maximum_protocol_fee_usd_atoms)?;
        let _max_fee_token = gofin_domain::Quantity::new(&bounds.maximum_protocol_fee_atoms)?;
        let collateral_only = is_collateral_only_operation(operation);
        let increasing = is_increase_operation(operation);
        if (!collateral_only && max_size.is_zero())
            || max_fee_usd.is_zero()
            || bounds.maximum_slippage_bps == 0
            || bounds.maximum_slippage_bps > 5_000
            || bounds.maximum_network_fee_lamports == 0
            || bounds.maximum_account_funding_lamports == 0
            || (increasing && max_collateral.is_zero())
            || (operation == OP_COLLATERAL_REMOVE && max_collateral.is_zero())
        {
            return Err(GoFinError::validation(
                "Perps permissions require finite position, collateral, fee, slippage and funding caps.",
            ));
        }
        let wallet_name = required_text(input, "wallet")?.to_owned();
        let wallet = self.wallet(input)?;
        let (market, market_mint, collateral_mint, side, position, funding_symbol) = if increasing {
            let market = required_market_symbol(input, "market")?;
            let side = match required_text(input, "side")? {
                "long" => 1,
                "short" => 2,
                _ => return Err(GoFinError::validation("Choose long or short Perps side.")),
            };
            let market_mint = market_mint_for_symbol(market)?.to_owned();
            let funding_symbol = required_funding_symbol(input, side, market)?;
            let (collateral_mint, position) = if operation == OP_COLLATERAL_ADD {
                let requested = required_text(input, "position")?.to_owned();
                gofin_solana::parse_address(&requested)?;
                let current = self.position_by_address(&wallet, &requested).await?;
                let current_side = if current.side == gofin_domain::PositionKind::DerivativeLong {
                    1
                } else if current.side == gofin_domain::PositionKind::DerivativeShort {
                    2
                } else {
                    return Err(invalid_account_set());
                };
                let collateral_mint = expected_position_collateral_mint(
                    market,
                    side,
                    Some(&current.collateral_mint),
                )?;
                let derived = derive_position_address(
                    wallet.address(),
                    &market_mint,
                    &collateral_mint,
                    side,
                )?;
                if requested != derived {
                    return Err(GoFinError::validation(
                        "The selected position does not match the wallet, market and side identities.",
                    ));
                }
                if current.market_mint != market_mint || current_side != side {
                    return Err(GoFinError::validation(
                        "The selected position does not match the requested market and side.",
                    ));
                }
                (collateral_mint, requested)
            } else {
                let collateral_mint = expected_position_collateral_mint(market, side, None)?;
                let derived = derive_position_address(
                    wallet.address(),
                    &market_mint,
                    &collateral_mint,
                    side,
                )?;
                (collateral_mint, derived)
            };
            (
                market,
                market_mint,
                collateral_mint,
                side,
                position,
                funding_symbol,
            )
        } else {
            let position = required_text(input, "position")?.to_owned();
            gofin_solana::parse_address(&position)?;
            let (market, market_mint, collateral_mint, side) =
                derive_position_identity(wallet.address(), &position)?;
            if !supported_decrease_collateral(market, side, &collateral_mint) {
                return Err(GoFinError::validation(
                    "This Perps position has an unsupported collateral contract for bounded decreases.",
                ));
            }
            let receive = required_text(input, "receive_token")?;
            if !supported_receive_mint(token_mint_for_symbol(receive)?) {
                return Err(GoFinError::validation(
                    "Choose a supported token for the Perps decrease payout.",
                ));
            }
            (
                market,
                market_mint,
                collateral_mint,
                side,
                position,
                receive,
            )
        };
        if operation == OP_DECREASE {
            let numerator = bounds
                .minimum_receive_per_size_numerator
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| GoFinError::validation("Perps decrease permission requires a positive minimum receive rate numerator."))?;
            let denominator = bounds
                .minimum_receive_per_size_denominator
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| GoFinError::validation("Perps decrease permission requires a positive minimum receive rate denominator."))?;
            if (u128::from(numerator))
                .checked_mul(u128::from(
                    bounds.maximum_size_usd_atoms.parse::<u64>().map_err(|_| {
                        GoFinError::validation("Perps size bound must be integer atoms.")
                    })?,
                ))
                .is_none()
                || (u128::from(denominator))
                    .checked_mul(u128::from(u64::MAX))
                    .is_none()
            {
                return Err(GoFinError::validation(
                    "Perps decrease receive rate exceeds the supported range.",
                ));
            }
        }
        let funding_mint = token_mint_for_symbol(funding_symbol)?.to_owned();
        let funding_decimals =
            crate::adapter::token_decimals(&wallet.runtime(), &funding_mint).await?;
        let maximum_size_atoms = bounds
            .maximum_size_usd_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Perps size bound must be integer atoms."))?;
        let maximum_collateral_atoms = bounds
            .maximum_collateral_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Perps collateral bound must be integer atoms."))?;
        let maximum_fee_usd_atoms = bounds
            .maximum_protocol_fee_usd_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Perps fee bound must be integer atoms."))?;
        let maximum_fee_token_atoms = bounds
            .maximum_protocol_fee_atoms
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Perps token fee bound must be integer atoms."))?;
        let collateral_decimals =
            crate::adapter::token_decimals(&wallet.runtime(), &collateral_mint).await?;
        let collateral_symbol = token_symbol_for_mint(&collateral_mint).ok_or_else(|| {
            GoFinError::validation("Perps collateral mint has no verified token label.")
        })?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let funding_asset = asset(&funding_mint, funding_decimals);
        let native_asset = asset("native", 9);
        let margin_asset = margin_usd_asset();
        let mut maximum_debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native_asset.clone(),
                atoms: gofin_domain::Quantity::from_u64(bounds.maximum_account_funding_lamports),
            },
        }];
        if increasing {
            maximum_debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: funding_asset.clone(),
                    atoms: max_collateral.clone(),
                },
            });
        }
        let mut possible_credits = Vec::new();
        if !increasing {
            possible_credits.push(FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: funding_asset,
            });
        }
        let parameters = serde_json::to_value(&bounds)
            .map_err(|_| GoFinError::validation("Perps boundary limits are invalid."))?;
        let spec = BoundarySpec {
            fixed: json!({
                "operation":operation,
                "wallet_name":wallet_name,
                "wallet_address":wallet.address(),
                "network":NETWORK,
                "cluster":CLUSTER,
                "program":crate::jlp::PERPS_PROGRAM_ID,
                "market":market,
                "market_mint":market_mint,
                "collateral_mint":collateral_mint,
                "side":side,
                "position":position,
                "funding_mint":funding_mint,
                "receive_token":if !increasing {Some(funding_symbol)} else {None},
                "collateral_usd_atoms":if operation == OP_COLLATERAL_REMOVE {input.get("collateral_usd_atoms")} else {None},
                "minimum_receive_atoms":if operation == OP_COLLATERAL_REMOVE {input.get("minimum_receive_atoms")} else {None},
            }),
            parameters,
            maximum_debits,
            maximum_costs: vec![
                CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: native_asset.clone(),
                        atoms: gofin_domain::Quantity::from_u64(
                            bounds.maximum_network_fee_lamports,
                        ),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                },
                position_protocol_fee_bound(
                    resource.clone(),
                    format!("{position}:collateral"),
                    margin_asset.clone(),
                    max_fee_usd,
                    increasing,
                ),
            ],
            summary: market_permission_summary(
                market,
                side,
                collateral_only,
                maximum_size_atoms,
                maximum_collateral_atoms,
                funding_symbol,
                funding_decimals,
                maximum_fee_usd_atoms,
                maximum_fee_token_atoms,
                collateral_symbol,
                collateral_decimals,
                bounds.maximum_slippage_bps,
                bounds.maximum_network_fee_lamports,
                bounds.maximum_account_funding_lamports,
            ),
        };
        Ok(PlanPermission {
            recovery_parent: None,
            spec,
            possible_credits,
            fee_assets: vec![native_asset, margin_asset],
            deferred_receipt: None,
            authorization_seconds: Some(600),
            cumulative_scope: None,
        })
    }

    fn validate_permission_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        if prepared.operation == OP_LIMIT_CREATE {
            return limit_create::validate_permission_candidate(spec, prepared);
        }
        if prepared.operation == OP_TPSL_CREATE {
            return tpsl::validate_permission_candidate(spec, prepared);
        }
        if prepared.operation == OP_TPSL_UPDATE || prepared.operation == OP_TPSL_CANCEL {
            return tpsl::validate_permission_candidate(spec, prepared);
        }
        if prepared.operation == OP_LIMIT_UPDATE {
            return validate_limit_update_permission_candidate(spec, prepared);
        }
        if prepared.operation == OP_LIMIT_CANCEL {
            return cancel::validate_permission_candidate(spec, prepared);
        }
        let bounds: PerpsBoundaryBounds = serde_json::from_value(spec.parameters.clone())
            .map_err(|_| GoFinError::validation("Approved Perps boundary limits are invalid."))?;
        let binding = &prepared.binding;
        let size = parse_binding_u64(binding, "size_usd_atoms")?;
        let collateral = parse_binding_u64(binding, "collateral_atoms")?;
        let fee_usd = parse_binding_u64(binding, "maximum_protocol_fee_usd_atoms")?;
        let fee_token = parse_binding_u64(binding, "maximum_protocol_fee_atoms")?;
        let network_fee = parse_binding_u64(binding, "maximum_network_fee_lamports")?;
        let account_funding = parse_binding_u64(binding, "maximum_account_funding_lamports")?;
        let slippage = binding
            .get("maximum_slippage_bps")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                GoFinError::validation("Perps boundary candidate is missing its slippage binding.")
            })?;
        let actual_fixed = [
            ("operation", Value::String(prepared.operation.clone())),
            ("network", json!(NETWORK)),
            ("cluster", json!(CLUSTER)),
            ("program", json!(crate::jlp::PERPS_PROGRAM_ID)),
            (
                "wallet_name",
                prepared.input.get("wallet").cloned().unwrap_or(Value::Null),
            ),
            (
                "wallet_address",
                binding
                    .get("wallet_address")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "market_mint",
                binding.get("market_mint").cloned().unwrap_or(Value::Null),
            ),
            (
                "collateral_mint",
                binding
                    .get("collateral_mint")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "funding_mint",
                binding.get("funding_mint").cloned().unwrap_or(Value::Null),
            ),
            ("side", binding.get("side").cloned().unwrap_or(Value::Null)),
            (
                "position",
                binding.get("position").cloned().unwrap_or(Value::Null),
            ),
        ];
        if actual_fixed
            .iter()
            .any(|(key, value)| spec.fixed.get(*key) != Some(value))
            || (size == 0 && !is_collateral_only_operation(&prepared.operation))
            || size
                > bounds
                    .maximum_size_usd_atoms
                    .parse::<u64>()
                    .map_err(|_| GoFinError::validation("Perps size boundary is invalid."))?
            || (is_increase_operation(&prepared.operation)
                && (collateral == 0
                    || collateral
                        > bounds
                            .maximum_collateral_atoms
                            .parse::<u64>()
                            .map_err(|_| {
                                GoFinError::validation("Perps collateral boundary is invalid.")
                            })?))
            || (prepared.operation == OP_COLLATERAL_REMOVE
                && (collateral == 0
                    || collateral
                        > bounds
                            .maximum_collateral_atoms
                            .parse::<u64>()
                            .map_err(|_| {
                                GoFinError::validation("Perps collateral boundary is invalid.")
                            })?))
            || fee_usd
                > bounds
                    .maximum_protocol_fee_usd_atoms
                    .parse::<u64>()
                    .map_err(|_| GoFinError::validation("Perps fee boundary is invalid."))?
            || fee_token
                > bounds
                    .maximum_protocol_fee_atoms
                    .parse::<u64>()
                    .map_err(|_| GoFinError::validation("Perps token fee boundary is invalid."))?
            || network_fee > bounds.maximum_network_fee_lamports
            || account_funding > bounds.maximum_account_funding_lamports
            || slippage > u64::from(bounds.maximum_slippage_bps)
        {
            return Err(GoFinError::new(
                "BOUNDARY_AUTHORIZATION_VIOLATION",
                "authorization",
                "The Perps candidate exceeds its approved market, position, collateral or cost scope.",
                "Prepare a new finite permission covering the exact intended Perps request.",
            ));
        }
        if prepared.operation == OP_COLLATERAL_REMOVE
            && ["collateral_usd_atoms", "minimum_receive_atoms"]
                .iter()
                .any(|key| {
                    spec.fixed
                        .get(*key)
                        .is_some_and(|expected| !expected.is_null())
                        && prepared.input.get(*key) != spec.fixed.get(*key)
                })
        {
            return Err(GoFinError::new(
                "BOUNDARY_AUTHORIZATION_VIOLATION",
                "authorization",
                "The Perps collateral withdrawal differs from its approved amount or minimum payout.",
                "Prepare a new finite permission for the intended withdrawal.",
            ));
        }
        if prepared.operation == OP_DECREASE {
            let numerator = bounds
                .minimum_receive_per_size_numerator
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Perps decrease permission lacks its minimum receive rate.",
                    )
                })?;
            let denominator = bounds
                .minimum_receive_per_size_denominator
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Perps decrease permission has an invalid receive rate denominator.",
                    )
                })?;
            let received = binding
                .get("minimum_receive_atoms")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Perps decrease candidate lacks its minimum receive amount.",
                    )
                })?;
            if u128::from(received)
                .checked_mul(u128::from(denominator))
                .unwrap_or(0)
                < u128::from(size)
                    .checked_mul(u128::from(numerator))
                    .ok_or_else(|| GoFinError::validation("Perps receive rate overflows."))?
            {
                return Err(GoFinError::validation(
                    "Perps decrease minimum payout falls below its approved rate.",
                ));
            }
        }
        Ok(())
    }

    async fn prepare_market_trade(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation> {
        validate_network(input)?;
        let wallet = self.wallet(input)?;
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected wallet has no protected signing capability.",
                "Enroll a protected signer before preparing a Jupiter Perps market request.",
            )
        })?;
        let increasing = is_increase_operation(operation);
        let collateral_only = is_collateral_only_operation(operation);
        if !is_market_write_operation(operation) {
            return Err(unknown());
        }
        let maximum_network_fee_lamports = required_u64(input, "maximum_network_fee_lamports")?;
        let maximum_account_funding_lamports =
            required_u64(input, "maximum_account_funding_lamports")?;
        let maximum_protocol_fee_usd_atoms = required_u64(input, "maximum_protocol_fee_usd_atoms")?;
        let maximum_protocol_fee_atoms = if increasing {
            required_u64(input, "maximum_protocol_fee_atoms")?
        } else {
            0
        };
        let max_slippage_bps = required_u64(input, "maximum_slippage_bps")?;
        if maximum_network_fee_lamports == 0 || max_slippage_bps == 0 || max_slippage_bps > 5_000 {
            return Err(GoFinError::validation(
                "Perps fee and slippage bounds must be positive and no more than 5,000 bps.",
            ));
        }
        let (
            path,
            body,
            expected_position,
            collateral_atoms,
            expected_size,
            side,
            entire_position,
            funding_token,
            market,
            expected_collateral_mint,
        ) = if operation == OP_COLLATERAL_ADD {
            let position = required_text(input, "position")?.to_owned();
            gofin_solana::parse_address(&position)?;
            let current = self.position_by_address(&wallet, &position).await?;
            let market = required_market_symbol(input, "market")?;
            let requested_side = match required_text(input, "side")? {
                "long" => 1u8,
                "short" => 2u8,
                _ => return Err(GoFinError::validation("Perps side must be long or short.")),
            };
            let actual_side = if current.side == gofin_domain::PositionKind::DerivativeLong {
                1
            } else {
                2
            };
            if market_symbol_for_mint(&current.market_mint) != Some(market)
                || actual_side != requested_side
            {
                return Err(GoFinError::validation(
                    "The selected position does not match the requested collateral market and side.",
                ));
            }
            let funding_token = required_funding_symbol(input, requested_side, market)?;
            let collateral_atoms = required_u64(input, "collateral_amount_atoms")?;
            if collateral_atoms == 0 {
                return Err(GoFinError::validation(
                    "Perps collateral must be a positive atomic amount.",
                ));
            }
            (
                "/positions/increase",
                json!({
                    "asset":market,
                    "inputToken":funding_token,
                    "inputTokenAmount":collateral_atoms.to_string(),
                    "maxSlippageBps":max_slippage_bps.to_string(),
                    "side":if requested_side == 1 {"long"} else {"short"},
                    "walletAddress":wallet.address()
                }),
                Some(position),
                collateral_atoms,
                0,
                requested_side,
                false,
                funding_token,
                market,
                Some(current.collateral_mint.clone()),
            )
        } else if increasing {
            let market = required_market_symbol(input, "market")?;
            let side = match required_text(input, "side")? {
                "long" => 1u8,
                "short" => 2u8,
                _ => return Err(GoFinError::validation("Perps side must be long or short.")),
            };
            let funding_token = required_funding_symbol(input, side, market)?;
            let collateral_atoms = required_u64(input, "collateral_amount_atoms")?;
            let size = required_u64(input, "size_usd_atoms")?;
            if collateral_atoms == 0 || size == 0 {
                return Err(GoFinError::validation(
                    "Perps collateral and size must be positive atomic amounts.",
                ));
            }
            (
                "/positions/increase",
                json!({
                    "asset":market,
                    "inputToken":funding_token,
                    "inputTokenAmount":collateral_atoms.to_string(),
                    "sizeUsdDelta":size.to_string(),
                    "maxSlippageBps":max_slippage_bps.to_string(),
                    "side":if side == 1 {"long"} else {"short"},
                    "walletAddress":wallet.address()
                }),
                None,
                collateral_atoms,
                size,
                side,
                false,
                funding_token,
                market,
                None,
            )
        } else if operation == OP_COLLATERAL_REMOVE {
            let position = required_text(input, "position")?.to_owned();
            gofin_solana::parse_address(&position)?;
            let requested_collateral_usd = required_u64(input, "collateral_usd_atoms")?;
            if requested_collateral_usd == 0 {
                return Err(GoFinError::validation(
                    "Perps collateral withdrawal must be a positive USD atomic amount.",
                ));
            }
            let fields = self.position_by_address(&wallet, &position).await?;
            if requested_collateral_usd > fields.collateral_usd_atoms {
                return Err(GoFinError::validation(
                    "Collateral withdrawal cannot exceed the exact current position collateral.",
                ));
            }
            let market = market_symbol_for_mint(&fields.market_mint).ok_or_else(unreviewed)?;
            let side = if fields.side == gofin_domain::PositionKind::DerivativeLong {
                1
            } else {
                2
            };
            let receive = required_text(input, "receive_token")?;
            if !supported_decrease_collateral(market, side, &fields.collateral_mint)
                || !supported_receive_mint(token_mint_for_symbol(receive)?)
            {
                return Err(invalid_account_set());
            }
            (
                "/positions/decrease",
                json!({
                    "positionPubkey":position,
                    "receiveToken":receive,
                    "collateralUsdDelta":requested_collateral_usd.to_string(),
                    "sizeUsdDelta":null,
                    "maxSlippageBps":max_slippage_bps.to_string(),
                    "entirePosition":false
                }),
                Some(position),
                requested_collateral_usd,
                0,
                side,
                false,
                receive,
                market,
                Some(fields.collateral_mint.clone()),
            )
        } else {
            let position = required_text(input, "position")?.to_owned();
            gofin_solana::parse_address(&position)?;
            let size = required_u64(input, "size_usd_atoms")?;
            if size == 0 {
                return Err(GoFinError::validation(
                    "Perps decrease size must be positive USD atomic units.",
                ));
            }
            let fields = self.position_by_address(&wallet, &position).await?;
            let market = market_symbol_for_mint(&fields.market_mint).ok_or_else(unreviewed)?;
            let side = if fields.side == gofin_domain::PositionKind::DerivativeLong {
                1
            } else {
                2
            };
            let receive = required_text(input, "receive_token")?;
            if !supported_decrease_collateral(market, side, &fields.collateral_mint)
                || !supported_receive_mint(token_mint_for_symbol(receive)?)
            {
                return Err(invalid_account_set());
            }
            if size > fields.size_usd_atoms {
                return Err(GoFinError::validation(
                    "A Perps decrease cannot exceed the exact current USD notional.",
                ));
            }
            let entire_position = size == fields.size_usd_atoms;
            (
                "/positions/decrease",
                json!({
                    "positionPubkey":position,
                    "receiveToken":receive,
                    "sizeUsdDelta":size.to_string(),
                    "maxSlippageBps":max_slippage_bps.to_string(),
                    "entirePosition":entire_position
                }),
                Some(position),
                0,
                size,
                side,
                entire_position,
                receive,
                market,
                Some(fields.collateral_mint.clone()),
            )
        };
        let minimum_receive_atoms = if increasing {
            0
        } else {
            required_u64(input, "minimum_receive_atoms")?
        };
        let response = self
            .api
            .request(
                JupiterHost::Perps,
                Method::POST,
                path,
                Some(body),
                RateBucket::Main,
                RequestPriority::Financial,
                DEADLINE,
            )
            .await?;
        let quote = response
            .get("quote")
            .filter(|value| value.is_object())
            .ok_or_else(bad_response)?;
        let encoded = response
            .get("serializedTxBase64")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(bad_response)?
            .to_owned();
        let position_claim = response
            .get("positionPubkey")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(bad_response)?
            .to_owned();
        gofin_solana::parse_address(&position_claim)?;
        if expected_position
            .as_deref()
            .is_some_and(|p| p != position_claim)
        {
            return Err(unreviewed());
        }
        let meta = response
            .get("txMetadata")
            .filter(|v| !v.is_null())
            .ok_or_else(bad_response)?;
        let last_valid_block_height = parse_u64_text(meta, "lastValidBlockHeight")?;
        let runtime = wallet.runtime();
        let frozen = runtime
            .freeze_transaction(
                &encoded,
                &signer,
                last_valid_block_height,
                maximum_network_fee_lamports,
                None,
                &[],
            )
            .await?;
        let instruction = inspect_market_request(
            &wallet,
            &frozen,
            increasing,
            expected_size,
            collateral_atoms,
            side,
            max_slippage_bps,
            minimum_receive_atoms,
            market,
            funding_token,
            expected_position.as_deref(),
            entire_position,
            expected_collateral_mint.as_deref(),
        )
        .await?;
        let fee_basis_usd_atoms = if collateral_only {
            if increasing {
                parse_u64_text(quote, "collateralUsdDelta")?
            } else {
                collateral_atoms
            }
        } else {
            expected_size
        };
        let position_fee_ceiling = ceil_bps(fee_basis_usd_atoms, instruction.maximum_fee_bps)?;
        let pre_position = self
            .existing_position(&wallet, &instruction.position)
            .await?;
        let position_pre_lamports =
            account_lamports_if_present(&wallet, &instruction.position).await?;
        if pre_position.is_some() != position_pre_lamports.is_some()
            || pre_position
                .as_ref()
                .zip(position_pre_lamports)
                .is_some_and(|(position, lamports)| position.rent_lamports != lamports)
        {
            return Err(invalid_account_set_detail(
                "position account identity and rent state",
            ));
        }
        let request_pre_lamports =
            account_lamports_if_present(&wallet, &instruction.request).await?;
        if request_pre_lamports.is_some() {
            return Err(GoFinError::new(
                "JUPITER_PERPS_REQUEST_EXISTS",
                "prepare",
                "The derived Perps request account already exists.",
                "Reconcile the existing request before preparing another market action.",
            ));
        }
        let request_account_rent_lamports =
            minimum_account_rent(&wallet, POSITION_REQUEST_ACCOUNT_SPACE).await?;
        let position_account_rent_lamports = if increasing && pre_position.is_none() {
            minimum_account_rent(&wallet, POSITION_ACCOUNT_SPACE).await?
        } else {
            0
        };
        let borrow_fee_ceiling = borrow_fee_ceiling_usd(
            &instruction,
            pre_position.as_ref(),
            expected_size,
            increasing,
        )?;
        let total_protocol_fee_ceiling = position_fee_ceiling
            .checked_add(borrow_fee_ceiling)
            .ok_or_else(|| {
                GoFinError::validation("The Perps fee ceiling exceeds the supported range.")
            })?;
        validate_quote_fee_bounds(
            quote,
            increasing,
            position_fee_ceiling,
            borrow_fee_ceiling,
            total_protocol_fee_ceiling,
        )?;
        if maximum_protocol_fee_usd_atoms < total_protocol_fee_ceiling {
            return Err(GoFinError::validation(
                "The approved USD fee cap is below the finalized custody's position and bounded accrued-interest fee ceiling.",
            ));
        }
        let mut minimum_net_collateral_atoms = None;
        let mut minimum_net_receive_atoms = None;
        if increasing && instruction.funding_mint != instruction.collateral_mint {
            let minimum_output = instruction.minimum_output.ok_or_else(unreviewed)?;
            let floor = conversion_output_floor(
                self.prices.clone(),
                &instruction,
                collateral_atoms,
                max_slippage_bps,
                maximum_protocol_fee_usd_atoms,
            )
            .await?;
            if minimum_output < floor.as_str().parse::<u64>().map_err(|_| bad_response())? {
                return Err(GoFinError::validation(
                    "The Perps internal collateral swap minimum output is below the independently priced net-margin floor.",
                ));
            }
            minimum_net_collateral_atoms = Some(floor);
        } else if !increasing {
            let quoted_output = parse_u64_text(quote, "transferAmountToken")?;
            let slippage_floor = gofin_domain::Quantity::from_u64(quoted_output).mul_div_floor(
                &gofin_domain::Quantity::from_u64(10_000 - max_slippage_bps),
                &gofin_domain::Quantity::from_u64(10_000),
            )?;
            if slippage_floor.integer()
                > gofin_domain::Quantity::from_u64(minimum_receive_atoms).integer()
            {
                return Err(GoFinError::validation(
                    "The approved minimum receive amount is below the requested Perps slippage floor.",
                ));
            }
            minimum_net_receive_atoms = Some(slippage_floor);
        }
        if !increasing && pre_position.is_none() {
            return Err(invalid_account_set());
        }
        let expected_collateral_mint =
            expected_position_collateral_mint(market, side, expected_collateral_mint.as_deref())?;
        if instruction.collateral_mint != expected_collateral_mint {
            return Err(GoFinError::validation(
                "The request collateral mint does not match the verified Perps position or supported market default.",
            ));
        }
        let average_price_usd = parse_u64_text(quote, "averagePriceUsd")?;
        if average_price_usd == 0
            || quote.get("side").and_then(Value::as_str)
                != Some(if side == 1 { "long" } else { "short" })
        {
            return Err(unreviewed());
        }
        let position_amount_atoms = expected_size;
        let expected_output_atoms = if collateral_only && increasing {
            parse_u64_text(quote, "collateralUsdDelta")?
        } else if increasing {
            position_amount_atoms
        } else {
            parse_u64_text(quote, "transferAmountToken")?
        };
        if expected_output_atoms == 0
            || (!increasing && expected_output_atoms < minimum_receive_atoms)
        {
            return Err(unreviewed());
        }
        let allowed_mints = vec![
            instruction.market_mint.clone(),
            instruction.collateral_mint.clone(),
            instruction.funding_mint.clone(),
        ];
        let auxiliary = crate::adapter::validate_auxiliary_instructions(
            &wallet,
            &frozen,
            &allowed_mints,
            (instruction.funding_mint == crate::swap::WRAPPED_SOL_MINT).then_some(collateral_atoms),
            maximum_account_funding_lamports,
        )
        .await?;
        let pre_token_rents = token_account_rent_states(&wallet)
            .await?
            .into_iter()
            .filter_map(|state| {
                let address = state.position.strip_prefix("account-funding:")?;
                auxiliary
                    .closed_wrapped_native_accounts
                    .iter()
                    .any(|closed| closed == address)
                    .then(|| (address.to_owned(), state.amount.atoms.as_str().to_owned()))
            })
            .collect::<BTreeMap<_, _>>();
        if auxiliary
            .closed_wrapped_native_accounts
            .iter()
            .any(|address| !pre_token_rents.contains_key(address))
        {
            return Err(unreviewed());
        }
        runtime.validate_fee(&frozen).await?;
        runtime.simulate(&frozen).await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let funding_asset = asset(&instruction.funding_mint, instruction.funding_decimals);
        let position_asset = crate::pricing::position_usd_asset(&instruction.market_mint)?;
        let (output_asset, output_atoms) = if increasing {
            (funding_asset.clone(), collateral_atoms)
        } else {
            (funding_asset.clone(), expected_output_atoms)
        };
        let (valuation_input_asset, valuation_input_atoms) = if collateral_only {
            (
                margin_usd_asset(),
                if increasing {
                    parse_u64_text(quote, "collateralUsdDelta")?
                } else {
                    collateral_atoms
                },
            )
        } else {
            (position_asset.clone(), expected_size)
        };
        let (mut valuation, fee_bound) = crate::adapter::valuation(
            self.prices.clone(),
            valuation_input_asset,
            valuation_input_atoms,
            output_asset,
            output_atoms,
            maximum_network_fee_lamports,
            wallet.address(),
        )
        .await?;
        let swap_fee_ceiling =
            if increasing && instruction.funding_mint != instruction.collateral_mint {
                let funding_price = valuation
                    .prices
                    .iter()
                    .find(|price| price.asset == funding_asset)
                    .ok_or_else(bad_response)?;
                let funding_value = value_atoms_micros(collateral_atoms, funding_price)?;
                ceil_bps(
                    funding_value
                        .as_str()
                        .parse::<u64>()
                        .map_err(|_| bad_response())?,
                    instruction.swap_fee_bps,
                )?
            } else {
                0
            };
        let full_protocol_fee_ceiling = total_protocol_fee_ceiling
            .checked_add(swap_fee_ceiling)
            .ok_or_else(invalid_account_set)?;
        if maximum_protocol_fee_usd_atoms < full_protocol_fee_ceiling {
            return Err(GoFinError::validation(
                "The approved USD fee cap is below the position, accrued-interest and finalized pool swap-fee ceiling.",
            ));
        }
        if increasing {
            let collateral_price = self
                .prices
                .quote(&asset(
                    &instruction.collateral_mint,
                    instruction.collateral_decimals,
                ))
                .await?;
            let collateral_asset_price = crate::adapter::valuation_asset_price(
                &asset(
                    &instruction.collateral_mint,
                    instruction.collateral_decimals,
                ),
                &collateral_price,
            )?;
            let required_fee_token_atoms =
                gofin_domain::Quantity::from_u64(full_protocol_fee_ceiling)
                    .mul_div_ceil(
                        &collateral_asset_price.unit_atoms,
                        &collateral_asset_price.usd_micros_per_unit,
                    )?
                    .as_str()
                    .parse::<u64>()
                    .map_err(|_| bad_response())?;
            if maximum_protocol_fee_atoms < required_fee_token_atoms {
                return Err(GoFinError::validation(
                    "The approved collateral-token fee cap is below the independently computed position, borrow and pool-swap fee ceiling.",
                )
                .with_details(json!({
                    "required_maximum_protocol_fee_atoms": required_fee_token_atoms.to_string(),
                    "fee_asset": instruction.collateral_mint,
                    "fee_asset_decimals": instruction.collateral_decimals,
                    "approved_maximum_protocol_fee_atoms": maximum_protocol_fee_atoms.to_string(),
                    "required_maximum_protocol_fee_usd_atoms": full_protocol_fee_ceiling.to_string(),
                    "approved_maximum_protocol_fee_usd_atoms": maximum_protocol_fee_usd_atoms.to_string(),
                })));
            }
        }
        let protocol_fee_usd = gofin_domain::Quantity::from_u64(maximum_protocol_fee_usd_atoms);
        let margin_asset = margin_usd_asset();
        let margin_prices = valuation
            .prices
            .iter()
            .filter(|price| price.asset == margin_asset)
            .count();
        if margin_prices > 1 {
            return Err(bad_response());
        }
        if margin_prices == 0 {
            let margin_quote = self.prices.quote(&margin_asset).await?;
            valuation.prices.push(crate::adapter::valuation_asset_price(
                &margin_asset,
                &margin_quote,
            )?);
        }
        let wallet_funding_accounts = auxiliary
            .funding_accounts
            .iter()
            .filter(|funding| {
                funding.payer == wallet.address()
                    && !auxiliary
                        .closed_wrapped_native_accounts
                        .contains(&funding.address)
            })
            .collect::<Vec<_>>();
        let wallet_account_funding_lamports = wallet_funding_accounts
            .iter()
            .try_fold(0u64, |total, funding| {
                total.checked_add(funding.lamports).ok_or_else(bad_response)
            })?;
        if wallet_account_funding_lamports > maximum_account_funding_lamports {
            return Err(unreviewed());
        }
        let native_price = valuation
            .prices
            .iter()
            .find(|price| price.asset == asset("native", 9))
            .ok_or_else(bad_response)?;
        let account_funding_usd =
            gofin_domain::Quantity::from_u64(maximum_account_funding_lamports)
                .mul_div_ceil(&native_price.usd_micros_per_unit, &native_price.unit_atoms)?;
        let required_account_funding_lamports = wallet_account_funding_lamports
            .checked_add(request_account_rent_lamports)
            .and_then(|value| value.checked_add(position_account_rent_lamports))
            .ok_or_else(bad_response)?;
        if required_account_funding_lamports > maximum_account_funding_lamports {
            return Err(GoFinError::validation(
                "The approved account-funding cap is below the finalized rent required for the Perps request/position PDAs and token accounts.",
            ));
        }
        let position_rent_funding_cap = position_account_rent_lamports;
        let valuation = gofin_domain::Valuation {
            maximum_fee_usd_micros: if increasing {
                valuation.maximum_fee_usd_micros
            } else {
                valuation
                    .maximum_fee_usd_micros
                    .checked_add(&protocol_fee_usd)?
            },
            additional_allocation_usd_micros: account_funding_usd,
            ..valuation
        };
        let position_kind = if side == 1 {
            gofin_domain::PositionKind::DerivativeLong
        } else {
            gofin_domain::PositionKind::DerivativeShort
        };
        let bps = gofin_domain::Quantity::from_u64(max_slippage_bps);
        let ten_thousand = gofin_domain::Quantity::from_u64(10_000);
        let _slippage_bound = (bps, ten_thousand);
        let min_delta = gofin_domain::Quantity::from_u64(position_amount_atoms);
        let max_delta = gofin_domain::Quantity::from_u64(position_amount_atoms);
        let signed_min = if increasing {
            min_delta.as_str().to_owned()
        } else {
            format!("-{}", max_delta.as_str())
        };
        let signed_max = if increasing {
            max_delta.as_str().to_owned()
        } else {
            format!("-{}", min_delta.as_str())
        };
        let position = instruction.position.clone();
        let mut maximum_costs = vec![fee_bound];
        if maximum_protocol_fee_usd_atoms > 0 {
            maximum_costs.push(position_protocol_fee_bound(
                resource.clone(),
                format!("{position}:collateral"),
                margin_usd_asset(),
                gofin_domain::Quantity::from_u64(maximum_protocol_fee_usd_atoms),
                increasing,
            ));
        }
        let mut expected = Vec::new();
        if increasing {
            expected.push(EffectConstraint::CostDenomination {
                location: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: format!("{position}:collateral"),
                    position_kind: gofin_domain::PositionKind::LockedCollateral,
                },
                cost_asset: margin_usd_asset(),
                category: "jupiter_perps_protocol_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
                denomination_asset: asset(
                    &instruction.collateral_mint,
                    instruction.collateral_decimals,
                ),
                maximum_amount: gofin_domain::Quantity::from_u64(maximum_protocol_fee_atoms),
            });
        }
        if !collateral_only {
            expected.push(EffectConstraint::DerivativePosition {
                resource: resource.clone(),
                position: position.clone(),
                asset: position_asset.clone(),
                position_kind: position_kind.clone(),
                minimum_delta: gofin_domain::SignedQuantity::new(signed_min)?,
                maximum_delta: gofin_domain::SignedQuantity::new(signed_max)?,
            });
        }
        if !increasing && !collateral_only {
            expected.push(EffectConstraint::PositionRealizedPnl {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: position_kind.clone(),
                asset: margin_usd_asset(),
                maximum_loss: gofin_domain::Quantity::from_u64(
                    pre_position
                        .as_ref()
                        .map(|position| position.collateral_usd_atoms)
                        .unwrap_or(0),
                ),
            });
        }
        if increasing && collateral_atoms > 0 {
            expected.push(EffectConstraint::Debit {
                resource: resource.clone(),
                asset: funding_asset.clone(),
                maximum_debit: gofin_domain::Quantity::from_u64(collateral_atoms),
            });
        }
        // Request PDA rent is accounted for by AccountFundingLifecycle
        // above. New Position PDA and wallet-owned ATA rent still settle as
        // ordinary CashDebit effects, each paired with its exact refundable
        // position/origin movement below. Keep this bound limited to those
        // raw wallet-funded accounts so the lifecycle debit is not counted a
        // second time.
        let raw_account_funding_lamports = wallet_account_funding_lamports
            .checked_add(position_rent_funding_cap)
            .ok_or_else(bad_response)?;
        if raw_account_funding_lamports > 0 {
            expected.push(EffectConstraint::Debit {
                resource: resource.clone(),
                asset: asset("native", 9),
                maximum_debit: gofin_domain::Quantity::from_u64(raw_account_funding_lamports),
            });
        }
        expected.push(EffectConstraint::PositionOfKind {
            resource: resource.clone(),
            position: format!("{position}:collateral"),
            asset: AssetId {
                network: NETWORK.into(),
                reference: crate::pricing::MARGIN_USD_REFERENCE.into(),
                decimals: 6,
            },
            position_kind: gofin_domain::PositionKind::LockedCollateral,
            direction: if increasing {
                gofin_domain::Direction::Increase
            } else {
                gofin_domain::Direction::Decrease
            },
            maximum_change: if increasing {
                gofin_domain::Quantity::from_u64(if collateral_only {
                    parse_u64_text(quote, "collateralUsdDelta")?
                } else {
                    valuation
                        .movement_usd_micros
                        .as_str()
                        .parse::<u64>()
                        .map_err(|_| bad_response())?
                })
            } else {
                gofin_domain::Quantity::from_u64(if collateral_only {
                    collateral_atoms
                        .checked_add(maximum_protocol_fee_usd_atoms)
                        .ok_or_else(invalid_account_set)?
                } else {
                    pre_position
                        .as_ref()
                        .map(|p| p.collateral_usd_atoms)
                        .unwrap_or(0)
                })
            },
        });
        if request_account_rent_lamports > 0 {
            expected.push(EffectConstraint::AccountFundingLifecycle {
                resource: resource.clone(),
                asset: asset("native", 9),
                accounts: vec![gofin_domain::RefundableFundingLimit {
                    position: format!("{}:rent", instruction.request),
                    account: instruction.request.clone(),
                    maximum_amount: gofin_domain::Quantity::from_u64(request_account_rent_lamports),
                }],
            });
        }
        for funding in &wallet_funding_accounts {
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: format!("account-funding:{}", funding.address),
                asset: asset("native", 9),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                direction: gofin_domain::Direction::Increase,
                maximum_change: gofin_domain::Quantity::from_u64(funding.lamports),
            });
            expected.push(EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                to: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: format!("account-funding:{}", funding.address),
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                debit_asset: asset("native", 9),
                credit_asset: asset("native", 9),
                maximum_debit: gofin_domain::Quantity::from_u64(funding.lamports),
                minimum_credit: gofin_domain::Quantity::from_u64(funding.lamports),
            });
        }
        for (address, rent_atoms) in &pre_token_rents {
            let rent = gofin_domain::Quantity::new(rent_atoms.clone())?;
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: format!("account-funding:{address}"),
                asset: asset("native", 9),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                direction: gofin_domain::Direction::Decrease,
                maximum_change: rent.clone(),
            });
            expected.push(EffectConstraint::OriginMovement {
                from: AccountingLocation::Position {
                    resource: resource.clone(),
                    position: format!("account-funding:{address}"),
                    position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                },
                to: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                debit_asset: asset("native", 9),
                credit_asset: asset("native", 9),
                maximum_debit: rent.clone(),
                minimum_credit: rent,
            });
        }
        if (increasing && position_rent_funding_cap > 0) || (!increasing && entire_position) {
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: format!("{position}:rent"),
                asset: asset("native", 9),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                direction: if increasing {
                    gofin_domain::Direction::Increase
                } else {
                    gofin_domain::Direction::Decrease
                },
                maximum_change: if increasing {
                    gofin_domain::Quantity::from_u64(position_rent_funding_cap)
                } else {
                    gofin_domain::Quantity::from_u64(
                        pre_position.as_ref().map(|p| p.rent_lamports).unwrap_or(0),
                    )
                },
            });
        }
        let collateral_location = AccountingLocation::Position {
            resource: resource.clone(),
            position: format!("{position}:collateral"),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        };
        expected.push(EffectConstraint::OriginMovement {
            from: if increasing {
                AccountingLocation::Cash {
                    resource: resource.clone(),
                }
            } else {
                collateral_location.clone()
            },
            to: if increasing {
                collateral_location
            } else {
                AccountingLocation::Cash {
                    resource: resource.clone(),
                }
            },
            debit_asset: if increasing {
                funding_asset.clone()
            } else {
                margin_usd_asset()
            },
            credit_asset: if increasing {
                margin_usd_asset()
            } else {
                funding_asset.clone()
            },
            maximum_debit: gofin_domain::Quantity::from_u64(if increasing {
                collateral_atoms
            } else {
                pre_position
                    .as_ref()
                    .map(|p| p.collateral_usd_atoms)
                    .unwrap_or(0)
            }),
            minimum_credit: gofin_domain::Quantity::from_u64(if increasing {
                1
            } else {
                minimum_receive_atoms
            }),
        });
        if (increasing && position_rent_funding_cap > 0) || (!increasing && entire_position) {
            expected.push(EffectConstraint::OriginMovement {
                from: if increasing {
                    AccountingLocation::Cash {
                        resource: resource.clone(),
                    }
                } else {
                    AccountingLocation::Position {
                        resource: resource.clone(),
                        position: format!("{position}:rent"),
                        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                    }
                },
                to: if increasing {
                    AccountingLocation::Position {
                        resource: resource.clone(),
                        position: format!("{position}:rent"),
                        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                    }
                } else {
                    AccountingLocation::Cash {
                        resource: resource.clone(),
                    }
                },
                debit_asset: asset("native", 9),
                credit_asset: asset("native", 9),
                maximum_debit: gofin_domain::Quantity::from_u64(if increasing {
                    position_rent_funding_cap
                } else {
                    pre_position.as_ref().map(|p| p.rent_lamports).unwrap_or(0)
                }),
                minimum_credit: gofin_domain::Quantity::from_u64(if increasing {
                    position_rent_funding_cap
                } else {
                    pre_position.as_ref().map(|p| p.rent_lamports).unwrap_or(0)
                }),
            });
        }
        if increasing {
            // The debit uses the selected funding mint. Margin is tracked
            // separately in the venue's exact USD accounting unit above.
        } else {
            expected.push(EffectConstraint::Credit {
                to: resource.clone(),
                asset: funding_asset.clone(),
                minimum_credit: gofin_domain::Quantity::from_u64(minimum_receive_atoms),
            });
            // Request rent is credited through its cumulative typed
            // AccountFundingProgress. Only a proven Position PDA rent refund
            // is a raw wallet credit; omit the constraint entirely when this
            // operation cannot return any Position rent.
            let position_rent_refund = if entire_position {
                pre_position.as_ref().map(|p| p.rent_lamports).unwrap_or(0)
            } else {
                0
            };
            if position_rent_refund > 0 {
                expected.push(EffectConstraint::Credit {
                    to: resource.clone(),
                    asset: asset("native", 9),
                    minimum_credit: gofin_domain::Quantity::from_u64(position_rent_refund),
                });
            }
        }
        let mut debits = Vec::new();
        if increasing && collateral_atoms > 0 {
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: funding_asset,
                    atoms: gofin_domain::Quantity::from_u64(collateral_atoms),
                },
            });
        }
        if required_account_funding_lamports > 0 {
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset("native", 9),
                    atoms: gofin_domain::Quantity::from_u64(required_account_funding_lamports),
                },
            });
        }
        let created_token_accounts = auxiliary
            .created_token_accounts
            .iter()
            .map(|account| {
                json!({
                    "address": account.address,
                    "mint": account.mint,
                    "token_program": account.token_program
                })
            })
            .collect::<Vec<_>>();
        let binding_core = json!({
            "wallet_address":wallet.address(), "operation":operation, "resource":resource,
            "position":position, "request":instruction.request,
            "market_mint":instruction.market_mint, "collateral_mint":instruction.collateral_mint,
            "funding_mint":instruction.funding_mint, "funding_decimals":instruction.funding_decimals,
            "market_decimals":instruction.market_decimals, "collateral_decimals":instruction.collateral_decimals,
            "pre_size_usd_atoms":pre_position.as_ref().map(|p|p.size_usd_atoms).unwrap_or(0).to_string(),
            "pre_collateral_usd_atoms":pre_position.as_ref().map(|p|p.collateral_usd_atoms).unwrap_or(0).to_string(),
            "pre_rent_lamports":pre_position.as_ref().map(|p|p.rent_lamports).unwrap_or(0).to_string(),
            "pre_amount_atoms":pre_position.as_ref().map(|p|p.amount.atoms.as_str()).unwrap_or("0"),
            "minimum_receive_atoms":minimum_receive_atoms.to_string(),
        });
        let mut binding = json!({
            "minimum_net_collateral_atoms":minimum_net_collateral_atoms.as_ref().map(|value|value.as_str()),
            "minimum_net_receive_atoms":minimum_net_receive_atoms.as_ref().map(|value|value.as_str()),
            "expected_output_atoms":expected_output_atoms.to_string(),
            "maximum_protocol_fee_usd_atoms":maximum_protocol_fee_usd_atoms.to_string(),
            "maximum_protocol_fee_atoms":maximum_protocol_fee_atoms.to_string(),
            "position_and_borrow_fee_ceiling_usd_atoms":total_protocol_fee_ceiling.to_string(),
            "collateral_swap_fee_ceiling_usd_atoms":swap_fee_ceiling.to_string(),
            "side":side, "size_usd_atoms":expected_size.to_string(), "collateral_atoms":collateral_atoms.to_string(),
            "counter":instruction.counter.to_string(), "request_change":if increasing {1} else {2},
            "message_sha256":frozen.message_sha256(), "last_valid_block_height":last_valid_block_height,
            "blockhash":frozen.recent_blockhash(), "transaction_base64":encoded,
            "maximum_network_fee_lamports":maximum_network_fee_lamports.to_string(),
            "maximum_account_funding_lamports":maximum_account_funding_lamports.to_string(),
            "maximum_slippage_bps":max_slippage_bps,
            "request_account_space":POSITION_REQUEST_ACCOUNT_SPACE,
            "request_account_rent_lamports":request_account_rent_lamports.to_string(),
            "position_account_space":if position_account_rent_lamports > 0 {Some(POSITION_ACCOUNT_SPACE)} else {None},
            "position_account_rent_lamports":position_account_rent_lamports.to_string(),
            "required_account_funding_lamports":required_account_funding_lamports.to_string(),
            "account_funding_lamports":auxiliary.account_funding_lamports.to_string(),
            "wallet_account_funding_lamports":wallet_account_funding_lamports.to_string(),
            "wrapped_native_lamports":auxiliary.wrapped_native_lamports.to_string(),
            "closed_wrapped_native_accounts":auxiliary.closed_wrapped_native_accounts,
            "created_token_accounts":created_token_accounts,
            "closed_token_account_rents":pre_token_rents,
            "market":instruction.market, "receive_token":input.get("receive_token"),
        });
        binding
            .as_object_mut()
            .ok_or_else(bad_response)?
            .extend(binding_core.as_object().ok_or_else(bad_response)?.clone());
        Ok(PreparedOperation {
            operation: operation.into(), input: input.clone(), binding,
            summary: if increasing {
                format!("Request a {} increase of {} USD atoms in {} using {} collateral atoms through Jupiter Perps. Settlement remains pending until the keeper's finalized request event is verified.", if side == 1 {"long"} else {"short"}, expected_size, instruction.market, collateral_atoms)
            } else {
                format!("Request a {} decrease of {} USD atoms for Perps position {}. Settlement remains pending until the keeper's finalized request event is verified.", if side == 1 {"long"} else {"short"}, expected_size, position)
            },
            checks: vec![check("Pinned program instruction and request identity"), check("Mainnet-beta pool, custody and PDA"), check("Bounded network fee and funding"), check("Transaction simulation")],
            warnings: vec!["A submitted request is not a completed position change; GoFin waits for the keeper's exact finalized event and position update.".into()],
            observed_at: Utc::now().to_rfc3339(), expires_at:(Utc::now()+ChronoDuration::seconds(20)).to_rfc3339(),
            valuation:Some(valuation), intent:FinancialIntent { debits, maximum_costs, expected, locks:vec![resource.clone(), position_lock_resource(&position)] }, action_deadline:None,operation_version:1,
        })
    }

    async fn prepare_limit_update(&self, input: &Value) -> Result<PreparedOperation> {
        validate_network(input)?;
        let wallet = self.wallet(input)?;
        let signer = wallet.signer().ok_or_else(|| {
            GoFinError::new(
                "SOLANA_SIGNING_UNAVAILABLE",
                "prepare",
                "The selected wallet has no protected signing capability.",
                "Enroll a protected signer before updating a Jupiter Perps limit order.",
            )
        })?;
        let request_address = required_text(input, "request")?.to_owned();
        gofin_solana::parse_address(&request_address)?;
        let parent_operation_id = required_text(input, "original_operation_id")?.to_owned();
        if parent_operation_id.trim().is_empty() {
            return Err(GoFinError::validation(
                "A Perps limit update must identify its original create operation.",
            ));
        }
        let (parent_record, parent_submission) =
            self.limit_create_parent(&parent_operation_id, wallet.address(), &request_address)?;
        if let Some(echo) = input
            .get("original_submission")
            .filter(|value| !value.is_null())
        {
            let echo: Submission = serde_json::from_value(echo.clone()).map_err(|_| {
                GoFinError::validation("The optional original submission echo is malformed.")
            })?;
            if parent_record.submission.as_ref() != Some(&echo) {
                return Err(unreviewed_detail(
                    "the supplied create submission differs from the durable parent record",
                ));
            }
        }
        let requested_trigger = required_u64(input, "trigger_price_usd_atoms")?;
        if requested_trigger == 0 {
            return Err(GoFinError::validation(
                "A Perps limit trigger price must be positive USD atomic units.",
            ));
        }
        let maximum_network_fee_lamports = required_u64(input, "maximum_network_fee_lamports")?;
        if maximum_network_fee_lamports == 0 {
            return Err(GoFinError::validation(
                "A positive network-fee cap is required for a Perps limit update.",
            ));
        }
        let existing = self
            .position_request_by_address(&wallet, &request_address)
            .await?;
        if existing.request_type != 1 || existing.executed {
            return Err(GoFinError::new(
                "JUPITER_PERPS_LIMIT_NOT_OPEN",
                "prepare",
                "The selected request is not an open, unexecuted Perps limit order.",
                "Read current Perps requests and update only an open limit order owned by this wallet.",
            ));
        }
        let response = self
            .api
            .request(
                JupiterHost::Perps,
                Method::PATCH,
                "/orders/limit",
                Some(json!({
                    "positionRequestPubkey":request_address,
                    "triggerPrice":requested_trigger.to_string()
                })),
                RateBucket::Main,
                RequestPriority::Financial,
                DEADLINE,
            )
            .await?;
        let encoded = response
            .get("serializedTxBase64")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(bad_response)?;
        let missing_external = limit_create::foreign_missing_signers(encoded, wallet.address())?;
        let approved_external_cosigners =
            limit_create::foreign_required_signers(encoded, wallet.address())?;
        if response
            .get("positionRequestPubkey")
            .and_then(Value::as_str)
            .is_some_and(|returned| returned != request_address)
        {
            return Err(unreviewed());
        }
        let last_valid_block_height = parse_u64_text(
            response.get("txMetadata").ok_or_else(bad_response)?,
            "lastValidBlockHeight",
        )?;
        let frozen = wallet
            .runtime()
            .freeze_transaction(
                encoded,
                &signer,
                last_valid_block_height,
                maximum_network_fee_lamports,
                None,
                &missing_external,
            )
            .await?;
        inspect_limit_update_instruction(&wallet, &frozen, &existing, requested_trigger)?;
        wallet.runtime().validate_fee(&frozen).await?;
        wallet.runtime().simulate(&frozen).await?;
        let resource = resource_for_wallet(wallet.address());
        let request_lock = ResourceId(format!(
            "solana:{CLUSTER}:jupiter-perps-request:{request_address}"
        ));
        let position_lock = position_lock_resource(&existing.position);
        let network_asset = asset("native", 9);
        let fee_bound = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: network_asset,
                atoms: gofin_domain::Quantity::from_u64(maximum_network_fee_lamports),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![fee_bound],
            expected: vec![],
            locks: vec![resource, request_lock.clone(), position_lock.clone()],
        };
        let (valuation, _) = crate::adapter::valuation(
            self.prices.clone(),
            asset("native", 9),
            0,
            asset("native", 9),
            0,
            maximum_network_fee_lamports,
            wallet.address(),
        )
        .await?;
        let binding = json!({
            "operation":OP_LIMIT_UPDATE,
            "wallet_address":wallet.address(),
            "request":request_address,
            "parent_operation_id":parent_operation_id,
            "parent_submission":parent_submission,
            "original_operation_id":parent_operation_id,
            "original_submission":parent_submission,
            "__gofin_amendment":{
                "version":1,
                "parent_operation_id":parent_operation_id,
                "parent_submission":parent_submission,
                "request_id":request_address,
                "shared_locks":[position_lock,request_lock]
            },
            "position":existing.position,
            "pool":existing.pool,
            "custody":existing.custody,
            "counter":existing.counter.to_string(),
            "side":existing.side,
            "request_change":existing.request_change,
            "size_usd_atoms":existing.size_usd_delta.to_string(),
            "collateral_atoms":existing.collateral_delta.to_string(),
            "trigger_above_threshold":existing.trigger_above,
            "previous_trigger_price_usd_atoms":existing.trigger_price.ok_or_else(unreviewed)?.to_string(),
            "trigger_price_usd_atoms":requested_trigger.to_string(),
            "message_sha256":frozen.message_sha256(),
            "last_valid_block_height":last_valid_block_height,
            "blockhash":frozen.recent_blockhash(),
            "transaction_base64":encoded,
            "maximum_network_fee_lamports":maximum_network_fee_lamports.to_string(),
            "approved_external_cosigners":approved_external_cosigners
        });
        Ok(PreparedOperation {
            operation: OP_LIMIT_UPDATE.into(),
            input: input.clone(),
            binding,
            summary: format!(
                "Update Perps limit request {request_address} trigger to {}.",
                format_usd_micros(requested_trigger)
            ),
            checks: vec![
                check("Open limit request belongs to the selected wallet"),
                check("Pinned instant update instruction and request identity"),
                check("Bounded network fee and transaction simulation"),
            ],
            warnings: vec![
                "The update applies only after its exact transaction is finalized.".into(),
            ],
            observed_at: Utc::now().to_rfc3339(),
            expires_at: (Utc::now() + ChronoDuration::seconds(20)).to_rfc3339(),
            valuation: Some(valuation),
            intent,
            action_deadline: None,
            operation_version: 1,
        })
    }

    async fn position_request_by_address(
        &self,
        wallet: &SolanaWalletAdapter,
        address: &str,
    ) -> Result<VerifiedRequest> {
        let response = wallet
            .runtime()
            .rpc()
            .request(
                "getAccountInfo",
                json!([address,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let account = response
            .get("value")
            .filter(|value| !value.is_null())
            .ok_or_else(invalid_account_set)?;
        if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
            return Err(invalid_account_set());
        }
        let data = gofin_solana::token::decode_rpc_account_data(
            account.get("data").ok_or_else(invalid_account_set)?,
        )?;
        let mut request = decode_position_request(wallet.address(), address, &data)?;
        request.rent_lamports = account
            .get("lamports")
            .and_then(Value::as_u64)
            .filter(|rent| *rent > 0)
            .ok_or_else(invalid_account_set)?;
        Ok(request)
    }

    async fn position_by_address(
        &self,
        wallet: &SolanaWalletAdapter,
        position: &str,
    ) -> Result<VerifiedPosition> {
        let data = account_data(wallet, position).await?;
        if data.get(..8) != Some(account_discriminator(POSITION_ACCOUNT).as_slice()) {
            return Err(invalid_account_set());
        }
        decode_position(wallet, position).await
    }

    async fn existing_position(
        &self,
        wallet: &SolanaWalletAdapter,
        position: &str,
    ) -> Result<Option<VerifiedPosition>> {
        let response = wallet
            .runtime()
            .rpc()
            .request(
                "getAccountInfo",
                json!([position,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let Some(account) = response.get("value").filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
            return Err(invalid_account_set());
        }
        let data = gofin_solana::token::decode_rpc_account_data(
            account.get("data").ok_or_else(invalid_account_set)?,
        )?;
        if data.get(..8) != Some(account_discriminator(POSITION_ACCOUNT).as_slice()) {
            return Ok(None);
        }
        decode_position(wallet, position).await.map(Some)
    }

    // Each proof component is passed separately to prevent substituting mutable
    // account state for the immutable placement and keeper evidence.
    #[allow(clippy::too_many_arguments)]
    async fn reconcile_keeper(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        wallet: &SolanaWalletAdapter,
        request_signature: &str,
        request_slot: u64,
        network_fee: u64,
        request_message_hash: &str,
        request_proof: &FinalizedReceiptProof,
    ) -> Result<Reconciliation> {
        let binding = &record.preview.as_ref().ok_or_else(bad_response)?.binding;
        let request = binding
            .get("request")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let position = binding
            .get("position")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let wallet_address = binding
            .get("wallet_address")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let market_mint = binding
            .get("market_mint")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let collateral_mint = binding
            .get("collateral_mint")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let funding_mint = binding
            .get("funding_mint")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?;
        let increasing = is_increase_operation(&record.operation);
        let collateral_only = is_collateral_only_operation(&record.operation);
        let expected_size = binding
            .get("size_usd_atoms")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(bad_response)?;
        let collateral_atoms = binding
            .get("collateral_atoms")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(bad_response)?;
        let pre_size = binding
            .get("pre_size_usd_atoms")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(bad_response)?;
        let pre_amount = binding
            .get("pre_amount_atoms")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u128>().ok())
            .ok_or_else(bad_response)?;
        let max_funding = binding
            .get("maximum_account_funding_lamports")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(bad_response)?;
        let side = binding
            .get("side")
            .and_then(Value::as_u64)
            .ok_or_else(bad_response)? as u8;
        if request_proof.error.is_some()
            || !request_instruction_matches(
                request_proof,
                request,
                position,
                wallet_address,
                increasing,
            )?
        {
            return Err(receipt_mismatch());
        }
        let request_token_delta = request_proof.token_delta(wallet_address, funding_mint)?;
        let wrapped_native_lamports = binding
            .get("wrapped_native_lamports")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(bad_response)?;
        let closed_wrapped_native_accounts = binding
            .get("closed_wrapped_native_accounts")
            .and_then(Value::as_array)
            .filter(|accounts| accounts.len() <= MAX_TOKEN_RENT_ACCOUNTS)
            .ok_or_else(bad_response)?;
        let native_principal_lamports = if increasing
            && funding_mint == crate::swap::WRAPPED_SOL_MINT
            && wrapped_native_lamports > 0
        {
            if wrapped_native_lamports != collateral_atoms || request_token_delta.as_str() != "0" {
                return Err(receipt_mismatch());
            }
            collateral_atoms
        } else {
            if increasing && request_token_delta.as_str() != format!("-{collateral_atoms}") {
                return Err(receipt_mismatch());
            }
            0
        };
        let _ = closed_wrapped_native_accounts;
        if increasing {
            // The exact request input is either the bound SPL delta or a
            // normalized native-SOL wrap that is closed in the same message.
        } else if request_token_delta.as_str() != "0" {
            return Err(receipt_mismatch());
        }

        let history = wallet
            .runtime()
            .rpc()
            .request(
                "getSignaturesForAddress",
                json!([request,{"commitment":"finalized","limit":100}]),
            )
            .await?;
        let entries = history
            .as_array()
            .filter(|items| items.len() <= 100)
            .ok_or_else(bad_response)?;
        for entry in entries {
            let Some(signature) = entry.get("signature").and_then(Value::as_str) else {
                return Err(bad_response());
            };
            let slot = entry
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or_else(bad_response)?;
            if signature == request_signature || slot < request_slot {
                continue;
            }
            let keeper_proof = match wallet
                .runtime()
                .observe_finalized_transaction(signature)
                .await?
            {
                ReceiptOutcome::Finalized {
                    failed: false,
                    proof,
                    ..
                } => *proof,
                _ => continue,
            };
            if keeper_proof.error.is_some()
                || !keeper_instruction_matches(&keeper_proof, request, position, increasing)?
            {
                continue;
            }
            let events = keeper_events(&keeper_proof.log_messages)?;
            let matches = events
                .iter()
                .filter(|event| {
                    keeper_event_matches(
                        event,
                        request,
                        position,
                        wallet_address,
                        side,
                        increasing,
                        0,
                        expected_size,
                        collateral_atoms,
                    )
                })
                .collect::<Vec<_>>();
            if increasing && funding_mint != collateral_mint {
                let pre_swaps = events
                    .iter()
                    .filter(|event| matches!(event, KeeperEvent::PreSwap { request: observed, transfer_amount, .. } if observed == request && *transfer_amount == collateral_atoms))
                    .count();
                if pre_swaps != 1 {
                    continue;
                }
            }
            if matches.len() != 1 {
                continue;
            }
            let event = matches[0];
            let expected_post_size = if increasing {
                pre_size.checked_add(expected_size)
            } else {
                pre_size.checked_sub(expected_size)
            }
            .ok_or_else(receipt_mismatch)?;
            let event_post_size = match event {
                KeeperEvent::Increase {
                    position_size_usd, ..
                }
                | KeeperEvent::Decrease {
                    position_size_usd, ..
                } => *position_size_usd,
                KeeperEvent::PreSwap { .. } => return Err(receipt_mismatch()),
            };
            if event_post_size != expected_post_size {
                return Err(receipt_mismatch());
            }
            let event_price = match event {
                KeeperEvent::Increase { price, .. } | KeeperEvent::Decrease { price, .. } => *price,
                KeeperEvent::PreSwap { .. } => return Err(receipt_mismatch()),
            };
            if event_price == 0 {
                return Err(receipt_mismatch());
            }
            if fee_breakdown_total(event)? > event_fee_usd(event)? {
                return Err(receipt_mismatch());
            }
            let keeper_message_hash = keeper_proof.message_sha256.clone();
            let post_amount = u128::from(expected_post_size);
            // Engine exposure is denominated in the exact USD 10^-6 notional
            // from the pinned position/event, never a synthetic underlying
            // token quantity inferred from execution price.
            let position_delta = signed_u128_delta(pre_amount, post_amount)?;
            if !collateral_only
                && (increasing && position_delta.starts_with('-')
                    || !increasing && !position_delta.starts_with('-'))
            {
                return Err(receipt_mismatch());
            }
            let position_asset = crate::pricing::position_usd_asset(market_mint)?;
            let position_anchor = transaction_position_anchor(
                resource_for_wallet(wallet_address),
                position.to_owned(),
                if side == 1 {
                    gofin_domain::PositionKind::DerivativeLong
                } else {
                    gofin_domain::PositionKind::DerivativeShort
                },
                Amount {
                    asset: position_asset.clone(),
                    atoms: gofin_domain::Quantity::from_u64(event_post_size),
                },
                signature,
                slot,
                &keeper_message_hash,
                json!({"source":"finalized_jupiter_perps_keeper_event","event":event}),
            );
            let mut effects = Vec::new();
            if !collateral_only {
                effects.push(gofin_domain::FinancialEffect::PositionChange {
                    resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                    position: position.to_owned(),
                    asset: position_asset.clone(),
                    delta: gofin_domain::SignedQuantity::new(position_delta)?,
                    scaled_receipt: None,
                    anchor: Some(position_anchor.clone()),
                });
                if let KeeperEvent::Decrease {
                    pnl_usd,
                    has_profit,
                    ..
                } = event
                {
                    effects.push(gofin_domain::FinancialEffect::PositionRealizedPnl {
                        resource: ResourceId(format!("solana:mainnet-beta:{wallet_address}")),
                        position: position.to_owned(),
                        position_kind: if side == 1 {
                            gofin_domain::PositionKind::DerivativeLong
                        } else {
                            gofin_domain::PositionKind::DerivativeShort
                        },
                        asset: margin_usd_asset(),
                        event_id: signature.to_owned(),
                        delta: gofin_domain::SignedQuantity::new(if *has_profit {
                            pnl_usd.to_string()
                        } else {
                            format!("-{pnl_usd}")
                        })?,
                        anchor: Box::new(position_anchor.clone()),
                    });
                }
            }
            let resource = ResourceId(format!("solana:mainnet-beta:{wallet_address}"));
            if increasing {
                let actual_debit = collateral_atoms;
                let funding_decimals = binding
                    .get("funding_decimals")
                    .and_then(Value::as_u64)
                    .ok_or_else(bad_response)? as u8;
                let debit_asset = asset(
                    if native_principal_lamports > 0 {
                        "native"
                    } else {
                        funding_mint
                    },
                    funding_decimals,
                );
                effects.push(gofin_domain::FinancialEffect::CashDebit {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: debit_asset.clone(),
                        atoms: gofin_domain::Quantity::from_u64(actual_debit),
                    },
                });
                if let KeeperEvent::Increase {
                    fee_token,
                    fee_usd,
                    collateral_delta_usd,
                    collateral_delta_token,
                    ..
                } = event
                {
                    // Keep exact finalized fee evidence even when it exceeds
                    // the approved cap. The typed Cost/IncludedCostConversion
                    // below lets the engine retain the overrun as needs
                    // attention instead of erasing a proved keeper fill.
                    let gross_converted = collateral_delta_usd
                        .checked_add(*fee_usd)
                        .ok_or_else(receipt_mismatch)?;
                    if *collateral_delta_token == 0 && *collateral_delta_usd != 0 {
                        return Err(receipt_mismatch_at(
                            "keeper collateral conversion omitted its token output",
                        ));
                    }
                    effects.push(gofin_domain::FinancialEffect::PositionChange {
                        resource: resource.clone(),
                        position: format!("{position}:collateral"),
                        asset: AssetId {
                            network: NETWORK.into(),
                            reference: crate::pricing::MARGIN_USD_REFERENCE.into(),
                            decimals: 6,
                        },
                        delta: gofin_domain::SignedQuantity::new(collateral_delta_usd.to_string())?,
                        scaled_receipt: None,
                        anchor: Some(transaction_position_anchor(
                            resource.clone(),
                            format!("{position}:collateral"),
                            gofin_domain::PositionKind::LockedCollateral,
                            Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(
                                    pre_collateral_usd_atoms(binding)?
                                        .checked_add(*collateral_delta_usd)
                                        .ok_or_else(receipt_mismatch)?,
                                ),
                            },
                            signature,
                            slot,
                            &keeper_message_hash,
                            json!({"source":"finalized_jupiter_perps_increase_event","event":event}),
                        )),
                    });
                    effects.push(gofin_domain::FinancialEffect::OriginMovement {
                        from: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        to: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("{position}:collateral"),
                            position_kind: gofin_domain::PositionKind::LockedCollateral,
                        },
                        debit: Amount {
                            asset: debit_asset.clone(),
                            atoms: gofin_domain::Quantity::from_u64(actual_debit),
                        },
                        credit: Amount {
                            asset: margin_usd_asset(),
                            atoms: gofin_domain::Quantity::from_u64(*collateral_delta_usd),
                        },
                    });
                    if protocol_fee_is_observed(*fee_usd, *fee_token) {
                        let collateral_location = AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("{position}:collateral"),
                            position_kind: gofin_domain::PositionKind::LockedCollateral,
                        };
                        effects.push(gofin_domain::FinancialEffect::Cost {
                            location: collateral_location.clone(),
                            amount: Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(*fee_usd),
                            },
                            category: "jupiter_perps_protocol_fee".into(),
                            treatment: CostTreatment::IncludedInDebit,
                        });
                        effects.push(gofin_domain::FinancialEffect::CostDenomination {
                            location: collateral_location.clone(),
                            cost_asset: margin_usd_asset(),
                            category: "jupiter_perps_protocol_fee".into(),
                            treatment: CostTreatment::IncludedInDebit,
                            denomination: Amount {
                                asset: asset(
                                    binding["collateral_mint"]
                                        .as_str()
                                        .ok_or_else(bad_response)?,
                                    binding["collateral_decimals"]
                                        .as_u64()
                                        .and_then(|value| u8::try_from(value).ok())
                                        .ok_or_else(bad_response)?,
                                ),
                                atoms: gofin_domain::Quantity::from_u64(*fee_token),
                            },
                            evidence_id: format!("perps-increase:{signature}"),
                        });
                        effects.push(gofin_domain::FinancialEffect::IncludedCostConversion {
                            location: collateral_location,
                            source: gofin_domain::IncludedCostSource::Cash {
                                resource: resource.clone(),
                            },
                            gross_debit: Amount {
                                asset: debit_asset,
                                atoms: gofin_domain::Quantity::from_u64(actual_debit),
                            },
                            gross_converted: Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(gross_converted),
                            },
                            net_position: Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(*collateral_delta_usd),
                            },
                            included_cost: Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(*fee_usd),
                            },
                            evidence_id: format!("perps-increase:{signature}"),
                        });
                    }
                }
            } else {
                let receive_delta = keeper_proof.token_delta(wallet_address, funding_mint)?;
                let credit = receive_delta
                    .as_str()
                    .parse::<u64>()
                    .map_err(|_| receipt_mismatch())?;
                let transfer = match event {
                    KeeperEvent::Decrease {
                        transfer_token: Some(value),
                        ..
                    } => *value,
                    _ => return Err(receipt_mismatch_at("keeper payout token amount is absent")),
                };
                if credit != transfer {
                    return Err(receipt_mismatch_at(
                        "finalized wallet payout differs from keeper event",
                    ));
                }
                effects.push(gofin_domain::FinancialEffect::UsableCredit {
                    to: resource.clone(),
                    amount: Amount {
                        asset: asset(
                            funding_mint,
                            binding
                                .get("funding_decimals")
                                .and_then(Value::as_u64)
                                .ok_or_else(bad_response)? as u8,
                        ),
                        atoms: gofin_domain::Quantity::from_u64(credit),
                    },
                });
                if let KeeperEvent::Decrease {
                    fee_usd,
                    original_position_collateral_usd,
                    position_collateral_usd,
                    ..
                } = event
                {
                    // Fee caps are enforced against the typed finalized Cost
                    // after this proof is assembled. Returning a receipt
                    // mismatch here would discard the exact event and payout
                    // evidence for an already-finalized transaction.
                    let pre_collateral = binding
                        .get("pre_collateral_usd_atoms")
                        .and_then(Value::as_str)
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or_else(bad_response)?;
                    if *original_position_collateral_usd != pre_collateral {
                        return Err(receipt_mismatch_at(
                            "keeper pre-collateral differs from approved position baseline",
                        ));
                    }
                    effects.push(gofin_domain::FinancialEffect::PositionChange {
                        resource: resource.clone(),
                        position: format!("{position}:collateral"),
                        asset: AssetId {
                            network: NETWORK.into(),
                            reference: crate::pricing::MARGIN_USD_REFERENCE.into(),
                            decimals: 6,
                        },
                        delta: gofin_domain::SignedQuantity::new(
                            if position_collateral_usd <= original_position_collateral_usd {
                                format!(
                                    "-{}",
                                    original_position_collateral_usd - position_collateral_usd
                                )
                            } else {
                                return Err(receipt_mismatch_at("keeper collateral increased during a decrease request"));
                            },
                        )?,
                        scaled_receipt: None,
                        anchor: Some(transaction_position_anchor(
                            resource.clone(),
                            format!("{position}:collateral"),
                            gofin_domain::PositionKind::LockedCollateral,
                            Amount {
                                asset: margin_usd_asset(),
                                atoms: gofin_domain::Quantity::from_u64(*position_collateral_usd),
                            },
                            signature,
                            slot,
                            &keeper_message_hash,
                            json!({"source":"finalized_jupiter_perps_decrease_event","event":event}),
                        )),
                    });
                    let collateral_debit = original_position_collateral_usd
                        .checked_sub(*position_collateral_usd)
                        .ok_or_else(receipt_mismatch)?;
                    effects.push(gofin_domain::FinancialEffect::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("{position}:collateral"),
                            position_kind: gofin_domain::PositionKind::LockedCollateral,
                        },
                        to: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        debit: Amount {
                            asset: margin_usd_asset(),
                            atoms: gofin_domain::Quantity::from_u64(collateral_debit),
                        },
                        credit: Amount {
                            asset: asset(
                                funding_mint,
                                binding
                                    .get("funding_decimals")
                                    .and_then(Value::as_u64)
                                    .ok_or_else(bad_response)?
                                    as u8,
                            ),
                            atoms: gofin_domain::Quantity::from_u64(credit),
                        },
                    });
                    if *fee_usd > 0 {
                        effects.push(protocol_fee_effect(&resource, position, *fee_usd));
                    }
                }
            }
            if network_fee > 0 {
                effects.push(gofin_domain::FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(network_fee),
                    },
                    category: "solana_network_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            let wallet_delta = request_proof
                .lamport_delta(wallet_address)?
                .as_str()
                .parse::<i128>()
                .map_err(|_| bad_response())?;
            let keeper_wallet_delta = keeper_proof
                .lamport_delta(wallet_address)?
                .as_str()
                .parse::<i128>()
                .map_err(|_| bad_response())?;
            let combined_wallet_delta = wallet_delta
                .checked_add(keeper_wallet_delta)
                .ok_or_else(bad_response)?;
            let keeper_fee = if keeper_proof
                .account_keys
                .first()
                .is_some_and(|payer| payer == wallet_address)
            {
                keeper_proof.fee_lamports
            } else {
                0
            };
            let total_network_fee = i128::from(network_fee)
                .checked_add(i128::from(keeper_fee))
                .ok_or_else(bad_response)?;
            let net_rent = total_network_fee
                .checked_neg()
                .and_then(|fee| fee.checked_sub(combined_wallet_delta))
                .and_then(|rent| rent.checked_sub(i128::from(native_principal_lamports)))
                .ok_or_else(bad_response)?;
            let pre_rent = binding
                .get("pre_rent_lamports")
                .and_then(Value::as_str)
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(bad_response)?;
            let request_rent = binding
                .get("request_account_rent_lamports")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|rent| *rent > 0)
                .ok_or_else(bad_response)?;
            let position_rent_funded = binding
                .get("position_account_rent_lamports")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(bad_response)?;
            let request_pre_rent = proof_lamports(request_proof, request, true)?;
            let request_post_rent = proof_lamports(request_proof, request, false)?;
            let keeper_request_pre_rent = proof_lamports(&keeper_proof, request, true)?;
            let keeper_request_post_rent = proof_lamports(&keeper_proof, request, false)?;
            let request_position_pre_rent = proof_lamports(request_proof, position, true)?;
            let request_position_post_rent = proof_lamports(request_proof, position, false)?;
            let keeper_position_pre_rent = proof_lamports(&keeper_proof, position, true)?;
            let keeper_position_post_rent = proof_lamports(&keeper_proof, position, false)?;
            let expected_position_post_request = pre_rent
                .checked_add(position_rent_funded)
                .ok_or_else(receipt_mismatch)?;
            if request_pre_rent != 0
                || request_post_rent != request_rent
                || keeper_request_pre_rent != request_rent
                || keeper_request_post_rent != 0
                || request_position_pre_rent != pre_rent
                || request_position_post_rent != expected_position_post_request
                || keeper_position_pre_rent != request_position_post_rent
            {
                return Err(receipt_mismatch());
            }
            effects.push(gofin_domain::FinancialEffect::AccountFundingProgress {
                resource: resource.clone(),
                position: format!("{request}:rent"),
                account: request.to_owned(),
                asset: asset("native", 9),
                funded: gofin_domain::Quantity::from_u64(request_rent),
                returned: gofin_domain::Quantity::from_u64(request_rent),
                remaining: gofin_domain::Quantity::zero(),
                state: gofin_domain::AccountFundingState::Closed,
                anchor: Some(transaction_position_anchor(
                    resource.clone(),
                    format!("{request}:rent"),
                    gofin_domain::PositionKind::RefundableAccountFunding,
                    Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::zero(),
                    },
                    signature,
                    slot,
                    &keeper_message_hash,
                    json!({
                        "source":"finalized_jupiter_perps_request_rent_refund",
                        "request":request,
                        "funded_lamports":request_rent,
                        "returned_lamports":request_rent,
                        "placement_signature":request_signature,
                        "placement_slot":request_slot,
                        "placement_message_sha256":request_message_hash,
                        "placement_proof":request_proof
                    }),
                )),
            });
            // The user request may create the Position PDA before the keeper
            // runs. Measure the durable account delta across both finalized
            // transactions; the PositionRequest PDA is temporary and must be
            // closed with its exact rent returned to the original owner.
            let position_rent_delta =
                i128::from(keeper_position_post_rent) - i128::from(request_position_pre_rent);
            let created_accounts = binding
                .get("created_token_accounts")
                .and_then(Value::as_array)
                .ok_or_else(bad_response)?;
            if created_accounts.len() > MAX_TOKEN_RENT_ACCOUNTS {
                return Err(bad_response());
            }
            let current_rent_states = token_account_rent_states(wallet).await?;
            let mut account_rent_changes = Vec::with_capacity(created_accounts.len());
            for created in created_accounts {
                let address = created
                    .get("address")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                let mint = created
                    .get("mint")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                let token_program = created
                    .get("token_program")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?;
                if gofin_solana::associated_token_address(wallet.address(), mint, token_program)?
                    != address
                {
                    return Err(receipt_mismatch());
                }
                let rent = current_rent_states
                    .iter()
                    .find(|state| state.position == format!("account-funding:{address}"));
                let rent_atoms = rent
                    .map(|state| state.amount.atoms.as_str().parse::<u64>())
                    .transpose()
                    .map_err(|_| receipt_mismatch())?
                    .unwrap_or(0);
                account_rent_changes.push((address.to_owned(), i128::from(rent_atoms)));
            }
            let closed_rents = binding
                .get("closed_token_account_rents")
                .and_then(Value::as_object)
                .ok_or_else(bad_response)?;
            for (address, rent_value) in closed_rents {
                if gofin_solana::associated_token_address(
                    wallet.address(),
                    crate::swap::WRAPPED_SOL_MINT,
                    gofin_solana::token::TOKEN_PROGRAM_ID,
                )? != *address
                {
                    return Err(receipt_mismatch());
                }
                let rent = rent_value
                    .as_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|rent| *rent > 0)
                    .ok_or_else(bad_response)?;
                if current_rent_states
                    .iter()
                    .any(|state| state.position == format!("account-funding:{address}"))
                {
                    return Err(receipt_mismatch());
                }
                if account_rent_changes
                    .iter()
                    .any(|(created, _)| created == address)
                {
                    return Err(receipt_mismatch());
                }
                account_rent_changes.push((address.clone(), -i128::from(rent)));
            }
            let account_rent_delta = account_rent_changes
                .iter()
                .try_fold(0i128, |sum, (_, rent)| sum.checked_add(*rent))
                .ok_or_else(bad_response)?;
            let rent_delta = position_rent_delta
                .checked_add(account_rent_delta)
                .ok_or_else(bad_response)?;
            if net_rent != rent_delta
                || (increasing && rent_delta > i128::from(max_funding))
                || (increasing && rent_delta < 0)
                || (!increasing && rent_delta > 0)
            {
                return Err(receipt_mismatch_at(
                    "finalized lamport and account-rent deltas do not reconcile",
                ));
            }
            if position_rent_delta != 0 {
                let rent_atoms = u64::try_from(position_rent_delta.unsigned_abs())
                    .map_err(|_| receipt_mismatch())?;
                effects.push(gofin_domain::FinancialEffect::PositionChange {
                    resource: resource.clone(),
                    position: format!("{position}:rent"),
                    asset: asset("native", 9),
                    delta: gofin_domain::SignedQuantity::new(position_rent_delta.to_string())?,
                    scaled_receipt: None,
                    anchor: Some(transaction_position_anchor(
                        resource.clone(),
                        format!("{position}:rent"),
                        gofin_domain::PositionKind::RefundableAccountFunding,
                        Amount {
                            asset: asset("native", 9),
                            atoms: gofin_domain::Quantity::from_u64(keeper_position_post_rent),
                        },
                        signature,
                        slot,
                        &keeper_message_hash,
                        json!({"source":"finalized_jupiter_perps_position_account_rent","request":request}),
                    )),
                });
                let rent_amount = Amount {
                    asset: asset("native", 9),
                    atoms: gofin_domain::Quantity::from_u64(rent_atoms),
                };
                if position_rent_delta > 0 {
                    effects.push(gofin_domain::FinancialEffect::CashDebit {
                        resource: resource.clone(),
                        amount: rent_amount.clone(),
                    });
                    effects.push(gofin_domain::FinancialEffect::OriginMovement {
                        from: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        to: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("{position}:rent"),
                            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        },
                        debit: rent_amount.clone(),
                        credit: rent_amount,
                    });
                } else {
                    effects.push(gofin_domain::FinancialEffect::UsableCredit {
                        to: resource.clone(),
                        amount: rent_amount.clone(),
                    });
                    effects.push(gofin_domain::FinancialEffect::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: resource.clone(),
                            position: format!("{position}:rent"),
                            position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        },
                        to: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        debit: rent_amount.clone(),
                        credit: rent_amount,
                    });
                }
            }
            for (address, rent) in account_rent_changes {
                if rent != 0 {
                    let post_rent = current_rent_states
                        .iter()
                        .find(|state| state.position == format!("account-funding:{address}"))
                        .map(|state| state.amount.atoms.clone())
                        .unwrap_or_else(gofin_domain::Quantity::zero);
                    let rent_amount =
                        u64::try_from(rent.unsigned_abs()).map_err(|_| receipt_mismatch())?;
                    let amount = Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(rent_amount),
                    };
                    effects.push(gofin_domain::FinancialEffect::PositionChange {
                        resource: resource.clone(),
                        position: format!("account-funding:{address}"),
                        asset: asset("native", 9),
                        delta: gofin_domain::SignedQuantity::new(rent.to_string())?,
                        scaled_receipt: None,
                        anchor: Some(transaction_position_anchor(
                            resource.clone(),
                            format!("account-funding:{address}"),
                            gofin_domain::PositionKind::RefundableAccountFunding,
                            Amount {
                                asset: asset("native", 9),
                                atoms: post_rent,
                            },
                            request_signature,
                            request_slot,
                            request_message_hash,
                            json!({"source":"finalized_jupiter_perps_token_account_rent","request":request,"address":address}),
                        )),
                    });
                    if rent > 0 {
                        effects.push(gofin_domain::FinancialEffect::CashDebit {
                            resource: resource.clone(),
                            amount: amount.clone(),
                        });
                        effects.push(gofin_domain::FinancialEffect::OriginMovement {
                            from: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            to: AccountingLocation::Position {
                                resource: resource.clone(),
                                position: format!("account-funding:{address}"),
                                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                            },
                            debit: amount.clone(),
                            credit: amount,
                        });
                    } else {
                        effects.push(gofin_domain::FinancialEffect::UsableCredit {
                            to: resource.clone(),
                            amount: amount.clone(),
                        });
                        effects.push(gofin_domain::FinancialEffect::OriginMovement {
                            from: AccountingLocation::Position {
                                resource: resource.clone(),
                                position: format!("account-funding:{address}"),
                                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                            },
                            to: AccountingLocation::Cash {
                                resource: resource.clone(),
                            },
                            debit: amount.clone(),
                            credit: amount,
                        });
                    }
                }
            }
            aggregate_native_credits(&mut effects, &resource)?;
            let mut verified = gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"request_signature":request_signature,"request_message_sha256":request_message_hash,"request_slot":request_slot,"request_proof":request_proof,"keeper_signature":signature,"keeper_slot":slot,"keeper_proof":keeper_proof,"keeper_event":event,"realized_pnl_usd_atoms":match event {KeeperEvent::Decrease{pnl_usd,has_profit,..}=>Some(if *has_profit {i128::from(*pnl_usd)} else {-i128::from(*pnl_usd)}),_=>None},"protocol_fee_usd_atoms":match event {KeeperEvent::Increase{fee_usd,..}|KeeperEvent::Decrease{fee_usd,..}=>*fee_usd,KeeperEvent::PreSwap{..}=>0},"position_size_usd_atoms":event_post_size,"event_price_usd_atoms":event_price}),
                observed_at: Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            return Ok(Reconciliation {
                status: Status::Completed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            });
        }
        Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"request":request,"position":position,"request_signature":request_signature,"request_slot":request_slot,"keeper_signatures_checked":entries.len(),"reason":"The request is finalized; no matching finalized keeper instruction and event has changed the exact position yet."}),
            verified: None,
        })
    }

    async fn positions(
        &self,
        wallet: &SolanaWalletAdapter,
    ) -> Result<(Vec<VerifiedPosition>, Vec<VerifiedRequest>)> {
        let position_disc = bs58::encode(account_discriminator(POSITION_ACCOUNT)).into_string();
        let response = wallet
            .runtime()
            .rpc()
            .request(
                "getProgramAccounts",
                json!([
                    crate::jlp::PERPS_PROGRAM_ID,
                    {
                        "encoding":"base64",
                        "commitment":"finalized",
                        "filters":[{"memcmp":{"offset":0,"bytes":position_disc}},{"memcmp":{"offset":8,"bytes":wallet.address()}}]
                    }
                ]),
            )
            .await?;
        let accounts = response
            .as_array()
            .ok_or_else(|| invalid_snapshot_account_set("position account scan response"))?;
        if accounts.len() > MAX_POSITIONS {
            return Err(GoFinError::new(
                "JUPITER_PERPS_POSITION_BOUND",
                "snapshot",
                "The Perps position account set exceeds GoFin's bounded observer limit.",
                "Reduce the wallet's active position count or use a reviewed paginated observer.",
            ));
        }
        let mut positions = Vec::with_capacity(accounts.len());
        for account in accounts {
            let address = account
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_snapshot_account_set("position account address"))?;
            gofin_solana::parse_address(address)
                .map_err(|_| invalid_snapshot_account_set("position account address"))?;
            let info = account
                .get("account")
                .ok_or_else(|| invalid_snapshot_account_set("position account data"))?;
            if info.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
                return Err(invalid_snapshot_account_set("position account owner"));
            }
            let data = gofin_solana::token::decode_rpc_account_data(
                info.get("data")
                    .ok_or_else(|| invalid_snapshot_account_set("position account data"))?,
            )
            .map_err(|_| invalid_snapshot_account_set("position account data"))?;
            if data.get(..8) != Some(account_discriminator(POSITION_ACCOUNT).as_slice()) {
                return Err(invalid_snapshot_account_set(
                    "position account discriminator",
                ));
            }
            positions.push(decode_position(wallet, address).await?);
        }
        let request_disc =
            bs58::encode(account_discriminator(POSITION_REQUEST_ACCOUNT)).into_string();
        let request_response = wallet.runtime().rpc().request(
            "getProgramAccounts",
            json!([crate::jlp::PERPS_PROGRAM_ID,{"encoding":"base64","commitment":"finalized","filters":[{"memcmp":{"offset":0,"bytes":request_disc}},{"memcmp":{"offset":8,"bytes":wallet.address()}}]}]),
        ).await?;
        let request_accounts = request_response.as_array().ok_or_else(|| {
            invalid_snapshot_account_set("position request account scan response")
        })?;
        if request_accounts.len() > MAX_POSITIONS {
            return Err(invalid_account_set());
        }
        let mut pending_requests = Vec::with_capacity(request_accounts.len());
        for account in request_accounts {
            let address = account
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_snapshot_account_set("position request address"))?;
            gofin_solana::parse_address(address)
                .map_err(|_| invalid_snapshot_account_set("position request address"))?;
            let info = account
                .get("account")
                .ok_or_else(|| invalid_snapshot_account_set("position request account data"))?;
            if info.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
                return Err(invalid_snapshot_account_set(
                    "position request account owner",
                ));
            }
            let data = gofin_solana::token::decode_rpc_account_data(
                info.get("data")
                    .ok_or_else(|| invalid_snapshot_account_set("position request account data"))?,
            )
            .map_err(|_| invalid_snapshot_account_set("position request account data"))?;
            let mut request = decode_position_request(wallet.address(), address, &data)
                .map_err(|_| invalid_snapshot_account_set("position request account layout"))?;
            request.rent_lamports = info
                .get("lamports")
                .and_then(Value::as_u64)
                .filter(|rent| *rent > 0)
                .ok_or_else(|| invalid_snapshot_account_set("position request rent"))?;
            pending_requests.push(request);
        }
        Ok((positions, pending_requests))
    }
}

async fn resting_limit_positions(
    wallet: &SolanaWalletAdapter,
    request: &VerifiedRequest,
) -> Result<(
    gofin_domain::PositionState,
    gofin_domain::PositionState,
    gofin_domain::PositionState,
)> {
    if request.request_type != 1 || request.executed || request.rent_lamports == 0 {
        return Err(invalid_account_set());
    }
    let token_program = crate::adapter::token_program_for(&wallet.runtime(), &request.mint).await?;
    let decimals = crate::adapter::token_decimals(&wallet.runtime(), &request.mint).await?;
    let token_account =
        gofin_solana::associated_token_address(&request.address, &request.mint, &token_program)?;
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getMultipleAccounts",
            json!([[request.address, token_account], {"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let slot = response
        .get("context")
        .and_then(|context| context.get("slot"))
        .and_then(Value::as_u64)
        .ok_or_else(invalid_account_set)?;
    let values = response
        .get("value")
        .and_then(Value::as_array)
        .filter(|values| values.len() == 2)
        .ok_or_else(invalid_account_set)?;
    let request_info = values[0].as_object().ok_or_else(invalid_account_set)?;
    if request_info.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID)
        || request_info.get("lamports").and_then(Value::as_u64) != Some(request.rent_lamports)
    {
        return Err(invalid_account_set());
    }
    let request_data = gofin_solana::token::decode_rpc_account_data(
        request_info.get("data").ok_or_else(invalid_account_set)?,
    )?;
    let mut finalized_request =
        decode_position_request(wallet.address(), &request.address, &request_data)?;
    finalized_request.rent_lamports = request.rent_lamports;
    if finalized_request.read_value() != request.read_value()
        || finalized_request.request_type != 1
        || finalized_request.executed
    {
        return Err(invalid_account_set());
    }
    let token_info = values[1].as_object().ok_or_else(invalid_account_set)?;
    if token_info.get("owner").and_then(Value::as_str) != Some(token_program.as_str()) {
        return Err(invalid_account_set());
    }
    let token_data = gofin_solana::token::decode_rpc_account_data(
        token_info.get("data").ok_or_else(invalid_account_set)?,
    )?;
    let token =
        gofin_solana::token::parse_token_account(&token_program, &token_account, &token_data)?;
    if token.token_owner != request.address
        || token.mint != request.mint
        || token.state != "initialized"
        || token.delegate.is_some() && token.delegated_amount_atoms > 0
        || token.amount_atoms != request.collateral_delta
    {
        return Err(invalid_account_set());
    }
    let token_lamports = token_info
        .get("lamports")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_account_set)?;
    let token_rent = if token.is_native {
        let reserve = token
            .rent_exempt_reserve_lamports
            .ok_or_else(invalid_account_set)?;
        if reserve.checked_add(token.amount_atoms) != Some(token_lamports) {
            return Err(invalid_account_set());
        }
        reserve
    } else {
        token_lamports
    };
    if token_rent == 0 {
        return Err(invalid_account_set());
    }
    let resource = resource_for_wallet(wallet.address());
    let escrow_position = format!("jupiter-perps-request:{}", request.address);
    let funding_asset = asset(&request.mint, decimals);
    let amount = Amount {
        asset: funding_asset,
        atoms: gofin_domain::Quantity::from_u64(token.amount_atoms),
    };
    let observed_at = Utc::now().to_rfc3339();
    let escrow_evidence = json!({
        "source":"finalized_jupiter_perps_resting_limit_escrow",
        "request":request.read_value(),
        "request_account":request.address,
        "request_account_owner":crate::jlp::PERPS_PROGRAM_ID,
        "request_rent_lamports":request.rent_lamports.to_string(),
        "request_token_account":token_account,
        "token_program":token_program,
        "token_owner":token.token_owner,
        "mint":token.mint,
        "amount_atoms":token.amount_atoms.to_string(),
        "slot":slot
    });
    let escrow_anchor = gofin_domain::PositionAnchor {
        resource: resource.clone(),
        position: escrow_position.clone(),
        position_kind: gofin_domain::PositionKind::Escrow,
        principal: amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: escrow_evidence,
        observed_at: observed_at.clone(),
    };
    let rent_position = format!("account-funding:{}", request.address);
    let rent_amount = Amount {
        asset: asset("native", 9),
        atoms: gofin_domain::Quantity::from_u64(request.rent_lamports),
    };
    let rent_anchor = gofin_domain::PositionAnchor {
        resource: resource.clone(),
        position: rent_position.clone(),
        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
        principal: rent_amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({
            "source":"finalized_jupiter_perps_request_account_rent",
            "request":request.address,
            "request_account_owner":crate::jlp::PERPS_PROGRAM_ID,
            "rent_lamports":request.rent_lamports.to_string(),
            "slot":slot
        }),
        observed_at,
    };
    let token_rent_position = format!("account-funding:{token_account}");
    let token_rent_amount = Amount {
        asset: asset("native", 9),
        atoms: gofin_domain::Quantity::from_u64(token_rent),
    };
    let token_rent_anchor = gofin_domain::PositionAnchor {
        resource: resource.clone(),
        position: token_rent_position.clone(),
        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
        principal: token_rent_amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({
            "source":"finalized_jupiter_perps_request_token_account_rent",
            "request":request.address,
            "account":token_account,
            "token_program":token_program,
            "token_owner":request.address,
            "mint":request.mint,
            "lamports":token_lamports.to_string(),
            "rent_lamports":token_rent.to_string(),
            "native_amount_atoms":if token.is_native { token.amount_atoms.to_string() } else { "0".to_owned() },
            "slot":slot
        }),
        observed_at: Utc::now().to_rfc3339(),
    };
    Ok((
        gofin_domain::PositionState {
            resource: resource.clone(),
            position: escrow_position,
            kind: gofin_domain::PositionKind::Escrow,
            amount,
            scaled_receipt: None,
            anchor: Some(escrow_anchor),
        },
        gofin_domain::PositionState {
            resource,
            position: rent_position,
            kind: gofin_domain::PositionKind::RefundableAccountFunding,
            amount: rent_amount,
            scaled_receipt: None,
            anchor: Some(rent_anchor),
        },
        gofin_domain::PositionState {
            resource: resource_for_wallet(wallet.address()),
            position: token_rent_position,
            kind: gofin_domain::PositionKind::RefundableAccountFunding,
            amount: token_rent_amount,
            scaled_receipt: None,
            anchor: Some(token_rent_anchor),
        },
    ))
}

async fn token_account_rent_states(
    wallet: &SolanaWalletAdapter,
) -> Result<Vec<gofin_domain::PositionState>> {
    let mut states = Vec::new();
    for token_program in [
        gofin_solana::token::TOKEN_PROGRAM_ID,
        gofin_solana::token::TOKEN_2022_PROGRAM_ID,
    ] {
        let response = wallet.runtime().rpc().request(
            "getTokenAccountsByOwner",
            json!([wallet.address(), {"programId":token_program}, {"encoding":"base64","commitment":"finalized"}]),
        ).await?;
        let accounts = response
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(invalid_account_set)?;
        if states.len().saturating_add(accounts.len()) > MAX_TOKEN_RENT_ACCOUNTS {
            return Err(GoFinError::new(
                "JUPITER_PERPS_TOKEN_ACCOUNT_BOUND",
                "snapshot",
                "The wallet token-account rent scope exceeds GoFin's bounded observer limit.",
                "Reduce the wallet's token-account count or use a reviewed paginated rent observer.",
            ));
        }
        for entry in accounts {
            let address = entry
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(invalid_account_set)?;
            gofin_solana::parse_address(address)?;
            let info = entry.get("account").ok_or_else(invalid_account_set)?;
            if info.get("owner").and_then(Value::as_str) != Some(token_program) {
                return Err(invalid_account_set());
            }
            let bytes = gofin_solana::token::decode_rpc_account_data(
                info.get("data").ok_or_else(invalid_account_set)?,
            )?;
            let token = gofin_solana::token::parse_token_account(token_program, address, &bytes)?;
            if token.token_owner != wallet.address() {
                return Err(invalid_account_set());
            }
            let rent = token
                .rent_exempt_reserve_lamports
                .or_else(|| info.get("lamports").and_then(Value::as_u64))
                .ok_or_else(invalid_account_set)?;
            if rent > 0 {
                states.push(gofin_domain::PositionState {
                    resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                    position: format!("account-funding:{address}"),
                    kind: gofin_domain::PositionKind::RefundableAccountFunding,
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(rent),
                    },
                    scaled_receipt: None,
                    anchor: Some(gofin_domain::PositionAnchor {
                        resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                        position: format!("account-funding:{address}"),
                        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        principal: Amount {
                            asset: asset("native", 9),
                            atoms: gofin_domain::Quantity::from_u64(rent),
                        },
                        scaled_receipt: None,
                        evidence_version: 1,
                        evidence: json!({"source":"finalized_token_account_rent","address":address,"token_program":token_program}),
                        observed_at: Utc::now().to_rfc3339(),
                    }),
                });
            }
        }
    }
    Ok(states)
}

#[async_trait]
impl Adapter for PerpsAdapter {
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        *self
            .execution_state
            .0
            .lock()
            .map_err(|_| GoFinError::validation("Jupiter Perps execution-state lock failed."))? =
            Some(state);
        Ok(())
    }

    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets
            .iter()
            .map(|(name, wallet)| ConnectionMetadata {
                name: name.clone(),
                provider: "jupiter".into(),
                resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                role: Some("perps".into()),
                parent: None,
                network: Some(NETWORK.into()),
                address: Some(wallet.address().into()),
                credential_ref: None,
                signing_enabled: wallet.signer().is_some(),
                details: json!({"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"observer":"pinned-idl-position-accounts"}),
            })
            .collect()
    }

    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        self.connections()
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.snapshot_scoped(&SnapshotScope::default()).await
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        let mut state = FinancialState::default();
        let mut coverage = Vec::new();
        for connection in self
            .connections()
            .into_iter()
            .filter(|c| scope.matches_connection(c))
        {
            let wallet = self
                .wallets
                .get(&connection.name)
                .ok_or_else(invalid_account_set)?;
            let (positions, pending_requests) = self.positions(wallet).await?;
            for position in positions {
                if !scope.matches_asset(&position.market_asset()) {
                    continue;
                }
                state
                    .positions
                    .push(position.state(connection.resource.clone()));
                if position.collateral_usd_atoms > 0 {
                    state
                        .positions
                        .push(position.collateral_state(connection.resource.clone())?);
                }
                if position.rent_lamports > 0 {
                    state
                        .positions
                        .push(position.rent_state(connection.resource.clone()));
                }
            }
            for request in pending_requests {
                if request.is_resting_limit() {
                    let (escrow, rent, token_rent) =
                        resting_limit_positions(wallet, &request).await?;
                    if scope.matches_asset(&escrow.amount.asset) {
                        state.positions.push(escrow);
                    }
                    if scope.matches_asset(&rent.amount.asset) {
                        state.positions.push(rent);
                    }
                    if scope.matches_asset(&token_rent.amount.asset) {
                        state.positions.push(token_rent);
                    }
                } else {
                    state.in_flight.push(request.address);
                }
            }
            coverage.push(connection.resource.0.clone());
            coverage.push(format!(
                "Verified finalized Position and PositionRequest accounts for {}; collateral is reported as a separate USD field and is not converted to token quantity.",
                connection.resource.0
            ));
        }
        let pending = state.in_flight.clone();
        // The account scans are a bounded, exact program-owned scope. Active
        // positions are complete observations; only unresolved keeper requests
        // make this snapshot incomplete.
        let complete = pending.is_empty();
        Ok(StateSnapshot {
            state,
            observed_at: Utc::now().to_rfc3339(),
            complete,
            coverage,
            pending,
        })
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        validate_network(input)?;
        match operation {
            OP_POSITIONS => {
                let wallet = self.wallet(input)?;
                let (positions, pending_requests) = self.positions(&wallet).await?;
                let mut request_rows = Vec::with_capacity(pending_requests.len());
                for request in pending_requests {
                    let mut row = request.read_value();
                    if request.is_resting_limit() {
                        let (escrow, _, _) = resting_limit_positions(&wallet, &request).await?;
                        let anchor = escrow.anchor.as_ref().ok_or_else(invalid_account_set)?;
                        row["escrow"] = json!({
                            "position":escrow.position,
                            "kind":"escrow",
                            "asset":escrow.amount.asset,
                            "amount_atoms":escrow.amount.atoms.as_str(),
                            "evidence":anchor.evidence
                        });
                    }
                    request_rows.push(row);
                }
                Ok(json!({
                    "wallet":wallet.address(),
                    "cluster":CLUSTER,
                    "positions":positions.into_iter().map(|p|p.read_value()).collect::<Vec<_>>(),
                    "pending_requests":request_rows
                }))
            }
            OP_MARKET_STATS => {
                let market = required_market_symbol(input, "market")?;
                let mint = token_mint_for_symbol(market)?;
                let response = self
                    .api
                    .request(
                        JupiterHost::Perps,
                        Method::GET,
                        &format!("/market-stats?mint={mint}"),
                        None,
                        RateBucket::Main,
                        RequestPriority::Normal,
                        DEADLINE,
                    )
                    .await?;
                normalize_market_stats(market, mint, response)
            }
            OP_COLLATERAL_LIMITS => {
                let position = required_text(input, "position")?;
                gofin_solana::parse_address(position)?;
                let input_token = match required_text(input, "input_token")? {
                    "SOL" => "SOL",
                    "ETH" => "ETH",
                    "BTC" => "BTC",
                    "USDC" => "USDC",
                    _ => {
                        return Err(GoFinError::validation(
                            "Choose a supported Perps input token.",
                        ));
                    }
                };
                let input_mint = token_mint_for_symbol(input_token)?;
                let response = self
                    .api
                    .request(
                        JupiterHost::Perps,
                        Method::GET,
                        &format!("/positions/collateral-limits?inputMint={input_mint}&positionPubkey={position}"),
                        None,
                        RateBucket::Main,
                        RequestPriority::Normal,
                        DEADLINE,
                    )
                    .await?;
                normalize_collateral_limits(position, input_token, response)
            }
            _ => Err(unknown()),
        }
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation == OP_TPSL_CREATE {
            tpsl::prepare_create(self, input).await
        } else if operation == OP_TPSL_UPDATE {
            tpsl::prepare_update(self, input).await
        } else if operation == OP_TPSL_CANCEL {
            tpsl::prepare_cancel(self, input).await
        } else if operation == OP_LIMIT_CREATE {
            limit_create::prepare(self, input).await
        } else if operation == OP_LIMIT_CANCEL {
            cancel::prepare(self, input).await
        } else if operation == OP_INCREASE
            || operation == OP_DECREASE
            || operation == OP_COLLATERAL_ADD
            || operation == OP_COLLATERAL_REMOVE
        {
            self.prepare_market_trade(operation, input).await
        } else if operation == OP_LIMIT_UPDATE {
            self.prepare_limit_update(input).await
        } else {
            Err(unknown())
        }
    }

    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<BoundarySpec> {
        let permission = self
            .permission_scope(&prepared.operation, &prepared.input, bounds)
            .await?;
        self.validate_permission_candidate(&permission.spec, prepared)?;
        Ok(permission.spec)
    }

    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        self.permission_scope(operation, input, bounds).await
    }

    fn validate_boundary_candidate(
        &self,
        spec: &BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        self.validate_permission_candidate(spec, prepared)
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        if prepared.operation == OP_TPSL_CREATE {
            return tpsl::revalidate(self, prepared).await;
        }
        if prepared.operation == OP_TPSL_UPDATE {
            return tpsl::revalidate_update(self, prepared).await;
        }
        if prepared.operation == OP_TPSL_CANCEL {
            return tpsl::revalidate_cancel(self, prepared).await;
        }
        if prepared.operation == OP_LIMIT_CREATE {
            return limit_create::revalidate(self, prepared).await;
        }
        if prepared.operation == OP_LIMIT_CANCEL {
            return cancel::revalidate(self, prepared).await;
        }
        if prepared.operation == OP_LIMIT_UPDATE {
            return revalidate_limit_update(self, prepared).await;
        }
        if !matches!(
            prepared.operation.as_str(),
            OP_INCREASE | OP_DECREASE | OP_COLLATERAL_ADD | OP_COLLATERAL_REMOVE
        ) {
            return Err(unknown());
        }
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
        if prepared
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            != Some(frozen.message_sha256())
        {
            return Err(unreviewed());
        }
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(bad_response)?;
        if frozen
            .last_valid_block_height()
            .is_some_and(|expires| height > expires)
        {
            return Err(GoFinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "revalidation",
                "The approved Perps request transaction expired and was not sent.",
                "Prepare and approve a new request; do not replay the expired transaction.",
            ));
        }
        wallet.runtime().validate_fee(&frozen).await?;
        wallet.runtime().simulate(&frozen).await?;
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        if prepared.operation != OP_LIMIT_UPDATE
            && !matches!(
                prepared.operation.as_str(),
                OP_INCREASE | OP_DECREASE | OP_COLLATERAL_ADD | OP_COLLATERAL_REMOVE
            )
            && prepared.operation != OP_LIMIT_CREATE
            && prepared.operation != OP_LIMIT_CANCEL
            && prepared.operation != OP_TPSL_CREATE
            && prepared.operation != OP_TPSL_UPDATE
            && prepared.operation != OP_TPSL_CANCEL
        {
            return Err(unknown());
        }
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
        if prepared
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            != Some(frozen.message_sha256())
        {
            return Err(unreviewed());
        }
        if prepared.operation == OP_LIMIT_CREATE {
            limit_create::validate_frozen(self, prepared, &frozen).await?;
        }
        if prepared.operation == OP_LIMIT_CANCEL {
            cancel::revalidate(self, prepared).await?;
        }
        if prepared.operation == OP_TPSL_CREATE {
            tpsl::validate_frozen(self, prepared, &frozen).await?;
        }
        if prepared.operation == OP_TPSL_UPDATE {
            tpsl::revalidate_update(self, prepared).await?;
        }
        if prepared.operation == OP_TPSL_CANCEL {
            tpsl::revalidate_cancel(self, prepared).await?;
        }
        let action = match prepared.operation.as_str() {
            OP_LIMIT_UPDATE => "update-limit-order",
            OP_LIMIT_CREATE => limit_create::EXECUTE_ACTION,
            OP_LIMIT_CANCEL => cancel::EXECUTE_ACTION,
            OP_TPSL_CREATE => tpsl::EXECUTE_CREATE_ACTION,
            OP_TPSL_UPDATE => tpsl::EXECUTE_UPDATE_ACTION,
            OP_TPSL_CANCEL => tpsl::EXECUTE_CANCEL_ACTION,
            _ => {
                return wallet
                    .runtime()
                    .dispatch(&frozen, &signer, prepared, journal)
                    .await;
            }
        };
        let sender = self.execute_sender(action);
        wallet
            .runtime()
            .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
            .await
    }

    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        let operation = prepared.operation.as_str();
        let wallet = self.wallet(&prepared.input)?;
        let signer = wallet.signer().ok_or_else(unknown)?;
        let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
        validated.assert_fresh()?;
        if prepared
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            != Some(frozen.message_sha256())
        {
            return Err(unreviewed());
        }
        // Final broker revalidation already checked the request, parent and
        // instruction accounts and simulated this exact message. Recheck the
        // fee on the frozen bytes without repeating the simulation or querying
        // a second time for mutable keeper state.
        if matches!(
            operation,
            OP_LIMIT_CREATE
                | OP_LIMIT_CANCEL
                | OP_LIMIT_UPDATE
                | OP_TPSL_CREATE
                | tpsl::OP_TPSL_UPDATE
                | tpsl::OP_TPSL_CANCEL
        ) {
            wallet.runtime().validate_fee(&frozen).await?;
            validated.assert_fresh()?;
        }
        let action = match operation {
            OP_LIMIT_CREATE => limit_create::EXECUTE_ACTION,
            OP_LIMIT_UPDATE => "update-limit-order",
            OP_LIMIT_CANCEL => cancel::EXECUTE_ACTION,
            OP_TPSL_CREATE => tpsl::EXECUTE_CREATE_ACTION,
            tpsl::OP_TPSL_UPDATE => tpsl::EXECUTE_UPDATE_ACTION,
            tpsl::OP_TPSL_CANCEL => tpsl::EXECUTE_CANCEL_ACTION,
            OP_INCREASE | OP_DECREASE | OP_COLLATERAL_ADD | OP_COLLATERAL_REMOVE => {
                return wallet
                    .runtime()
                    .dispatch(&frozen, &signer, prepared, journal)
                    .await;
            }
            _ => return Err(unknown()),
        };
        let sender = self.execute_sender(action);
        wallet
            .runtime()
            .dispatch_with_sender(&frozen, &signer, prepared, journal, &sender)
            .await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        if record.operation == OP_TPSL_CREATE {
            return tpsl::reconcile_create(self, record).await;
        }
        if record.operation == OP_TPSL_UPDATE {
            return tpsl::reconcile_update(self, record).await;
        }
        if record.operation == OP_TPSL_CANCEL {
            return tpsl::reconcile_cancel(self, record).await;
        }
        if record.operation == OP_LIMIT_CREATE {
            return limit_create::reconcile(self, record).await;
        }
        if record.operation == OP_LIMIT_CANCEL {
            return cancel::reconcile(self, record).await;
        }
        if record.operation == OP_LIMIT_UPDATE {
            return reconcile_limit_update(self, record).await;
        }
        if !matches!(
            record.operation.as_str(),
            OP_INCREASE | OP_DECREASE | OP_COLLATERAL_ADD | OP_COLLATERAL_REMOVE
        ) {
            return Err(unknown());
        }
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Perps request submission exists."}),
                verified: None,
            });
        };
        let wallet = self.wallet(&record.input)?;
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::ExpiredWithoutInclusion { reason, .. } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"reason":reason,"settlement_verifier":"expired_without_inclusion_not_supported"}),
                verified: None,
            }),
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason,"submission":submission}),
                verified: None,
            }),
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => {
                let verified =
                    crate::adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
                Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            }
            ReceiptOutcome::Finalized {
                signature,
                slot,
                fee_lamports,
                failed,
                message_sha256,
                proof,
            } => {
                if failed {
                    let verified = crate::adapter::verify_failed_cost(
                        record,
                        submission,
                        &signature,
                        slot,
                        fee_lamports,
                        &proof,
                    )?;
                    return Ok(Reconciliation {
                        status: Status::Failed,
                        evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"proof":proof}),
                        verified: Some(verified),
                    });
                }
                self.reconcile_keeper(
                    record,
                    submission,
                    &wallet,
                    &signature,
                    slot,
                    fee_lamports,
                    &message_sha256,
                    &proof,
                )
                .await
            }
        }
    }
}

async fn revalidate_limit_update(
    adapter: &PerpsAdapter,
    prepared: &PreparedOperation,
) -> Result<PreparedOperation> {
    let wallet = adapter.wallet(&prepared.input)?;
    let signer = wallet.signer().ok_or_else(unknown)?;
    let parent_id = prepared.binding["parent_operation_id"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request = prepared.binding["request"]
        .as_str()
        .ok_or_else(bad_response)?;
    let (_, parent_submission) =
        adapter.limit_create_parent(parent_id, wallet.address(), request)?;
    validate_limit_update_parent_aliases(&prepared.binding)?;
    if serde_json::to_value(&parent_submission).map_err(|_| bad_response())?
        != prepared.binding["parent_submission"]
        || prepared.binding.product_value("__gofin_amendment")?["version"].as_u64() != Some(1)
        || prepared.binding.product_value("__gofin_amendment")?["parent_operation_id"].as_str()
            != Some(parent_id)
        || prepared.binding.product_value("__gofin_amendment")?["request_id"].as_str()
            != Some(request)
        || prepared.binding.product_value("__gofin_amendment")?["parent_submission"]
            != serde_json::to_value(&parent_submission).map_err(|_| bad_response())?
        || prepared.binding.product_value("__gofin_amendment")?["shared_locks"]
            != json!([
                position_lock_resource(
                    prepared.binding["position"]
                        .as_str()
                        .ok_or_else(bad_response)?
                )
                .0,
                format!("solana:{CLUSTER}:jupiter-perps-request:{request}")
            ])
    {
        return Err(unreviewed_detail(
            "the original Perps limit submission changed after update preparation",
        ));
    }
    let frozen = freeze_prepared(&wallet, &signer, &prepared.binding).await?;
    if prepared
        .binding
        .get("message_sha256")
        .and_then(Value::as_str)
        != Some(frozen.message_sha256())
    {
        return Err(unreviewed());
    }
    let request = prepared
        .binding
        .get("request")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let existing = adapter
        .position_request_by_address(&wallet, request)
        .await?;
    if existing.position
        != prepared
            .binding
            .get("position")
            .and_then(Value::as_str)
            .ok_or_else(bad_response)?
        || existing.pool
            != prepared
                .binding
                .get("pool")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
        || existing.custody
            != prepared
                .binding
                .get("custody")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
        || existing.counter.to_string()
            != prepared
                .binding
                .get("counter")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
        || existing.side
            != prepared
                .binding
                .get("side")
                .and_then(Value::as_u64)
                .ok_or_else(bad_response)? as u8
        || existing.request_change
            != prepared
                .binding
                .get("request_change")
                .and_then(Value::as_u64)
                .ok_or_else(bad_response)? as u8
        || existing.size_usd_delta.to_string()
            != prepared
                .binding
                .get("size_usd_atoms")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
        || existing.collateral_delta.to_string()
            != prepared
                .binding
                .get("collateral_atoms")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?
        || existing.trigger_price
            != prepared
                .binding
                .get("previous_trigger_price_usd_atoms")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
        || existing.trigger_above
            != prepared
                .binding
                .get("trigger_above_threshold")
                .and_then(Value::as_bool)
        || existing.request_type != 1
        || existing.executed
    {
        return Err(receipt_mismatch());
    }
    inspect_limit_update_instruction(
        &wallet,
        &frozen,
        &existing,
        parse_binding_u64(&prepared.binding, "trigger_price_usd_atoms")?,
    )?;
    wallet.runtime().validate_fee(&frozen).await?;
    wallet.runtime().simulate(&frozen).await?;
    Ok(prepared.clone())
}

fn validate_limit_update_parent_aliases(binding: &Value) -> Result<()> {
    let operation_id = binding.get("original_operation_id");
    let submission = binding.get("original_submission");
    if operation_id.is_none() && submission.is_none() {
        return Ok(());
    }
    let alias_submission: Submission =
        serde_json::from_value(submission.cloned().ok_or_else(bad_response)?)
            .map_err(|_| bad_response())?;
    let parent_submission = binding.get("parent_submission").ok_or_else(bad_response)?;
    if operation_id.and_then(Value::as_str)
        != binding.get("parent_operation_id").and_then(Value::as_str)
        || parent_submission
            != &serde_json::to_value(alias_submission).map_err(|_| bad_response())?
    {
        return Err(unreviewed_detail(
            "the Perps limit recovery aliases differ from the authoritative parent binding",
        ));
    }
    Ok(())
}

async fn reconcile_limit_update(
    adapter: &PerpsAdapter,
    record: &OperationRecord,
) -> Result<Reconciliation> {
    let Some(submission) = record.submission.as_ref() else {
        return Ok(Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":"No durable Perps limit update submission exists."}),
            verified: None,
        });
    };
    let preview = record.preview.as_ref().ok_or_else(bad_response)?;
    let binding = &preview.binding;
    let wallet = adapter.wallet(&record.input)?;
    let parent_id = binding["parent_operation_id"]
        .as_str()
        .ok_or_else(bad_response)?;
    let request = binding["request"].as_str().ok_or_else(bad_response)?;
    let parent = adapter.parent_operation(parent_id)?.ok_or_else(|| {
        unreviewed_detail("the original limit create record is unavailable during update recovery")
    })?;
    let parent_binding = parent
        .preview
        .as_ref()
        .map(|value| &value.binding)
        .ok_or_else(bad_response)?;
    let saved_parent_submission: Submission =
        serde_json::from_value(binding["parent_submission"].clone()).map_err(|_| bad_response())?;
    if parent.operation != OP_LIMIT_CREATE
        || parent_binding["wallet_address"].as_str() != Some(wallet.address())
        || parent_binding["request"] != binding["request"]
        || parent.submission.as_ref() != Some(&saved_parent_submission)
        || binding.product_value("__gofin_amendment")?["version"].as_u64() != Some(1)
        || binding.product_value("__gofin_amendment")?["parent_operation_id"].as_str()
            != Some(parent_id)
        || binding.product_value("__gofin_amendment")?["request_id"].as_str() != Some(request)
        || binding.product_value("__gofin_amendment")?["parent_submission"]
            != serde_json::to_value(&saved_parent_submission).map_err(|_| bad_response())?
    {
        return Err(unreviewed_detail(
            "the durable update lineage no longer matches its original create request",
        ));
    }
    match wallet
        .runtime()
        .reconcile_submission(submission, record.dispatch_evidence.as_ref())
        .await?
    {
        ReceiptOutcome::ExpiredWithoutInclusion { reason, .. } => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"reason":reason,"settlement_verifier":"expired_without_inclusion_not_supported"}),
            verified: None,
        }),
        ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"confirmed":confirmed,"reason":reason}),
            verified: None,
        }),
        ReceiptOutcome::NotDispatched {
            reason_code,
            reason,
        } => {
            let verified =
                crate::adapter::verify_not_dispatched(submission, &reason_code, &reason)?;
            Ok(Reconciliation {
                status: Status::Failed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            })
        }
        ReceiptOutcome::Finalized {
            signature,
            slot,
            fee_lamports,
            failed,
            message_sha256,
            proof,
        } => {
            if failed {
                let verified = crate::adapter::verify_failed_cost(
                    record,
                    submission,
                    &signature,
                    slot,
                    fee_lamports,
                    &proof,
                )?;
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                });
            }
            let request = binding
                .get("request")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?;
            let trigger = parse_binding_u64(binding, "trigger_price_usd_atoms")?;
            let wallet_address = binding
                .get("wallet_address")
                .and_then(Value::as_str)
                .ok_or_else(bad_response)?;
            if proof.message_sha256 != message_sha256
                || fee_lamports > parse_binding_u64(binding, "maximum_network_fee_lamports")?
            {
                return Err(receipt_mismatch());
            }
            inspect_limit_update_message(
                &proof.message_instructions,
                wallet_address,
                request,
                binding
                    .get("position")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?,
                binding
                    .get("pool")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?,
                binding
                    .get("custody")
                    .and_then(Value::as_str)
                    .ok_or_else(bad_response)?,
                parse_binding_u64(binding, "size_usd_atoms")?,
                trigger,
            )?;
            let mut effects = Vec::new();
            if proof
                .account_keys
                .first()
                .is_some_and(|payer| payer == wallet_address)
                && fee_lamports > 0
            {
                effects.push(gofin_domain::FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource_for_wallet(wallet_address),
                    },
                    amount: Amount {
                        asset: asset("native", 9),
                        atoms: gofin_domain::Quantity::from_u64(fee_lamports),
                    },
                    category: "solana_network_fee".into(),
                    treatment: CostTreatment::AdditionalDebit,
                });
            }
            let evidence = json!({
                "cluster":CLUSTER,
                "signature":signature,
                "slot":slot,
                "message_sha256":message_sha256,
                "request":request,
                "original_operation_id":parent_id,
                "previous_trigger_price_usd_atoms":binding.get("previous_trigger_price_usd_atoms"),
                "trigger_price_usd_atoms":trigger.to_string(),
                "request_state":"finalized_open_limit",
                "proof":proof
            });
            let mut verified = gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence,
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

struct VerifiedPosition {
    address: String,
    side: gofin_domain::PositionKind,
    market_mint: String,
    market_decimals: u8,
    amount: Amount,
    collateral_mint: String,
    collateral_usd_atoms: u64,
    rent_lamports: u64,
    locked_profit_cap_atoms: u64,
    size_usd_atoms: u64,
    price_usd_atoms: u64,
    realized_pnl_usd_atoms: i64,
    cumulative_interest_snapshot: u128,
    observed_slot: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
struct VerifiedRequest {
    address: String,
    pool: String,
    custody: String,
    position: String,
    mint: String,
    size_usd_delta: u64,
    collateral_delta: u64,
    request_change: u8,
    request_type: u8,
    side: u8,
    trigger_price: Option<u64>,
    trigger_above: Option<bool>,
    entire_position: Option<bool>,
    executed: bool,
    counter: u64,
    rent_lamports: u64,
}

impl VerifiedRequest {
    fn is_resting_limit(&self) -> bool {
        self.request_type == 1 && !self.executed
    }

    fn read_value(&self) -> Value {
        json!({
            "request":self.address,
            "position":self.position,
            "mint":self.mint,
            "request_change":match self.request_change {1=>"increase",2=>"decrease",_=>"unknown"},
            "request_type":match self.request_type {0=>"market",1=>"trigger",_=>"unknown"},
            "side":match self.side {1=>"long",2=>"short",_=>"unknown"},
            "size_usd_delta_atoms":self.size_usd_delta.to_string(),
            "collateral_delta_atoms":self.collateral_delta.to_string(),
            "trigger_price_usd_atoms":self.trigger_price.map(|value|value.to_string()),
            "trigger_above_threshold":self.trigger_above,
            "entire_position":self.entire_position,
            "executed":self.executed,
            "counter":self.counter.to_string(),
            "rent_lamports":self.rent_lamports.to_string()
        })
    }
}

fn decode_position_request(owner: &str, address: &str, data: &[u8]) -> Result<VerifiedRequest> {
    if data.len() != POSITION_REQUEST_ACCOUNT_SPACE
        || data.get(..8) != Some(account_discriminator(POSITION_REQUEST_ACCOUNT).as_slice())
    {
        return Err(invalid_account_set());
    }
    let mut cursor = BorshCursor::new(&data[8..]);
    let request_owner = cursor.key()?;
    let pool = cursor.key()?;
    let custody = cursor.key()?;
    let position = cursor.key()?;
    let mint = cursor.key()?;
    cursor.take(16)?; // openTime, updateTime
    let size_usd_delta = cursor.u64()?;
    let collateral_delta = cursor.u64()?;
    let request_change = cursor.u8()?;
    let request_type = cursor.u8()?;
    let side = cursor.u8()?;
    cursor.option_u64()?; // priceSlippage
    cursor.option_u64()?; // jupiterMinimumOut
    cursor.option_u64()?; // preSwapAmount
    let trigger_price = cursor.option_u64()?;
    let trigger_above = cursor.option_bool()?;
    let entire_position = cursor.option_bool()?;
    let executed = cursor.bool()?;
    let counter = cursor.u64()?;
    cursor.u8()?; // bump
    cursor.option_key()?; // referral
    cursor.finish_zero_padding()?;
    if request_owner != owner
        || !matches!(request_change, 1 | 2)
        || !matches!(request_type, 0 | 1)
        || !matches!(side, 1 | 2)
        || (request_type == 0 && trigger_price.is_some())
        || (request_type == 1 && (trigger_price.is_none() || trigger_above.is_none()))
        || position_request_pda(&position, counter, request_change)? != address
    {
        return Err(invalid_account_set());
    }
    Ok(VerifiedRequest {
        address: address.to_owned(),
        pool,
        custody,
        position,
        mint,
        size_usd_delta,
        collateral_delta,
        request_change,
        request_type,
        side,
        trigger_price,
        trigger_above,
        entire_position,
        executed,
        counter,
        rent_lamports: 0,
    })
}

fn inspect_limit_update_instruction(
    wallet: &SolanaWalletAdapter,
    frozen: &FrozenTransaction,
    request: &VerifiedRequest,
    trigger_price: u64,
) -> Result<()> {
    inspect_limit_update_message(
        frozen.instructions(),
        wallet.address(),
        &request.address,
        &request.position,
        &request.pool,
        &request.custody,
        request.size_usd_delta,
        trigger_price,
    )
}

// These explicit values are the immutable approved update binding.
#[allow(clippy::too_many_arguments)]
fn inspect_limit_update_message(
    instructions: &[gofin_solana::transaction::NormalizedInstruction],
    wallet_address: &str,
    request_address: &str,
    position_address: &str,
    pool_address: &str,
    custody_address: &str,
    expected_size: u64,
    trigger_price: u64,
) -> Result<()> {
    let perps = instructions
        .iter()
        .filter(|instruction| instruction.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if perps.len() != 1 {
        return Err(unreviewed_detail("expected one limit update instruction"));
    }
    if instructions.iter().any(|instruction| {
        instruction.program_id != crate::jlp::PERPS_PROGRAM_ID
            && instruction.program_id != "ComputeBudget111111111111111111111111111111"
    }) {
        return Err(unreviewed_detail(
            "unexpected auxiliary program instruction",
        ));
    }
    let instruction = perps[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| bad_response())?;
    if data.get(..8) != Some(instruction_discriminator("instantUpdateLimitOrder").as_slice())
        || instruction.accounts.len() != 10
    {
        return Err(unreviewed_detail(
            "limit update discriminator or pinned account count",
        ));
    }
    let mut cursor = BorshCursor::new(data.get(8..).ok_or_else(bad_response)?);
    let size = cursor.u64()?;
    let updated_trigger = cursor.u64()?;
    let request_time = i64::from_le_bytes(cursor.take(8)?.try_into().map_err(|_| bad_response())?);
    cursor.finish()?;
    if size != expected_size || updated_trigger != trigger_price || request_time <= 0 {
        return Err(unreviewed_detail(
            "limit update size or trigger price differs from approved input",
        ));
    }
    let accounts = &instruction.accounts;
    let keeper = &accounts[0].address;
    let api_keeper = &accounts[1].address;
    if keeper == wallet_address
        || api_keeper == wallet_address
        || keeper == api_keeper
        || !accounts[0].signer
        || !accounts[1].signer
        || accounts
            .iter()
            .enumerate()
            .any(|(index, account)| account.signer && !matches!(index, 0..=2))
        || accounts[2].address != wallet_address
        || !accounts[2].signer
        || accounts[3].address != pda(&[b"perpetuals"])?
        || accounts[4].address != pool_address
        || accounts[5].address != position_address
        || accounts[6].address != request_address
        || accounts[7].address != custody_address
        || !accounts[6].writable
    {
        return Err(unreviewed_detail(
            "limit update account identity or privilege",
        ));
    }
    Ok(())
}

struct MarketRequest {
    position: String,
    request: String,
    market_mint: String,
    collateral_mint: String,
    funding_mint: String,
    market_decimals: u8,
    collateral_decimals: u8,
    funding_decimals: u8,
    maximum_fee_bps: u64,
    minimum_output: Option<u64>,
    borrow_cumulative_interest: u128,
    borrow_hourly_rate: u128,
    borrow_last_update: i64,
    max_request_execution_seconds: u64,
    swap_fee_bps: u64,
    counter: u64,
    market: String,
}

// This checks the full request against pinned market and funding identities.
#[allow(clippy::too_many_arguments)]
async fn inspect_market_request(
    wallet: &SolanaWalletAdapter,
    frozen: &gofin_solana::transaction::FrozenTransaction,
    increase: bool,
    requested_size: u64,
    requested_collateral: u64,
    side: u8,
    max_slippage_bps: u64,
    minimum_receive_atoms: u64,
    requested_market: &str,
    requested_collateral_symbol: &str,
    expected_position: Option<&str>,
    entire_position: bool,
    expected_position_collateral_mint: Option<&str>,
) -> Result<MarketRequest> {
    let name = if increase {
        "createIncreasePositionMarketRequest"
    } else {
        "createDecreasePositionMarketRequest"
    };
    let discriminator = instruction_discriminator(name);
    let selected_mint = token_mint_for_symbol(requested_collateral_symbol)?;
    let position_collateral_mint = if increase {
        expected_position_collateral_mint
            .unwrap_or(collateral_mint_for_market(requested_market, side)?)
    } else {
        expected_position_collateral_mint.ok_or_else(unreviewed)?
    };
    let requires_conversion = selected_mint != position_collateral_mint;
    let primary = frozen
        .instructions()
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .collect::<Vec<_>>();
    if primary.len() != 1 {
        return Err(unreviewed_detail("expected one Perps instruction"));
    }
    let ix = primary[0];
    let data = base64::engine::general_purpose::STANDARD
        .decode(&ix.data_base64)
        .map_err(|_| bad_response())?;
    if data.get(..8) != Some(discriminator.as_slice()) || ix.accounts.len() != 16 {
        return Err(unreviewed_detail(
            "instruction discriminator or account count",
        ));
    }
    for (index, account) in ix.accounts.iter().enumerate() {
        let (signer, writable) = if increase {
            (index == 0, matches!(index, 0 | 1 | 4 | 5 | 6))
        } else {
            (index == 0, matches!(index, 0 | 1 | 5 | 6))
        };
        if account.signer != signer || account.writable != writable {
            return Err(unreviewed_detail(&format!(
                "account privilege at index {index}"
            )));
        }
    }
    let (size, collateral, encoded_side, minimum_output, counter) = if increase {
        let mut c = BorshCursor::new(data.get(8..).ok_or_else(bad_response)?);
        let size = c.u64()?;
        let collateral = c.u64()?;
        let side = c.u8()?;
        let slippage = c.u64()?;
        if slippage != max_slippage_bps {
            return Err(unreviewed_detail(
                "instruction slippage does not match input",
            ));
        }
        let minimum_output = c.option_u64()?;
        validate_minimum_output(minimum_output, requires_conversion, minimum_receive_atoms)?;
        let counter = c.u64()?;
        c.finish()?;
        (size, collateral, side, minimum_output, counter)
    } else {
        let (collateral, size, slippage, minimum_output, entire, counter) =
            decode_decrease_market_params(data.get(8..).ok_or_else(bad_response)?)?;
        if slippage != max_slippage_bps {
            return Err(unreviewed_detail(
                "instruction slippage does not match input",
            ));
        }
        validate_minimum_output(minimum_output, requires_conversion, minimum_receive_atoms)?;
        if entire != Some(entire_position) {
            return Err(unreviewed_detail(
                "entire-position option does not match input",
            ));
        }
        (size, collateral, side, minimum_output, counter)
    };
    if size != requested_size || collateral != requested_collateral || encoded_side != side {
        return Err(unreviewed_detail(
            "encoded size, collateral, or side does not match input",
        ));
    }
    if ix.accounts[0].address != wallet.address()
        || ix.accounts[2].address != pda(&[b"perpetuals"])?
        || ix.accounts[14].address != pda(&[b"__event_authority"])?
        || ix.accounts[15].address != crate::jlp::PERPS_PROGRAM_ID
        || !valid_optional_referral(&ix.accounts[10].address)
        || ix.accounts[12].address != "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        || ix.accounts[13].address != "11111111111111111111111111111111"
    {
        return Err(unreviewed_detail("required Perps program account identity"));
    }
    let pool = &ix.accounts[3].address;
    let custody = &ix.accounts[7].address;
    let collateral_custody = &ix.accounts[8].address;
    let pool_data = account_data(wallet, pool)
        .await
        .map_err(|_| invalid_account_set_detail("finalized pool account"))?;
    let custody_data = account_data(wallet, custody)
        .await
        .map_err(|_| invalid_account_set_detail("finalized market custody account"))?;
    let collateral_data = account_data(wallet, collateral_custody)
        .await
        .map_err(|_| invalid_account_set_detail("finalized collateral custody account"))?;
    if pool_data.get(..8) != Some(account_discriminator("Pool").as_slice())
        || custody_data.len() < 105
        || collateral_data.len() < 105
        || address_field(&custody_data, 8)? != *pool
        || address_field(&collateral_data, 8)? != *pool
    {
        return Err(invalid_account_set_detail(
            "pool and custody account layouts",
        ));
    }
    let (market, name_end) = decode_pool_name(&pool_data)?;
    let pool_name_len = market.len();
    // Pool.name identifies a liquidity pool (currently "Pool"), not the
    // selected market. The market identity is bound below through the
    // finalized custody mint, while the exact pool name is used in its PDA.
    if market.trim().is_empty() || !market.is_ascii() {
        return Err(invalid_account_set_detail("pool custody membership"));
    }
    let pool_count = read_u32(&pool_data, name_end)? as usize;
    if pool_count == 0 || pool_count > 32 {
        return Err(invalid_account_set_detail(
            "custody identity or mint layout",
        ));
    }
    let pool_keys_start = name_end + 4;
    let pool_keys_end = pool_keys_start
        .checked_add(pool_count.checked_mul(32).ok_or_else(invalid_account_set)?)
        .ok_or_else(invalid_account_set)?;
    if pool_keys_end > pool_data.len()
        || !pool_data[pool_keys_start..pool_keys_end]
            .chunks_exact(32)
            .any(|key| bs58::encode(key).into_string() == *custody)
        || !pool_data[pool_keys_start..pool_keys_end]
            .chunks_exact(32)
            .any(|key| bs58::encode(key).into_string() == *collateral_custody)
    {
        return Err(invalid_account_set_detail("pool custody membership"));
    }
    let pool_pda = solana_pubkey3::Pubkey::find_program_address(
        &[b"pool", market.as_bytes()],
        &crate::jlp::PERPS_PROGRAM_ID
            .parse()
            .map_err(|_| invalid_account_set())?,
    )
    .0
    .to_string();
    if pool_pda != *pool {
        return Err(invalid_account_set());
    }
    let market_mint_address = address_field(&custody_data, 40)?;
    let collateral_mint = address_field(&collateral_data, 40)?;
    let expected_market_mint = market_mint_for_symbol(requested_market)?;
    let expected_collateral_mint = if increase {
        expected_position_collateral_mint
            .unwrap_or(collateral_mint_for_market(requested_market, side)?)
    } else {
        expected_position_collateral_mint.ok_or_else(unreviewed)?
    };
    let funding_mint = token_mint_for_symbol(requested_collateral_symbol)?;
    if market_mint_address != expected_market_mint
        || collateral_mint != expected_collateral_mint
        || !supported_receive_mint(funding_mint)
    {
        return Err(unreviewed_detail(&format!(
            "market mint match={}, historical collateral match={}, receive mint supported={}",
            market_mint_address == expected_market_mint,
            collateral_mint == expected_collateral_mint,
            supported_receive_mint(funding_mint),
        )));
    }
    verify_custody_pda(pool, custody, &market_mint_address)?;
    verify_custody_pda(pool, collateral_custody, &collateral_mint)?;
    if ix.accounts[9].address != funding_mint {
        return Err(unreviewed_detail("receive mint account role"));
    }
    let position = &ix.accounts[4].address;
    if expected_position.is_some_and(|expected| expected != position) {
        return Err(unreviewed_detail("exact position account"));
    }
    let expected = position_pda(wallet.address(), pool, custody, collateral_custody, side)?;
    if expected != *position {
        return Err(unreviewed_detail("position PDA derivation"));
    }
    let expected_request = position_request_pda(position, counter, if increase { 1 } else { 2 })?;
    if ix.accounts[5].address != expected_request {
        return Err(unreviewed_detail("request PDA derivation"));
    }
    let token_program = ix.accounts[11].address.as_str();
    if token_program != gofin_solana::token::TOKEN_PROGRAM_ID
        && token_program != gofin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(unreviewed());
    }
    if custody_data.len() < 932 {
        return Err(invalid_account_set_detail(&format!(
            "custody risk and fee parameters for {} ({} bytes, 932 required)",
            custody,
            custody_data.len(),
        )));
    }
    let base_fee_bps = read_u64(&custody_data, if increase { 296 } else { 304 })?;
    // Custody.maxFeeBps bounds the total position fee once the dynamic impact
    // branch is enabled. Without that branch, bound the base fee plus the
    // maximum linear impact implied by tradeImpactFeeScalar.
    let fee_factor = read_u64(&custody_data, 904)?;
    let maximum_fee_bps = if fee_factor > 0 {
        read_u64(&custody_data, 924)?
    } else {
        let impact_scalar = read_u64(&custody_data, 151)?;
        let linear_impact = if impact_scalar == 0 {
            0
        } else {
            let numerator = u128::from(requested_size)
                .checked_mul(10_000)
                .ok_or_else(invalid_account_set)?;
            u64::try_from(
                numerator
                    .checked_add(u128::from(impact_scalar) - 1)
                    .ok_or_else(invalid_account_set)?
                    / u128::from(impact_scalar),
            )
            .map_err(|_| invalid_account_set())?
        };
        base_fee_bps
            .checked_add(linear_impact)
            .ok_or_else(invalid_account_set)?
    };
    let swap_fee_bps = pool_swap_fee_bps(&pool_data, pool_name_len, pool_count)?;
    let market_decimals = mint_decimals(wallet, &market_mint_address).await?;
    let collateral_decimals = mint_decimals(wallet, &collateral_mint).await?;
    let funding_decimals = mint_decimals(wallet, funding_mint).await?;
    let expected_wallet_token =
        gofin_solana::associated_token_address(wallet.address(), funding_mint, token_program)?;
    if ix.accounts[1].address != expected_wallet_token {
        return Err(unreviewed_detail("wallet receive token account"));
    }
    // Increase requests stage the selected input token in the request ATA;
    // decrease requests stage the selected desired output token there.
    let expected_request_ata =
        gofin_solana::associated_token_address(&expected_request, funding_mint, token_program)?;
    if ix.accounts[6].address != expected_request_ata {
        return Err(unreviewed_detail("request token account"));
    }
    Ok(MarketRequest {
        position: position.clone(),
        request: expected_request,
        market_mint: market_mint_address,
        collateral_mint,
        funding_mint: funding_mint.into(),
        market_decimals,
        collateral_decimals,
        funding_decimals,
        maximum_fee_bps,
        minimum_output,
        // The pinned custody's first fundingRateState is the state used by
        // Position.cumulativeInterestSnapshot. borrowsFundingRateState later
        // in the account belongs to the separate borrow/lend subsystem.
        borrow_cumulative_interest: read_u128(&custody_data, 262)?,
        borrow_hourly_rate: custody_hourly_funding_rate(&custody_data)?,
        borrow_last_update: read_i64(&custody_data, 278)?,
        max_request_execution_seconds: pool_max_request_execution_seconds(
            &pool_data,
            pool_name_len,
            pool_count,
        )?,
        swap_fee_bps,
        counter,
        market,
    })
}

/// Decode the pinned `CreateDecreasePositionMarketRequestParams` Borsh layout.
/// Keep this independent of instruction/account assembly so sentinel tests can
/// catch field reordering or width regressions against the checked-in IDL.
type DecreaseMarketParams = (u64, u64, u64, Option<u64>, Option<bool>, u64);

fn decode_decrease_market_params(bytes: &[u8]) -> Result<DecreaseMarketParams> {
    let mut c = BorshCursor::new(bytes);
    let collateral_usd_delta = c.u64()?;
    let size_usd_delta = c.u64()?;
    let price_slippage = c.u64()?;
    let jupiter_minimum_out = c.option_u64()?;
    let entire_position = c.option_bool()?;
    let counter = c.u64()?;
    c.finish()?;
    Ok((
        collateral_usd_delta,
        size_usd_delta,
        price_slippage,
        jupiter_minimum_out,
        entire_position,
        counter,
    ))
}

fn verify_custody_pda(pool: &str, custody: &str, mint: &str) -> Result<()> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let pool = pool
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let mint = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let expected = solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool.as_ref(), mint.as_ref()],
        &program,
    )
    .0
    .to_string();
    if expected == custody {
        Ok(())
    } else {
        Err(invalid_account_set())
    }
}

fn pda(seeds: &[&[u8]]) -> Result<String> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    Ok(
        solana_pubkey3::Pubkey::find_program_address(seeds, &program)
            .0
            .to_string(),
    )
}

fn position_pda(
    owner: &str,
    pool: &str,
    custody: &str,
    collateral_custody: &str,
    side: u8,
) -> Result<String> {
    if !matches!(side, 1 | 2) {
        return Err(invalid_account_set());
    }
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let owner = owner
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let pool = pool
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let custody = custody
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let collateral_custody = collateral_custody
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[
            b"position",
            owner.as_ref(),
            pool.as_ref(),
            custody.as_ref(),
            collateral_custody.as_ref(),
            &[side],
        ],
        &program,
    )
    .0
    .to_string())
}

fn position_request_pda(position: &str, counter: u64, request_change: u8) -> Result<String> {
    if !matches!(request_change, 1 | 2) {
        return Err(invalid_account_set());
    }
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let position = position
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let counter_seed = counter.to_le_bytes();
    Ok(solana_pubkey3::Pubkey::find_program_address(
        &[
            b"position_request",
            position.as_ref(),
            &counter_seed,
            &[request_change],
        ],
        &program,
    )
    .0
    .to_string())
}

fn derive_position_address(
    owner: &str,
    market_mint: &str,
    collateral_mint: &str,
    side: u8,
) -> Result<String> {
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let owner = owner
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let market = market_mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let collateral = collateral_mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_account_set())?;
    let pool = main_pool_address(&program);
    let custody = solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool.as_ref(), market.as_ref()],
        &program,
    )
    .0;
    let collateral_custody = solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool.as_ref(), collateral.as_ref()],
        &program,
    )
    .0;
    position_pda(
        &owner.to_string(),
        &pool.to_string(),
        &custody.to_string(),
        &collateral_custody.to_string(),
        side,
    )
}

fn main_pool_address(program: &solana_pubkey3::Pubkey) -> solana_pubkey3::Pubkey {
    solana_pubkey3::Pubkey::find_program_address(&[b"pool", MAIN_POOL_NAME], program).0
}

fn decode_pool_name(pool_data: &[u8]) -> Result<(String, usize)> {
    if pool_data.get(..8) != Some(account_discriminator("Pool").as_slice()) {
        return Err(invalid_account_set_detail("pool PDA"));
    }
    let name_len = read_u32(pool_data, 8)? as usize;
    if name_len == 0 || name_len > 64 {
        return Err(invalid_account_set_detail("pool custody list"));
    }
    let name_end = 12usize
        .checked_add(name_len)
        .ok_or_else(invalid_account_set)?;
    if name_end + 4 > pool_data.len() {
        return Err(invalid_account_set_detail("pool PDA"));
    }
    let name = std::str::from_utf8(&pool_data[12..name_end])
        .map_err(|_| invalid_account_set())?
        .to_owned();
    Ok((name, name_end))
}

fn derive_position_identity(
    owner: &str,
    position: &str,
) -> Result<(&'static str, String, String, u8)> {
    gofin_solana::parse_address(position)?;
    for market in ["SOL", "ETH", "BTC"] {
        let market_mint = market_mint_for_symbol(market)?.to_owned();
        for side in [1, 2] {
            let collateral_mints = if side == 2 {
                vec![
                    collateral_mint_for_market(market, side)?,
                    token_mint_for_symbol("USDT")?,
                ]
            } else {
                vec![collateral_mint_for_market(market, side)?]
            };
            for collateral_mint in collateral_mints {
                if derive_position_address(owner, &market_mint, collateral_mint, side)? == position
                {
                    return Ok((market, market_mint, collateral_mint.to_owned(), side));
                }
            }
        }
    }
    Err(GoFinError::new(
        "JUPITER_PERPS_POSITION_UNREVIEWED",
        "authorization",
        "The Perps position address does not match a reviewed wallet, market and side identity.",
        "Use a position derived for this enrolled wallet on a supported SOL, ETH or BTC market.",
    ))
}

fn parse_binding_u64(binding: &Value, key: &str) -> Result<u64> {
    binding
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| binding.get(key).and_then(Value::as_u64))
        .ok_or_else(|| GoFinError::validation("The Perps boundary binding is incomplete."))
}

fn instruction_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut snake = String::new();
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            snake.push('_');
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
    }
    let digest = Sha256::digest(format!("global:{snake}").as_bytes());
    digest[..8].try_into().expect("eight byte discriminator")
}

fn required_text<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GoFinError::validation("A required Perps input is missing."))
}
fn required_u64(input: &Value, field: &str) -> Result<u64> {
    input
        .get(field)
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v.as_u64())
        })
        .filter(|v| *v > 0)
        .ok_or_else(|| GoFinError::validation("A positive Perps atomic amount is required."))
}
fn required_market_symbol(input: &Value, field: &str) -> Result<&'static str> {
    match required_text(input, field)? {
        "SOL" => Ok("SOL"),
        "ETH" => Ok("ETH"),
        "BTC" => Ok("BTC"),
        _ => Err(GoFinError::validation(
            "Choose a reviewed SOL, ETH, or BTC Jupiter Perps market.",
        )),
    }
}
fn required_funding_symbol(input: &Value, _side: u8, _market: &str) -> Result<&'static str> {
    let symbol = match required_text(input, "funding_token")? {
        "SOL" => "SOL",
        "ETH" => "ETH",
        "BTC" => "BTC",
        "USDC" => "USDC",
        _ => {
            return Err(GoFinError::validation(
                "Choose a funding token supported by the Jupiter Perps market builder.",
            ));
        }
    };
    // The venue builder accepts all four reviewed input tokens and converts
    // them into the market's collateral mint. Prepare verifies the exact
    // minimum conversion output against prices, slippage and the fee bound.
    Ok(symbol)
}

fn market_mint_for_symbol(symbol: &str) -> Result<&'static str> {
    match symbol {
        "SOL" => Ok("So11111111111111111111111111111111111111112"),
        "ETH" => Ok("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs"),
        "BTC" => Ok("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh"),
        _ => Err(unreviewed()),
    }
}

fn market_symbol_for_mint(mint: &str) -> Option<&'static str> {
    ["SOL", "ETH", "BTC"]
        .into_iter()
        .find(|symbol| market_mint_for_symbol(symbol).ok() == Some(mint))
}

fn token_mint_for_symbol(symbol: &str) -> Result<&'static str> {
    match symbol {
        "USDC" => Ok("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        "USDT" => Ok("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
        symbol => market_mint_for_symbol(symbol),
    }
}

fn token_symbol_for_mint(mint: &str) -> Option<&'static str> {
    ["SOL", "ETH", "BTC", "USDC", "USDT"]
        .into_iter()
        .find(|symbol| token_mint_for_symbol(symbol).ok() == Some(mint))
}

fn supported_receive_mint(mint: &str) -> bool {
    ["SOL", "ETH", "BTC", "USDC"]
        .into_iter()
        .any(|symbol| token_mint_for_symbol(symbol).ok() == Some(mint))
}

fn supported_decrease_collateral(market: &str, side: u8, mint: &str) -> bool {
    if side == 1 {
        market_mint_for_symbol(market).ok() == Some(mint)
    } else {
        token_mint_for_symbol("USDC").ok() == Some(mint)
            || token_mint_for_symbol("USDT").ok() == Some(mint)
    }
}

/// Keep the collateral identity of an existing position when preparing a
/// mutation. The market default is only valid when no position exists yet.
fn expected_position_collateral_mint(
    market: &str,
    side: u8,
    existing_position_mint: Option<&str>,
) -> Result<String> {
    match existing_position_mint {
        Some(mint) if supported_decrease_collateral(market, side, mint) => Ok(mint.to_owned()),
        Some(_) => Err(GoFinError::validation(
            "The existing Perps position uses an unsupported collateral mint.",
        )),
        None => Ok(collateral_mint_for_market(market, side)?.to_owned()),
    }
}

fn validate_minimum_output(
    minimum_output: Option<u64>,
    requires_conversion: bool,
    minimum_receive: u64,
) -> Result<()> {
    if (requires_conversion && minimum_output.unwrap_or(0) == 0)
        || minimum_output.is_some_and(|minimum| minimum < minimum_receive)
    {
        Err(unreviewed())
    } else {
        Ok(())
    }
}

async fn conversion_output_floor(
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
    request: &MarketRequest,
    input_atoms: u64,
    maximum_slippage_bps: u64,
    maximum_fee_usd_micros: u64,
) -> Result<gofin_domain::Quantity> {
    if maximum_slippage_bps >= 10_000 {
        return Err(unreviewed());
    }
    let input = asset(&request.funding_mint, request.funding_decimals);
    let output = asset(&request.collateral_mint, request.collateral_decimals);
    let quotes = prices.quotes(&[input.clone(), output.clone()]).await?;
    if quotes.len() != 2 {
        return Err(bad_response());
    }
    let input_price = crate::adapter::valuation_asset_price(&input, &quotes[0])?;
    let output_price = crate::adapter::valuation_asset_price(&output, &quotes[1])?;
    let input_usd = gofin_domain::Quantity::from_u64(input_atoms)
        .mul_div_floor(&input_price.usd_micros_per_unit, &input_price.unit_atoms)?;
    conversion_minimum_atoms(
        &input_usd,
        &output_price,
        maximum_slippage_bps,
        maximum_fee_usd_micros,
    )
}

fn conversion_minimum_atoms(
    input_usd_micros: &gofin_domain::Quantity,
    output_price: &gofin_domain::AssetPrice,
    maximum_slippage_bps: u64,
    maximum_fee_usd_micros: u64,
) -> Result<gofin_domain::Quantity> {
    if maximum_slippage_bps >= 10_000 {
        return Err(unreviewed());
    }
    let after_slippage = input_usd_micros.mul_div_floor(
        &gofin_domain::Quantity::from_u64(10_000 - maximum_slippage_bps),
        &gofin_domain::Quantity::from_u64(10_000),
    )?;
    let fee_cap = gofin_domain::Quantity::from_u64(maximum_fee_usd_micros);
    if after_slippage <= fee_cap {
        return Err(GoFinError::validation(
            "The approved collateral amount and slippage bound leave no positive net margin after the protocol-fee cap.",
        ));
    }
    let net_margin_usd = after_slippage.checked_sub(&fee_cap)?;
    net_margin_usd.mul_div_floor(&output_price.unit_atoms, &output_price.usd_micros_per_unit)
}

fn value_atoms_micros(
    atoms: u64,
    price: &gofin_domain::AssetPrice,
) -> Result<gofin_domain::Quantity> {
    gofin_domain::Quantity::from_u64(atoms)
        .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)
}

fn valid_optional_referral(address: &str) -> bool {
    // Anchor's optional-account None sentinel is the invoked program ID.
    // Named referrals remain unsupported until their fee/identity effects are
    // independently reviewed.
    address == crate::jlp::PERPS_PROGRAM_ID
}

fn collateral_mint_for_market(market: &str, side: u8) -> Result<&'static str> {
    if side == 1 {
        market_mint_for_symbol(market)
    } else if side == 2 {
        Ok("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
    } else {
        Err(unreviewed())
    }
}
fn parse_u64_text(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad_response)
}
async fn freeze_prepared(
    wallet: &SolanaWalletAdapter,
    signer: &Arc<gofin_solana::SolanaSigner>,
    binding: &Value,
) -> Result<FrozenTransaction> {
    let encoded = binding
        .get("transaction_base64")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let last_valid = binding
        .get("last_valid_block_height")
        .and_then(Value::as_u64)
        .ok_or_else(bad_response)?;
    let fee_cap = binding
        .get("maximum_network_fee_lamports")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(bad_response)?;
    let operation = binding
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(bad_response)?;
    let allowed_cosigners = if matches!(
        operation,
        OP_LIMIT_CREATE
            | OP_LIMIT_UPDATE
            | OP_LIMIT_CANCEL
            | tpsl::OP_TPSL_CREATE
            | tpsl::OP_TPSL_UPDATE
            | tpsl::OP_TPSL_CANCEL
    ) {
        approved_missing_signers_from_binding(encoded, signer.address(), binding)?
    } else {
        vec![]
    };
    wallet
        .runtime()
        .freeze_transaction(
            encoded,
            signer,
            last_valid,
            fee_cap,
            None,
            &allowed_cosigners,
        )
        .await
}

pub(super) fn approved_missing_signers(
    encoded: &str,
    wallet_signer: &str,
    approved_external: &[String],
) -> Result<Vec<String>> {
    let required = limit_create::foreign_required_signers(encoded, wallet_signer)?;
    if approved_external != required {
        return Err(unreviewed_detail(
            "the saved external co-signer scope differs from the immutable transaction",
        ));
    }
    let missing = limit_create::foreign_missing_signers(encoded, wallet_signer)?;
    if missing
        .iter()
        .any(|signer| !approved_external.contains(signer))
    {
        return Err(unreviewed_detail(
            "the transaction requires an external signer outside the saved approval scope",
        ));
    }
    Ok(missing)
}

pub(super) fn approved_missing_signers_from_binding(
    encoded: &str,
    wallet_signer: &str,
    binding: &Value,
) -> Result<Vec<String>> {
    let approved = binding
        .get("approved_external_cosigners")
        .and_then(Value::as_array)
        .ok_or_else(bad_response)?
        .iter()
        .map(|value| value.as_str().map(str::to_owned).ok_or_else(bad_response))
        .collect::<Result<Vec<_>>>()?;
    approved_missing_signers(encoded, wallet_signer, &approved)
}
fn asset(reference: &str, decimals: u8) -> AssetId {
    AssetId {
        network: NETWORK.into(),
        reference: reference.into(),
        decimals,
    }
}

fn margin_usd_asset() -> AssetId {
    AssetId {
        network: NETWORK.into(),
        reference: crate::pricing::MARGIN_USD_REFERENCE.into(),
        decimals: 6,
    }
}

fn resource_for_wallet(wallet_address: &str) -> ResourceId {
    ResourceId(format!("solana:mainnet-beta:{wallet_address}"))
}

// The anchor captures every independently verified account/event field.
#[allow(clippy::too_many_arguments)]
fn transaction_position_anchor(
    resource: ResourceId,
    position: String,
    position_kind: gofin_domain::PositionKind,
    principal: Amount,
    signature: &str,
    slot: u64,
    message_sha256: &str,
    event: Value,
) -> gofin_domain::PositionAnchor {
    gofin_domain::PositionAnchor {
        resource,
        position,
        position_kind,
        principal,
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({
            "source":"finalized_jupiter_perps_keeper_transaction",
            "signature":signature,
            "slot":slot,
            "message_sha256":message_sha256,
            "event":event
        }),
        observed_at: Utc::now().to_rfc3339(),
    }
}

fn protocol_fee_effect(
    resource: &ResourceId,
    position: &str,
    fee_usd_atoms: u64,
) -> gofin_domain::FinancialEffect {
    gofin_domain::FinancialEffect::Cost {
        location: AccountingLocation::Position {
            resource: resource.clone(),
            position: format!("{position}:collateral"),
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        },
        amount: Amount {
            asset: margin_usd_asset(),
            atoms: gofin_domain::Quantity::from_u64(fee_usd_atoms),
        },
        category: "jupiter_perps_protocol_fee".into(),
        treatment: CostTreatment::AdditionalDebit,
    }
}

fn pre_collateral_usd_atoms(binding: &Value) -> Result<u64> {
    binding
        .get("pre_collateral_usd_atoms")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(bad_response)
}
fn check(name: &str) -> Check {
    Check { name:name.into(), status:CheckStatus::Passed, detail:"Verified by the protected broker against the pinned Perps IDL and finalized Solana mainnet-beta RPC.".into() }
}
fn unreviewed() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_TRANSACTION_UNREVIEWED",
        "prepare",
        "The provider transaction does not match the exact supported Jupiter Perps request.",
        "Do not approve or submit this candidate; retry after GoFin updates its pinned instruction review.",
    )
}
fn unreviewed_detail(reason: &str) -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_TRANSACTION_UNREVIEWED",
        "prepare",
        format!("The provider transaction failed a reviewed Perps check: {reason}."),
        "Do not approve or submit this candidate; retry after GoFin updates its pinned instruction review.",
    )
}
fn bad_response() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_RESPONSE_INVALID",
        "provider",
        "Jupiter Perps returned data that does not match the supported response contract.",
        "For a read, retry the read. For a write, check GoFin status first; if it has a saved submission, reconcile that exact operation and never replay it.",
    )
}

impl VerifiedPosition {
    fn state(&self, resource: ResourceId) -> gofin_domain::PositionState {
        let anchor = self.anchor(
            resource.clone(),
            self.address.clone(),
            self.side.clone(),
            self.amount.clone(),
            json!({
                "source":"finalized_jupiter_perps_position_account",
                "position":self.address,
                "market_mint":self.market_mint,
                "collateral_mint":self.collateral_mint,
                "observed_slot":self.observed_slot
            }),
        );
        gofin_domain::PositionState {
            resource,
            position: self.address.clone(),
            kind: self.side.clone(),
            amount: self.amount.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        }
    }

    fn collateral_state(&self, resource: ResourceId) -> Result<gofin_domain::PositionState> {
        let amount = Amount {
            asset: AssetId {
                network: NETWORK.into(),
                reference: crate::pricing::MARGIN_USD_REFERENCE.into(),
                decimals: 6,
            },
            atoms: gofin_domain::Quantity::from_u64(self.collateral_usd_atoms),
        };
        let anchor = self.anchor(
            resource.clone(),
            format!("{}:collateral", self.address),
            gofin_domain::PositionKind::LockedCollateral,
            amount.clone(),
            json!({
                "source":"finalized_jupiter_perps_position_account",
                "position":self.address,
                "collateral_custody":self.collateral_mint,
                "observed_slot":self.observed_slot
            }),
        );
        Ok(gofin_domain::PositionState {
            resource,
            position: format!("{}:collateral", self.address),
            kind: gofin_domain::PositionKind::LockedCollateral,
            amount,
            scaled_receipt: None,
            anchor: Some(anchor),
        })
    }

    fn rent_state(&self, resource: ResourceId) -> gofin_domain::PositionState {
        let position = format!("{}:rent", self.address);
        let amount = Amount {
            asset: asset("native", 9),
            atoms: gofin_domain::Quantity::from_u64(self.rent_lamports),
        };
        gofin_domain::PositionState {
            resource: resource.clone(),
            position: position.clone(),
            kind: gofin_domain::PositionKind::RefundableAccountFunding,
            amount: amount.clone(),
            scaled_receipt: None,
            anchor: Some(self.anchor(
                resource,
                position,
                gofin_domain::PositionKind::RefundableAccountFunding,
                amount,
                json!({"source":"finalized_jupiter_perps_position_account_rent","position":self.address,"observed_slot":self.observed_slot}),
            )),
        }
    }

    fn market_asset(&self) -> AssetId {
        AssetId {
            network: NETWORK.into(),
            reference: self.market_mint.clone(),
            decimals: self.market_decimals,
        }
    }

    fn anchor(
        &self,
        resource: ResourceId,
        position: String,
        position_kind: gofin_domain::PositionKind,
        principal: Amount,
        evidence: Value,
    ) -> gofin_domain::PositionAnchor {
        gofin_domain::PositionAnchor {
            resource,
            position,
            position_kind,
            principal,
            scaled_receipt: None,
            evidence_version: 1,
            evidence,
            observed_at: Utc::now().to_rfc3339(),
        }
    }

    fn read_value(&self) -> Value {
        json!({
            "position":self.address,
            "side":if self.side==gofin_domain::PositionKind::DerivativeLong {"long"} else {"short"},
            "market_mint":self.market_mint,
            "market_decimals":self.market_decimals,
            "position_usd_asset":self.amount.asset.reference,
            "size_usd_atoms":self.size_usd_atoms,
            "entry_price_usd_atoms":self.price_usd_atoms,
            "collateral_mint":self.collateral_mint,
            "collateral_usd_atoms":self.collateral_usd_atoms,
            "locked_amount_token_atoms":self.locked_profit_cap_atoms,
            "realized_pnl_usd_atoms":self.realized_pnl_usd_atoms
        })
    }
}

async fn decode_position(wallet: &SolanaWalletAdapter, address: &str) -> Result<VerifiedPosition> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let observed_slot = response
        .get("context")
        .and_then(|value| value.get("slot"))
        .and_then(Value::as_u64)
        .filter(|slot| *slot > 0)
        .ok_or_else(|| invalid_snapshot_account_set("position account context slot"))?;
    let account = response
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(|| invalid_snapshot_account_set("position account response"))?;
    if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
        return Err(invalid_snapshot_account_set("position account owner"));
    }
    let data = gofin_solana::token::decode_rpc_account_data(
        account
            .get("data")
            .ok_or_else(|| invalid_snapshot_account_set("position account data"))?,
    )
    .map_err(|_| invalid_snapshot_account_set("position account data"))?;
    // Pinned IDL Position layout: owner, pool, custody, collateral custody,
    // open/update timestamps, side, price, sizeUsd, collateralUsd.
    let fields = decode_position_fields(wallet.address(), &data)
        .map_err(|_| invalid_snapshot_account_set("position account layout"))?;
    let (pool, custody, collateral_custody, side, price, size_usd) = (
        fields.pool,
        fields.custody,
        fields.collateral_custody,
        fields.side,
        fields.price,
        fields.size_usd,
    );
    let custody_data = account_data(wallet, &custody)
        .await
        .map_err(|_| invalid_snapshot_account_set("market custody account"))?;
    if custody_data.len() < 105
        || address_field(&custody_data, 8)
            .map_err(|_| invalid_snapshot_account_set("market custody pool"))?
            != pool
    {
        return Err(invalid_snapshot_account_set("market custody pool"));
    }
    let mint = address_field(&custody_data, 40)
        .map_err(|_| invalid_snapshot_account_set("market custody mint"))?;
    let collateral_data = account_data(wallet, &collateral_custody)
        .await
        .map_err(|_| invalid_snapshot_account_set("collateral custody account"))?;
    if collateral_data.len() < 105
        || address_field(&collateral_data, 8)
            .map_err(|_| invalid_snapshot_account_set("collateral custody pool"))?
            != pool
    {
        return Err(invalid_snapshot_account_set("collateral custody pool"));
    }
    let collateral_mint = address_field(&collateral_data, 40)
        .map_err(|_| invalid_snapshot_account_set("collateral custody mint"))?;
    let collateral_decimals = mint_decimals(wallet, &collateral_mint)
        .await
        .map_err(|_| invalid_snapshot_account_set("collateral mint decimals"))?;
    let pool_data = account_data(wallet, &pool)
        .await
        .map_err(|_| invalid_snapshot_account_set("Perps pool account"))?;
    if pool_data.get(..8) != Some(account_discriminator("Pool").as_slice()) {
        return Err(invalid_snapshot_account_set("Perps pool discriminator"));
    }
    let name_len = read_u32(&pool_data, 8)
        .map_err(|_| invalid_snapshot_account_set("Perps pool name"))? as usize;
    let name_end = 12usize
        .checked_add(name_len)
        .ok_or_else(|| invalid_snapshot_account_set("Perps pool name"))?;
    if name_len == 0 || name_len > 64 || name_end + 4 > pool_data.len() {
        return Err(invalid_snapshot_account_set("Perps pool name"));
    }
    let pool_name = std::str::from_utf8(&pool_data[12..name_end])
        .map_err(|_| invalid_snapshot_account_set("Perps pool name"))?;
    let custody_count = read_u32(&pool_data, name_end)
        .map_err(|_| invalid_snapshot_account_set("Perps pool custody list"))?
        as usize;
    let custody_start = name_end + 4;
    let custody_end = custody_start
        .checked_add(
            custody_count
                .checked_mul(32)
                .ok_or_else(|| invalid_snapshot_account_set("Perps pool custody list"))?,
        )
        .ok_or_else(|| invalid_snapshot_account_set("Perps pool custody list"))?;
    if custody_count == 0
        || custody_count > 32
        || custody_end > pool_data.len()
        || !pool_data[custody_start..custody_end]
            .chunks_exact(32)
            .any(|key| bs58::encode(key).into_string() == custody)
    {
        return Err(invalid_snapshot_account_set(
            "Perps pool custody membership",
        ));
    }
    let program = crate::jlp::PERPS_PROGRAM_ID
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_snapshot_account_set("Perps program address"))?;
    let pool_key = pool
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_snapshot_account_set("Perps pool address"))?;
    let mint_key = mint
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid_snapshot_account_set("market mint address"))?;
    let expected_pool =
        solana_pubkey3::Pubkey::find_program_address(&[b"pool", pool_name.as_bytes()], &program).0;
    let expected_custody = solana_pubkey3::Pubkey::find_program_address(
        &[b"custody", pool_key.as_ref(), mint_key.as_ref()],
        &program,
    )
    .0;
    let side_seed = if side == gofin_domain::PositionKind::DerivativeLong {
        [1]
    } else {
        [2]
    };
    let expected_position = position_pda(
        wallet.address(),
        &pool,
        &custody,
        &collateral_custody,
        side_seed[0],
    )
    .map_err(|_| invalid_snapshot_account_set("Perps position derivation"))?
    .parse::<solana_pubkey3::Pubkey>()
    .map_err(|_| invalid_snapshot_account_set("Perps position address"))?;
    if expected_pool.to_string() != pool
        || expected_custody.to_string() != custody
        || expected_position.to_string() != address
    {
        return Err(invalid_snapshot_account_set("position PDA derivation"));
    }
    let decimals = mint_decimals(wallet, &mint)
        .await
        .map_err(|_| invalid_snapshot_account_set("market mint decimals"))?;
    if decimals != custody_data[104] || collateral_decimals != collateral_data[104] {
        return Err(invalid_snapshot_account_set("custody mint decimals"));
    }
    if size_usd == 0 {
        return Err(invalid_snapshot_account_set("position size"));
    }
    Ok(VerifiedPosition {
        address: address.to_owned(),
        side,
        market_mint: mint.clone(),
        market_decimals: decimals,
        amount: Amount {
            asset: crate::pricing::position_usd_asset(&mint)
                .map_err(|_| invalid_snapshot_account_set("position valuation asset"))?,
            atoms: gofin_domain::Quantity::from_u64(size_usd),
        },
        collateral_mint,
        collateral_usd_atoms: fields.collateral_usd,
        rent_lamports: account_lamports(wallet, address)
            .await
            .map_err(|_| invalid_snapshot_account_set("position account rent"))?,
        locked_profit_cap_atoms: read_u64(&data, 201)
            .map_err(|_| invalid_snapshot_account_set("position account layout"))?,
        size_usd_atoms: size_usd,
        price_usd_atoms: price,
        realized_pnl_usd_atoms: fields.realized_pnl,
        cumulative_interest_snapshot: fields.cumulative_interest_snapshot,
        observed_slot,
    })
}

struct PositionFields {
    pool: String,
    custody: String,
    collateral_custody: String,
    side: gofin_domain::PositionKind,
    price: u64,
    size_usd: u64,
    collateral_usd: u64,
    realized_pnl: i64,
    cumulative_interest_snapshot: u128,
}

fn decode_position_fields(owner: &str, data: &[u8]) -> Result<PositionFields> {
    if data.len() < 209
        || data.get(..8) != Some(account_discriminator(POSITION_ACCOUNT).as_slice())
        || address_field(data, 8)? != owner
    {
        return Err(invalid_account_set());
    }
    let side = match data[152] {
        1 => gofin_domain::PositionKind::DerivativeLong,
        2 => gofin_domain::PositionKind::DerivativeShort,
        _ => return Err(invalid_account_set()),
    };
    let price = read_u64(data, 153)?;
    let size_usd = read_u64(data, 161)?;
    if price == 0 || size_usd == 0 {
        return Err(invalid_account_set());
    }
    Ok(PositionFields {
        pool: address_field(data, 40)?,
        custody: address_field(data, 72)?,
        collateral_custody: address_field(data, 104)?,
        side,
        price,
        size_usd,
        collateral_usd: read_u64(data, 169)?,
        realized_pnl: i64::from_le_bytes(
            data[177..185]
                .try_into()
                .map_err(|_| invalid_account_set())?,
        ),
        cumulative_interest_snapshot: read_u128(data, 185)?,
    })
}

async fn mint_decimals(wallet: &SolanaWalletAdapter, mint: &str) -> Result<u8> {
    let mint_account = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([mint,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let mint_value = mint_account
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(invalid_account_set)?;
    let token_program = mint_value
        .get("owner")
        .and_then(Value::as_str)
        .ok_or_else(invalid_account_set)?;
    if token_program != gofin_solana::token::TOKEN_PROGRAM_ID
        && token_program != gofin_solana::token::TOKEN_2022_PROGRAM_ID
    {
        return Err(invalid_account_set());
    }
    let epoch = wallet
        .runtime()
        .rpc()
        .request("getEpochInfo", json!([{"commitment":"finalized"}]))
        .await?
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_account_set)?;
    let bytes = gofin_solana::token::decode_rpc_account_data(
        mint_value.get("data").ok_or_else(invalid_account_set)?,
    )?;
    let info = gofin_solana::token::parse_mint(token_program, &bytes, epoch)?;
    if !info.initialized {
        return Err(invalid_account_set());
    }
    Ok(info.decimals)
}

async fn account_data(wallet: &SolanaWalletAdapter, address: &str) -> Result<Vec<u8>> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = response
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or_else(invalid_account_set)?;
    if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
        return Err(invalid_account_set());
    }
    gofin_solana::token::decode_rpc_account_data(
        account.get("data").ok_or_else(invalid_account_set)?,
    )
}

async fn account_lamports(wallet: &SolanaWalletAdapter, address: &str) -> Result<u64> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let account = response
        .get("value")
        .filter(|value| !value.is_null())
        .ok_or_else(invalid_account_set)?;
    if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
        return Err(invalid_account_set());
    }
    account
        .get("lamports")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_account_set)
}

async fn account_lamports_if_present(
    wallet: &SolanaWalletAdapter,
    address: &str,
) -> Result<Option<u64>> {
    gofin_solana::parse_address(address)?;
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getAccountInfo",
            json!([address,{"encoding":"base64","commitment":"finalized"}]),
        )
        .await?;
    let Some(account) = response.get("value").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if account.get("owner").and_then(Value::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
        return Err(invalid_account_set());
    }
    account
        .get("lamports")
        .and_then(Value::as_u64)
        .map(Some)
        .ok_or_else(invalid_account_set)
}

async fn minimum_account_rent(wallet: &SolanaWalletAdapter, space: usize) -> Result<u64> {
    let response = wallet
        .runtime()
        .rpc()
        .request(
            "getMinimumBalanceForRentExemption",
            json!([space,{"commitment":"finalized"}]),
        )
        .await?;
    response
        .as_u64()
        .filter(|rent| *rent > 0)
        .ok_or_else(invalid_account_set)
}

fn proof_lamports(proof: &FinalizedReceiptProof, address: &str, pre: bool) -> Result<u64> {
    let index = proof
        .account_keys
        .iter()
        .position(|key| key == address)
        .ok_or_else(receipt_mismatch)?;
    let values = if pre {
        &proof.pre_balances_lamports
    } else {
        &proof.post_balances_lamports
    };
    values.get(index).copied().ok_or_else(receipt_mismatch)
}

fn aggregate_native_credits(
    effects: &mut Vec<gofin_domain::FinancialEffect>,
    resource: &ResourceId,
) -> Result<()> {
    let native_asset = asset("native", 9);
    let mut total = gofin_domain::Quantity::zero();
    let mut retained = Vec::with_capacity(effects.len());
    for effect in std::mem::take(effects) {
        match effect {
            gofin_domain::FinancialEffect::UsableCredit { to, amount }
                if to == *resource && amount.asset == native_asset =>
            {
                total = total.checked_add(&amount.atoms)?;
            }
            other => retained.push(other),
        }
    }
    if !total.is_zero() {
        retained.push(gofin_domain::FinancialEffect::UsableCredit {
            to: resource.clone(),
            amount: Amount {
                asset: native_asset,
                atoms: total,
            },
        });
    }
    *effects = retained;
    Ok(())
}

fn address_field(data: &[u8], start: usize) -> Result<String> {
    let bytes = data
        .get(start..start + 32)
        .ok_or_else(invalid_account_set)?;
    let text = bs58::encode(bytes).into_string();
    gofin_solana::parse_address(&text)?;
    Ok(text)
}

fn read_u64(data: &[u8], start: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(start..start + 8)
            .ok_or_else(invalid_account_set)?
            .try_into()
            .map_err(|_| invalid_account_set())?,
    ))
}

fn read_u128(data: &[u8], start: usize) -> Result<u128> {
    Ok(u128::from_le_bytes(
        data.get(start..start + 16)
            .ok_or_else(invalid_account_set)?
            .try_into()
            .map_err(|_| invalid_account_set())?,
    ))
}

fn read_i64(data: &[u8], start: usize) -> Result<i64> {
    Ok(i64::from_le_bytes(
        data.get(start..start + 8)
            .ok_or_else(invalid_account_set)?
            .try_into()
            .map_err(|_| invalid_account_set())?,
    ))
}

fn pool_max_request_execution_seconds(
    data: &[u8],
    name_len: usize,
    custody_count: usize,
) -> Result<u64> {
    if name_len == 0 || name_len > 64 || custody_count == 0 || custody_count > 32 {
        return Err(invalid_account_set());
    }
    let custody_start = 12usize
        .checked_add(name_len)
        .and_then(|value| value.checked_add(4))
        .ok_or_else(invalid_account_set)?;
    let custody_end = custody_start
        .checked_add(
            custody_count
                .checked_mul(32)
                .ok_or_else(invalid_account_set)?,
        )
        .ok_or_else(invalid_account_set)?;
    // Pinned Pool layout: aumUsd (u128), Limit (40 bytes), Fees (72 bytes),
    // PoolApr (24 bytes), then maxRequestExecutionSec (i64).
    let timeout_offset = custody_end
        .checked_add(16 + 40 + 72 + 24)
        .ok_or_else(invalid_account_set)?;
    let seconds = read_i64(data, timeout_offset)?;
    if seconds <= 0 {
        return Err(invalid_account_set());
    }
    u64::try_from(seconds).map_err(|_| invalid_account_set())
}

fn pool_swap_fee_bps(data: &[u8], name_len: usize, custody_count: usize) -> Result<u64> {
    if name_len == 0 || name_len > 64 || custody_count == 0 || custody_count > 32 {
        return Err(invalid_account_set());
    }
    let custody_end = 12usize
        .checked_add(name_len)
        .and_then(|value| value.checked_add(4))
        .and_then(|value| value.checked_add(custody_count.checked_mul(32)?))
        .ok_or_else(invalid_account_set)?;
    // Fees.swapBps is the normal fee; taxBps is the maximum additional
    // utilization tax charged by the same pool fee schedule.
    let fees_start = custody_end
        .checked_add(16 + 40)
        .ok_or_else(invalid_account_set)?;
    read_u64(data, fees_start + 24)?
        .checked_add(read_u64(data, fees_start + 32)?)
        .ok_or_else(invalid_account_set)
}

fn read_u32(data: &[u8], start: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(start..start + 4)
            .ok_or_else(invalid_account_set)?
            .try_into()
            .map_err(|_| invalid_account_set())?,
    ))
}

fn account_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("account:{name}").as_bytes());
    digest[..8].try_into().expect("eight byte discriminator")
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
enum KeeperEvent {
    PreSwap {
        request: String,
        transfer_amount: u64,
        collateral_before_swap: u64,
    },
    Increase {
        position: String,
        request: String,
        owner: String,
        side: u8,
        size_delta_usd: u64,
        collateral_delta_usd: u64,
        collateral_delta_token: u64,
        position_size_usd: u64,
        request_change: u8,
        request_type: u8,
        fee_token: u64,
        fee_usd: u64,
        position_fee_usd: u64,
        funding_fee_usd: u64,
        price_impact_fee_usd: u64,
        price: u64,
    },
    Decrease {
        position: String,
        request: String,
        owner: String,
        side: u8,
        size_delta_usd: u64,
        transfer_amount_usd: u64,
        transfer_token: Option<u64>,
        position_size_usd: u64,
        request_change: u8,
        request_type: u8,
        pnl_usd: u64,
        has_profit: bool,
        fee_usd: u64,
        position_fee_usd: u64,
        funding_fee_usd: u64,
        price_impact_fee_usd: u64,
        price: u64,
        original_position_collateral_usd: u64,
        position_collateral_usd: u64,
    },
}

/// Decode only pinned Perps event payloads emitted while the Perps program is
/// the active log frame. Other program data is deliberately ignored.
fn keeper_events(logs: &[String]) -> Result<Vec<KeeperEvent>> {
    let mut stack = Vec::<String>::new();
    let mut events = Vec::new();
    for line in logs {
        if let Some(rest) = line
            .strip_prefix("Program ")
            .filter(|rest| !rest.starts_with("data: "))
        {
            if let Some((program, suffix)) = rest.split_once(" invoke [") {
                let depth = suffix
                    .strip_suffix(']')
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(invalid_account_set)?;
                if depth == 0 || depth > 32 {
                    return Err(invalid_account_set());
                }
                stack.truncate(depth - 1);
                stack.push(program.to_owned());
                continue;
            }
            if let Some((program, suffix)) = rest.split_once(' ')
                && (suffix == "success" || suffix.starts_with("failed:"))
            {
                if stack.last().is_some_and(|active| active == program) {
                    stack.pop();
                } else {
                    return Err(invalid_account_set());
                }
            }
            continue;
        }
        if stack.last().map(String::as_str) != Some(crate::jlp::PERPS_PROGRAM_ID) {
            continue;
        }
        let Some(encoded) = line.strip_prefix("Program data: ") else {
            continue;
        };
        if encoded.len() > 4096 {
            return Err(invalid_account_set());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| invalid_account_set())?;
        if let Some(event) = decode_keeper_event(&bytes)? {
            events.push(event);
        }
    }
    Ok(events)
}

fn decode_keeper_event(data: &[u8]) -> Result<Option<KeeperEvent>> {
    if data.len() < 8 {
        return Ok(None);
    }
    let increase = event_discriminator("IncreasePositionEvent");
    let decrease = event_discriminator("DecreasePositionEvent");
    let pre_swap = event_discriminator("IncreasePositionPreSwapEvent");
    if data[..8] == pre_swap {
        let mut c = BorshCursor::new(&data[8..]);
        let request = c.key()?;
        let transfer_amount = c.u64()?;
        let collateral_before_swap = c.u64()?;
        c.finish()?;
        return Ok(Some(KeeperEvent::PreSwap {
            request,
            transfer_amount,
            collateral_before_swap,
        }));
    }
    if data[..8] == increase {
        // Pinned IDL field order: position, side, custody, collateral custody,
        // size, mint, request, request mint, request kind, request type,
        // collateral delta, owner, pool, size delta, collateral USD delta,
        // collateral token delta, price, slippage, fee token, fee USD.
        let mut c = BorshCursor::new(&data[8..]);
        let position = c.key()?;
        let side = c.u8()?;
        let _custody = c.key()?;
        let _collateral_custody = c.key()?;
        let position_size_usd = c.u64()?;
        let _mint = c.key()?;
        let request = c.key()?;
        let _request_mint = c.key()?;
        let request_change = c.u8()?;
        let request_type = c.u8()?;
        let _request_collateral = c.u64()?;
        let owner = c.key()?;
        let _pool = c.key()?;
        let size_delta_usd = c.u64()?;
        let collateral_delta_usd = c.u64()?;
        let collateral_delta_token = c.u64()?;
        let price = c.u64()?;
        c.option_u64()?;
        let fee_token = c.u64()?;
        let fee_usd = c.u64()?;
        c.take(8)?; // openTime
        c.option_key()?;
        let position_fee_usd = c.u64()?;
        let funding_fee_usd = c.u64()?;
        let price_impact_fee_usd = c.u64()?;
        c.finish()?;
        if !(1..=2).contains(&side) {
            return Err(invalid_account_set());
        }
        return Ok(Some(KeeperEvent::Increase {
            position,
            request,
            owner,
            side,
            size_delta_usd,
            collateral_delta_usd,
            collateral_delta_token,
            position_size_usd,
            request_change,
            request_type,
            fee_token,
            fee_usd,
            position_fee_usd,
            funding_fee_usd,
            price_impact_fee_usd,
            price,
        }));
    }
    if data[..8] == decrease {
        // Pinned IDL field order through feeUsd; remaining fields are the
        // recorded keeper fee attribution and original-position context.
        let mut c = BorshCursor::new(&data[8..]);
        let position = c.key()?;
        let side = c.u8()?;
        let _custody = c.key()?;
        let _collateral_custody = c.key()?;
        let position_size_usd = c.u64()?;
        let _mint = c.key()?;
        let request = c.key()?;
        let _request_mint = c.key()?;
        let request_change = c.u8()?;
        let request_type = c.u8()?;
        let has_profit = c.bool()?;
        let pnl_usd = c.u64()?;
        let owner = c.key()?;
        let _pool = c.key()?;
        let size_delta_usd = c.u64()?;
        let transfer_amount_usd = c.u64()?;
        let transfer_token = c.option_u64()?;
        let price = c.u64()?;
        c.option_u64()?;
        let fee_usd = c.u64()?;
        c.take(8)?; // openTime
        c.option_key()?;
        let position_fee_usd = c.u64()?;
        let funding_fee_usd = c.u64()?;
        let price_impact_fee_usd = c.u64()?;
        let original_position_collateral_usd = c.u64()?;
        let position_collateral_usd = c.u64()?;
        c.take(16)?; // positionOpenTime, positionPrice
        c.finish()?;
        if !(1..=2).contains(&side) {
            return Err(invalid_account_set());
        }
        return Ok(Some(KeeperEvent::Decrease {
            position,
            request,
            owner,
            side,
            size_delta_usd,
            transfer_amount_usd,
            transfer_token,
            position_size_usd,
            request_change,
            request_type,
            pnl_usd,
            has_profit,
            fee_usd,
            position_fee_usd,
            funding_fee_usd,
            price_impact_fee_usd,
            price,
            original_position_collateral_usd,
            position_collateral_usd,
        }));
    }
    Ok(None)
}

fn event_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("event:{name}").as_bytes());
    digest[..8].try_into().expect("eight byte discriminator")
}

fn request_instruction_matches(
    proof: &FinalizedReceiptProof,
    request: &str,
    position: &str,
    wallet: &str,
    increase: bool,
) -> Result<bool> {
    let name = if increase {
        "createIncreasePositionMarketRequest"
    } else {
        "createDecreasePositionMarketRequest"
    };
    let expected = instruction_discriminator(name);
    let matches = proof
        .message_instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .filter_map(|ix| {
            let data = base64::engine::general_purpose::STANDARD
                .decode(&ix.data_base64)
                .ok()?;
            (data.get(..8) == Some(expected.as_slice())).then_some(ix)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Ok(false);
    }
    let ix = matches[0];
    Ok(ix
        .accounts
        .first()
        .is_some_and(|a| a.address == wallet && a.signer)
        && ix.accounts.get(4).is_some_and(|a| a.address == position)
        && ix.accounts.get(5).is_some_and(|a| a.address == request))
}

fn keeper_instruction_matches(
    proof: &FinalizedReceiptProof,
    request: &str,
    position: &str,
    increase: bool,
) -> Result<bool> {
    let names: &[&str] = if increase {
        &[
            "increasePosition4",
            "increasePositionPreSwap",
            "increasePositionWithInternalSwap",
        ]
    } else {
        &["decreasePosition4", "decreasePositionWithInternalSwap"]
    };
    let allowed = names
        .iter()
        .map(|name| instruction_discriminator(name))
        .collect::<Vec<_>>();
    let matches = proof
        .message_instructions
        .iter()
        .filter(|ix| ix.program_id == crate::jlp::PERPS_PROGRAM_ID)
        .filter_map(|ix| {
            let data = base64::engine::general_purpose::STANDARD
                .decode(&ix.data_base64)
                .ok()?;
            allowed
                .iter()
                .any(|disc| data.get(..8) == Some(disc.as_slice()))
                .then_some(ix)
        })
        .collect::<Vec<_>>();
    Ok(matches.len() == 1
        && matches[0]
            .accounts
            .iter()
            .any(|meta| meta.address == request)
        && matches[0]
            .accounts
            .iter()
            .any(|meta| meta.address == position))
}

// These explicit parameters are the exact keeper event identity and expected
// request economics, checked by this small pure predicate.
#[allow(clippy::too_many_arguments)]
fn keeper_event_matches(
    event: &KeeperEvent,
    request: &str,
    position: &str,
    wallet: &str,
    side: u8,
    increase: bool,
    request_type: u8,
    size: u64,
    _collateral: u64,
) -> bool {
    match event {
        KeeperEvent::PreSwap { .. } => false,
        KeeperEvent::Increase {
            position: p,
            request: r,
            owner,
            side: s,
            size_delta_usd,
            collateral_delta_token,
            request_change,
            request_type: observed_type,
            ..
        } => {
            increase
                && p == position
                && r == request
                && owner == wallet
                && *s == side
                && *size_delta_usd == size
                && *collateral_delta_token > 0
                && *request_change == 1
                && *observed_type == request_type
        }
        KeeperEvent::Decrease {
            position: p,
            request: r,
            owner,
            side: s,
            size_delta_usd,
            transfer_token,
            request_change,
            request_type: observed_type,
            ..
        } => {
            !increase
                && p == position
                && r == request
                && owner == wallet
                && *s == side
                && *size_delta_usd == size
                && transfer_token.is_some_and(|amount| amount > 0)
                && *request_change == 2
                && *observed_type == request_type
        }
    }
}

fn fee_breakdown_total(event: &KeeperEvent) -> Result<u64> {
    let (position, funding, impact) = match event {
        KeeperEvent::Increase {
            position_fee_usd,
            funding_fee_usd,
            price_impact_fee_usd,
            ..
        }
        | KeeperEvent::Decrease {
            position_fee_usd,
            funding_fee_usd,
            price_impact_fee_usd,
            ..
        } => (*position_fee_usd, *funding_fee_usd, *price_impact_fee_usd),
        KeeperEvent::PreSwap { .. } => return Err(receipt_mismatch()),
    };
    position
        .checked_add(funding)
        .and_then(|total| total.checked_add(impact))
        .ok_or_else(receipt_mismatch)
}

fn event_fee_usd(event: &KeeperEvent) -> Result<u64> {
    match event {
        KeeperEvent::Increase { fee_usd, .. } | KeeperEvent::Decrease { fee_usd, .. } => {
            Ok(*fee_usd)
        }
        KeeperEvent::PreSwap { .. } => Err(receipt_mismatch()),
    }
}

fn signed_u128_delta(before: u128, after: u128) -> Result<String> {
    if after >= before {
        Ok((after - before).to_string())
    } else {
        Ok(format!("-{}", before - after))
    }
}

fn ceil_bps(amount: u64, bps: u64) -> Result<u64> {
    let product = u128::from(amount)
        .checked_mul(u128::from(bps))
        .ok_or_else(bad_response)?;
    let result = product.checked_add(9_999).ok_or_else(bad_response)? / 10_000;
    u64::try_from(result).map_err(|_| bad_response())
}

fn validate_quote_fee_bounds(
    quote: &Value,
    increasing: bool,
    position_fee_ceiling: u64,
    borrow_fee_ceiling: u64,
    total_fee_ceiling: u64,
) -> Result<()> {
    let borrow_fee = parse_u64_text(quote, "outstandingBorrowFeeUsd")?;
    let position_fee = parse_u64_text(
        quote,
        if increasing {
            "openFeeUsd"
        } else {
            "closeFeeUsd"
        },
    )?;
    let position_limit = if increasing {
        total_fee_ceiling
    } else {
        position_fee_ceiling
    };
    if borrow_fee > borrow_fee_ceiling || position_fee > position_limit {
        return Err(GoFinError::validation(
            "The provider quote's position or accrued-borrow fee exceeds the independently derived pinned-custody fee bound.",
        ));
    }
    Ok(())
}

fn borrow_fee_ceiling_usd(
    request: &MarketRequest,
    position: Option<&VerifiedPosition>,
    _requested_size_usd_atoms: u64,
    _increasing: bool,
) -> Result<u64> {
    let max_seconds = request.max_request_execution_seconds;
    if max_seconds == 0 || max_seconds > 86_400 {
        return Err(invalid_account_set());
    }
    let Some(position) = position else {
        // A new position takes its interest snapshot when the keeper creates
        // it; no pre-existing borrow accrues while the request is pending.
        return Ok(0);
    };
    let now = Utc::now().timestamp();
    let elapsed = now
        .checked_sub(request.borrow_last_update)
        .unwrap_or(0)
        .max(0)
        .checked_add(i64::try_from(max_seconds).map_err(|_| invalid_account_set())?)
        .ok_or_else(invalid_account_set)?;
    let projected_interest = div_ceil_u128(
        request
            .borrow_hourly_rate
            .checked_mul(u128::try_from(elapsed).map_err(|_| invalid_account_set())?)
            .ok_or_else(invalid_account_set)?,
        3_600,
    )?;
    let cumulative = request
        .borrow_cumulative_interest
        .checked_add(projected_interest)
        .ok_or_else(invalid_account_set)?;
    let interest_delta = cumulative
        .checked_sub(position.cumulative_interest_snapshot)
        .ok_or_else(invalid_account_set)?;
    let fee = div_ceil_u128(
        interest_delta
            .checked_mul(u128::from(position.size_usd_atoms))
            .ok_or_else(invalid_account_set)?,
        1_000_000_000,
    )?;
    u64::try_from(fee).map_err(|_| invalid_account_set())
}

fn custody_hourly_funding_rate(data: &[u8]) -> Result<u128> {
    const RATE_POWER: u128 = 1_000_000_000;
    const DBPS_POWER: u128 = 100_000;
    const BPS_POWER: u128 = 10_000;
    const HOURS_IN_YEAR: u128 = 8_760;
    const FUNDING_RATE_STATE: usize = 262;
    const JUMP_RATE_STATE: usize = 352;
    const OWNED: usize = 222;
    const LOCKED: usize = 230;
    const DEBT: usize = 604;
    const BORROW_INTEREST: usize = 620;
    if data.len() < 684 {
        return Err(invalid_account_set());
    }
    let hourly_dbps = read_u64(data, FUNDING_RATE_STATE + 24)?;
    let owned_assets = u128::from(read_u64(data, OWNED)?);
    let locked_assets = u128::from(read_u64(data, LOCKED)?);
    let debt = read_u128(data, DEBT)?;
    let debt_interest = read_u128(data, BORROW_INTEREST)?;
    let net_debt = div_ceil_u128(debt.saturating_sub(debt_interest), RATE_POWER)?;
    let owned = owned_assets
        .checked_add(net_debt)
        .ok_or_else(invalid_account_set)?;
    let locked = locked_assets
        .checked_add(net_debt)
        .ok_or_else(invalid_account_set)?;
    if hourly_dbps > 0 {
        let hourly = u128::from(hourly_dbps)
            .checked_mul(RATE_POWER)
            .ok_or_else(invalid_account_set)?
            / DBPS_POWER;
        if owned == 0 || locked == 0 {
            return Ok(0);
        }
        return div_ceil_u128(
            locked.checked_mul(hourly).ok_or_else(invalid_account_set)?,
            owned,
        );
    }
    // In Jump mode the utilization curve is bounded by maxRateBps. Using the
    // published maximum is conservative for any utilization during the
    // request's on-chain execution window.
    let max_rate_bps = u128::from(read_u64(data, JUMP_RATE_STATE + 8)?);
    div_ceil_u128(
        max_rate_bps
            .checked_mul(RATE_POWER)
            .ok_or_else(invalid_account_set)?,
        BPS_POWER
            .checked_mul(HOURS_IN_YEAR)
            .ok_or_else(invalid_account_set)?,
    )
}

fn div_ceil_u128(numerator: u128, denominator: u128) -> Result<u128> {
    if denominator == 0 {
        return Err(invalid_account_set());
    }
    numerator
        .checked_add(denominator - 1)
        .map(|value| value / denominator)
        .ok_or_else(invalid_account_set)
}

fn receipt_mismatch() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_SETTLEMENT_MISMATCH",
        "reconciliation",
        "Finalized Perps evidence does not match the approved request, keeper event and resulting position.",
        "Keep this operation unresolved and inspect its request PDA and finalized transaction history; do not submit another request.",
    )
}

fn receipt_mismatch_at(reason: &'static str) -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_SETTLEMENT_MISMATCH",
        "reconciliation",
        format!("Finalized Perps evidence failed a safe reconciliation check: {reason}."),
        "Keep this operation unresolved and inspect its request PDA and finalized transaction history; do not submit another request.",
    )
}

struct BorshCursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> BorshCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(invalid_account_set)?;
        let value = self
            .data
            .get(self.offset..end)
            .ok_or_else(invalid_account_set)?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid_account_set()),
        }
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| invalid_account_set())?,
        ))
    }
    fn key(&mut self) -> Result<String> {
        let value = bs58::encode(self.take(32)?).into_string();
        gofin_solana::parse_address(&value)?;
        Ok(value)
    }
    fn option_u64(&mut self) -> Result<Option<u64>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err(invalid_account_set()),
        }
    }
    fn option_bool(&mut self) -> Result<Option<bool>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.bool()?)),
            _ => Err(invalid_account_set()),
        }
    }
    fn option_key(&mut self) -> Result<Option<String>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.key()?)),
            _ => Err(invalid_account_set()),
        }
    }
    fn finish(&self) -> Result<()> {
        if self.offset == self.data.len() {
            Ok(())
        } else {
            Err(invalid_account_set())
        }
    }
    fn finish_zero_padding(&self) -> Result<()> {
        if self.data[self.offset..].iter().all(|byte| *byte == 0) {
            Ok(())
        } else {
            Err(invalid_account_set())
        }
    }
}

fn validate_network(input: &Value) -> Result<()> {
    if input.get("network").and_then(Value::as_str) != Some("solana")
        || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
    {
        return Err(GoFinError::validation(
            "Jupiter Perps reads require Solana mainnet-beta.",
        ));
    }
    Ok(())
}

fn normalize_market_stats(market: &str, mint: &str, response: Value) -> Result<Value> {
    let fields = [
        ("price", "price_usd", false),
        ("priceChange24H", "price_change_24h_percent", true),
        ("priceHigh24H", "price_high_24h_usd", false),
        ("priceLow24H", "price_low_24h_usd", false),
        ("volume", "volume_24h_usd", false),
    ];
    let mut normalized = Map::new();
    normalized.insert("market".into(), json!(market));
    normalized.insert("mint".into(), json!(mint));
    for (provider_key, output_key, signed) in fields {
        let value = response
            .get(provider_key)
            .and_then(Value::as_str)
            .filter(|value| valid_provider_decimal(value, signed))
            .ok_or_else(invalid_market_stats)?;
        normalized.insert(output_key.into(), json!(value));
    }
    Ok(Value::Object(normalized))
}

fn normalize_collateral_limits(
    position: &str,
    input_token: &str,
    response: Value,
) -> Result<Value> {
    let fields = [
        ("maxCollateralDepositTokenAmount", "max_deposit_token_atoms"),
        ("maxCollateralDepositUsd", "max_deposit_usd_atoms"),
        ("minCollateralDepositTokenAmount", "min_deposit_token_atoms"),
        ("minCollateralDepositUsd", "min_deposit_usd_atoms"),
        (
            "maxCollateralWithdrawalTokenAmount",
            "max_withdrawal_token_atoms",
        ),
        ("maxCollateralWithdrawalUsd", "max_withdrawal_usd_atoms"),
        (
            "minCollateralWithdrawalTokenAmount",
            "min_withdrawal_token_atoms",
        ),
        ("minCollateralWithdrawalUsd", "min_withdrawal_usd_atoms"),
    ];
    let mut normalized = Map::new();
    normalized.insert("position".into(), json!(position));
    normalized.insert("input_token".into(), json!(input_token));
    for (provider_key, output_key) in fields {
        let value = response
            .get(provider_key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(invalid_collateral_limits)?;
        normalized.insert(output_key.into(), json!(value));
    }
    Ok(Value::Object(normalized))
}

fn valid_provider_decimal(value: &str, signed: bool) -> bool {
    let digits = value.strip_prefix('-').filter(|_| signed).unwrap_or(value);
    if digits.is_empty() {
        return false;
    }
    let mut parts = digits.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(fraction) => {
            !fraction.is_empty()
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && parts.next().is_none()
        }
    }
}

fn invalid_market_stats() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_MARKET_STATS_INVALID",
        "response",
        "Jupiter Perps returned market statistics outside the documented decimal-string schema.",
        "Retry the read later; do not use malformed provider values to authorize a financial action.",
    )
}

fn invalid_collateral_limits() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_COLLATERAL_LIMITS_INVALID",
        "response",
        "Jupiter Perps returned collateral limits outside the documented atomic-string schema.",
        "Retry the read later; do not use malformed provider limits to authorize a financial action.",
    )
}

fn position_lock_resource(position: &str) -> ResourceId {
    ResourceId(format!(
        "solana:mainnet-beta:jupiter-perps-position:{position}"
    ))
}

fn is_increase_operation(operation: &str) -> bool {
    operation == OP_INCREASE || operation == OP_COLLATERAL_ADD
}

fn is_collateral_only_operation(operation: &str) -> bool {
    operation == OP_COLLATERAL_ADD || operation == OP_COLLATERAL_REMOVE
}

fn is_market_write_operation(operation: &str) -> bool {
    matches!(
        operation,
        OP_INCREASE | OP_DECREASE | OP_COLLATERAL_ADD | OP_COLLATERAL_REMOVE
    )
}

pub fn operation_definitions() -> Vec<Definition> {
    let mut definitions = [OP_POSITIONS, OP_MARKET_STATS, OP_COLLATERAL_LIMITS]
        .into_iter()
        .map(|name| Definition {
            name: name.into(),
            description: if name == OP_POSITIONS {
                "Read verified Jupiter Perps long and short exposure for an enrolled wallet.".into()
            } else if name == OP_MARKET_STATS {
                "Read typed Jupiter Perps market statistics for SOL, ETH or BTC.".into()
            } else {
                "Read the provider's collateral deposit and withdrawal limits for an exact position and input token.".into()
            },
            kind: OperationKind::Read,
            execution_actor: ExecutionActor::AgentReadable,
            input_schema: if name == OP_POSITIONS {
                json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1}},"required":["network","cluster","wallet"],"additionalProperties":false})
            } else if name == OP_MARKET_STATS {
                json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"market":{"enum":["SOL","ETH","BTC"]}},"required":["network","cluster","market"],"additionalProperties":false})
            } else {
                json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"position":{"type":"string","minLength":32,"maxLength":44},"input_token":{"enum":["SOL","ETH","BTC","USDC"]}},"required":["network","cluster","position","input_token"],"additionalProperties":false})
            },
            output_schema: if name == OP_POSITIONS {
                json!({
                    "type":"object",
                    "properties":{
                        "wallet":{"type":"string"},
                        "cluster":{"const":CLUSTER},
                        "positions":{"type":"array","items":{
                            "type":"object",
                            "properties":{
                                "position":{"type":"string"},
                                "side":{"enum":["long","short"]},
                                "market_mint":{"type":"string","description":"Underlying market mint; position quantity is reported separately as exact USD notional."},
                                "market_decimals":{"type":"integer","minimum":0,"maximum":18},
                                "position_usd_asset":{"const":"jupiter:position-usd:<market_mint>","description":"Non-transferable $1-denominated protocol accounting unit preserving exact venue notional, never an SPL-token quantity."},
                                "size_usd_atoms":{"type":"string","description":"USD value in 10^-6 dollar units from the on-chain Position account."},
                                "entry_price_usd_atoms":{"type":"string","description":"USD price in 10^-6 dollar units from the on-chain Position account."},
                                "collateral_mint":{"type":"string"},
                                "collateral_usd_atoms":{"type":"string","description":"On-chain collateral value in 10^-6 dollar units; token quantity is not inferred."},
                                "locked_amount_token_atoms":{"type":"string","description":"Exact locked token amount in the collateral custody token's atomic units."},
                                "realized_pnl_usd_atoms":{"type":"string","description":"Signed USD 10^-6-unit realized PnL from the on-chain Position account."}
                            },
                            "required":["position","side","market_mint","market_decimals","position_usd_asset","size_usd_atoms","entry_price_usd_atoms","collateral_mint","collateral_usd_atoms","locked_amount_token_atoms","realized_pnl_usd_atoms"],
                            "additionalProperties":false
                        }},
                        "pending_requests":{"type":"array","items":{"type":"object","properties":{"request":{"type":"string"},"position":{"type":"string"},"mint":{"type":"string"},"request_change":{"enum":["increase","decrease"]},"request_type":{"enum":["market","trigger"]},"side":{"enum":["long","short"]},"size_usd_delta_atoms":{"type":"string","pattern":"^[0-9]+$"},"collateral_delta_atoms":{"type":"string","pattern":"^[0-9]+$"},"trigger_price_usd_atoms":{"type":["string","null"]},"trigger_above_threshold":{"type":["boolean","null"]},"entire_position":{"type":["boolean","null"]},"executed":{"type":"boolean"},"counter":{"type":"string","pattern":"^[0-9]+$"},"rent_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"escrow":{"type":"object","properties":{"position":{"type":"string"},"kind":{"const":"escrow"},"asset":{"type":"object","properties":{"network":{"const":"solana:mainnet-beta"},"reference":{"type":"string","minLength":1},"decimals":{"type":"integer","minimum":0,"maximum":18}},"required":["network","reference","decimals"],"additionalProperties":false},"amount_atoms":{"type":"string","pattern":"^[1-9][0-9]*$"},"evidence":{"type":"object"}},"required":["position","kind","asset","amount_atoms","evidence"],"additionalProperties":false,"description":"Finalized exact request-owned token escrow; present for an unexecuted resting limit request."}},"required":["request","position","mint","request_change","request_type","side","size_usd_delta_atoms","collateral_delta_atoms","trigger_price_usd_atoms","trigger_above_threshold","entire_position","executed","counter","rent_lamports"],"additionalProperties":false,"description":"Exact pinned-IDL PositionRequest. Open limit escrow is separately shown as a finalized non-spendable escrow position; market or executed requests remain unresolved until finalized keeper/cancellation evidence."}}
                    },
                    "required":["wallet","cluster","positions","pending_requests"],
                    "additionalProperties":false
                })
            } else if name == OP_MARKET_STATS {
                json!({"type":"object","properties":{"market":{"enum":["SOL","ETH","BTC"]},"mint":{"type":"string"},"price_usd":{"type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$"},"price_change_24h_percent":{"type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$"},"price_high_24h_usd":{"type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$"},"price_low_24h_usd":{"type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$"},"volume_24h_usd":{"type":"string","pattern":"^-?[0-9]+(\\.[0-9]+)?$"}},"required":["market","mint","price_usd","price_change_24h_percent","price_high_24h_usd","price_low_24h_usd","volume_24h_usd"],"additionalProperties":false,"description":"Typed Jupiter Perps market statistics from the fixed provider host. Decimal values are preserved as strings."})
            } else {
                json!({"type":"object","properties":{"position":{"type":"string"},"input_token":{"enum":["SOL","ETH","BTC","USDC"]},"max_deposit_token_atoms":{"type":"string","pattern":"^[0-9]+$"},"max_deposit_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},"min_deposit_token_atoms":{"type":"string","pattern":"^[0-9]+$"},"min_deposit_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},"max_withdrawal_token_atoms":{"type":"string","pattern":"^[0-9]+$"},"max_withdrawal_usd_atoms":{"type":"string","pattern":"^[0-9]+$"},"min_withdrawal_token_atoms":{"type":"string","pattern":"^[0-9]+$"},"min_withdrawal_usd_atoms":{"type":"string","pattern":"^[0-9]+$"}},"required":["position","input_token","max_deposit_token_atoms","max_deposit_usd_atoms","min_deposit_token_atoms","min_deposit_usd_atoms","max_withdrawal_token_atoms","max_withdrawal_usd_atoms","min_withdrawal_token_atoms","min_withdrawal_usd_atoms"],"additionalProperties":false})
            },
            error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples: vec![if name == OP_POSITIONS {json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet"})} else if name == OP_MARKET_STATS {json!({"network":"solana","cluster":CLUSTER,"market":"SOL"})} else {json!({"network":"solana","cluster":CLUSTER,"position":"11111111111111111111111111111111","input_token":"USDC"})}],
            coverage: if name == OP_POSITIONS { "Bounded finalized program-owned Position accounts decoded against the pinned Jupiter Perps IDL; exposure uses on-chain size and price.".into() } else if name == OP_MARKET_STATS { "Jupiter Perps market statistics from the fixed Perps API host, validated against the documented decimal-string fields.".into() } else { "Collateral limits are provider guidance for a specific position and input mint; exact atomic values are preserved.".into() },
            completion: "Read only; provider summaries do not establish wallet exposure or complete settlement.".into(),
            semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"position_account":POSITION_ACCOUNT,"candidate_indexers_are_not_exhaustive":true}),
        })
        .collect::<Vec<_>>();
    definitions.extend(collateral::operation_definitions());
    definitions.extend(tpsl::operation_definitions());
    definitions.extend([OP_INCREASE, OP_DECREASE].into_iter().map(|name| {
        let increase = name == OP_INCREASE;
        let input_schema = if increase {
            json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"market":{"enum":["SOL","ETH","BTC"]},"side":{"enum":["long","short"]},"funding_token":{"enum":["SOL","ETH","BTC","USDC"],"description":"Wallet debit token. The Perps builder may internally convert any supported input token to the market's required collateral; GoFin enforces a price-derived minimum conversion output and collateral-token fee cap."},"collateral_amount_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"Funding token atomic units: use the selected token's decimals."},"size_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"Perps position notional in USD 10^-6 units."},"maximum_protocol_fee_atoms":{"type":"string","pattern":"^[0-9]+$","description":"Maximum keeper fee withheld in the position collateral token's atomic units."},"maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[0-9]+$","description":"Maximum keeper protocol fee in USD 10^-6 units."},"maximum_slippage_bps":{"type":"integer","minimum":1,"maximum":5000},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$","description":"Required SOL lamport cap for the temporary PositionRequest PDA, any new Position PDA, and created token accounts; preview reports exact finalized rent and peak funding."}},"required":["network","cluster","wallet","market","side","funding_token","collateral_amount_atoms","size_usd_atoms","maximum_protocol_fee_atoms","maximum_protocol_fee_usd_atoms","maximum_slippage_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false})
        } else {
            json!({"type":"object","properties":{"network":{"const":"solana"},"cluster":{"const":CLUSTER},"wallet":{"type":"string","minLength":1},"position":{"type":"string","minLength":32,"maxLength":44},"receive_token":{"enum":["SOL","ETH","BTC","USDC"],"description":"Supported payout token. The venue may internally convert from the position collateral, subject to the exact approved minimum payout."},"size_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"Perps decrease notional in USD 10^-6 units."},"minimum_receive_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"Minimum payout in receive-token atomic units."},"maximum_protocol_fee_usd_atoms":{"type":"string","pattern":"^[0-9]+$","description":"Maximum keeper protocol fee in USD 10^-6 units."},"maximum_slippage_bps":{"type":"integer","minimum":1,"maximum":5000},"maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"},"maximum_account_funding_lamports":{"type":"string","pattern":"^[0-9]+$","description":"Required SOL lamport cap for the temporary PositionRequest PDA, any new Position PDA, and created token accounts; preview reports exact finalized rent and peak funding."}},"required":["network","cluster","wallet","position","receive_token","size_usd_atoms","minimum_receive_atoms","maximum_protocol_fee_usd_atoms","maximum_slippage_bps","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false})
        };
        Definition {
            name:name.into(),
            description:if increase {"Request a bounded Jupiter Perps SOL, ETH, or BTC market long or short increase.".into()} else {"Request a bounded decrease of an existing Jupiter Perps SOL, ETH, or BTC position.".into()},
            kind:OperationKind::Write,
            execution_actor:ExecutionActor::AgentInvocable,
            input_schema,
            output_schema:json!({"type":"object","properties":{"state":{"enum":["request_submitted","keeper_pending","settled"]},"request":{"type":"string","description":"Exact immutable PositionRequest PDA."},"position":{"type":"string","description":"Exact Position PDA."},"signature":{"type":"string","description":"Finalized request transaction signature when submitted."},"size_usd_atoms":{"type":"string","description":"Exact requested notional in USD 10^-6 units."},"collateral_atoms":{"type":"string","description":"Input collateral atomic units, with asset specified separately."},"collateral_asset":{"type":"string"},"settlement":{"type":"object","properties":{"keeper_signature":{"type":"string"},"keeper_slot":{"type":"integer"},"position_size_usd_atoms":{"type":"string"},"event_price_usd_atoms":{"type":"string"},"realized_pnl_usd_atoms":{"type":["string","null"],"description":"Signed USD 10^-6 event value; not token atoms."},"protocol_fee_usd_atoms":{"type":"string","description":"USD 10^-6 event value; not token atoms."},"proof_digest":{"type":"string"}},"required":["keeper_signature","keeper_slot","position_size_usd_atoms","event_price_usd_atoms","protocol_fee_usd_atoms","proof_digest"],"additionalProperties":false}},"required":["state","request","position","size_usd_atoms","collateral_atoms","collateral_asset"],"additionalProperties":false}),
            error_schema:json!({"type":"object","required":["code","stage","message","next_action"]}),
            examples:vec![if increase {json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet","market":"SOL","side":"long","funding_token":"USDC","collateral_amount_atoms":"1000000","size_usd_atoms":"2000000","maximum_protocol_fee_atoms":"10000","maximum_protocol_fee_usd_atoms":"10000","maximum_slippage_bps":100,"maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})} else {json!({"network":"solana","cluster":CLUSTER,"wallet":"wallet","position":"11111111111111111111111111111111","receive_token":"USDC","size_usd_atoms":"1000000","minimum_receive_atoms":"900000","maximum_protocol_fee_usd_atoms":"10000","maximum_slippage_bps":100,"maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"5000000"})}],
            coverage:"The exact builder request is simulated and bound to a pinned-IDL position/request PDA. Submission remains pending until an exact keeper instruction and finalized IDL event are correlated to that request.".into(),
            completion:"A provider response or request transaction is not settlement; only finalized keeper proof plus the exact on-chain position update can complete the operation.".into(),
            semantics:json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"position_account":POSITION_ACCOUNT,"request_account":POSITION_REQUEST_ACCOUNT,"request_change":if increase {"increase"} else {"decrease"},"keeper_event_required":true,"uncertain_submission":"reconcile_original_request_never_resubmit"}),
        }
    }));
    definitions.push(Definition {
        name: OP_LIMIT_UPDATE.into(),
        description: "Update the trigger price of one open Jupiter Perps limit request owned by an enrolled wallet.".into(),
        kind: OperationKind::Write,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: json!({
            "type":"object",
            "properties":{
                "network":{"const":"solana"},
                "cluster":{"const":CLUSTER},
                "wallet":{"type":"string","minLength":1},
                "request":{"anyOf":[{"type":"string","minLength":32,"maxLength":44},{"type":"null"}],"description":"Exact PositionRequest PDA from the verified create parent; null for a named-plan recovery placeholder."},
                "original_operation_id":{"anyOf":[{"type":"string","minLength":1},{"type":"null"}],"description":"Exact pending limit.create operation ID from GoFin status; null for a named-plan recovery placeholder."},
                "original_submission":{"anyOf":[{"type":"object","properties":{"kind":{"type":"string"},"reference":{"type":"string"},"data":{"type":"object"}},"required":["kind","reference","data"],"additionalProperties":true},{"type":"null"}],"description":"Optional exact submission echo; the adapter loads the authoritative original submission from its journal."},
                "trigger_price_usd_atoms":{"type":"string","pattern":"^[1-9][0-9]*$","description":"New trigger price in USD 10^-6 units."},
                "maximum_network_fee_lamports":{"type":"string","pattern":"^[1-9][0-9]*$"}
            },
            "required":["network","cluster","wallet","request","original_operation_id","trigger_price_usd_atoms","maximum_network_fee_lamports"],
            "additionalProperties":false
        }),
        output_schema: json!({
            "type":"object",
            "properties":{
                "state":{"const":"updated"},
                "request":{"type":"string"},
                "original_operation_id":{"type":"string"},
                "previous_trigger_price_usd_atoms":{"type":"string"},
                "trigger_price_usd_atoms":{"type":"string"},
                "signature":{"type":"string"},
                "slot":{"type":"integer"},
                "proof_digest":{"type":"string"}
            },
            "required":["state","request","original_operation_id","previous_trigger_price_usd_atoms","trigger_price_usd_atoms","signature","slot","proof_digest"],
            "additionalProperties":false
        }),
        error_schema: json!({"type":"object","required":["code","stage","message","next_action"]}),
        examples: vec![json!({"network":"solana","cluster":CLUSTER,"wallet":"my-solana-wallet","request":"11111111111111111111111111111111","original_operation_id":"operation-id-from-gofin-status","trigger_price_usd_atoms":"150000000","maximum_network_fee_lamports":"10000"})],
        coverage: "Provider transaction is checked against the pinned instantUpdateLimitOrder instruction and exact open PositionRequest PDA; the updated on-chain trigger must match before completion.".into(),
        completion: "Completed only after finalized transaction proof and a finalized PositionRequest read prove the exact new trigger price.".into(),
        semantics: json!({"network":NETWORK,"cluster":CLUSTER,"program":crate::jlp::PERPS_PROGRAM_ID,"instruction":"instantUpdateLimitOrder","request_account":POSITION_REQUEST_ACCOUNT,"parent_operation":OP_LIMIT_CREATE,"parent_binding":"authoritative durable create submission; caller-supplied binding is ignored","order_lifecycle":"update child never settles or releases original create escrow reservation; original create record owns future fill or cancellation","uncertain_submission":"reconcile_original_identity_never_retry"}),
    });
    definitions.push(limit_create::operation_definition());
    definitions.push(cancel::operation_definition());
    definitions
}

fn unknown() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_OPERATION_UNKNOWN",
        "operation",
        "This Jupiter Perps adapter supports only reviewed read operations.",
        "Inspect GoFin discovery for the implemented operations.",
    )
}

fn permission_unsupported() -> GoFinError {
    GoFinError::new(
        "PLAN_PERMISSION_UNSUPPORTED",
        "authorization",
        "Jupiter Perps finite permissions support only the reviewed market increase and decrease operations.",
        "Use an exact per-operation approval for this Perps action.",
    )
}

async fn permission_limit_update(
    adapter: &PerpsAdapter,
    input: &Value,
    bounds: &Value,
) -> Result<PlanPermission> {
    validate_network(input)?;
    let wallet = adapter.wallet(input)?;
    let recovery_step = bounds
        .get("recovery_parent_step_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let supplied_parent = input
        .get("original_operation_id")
        .is_some_and(|v| !v.is_null())
        || input
            .get("original_submission")
            .is_some_and(|v| !v.is_null());
    let (parent_id, parent_submission, recovery_parent) = match recovery_step {
        Some(_step) if supplied_parent => {
            return Err(GoFinError::validation(
                "For a named-plan update, omit original operation and submission; GoFin binds them from the immediately preceding create step.",
            ));
        }
        Some(step) => (
            "__gofin_plan_recovery_parent__".to_owned(),
            Submission {
                kind: "plan_recovery_parent".into(),
                reference: step.to_owned(),
                data: json!({"recovery_parent":true}),
            },
            Some(gofin_domain::financial_plans::FinancialPlanRecoveryParent {
                step_id: step.to_owned(),
                operation_id_pointer: "/original_operation_id".into(),
                submission_pointer: "/original_submission".into(),
                parent_binding_fields: vec![
                    gofin_domain::financial_plans::FinancialPlanRecoveryBinding {
                        parent_binding_pointer: "/request".into(),
                        child_input_pointer: "/request".into(),
                    },
                ],
            }),
        ),
        None => {
            let request = input
                .get("request")
                .and_then(Value::as_str)
                .ok_or_else(|| GoFinError::validation("Select the exact Perps limit request."))?;
            let parent_id = input
                .get("original_operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Provide the original create operation ID from GoFin status.",
                    )
                })?;
            let (_, submission) =
                adapter.limit_create_parent(parent_id, wallet.address(), request)?;
            if let Some(echo) = input
                .get("original_submission")
                .filter(|value| !value.is_null())
            {
                let echo: Submission = serde_json::from_value(echo.clone()).map_err(|_| {
                    GoFinError::validation("The original submission echo is malformed.")
                })?;
                if echo != submission {
                    return Err(unreviewed_detail(
                        "the submission echo differs from the durable create record",
                    ));
                }
            }
            (parent_id.to_owned(), submission, None)
        }
    };
    let request = input
        .get("request")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("__gofin_plan_recovery_request__");
    if recovery_step.is_none() {
        gofin_solana::parse_address(request)?;
    }
    let max_fee = bounds
        .get("maximum_network_fee_lamports")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            GoFinError::validation("A positive Perps limit update fee cap is required.")
        })?;
    let exact_trigger = input
        .get("trigger_price_usd_atoms")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let minimum_trigger = bounds
        .get("minimum_trigger_price_usd_atoms")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .or(exact_trigger)
        .ok_or_else(|| GoFinError::validation("Set a positive minimum trigger price."))?;
    let maximum_trigger = bounds
        .get("maximum_trigger_price_usd_atoms")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .or(exact_trigger)
        .ok_or_else(|| GoFinError::validation("Set a positive maximum trigger price."))?;
    if minimum_trigger == 0 || maximum_trigger < minimum_trigger {
        return Err(GoFinError::validation(
            "The trigger price range is invalid.",
        ));
    }
    let resource = ResourceId(format!("solana:{CLUSTER}:{}", wallet.address()));
    let native = asset("native", 9);
    let recovery_mode = recovery_step.is_some();
    let spec = BoundarySpec {
        fixed: json!({
            "operation":OP_LIMIT_UPDATE,"network":NETWORK,"cluster":CLUSTER,
            "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":input["wallet"],
            "wallet_address":wallet.address(),"request":request,
            "parent_operation_id":parent_id,"parent_submission":parent_submission,
            "recovery_parent":recovery_mode
        }),
        parameters: json!({
            "minimum_trigger_price_usd_atoms":minimum_trigger.to_string(),
            "maximum_trigger_price_usd_atoms":maximum_trigger.to_string(),
            "maximum_network_fee_lamports":max_fee.to_string()
        }),
        maximum_debits: vec![],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash { resource },
            amount: Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(max_fee),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Authorize Perps limit trigger updates from {} to {}, with network fees up to {max_fee} lamports.",
            format_usd_micros(minimum_trigger),
            format_usd_micros(maximum_trigger)
        ),
    };
    Ok(PlanPermission {
        spec,
        recovery_parent,
        possible_credits: vec![],
        fee_assets: vec![native],
        deferred_receipt: None,
        authorization_seconds: Some(600),
        cumulative_scope: None,
    })
}

fn validate_limit_update_permission_candidate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let binding = &prepared.binding;
    let fixed = &spec.fixed;
    let recovery_mode = fixed.get("recovery_parent").and_then(Value::as_bool) == Some(true);
    for (key, actual) in [
        ("operation", Value::String(prepared.operation.clone())),
        ("network", json!(NETWORK)),
        ("cluster", json!(CLUSTER)),
        ("program", json!(crate::jlp::PERPS_PROGRAM_ID)),
        (
            "wallet_name",
            prepared.input.get("wallet").cloned().unwrap_or(Value::Null),
        ),
        (
            "wallet_address",
            binding
                .get("wallet_address")
                .cloned()
                .unwrap_or(Value::Null),
        ),
        (
            "parent_operation_id",
            binding
                .get("parent_operation_id")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    ] {
        if recovery_mode && key == "parent_operation_id" {
            continue;
        }
        if fixed.get(key) != Some(&actual) {
            return Err(GoFinError::validation(
                "Perps limit update differs from the authorized parent or wallet.",
            ));
        }
    }
    if !gofin_domain::product_names::recovery_placeholder(
        fixed.get("request").and_then(Value::as_str),
        "__gofin_plan_recovery_request__",
    ) && fixed.get("request") != binding.get("request")
    {
        return Err(GoFinError::validation(
            "Perps limit update request differs from the authorized order.",
        ));
    }
    if !recovery_mode && fixed.get("parent_submission") != binding.get("parent_submission") {
        return Err(GoFinError::validation(
            "Perps limit update parent submission changed.",
        ));
    }
    let trigger = parse_binding_u64(binding, "trigger_price_usd_atoms")?;
    let min = spec.parameters["minimum_trigger_price_usd_atoms"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    let max = spec.parameters["maximum_trigger_price_usd_atoms"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    let fee = parse_binding_u64(binding, "maximum_network_fee_lamports")?;
    let fee_cap = spec.parameters["maximum_network_fee_lamports"]
        .as_str()
        .ok_or_else(bad_response)?
        .parse::<u64>()
        .map_err(|_| bad_response())?;
    if trigger < min || trigger > max || fee == 0 || fee > fee_cap {
        return Err(GoFinError::validation(
            "Perps limit update exceeds its trigger or fee boundary.",
        ));
    }
    Ok(())
}

fn format_usd_micros(atoms: u64) -> String {
    let dollars = atoms / 1_000_000;
    let mut fraction = format!("{:06}", atoms % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${dollars}.{fraction}")
}

fn format_token_atoms(atoms: u64, decimals: u8) -> String {
    let scale = 10u64.checked_pow(decimals as u32);
    let Some(scale) = scale.filter(|scale| *scale > 0) else {
        return format!("{atoms} atomic units");
    };
    let whole = atoms / scale;
    let fractional = atoms % scale;
    if fractional == 0 {
        return whole.to_string();
    }
    let mut fraction = format!("{fractional:0width$}", width = decimals as usize);
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
}

fn protocol_fee_is_observed(fee_usd_micros: u64, fee_token_atoms: u64) -> bool {
    fee_usd_micros > 0 || fee_token_atoms > 0
}

#[allow(clippy::too_many_arguments)]
fn position_protocol_fee_bound(
    resource: ResourceId,
    position: String,
    margin_asset: AssetId,
    fee_atoms: gofin_domain::Quantity,
    gross_funded_increase: bool,
) -> CostBound {
    CostBound {
        location: AccountingLocation::Position {
            resource,
            position,
            position_kind: gofin_domain::PositionKind::LockedCollateral,
        },
        amount: Amount {
            asset: margin_asset,
            atoms: fee_atoms,
        },
        treatment: if gross_funded_increase {
            CostTreatment::IncludedInDebit
        } else {
            CostTreatment::AdditionalDebit
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn market_permission_summary(
    market: &str,
    side: u8,
    collateral_only: bool,
    maximum_size_atoms: u64,
    maximum_funding_atoms: u64,
    funding_symbol: &str,
    funding_decimals: u8,
    maximum_fee_usd_atoms: u64,
    maximum_fee_token_atoms: u64,
    collateral_symbol: &str,
    collateral_decimals: u8,
    maximum_slippage_bps: u16,
    maximum_network_fee_lamports: u64,
    maximum_account_funding_lamports: u64,
) -> String {
    let action = if collateral_only {
        format!(
            "Authorize a {market} {} Perps collateral adjustment using up to {} {funding_symbol}.",
            if side == 1 { "long" } else { "short" },
            format_token_atoms(maximum_funding_atoms, funding_decimals),
        )
    } else {
        format!(
            "Authorize a {market} {} Perps position up to {} notional, funded with up to {} {funding_symbol}.",
            if side == 1 { "long" } else { "short" },
            format_usd_micros(maximum_size_atoms),
            format_token_atoms(maximum_funding_atoms, funding_decimals),
        )
    };
    format!(
        "{action} Protocol fees are bounded at {} and {} {collateral_symbol}; slippage is at most {maximum_slippage_bps} bps. Reserve up to {} SOL for network fees and {} SOL for refundable account funding.",
        format_usd_micros(maximum_fee_usd_atoms),
        format_token_atoms(maximum_fee_token_atoms, collateral_decimals),
        format_token_atoms(maximum_network_fee_lamports, 9),
        format_token_atoms(maximum_account_funding_lamports, 9),
    )
}

fn invalid_account_set() -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_POSITION_INVALID",
        "snapshot",
        "The finalized Jupiter Perps account set could not be verified against the pinned IDL.",
        "Keep the snapshot incomplete and inspect the on-chain position account data.",
    )
}

fn invalid_account_set_detail(detail: &str) -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_POSITION_INVALID",
        "prepare",
        format!(
            "The finalized Jupiter Perps {detail} could not be verified against the pinned IDL."
        ),
        "Do not approve or submit this candidate; retry after the provider account data is available.",
    )
}

fn invalid_snapshot_account_set(detail: &str) -> GoFinError {
    GoFinError::new(
        "JUPITER_PERPS_POSITION_INVALID",
        "snapshot",
        format!(
            "The finalized Jupiter Perps {detail} could not be verified against the pinned IDL."
        ),
        "Keep the snapshot incomplete and inspect the on-chain position account data.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn native_refundable_credits_are_aggregated_to_the_approved_minimum() {
        let resource = ResourceId("solana:mainnet-beta:wallet".into());
        let native = asset("native", 9);
        let token = asset("USDC1111111111111111111111111111111111111", 6);
        let mut effects = vec![
            gofin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: gofin_domain::Quantity::from_u64(2_881_440),
                },
            },
            gofin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: native.clone(),
                    atoms: gofin_domain::Quantity::from_u64(2_352_480),
                },
            },
            gofin_domain::FinancialEffect::UsableCredit {
                to: resource.clone(),
                amount: Amount {
                    asset: token.clone(),
                    atoms: gofin_domain::Quantity::from_u64(10_998_000),
                },
            },
        ];

        aggregate_native_credits(&mut effects, &resource).unwrap();

        let native_credits = effects
            .iter()
            .filter_map(|effect| match effect {
                gofin_domain::FinancialEffect::UsableCredit { to, amount }
                    if to == &resource && amount.asset == native =>
                {
                    Some(amount.atoms.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(native_credits, ["5233920"]);
        assert!(effects.iter().any(|effect| matches!(effect,
            gofin_domain::FinancialEffect::UsableCredit { to, amount }
                if to == &resource && amount.asset == token && amount.atoms.as_str() == "10998000"
        )));
    }

    fn test_position_request_data(
        owner: &str,
        position: &str,
        mint: &str,
        counter: u64,
        request_change: u8,
    ) -> Vec<u8> {
        let mut data = account_discriminator(POSITION_REQUEST_ACCOUNT).to_vec();
        for key in [owner, owner, owner, position, mint] {
            data.extend_from_slice(&solana_pubkey3::Pubkey::from_str(key).unwrap().to_bytes());
        }
        data.extend_from_slice(&0i64.to_le_bytes()); // open time
        data.extend_from_slice(&0i64.to_le_bytes()); // update time
        data.extend_from_slice(&2_000_000u64.to_le_bytes());
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.push(request_change);
        data.push(0); // market request
        data.push(1); // long
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // six absent options
        data.push(0); // not executed
        data.extend_from_slice(&counter.to_le_bytes());
        data.push(0); // bump (validated through the PDA identity)
        data.push(0); // absent referral
        data.resize(POSITION_REQUEST_ACCOUNT_SPACE, 0);
        data
    }

    #[test]
    fn position_request_decoder_binds_owner_pda_and_pinned_fields() {
        let owner = solana_pubkey3::Pubkey::new_unique().to_string();
        let position = solana_pubkey3::Pubkey::new_unique().to_string();
        let mint = crate::swap::WRAPPED_SOL_MINT.to_owned();
        let counter = 7;
        let address = position_request_pda(&position, counter, 1).unwrap();
        let data = test_position_request_data(&owner, &position, &mint, counter, 1);

        let request = decode_position_request(&owner, &address, &data).unwrap();
        assert_eq!(request.address, address);
        assert_eq!(request.position, position);
        assert_eq!(request.mint, mint);
        assert_eq!(request.size_usd_delta, 2_000_000);
        assert_eq!(request.collateral_delta, 1_000_000);
        assert_eq!(request.read_value()["request_change"], "increase");
        let mut resting = request.clone();
        resting.request_type = 1;
        assert!(resting.is_resting_limit());
        let mut executed = request.clone();
        executed.executed = true;
        assert!(!executed.is_resting_limit());
        let mut market = request.clone();
        market.request_type = 0;
        assert!(!market.is_resting_limit());

        let other_owner = solana_pubkey3::Pubkey::new_unique().to_string();
        assert!(decode_position_request(&other_owner, &address, &data).is_err());
        let wrong_address = position_request_pda(&position, counter + 1, 1).unwrap();
        assert!(decode_position_request(&owner, &wrong_address, &data).is_err());

        let mut invalid_enum = data;
        // The request_change byte follows discriminator, five pubkeys, two timestamps and two u64s.
        invalid_enum[8 + 5 * 32 + 16 + 16] = 9;
        assert!(decode_position_request(&owner, &address, &invalid_enum).is_err());
    }

    #[test]
    fn limit_update_instruction_binds_exact_request_and_parameters() {
        use gofin_solana::transaction::{NormalizedAccountMeta as Meta, NormalizedInstruction};

        let wallet = solana_pubkey3::Pubkey::new_unique().to_string();
        let pool = solana_pubkey3::Pubkey::new_unique().to_string();
        let position = solana_pubkey3::Pubkey::new_unique().to_string();
        let request = solana_pubkey3::Pubkey::new_unique().to_string();
        let custody = solana_pubkey3::Pubkey::new_unique().to_string();
        let mut data = instruction_discriminator("instantUpdateLimitOrder").to_vec();
        data.extend_from_slice(&2_000_000u64.to_le_bytes());
        data.extend_from_slice(&151_000_000u64.to_le_bytes());
        data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        let mut accounts = vec![
            Meta {
                address: solana_pubkey3::Pubkey::new_unique().to_string(),
                signer: true,
                writable: false,
            },
            Meta {
                address: solana_pubkey3::Pubkey::new_unique().to_string(),
                signer: true,
                writable: false,
            },
            Meta {
                address: wallet.clone(),
                signer: true,
                writable: true,
            },
            Meta {
                address: pda(&[b"perpetuals"]).unwrap(),
                signer: false,
                writable: false,
            },
            Meta {
                address: pool.clone(),
                signer: false,
                writable: false,
            },
            Meta {
                address: position.clone(),
                signer: false,
                writable: false,
            },
            Meta {
                address: request.clone(),
                signer: false,
                writable: true,
            },
            Meta {
                address: custody.clone(),
                signer: false,
                writable: false,
            },
            Meta {
                address: solana_pubkey3::Pubkey::new_unique().to_string(),
                signer: false,
                writable: false,
            },
            Meta {
                address: solana_pubkey3::Pubkey::new_unique().to_string(),
                signer: false,
                writable: false,
            },
        ];
        let instruction = NormalizedInstruction {
            program_id: crate::jlp::PERPS_PROGRAM_ID.into(),
            accounts: accounts.clone(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
        };
        let checked = || {
            inspect_limit_update_message(
                std::slice::from_ref(&instruction),
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_000,
                151_000_000,
            )
        };
        assert!(checked().is_ok());

        let mut wallet_alias_signer = accounts.clone();
        wallet_alias_signer[1].address = wallet.clone();
        let invalid_roles = NormalizedInstruction {
            accounts: wallet_alias_signer,
            ..instruction.clone()
        };
        assert!(
            inspect_limit_update_message(
                &[invalid_roles],
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_000,
                151_000_000
            )
            .is_err()
        );

        let mut extra_signer = accounts.clone();
        extra_signer[8].signer = true;
        let invalid_roles = NormalizedInstruction {
            accounts: extra_signer,
            ..instruction.clone()
        };
        assert!(
            inspect_limit_update_message(
                &[invalid_roles],
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_000,
                151_000_000
            )
            .is_err()
        );

        accounts[6].address = solana_pubkey3::Pubkey::new_unique().to_string();
        let wrong_request = NormalizedInstruction {
            accounts,
            ..instruction.clone()
        };
        assert!(
            inspect_limit_update_message(
                &[wrong_request],
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_000,
                151_000_000
            )
            .is_err()
        );
        assert!(
            inspect_limit_update_message(
                std::slice::from_ref(&instruction),
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_001,
                151_000_000
            )
            .is_err()
        );
        let mut bad_data = data;
        bad_data[16..24].copy_from_slice(&1u64.to_le_bytes());
        let bad_trigger = NormalizedInstruction {
            data_base64: base64::engine::general_purpose::STANDARD.encode(bad_data),
            ..instruction
        };
        assert!(
            inspect_limit_update_message(
                &[bad_trigger],
                &wallet,
                &request,
                &position,
                &pool,
                &custody,
                2_000_000,
                151_000_000
            )
            .is_err()
        );
    }

    #[test]
    fn perps_summaries_format_usd_and_token_quantities_without_rounding() {
        assert_eq!(format_usd_micros(12_100_000), "$12.10");
        assert_eq!(format_usd_micros(12_000_000), "$12.00");
        assert_eq!(format_token_atoms(11_000_000, 6), "11");
        assert_eq!(format_token_atoms(1_234_500, 6), "1.2345");
        assert_eq!(format_token_atoms(123, 2), "1.23");
        assert_eq!(format_token_atoms(15_000, 9), "0.000015");
        assert!(protocol_fee_is_observed(0, 1));
        assert!(protocol_fee_is_observed(1, 0));
        assert!(!protocol_fee_is_observed(0, 0));
        assert_eq!(
            market_permission_summary(
                "SOL", 1, false, 12_100_000, 11_000_000, "USDC", 6, 1_000, 1_000, "USDC", 6, 100,
                15_000, 2_881_440,
            ),
            "Authorize a SOL long Perps position up to $12.10 notional, funded with up to 11 USDC. Protocol fees are bounded at $0.001 and 0.001 USDC; slippage is at most 100 bps. Reserve up to 0.000015 SOL for network fees and 0.00288144 SOL for refundable account funding."
        );
    }

    #[test]
    fn perps_boundary_fee_treatment_matches_gross_funded_settlement() {
        let resource = ResourceId("solana:mainnet-beta:wallet".into());
        let margin = margin_usd_asset();
        let increase = position_protocol_fee_bound(
            resource.clone(),
            "position:collateral".into(),
            margin.clone(),
            gofin_domain::Quantity::from_u64(500_000),
            true,
        );
        assert_eq!(increase.treatment, CostTreatment::IncludedInDebit);
        assert_eq!(increase.amount.asset, margin);
        assert_eq!(increase.amount.atoms.as_str(), "500000");

        let decrease = position_protocol_fee_bound(
            resource,
            "position:collateral".into(),
            margin_usd_asset(),
            gofin_domain::Quantity::from_u64(500_000),
            false,
        );
        assert_eq!(decrease.treatment, CostTreatment::AdditionalDebit);
    }

    #[test]
    fn perps_market_stats_normalize_documented_decimal_fields_and_reject_malformed_values() {
        let raw = json!({
            "price":"123.00",
            "priceChange24H":"-2.50",
            "priceHigh24H":"125.0",
            "priceLow24H":"120",
            "volume":"48123049.00"
        });
        let normalized =
            normalize_market_stats("SOL", "So11111111111111111111111111111111111111112", raw)
                .unwrap();
        assert_eq!(normalized["market"], "SOL");
        assert_eq!(normalized["price_usd"], "123.00");
        assert_eq!(normalized["price_change_24h_percent"], "-2.50");
        assert_eq!(normalized["volume_24h_usd"], "48123049.00");
        assert!(
            normalize_market_stats(
                "SOL",
                "mint",
                json!({
                    "price":"1e2","priceChange24H":"0","priceHigh24H":"2",
                    "priceLow24H":"1","volume":"10"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn perps_collateral_limits_preserve_atomic_strings_and_reject_nonintegers() {
        let raw = json!({
            "maxCollateralDepositTokenAmount":"123456789",
            "maxCollateralDepositUsd":"123456",
            "minCollateralDepositTokenAmount":"1",
            "minCollateralDepositUsd":"1",
            "maxCollateralWithdrawalTokenAmount":"987654321",
            "maxCollateralWithdrawalUsd":"987654",
            "minCollateralWithdrawalTokenAmount":"2",
            "minCollateralWithdrawalUsd":"2"
        });
        let normalized = normalize_collateral_limits("request-pda", "SOL", raw).unwrap();
        assert_eq!(normalized["max_deposit_token_atoms"], "123456789");
        assert_eq!(normalized["max_withdrawal_usd_atoms"], "987654");
        assert!(
            normalize_collateral_limits(
                "request-pda",
                "SOL",
                json!({"maxCollateralDepositTokenAmount":"1.5"})
            )
            .is_err()
        );
    }

    #[test]
    fn internal_collateral_minimum_is_price_slippage_and_fee_bounded() {
        let output_price = gofin_domain::AssetPrice {
            asset: asset("So11111111111111111111111111111111111111112", 9),
            usd_micros_per_unit: gofin_domain::Quantity::new(format!("1{}", "0".repeat(38)))
                .unwrap(),
            unit_atoms: gofin_domain::Quantity::new(format!("1{}", "0".repeat(39))).unwrap(),
            source: "synthetic exact SOL price".into(),
            observed_at: Utc::now().to_rfc3339(),
        };
        let floor = conversion_minimum_atoms(
            &gofin_domain::Quantity::from_u64(1_000_000),
            &output_price,
            50,
            10_000,
        )
        .unwrap();
        assert_eq!(floor.as_str(), "9850000");
        assert!(gofin_domain::Quantity::from_u64(1) < floor);
        assert!(
            conversion_minimum_atoms(
                &gofin_domain::Quantity::from_u64(1_000_000),
                &output_price,
                50,
                1_000_000,
            )
            .is_err()
        );
    }

    #[test]
    fn perps_requires_explicit_mainnet_beta_cluster() {
        assert!(validate_network(&json!({"network":"solana"})).is_err());
        assert!(validate_network(&json!({"network":"solana","cluster":"devnet"})).is_err());
        assert!(validate_network(&json!({"network":"solana","cluster":"mainnet-beta"})).is_ok());
    }

    #[test]
    fn perps_invalid_response_guidance_never_asserts_no_submission_or_recommends_replay() {
        let error = bad_response();
        assert!(error.next_action.contains("check GoFin status first"));
        assert!(error.next_action.contains("reconcile that exact operation"));
        assert!(!error.next_action.contains("no transaction was submitted"));
        assert!(!error.next_action.contains("request a new candidate"));
    }

    #[test]
    fn tracked_perps_idl_hash_and_current_request_event_layouts_are_pinned() {
        use sha2::{Digest, Sha256};
        const IDL: &str = include_str!("../resources/perps-published-idl.json");
        assert_eq!(
            hex::encode(Sha256::digest(IDL.as_bytes())),
            "430dac170062936bd2fc485f33e55dfdfb28335ebb9b71ea722dbf9b81ae9d53"
        );
        let idl: Value = serde_json::from_str(IDL).unwrap();
        let ix_names = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|ix| ix["name"].as_str())
            .collect::<Vec<_>>();
        assert!(ix_names.contains(&"createIncreasePositionMarketRequest"));
        assert!(ix_names.contains(&"createDecreasePositionMarketRequest"));
        let market_instruction = |name: &str| {
            idl["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|ix| ix["name"] == name)
                .unwrap()
        };
        for (name, expected_accounts, writable) in [
            (
                "createIncreasePositionMarketRequest",
                [
                    "owner",
                    "fundingAccount",
                    "perpetuals",
                    "pool",
                    "position",
                    "positionRequest",
                    "positionRequestAta",
                    "custody",
                    "collateralCustody",
                    "inputMint",
                    "referral",
                    "tokenProgram",
                    "associatedTokenProgram",
                    "systemProgram",
                    "eventAuthority",
                    "program",
                ],
                [0usize, 1, 4, 5, 6].as_slice(),
            ),
            (
                "createDecreasePositionMarketRequest",
                [
                    "owner",
                    "receivingAccount",
                    "perpetuals",
                    "pool",
                    "position",
                    "positionRequest",
                    "positionRequestAta",
                    "custody",
                    "collateralCustody",
                    "desiredMint",
                    "referral",
                    "tokenProgram",
                    "associatedTokenProgram",
                    "systemProgram",
                    "eventAuthority",
                    "program",
                ],
                [0usize, 1, 5, 6].as_slice(),
            ),
        ] {
            let instruction = market_instruction(name);
            let accounts = instruction["accounts"].as_array().unwrap();
            assert_eq!(
                accounts
                    .iter()
                    .map(|account| account["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                expected_accounts
            );
            for (index, account) in accounts.iter().enumerate() {
                assert_eq!(account["isSigner"], index == 0, "{name} signer {index}");
                assert_eq!(
                    account["isMut"],
                    writable.contains(&index),
                    "{name} writable {index}"
                );
            }
        }
        let decrease_args = &idl["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ty| ty["name"] == "CreateDecreasePositionMarketRequestParams")
            .unwrap()["type"]["fields"];
        let fields = decrease_args.as_array().unwrap();
        assert_eq!(
            fields
                .iter()
                .map(|field| field["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "collateralUsdDelta",
                "sizeUsdDelta",
                "priceSlippage",
                "jupiterMinimumOut",
                "entirePosition",
                "counter"
            ]
        );
        assert_eq!(fields[0]["type"], "u64");
        assert_eq!(fields[1]["type"], "u64");
        assert_eq!(fields[2]["type"], "u64");
        let event_fields = |name: &str| -> Vec<String> {
            idl["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["name"] == name)
                .unwrap()["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field["name"].as_str().unwrap().to_owned())
                .collect()
        };
        fn borsh_size(ty: &Value, idl: &Value) -> usize {
            match ty {
                Value::String(name) => match name.as_str() {
                    "publicKey" => 32,
                    "u8" | "bool" => 1,
                    "i64" | "u64" => 8,
                    "u128" => 16,
                    _ => panic!("unrecognized pinned IDL type {name}"),
                },
                Value::Object(fields) if fields.contains_key("option") => {
                    1 + borsh_size(&fields["option"], idl)
                }
                Value::Object(fields) if fields.contains_key("defined") => {
                    let name = fields["defined"].as_str().unwrap();
                    let account = idl["accounts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .chain(idl["types"].as_array().unwrap())
                        .find(|entry| entry["name"] == name)
                        .unwrap_or_else(|| panic!("missing pinned type {name}"));
                    if account["type"]["kind"] == "enum" {
                        1
                    } else {
                        account["type"]["fields"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|field| borsh_size(&field["type"], idl))
                            .sum()
                    }
                }
                _ => panic!("unsupported pinned IDL type {ty}"),
            }
        }
        let account_space = |name: &str| -> usize {
            let account = idl["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap();
            8 + account["type"]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| borsh_size(&field["type"], &idl))
                .sum::<usize>()
        };
        let request_space = idl["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == POSITION_REQUEST_ACCOUNT)
            .map(|entry| {
                8 + entry["type"]["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|field| borsh_size(&field["type"], &idl))
                    .sum::<usize>()
            })
            .unwrap();
        assert_eq!(request_space, POSITION_REQUEST_ACCOUNT_SPACE);
        assert_eq!(account_space(POSITION_ACCOUNT), POSITION_ACCOUNT_SPACE);
        let increase = event_fields("IncreasePositionEvent");
        assert!(increase.ends_with(&[
            "openTime".into(),
            "referral".into(),
            "positionFeeUsd".into(),
            "fundingFeeUsd".into(),
            "priceImpactFeeUsd".into(),
        ]));
        assert!(increase.contains(&"collateralUsdDelta".into()));
        assert!(increase.contains(&"collateralTokenDelta".into()));
        let decrease = event_fields("DecreasePositionEvent");
        assert!(decrease.ends_with(&[
            "positionCollateralUsd".into(),
            "positionOpenTime".into(),
            "positionPrice".into(),
        ]));
        assert!(decrease.contains(&"originalPositionCollateralUsd".into()));
    }

    #[test]
    fn perps_operation_schemas_require_real_fee_and_collateral_caps() {
        let definitions = operation_definitions();
        let increase = definitions
            .iter()
            .find(|def| def.name == OP_INCREASE)
            .unwrap();
        assert!(
            increase.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "maximum_protocol_fee_usd_atoms")
        );
        assert!(
            increase.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "maximum_protocol_fee_atoms")
        );
        assert!(
            increase.input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "funding_token")
        );
        assert_eq!(
            increase.input_schema["properties"]["funding_token"]["enum"][0],
            "SOL"
        );
        assert_eq!(
            increase.input_schema["properties"]["funding_token"]["enum"][3],
            "USDC"
        );
        assert_eq!(
            increase.input_schema["properties"]["market"]["enum"][1],
            "ETH"
        );
        assert_eq!(
            required_funding_symbol(&json!({"funding_token":"USDC"}), 1, "SOL").unwrap(),
            "USDC"
        );
        assert_eq!(
            required_funding_symbol(&json!({"funding_token":"SOL"}), 2, "SOL").unwrap(),
            "SOL"
        );
        assert!(required_funding_symbol(&json!({"funding_token":"USDC"}), 2, "SOL").is_ok());
        assert_eq!(
            market_symbol_for_mint(market_mint_for_symbol("ETH").unwrap()),
            Some("ETH")
        );
        assert_eq!(
            market_symbol_for_mint(market_mint_for_symbol("BTC").unwrap()),
            Some("BTC")
        );
        assert_eq!(
            required_funding_symbol(&json!({"funding_token":"ETH"}), 1, "BTC").unwrap(),
            "ETH"
        );
        let decrease = definitions
            .iter()
            .find(|def| def.name == OP_DECREASE)
            .unwrap();
        assert!(decrease.output_schema["properties"]["settlement"]["properties"]["event_price_usd_atoms"].is_object());
    }

    #[test]
    fn perps_operation_examples_include_every_required_input() {
        for definition in operation_definitions() {
            let Some(required) = definition
                .input_schema
                .get("required")
                .and_then(Value::as_array)
            else {
                continue;
            };
            for example in &definition.examples {
                for field in required.iter().filter_map(Value::as_str) {
                    assert!(
                        example.get(field).is_some(),
                        "{} example omits required field {field}",
                        definition.name
                    );
                }
            }
        }
    }

    #[test]
    fn decrease_market_request_params_match_pinned_idl_field_order_and_widths() {
        // Distinct sentinels ensure a shifted or narrowed field cannot pass by
        // coincidentally matching zero-valued close-position parameters.
        let collateral = 0x0102_0304_0506_0708u64;
        let size = 0x1112_1314_1516_1718u64;
        let slippage = 0x2122_2324_2526_2728u64;
        let minimum_out = 0x3132_3334_3536_3738u64;
        let counter = 0x4142_4344_4546_4748u64;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&collateral.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&slippage.to_le_bytes());
        bytes.push(1); // Some(minimum_out)
        bytes.extend_from_slice(&minimum_out.to_le_bytes());
        bytes.extend_from_slice(&[1, 1]); // Some(true)
        bytes.extend_from_slice(&counter.to_le_bytes());

        assert_eq!(
            decode_decrease_market_params(&bytes).unwrap(),
            (
                collateral,
                size,
                slippage,
                Some(minimum_out),
                Some(true),
                counter
            )
        );
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_decrease_market_params(&trailing).is_err());
    }

    #[test]
    fn limit_update_permission_binds_recovery_parent_and_trigger_range() {
        let actual_submission = json!({"kind":"solana_transaction","reference":"original-signature","data":{"message_sha256":"abc"}});
        let prepared = PreparedOperation {
            operation: OP_LIMIT_UPDATE.into(),
            input: json!({"wallet":"wallet-name"}),
            binding: json!({
                "wallet_address":"wallet-address",
                "parent_operation_id":"durable-create-id",
                "parent_submission":actual_submission,
                "request":"request-address",
                "trigger_price_usd_atoms":"150000000",
                "maximum_network_fee_lamports":"1000"
            }),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![],
            },
            action_deadline: None,
            operation_version: 1,
        };
        let spec = BoundarySpec {
            fixed: json!({
                "operation":OP_LIMIT_UPDATE,"network":NETWORK,"cluster":CLUSTER,
                "program":crate::jlp::PERPS_PROGRAM_ID,"wallet_name":"wallet-name",
                "wallet_address":"wallet-address","request":"__gofin_plan_recovery_request__",
                "parent_operation_id":"__gofin_plan_recovery_parent__",
                "parent_submission":{"kind":"plan_recovery_parent","reference":"create-step","data":{"recovery_parent":true}},
                "recovery_parent":true
            }),
            parameters: json!({
                "minimum_trigger_price_usd_atoms":"100000000",
                "maximum_trigger_price_usd_atoms":"200000000",
                "maximum_network_fee_lamports":"2000"
            }),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: String::new(),
        };
        assert!(validate_limit_update_permission_candidate(&spec, &prepared).is_ok());
        let mut outside = prepared;
        outside.binding["trigger_price_usd_atoms"] = json!("200000001");
        assert!(validate_limit_update_permission_candidate(&spec, &outside).is_err());
    }

    #[test]
    fn limit_update_recovery_aliases_must_match_saved_parent() {
        let submission = json!({"kind":"solana_transaction","reference":"sig","data":{}});
        let binding = json!({
            "parent_operation_id":"parent",
            "parent_submission":submission,
            "original_operation_id":"parent",
            "original_submission":submission
        });
        assert!(validate_limit_update_parent_aliases(&binding).is_ok());
        assert!(
            validate_limit_update_parent_aliases(&json!({
                "parent_operation_id":"parent",
                "parent_submission":submission
            }))
            .is_ok()
        );
        assert!(
            validate_limit_update_parent_aliases(&json!({
                "parent_operation_id":"parent",
                "parent_submission":submission,
                "original_operation_id":"other",
                "original_submission":submission
            }))
            .is_err()
        );
        assert!(
            validate_limit_update_parent_aliases(&json!({
                "parent_operation_id":"parent",
                "parent_submission":submission,
                "original_operation_id":"parent",
                "original_submission":{"kind":"solana_transaction","reference":"other","data":{}}
            }))
            .is_err()
        );
    }

    #[test]
    fn perps_custody_fee_ceiling_rounds_up_and_rejects_overflow() {
        assert_eq!(ceil_bps(10_000, 50).unwrap(), 50);
        assert_eq!(ceil_bps(1, 1).unwrap(), 1);
        assert_eq!(ceil_bps(10_001, 50).unwrap(), 51);
        assert!(ceil_bps(u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn perps_provider_fee_quotes_are_required_and_custody_bounded() {
        let open = json!({"openFeeUsd":"150","outstandingBorrowFeeUsd":"50"});
        assert!(validate_quote_fee_bounds(&open, true, 100, 50, 150).is_ok());
        assert!(validate_quote_fee_bounds(&open, true, 100, 49, 150).is_err());
        assert!(validate_quote_fee_bounds(&open, true, 100, 50, 149).is_err());
        let close = json!({"closeFeeUsd":"100","outstandingBorrowFeeUsd":"50"});
        assert!(validate_quote_fee_bounds(&close, false, 100, 50, 150).is_ok());
        assert!(validate_quote_fee_bounds(&close, false, 99, 50, 150).is_err());
        assert!(
            validate_quote_fee_bounds(
                &json!({"closeFeeUsd":"1.2","outstandingBorrowFeeUsd":"0"}),
                false,
                100,
                50,
                150
            )
            .is_err()
        );
    }

    #[test]
    fn perps_borrow_fee_cap_includes_accrued_interest_and_maximum_keeper_window() {
        let request = MarketRequest {
            position: key(1),
            request: key(2),
            market_mint: key(3),
            collateral_mint: key(4),
            funding_mint: key(5),
            market_decimals: 9,
            collateral_decimals: 6,
            funding_decimals: 6,
            maximum_fee_bps: 50,
            minimum_output: None,
            borrow_cumulative_interest: 2_000_000_000,
            borrow_hourly_rate: 100_000_000,
            borrow_last_update: Utc::now().timestamp(),
            max_request_execution_seconds: 3_600,
            swap_fee_bps: 50,
            counter: 0,
            market: "SOL".into(),
        };
        let position = VerifiedPosition {
            address: key(1),
            side: gofin_domain::PositionKind::DerivativeLong,
            market_mint: key(3),
            market_decimals: 9,
            amount: Amount {
                asset: crate::pricing::position_usd_asset(&key(3)).unwrap(),
                atoms: gofin_domain::Quantity::from_u64(100_000_000),
            },
            collateral_mint: key(4),
            collateral_usd_atoms: 10_000_000,
            rent_lamports: 0,
            locked_profit_cap_atoms: 0,
            size_usd_atoms: 100_000_000,
            price_usd_atoms: 1_000_000,
            realized_pnl_usd_atoms: 0,
            cumulative_interest_snapshot: 1_000_000_000,
            observed_slot: 1,
        };
        // The snapshot delta plus one hour of the bounded rate is applied
        // to the existing 100 USD exposure, rounded in USD micros.
        assert_eq!(
            borrow_fee_ceiling_usd(&request, Some(&position), 100_000_000, true).unwrap(),
            110_000_000
        );
        assert!(borrow_fee_ceiling_usd(&request, Some(&position), 1, false).is_ok());
    }

    #[test]
    fn perps_pool_timeout_uses_pinned_pool_layout() {
        let name_len = 4usize;
        let custody_count = 2usize;
        let custody_end = 12 + name_len + 4 + custody_count * 32;
        let timeout_offset = custody_end + 16 + 40 + 72 + 24;
        let mut data = vec![0u8; timeout_offset + 8];
        data[timeout_offset..timeout_offset + 8].copy_from_slice(&120i64.to_le_bytes());
        assert_eq!(
            pool_max_request_execution_seconds(&data, name_len, custody_count).unwrap(),
            120
        );
        data[timeout_offset..timeout_offset + 8].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(pool_max_request_execution_seconds(&data, name_len, custody_count).is_err());
    }

    #[test]
    fn perps_main_pool_name_derives_the_reviewed_mainnet_pool() {
        let program = crate::jlp::PERPS_PROGRAM_ID
            .parse::<solana_pubkey3::Pubkey>()
            .unwrap();
        let mut account = account_discriminator("Pool").to_vec();
        account.extend_from_slice(&(MAIN_POOL_NAME.len() as u32).to_le_bytes());
        account.extend_from_slice(MAIN_POOL_NAME);
        account.extend_from_slice(&0u32.to_le_bytes());
        let (decoded_name, _) = decode_pool_name(&account).unwrap();
        assert_eq!(decoded_name.as_bytes(), MAIN_POOL_NAME);
        assert_eq!(
            main_pool_address(&program).to_string(),
            "5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq"
        );
        assert_eq!(
            solana_pubkey3::Pubkey::find_program_address(
                &[b"pool", decoded_name.as_bytes()],
                &program
            )
            .0,
            main_pool_address(&program)
        );
    }

    #[test]
    fn legacy_usdt_short_position_identity_remains_discoverable() {
        let wallet = "11111111111111111111111111111111";
        let market = market_mint_for_symbol("SOL").unwrap();
        let usdt = token_mint_for_symbol("USDT").unwrap();
        let address = derive_position_address(wallet, market, usdt, 2).unwrap();
        let (symbol, actual_market, actual_collateral, side) =
            derive_position_identity(wallet, &address).unwrap();
        assert_eq!(symbol, "SOL");
        assert_eq!(actual_market, market);
        assert_eq!(actual_collateral, usdt);
        assert_eq!(side, 2);
        assert_eq!(
            expected_position_collateral_mint("SOL", side, Some(&actual_collateral)).unwrap(),
            usdt,
            "legacy short mutations retain their actual USDT collateral identity"
        );
        assert_eq!(
            expected_position_collateral_mint("SOL", side, None).unwrap(),
            token_mint_for_symbol("USDC").unwrap(),
            "only new short positions use the current USDC default"
        );
        assert!(supported_decrease_collateral("SOL", 2, &actual_collateral));
        assert!(supported_receive_mint(
            token_mint_for_symbol("USDC").unwrap()
        ));
        assert!(supported_receive_mint(
            token_mint_for_symbol("SOL").unwrap()
        ));
        // Existing USDT-backed shorts keep their collateral identity while
        // allowing a different supported payout token.
        assert_ne!(actual_collateral, token_mint_for_symbol("USDC").unwrap());
        assert_ne!(
            address,
            derive_position_address(
                wallet,
                market,
                collateral_mint_for_market("SOL", 2).unwrap(),
                2
            )
            .unwrap()
        );
    }

    #[test]
    fn perps_funding_and_receive_conversion_matrix_is_explicit() {
        for (symbol, expected_market) in [
            ("SOL", "SOL"),
            ("ETH", "ETH"),
            ("BTC", "BTC"),
            ("USDC", "USDC"),
        ] {
            assert_eq!(
                required_funding_symbol(&json!({"funding_token":symbol}), 1, "SOL").unwrap(),
                expected_market
            );
            assert_eq!(
                required_funding_symbol(&json!({"funding_token":symbol}), 2, "BTC").unwrap(),
                expected_market
            );
            assert!(supported_receive_mint(
                token_mint_for_symbol(symbol).unwrap()
            ));
        }
        assert!(required_funding_symbol(&json!({"funding_token":"USDT"}), 2, "SOL").is_err());
        assert!(!supported_receive_mint(
            token_mint_for_symbol("USDT").unwrap()
        ));
        assert!(supported_decrease_collateral(
            "SOL",
            2,
            token_mint_for_symbol("USDT").unwrap()
        ));
        assert!(!supported_decrease_collateral(
            "SOL",
            1,
            token_mint_for_symbol("USDT").unwrap()
        ));
    }

    #[test]
    fn perps_execute_extracts_the_exact_first_signature_from_wire() {
        let signature = [7u8; 64];
        let encoded = base64::engine::general_purpose::STANDARD
            .encode([vec![1], signature.to_vec(), vec![99, 1, 2]].concat());
        assert_eq!(
            first_signature_from_wire(&encoded).unwrap(),
            bs58::encode(signature).into_string()
        );
        assert!(first_signature_from_wire("not base64").is_err());
        assert!(
            first_signature_from_wire(&base64::engine::general_purpose::STANDARD.encode([0, 1, 2]))
                .is_err()
        );
    }

    #[test]
    fn sponsored_execute_txid_is_accepted_only_for_an_empty_authorized_payer_slot() {
        let returned = bs58::encode([9u8; 64]).into_string();
        let authorized = vec!["sponsor".to_owned()];
        assert!(validate_execute_txid(None, "sponsor", "wallet", &authorized, &returned).is_ok());
        assert!(validate_execute_txid(None, "sponsor", "wallet", &[], &returned).is_err());
        assert!(validate_execute_txid(None, "wallet", "wallet", &authorized, &returned).is_err());
        assert!(validate_execute_txid(None, "sponsor", "wallet", &authorized, "bad").is_err());
        assert!(
            validate_execute_txid(
                None,
                "sponsor",
                "wallet",
                &authorized,
                &bs58::encode([0u8; 64]).into_string()
            )
            .is_err()
        );

        let known = bs58::encode([7u8; 64]).into_string();
        assert!(validate_execute_txid(Some(&known), "wallet", "wallet", &[], &known).is_ok());
        assert!(validate_execute_txid(Some(&known), "wallet", "wallet", &[], &returned).is_err());
    }

    #[test]
    fn perps_interest_and_fee_fields_match_the_tracked_idl() {
        let idl: Value =
            serde_json::from_str(include_str!("../resources/perps-published-idl.json")).unwrap();
        let account_fields = |name: &str| {
            idl["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|account| account["name"] == name)
                .unwrap()["type"]["fields"]
                .as_array()
                .unwrap()
        };
        let custody = account_fields("Custody");
        let custody_names = custody
            .iter()
            .filter_map(|field| field["name"].as_str())
            .collect::<Vec<_>>();
        let position = account_fields("Position");
        let position_names = position
            .iter()
            .filter_map(|field| field["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(custody_names[10], "fundingRateState");
        assert_eq!(custody_names[11], "bump");
        assert_eq!(custody_names[21], "borrowsFundingRateState");
        assert_eq!(position_names[11], "cumulativeInterestSnapshot");
        assert_eq!(position_names[12], "lockedAmount");

        let events = idl["events"].as_array().unwrap();
        for event_name in ["IncreasePositionEvent", "DecreasePositionEvent"] {
            let event = events
                .iter()
                .find(|event| event["name"] == event_name)
                .unwrap();
            let fields = event["fields"].as_array().unwrap();
            for fee_name in ["positionFeeUsd", "fundingFeeUsd", "priceImpactFeeUsd"] {
                assert!(fields.iter().any(|field| field["name"] == fee_name));
            }
        }
    }

    #[test]
    fn perps_borrow_rate_uses_funding_state_or_jump_maximum() {
        let mut custody = vec![0u8; 684];
        custody[222..230].copy_from_slice(&2_000u64.to_le_bytes());
        custody[230..238].copy_from_slice(&1_000u64.to_le_bytes());
        custody[286..294].copy_from_slice(&100_000u64.to_le_bytes());
        assert_eq!(custody_hourly_funding_rate(&custody).unwrap(), 500_000_000);
        custody[286..294].copy_from_slice(&0u64.to_le_bytes());
        custody[360..368].copy_from_slice(&87_600u64.to_le_bytes());
        assert_eq!(custody_hourly_funding_rate(&custody).unwrap(), 1_000_000);
    }

    #[test]
    fn perps_position_pda_uses_custody_accounts_and_anchor_none_sentinel() {
        let derived = position_pda(&key(1), &key(2), &key(3), &key(4), 1).unwrap();
        assert_eq!(derived, "AFez2197Nz6BzFzKdRxBE5BKtvFf6pEKvSruwHpTdbZa");
        assert!(valid_optional_referral(crate::jlp::PERPS_PROGRAM_ID));
        assert!(!valid_optional_referral("11111111111111111111111111111111"));
        assert!(position_pda(&key(1), &key(2), &key(3), &key(4), 3).is_err());
    }

    #[test]
    fn perps_minimum_out_is_required_only_for_token_conversion() {
        assert!(validate_minimum_output(None, false, 1).is_ok());
        assert!(validate_minimum_output(None, true, 0).is_err());
        assert!(validate_minimum_output(Some(99), true, 100).is_err());
        assert!(validate_minimum_output(Some(100), true, 100).is_ok());
    }

    #[test]
    fn position_layout_requires_exact_idl_discriminator_and_supported_side() {
        assert!(input_schema_positions_has_wallet());
        let owner = key(1);
        let mut data = vec![0; 209];
        data[..8].copy_from_slice(&account_discriminator("Position"));
        data[8..40].copy_from_slice(owner.parse::<solana_pubkey3::Pubkey>().unwrap().as_ref());
        data[40..72].copy_from_slice(key(2).parse::<solana_pubkey3::Pubkey>().unwrap().as_ref());
        data[72..104].copy_from_slice(key(3).parse::<solana_pubkey3::Pubkey>().unwrap().as_ref());
        data[104..136].copy_from_slice(key(4).parse::<solana_pubkey3::Pubkey>().unwrap().as_ref());
        data[152] = 1;
        data[153..161].copy_from_slice(&100_000_000u64.to_le_bytes());
        data[161..169].copy_from_slice(&2_000_000u64.to_le_bytes());
        data[169..177].copy_from_slice(&500_000u64.to_le_bytes());
        data[177..185].copy_from_slice(&(-100i64).to_le_bytes());
        let parsed = decode_position_fields(&owner, &data).unwrap();
        assert_eq!(parsed.side, gofin_domain::PositionKind::DerivativeLong);
        assert_eq!(parsed.size_usd, 2_000_000);
        assert_eq!(parsed.collateral_usd, 500_000);
        assert_eq!(parsed.realized_pnl, -100);
        data[152] = 0;
        assert!(decode_position_fields(&owner, &data).is_err());
        data[152] = 1;
        assert!(decode_position_fields(&key(99), &data).is_err());
        data[0] ^= 1;
        assert!(decode_position_fields(&owner, &data).is_err());
    }

    #[test]
    fn keeper_events_require_a_perps_log_frame_and_decode_exact_pinned_fields() {
        let mut bytes = event_discriminator("IncreasePositionEvent").to_vec();
        push_key(&mut bytes, 1);
        bytes.push(1); // long
        push_key(&mut bytes, 2);
        push_key(&mut bytes, 3);
        bytes.extend_from_slice(&1_000_000u64.to_le_bytes());
        push_key(&mut bytes, 4);
        push_key(&mut bytes, 5); // request PDA
        push_key(&mut bytes, 6);
        bytes.extend_from_slice(&[1, 1]);
        bytes.extend_from_slice(&50_000u64.to_le_bytes());
        push_key(&mut bytes, 7); // owner
        push_key(&mut bytes, 8);
        bytes.extend_from_slice(&200_000u64.to_le_bytes());
        bytes.extend_from_slice(&30_000u64.to_le_bytes());
        bytes.extend_from_slice(&40_000u64.to_le_bytes());
        bytes.extend_from_slice(&100_000_000u64.to_le_bytes());
        bytes.push(0); // no slippage option in event
        bytes.extend_from_slice(&500u64.to_le_bytes());
        bytes.extend_from_slice(&900u64.to_le_bytes());
        bytes.extend_from_slice(&123i64.to_le_bytes());
        bytes.push(0); // no referral
        bytes.extend_from_slice(
            &[1u64, 2, 3]
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .unwrap();
        assert!(
            decode_keeper_event(&raw).unwrap().is_some(),
            "event bytes length {} discriminator {:?}",
            raw.len(),
            &raw[..8]
        );
        let logs = vec![
            format!("Program {} invoke [1]", crate::jlp::PERPS_PROGRAM_ID),
            format!("Program data: {payload}"),
            format!("Program {} success", crate::jlp::PERPS_PROGRAM_ID),
        ];
        let decoded = keeper_events(&logs).unwrap();
        assert!(
            matches!(decoded.as_slice(), [KeeperEvent::Increase {
            request, owner, side: 1, size_delta_usd: 200_000,
            request_type: 1,
            collateral_delta_usd: 30_000, collateral_delta_token: 40_000,
            fee_token: 500, fee_usd: 900, position_fee_usd: 1,
            funding_fee_usd: 2, price_impact_fee_usd: 3, ..
        }] if request == &key(5) && owner == &key(7)),
            "decoded events: {decoded:?}"
        );

        let foreign = vec![
            "Program Other11111111111111111111111111111111 invoke [1]".into(),
            format!("Program data: {payload}"),
            "Program Other11111111111111111111111111111111 success".into(),
        ];
        assert!(keeper_events(&foreign).unwrap().is_empty());
    }

    fn input_schema_positions_has_wallet() -> bool {
        operation_definitions()
            .iter()
            .find(|d| d.name == OP_POSITIONS)
            .is_some_and(|d| {
                d.input_schema["required"]
                    .as_array()
                    .is_some_and(|r| r.contains(&json!("wallet")))
            })
    }

    fn key(byte: u8) -> String {
        solana_pubkey3::Pubkey::new_from_array([byte; 32]).to_string()
    }

    fn push_key(data: &mut Vec<u8>, byte: u8) {
        data.extend_from_slice(&[byte; 32]);
    }
}
