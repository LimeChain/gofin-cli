//! Shared Meteora adapter lifecycle. Family-specific parsing and instruction
//! construction stay behind `MeteoraProtocol`; authorization, freezing,
//! submission identity, recovery, and accounting stay here.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_domain::financial_plans::{FinancialPlanFundingIdentity, PlanPermission};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, CandidateExpiryKind, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, Direction, EffectConstraint,
    FinancialIntent, FinancialState, GoFinError, LiquidityBackingValuation,
    LiquidityPositionEvidence, OperationRecord, PositionKind, PreparationContext,
    PreparedOperation, Reconciliation, ResourceAmount, ResourceId, Result, SnapshotScope,
    StateSnapshot, Status, Submission, SubmissionJournal, Valuation, VerifiedNonDispatchExpiry,
};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{
    CLUSTER, NETWORK, SolanaSigner, SolanaWalletAdapter,
    transaction::{FrozenTransaction, ReceiptOutcome},
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    boundaries,
    protocol::{
        MeteoraAction, MeteoraIntent, MeteoraProtocol, PoolInfo, PoolQuery, ProtocolBuildContext,
        ProtocolCandidate, ProtocolFamily,
    },
    settlement,
    types::{self, *},
};

const QUOTE_LIFETIME: Duration = Duration::from_secs(30);

/// Broker-only bridge to the protected credential store. References are opaque
/// and private key bytes never cross this interface.
pub trait SecondarySignerVault: Send + Sync {
    fn create_or_load(
        &self,
        logical_operation_key: &str,
        owner_address: &str,
        purpose: &str,
    ) -> Result<(String, Arc<SolanaSigner>)>;
    fn load(&self, opaque_reference: &str) -> Result<Arc<SolanaSigner>>;
}

pub struct MeteoraAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocols: BTreeMap<ProtocolFamily, Arc<dyn MeteoraProtocol>>,
    prices: Arc<dyn UsdPriceSource>,
    secondary_signers: Arc<dyn SecondarySignerVault>,
}

impl MeteoraAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        protocols: Vec<Arc<dyn MeteoraProtocol>>,
        prices: Arc<dyn UsdPriceSource>,
        secondary_signers: Arc<dyn SecondarySignerVault>,
    ) -> Result<Self> {
        if wallets.is_empty()
            || wallets
                .iter()
                .any(|(name, wallet)| name.trim().is_empty() || wallet.address().is_empty())
        {
            return Err(GoFinError::validation(
                "Meteora requires at least one enrolled Solana wallet.",
            ));
        }
        let protocols = protocols
            .into_iter()
            .map(|protocol| (protocol.family(), protocol))
            .collect::<BTreeMap<_, _>>();
        if protocols.len() != 2
            || !protocols.contains_key(&ProtocolFamily::Dlmm)
            || !protocols.contains_key(&ProtocolFamily::DammV2)
        {
            return Err(GoFinError::validation(
                "Meteora requires exactly one DLMM and one DAMM v2 protocol module.",
            ));
        }
        Ok(Self {
            wallets,
            protocols,
            prices,
            secondary_signers,
        })
    }

    fn wallet(&self, name: &str) -> Result<Arc<SolanaWalletAdapter>> {
        self.wallets.get(name).cloned().ok_or_else(|| {
            GoFinError::new(
                "METEORA_WALLET_UNKNOWN",
                "connection",
                "The selected Solana wallet is not enrolled for Meteora.",
                "Choose an enrolled Solana wallet from connection discovery.",
            )
        })
    }

    fn protocol(&self, family: ProtocolFamily) -> Result<&Arc<dyn MeteoraProtocol>> {
        self.protocols.get(&family).ok_or_else(unknown_operation)
    }

    fn parse_intent(&self, operation: &str, input: &Value) -> Result<MeteoraIntent> {
        parse_public_intent(operation, input)
    }

    fn context(
        &self,
        intent: &MeteoraIntent,
        position_signer_address: Option<String>,
        temporary_wsol_signer_address: Option<String>,
    ) -> Result<ProtocolBuildContext> {
        let wallet = self.wallet(&intent.wallet)?;
        Ok(ProtocolBuildContext {
            wallet_address: wallet.address().to_owned(),
            position_signer_address,
            temporary_wsol_signer_address,
        })
    }

    fn candidate_from_binding(prepared: &PreparedOperation) -> Result<ProtocolCandidate> {
        serde_json::from_value(prepared.binding["protocol_candidate"].clone())
            .map_err(|_| GoFinError::validation("The prepared Meteora candidate is invalid."))
    }

    fn signer_ref_for_address<'a>(
        prepared: &'a PreparedOperation,
        address: &str,
    ) -> Option<&'a str> {
        let refs = prepared.binding.get("secondary_signer_refs")?.as_object()?;
        let binding = &prepared.binding;
        [
            (
                binding["position_signer_address"].as_str(),
                refs.get("meteora_position_nft_mint"),
            ),
            (
                binding["temporary_wsol_signer_address"].as_str(),
                refs.get("meteora_temporary_wsol"),
            ),
        ]
        .into_iter()
        .find_map(|(identity, reference)| {
            (identity == Some(address))
                .then(|| reference.and_then(Value::as_str))
                .flatten()
        })
    }

    async fn freeze_prepared(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<(
        Arc<SolanaWalletAdapter>,
        MeteoraIntent,
        ProtocolBuildContext,
        ProtocolCandidate,
        FrozenTransaction,
    )> {
        let intent = self.parse_intent(&prepared.operation, &prepared.input)?;
        let wallet = self.wallet(&intent.wallet)?;
        let candidate = Self::candidate_from_binding(prepared)?;
        let position_signer_address = prepared
            .binding
            .get("position_signer_address")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let temporary_wsol_signer_address = prepared
            .binding
            .get("temporary_wsol_signer_address")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let context = self.context(
            &intent,
            position_signer_address,
            temporary_wsol_signer_address,
        )?;
        let protocol = self.protocol(intent.family)?;
        let pool = protocol.pool(&wallet.runtime(), &intent.pool).await?;
        validate_pool(&pool, &intent)?;
        protocol
            .validate_candidate(&wallet.runtime(), &intent, &context, &candidate)
            .await?;
        validate_quote(&intent, &candidate.quote, &pool, true)?;
        let signer = wallet.signer().ok_or_else(read_only_wallet)?;
        let allowed_missing = candidate.required_secondary_signers.clone();
        let frozen = wallet
            .runtime()
            .freeze_instructions(
                &candidate.instructions,
                &candidate.lookup_tables,
                &candidate.blockhash,
                &signer,
                candidate.last_valid_block_height,
                intent.maximum_network_fee_lamports,
                None,
                &allowed_missing,
            )
            .await?;
        if frozen.signer() != wallet.address()
            || frozen.missing_cosigners() != candidate.required_secondary_signers.as_slice()
            || prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256())
        {
            return Err(GoFinError::new(
                "METEORA_MESSAGE_CHANGED",
                "revalidation",
                "The frozen Meteora transaction no longer matches the approved message and signer identities.",
                "Keep the operation unresolved and prepare a fresh candidate within active authority.",
            ));
        }
        wallet.runtime().validate_fee(&frozen).await?;
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(parse_error)?;
        if height > candidate.last_valid_block_height {
            return Err(GoFinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "revalidation",
                "The approved Meteora transaction expired before submission.",
                "Prepare a fresh candidate within active authority; do not replay the expired transaction.",
            ));
        }
        // Pool/graph validation, fee lookup and block-height observation can
        // await RPC. Recheck quote lifetime after that work so a slow final
        // validation cannot authorize a quote that expired while it ran.
        validate_quote(&intent, &candidate.quote, &pool, true)?;
        Ok((wallet, intent, context, candidate, frozen))
    }

    async fn prepare_write(
        &self,
        operation: &str,
        input: &Value,
        logical_operation_id: Option<&str>,
    ) -> Result<PreparedOperation> {
        let intent = self.parse_intent(operation, input)?;
        validate_network(input)?;
        let wallet = self.wallet(&intent.wallet)?;
        if wallet.signer().is_none() {
            return Err(read_only_wallet());
        }
        let request_nonce = logical_operation_id
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().hyphenated().to_string());
        let operation_key = scoped_signer_operation_key(
            &request_nonce,
            self.wallet(&intent.wallet)?.address(),
            intent.family,
            &intent.pool,
            intent.action,
        );
        let mut secondary_signer_refs = serde_json::Map::new();
        let position_signer_address = if intent.action == MeteoraAction::Open {
            let (reference, signer) = self.secondary_signers.create_or_load(
                &operation_key,
                self.wallet(&intent.wallet)?.address(),
                "meteora_position_nft_mint",
            )?;
            let address = signer.address().to_owned();
            secondary_signer_refs.insert("meteora_position_nft_mint".into(), json!(reference));
            Some(address)
        } else {
            None
        };
        let needs_temporary_wsol = intent.action == MeteoraAction::Swap
            && (intent.input_mint.as_deref() == Some("native")
                || intent.output_mint.as_deref() == Some("native"));
        let temporary_wsol_signer_address = if needs_temporary_wsol {
            let (reference, signer) = self.secondary_signers.create_or_load(
                &operation_key,
                wallet.address(),
                "meteora_temporary_wsol",
            )?;
            secondary_signer_refs.insert("meteora_temporary_wsol".into(), json!(reference));
            Some(signer.address().to_owned())
        } else {
            None
        };
        let context = self.context(
            &intent,
            position_signer_address.clone(),
            temporary_wsol_signer_address.clone(),
        )?;
        let protocol = self.protocol(intent.family)?;
        let pool = protocol.pool(&wallet.runtime(), &intent.pool).await?;
        validate_pool(&pool, &intent)?;
        let candidate = protocol
            .prepare_candidate(&wallet.runtime(), &intent, &context)
            .await?;
        crate::protocol::validate_candidate_family(intent.family, &intent, &candidate)?;
        validate_quote(&intent, &candidate.quote, &pool, true)?;
        let expected_secondary = position_signer_address
            .iter()
            .chain(temporary_wsol_signer_address.iter())
            .cloned()
            .collect::<Vec<_>>();
        if intent.action == MeteoraAction::Open && candidate.stable_position_address.is_none() {
            return Err(GoFinError::validation(
                "The Meteora open candidate changed its stable position or broker-owned mint signer identity.",
            ));
        }
        if candidate.required_secondary_signers != expected_secondary {
            return Err(GoFinError::validation(
                "The Meteora candidate changed its approved broker-owned secondary signer identities.",
            ));
        }
        let signer = wallet.signer().ok_or_else(read_only_wallet)?;
        let frozen = wallet
            .runtime()
            .freeze_instructions(
                &candidate.instructions,
                &candidate.lookup_tables,
                &candidate.blockhash,
                &signer,
                candidate.last_valid_block_height,
                intent.maximum_network_fee_lamports,
                None,
                &candidate.required_secondary_signers,
            )
            .await?;
        wallet.runtime().simulate(&frozen).await?;
        let expected_fee = wallet.runtime().validate_fee(&frozen).await?;
        let preview_details = preview_transaction_requirements(
            &wallet.runtime(),
            wallet.address(),
            &intent,
            &candidate,
            expected_fee,
        )
        .await?;
        if preview_details["rent_estimate_complete"] != true {
            return Err(GoFinError::validation(
                "The Meteora candidate does not have a complete rent-funding estimate; no write was prepared.",
            ));
        }
        if preview_details["funding_within_approved_cap"] != true {
            return Err(GoFinError::validation(format!(
                "Meteora account funding requires {} lamports, above the approved maximum of {}.",
                preview_details["account_funding_lamports"]
                    .as_str()
                    .unwrap_or("unknown"),
                intent.maximum_account_funding_lamports
            )));
        }
        if preview_details["remaining_sol_after_requirements_lamports"]
            .as_str()
            .and_then(|value| value.parse::<i128>().ok())
            .is_none_or(|remaining| remaining < 0)
        {
            return Err(GoFinError::validation(
                "The observed wallet SOL balance does not cover Meteora fees, native input, and account funding.",
            ));
        }
        let mut prepared = build_prepared(
            operation,
            input,
            &intent,
            &context,
            &pool,
            &candidate,
            &frozen,
            preview_details,
            secondary_signer_refs,
        )?;
        prepared.valuation = Some(
            self.valuation(&intent, &pool, &candidate, &candidate.quote)
                .await?,
        );
        Ok(prepared)
    }

    async fn valuation(
        &self,
        intent: &MeteoraIntent,
        pool: &PoolInfo,
        candidate: &ProtocolCandidate,
        quote: &crate::protocol::QuoteInfo,
    ) -> Result<Valuation> {
        let mut movement = BTreeMap::<AssetId, gofin_domain::Quantity>::new();
        let mut add = |asset: AssetId, atoms: Option<&str>| -> Result<()> {
            let Some(atoms) = atoms else {
                return Ok(());
            };
            let amount = gofin_domain::Quantity::new(atoms)?;
            if !amount.is_zero() {
                let entry = movement
                    .entry(asset)
                    .or_insert_with(gofin_domain::Quantity::zero);
                *entry = entry.checked_add(&amount)?;
            }
            Ok(())
        };
        let token_a = AssetId {
            network: NETWORK.into(),
            reference: pool.token_a.mint.clone(),
            decimals: pool.token_a.decimals,
        };
        let token_b = AssetId {
            network: NETWORK.into(),
            reference: pool.token_b.mint.clone(),
            decimals: pool.token_b.decimals,
        };
        let native = AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        match intent.action {
            MeteoraAction::Swap => {
                if intent.input_mint.as_deref() == Some("native") {
                    add(native.clone(), intent.amount_atoms.as_deref())?;
                } else if intent.input_mint.as_deref() == Some(pool.token_a.mint.as_str()) {
                    add(token_a.clone(), quote.token_a_debit_atoms.as_deref())?;
                } else {
                    add(token_b.clone(), quote.token_b_debit_atoms.as_deref())?;
                }
            }
            MeteoraAction::Open | MeteoraAction::Add => {
                add(token_a.clone(), quote.token_a_debit_atoms.as_deref())?;
                add(token_b.clone(), quote.token_b_debit_atoms.as_deref())?;
            }
            MeteoraAction::Remove | MeteoraAction::Collect | MeteoraAction::Close => {
                add(token_a.clone(), quote.token_a_credit_atoms.as_deref())?;
                add(token_b.clone(), quote.token_b_credit_atoms.as_deref())?;
                for reward in &quote.rewards {
                    add(
                        AssetId {
                            network: NETWORK.into(),
                            reference: reward.mint.clone(),
                            decimals: reward.decimals,
                        },
                        Some(&reward.amount_atoms),
                    )?;
                }
            }
        }
        let nonrefundable_account_costs = candidate
            .metadata
            .get("nonrefundable_account_costs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| row.get("maximum_lamports").and_then(json_u64))
            .try_fold(0u64, u64::checked_add)
            .ok_or_else(|| {
                GoFinError::validation("Meteora nonrefundable account-cost caps overflow.")
            })?;
        let fee_atoms = gofin_domain::Quantity::from_u64(
            intent
                .maximum_network_fee_lamports
                .checked_add(nonrefundable_account_costs)
                .ok_or_else(|| GoFinError::validation("Meteora maximum cost caps overflow."))?,
        );
        let mut requested = movement.keys().cloned().collect::<Vec<_>>();
        if !requested.contains(&native) {
            requested.push(native.clone());
        }
        let quotes = self.prices.quotes(&requested).await?;
        if quotes.len() != requested.len() {
            return Err(GoFinError::validation(
                "Meteora valuation provider returned an incomplete price set.",
            ));
        }
        let mut prices = Vec::with_capacity(requested.len());
        let mut movement_usd = gofin_domain::Quantity::zero();
        let mut fee_usd = gofin_domain::Quantity::zero();
        let mut sources = Vec::new();
        for (asset, quote) in requested.iter().zip(&quotes) {
            let unit_price = gofin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?;
            if unit_price.is_zero() {
                return Err(GoFinError::validation(
                    "Meteora valuation requires a positive exact-asset price.",
                ));
            }
            let unit_atoms = gofin_domain::Quantity::new(
                "1".to_owned() + &"0".repeat(usize::from(asset.decimals) + 30),
            )?;
            if let Some(amount) = movement.get(asset) {
                let value = amount.mul_div_ceil(&unit_price, &unit_atoms)?;
                movement_usd = movement_usd.checked_add(&value)?;
            }
            if asset == &native {
                fee_usd = fee_atoms.mul_div_ceil(&unit_price, &unit_atoms)?;
            }
            prices.push(gofin_domain::AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: unit_price,
                unit_atoms,
                observed_at: quote.updated_at.clone(),
                source: quote.source.clone(),
            });
            sources.push(quote.source.clone());
        }
        sources.sort();
        sources.dedup();
        Ok(Valuation {
            movement_usd_micros: movement_usd,
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
            observed_at: Utc::now().to_rfc3339(),
            source: sources.join(";"),
            prices,
        })
    }

    async fn snapshot_for_wallets(
        &self,
        selected: Vec<(&str, &SolanaWalletAdapter)>,
    ) -> Result<StateSnapshot> {
        let mut positions = Vec::new();
        let mut coverage = Vec::new();
        let mut complete = true;
        let mut pending = Vec::new();
        for (name, wallet) in selected {
            for family in [ProtocolFamily::Dlmm, ProtocolFamily::DammV2] {
                let protocol = self.protocol(family)?;
                let inventory = protocol
                    .positions(&wallet.runtime(), wallet.address())
                    .await?;
                complete &= inventory.complete;
                coverage.push(inventory.coverage.clone());
                // Engine position-baseline capture accepts only an exact
                // resource attestation. The protocol inventory's descriptive
                // coverage string is useful to readers, but cannot substitute
                // for this canonical wallet resource token.
                coverage.push(format!("solana:mainnet-beta:{}", wallet.address()));
                if !inventory.complete {
                    pending.push(format!(
                        "{} {family:?} position inventory is incomplete.",
                        wallet.address()
                    ));
                }
                for observed in inventory.positions {
                    if observed.owner != wallet.address() || observed.family != family {
                        return Err(GoFinError::validation(
                            "Meteora returned a position with a mismatched owner or family.",
                        ));
                    }
                    let mut anchors =
                        settlement::snapshot_anchors(name, wallet.address(), &observed)?;
                    for position in &mut anchors {
                        let Some(anchor) = &mut position.anchor else {
                            continue;
                        };
                        let Some(raw) = anchor.evidence.get("liquidity_position") else {
                            continue;
                        };
                        let mut evidence: LiquidityPositionEvidence =
                            match serde_json::from_value(raw.clone()) {
                                Ok(evidence) => evidence,
                                Err(_) => continue,
                            };
                        let (Some(a_atoms), Some(b_atoms)) =
                            (&evidence.token_a.atoms, &evidence.token_b.atoms)
                        else {
                            pending.push(format!(
                                "Meteora LP backing for {} has an unknown token balance and remains unpriced.",
                                observed.address
                            ));
                            continue;
                        };
                        let requested = [&evidence.token_a.asset, &evidence.token_b.asset];
                        match self.prices.quotes(&[requested[0].clone(), requested[1].clone()]).await {
                            Ok(quotes) if quotes.len() == 2 => {
                                evidence.backing_valuation = calculate_backing_valuation(
                                    &evidence,
                                    a_atoms,
                                    b_atoms,
                                    &quotes,
                                )
                                .ok();
                                if evidence.backing_valuation.is_none() {
                                    pending.push(format!(
                                        "Meteora LP backing for {} has no valid exact USD price and remains unpriced.",
                                        observed.address
                                    ));
                                }
                            }
                            _ => pending.push(format!(
                                "Meteora LP backing for {} has no complete USD price set and remains unpriced.",
                                observed.address
                            )),
                        }
                        anchor.evidence["liquidity_position"] = serde_json::to_value(evidence)
                            .map_err(|_| {
                                GoFinError::validation(
                                    "Meteora backing valuation could not be encoded.",
                                )
                            })?;
                    }
                    positions.extend(anchors);
                }
            }
        }
        Ok(StateSnapshot {
            state: FinancialState {
                positions,
                ..FinancialState::default()
            },
            observed_at: Utc::now().to_rfc3339(),
            complete,
            coverage,
            pending,
        })
    }
}

fn calculate_backing_valuation(
    evidence: &LiquidityPositionEvidence,
    token_a_atoms: &gofin_domain::Quantity,
    token_b_atoms: &gofin_domain::Quantity,
    quotes: &[UsdQuote],
) -> Result<LiquidityBackingValuation> {
    if quotes.len() != 2 || evidence.token_a.asset == evidence.token_b.asset {
        return Err(GoFinError::validation(
            "Meteora backing price inputs are invalid.",
        ));
    }
    let value = |asset: &AssetId,
                 atoms: &gofin_domain::Quantity,
                 quote: &UsdQuote|
     -> Result<gofin_domain::Quantity> {
        let unit_price = gofin_domain::Quantity::from_decimal(&quote.usd_per_unit, 36)?;
        if unit_price.is_zero() {
            return Err(GoFinError::validation(
                "Meteora backing price must be positive.",
            ));
        }
        let unit_atoms = gofin_domain::Quantity::new(
            "1".to_owned() + &"0".repeat(usize::from(asset.decimals) + 30),
        )?;
        atoms.mul_div_ceil(&unit_price, &unit_atoms)
    };
    let token_a_usd_micros = value(&evidence.token_a.asset, token_a_atoms, &quotes[0])?;
    let token_b_usd_micros = value(&evidence.token_b.asset, token_b_atoms, &quotes[1])?;
    let mut sources = vec![quotes[0].source.clone(), quotes[1].source.clone()];
    sources.sort();
    sources.dedup();
    Ok(LiquidityBackingValuation {
        total_usd_micros: token_a_usd_micros.checked_add(&token_b_usd_micros)?,
        token_a_usd_micros,
        token_b_usd_micros,
        observed_at: Utc::now().to_rfc3339(),
        source: sources.join(";"),
    })
}

#[async_trait]
impl Adapter for MeteoraAdapter {
    fn definitions(&self) -> Vec<Definition> {
        types::operation_definitions()
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets
            .iter()
            .map(|(name, wallet)| ConnectionMetadata {
                name: name.clone(),
                provider: "meteora".into(),
                resource: ResourceId(format!("solana:mainnet-beta:{}", wallet.address())),
                role: Some("liquidity".into()),
                parent: None,
                network: Some(NETWORK.into()),
                address: Some(wallet.address().into()),
                credential_ref: None,
                signing_enabled: wallet.signer().is_some(),
                details: json!({"cluster":CLUSTER,"protocol_families":["dlmm","damm_v2"]}),
            })
            .collect()
    }

    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        self.connections()
    }

    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        validate_network(input)?;
        let intent = self.parse_intent(operation, input)?;
        if intent.action == MeteoraAction::Open {
            return Err(GoFinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Meteora named plans cannot open a position whose broker-generated identity is not yet bound to plan approval.",
                "Open the position as a separately approved operation, then use its exact position address in a named plan.",
            ));
        }
        let wallet = self.wallet(&intent.wallet)?;
        let protocol = self.protocol(intent.family)?;
        let pool = protocol.pool(&wallet.runtime(), &intent.pool).await?;
        validate_pool(&pool, &intent)?;
        let position = if matches!(
            intent.action,
            MeteoraAction::Add
                | MeteoraAction::Remove
                | MeteoraAction::Collect
                | MeteoraAction::Close
        ) {
            let address = intent.position.as_deref().ok_or_else(|| {
                GoFinError::validation(
                    "Meteora position plan steps require an exact position address.",
                )
            })?;
            let observed = protocol.position(&wallet.runtime(), address).await?;
            if observed.address != address
                || observed.owner != wallet.address()
                || observed.family != intent.family
                || observed.pool != pool.address
                || observed.token_a.mint != pool.token_a.mint
                || observed.token_b.mint != pool.token_b.mint
            {
                return Err(GoFinError::validation(
                    "The selected Meteora position is not owned by this wallet or does not match the pinned pool assets.",
                ));
            }
            Some(observed)
        } else {
            None
        };
        match intent.action {
            MeteoraAction::Swap if bounds.get("maximum_input_atoms").is_none() => {
                return Err(GoFinError::validation(
                    "A Meteora swap plan requires maximum_input_atoms.",
                ));
            }
            MeteoraAction::Add
                if bounds.get("maximum_token_a_atoms").is_none()
                    || bounds.get("maximum_token_b_atoms").is_none() =>
            {
                return Err(GoFinError::validation(
                    "A Meteora add plan requires maximum_token_a_atoms and maximum_token_b_atoms.",
                ));
            }
            MeteoraAction::Remove | MeteoraAction::Close
                if bounds.get("minimum_token_a_atoms").is_none()
                    || bounds.get("minimum_token_b_atoms").is_none() =>
            {
                return Err(GoFinError::validation(
                    "A Meteora remove or close plan requires minimum_token_a_atoms and minimum_token_b_atoms.",
                ));
            }
            _ => {}
        }
        let stable_position_address = intent.position.clone();
        let plan_prepared = PreparedOperation {
            operation: operation.to_owned(),
            input: input.clone(),
            binding: json!({
                "protocol_candidate": {
                    "stable_position_address": stable_position_address,
                    "quote": {"receipt_delta_atoms": null}
                }
            }),
            summary: "Balance-independent Meteora plan scope.".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: Utc::now().to_rfc3339(),
            expires_at: Utc::now().to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let spec = boundaries::prepare_plan(&self.wallets, &self.protocols, &plan_prepared, bounds)
            .await?;
        let resource = ResourceId(format!("solana:mainnet-beta:{}", wallet.address()));
        let mut possible_credits = Vec::new();
        let mut add_credit = |asset: AssetId| {
            let identity = FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset,
            };
            if !possible_credits.contains(&identity) {
                possible_credits.push(identity);
            }
        };
        match intent.action {
            MeteoraAction::Swap => {
                let input_mint = intent.input_mint.as_deref().ok_or_else(|| {
                    GoFinError::validation("A Meteora swap plan requires an exact input mint.")
                })?;
                let output_mint = intent.output_mint.as_deref().ok_or_else(|| {
                    GoFinError::validation("A Meteora swap plan requires an exact output mint.")
                })?;
                add_credit(swap_output_credit(&pool, input_mint, output_mint)?);
            }
            MeteoraAction::Remove | MeteoraAction::Collect | MeteoraAction::Close => {
                add_credit(AssetId {
                    network: NETWORK.into(),
                    reference: pool.token_a.mint.clone(),
                    decimals: pool.token_a.decimals,
                });
                add_credit(AssetId {
                    network: NETWORK.into(),
                    reference: pool.token_b.mint.clone(),
                    decimals: pool.token_b.decimals,
                });
                if let Some(position) = &position {
                    for reward in &position.rewards {
                        add_credit(AssetId {
                            network: NETWORK.into(),
                            reference: reward.mint.clone(),
                            decimals: reward.decimals,
                        });
                    }
                }
            }
            MeteoraAction::Add | MeteoraAction::Open => {}
        }
        Ok(PlanPermission {
            spec,
            recovery_parent: None,
            possible_credits,
            fee_assets: vec![AssetId {
                network: NETWORK.into(),
                reference: "native".into(),
                decimals: 9,
            }],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }

    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.snapshot_for_wallets(
            self.wallets
                .iter()
                .map(|(name, wallet)| (name.as_str(), wallet.as_ref()))
                .collect(),
        )
        .await
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        let selected = self
            .wallets
            .iter()
            .filter(|(_, wallet)| {
                scope.matches_network(NETWORK)
                    && scope.matches_resource(&ResourceId(format!(
                        "solana:mainnet-beta:{}",
                        wallet.address()
                    )))
            })
            .map(|(name, wallet)| (name.as_str(), wallet.as_ref()))
            .collect::<Vec<_>>();
        if scope.is_full() {
            self.snapshot().await
        } else if selected.is_empty() {
            Ok(StateSnapshot {
                state: FinancialState::default(),
                observed_at: Utc::now().to_rfc3339(),
                complete: true,
                coverage: vec!["No enrolled Meteora wallet matched the requested scope.".into()],
                pending: vec![],
            })
        } else {
            self.snapshot_for_wallets(selected).await
        }
    }

    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        let family = input
            .get("family")
            .and_then(Value::as_str)
            .map(parse_family)
            .transpose()?;
        match operation {
            OP_POOLS => {
                let search = input.get("search").and_then(Value::as_str);
                let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(25);
                if !(1..=100).contains(&limit) {
                    return Err(GoFinError::validation(
                        "Meteora pool page limit must be from 1 to 100.",
                    ));
                }
                let query = PoolQuery {
                    search: search.map(str::to_owned),
                    limit: limit as u16,
                    cursor: input
                        .get("cursor")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    token_a_mint: input
                        .get("token_a_mint")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    token_b_mint: input
                        .get("token_b_mint")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                };
                let families = family
                    .map(|family| vec![family])
                    .unwrap_or_else(|| vec![ProtocolFamily::Dlmm, ProtocolFamily::DammV2]);
                let mut pages = Vec::new();
                let runtime = self.any_runtime()?;
                for family in families {
                    pages.push((
                        family,
                        self.protocol(family)?.pools(&runtime, &query).await?,
                    ));
                }
                let (family, page) = if pages.len() == 1 {
                    pages.remove(0)
                } else {
                    if query.cursor.is_some() {
                        return Err(GoFinError::validation(
                            "A family is required when following a Meteora pool cursor.",
                        ));
                    }
                    let pools = pages
                        .iter()
                        .flat_map(|(_, page)| page.pools.clone())
                        .collect::<Vec<_>>();
                    return Ok(
                        json!({"pools":pools,"complete":pages.iter().all(|(_,page)|page.complete),"next_cursor":null,"observed_at":Utc::now().to_rfc3339()}),
                    );
                };
                Ok(
                    json!({"family":family,"pools":page.pools,"complete":page.complete,"next_cursor":page.next_cursor,"observed_at":Utc::now().to_rfc3339()}),
                )
            }
            OP_POOL => {
                let family = family
                    .ok_or_else(|| GoFinError::validation("Select a Meteora protocol family."))?;
                let address = required_string(input, "pool")?;
                let runtime = self.any_runtime()?;
                let pool = self.protocol(family)?.pool(&runtime, address).await?;
                Ok(json!({"pool":pool,"observed_at":Utc::now().to_rfc3339()}))
            }
            OP_POSITIONS => {
                let family = family
                    .ok_or_else(|| GoFinError::validation("Select a Meteora protocol family."))?;
                let wallet = self.wallet(required_string(input, "wallet")?)?;
                let positions = self
                    .protocol(family)?
                    .positions(&wallet.runtime(), wallet.address())
                    .await?;
                Ok(
                    json!({"positions":positions.positions,"complete":positions.complete,"coverage":positions.coverage,"observed_at":positions.observed_at}),
                )
            }
            OP_POSITION => {
                let family = family
                    .ok_or_else(|| GoFinError::validation("Select a Meteora protocol family."))?;
                let address = required_string(input, "position")?;
                let runtime = self.any_runtime()?;
                let position = self.protocol(family)?.position(&runtime, address).await?;
                Ok(json!({"position":position,"observed_at":Utc::now().to_rfc3339()}))
            }
            OP_QUOTE => {
                validate_network(input)?;
                let intent = self.parse_intent(OP_QUOTE, input)?;
                let wallet = self.wallet(&intent.wallet)?;
                let position_probe =
                    (intent.action == MeteoraAction::Open).then(ephemeral_public_identity);
                let native_probe = (intent.action == MeteoraAction::Swap
                    && (intent.input_mint.as_deref() == Some("native")
                        || intent.output_mint.as_deref() == Some("native")))
                .then(ephemeral_public_identity);
                let context = self.context(&intent, position_probe, native_probe)?;
                let protocol = self.protocol(intent.family)?;
                let pool = protocol.pool(&wallet.runtime(), &intent.pool).await?;
                validate_pool(&pool, &intent)?;
                // Read-only quote requests may omit execution floors. Build
                // the fee/account-graph preview with zero floors, then derive
                // quote evidence from that same candidate so its simulation
                // and transaction graph cannot be separated by a stale quote.
                // This candidate is never frozen, signed, or submitted.
                let mut preview_intent = intent.clone();
                if preview_intent.action == MeteoraAction::Swap {
                    preview_intent
                        .minimum_output_atoms
                        .get_or_insert_with(|| "0".into());
                } else if matches!(
                    preview_intent.action,
                    MeteoraAction::Remove | MeteoraAction::Close
                ) {
                    preview_intent
                        .minimum_token_a_atoms
                        .get_or_insert_with(|| "0".into());
                    preview_intent
                        .minimum_token_b_atoms
                        .get_or_insert_with(|| "0".into());
                }
                let mut candidate = protocol
                    .build_candidate(&wallet.runtime(), &preview_intent, &context)
                    .await?;
                let quote = protocol
                    .quote_candidate(&wallet.runtime(), &intent, &context, &candidate)
                    .await?;
                validate_quote(&intent, &quote, &pool, false)?;
                candidate.quote = quote.clone();
                let expected_fee = wallet
                    .runtime()
                    .estimate_fee_for_instructions(
                        wallet.address(),
                        &candidate.instructions,
                        &candidate.lookup_tables,
                        &candidate.blockhash,
                        intent.maximum_network_fee_lamports,
                    )
                    .await?;
                let transaction_requirements = preview_transaction_requirements(
                    &wallet.runtime(),
                    wallet.address(),
                    &intent,
                    &candidate,
                    expected_fee,
                )
                .await?;
                let (output_mint, mut output_mints) = if intent.action == MeteoraAction::Swap {
                    let input = intent.input_mint.as_deref().ok_or_else(|| {
                        GoFinError::validation("An exact input mint is required for swap quote.")
                    })?;
                    const WSOL: &str = "So11111111111111111111111111111111111111112";
                    let resolved_input = if input == "native" {
                        if pool.token_a.mint == WSOL {
                            &pool.token_a
                        } else if pool.token_b.mint == WSOL {
                            &pool.token_b
                        } else {
                            return Err(GoFinError::validation(
                                "Native SOL swaps require a pool containing wrapped SOL.",
                            ));
                        }
                    } else if input == pool.token_a.mint {
                        &pool.token_a
                    } else if input == pool.token_b.mint {
                        &pool.token_b
                    } else {
                        return Err(GoFinError::validation(
                            "Swap input mint is outside the selected pool.",
                        ));
                    };
                    let output = if resolved_input.mint == pool.token_a.mint {
                        pool.token_b.clone()
                    } else {
                        pool.token_a.clone()
                    };
                    if intent.output_mint.as_deref() == Some("native") && output.mint != WSOL {
                        return Err(GoFinError::validation(
                            "Native SOL output requires wrapped SOL as the pool output token.",
                        ));
                    }
                    let output_identity = if intent.output_mint.as_deref() == Some("native") {
                        "native".to_owned()
                    } else {
                        output.mint.clone()
                    };
                    (
                        Some(output_identity.clone()),
                        vec![
                            json!({"mint":output_identity,"decimals":if output_identity=="native" {9} else {output.decimals}}),
                        ],
                    )
                } else {
                    let mut outputs = Vec::new();
                    if quote.token_a_credit_atoms.is_some()
                        || matches!(
                            intent.action,
                            MeteoraAction::Remove | MeteoraAction::Collect | MeteoraAction::Close
                        )
                    {
                        outputs.push(
                            json!({"mint":pool.token_a.mint,"decimals":pool.token_a.decimals}),
                        );
                    }
                    if quote.token_b_credit_atoms.is_some()
                        || matches!(
                            intent.action,
                            MeteoraAction::Remove | MeteoraAction::Collect | MeteoraAction::Close
                        )
                    {
                        outputs.push(
                            json!({"mint":pool.token_b.mint,"decimals":pool.token_b.decimals}),
                        );
                    }
                    (None, outputs)
                };
                for reward in &quote.rewards {
                    if !output_mints
                        .iter()
                        .any(|token| token["mint"] == reward.mint)
                    {
                        output_mints.push(json!({"mint":reward.mint,"decimals":reward.decimals}));
                    }
                }
                let recommended_floors = slippage_recommendations(&intent, &quote, &pool)?;
                Ok(
                    json!({"action":intent.action,"quote":quote,"output_mint":output_mint,"output_mints":output_mints,"recommended_floors":recommended_floors,"transaction_requirements":transaction_requirements,"observed_at":Utc::now().to_rfc3339()}),
                )
            }
            _ => Err(unknown_operation()),
        }
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.prepare_write(operation, input, None).await
    }

    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.prepare_write(operation, input, context.logical_operation_id.as_deref())
            .await
    }

    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<gofin_domain::boundaries::BoundarySpec> {
        boundaries::prepare(&self.wallets, &self.protocols, prepared, bounds).await
    }

    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        boundaries::validate_candidate(spec, prepared)
    }

    fn preparation_reuse_window(&self, prepared: &PreparedOperation) -> Option<Duration> {
        matches!(
            prepared.operation.as_str(),
            OP_SWAP | OP_OPEN | OP_ADD | OP_REMOVE | OP_COLLECT | OP_CLOSE
        )
        .then_some(QUOTE_LIFETIME)
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        // The candidate was fully simulated, frozen and fee-checked before
        // approval. Keep this post-approval phase local and short; submit_inner
        // performs one complete fresh pool/graph/message/fee/lifetime check
        // immediately before dispatch. Repeating freeze_prepared here and in
        // submit_inner consumed the short quote window twice.
        let intent = self.parse_intent(&prepared.operation, &prepared.input)?;
        validate_network(&prepared.input)?;
        let wallet = self.wallet(&intent.wallet)?;
        if wallet.signer().is_none() {
            return Err(read_only_wallet());
        }
        if prepared.expires_at.trim().is_empty()
            || DateTime::parse_from_rfc3339(&prepared.expires_at)
                .map_err(|_| GoFinError::validation("Prepared Meteora expiry is invalid."))?
                .to_utc()
                <= Utc::now()
        {
            return Err(GoFinError::validation(
                "The approved Meteora candidate has expired.",
            ));
        }
        let candidate = Self::candidate_from_binding(prepared)?;
        crate::protocol::validate_candidate_family(intent.family, &intent, &candidate)?;
        let pool: PoolInfo = serde_json::from_value(prepared.binding["pool"].clone())
            .map_err(|_| GoFinError::validation("The approved Meteora pool binding is invalid."))?;
        validate_pool(&pool, &intent)?;
        validate_quote(&intent, &candidate.quote, &pool, true)?;
        let position_signer_address = prepared
            .binding
            .get("position_signer_address")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let temporary_wsol_signer_address = prepared
            .binding
            .get("temporary_wsol_signer_address")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let context = self.context(
            &intent,
            position_signer_address,
            temporary_wsol_signer_address,
        )?;
        crate::protocol::validate_candidate_signer_scope(&candidate, &context)?;
        if prepared.binding["wallet_address"].as_str() != Some(wallet.address())
            || prepared.binding["message_sha256"]
                .as_str()
                .is_none_or(|digest| {
                    digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit())
                })
            || prepared.binding["last_valid_block_height"].as_u64()
                != Some(candidate.last_valid_block_height)
            || prepared.binding["required_secondary_signers"]
                .as_array()
                .and_then(|values| values.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
                .is_none_or(|values| {
                    values.iter().copied().ne(candidate
                        .required_secondary_signers
                        .iter()
                        .map(String::as_str))
                })
        {
            return Err(GoFinError::new(
                "METEORA_MESSAGE_CHANGED",
                "revalidation",
                "The approved Meteora message or signer binding is incomplete or changed.",
                "Keep the operation unresolved and prepare a fresh candidate within active authority.",
            ));
        }
        for address in &candidate.required_secondary_signers {
            let reference = Self::signer_ref_for_address(prepared, address).ok_or_else(|| {
                GoFinError::validation(
                    "A required Meteora secondary signer has no protected credential reference.",
                )
            })?;
            let signer = self.secondary_signers.load(reference)?;
            if signer.address() != address {
                return Err(GoFinError::validation(
                    "The protected Meteora secondary signer changed.",
                ));
            }
        }
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let validated = prepared.clone();
        self.submit_inner(&validated, journal).await
    }

    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        self.submit_inner(validated.prepared(), journal).await
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable Meteora submission exists."}),
                verified: None,
            });
        };
        let intent = self.parse_intent(&record.operation, &record.input)?;
        let wallet = self.wallet(&intent.wallet)?;
        match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::Pending { confirmed, reason } => Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"confirmed":confirmed,"reason":reason}),
                verified: None,
            }),
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => Ok(Reconciliation {
                status: Status::Failed,
                evidence: json!({"no_network_send":true,"reason_code":reason_code,"reason":reason}),
                verified: None,
            }),
            ReceiptOutcome::Finalized {
                signature,
                slot,
                fee_lamports,
                failed,
                message_sha256,
                proof,
            } => {
                if failed {
                    let verified = settlement::verify_failed(
                        submission,
                        &proof,
                        wallet.address(),
                        intent.maximum_network_fee_lamports,
                        fee_lamports,
                        slot,
                    )?;
                    return Ok(Reconciliation {
                        status: Status::Failed,
                        evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"proof":proof}),
                        verified: Some(verified),
                    });
                }
                let candidate = Self::candidate_from_record(record)?;
                let binding = &record.preview.as_ref().ok_or_else(parse_error)?.binding;
                let position_signer_address = binding["position_signer_address"]
                    .as_str()
                    .map(str::to_owned);
                let temporary_wsol_signer_address = binding["temporary_wsol_signer_address"]
                    .as_str()
                    .map(str::to_owned);
                let context = self.context(
                    &intent,
                    position_signer_address,
                    temporary_wsol_signer_address,
                )?;
                let verified = self
                    .protocol(intent.family)?
                    .verify_finalized(&wallet.runtime(), &intent, &context, &candidate, &proof)
                    .await?;
                let bundle = settlement::verify(
                    record,
                    submission,
                    &intent,
                    &candidate,
                    &verified,
                    &proof,
                    slot,
                    fee_lamports,
                )?;
                Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: bundle.evidence.clone(),
                    verified: Some(bundle),
                })
            }
        }
    }

    async fn verify_non_dispatch_expiry(
        &self,
        record: &OperationRecord,
    ) -> Result<Option<VerifiedNonDispatchExpiry>> {
        let Some(submission) = record.submission.as_ref() else {
            return Ok(None);
        };
        let Some(verified) = record
            .effects
            .as_ref()
            .filter(|effects| effects.effects.is_empty() && &effects.submission == submission)
        else {
            return Ok(None);
        };
        if record.status != Status::Failed {
            return Ok(None);
        }
        let Some(kind) = persisted_non_dispatch_expiry_kind(record, submission, verified) else {
            return Ok(None);
        };

        // Journal flags are only a hint. Re-run Solana's persisted message,
        // signature, and lifetime checks before allowing a refresh.
        let intent = self.parse_intent(&record.operation, &record.input)?;
        let wallet = self.wallet(&intent.wallet)?;
        let outcome = wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?;
        let ReceiptOutcome::NotDispatched { reason_code, .. } = outcome else {
            return Ok(None);
        };
        if non_dispatch_expiry_kind(&reason_code) != Some(kind) {
            return Ok(None);
        }
        let proof = VerifiedNonDispatchExpiry::from_verified_effects(
            &record.id, submission, verified, kind,
        )?;
        Ok(Some(proof))
    }
}

fn persisted_non_dispatch_expiry_kind(
    record: &OperationRecord,
    submission: &Submission,
    verified: &gofin_domain::VerifiedEffects,
) -> Option<CandidateExpiryKind> {
    let dispatch = record.dispatch_evidence.as_ref()?;
    if record.status != Status::Failed
        || dispatch.get("no_network_send").and_then(Value::as_bool) != Some(true)
        || dispatch.get("chain_signature").is_some()
        || dispatch.get("message_sha256").and_then(Value::as_str)
            != submission
                .data
                .get("message_sha256")
                .and_then(Value::as_str)
        || &verified.submission != submission
        || !verified.effects.is_empty()
        || gofin_domain::verified_effect_digest(verified).ok()? != verified.proof_digest
        || verified.evidence.get("state").and_then(Value::as_str) != Some("not_dispatched")
    {
        return None;
    }
    let reason = dispatch.get("reason_code").and_then(Value::as_str)?;
    if verified.evidence.get("reason_code").and_then(Value::as_str) != Some(reason) {
        return None;
    }
    non_dispatch_expiry_kind(reason)
}

fn non_dispatch_expiry_kind(reason_code: &str) -> Option<CandidateExpiryKind> {
    match reason_code {
        "SOLANA_BLOCKHASH_EXPIRED" | "SOLANA_TX_EXPIRED" => {
            Some(CandidateExpiryKind::TransactionLifetime)
        }
        "PREVIEW_EXPIRED" => Some(CandidateExpiryKind::Quote),
        _ => None,
    }
}

#[cfg(test)]
mod non_dispatch_expiry_tests {
    use super::*;

    fn record_fixture(reason_code: &str) -> OperationRecord {
        let submission = Submission {
            kind: "solana.transaction".into(),
            reference: "solana:message:fixture".into(),
            data: json!({"message_sha256":"fixture-digest"}),
        };
        let mut verified = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: Vec::new(),
            evidence: json!({
                "state":"not_dispatched",
                "reason_code":reason_code,
                "financial_effects":[]
            }),
            observed_at: "2026-10-14T00:00:00Z".into(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
        OperationRecord {
            id: "operation-1".into(),
            request_id: "request-1".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: OP_SWAP.into(),
            input: json!({"wallet":"fixture"}),
            status: Status::Failed,
            created_at: "2026-10-14T00:00:00Z".into(),
            updated_at: "2026-10-14T00:00:00Z".into(),
            preview: None,
            approval: None,
            submission: Some(submission),
            dispatch_evidence: Some(json!({
                "no_network_send":true,
                "reason_code":reason_code,
                "message_sha256":"fixture-digest"
            })),
            position_baseline: None,
            effects: Some(verified),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    #[test]
    fn expiry_refresh_requires_exact_persisted_message_and_empty_effect_proof() {
        let mut record = record_fixture("SOLANA_BLOCKHASH_EXPIRED");
        let submission = record.submission.as_ref().unwrap();
        let verified = record.effects.as_ref().unwrap();
        assert_eq!(
            persisted_non_dispatch_expiry_kind(&record, submission, verified),
            Some(CandidateExpiryKind::TransactionLifetime)
        );

        record.dispatch_evidence.as_mut().unwrap()["no_network_send"] = json!(false);
        assert_eq!(
            persisted_non_dispatch_expiry_kind(
                &record,
                record.submission.as_ref().unwrap(),
                record.effects.as_ref().unwrap()
            ),
            None
        );

        let mut forged = record_fixture("SOLANA_BLOCKHASH_EXPIRED");
        forged.dispatch_evidence.as_mut().unwrap()["message_sha256"] = json!("other-message");
        assert_eq!(
            persisted_non_dispatch_expiry_kind(
                &forged,
                forged.submission.as_ref().unwrap(),
                forged.effects.as_ref().unwrap()
            ),
            None
        );

        let mut authority_denial = record_fixture("PLAN_AUTHORIZATION_INACTIVE");
        assert_eq!(
            persisted_non_dispatch_expiry_kind(
                &authority_denial,
                authority_denial.submission.as_ref().unwrap(),
                authority_denial.effects.as_ref().unwrap()
            ),
            None
        );
        authority_denial.effects.as_mut().unwrap().proof_digest = "forged".into();
        assert_eq!(
            persisted_non_dispatch_expiry_kind(
                &authority_denial,
                authority_denial.submission.as_ref().unwrap(),
                authority_denial.effects.as_ref().unwrap()
            ),
            None
        );
    }
}

impl MeteoraAdapter {
    async fn submit_inner(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let (wallet, _intent, _context, candidate, frozen) = self.freeze_prepared(prepared).await?;
        let signer = wallet.signer().ok_or_else(read_only_wallet)?;
        let mut secondary = Vec::new();
        for address in &candidate.required_secondary_signers {
            let reference = Self::signer_ref_for_address(prepared, address).ok_or_else(|| {
                GoFinError::validation(
                    "A required Meteora secondary signer has no protected credential reference.",
                )
            })?;
            let loaded = self.secondary_signers.load(reference)?;
            if loaded.address() != address {
                return Err(GoFinError::validation(
                    "The protected Meteora signer identity does not match the candidate.",
                ));
            }
            secondary.push(loaded);
        }
        wallet
            .runtime()
            .dispatch_with_secondary_signers(
                &frozen,
                &signer,
                &secondary,
                prepared,
                journal,
                &gofin_solana::transaction::RpcTransactionSender::new(
                    wallet.runtime().rpc().clone(),
                ),
            )
            .await
    }

    fn candidate_from_record(record: &OperationRecord) -> Result<ProtocolCandidate> {
        let binding = record
            .preview
            .as_ref()
            .map(|preview| &preview.binding)
            .ok_or_else(|| {
                GoFinError::validation("The durable Meteora operation has no prepared candidate.")
            })?;
        serde_json::from_value(binding["protocol_candidate"].clone()).map_err(|_| {
            GoFinError::validation("The durable Meteora transaction candidate is invalid.")
        })
    }

    fn any_runtime(&self) -> Result<Arc<gofin_solana::SolanaRuntime>> {
        self.wallets
            .values()
            .next()
            .map(|wallet| wallet.runtime())
            .ok_or_else(unknown_operation)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "The immutable prepared object is assembled from separately validated protocol, transaction, preview, and signer inputs."
)]
fn build_prepared(
    operation: &str,
    input: &Value,
    intent: &MeteoraIntent,
    context: &ProtocolBuildContext,
    pool: &PoolInfo,
    candidate: &ProtocolCandidate,
    frozen: &FrozenTransaction,
    preview_details: Value,
    secondary_signer_refs: serde_json::Map<String, Value>,
) -> Result<PreparedOperation> {
    let resource = ResourceId(format!("solana:mainnet-beta:{}", context.wallet_address));
    let a = AssetId {
        network: NETWORK.into(),
        reference: pool.token_a.mint.clone(),
        decimals: pool.token_a.decimals,
    };
    let b = AssetId {
        network: NETWORK.into(),
        reference: pool.token_b.mint.clone(),
        decimals: pool.token_b.decimals,
    };
    let native = AssetId {
        network: NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    };
    let fee = Amount {
        asset: native.clone(),
        atoms: gofin_domain::Quantity::from_u64(intent.maximum_network_fee_lamports),
    };
    let position = candidate
        .stable_position_address
        .clone()
        .or_else(|| intent.position.clone())
        .unwrap_or_default();
    let liquidity_position = settlement::liquidity_position_id(intent.family.as_str(), &position);
    let liquidity_asset = settlement::liquidity_asset(intent.family.as_str(), &position);
    let pa = liquidity_position.clone();
    let pb = liquidity_position.clone();
    let mut debits = Vec::new();
    let mut expected = Vec::new();
    let mut costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: fee.clone(),
        treatment: CostTreatment::AdditionalDebit,
    }];
    let refundable_rows = candidate
        .metadata
        .get("refundable_rent_accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let refundable_limits = refundable_rows
        .iter()
        .filter_map(|row| {
            let position = row.get("position")?.as_str()?.to_owned();
            let account = row.get("account")?.as_str()?;
            let declared = row.get("maximum_lamports").and_then(json_u64).unwrap_or(0);
            let temporary_wsol = candidate
                .metadata
                .get("temporary_wsol")
                .and_then(|value| value.get("account"))
                .and_then(Value::as_str);
            let maximum = reviewed_rent_account_funding_limit(
                account,
                declared,
                &candidate.quote.protocol_details["rent_account_deltas"],
                temporary_wsol,
            );
            (maximum > 0).then_some(gofin_domain::AccountFundingLimit {
                position,
                maximum_amount: gofin_domain::Quantity::from_u64(maximum),
            })
        })
        .collect::<Vec<_>>();
    let refundable_funding_cap = refundable_limits
        .iter()
        .try_fold(0u64, |total, limit| {
            total.checked_add(limit.maximum_amount.as_str().parse::<u64>().ok()?)
        })
        .ok_or_else(|| {
            GoFinError::validation("Meteora refundable account-funding caps overflow.")
        })?;
    let nonrefundable_cap = candidate
        .metadata
        .get("nonrefundable_account_costs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("maximum_lamports").and_then(json_u64))
        .try_fold(0u64, u64::checked_add)
        .ok_or_else(|| {
            GoFinError::validation("Meteora nonrefundable account-cost caps overflow.")
        })?;
    if refundable_funding_cap.saturating_add(nonrefundable_cap)
        > intent.maximum_account_funding_lamports
    {
        return Err(GoFinError::validation(
            "The reviewed Meteora account-funding and persistent account-cost limits exceed the approved cap.",
        ));
    }
    if nonrefundable_cap > 0 {
        costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(nonrefundable_cap),
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    match intent.action {
        MeteoraAction::Swap => {
            let input_mint = intent
                .input_mint
                .as_deref()
                .ok_or_else(|| GoFinError::validation("Exact input mint is required."))?;
            const WSOL: &str = "So11111111111111111111111111111111111111112";
            let input_is_a = if input_mint == "native" {
                pool.token_a.mint == WSOL
            } else {
                input_mint == pool.token_a.mint
            };
            let input_valid = if input_mint == "native" {
                pool.token_a.mint == WSOL || pool.token_b.mint == WSOL
            } else {
                input_mint == pool.token_a.mint || input_mint == pool.token_b.mint
            };
            if !input_valid {
                return Err(GoFinError::validation(
                    "Swap input mint must match one token in the selected pool.",
                ));
            }
            let token_input = if input_is_a { &a } else { &b };
            let token_output = if input_is_a { &b } else { &a };
            if intent
                .output_mint
                .as_deref()
                .is_some_and(|mint| mint != "native" && mint != token_output.reference)
            {
                return Err(GoFinError::validation(
                    "The requested output mint does not match the selected pool direction.",
                ));
            }
            if intent.output_mint.as_deref() == Some("native") && token_output.reference != WSOL {
                return Err(GoFinError::validation(
                    "Native SOL output requires wrapped SOL as the pool output token.",
                ));
            }
            let in_asset = if input_mint == "native" {
                &native
            } else {
                token_input
            };
            let out_asset = if intent.output_mint.as_deref() == Some("native") {
                &native
            } else {
                token_output
            };
            let input_atoms = intent
                .amount_atoms
                .as_deref()
                .ok_or_else(|| GoFinError::validation("Exact swap input atoms are required."))?;
            let minimum = intent
                .minimum_output_atoms
                .as_deref()
                .ok_or_else(|| GoFinError::validation("Minimum swap output atoms are required."))?;
            let max_in = gofin_domain::Quantity::new(input_atoms)?;
            let min_out = gofin_domain::Quantity::new(minimum)?;
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: in_asset.clone(),
                    atoms: max_in.clone(),
                },
            });
            expected.push(EffectConstraint::Conversion {
                resource: resource.clone(),
                input: in_asset.clone(),
                maximum_input: max_in,
                output: out_asset.clone(),
                minimum_output: min_out,
            });
        }
        MeteoraAction::Open | MeteoraAction::Add => {
            for (asset, atoms) in [
                (&a, intent.token_a_atoms.as_deref()),
                (&b, intent.token_b_atoms.as_deref()),
            ] {
                if let Some(atoms) = atoms {
                    let amount = gofin_domain::Quantity::new(atoms)?;
                    if !amount.is_zero() {
                        expected.push(EffectConstraint::Debit {
                            resource: resource.clone(),
                            asset: asset.clone(),
                            maximum_debit: amount.clone(),
                        });
                        debits.push(ResourceAmount {
                            resource: resource.clone(),
                            amount: Amount {
                                asset: asset.clone(),
                                atoms: amount.clone(),
                            },
                        });
                    }
                }
            }
            // Liquidity units are accounting state, not a fungible receipt
            // floor. DLMM's add instruction has no minimum-share guard, so
            // bound the proved delta only by its ABI-representable range.
            // DAMM's instruction pins an exact u128 liquidity delta.
            let max_units = match intent.family {
                ProtocolFamily::Dlmm => {
                    num_bigint::BigUint::from(u128::MAX) * num_bigint::BigUint::from(70u8)
                }
                ProtocolFamily::DammV2 => num_bigint::BigUint::from(u128::MAX),
            };
            expected.push(EffectConstraint::PositionOfKind {
                resource: resource.clone(),
                position: liquidity_position.clone(),
                asset: liquidity_asset.clone(),
                position_kind: PositionKind::Liquidity,
                direction: Direction::Increase,
                maximum_change: gofin_domain::Quantity::new(max_units.to_string())?,
            });
        }
        MeteoraAction::Remove | MeteoraAction::Collect | MeteoraAction::Close => {
            expected.extend(liquidity_exit_constraints(
                intent,
                candidate,
                &resource,
                &a,
                &b,
                &liquidity_position,
                &liquidity_asset,
            )?);
            if intent.action == MeteoraAction::Close {
                let refunds = candidate
                    .quote
                    .protocol_details
                    .get("rent_account_deltas")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|row| {
                        let account = row.get("account")?.as_str()?;
                        let position = refundable_rows
                            .iter()
                            .find(|rent| {
                                rent.get("account").and_then(Value::as_str) == Some(account)
                            })?
                            .get("position")?
                            .as_str()?;
                        let delta = json_i128(row.get("lamports_delta")?)?;
                        (delta < 0).then_some((position.to_owned(), u64::try_from(-delta).ok()?))
                    })
                    .collect::<Vec<_>>();
                let total_refund = refunds
                    .iter()
                    .try_fold(0u64, |total, (_, amount)| total.checked_add(*amount))
                    .ok_or_else(|| {
                        GoFinError::validation("Meteora rent refund total overflows.")
                    })?;
                for (position, amount) in refunds {
                    expected.push(EffectConstraint::PositionOfKind {
                        resource: resource.clone(),
                        position: position.clone(),
                        asset: native.clone(),
                        position_kind: PositionKind::RefundableAccountFunding,
                        direction: Direction::Decrease,
                        maximum_change: gofin_domain::Quantity::from_u64(amount),
                    });
                    expected.push(EffectConstraint::OriginMovement {
                        from: AccountingLocation::Position {
                            resource: resource.clone(),
                            position,
                            position_kind: PositionKind::RefundableAccountFunding,
                        },
                        to: AccountingLocation::Cash {
                            resource: resource.clone(),
                        },
                        debit_asset: native.clone(),
                        credit_asset: native.clone(),
                        maximum_debit: gofin_domain::Quantity::from_u64(amount),
                        minimum_credit: gofin_domain::Quantity::from_u64(amount),
                    });
                }
                if total_refund > 0 {
                    expected.push(EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: native.clone(),
                        minimum_credit: gofin_domain::Quantity::from_u64(total_refund),
                    });
                }
            }
            if intent.action == MeteoraAction::Remove {
                let units = candidate
                    .quote
                    .receipt_delta_atoms
                    .as_deref()
                    .filter(|value| value.starts_with('-'))
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "The Meteora quote did not prove the liquidity units to remove.",
                        )
                    })?;
                expected.push(EffectConstraint::PositionOfKind {
                    resource: resource.clone(),
                    position: liquidity_position.clone(),
                    asset: liquidity_asset.clone(),
                    position_kind: PositionKind::Liquidity,
                    direction: Direction::Decrease,
                    maximum_change: gofin_domain::Quantity::new(units.trim_start_matches('-'))?,
                });
            }
        }
    }
    if intent.action != MeteoraAction::Close && !refundable_limits.is_empty() {
        debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: gofin_domain::Quantity::from_u64(refundable_funding_cap),
            },
        });
        expected.push(EffectConstraint::OptionalAccountFunding {
            resource: resource.clone(),
            asset: native.clone(),
            accounts: refundable_limits,
        });
    }
    let observed_at = Utc::now().to_rfc3339();
    let expires_at = candidate.quote.expires_at.clone();
    DateTime::parse_from_rfc3339(&expires_at)
        .map_err(|_| GoFinError::validation("The Meteora candidate expiry is invalid."))?;
    let expected_fee = preview_details["expected_network_fee_lamports"]
        .as_str()
        .unwrap_or("unknown");
    let available_sol = preview_details["available_sol_lamports"]
        .as_str()
        .unwrap_or("unknown");
    let required_sol = preview_details["required_starting_sol_lamports"]
        .as_str()
        .unwrap_or("unknown");
    let remaining_sol = preview_details["remaining_sol_after_requirements_lamports"]
        .as_str()
        .unwrap_or("unknown");
    let mut binding = json!({"wallet_address":context.wallet_address,"position_signer_address":context.position_signer_address,"temporary_wsol_signer_address":context.temporary_wsol_signer_address,"pool":pool,"protocol_candidate":candidate,"message_sha256":frozen.message_sha256(),"last_valid_block_height":candidate.last_valid_block_height,"required_secondary_signers":candidate.required_secondary_signers,"secondary_signer_refs":secondary_signer_refs,"maximum_network_fee_lamports":intent.maximum_network_fee_lamports,"maximum_account_funding_lamports":intent.maximum_account_funding_lamports,"transaction_requirements":preview_details,"token_a_position":pa,"token_b_position":pb});
    if !matches!(intent.action, MeteoraAction::Swap) {
        let units = candidate
            .metadata
            .get("position_units_before_atoms")
            .and_then(Value::as_str)
            .and_then(|units| gofin_domain::Quantity::new(units).ok())
            .ok_or_else(|| {
                GoFinError::validation(
                    "The Meteora candidate is missing its exact liquidity-unit prestate.",
                )
            })?;
        binding[gofin_domain::LIQUIDITY_TRANSITION_BINDING_KEY] =
            serde_json::to_value(gofin_domain::LiquidityTransitionBinding {
                version: 1,
                resource: resource.clone(),
                position: liquidity_position,
                asset: liquidity_asset,
                pre_transaction_units_atoms: units,
            })
            .map_err(|_| GoFinError::validation("Could not bind the liquidity-unit prestate."))?;
    }
    Ok(PreparedOperation{operation:operation.into(),input:input.clone(),binding,summary:crate::presentation::approval_summary(intent,&candidate.quote,pool,&preview_details),checks:vec![Check{name:"Exact pool and asset identities".into(),status:CheckStatus::Passed,detail:"The pool and token mints are pinned.".into()},Check{name:"Protocol candidate and account review".into(),status:CheckStatus::Passed,detail:"The family module validated the immutable instruction candidate.".into()},Check{name:"Network fee and simulation".into(),status:CheckStatus::Passed,detail:format!("The frozen transaction passed bounded simulation. Estimated fee is {expected_fee} lamports; transaction-start SOL need is {required_sol} lamports, with {remaining_sol} lamports remaining from the observed {available_sol} lamports." )}],warnings:vec!["Settlement is reported only after the finalized transaction proves the protocol and two-token effects.".into()],observed_at,expires_at,valuation:None,intent:FinancialIntent{debits,maximum_costs:costs,expected,locks:vec![resource]},operation_version:1,action_deadline:None})
}

fn liquidity_exit_constraints(
    intent: &MeteoraIntent,
    candidate: &ProtocolCandidate,
    resource: &ResourceId,
    token_a: &AssetId,
    token_b: &AssetId,
    liquidity_position: &str,
    liquidity_asset: &AssetId,
) -> Result<Vec<EffectConstraint>> {
    let mut constraints = Vec::new();
    let mut credit_limits = Vec::<(AssetId, gofin_domain::Quantity)>::new();
    for (asset, minimum) in [
        (token_a, intent.minimum_token_a_atoms.as_deref()),
        (token_b, intent.minimum_token_b_atoms.as_deref()),
    ] {
        let floor = minimum
            .map(gofin_domain::Quantity::new)
            .transpose()?
            .or_else(|| {
                (intent.action == MeteoraAction::Collect).then(gofin_domain::Quantity::zero)
            });
        if let Some(floor) = floor {
            credit_limits.push((asset.clone(), floor));
        }
    }
    for reward in &candidate.quote.rewards {
        let asset = AssetId {
            network: NETWORK.into(),
            reference: reward.mint.clone(),
            decimals: reward.decimals,
        };
        if let Some((_, existing_floor)) = credit_limits
            .iter_mut()
            .find(|(existing, _)| existing == &asset)
        {
            // Pool-token minima remain authoritative when a claimed
            // fee/reward shares the same mint.
            *existing_floor = std::cmp::max(existing_floor.clone(), gofin_domain::Quantity::zero());
        } else {
            credit_limits.push((asset, gofin_domain::Quantity::zero()));
        }
    }
    constraints.extend(credit_limits.into_iter().map(|(asset, minimum_credit)| {
        EffectConstraint::Credit {
            to: resource.clone(),
            asset,
            minimum_credit,
        }
    }));
    if matches!(intent.action, MeteoraAction::Collect | MeteoraAction::Close) {
        constraints.push(EffectConstraint::PositionOfKind {
            resource: resource.clone(),
            position: liquidity_position.to_owned(),
            asset: liquidity_asset.clone(),
            position_kind: PositionKind::Liquidity,
            direction: Direction::Increase,
            maximum_change: gofin_domain::Quantity::zero(),
        });
    }
    Ok(constraints)
}

async fn preview_transaction_requirements(
    runtime: &gofin_solana::SolanaRuntime,
    wallet_address: &str,
    intent: &MeteoraIntent,
    candidate: &ProtocolCandidate,
    expected_fee_lamports: u64,
) -> Result<Value> {
    let balance = runtime
        .rpc()
        .request(
            "getBalance",
            json!([wallet_address,{"commitment":"confirmed"}]),
        )
        .await?
        .get("value")
        .and_then(Value::as_u64)
        .ok_or_else(parse_error)?;

    let mut accounts = BTreeMap::<String, (bool, bool, bool)>::new();
    accounts.insert(wallet_address.to_owned(), (true, true, false));
    for instruction in &candidate.instructions {
        accounts
            .entry(instruction.program_id.clone())
            .or_insert((false, false, true));
        for meta in &instruction.accounts {
            let entry = accounts.entry(meta.address.clone()).or_default();
            entry.0 |= meta.signer;
            entry.1 |= meta.writable;
        }
    }
    let required_accounts = accounts
        .into_iter()
        .map(|(address, (signer, writable, program))| {
            json!({"address":address,"signer":signer,"writable":writable,"program":program})
        })
        .collect::<Vec<_>>();

    let mut rent = BTreeMap::<String, (u64, i128)>::new();
    let mut declared_rent_accounts = std::collections::BTreeSet::new();
    let mut observed_rent_accounts = std::collections::BTreeSet::new();
    if let Some(rows) = candidate
        .metadata
        .get("refundable_rent_accounts")
        .and_then(Value::as_array)
    {
        for row in rows {
            let Some(account) = row.get("account").and_then(Value::as_str) else {
                continue;
            };
            declared_rent_accounts.insert(account.to_owned());
            let entry = rent.entry(account.to_owned()).or_default();
            entry.0 = entry
                .0
                .max(row.get("maximum_lamports").and_then(json_u64).unwrap_or(0));
        }
    }
    if let Some(rows) = candidate
        .quote
        .protocol_details
        .get("rent_account_deltas")
        .and_then(Value::as_array)
    {
        for row in rows {
            let Some(account) = row.get("account").and_then(Value::as_str) else {
                continue;
            };
            let Some(delta) = row.get("lamports_delta").and_then(json_i128) else {
                continue;
            };
            observed_rent_accounts.insert(account.to_owned());
            let entry = rent.entry(account.to_owned()).or_default();
            entry.1 = delta;
            if delta > 0 {
                entry.0 = entry.0.max(u64::try_from(delta).unwrap_or(u64::MAX));
            }
        }
    }
    let temporary = candidate.metadata.get("temporary_wsol");
    let temp_account = temporary
        .and_then(|value| value.get("account"))
        .and_then(Value::as_str);
    let temporary_rent = temporary
        .and_then(|value| value.get("rent_lamports"))
        .and_then(json_u64)
        .unwrap_or(0);
    if let Some(account) = temp_account {
        declared_rent_accounts.insert(account.to_owned());
        let entry = rent.entry(account.to_owned()).or_default();
        entry.0 = entry.0.max(temporary_rent);
    }
    let unknown_rent_accounts = declared_rent_accounts
        .iter()
        .filter(|account| {
            !observed_rent_accounts.contains(*account) && Some(account.as_str()) != temp_account
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut nonrefundable_account_cost_lamports = 0u64;
    let nonrefundable_account_costs = candidate
        .metadata
        .get("nonrefundable_account_costs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let account = row.get("account")?.as_str()?;
            let maximum = row.get("maximum_lamports").and_then(json_u64)?;
            nonrefundable_account_cost_lamports =
                nonrefundable_account_cost_lamports.saturating_add(maximum);
            Some(json!({"account":account,"maximum_lamports":maximum.to_string()}))
        })
        .collect::<Vec<_>>();

    let mut retained_rent = 0u64;
    let mut refundable_rent = 0u64;
    let mut account_funding = 0u64;
    let rent_accounts = rent
        .iter()
        .map(|(account, (funding, delta))| {
            let required_funding = rent_account_funding_required(
                account,
                *funding,
                *delta,
                observed_rent_accounts.contains(account),
                temp_account,
            );
            account_funding = account_funding.saturating_add(required_funding);
            if *delta > 0 && Some(account.as_str()) != temp_account {
                retained_rent = retained_rent.saturating_add(u64::try_from(*delta).unwrap_or(u64::MAX));
            } else if *delta < 0 {
                refundable_rent = refundable_rent.saturating_add(u64::try_from(delta.saturating_abs()).unwrap_or(u64::MAX));
            }
            json!({"account":account,"required_funding_lamports":required_funding.to_string(),"observed_net_lamports_delta":delta.to_string()})
        })
        .collect::<Vec<_>>();
    let native_input =
        if intent.action == MeteoraAction::Swap && intent.input_mint.as_deref() == Some("native") {
            intent
                .amount_atoms
                .as_deref()
                .unwrap_or("0")
                .parse::<u64>()
                .map_err(|_| parse_error())?
        } else {
            0
        };
    let required_start = expected_fee_lamports
        .saturating_add(account_funding)
        .saturating_add(nonrefundable_account_cost_lamports)
        .saturating_add(native_input);
    let remaining = i128::from(balance) - i128::from(required_start);
    let temp_principal = if temp_account.is_some() {
        native_input
    } else {
        0
    };
    let rent_estimate_complete = unknown_rent_accounts.is_empty();
    Ok(json!({
        "required_accounts":required_accounts,
        "expected_network_fee_lamports":expected_fee_lamports.to_string(),
        "maximum_network_fee_lamports":intent.maximum_network_fee_lamports.to_string(),
        "account_funding_lamports":account_funding.to_string(),
        "nonrefundable_account_cost_lamports":nonrefundable_account_cost_lamports.to_string(),
        "nonrefundable_account_costs":nonrefundable_account_costs,
        "retained_rent_lamports":retained_rent.to_string(),
        "refundable_rent_lamports":refundable_rent.to_string(),
        "rent_accounts":rent_accounts,
        "unknown_rent_accounts":unknown_rent_accounts,
        "rent_estimate_complete":rent_estimate_complete,
        "temporary_wsol_rent_lamports":temporary_rent.to_string(),
        "temporary_wsol_principal_lamports":temp_principal.to_string(),
        "native_input_lamports":native_input.to_string(),
        "required_starting_sol_lamports":required_start.to_string(),
        "available_sol_lamports":balance.to_string(),
        "remaining_sol_after_requirements_lamports":remaining.to_string(),
        "funding_within_approved_cap":account_funding.saturating_add(nonrefundable_account_cost_lamports) <= intent.maximum_account_funding_lamports
    }))
}

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn json_i128(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .or_else(|| value.as_str()?.parse().ok())
}

fn rent_account_funding_required(
    account: &str,
    declared: u64,
    observed_delta: i128,
    observed: bool,
    temporary_account: Option<&str>,
) -> u64 {
    if temporary_account == Some(account) || !observed {
        return declared;
    }
    u64::try_from(observed_delta.max(0)).unwrap_or(u64::MAX)
}

fn reviewed_rent_account_funding_limit(
    account: &str,
    declared: u64,
    rent_account_deltas: &Value,
    temporary_account: Option<&str>,
) -> u64 {
    let observed_delta = rent_account_deltas
        .as_array()
        .into_iter()
        .flatten()
        .find(|delta| delta.get("account").and_then(Value::as_str) == Some(account))
        .and_then(|delta| delta.get("lamports_delta").and_then(json_i128));
    rent_account_funding_required(
        account,
        declared,
        observed_delta.unwrap_or_default(),
        observed_delta.is_some(),
        temporary_account,
    )
}

#[cfg(test)]
mod rent_funding_preview_tests {
    use super::{rent_account_funding_required, reviewed_rent_account_funding_limit};
    use serde_json::json;

    #[test]
    fn observed_refund_is_not_a_funding_requirement() {
        assert_eq!(
            rent_account_funding_required("position", 57_406_080, -57_406_080, true, None),
            0
        );
        assert_eq!(
            rent_account_funding_required("new-ata", 2_039_280, 2_039_280, true, None),
            2_039_280
        );
        assert_eq!(
            rent_account_funding_required("existing-ata", 2_039_280, 0, true, None),
            0
        );
    }

    #[test]
    fn temporary_wsol_keeps_declared_rent_when_closed_in_same_transaction() {
        assert_eq!(
            rent_account_funding_required(
                "temporary-wsol",
                2_039_280,
                0,
                true,
                Some("temporary-wsol")
            ),
            2_039_280
        );
    }

    #[test]
    fn reviewed_close_funding_cap_uses_the_observed_rent_refund() {
        let deltas = json!([{"account":"position","lamports_delta":-57_406_080}]);
        assert_eq!(
            reviewed_rent_account_funding_limit("position", 57_406_080, &deltas, None),
            0
        );
    }
}

fn validate_quote(
    intent: &MeteoraIntent,
    quote: &crate::protocol::QuoteInfo,
    pool: &PoolInfo,
    enforce_slippage_floor: bool,
) -> Result<()> {
    if quote.family != intent.family || quote.action != intent.action || quote.pool != intent.pool {
        return Err(GoFinError::validation(
            "Meteora quote changed the selected family, action, or pool.",
        ));
    }
    let expires = DateTime::parse_from_rfc3339(&quote.expires_at)
        .map_err(|_| GoFinError::validation("Meteora quote expiry is invalid."))?;
    if expires.to_utc() <= Utc::now() {
        return Err(GoFinError::validation("Meteora quote has expired."));
    }
    let slippage_numerator = gofin_domain::Quantity::from_u64(u64::from(
        10_000_u16.saturating_sub(intent.maximum_slippage_bps),
    ));
    let slippage_denominator = gofin_domain::Quantity::from_u64(10_000);
    if intent.action == MeteoraAction::Swap {
        let output = if intent.output_mint.as_deref() == Some("native") {
            quote
                .native_cash_delta
                .as_deref()
                .map(gofin_domain::SignedQuantity::new)
                .transpose()?
                .filter(|delta| !delta.is_negative() && !delta.is_zero())
                .map(|delta| delta.magnitude())
        } else if intent.output_mint.as_deref() == Some(pool.token_a.mint.as_str()) {
            quote
                .token_a_credit_atoms
                .as_deref()
                .map(gofin_domain::Quantity::new)
                .transpose()?
        } else if intent.output_mint.as_deref() == Some(pool.token_b.mint.as_str()) {
            quote
                .token_b_credit_atoms
                .as_deref()
                .map(gofin_domain::Quantity::new)
                .transpose()?
        } else {
            return Err(GoFinError::validation(
                "The Meteora quote output mint is outside the selected pool.",
            ));
        }
        .ok_or_else(|| GoFinError::validation("Meteora quote did not prove an exact output."))?;
        let slip_floor = output.mul_div_floor(&slippage_numerator, &slippage_denominator)?;
        if enforce_slippage_floor {
            let minimum = gofin_domain::Quantity::new(
                intent.minimum_output_atoms.as_deref().ok_or_else(|| {
                    GoFinError::validation(format!(
                        "Set minimum_output_atoms to at least {slip_floor} for the current Meteora quote."
                    ))
                })?,
            )?;
            if minimum < slip_floor || minimum > output {
                return Err(GoFinError::validation(format!(
                    "Meteora minimum_output_atoms must be at least {slip_floor} (quote {output} less {} bps slippage) and no greater than quoted output.",
                    intent.maximum_slippage_bps
                )));
            }
        }
    } else if matches!(intent.action, MeteoraAction::Remove | MeteoraAction::Close) {
        for (mint, quoted, requested) in [
            (
                pool.token_a.mint.as_str(),
                quote.token_a_credit_atoms.as_deref(),
                intent.minimum_token_a_atoms.as_deref(),
            ),
            (
                pool.token_b.mint.as_str(),
                quote.token_b_credit_atoms.as_deref(),
                intent.minimum_token_b_atoms.as_deref(),
            ),
        ] {
            let output = gofin_domain::Quantity::new(quoted.ok_or_else(|| {
                GoFinError::validation(format!(
                    "Meteora quote did not prove token output for {mint}."
                ))
            })?)?;
            let slip_floor = output.mul_div_floor(&slippage_numerator, &slippage_denominator)?;
            if enforce_slippage_floor {
                let minimum = gofin_domain::Quantity::new(requested.ok_or_else(|| {
                    GoFinError::validation(format!(
                        "Set minimum output for {mint} to at least {slip_floor} for the current Meteora quote."
                    ))
                })?)?;
                if minimum < slip_floor || minimum > output {
                    return Err(GoFinError::validation(format!(
                        "Meteora minimum output for {mint} must be at least {slip_floor} (quote {output} less {} bps slippage) and no greater than quoted output.",
                        intent.maximum_slippage_bps
                    )));
                }
            }
        }
    }
    Ok(())
}

fn slippage_recommendations(
    intent: &MeteoraIntent,
    quote: &crate::protocol::QuoteInfo,
    pool: &PoolInfo,
) -> Result<Value> {
    let numerator = gofin_domain::Quantity::from_u64(u64::from(
        10_000_u16.saturating_sub(intent.maximum_slippage_bps),
    ));
    let denominator = gofin_domain::Quantity::from_u64(10_000);
    if intent.action == MeteoraAction::Swap {
        let output =
            if intent.output_mint.as_deref() == Some("native") {
                gofin_domain::SignedQuantity::new(quote.native_cash_delta.as_deref().ok_or_else(
                    || GoFinError::validation("Quote did not prove native SOL output."),
                )?)?
                .magnitude()
            } else if intent.output_mint.as_deref() == Some(pool.token_a.mint.as_str()) {
                gofin_domain::Quantity::new(quote.token_a_credit_atoms.as_deref().ok_or_else(
                    || GoFinError::validation("Quote did not prove token A output."),
                )?)?
            } else {
                gofin_domain::Quantity::new(quote.token_b_credit_atoms.as_deref().ok_or_else(
                    || GoFinError::validation("Quote did not prove token B output."),
                )?)?
            };
        return Ok(json!({"minimum_output_atoms":output.mul_div_floor(&numerator,&denominator)?}));
    }
    let mut result = serde_json::Map::new();
    if matches!(intent.action, MeteoraAction::Remove | MeteoraAction::Close) {
        for (name, value) in [
            (
                "minimum_token_a_atoms",
                quote.token_a_credit_atoms.as_deref(),
            ),
            (
                "minimum_token_b_atoms",
                quote.token_b_credit_atoms.as_deref(),
            ),
        ] {
            let output = gofin_domain::Quantity::new(value.ok_or_else(|| {
                GoFinError::validation(format!("Quote did not prove {name} output."))
            })?)?;
            result.insert(
                name.into(),
                json!(output.mul_div_floor(&numerator, &denominator)?),
            );
        }
    }
    Ok(Value::Object(result))
}

fn validate_pool(pool: &PoolInfo, intent: &MeteoraIntent) -> Result<()> {
    let checks = json!({
        "family_matches": pool.family == intent.family,
        "pool_matches": pool.address == intent.pool,
        "active": pool.active,
        "pool_mints_distinct": pool.token_a.mint != pool.token_b.mint,
        "token_a_matches": intent.action == MeteoraAction::Swap
            || intent.token_a_mint.as_deref() == Some(pool.token_a.mint.as_str()),
        "token_b_matches": intent.action == MeteoraAction::Swap
            || intent.token_b_mint.as_deref() == Some(pool.token_b.mint.as_str()),
    });
    if checks["family_matches"] != true
        || checks["pool_matches"] != true
        || checks["active"] != true
        || checks["pool_mints_distinct"] != true
        || checks["token_a_matches"] != true
        || checks["token_b_matches"] != true
    {
        return Err(GoFinError::validation(
            "The selected Meteora pool is inactive or its canonical identities changed.",
        )
        .with_details(json!({
            "checks": checks,
            "expected": {
                "family": intent.family,
                "pool": intent.pool,
                "token_a_mint": intent.token_a_mint,
                "token_b_mint": intent.token_b_mint,
            },
            "observed": {
                "family": pool.family,
                "pool": pool.address,
                "active": pool.active,
                "token_a_mint": pool.token_a.mint,
                "token_b_mint": pool.token_b.mint,
            },
        })));
    }
    Ok(())
}

fn swap_output_credit(pool: &PoolInfo, input_mint: &str, output_mint: &str) -> Result<AssetId> {
    const WSOL: &str = "So11111111111111111111111111111111111111112";
    let input_is_a = if input_mint == "native" {
        if pool.token_a.mint == WSOL {
            true
        } else if pool.token_b.mint == WSOL {
            false
        } else {
            return Err(GoFinError::validation(
                "Native SOL plan input requires wrapped SOL in the selected pool.",
            ));
        }
    } else if input_mint == pool.token_a.mint {
        true
    } else if input_mint == pool.token_b.mint {
        false
    } else {
        return Err(GoFinError::validation(
            "The Meteora plan input mint is outside the selected pool.",
        ));
    };
    let expected_output = if input_is_a {
        &pool.token_b
    } else {
        &pool.token_a
    };
    if (output_mint == "native" && expected_output.mint != WSOL)
        || (output_mint != "native" && output_mint != expected_output.mint)
    {
        return Err(GoFinError::validation(
            "The Meteora plan output mint is outside the exact pool direction.",
        ));
    }
    Ok(if output_mint == "native" {
        AssetId {
            network: NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        }
    } else {
        AssetId {
            network: NETWORK.into(),
            reference: expected_output.mint.clone(),
            decimals: expected_output.decimals,
        }
    })
}

fn action_for_operation(operation: &str) -> Option<MeteoraAction> {
    Some(match operation {
        OP_SWAP => MeteoraAction::Swap,
        OP_OPEN => MeteoraAction::Open,
        OP_ADD => MeteoraAction::Add,
        OP_REMOVE => MeteoraAction::Remove,
        OP_COLLECT => MeteoraAction::Collect,
        OP_CLOSE => MeteoraAction::Close,
        _ => return None,
    })
}

pub(crate) fn parse_public_intent(operation: &str, input: &Value) -> Result<MeteoraIntent> {
    let mut value = input.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| GoFinError::validation("Meteora input must be an object."))?;
    object.remove("network");
    object.remove("cluster");
    if operation == OP_QUOTE {
        let action = object
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Select the Meteora action to quote."))?;
        if !matches!(
            action,
            "swap" | "open" | "add" | "remove" | "collect" | "close"
        ) {
            return Err(GoFinError::validation(
                "Select a supported Meteora quote action.",
            ));
        }
    } else {
        let action = action_for_operation(operation).ok_or_else(unknown_operation)?;
        if object
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|supplied| supplied != format!("{action:?}").to_ascii_lowercase())
        {
            return Err(GoFinError::validation(
                "Meteora action does not match the selected operation.",
            ));
        }
        object.remove("action");
        object.insert("action".into(), json!(action));
    }
    let intent: MeteoraIntent = serde_json::from_value(value).map_err(|_| {
        GoFinError::validation(
            "The Meteora request does not match its exact family, pool and amount schema.",
        )
    })?;
    if intent.action == MeteoraAction::Swap
        && intent.output_mint.as_deref().is_none_or(str::is_empty)
    {
        return Err(GoFinError::validation(
            "An explicit output_mint is required for every Meteora swap.",
        ));
    }
    if intent.action != MeteoraAction::Swap
        && (intent.token_a_mint.as_deref().is_none_or(str::is_empty)
            || intent.token_b_mint.as_deref().is_none_or(str::is_empty))
    {
        return Err(GoFinError::validation(
            "Explicit token_a_mint and token_b_mint are required for Meteora liquidity actions.",
        ));
    }
    Ok(intent)
}
fn scoped_signer_operation_key(
    nonce: &str,
    owner: &str,
    family: ProtocolFamily,
    pool: &str,
    action: MeteoraAction,
) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "gofin:solana:auxiliary:{nonce}:{owner}:{}:{pool}:{action:?}",
            family.as_str()
        )
        .as_bytes(),
    );
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).hyphenated().to_string()
}
fn ephemeral_public_identity() -> String {
    use sha2::{Digest, Sha256};
    let nonce = Uuid::new_v4();
    let digest = Sha256::digest(
        [
            b"gofin:meteora:read-only-probe:".as_slice(),
            nonce.as_bytes(),
        ]
        .concat(),
    );
    bs58::encode(&digest[..32]).into_string()
}
fn parse_family(value: &str) -> Result<ProtocolFamily> {
    match value {
        "dlmm" => Ok(ProtocolFamily::Dlmm),
        "damm_v2" => Ok(ProtocolFamily::DammV2),
        _ => Err(GoFinError::validation(
            "Select `dlmm` or `damm_v2` as the Meteora protocol family.",
        )),
    }
}
fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GoFinError::validation(format!("Meteora `{key}` is required.")))
}
fn validate_network(input: &Value) -> Result<()> {
    if input.get("network").and_then(Value::as_str) != Some("solana")
        || input.get("cluster").and_then(Value::as_str) != Some(CLUSTER)
    {
        return Err(GoFinError::validation(
            "Meteora operations require Solana mainnet-beta.",
        ));
    }
    Ok(())
}
fn read_only_wallet() -> GoFinError {
    GoFinError::new(
        "SOLANA_WALLET_READ_ONLY",
        "dispatch",
        "The selected Solana wallet has no protected signer.",
        "Enroll a protected signer before requesting a Meteora write.",
    )
}
fn unknown_operation() -> GoFinError {
    GoFinError::new(
        "METEORA_OPERATION_UNSUPPORTED",
        "prepare",
        "The requested Meteora operation is not supported.",
        "Use one of the discovered Meteora operations.",
    )
}
fn parse_error() -> GoFinError {
    GoFinError::new(
        "SOLANA_RESPONSE_INVALID",
        "rpc",
        "The Solana RPC response was invalid.",
        "Retry with a trusted mainnet-beta RPC endpoint.",
    )
}

#[cfg(test)]
mod quote_expiry_tests {
    use super::*;
    use crate::protocol::{
        MeteoraProtocol, PoolPage, PoolQuery, PositionInventory, ProtocolBuildContext, QuoteInfo,
        TokenInfo, VerifiedProtocolEffects, validate_candidate_family,
        validate_candidate_signer_scope,
    };
    use async_trait::async_trait;
    use base64::Engine as _;
    use gofin_solana::{
        FinalizedReceiptProof, SolanaSigner,
        rpc::SolanaRpc,
        transaction::{NormalizedAccountMeta, NormalizedInstruction},
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const WALLET: &str = "11111111111111111111111111111111";
    const WSOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

    struct CountingRpc(AtomicUsize, Mutex<Vec<String>>);

    #[async_trait]
    impl SolanaRpc for CountingRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            self.0.fetch_add(1, Ordering::Relaxed);
            self.1.lock().unwrap().push(method.to_owned());
            if method == "getSlot" || method == "getBlockHeight" {
                return Ok(json!(1));
            }
            Err(GoFinError::validation(format!(
                "Unexpected RPC in local revalidation fixture: {method}"
            )))
        }
    }

    struct NoPrices;

    #[async_trait]
    impl UsdPriceSource for NoPrices {
        async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
            Err(GoFinError::validation(
                "Prices are not used in this fixture.",
            ))
        }
    }

    struct NoSecondarySigners;

    impl SecondarySignerVault for NoSecondarySigners {
        fn create_or_load(
            &self,
            _logical_operation_key: &str,
            _owner_address: &str,
            _purpose: &str,
        ) -> Result<(String, Arc<SolanaSigner>)> {
            Err(GoFinError::validation("No secondary signer is expected."))
        }

        fn load(&self, _opaque_reference: &str) -> Result<Arc<SolanaSigner>> {
            Err(GoFinError::validation("No secondary signer is expected."))
        }
    }

    struct WrongIdentitySecondarySigner(Arc<SolanaSigner>);

    impl SecondarySignerVault for WrongIdentitySecondarySigner {
        fn create_or_load(
            &self,
            _logical_operation_key: &str,
            _owner_address: &str,
            _purpose: &str,
        ) -> Result<(String, Arc<SolanaSigner>)> {
            Err(GoFinError::validation(
                "No secondary signer is created in this fixture.",
            ))
        }

        fn load(&self, _opaque_reference: &str) -> Result<Arc<SolanaSigner>> {
            Ok(self.0.clone())
        }
    }

    struct StaticProtocol {
        family: ProtocolFamily,
        pool: PoolInfo,
    }

    #[async_trait]
    impl MeteoraProtocol for StaticProtocol {
        fn family(&self) -> ProtocolFamily {
            self.family
        }
        async fn pools(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _query: &PoolQuery,
        ) -> Result<PoolPage> {
            Err(GoFinError::validation("Not used."))
        }
        async fn pool(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _address: &str,
        ) -> Result<PoolInfo> {
            Ok(self.pool.clone())
        }
        async fn positions(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _owner: &str,
        ) -> Result<PositionInventory> {
            Err(GoFinError::validation("Not used."))
        }
        async fn position(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _address: &str,
        ) -> Result<crate::protocol::PositionInfo> {
            Err(GoFinError::validation("Not used."))
        }
        async fn quote(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _intent: &MeteoraIntent,
            _context: &ProtocolBuildContext,
        ) -> Result<QuoteInfo> {
            Err(GoFinError::validation("Not used."))
        }
        async fn build_candidate(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _intent: &MeteoraIntent,
            _context: &ProtocolBuildContext,
        ) -> Result<ProtocolCandidate> {
            Err(GoFinError::validation("Not used."))
        }
        async fn validate_candidate(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            intent: &MeteoraIntent,
            context: &ProtocolBuildContext,
            candidate: &ProtocolCandidate,
        ) -> Result<()> {
            validate_candidate_family(self.family, intent, candidate)?;
            validate_candidate_signer_scope(candidate, context)
        }
        async fn verify_finalized(
            &self,
            _runtime: &gofin_solana::SolanaRuntime,
            _intent: &MeteoraIntent,
            _context: &ProtocolBuildContext,
            _candidate: &ProtocolCandidate,
            _proof: &FinalizedReceiptProof,
        ) -> Result<VerifiedProtocolEffects> {
            Err(GoFinError::validation("Not used."))
        }
    }

    struct NoDispatchJournal;

    impl SubmissionJournal for NoDispatchJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }
        fn persist_submission(&self, _submission: &Submission) -> Result<()> {
            panic!("The unauthorized mutated candidate must not reach dispatch.")
        }
    }

    fn signing_key() -> Arc<SolanaSigner> {
        Arc::new(SolanaSigner::parse(
            "99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM",
        ).unwrap())
    }

    async fn prepared_fixture() -> (MeteoraAdapter, Arc<CountingRpc>, PreparedOperation) {
        let signer = signing_key();
        let wallet_address = signer.address().to_owned();
        let rpc = Arc::new(CountingRpc(AtomicUsize::new(0), Mutex::new(Vec::new())));
        let wallet = Arc::new(
            SolanaWalletAdapter::new(
                "wallet",
                &wallet_address,
                Some(signer.clone()),
                rpc.clone(),
                Arc::new(NoPrices),
            )
            .unwrap(),
        );
        let pool = PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: WALLET.into(),
            token_a: TokenInfo {
                mint: WSOL.into(),
                decimals: 9,
                token_program: TOKEN_PROGRAM.into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: USDC.into(),
                decimals: 6,
                token_program: TOKEN_PROGRAM.into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: None,
            fee_bps: None,
            protocol_state: Value::Null,
        };
        let protocols: Vec<Arc<dyn MeteoraProtocol>> =
            [ProtocolFamily::Dlmm, ProtocolFamily::DammV2]
                .into_iter()
                .map(|family| {
                    Arc::new(StaticProtocol {
                        family,
                        pool: pool.clone(),
                    }) as Arc<dyn MeteoraProtocol>
                })
                .collect();
        let adapter = MeteoraAdapter::new(
            BTreeMap::from([("wallet".into(), wallet.clone())]),
            protocols,
            Arc::new(NoPrices),
            Arc::new(NoSecondarySigners),
        )
        .unwrap();
        let input = json!({
            "wallet":"wallet", "network":"solana", "cluster":"mainnet-beta",
            "family":"dlmm", "action":"swap", "pool":pool.address,
            "input_mint":WSOL, "output_mint":USDC, "amount_atoms":"100",
            "minimum_output_atoms":"995", "maximum_slippage_bps":50,
            "maximum_network_fee_lamports":20000, "maximum_account_funding_lamports":0
        });
        let intent = adapter.parse_intent(OP_SWAP, &input).unwrap();
        let candidate = ProtocolCandidate {
            quote: QuoteInfo {
                family: ProtocolFamily::Dlmm,
                action: MeteoraAction::Swap,
                pool: pool.address.clone(),
                position: None,
                token_a_debit_atoms: Some("100".into()),
                token_b_debit_atoms: None,
                token_a_credit_atoms: None,
                token_b_credit_atoms: Some("1000".into()),
                receipt_delta_atoms: None,
                fees_a_atoms: Some("0".into()),
                fees_b_atoms: None,
                rewards: vec![],
                expires_at: "2099-01-01T00:00:00Z".into(),
                price_impact_bps: Some(0),
                native_cash_delta: None,
                protocol_details: Value::Null,
            },
            instructions: vec![NormalizedInstruction {
                program_id: WALLET.into(),
                accounts: vec![NormalizedAccountMeta {
                    address: wallet_address.clone(),
                    signer: true,
                    writable: true,
                }],
                data_base64: base64::engine::general_purpose::STANDARD.encode([1u8]),
            }],
            lookup_tables: vec![],
            blockhash: bs58::encode([42u8; 32]).into_string(),
            last_valid_block_height: 100,
            stable_position_address: None,
            required_secondary_signers: vec![],
            metadata: Value::Null,
        };
        let frozen = wallet
            .runtime()
            .freeze_instructions(
                &candidate.instructions,
                &candidate.lookup_tables,
                &candidate.blockhash,
                &signer,
                candidate.last_valid_block_height,
                intent.maximum_network_fee_lamports,
                None,
                &[],
            )
            .await
            .unwrap();
        let binding = json!({
            "wallet_address":wallet_address, "position_signer_address":null,
            "temporary_wsol_signer_address":null, "pool":pool,
            "protocol_candidate":candidate, "message_sha256":frozen.message_sha256(),
            "last_valid_block_height":candidate.last_valid_block_height,
            "required_secondary_signers":[], "secondary_signer_refs":{},
            "maximum_network_fee_lamports":intent.maximum_network_fee_lamports
        });
        let prepared = PreparedOperation {
            operation: OP_SWAP.into(),
            input,
            binding,
            summary: "synthetic approved swap".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: Utc::now().to_rfc3339(),
            expires_at: candidate.quote.expires_at.clone(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![],
            },
            operation_version: 1,
            action_deadline: None,
        };
        rpc.0.store(0, Ordering::Relaxed);
        rpc.1.lock().unwrap().clear();
        (adapter, rpc, prepared)
    }

    #[test]
    fn completed_simulation_does_not_make_an_expired_quote_fresh() {
        let intent: MeteoraIntent = serde_json::from_value(json!({
            "wallet":"wallet",
            "family":"damm_v2",
            "action":"collect",
            "pool":"pool",
            "maximum_slippage_bps":0,
            "maximum_network_fee_lamports":10000,
            "maximum_account_funding_lamports":0
        }))
        .unwrap();
        let pool = PoolInfo {
            family: ProtocolFamily::DammV2,
            address: "pool".into(),
            token_a: TokenInfo {
                mint: "token-a".into(),
                decimals: 9,
                token_program: "token-program".into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: "token-b".into(),
                decimals: 6,
                token_program: "token-program".into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: None,
            fee_bps: None,
            protocol_state: Value::Null,
        };
        let quote = QuoteInfo {
            family: ProtocolFamily::DammV2,
            action: MeteoraAction::Collect,
            pool: "pool".into(),
            position: Some("position".into()),
            token_a_debit_atoms: Some("0".into()),
            token_b_debit_atoms: Some("0".into()),
            token_a_credit_atoms: Some("0".into()),
            token_b_credit_atoms: Some("0".into()),
            receipt_delta_atoms: Some("0".into()),
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            expires_at: "2026-01-01T00:00:00Z".into(),
            price_impact_bps: None,
            native_cash_delta: None,
            protocol_details: Value::Null,
        };

        let error = validate_quote(&intent, &quote, &pool, false).unwrap_err();
        assert_eq!(error.message, "Meteora quote has expired.");
    }

    #[tokio::test]
    async fn post_approval_revalidation_is_local_and_submission_rechecks_mutated_message() {
        let (adapter, rpc, prepared) = prepared_fixture().await;
        let revalidated = Adapter::revalidate(&adapter, &prepared).await.unwrap();
        assert_eq!(revalidated.binding, prepared.binding);
        assert_eq!(
            rpc.0.load(Ordering::Relaxed),
            0,
            "approval revalidation must not repeat RPC preparation"
        );

        let mut tampered = prepared;
        let mut candidate: ProtocolCandidate =
            serde_json::from_value(tampered.binding["protocol_candidate"].clone()).unwrap();
        candidate.instructions[0].data_base64 =
            base64::engine::general_purpose::STANDARD.encode([2u8]);
        tampered.binding["protocol_candidate"] = serde_json::to_value(&candidate).unwrap();
        let error = Adapter::submit(&adapter, &tampered, Arc::new(NoDispatchJournal))
            .await
            .unwrap_err();
        assert_eq!(error.code, "METEORA_MESSAGE_CHANGED");
        let methods = rpc.1.lock().unwrap().clone();
        assert!(
            methods
                .iter()
                .all(|method| method == "getSlot" || method == "getBlockHeight"),
            "mutated candidate must be rejected before fee lookup or dispatch: {methods:?}"
        );
    }

    #[tokio::test]
    async fn post_approval_revalidation_rejects_expiry_and_secondary_signer_mismatch_locally() {
        let (adapter, rpc, prepared) = prepared_fixture().await;

        for expired in [
            {
                let mut value = prepared.clone();
                value.expires_at = "2020-01-01T00:00:00Z".into();
                value
            },
            {
                let mut value = prepared.clone();
                value.expires_at = "not-an-rfc3339-date".into();
                value
            },
            {
                let mut value = prepared.clone();
                value.binding["protocol_candidate"]["quote"]["expires_at"] =
                    Value::String("2020-01-01T00:00:00Z".into());
                value
            },
        ] {
            assert!(Adapter::revalidate(&adapter, &expired).await.is_err());
        }

        let wallet_signer = signing_key();
        let expected_address = USDC.to_owned();
        let mut mismatch = prepared;
        let mut candidate: ProtocolCandidate =
            serde_json::from_value(mismatch.binding["protocol_candidate"].clone()).unwrap();
        candidate.required_secondary_signers = vec![expected_address.clone()];
        candidate.instructions[0]
            .accounts
            .push(NormalizedAccountMeta {
                address: expected_address.clone(),
                signer: true,
                writable: false,
            });
        mismatch.binding["protocol_candidate"] = serde_json::to_value(candidate).unwrap();
        mismatch.binding["position_signer_address"] = Value::String(expected_address.clone());
        mismatch.binding["required_secondary_signers"] = json!([expected_address.clone()]);
        mismatch.binding["secondary_signer_refs"] =
            json!({"meteora_position_nft_mint":"synthetic-ref"});
        let wrong_adapter = MeteoraAdapter::new(
            adapter.wallets.clone(),
            adapter.protocols.values().cloned().collect(),
            Arc::new(NoPrices),
            Arc::new(WrongIdentitySecondarySigner(wallet_signer)),
        )
        .unwrap();
        let error = Adapter::revalidate(&wrong_adapter, &mismatch)
            .await
            .unwrap_err();
        assert!(error.message.contains("secondary signer changed"));
        assert_eq!(rpc.0.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod signer_identity_tests {
    use super::*;

    fn pool(token_a: &str, token_b: &str) -> PoolInfo {
        PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: "pool".into(),
            token_a: crate::protocol::TokenInfo {
                mint: token_a.into(),
                decimals: if token_a == "So11111111111111111111111111111111111111112" {
                    9
                } else {
                    6
                },
                token_program: "token".into(),
                transfer_fee: None,
            },
            token_b: crate::protocol::TokenInfo {
                mint: token_b.into(),
                decimals: 6,
                token_program: "token".into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: None,
            fee_bps: None,
            protocol_state: Value::Null,
        }
    }

    #[test]
    fn plan_swap_credit_requires_opposite_exact_pool_side() {
        const WSOL: &str = "So11111111111111111111111111111111111111112";
        let wsol_usdc = pool(WSOL, "usdc");
        assert_eq!(
            swap_output_credit(&wsol_usdc, "native", "usdc")
                .unwrap()
                .reference,
            "usdc"
        );
        assert_eq!(
            swap_output_credit(&wsol_usdc, "usdc", "native")
                .unwrap()
                .reference,
            "native"
        );
        assert!(swap_output_credit(&wsol_usdc, "native", "native").is_err());
        assert!(swap_output_credit(&pool("a", "b"), "native", "b").is_err());
        assert!(swap_output_credit(&wsol_usdc, "usdc", "usdc").is_err());
    }

    #[test]
    fn signer_identity_key_is_scoped_to_owner_family_pool_action_and_request() {
        let logical = "plan-execution:plan-a:revision-1:open-position:logical-request";
        let first = scoped_signer_operation_key(
            logical,
            "wallet-a",
            ProtocolFamily::Dlmm,
            "pool-a",
            MeteoraAction::Open,
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                "plan-execution:plan-a:revision-1:other-step:logical-request",
                "wallet-a",
                ProtocolFamily::Dlmm,
                "pool-a",
                MeteoraAction::Open,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                logical,
                "wallet-b",
                ProtocolFamily::Dlmm,
                "pool-a",
                MeteoraAction::Open,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                logical,
                "wallet-a",
                ProtocolFamily::DammV2,
                "pool-a",
                MeteoraAction::Open,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                logical,
                "wallet-a",
                ProtocolFamily::Dlmm,
                "pool-b",
                MeteoraAction::Open,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                logical,
                "wallet-a",
                ProtocolFamily::Dlmm,
                "pool-a",
                MeteoraAction::Add,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                logical,
                "wallet-a",
                ProtocolFamily::Dlmm,
                "pool-b",
                MeteoraAction::Open,
            )
        );
        assert_ne!(
            first,
            scoped_signer_operation_key(
                "plan-execution:plan-a:revision-2:open-position:logical-request",
                "wallet-a",
                ProtocolFamily::Dlmm,
                "pool-a",
                MeteoraAction::Open,
            )
        );
    }
}

#[cfg(test)]
mod backing_valuation_tests {
    use super::*;
    use gofin_domain::{
        LiquidityCompositionAmount, LiquidityPositionEvent, PositionAnchor, PositionState,
    };

    fn asset(reference: &str, decimals: u8) -> AssetId {
        AssetId {
            network: NETWORK.into(),
            reference: reference.into(),
            decimals,
        }
    }

    #[test]
    fn exact_backing_value_is_separate_from_synthetic_units_and_fungible_holdings() {
        let evidence = LiquidityPositionEvidence {
            version: 1,
            family: "dlmm".into(),
            pool: "pool".into(),
            position: "position".into(),
            anchor_position_id: Some("meteora:liquidity:dlmm:position".into()),
            observed_slot: Some(42),
            pre_transaction_units_atoms: None,
            liquidity_units_atoms: Some(gofin_domain::Quantity::new("17").unwrap()),
            liquidity_units_known: true,
            historical_principal_known: false,
            token_a: LiquidityCompositionAmount {
                asset: asset("usdc-mint", 6),
                token_program: "token".into(),
                atoms: Some(gofin_domain::Quantity::new("1000000").unwrap()),
            },
            token_b: LiquidityCompositionAmount {
                asset: asset("sol-mint", 9),
                token_program: "token".into(),
                atoms: Some(gofin_domain::Quantity::new("2000000000").unwrap()),
            },
            backing_valuation: None,
            history: Vec::<LiquidityPositionEvent>::new(),
        };
        let quotes = [
            UsdQuote {
                usd_per_unit: "1".into(),
                updated_at: "2026-10-14T00:00:00Z".into(),
                observed_at: "2026-10-14T00:00:00Z".into(),
                source: "test".into(),
            },
            UsdQuote {
                usd_per_unit: "100".into(),
                updated_at: "2026-10-14T00:00:00Z".into(),
                observed_at: "2026-10-14T00:00:00Z".into(),
                source: "test".into(),
            },
        ];
        let backing = calculate_backing_valuation(
            &evidence,
            evidence.token_a.atoms.as_ref().unwrap(),
            evidence.token_b.atoms.as_ref().unwrap(),
            &quotes,
        )
        .unwrap();
        assert_eq!(
            backing.token_a_usd_micros,
            gofin_domain::Quantity::new("1000000").unwrap()
        );
        assert_eq!(
            backing.token_b_usd_micros,
            gofin_domain::Quantity::new("200000000").unwrap()
        );
        assert_eq!(
            backing.total_usd_micros,
            gofin_domain::Quantity::new("201000000").unwrap()
        );

        let synthetic_asset = AssetId {
            network: NETWORK.into(),
            reference: "meteora:liquidity:dlmm:position".into(),
            decimals: 0,
        };
        let principal = Amount {
            asset: synthetic_asset.clone(),
            atoms: evidence.liquidity_units_atoms.clone().unwrap(),
        };
        let resource = ResourceId("solana:mainnet-beta:wallet".into());
        let state_position = PositionState {
            resource: resource.clone(),
            position: "meteora:liquidity:dlmm:position".into(),
            kind: PositionKind::Liquidity,
            amount: principal.clone(),
            scaled_receipt: None,
            anchor: Some(PositionAnchor {
                resource,
                position: "meteora:liquidity:dlmm:position".into(),
                position_kind: PositionKind::Liquidity,
                principal: principal.clone(),
                scaled_receipt: None,
                evidence_version: 3,
                evidence: json!({"liquidity_position": evidence}),
                observed_at: "2026-10-14T00:00:00Z".into(),
            }),
        };
        let state = FinancialState {
            positions: vec![state_position],
            ..FinancialState::default()
        };
        assert!(state.holdings.is_empty());
        assert_eq!(state.positions[0].amount, principal);
        assert_ne!(
            backing.total_usd_micros,
            gofin_domain::Quantity::from_u64(17)
        );
    }
}

#[cfg(test)]
mod liquidity_exit_constraint_tests {
    use super::*;
    use crate::protocol::QuoteInfo;

    #[test]
    fn collect_authorizes_only_pool_and_quoted_reward_mints_without_user_floors() {
        let intent: MeteoraIntent = serde_json::from_value(json!({
            "wallet":"wallet",
            "family":"dlmm",
            "action":"collect",
            "pool":"pool",
            "maximum_slippage_bps":0,
            "maximum_network_fee_lamports":10000,
            "maximum_account_funding_lamports":0
        }))
        .unwrap();
        let candidate = ProtocolCandidate {
            quote: QuoteInfo {
                family: ProtocolFamily::Dlmm,
                action: MeteoraAction::Collect,
                pool: "pool".into(),
                position: Some("position".into()),
                token_a_debit_atoms: None,
                token_b_debit_atoms: None,
                token_a_credit_atoms: Some("0".into()),
                token_b_credit_atoms: Some("0".into()),
                receipt_delta_atoms: Some("0".into()),
                fees_a_atoms: None,
                fees_b_atoms: None,
                rewards: vec![crate::protocol::PositionReward {
                    mint: "reward-mint".into(),
                    decimals: 8,
                    token_program: "reward-token-program".into(),
                    amount_atoms: "9".into(),
                }],
                expires_at: "2030-01-01T00:00:00Z".into(),
                price_impact_bps: None,
                native_cash_delta: None,
                protocol_details: Value::Null,
            },
            instructions: vec![],
            lookup_tables: vec![],
            blockhash: "blockhash".into(),
            last_valid_block_height: 1,
            stable_position_address: Some("position".into()),
            required_secondary_signers: vec![],
            metadata: Value::Null,
        };
        let resource = ResourceId("solana:mainnet-beta:wallet".into());
        let token_a = AssetId {
            network: NETWORK.into(),
            reference: "token-a".into(),
            decimals: 9,
        };
        let token_b = AssetId {
            network: NETWORK.into(),
            reference: "token-b".into(),
            decimals: 6,
        };
        let liquidity_asset = AssetId {
            network: NETWORK.into(),
            reference: "meteora:liquidity:dlmm:position".into(),
            decimals: 0,
        };
        let constraints = liquidity_exit_constraints(
            &intent,
            &candidate,
            &resource,
            &token_a,
            &token_b,
            "meteora:dlmm:position:liquidity_units",
            &liquidity_asset,
        )
        .unwrap();
        assert_eq!(constraints.len(), 4);
        for asset in [&token_a, &token_b] {
            assert!(constraints.iter().any(|constraint| matches!(
                constraint,
                EffectConstraint::Credit { to, asset: credited, minimum_credit }
                    if to == &resource && credited == asset && minimum_credit.is_zero()
            )));
        }
        assert!(constraints.iter().any(|constraint| matches!(
            constraint,
            EffectConstraint::Credit { to, asset, minimum_credit }
                if to == &resource && asset.reference == "reward-mint" && asset.decimals == 8 && minimum_credit.is_zero()
        )));
        assert!(constraints.iter().any(|constraint| matches!(
            constraint,
            EffectConstraint::PositionOfKind { position, position_kind: PositionKind::Liquidity, maximum_change, .. }
                if position == "meteora:dlmm:position:liquidity_units" && maximum_change.is_zero()
        )));
    }
}
