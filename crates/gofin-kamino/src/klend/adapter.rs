//! Direct KLend lifecycle over the broker-owned Solana wallet runtime.
use super::{boundaries, settlement, types::*};
use crate::{
    adapter::{asset, atoms, native_asset, resource},
    common,
};
use async_trait::async_trait;
use chrono::Utc;
use gofin_domain::*;
use gofin_pricing::UsdPriceSource;
use gofin_solana::{
    SolanaWalletAdapter,
    transaction::{FrozenTransaction, ReceiptOutcome},
};
use num_bigint::BigUint;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

pub struct KlendAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocol: Arc<dyn KlendProtocol>,
    prices: Arc<dyn UsdPriceSource>,
}
impl KlendAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        protocol: Arc<dyn KlendProtocol>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        if wallets.is_empty() || wallets.keys().any(|name| name.trim().is_empty()) {
            return Err(invalid("KLend requires enrolled Solana wallets."));
        }
        Ok(Self {
            wallets,
            protocol,
            prices,
        })
    }
    fn wallet(&self, name: &str) -> Result<&Arc<SolanaWalletAdapter>> {
        self.wallets
            .get(name)
            .ok_or_else(|| invalid("Select an enrolled Solana wallet."))
    }
    async fn resolve(
        &self,
        operation: &str,
        input: &Value,
        resolve_all: bool,
    ) -> Result<(LendingIntent, ReserveInfo, ObligationInfo)> {
        let public: PublicIntent = serde_json::from_value(input.clone())
            .map_err(|_| invalid("Invalid KLend input; discover the exact operation schema."))?;
        if public.network != "solana"
            || public.cluster != gofin_solana::CLUSTER
            || public.maximum_network_fee_lamports == 0
        {
            return Err(invalid(
                "KLend requires Solana mainnet identities and a finite nonzero network fee cap.",
            ));
        }
        let action = action(operation, public.action)?;
        let wallet = self.wallet(&public.wallet)?;
        for key in [
            &public.market,
            &public.reserve,
            &public.obligation,
            &public.token_mint,
        ] {
            gofin_solana::parse_address(key)?;
        }
        let reserve = self
            .protocol
            .reserve(&wallet.runtime(), &public.market, &public.reserve)
            .await?;
        if reserve.address != public.reserve
            || reserve.market != public.market
            || reserve.token.mint != public.token_mint
        {
            return Err(invalid(
                "Canonical KLend market, reserve or underlying mint differs.",
            ));
        }
        let obligation = self
            .protocol
            .obligation(
                &wallet.runtime(),
                wallet.address(),
                &public.market,
                &public.obligation,
            )
            .await?;
        validate_obligation(
            &obligation,
            wallet.address(),
            &public.market,
            &public.obligation,
        )?;
        if !obligation.writes_available
            || !obligation.complete
            || obligation.tag != 0
            || obligation.elevation_group != 0
        {
            return Err(invalid(
                "This obligation does not support complete vanilla floating-debt writes; inspect its restrictions.",
            ));
        }
        let selected = if action == LendingAction::Withdraw {
            if public.amount_atoms.is_some() || public.maximum_repay_atoms.is_some() {
                return Err(invalid(
                    "Collateral withdrawal takes collateral_atoms only.",
                ));
            }
            public
                .collateral_atoms
                .as_deref()
                .ok_or_else(|| invalid("Exact collateral_atoms or all is required."))?
        } else {
            if public.collateral_atoms.is_some() {
                return Err(invalid("Only withdrawal accepts collateral_atoms."));
            }
            public
                .amount_atoms
                .as_deref()
                .ok_or_else(|| invalid("Exact amount_atoms is required."))?
        };
        let all = selected == "all";
        let amount = match (action, all) {
            (LendingAction::Repay, true) => atoms(
                public
                    .maximum_repay_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Repay all requires a finite maximum_repay_atoms."))?,
            )?,
            (LendingAction::Withdraw, true) if resolve_all => {
                let position = obligation
                    .positions
                    .iter()
                    .find(|p| p.reserve == reserve.address && p.side == LendingSide::Supply)
                    .ok_or_else(|| invalid("No collateral exists in this reserve."))?;
                atoms(&position.native_units)?
            }
            (LendingAction::Withdraw, true) => Quantity::from_u64(1),
            (_, true) => return Err(invalid("Only repay and collateral withdrawal support all.")),
            (_, false) => {
                if public.maximum_repay_atoms.is_some() {
                    return Err(invalid("maximum_repay_atoms is valid only for repay all."));
                }
                atoms(selected)?
            }
        };
        if amount.is_zero() || amount > Quantity::from_u64(u64::MAX - 1) {
            return Err(invalid(
                "The native quantity must be positive and finite; unbounded sentinels are forbidden.",
            ));
        }
        atoms(&public.maximum_protocol_fee_atoms)?;
        if matches!(action, LendingAction::Borrow | LendingAction::Withdraw) {
            validate_risk_limits(public.risk_limits.as_ref().ok_or_else(|| {
                invalid("Borrow and collateral removal require explicit obligation risk limits.")
            })?)?;
        }
        if action == LendingAction::Borrow {
            if atoms(
                public
                    .maximum_debt_increase_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Borrow requires a finite gross debt-increase cap."))?,
            )? < amount
            {
                return Err(invalid(
                    "Gross debt cap cannot be smaller than the exact borrow receipt.",
                ));
            }
        } else if public.maximum_debt_increase_atoms.is_some() {
            return Err(invalid("Only borrowing accepts a debt-increase cap."));
        }
        if action == LendingAction::Withdraw && public.minimum_receive_atoms.is_none() {
            return Err(invalid(
                "Withdrawal requires an explicit minimum expected receipt.",
            ));
        }
        if let Some(min) = &public.minimum_receive_atoms {
            if action != LendingAction::Withdraw {
                return Err(invalid(
                    "Only collateral withdrawal accepts a receipt floor.",
                ));
            }
            atoms(min)?;
        }
        Ok((
            LendingIntent {
                wallet: public.wallet,
                action,
                market: public.market,
                reserve: public.reserve,
                obligation: public.obligation,
                token: reserve.token.clone(),
                amount_atoms: amount.to_string(),
                all,
                minimum_receive_atoms: public.minimum_receive_atoms,
                maximum_debt_increase_atoms: public.maximum_debt_increase_atoms,
                maximum_protocol_fee_atoms: public.maximum_protocol_fee_atoms,
                risk_limits: public.risk_limits,
                maximum_network_fee_lamports: public.maximum_network_fee_lamports,
                maximum_account_funding_lamports: public.maximum_account_funding_lamports,
            },
            reserve,
            obligation,
        ))
    }
    async fn candidate(
        &self,
        intent: &LendingIntent,
        reserve: &ReserveInfo,
    ) -> Result<LendingCandidate> {
        let wallet = self.wallet(&intent.wallet)?;
        let candidate = self
            .protocol
            .build_candidate(&wallet.runtime(), intent, wallet.address())
            .await?;
        if candidate.reserve.address != reserve.address
            || candidate.reserve.token != reserve.token
            || candidate.reserve.collateral_token != reserve.collateral_token
        {
            return Err(invalid(
                "Native candidate changed the selected reserve assets.",
            ));
        }
        validate_candidate(intent, wallet.address(), &candidate)?;
        self.protocol
            .validate_candidate(&wallet.runtime(), intent, wallet.address(), &candidate)
            .await?;
        Ok(candidate)
    }
    async fn prepare_write(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        if !matches!(operation, OP_SUPPLY | OP_WITHDRAW | OP_BORROW | OP_REPAY) {
            return Err(invalid("Unknown KLend write operation."));
        }
        let (intent, reserve, _) = self.resolve(operation, input, true).await?;
        let wallet = self.wallet(&intent.wallet)?;
        let signer = wallet
            .signer()
            .ok_or_else(|| invalid("The selected wallet cannot sign."))?;
        let mut candidate = self.candidate(&intent, &reserve).await?;
        let mut rent_observations = Vec::new();
        for row in rent_limits(&candidate)? {
            let address = row
                .position
                .strip_prefix("account-funding:")
                .ok_or_else(|| invalid("Invalid account funding identity."))?;
            let response = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([address,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            if response.get("value") != Some(&Value::Null)
                || response["context"]["slot"].as_u64().is_none()
            {
                return Err(invalid(
                    "New account funding requires a current finalized absence observation.",
                ));
            }
            rent_observations.push(json!({"account":address,"context_slot":response["context"]["slot"],"observed_at":Utc::now().to_rfc3339(),"commitment":"finalized","account_absence_proven":true}));
        }
        candidate.metadata["account_funding_observations"] = json!(rent_observations);
        let valuation = self.valuation(&intent, &candidate.quote).await?;
        (candidate.blockhash, candidate.last_valid_block_height) =
            common::latest_blockhash(&wallet.runtime()).await?;
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
            .await?;
        validate_signer(wallet.address(), &frozen)?;
        wallet.runtime().simulate(&frozen).await?;
        let fee = wallet.runtime().validate_fee(&frozen).await?;
        let mut prepared = build_prepared(
            operation,
            input,
            &intent,
            wallet.address(),
            &candidate,
            &frozen,
            fee,
            context,
        )?;
        prepared.valuation = Some(valuation);
        Ok(prepared)
    }
    async fn freeze(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<(Arc<SolanaWalletAdapter>, FrozenTransaction)> {
        let intent = bound_intent(prepared)?;
        let wallet = self.wallet(&intent.wallet)?.clone();
        let candidate = candidate_from(prepared)?;
        validate_candidate(&intent, wallet.address(), &candidate)?;
        validate_baselines(prepared, &candidate)?;
        self.protocol
            .validate_candidate(&wallet.runtime(), &intent, wallet.address(), &candidate)
            .await?;
        let signer = wallet
            .signer()
            .ok_or_else(|| invalid("Wallet cannot sign."))?;
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
            .await?;
        validate_signer(wallet.address(), &frozen)?;
        if prepared.binding["message_sha256"].as_str() != Some(frozen.message_sha256()) {
            return Err(invalid("The approved native lending message changed."));
        }
        wallet.runtime().validate_fee(&frozen).await?;
        Ok((wallet, frozen))
    }
    async fn valuation(&self, intent: &LendingIntent, quote: &LendingQuote) -> Result<Valuation> {
        let principal = if intent.action == LendingAction::Withdraw {
            quote
                .credits
                .iter()
                .filter_map(|r| r.atoms.as_deref())
                .try_fold(Quantity::zero(), |sum, n| sum.checked_add(&atoms(n)?))?
        } else if intent.action == LendingAction::Borrow {
            atoms(
                intent
                    .maximum_debt_increase_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Missing debt cap."))?,
            )?
        } else {
            atoms(&intent.amount_atoms)?
        };
        let native = native_asset();
        let underlying = asset(&intent.token);
        let tokens = vec![underlying.clone(), native.clone()];
        let quotes = self.prices.quotes(&tokens).await?;
        if quotes.len() != 2 {
            return Err(invalid("Incomplete lending valuation prices."));
        }
        let mut prices = Vec::new();
        let mut movement = Quantity::zero();
        let mut fee = Quantity::zero();
        let mut funding = Quantity::zero();
        let mut sources = BTreeSet::new();
        for (token, quote) in tokens.into_iter().zip(quotes) {
            let price = Quantity::from_decimal(&quote.usd_per_unit, 36)?;
            if price.is_zero() {
                return Err(invalid("Valuation requires positive prices."));
            }
            let unit = Quantity::new(format!("1{}", "0".repeat(usize::from(token.decimals) + 30)))?;
            if token == underlying {
                movement = principal.mul_div_ceil(&price, &unit)?;
                fee = fee.checked_add(
                    &atoms(&intent.maximum_protocol_fee_atoms)?.mul_div_ceil(&price, &unit)?,
                )?;
            } else {
                fee = fee.checked_add(
                    &Quantity::from_u64(intent.maximum_network_fee_lamports)
                        .mul_div_ceil(&price, &unit)?,
                )?;
                funding = Quantity::from_u64(intent.maximum_account_funding_lamports)
                    .mul_div_ceil(&price, &unit)?;
            }
            sources.insert(quote.source.clone());
            prices.push(AssetPrice {
                asset: token,
                usd_micros_per_unit: price,
                unit_atoms: unit,
                observed_at: quote.updated_at,
                source: quote.source,
            });
        }
        Ok(Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: fee,
            additional_allocation_usd_micros: funding,
            observed_at: Utc::now().to_rfc3339(),
            source: sources.into_iter().collect::<Vec<_>>().join(";"),
            prices,
        })
    }
    async fn snapshots(&self, scope: Option<&SnapshotScope>) -> Result<StateSnapshot> {
        let mut positions = Vec::new();
        let mut coverage = Vec::new();
        let mut pending = Vec::new();
        let mut complete = true;
        let mut seen = BTreeSet::new();
        for wallet in self.wallets.values() {
            let resource = resource(wallet.address());
            if !seen.insert(resource.clone())
                || scope.is_some_and(|s| {
                    !s.matches_resource(&resource) || !s.matches_network(gofin_solana::NETWORK)
                })
            {
                continue;
            }
            match self
                .protocol
                .obligations(
                    &wallet.runtime(),
                    wallet.address(),
                    None,
                    &LendingQuery {
                        limit: 100,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(page) => {
                    let mut covered = page.complete && page.next_cursor.is_none();
                    pending.extend(page.warnings);
                    for obligation in page.obligations {
                        validate_obligation(
                            &obligation,
                            wallet.address(),
                            &obligation.market,
                            &obligation.address,
                        )?;
                        covered &= obligation.complete;
                        if obligation.positions.iter().any(|p| {
                            p.side == LendingSide::Debt
                                && p.protocol_state["debt_kind"] != "floating"
                        }) {
                            covered = false;
                            pending.push(format!("KLend obligation {} has non-floating or unclassified debt excluded from floating-debt portfolio accounting.", obligation.address));
                        }
                        for mut state in settlement::snapshot_positions(&obligation)? {
                            settlement::value_position(&mut state, self.prices.as_ref()).await?;
                            positions.push(state);
                        }
                    }
                    if covered {
                        coverage.push(resource.0);
                    }
                    complete &= covered;
                }
                Err(error) => {
                    complete = false;
                    pending.push(format!("KLend positions unavailable: {}", error.message));
                }
            }
        }
        Ok(StateSnapshot {
            state: FinancialState {
                positions,
                ..Default::default()
            },
            observed_at: Utc::now().to_rfc3339(),
            complete,
            coverage,
            pending,
        })
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> GoFinError {
    GoFinError::validation(message)
}
pub(crate) fn bound_intent(prepared: &PreparedOperation) -> Result<LendingIntent> {
    serde_json::from_value(prepared.binding["intent"].clone())
        .map_err(|_| invalid("Missing native lending intent binding."))
}
pub(crate) fn candidate_from(prepared: &PreparedOperation) -> Result<LendingCandidate> {
    serde_json::from_value(prepared.binding["protocol_candidate"].clone())
        .map_err(|_| invalid("Missing native lending candidate binding."))
}
fn action(operation: &str, quote: Option<LendingAction>) -> Result<LendingAction> {
    match operation {
        OP_SUPPLY => Ok(LendingAction::Supply),
        OP_WITHDRAW => Ok(LendingAction::Withdraw),
        OP_BORROW => Ok(LendingAction::Borrow),
        OP_REPAY => Ok(LendingAction::Repay),
        OP_QUOTE => quote.ok_or_else(|| invalid("Quote requires an explicit action.")),
        _ => Err(invalid("Unknown KLend operation.")),
    }
    .and_then(|action| {
        if operation != OP_QUOTE && quote.is_some() {
            Err(invalid("Only quote accepts action."))
        } else {
            Ok(action)
        }
    })
}
fn validate_signer(owner: &str, frozen: &FrozenTransaction) -> Result<()> {
    if frozen.signer() != owner
        || frozen.fee_payer() != owner
        || frozen.required_signers() != [owner]
        || !frozen.missing_cosigners().is_empty()
    {
        return Err(invalid(
            "Only the selected protected wallet may sign a lending transaction.",
        ));
    }
    Ok(())
}
pub(crate) fn validate_obligation(
    obligation: &ObligationInfo,
    owner: &str,
    market: &str,
    address: &str,
) -> Result<()> {
    if obligation.owner != owner
        || obligation.market != market
        || obligation.address != address
        || obligation.program != klend_interface::KLEND_PROGRAM_ID.to_string()
    {
        return Err(invalid(
            "Obligation ownership, market, address or program differs.",
        ));
    }
    let mut seen = BTreeSet::new();
    for position in &obligation.positions {
        gofin_solana::parse_address(&position.reserve)?;
        gofin_solana::parse_address(&position.token.mint)?;
        if !seen.insert((&position.reserve, position.side)) {
            return Err(invalid("Duplicate obligation reserve position."));
        }
        if position.side == LendingSide::Supply {
            atoms(&position.native_units)?;
        } else {
            fraction(&position.native_units)?;
        }
        if let Some(index) = &position.cumulative_borrow_rate_bsf {
            fraction(index)?;
        }
        if let Some(n) = &position.underlying_atoms {
            atoms(n)?;
        }
        if position.side == LendingSide::Supply && position.collateral_token.is_none() {
            return Err(invalid("Supply position has no canonical collateral mint."));
        }
    }
    Ok(())
}
pub(crate) fn fraction(raw: &str) -> Result<Quantity> {
    let q = Quantity::new(raw)?;
    if q.as_str() != raw || q.integer().bits() > 256 {
        return Err(invalid(
            "Native protocol fraction must be canonical unsigned U256 decimal text.",
        ));
    }
    Ok(q)
}
fn big(raw: &str) -> Result<BigUint> {
    let q = fraction(raw)?;
    BigUint::parse_bytes(q.as_str().as_bytes(), 10)
        .ok_or_else(|| invalid("Invalid protocol fraction."))
}
pub(crate) fn validate_risk_limits(limits: &RiskLimits) -> Result<()> {
    if limits.minimum_health_factor_bps <= 10000
        || limits.maximum_debt_to_collateral_bps == 0
        || limits.maximum_debt_to_collateral_bps >= 10000
    {
        return Err(invalid(
            "Risk limits require health greater than1 and an adjusted debt ratio below100%.",
        ));
    }
    atoms(&limits.maximum_debt_usd_micros)?;
    Ok(())
}
pub(crate) fn validate_risk(risk: &LendingRisk, limits: &RiskLimits) -> Result<()> {
    validate_risk_limits(limits)?;
    if !risk.refreshed {
        return Err(invalid(
            "Risk limits require refreshed native obligation evidence.",
        ));
    }
    let debt = big(&risk.borrow_factor_adjusted_debt_value_sf)?;
    let collateral = big(&risk.deposited_value_sf)?;
    let unhealthy = big(&risk.unhealthy_borrow_value_sf)?;
    if &debt * BigUint::from(limits.minimum_health_factor_bps)
        > &unhealthy * BigUint::from(10000u64)
        || &debt * BigUint::from(10000u64)
            > &collateral * BigUint::from(limits.maximum_debt_to_collateral_bps)
        || big(&risk.borrowed_assets_market_value_sf)? * BigUint::from(1_000_000u64)
            > big(&limits.maximum_debt_usd_micros)? * (BigUint::from(1u64) << 60)
        || debt > big(&risk.allowed_borrow_value_sf)?
    {
        return Err(invalid(
            "The refreshed obligation exceeds reviewed debt, collateral or health limits.",
        ));
    }
    Ok(())
}

#[async_trait]
impl Adapter for KlendAdapter {
    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets.iter().map(|(name,w)|ConnectionMetadata{name:name.clone(),provider:"kamino".into(),resource:resource(w.address()),role:Some("lending".into()),parent:None,network:Some(gofin_solana::NETWORK.into()),address:Some(w.address().into()),credential_ref:None,signing_enabled:w.signer().is_some(),details:json!({"program":klend_interface::KLEND_PROGRAM_ID.to_string(),"cluster":gofin_solana::CLUSTER,"obligations":"vanilla_floating"})}).collect()
    }
    fn uses_rpc_reads(&self, _: &str, _: &Value) -> bool {
        true
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.snapshots(None).await
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        self.snapshots(Some(scope)).await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        if operation == OP_QUOTE {
            let (intent, reserve, _) = self.resolve(operation, input, true).await?;
            let candidate = self.candidate(&intent, &reserve).await?;
            return Ok(
                json!({"intent":intent,"reserve":candidate.reserve,"obligation":candidate.obligation_before,"quote":candidate.quote}),
            );
        }
        let r: ReadInput = serde_json::from_value(input.clone())
            .map_err(|_| invalid("Invalid KLend read input."))?;
        let wallet = match r.wallet.as_deref() {
            Some(name) => self.wallet(name)?,
            None => self
                .wallets
                .values()
                .next()
                .ok_or_else(|| invalid("No Solana connection."))?,
        };
        let limit = r.limit.unwrap_or(25);
        if !(1..=100).contains(&limit) {
            return Err(invalid("Read limit must be1–100."));
        }
        let query = LendingQuery {
            search: r.search.clone(),
            token_mint: r.token_mint.clone(),
            limit,
            cursor: r.cursor.clone(),
        };
        let market = || {
            r.market
                .as_deref()
                .ok_or_else(|| invalid("An explicit lending market is required."))
        };
        match operation {
            OP_MARKETS if r.market.is_some() => Ok(json!(MarketPage {
                markets: vec![self.protocol.market(&wallet.runtime(), market()?).await?],
                complete: true,
                next_cursor: None,
                warnings: vec![]
            })),
            OP_MARKETS => Ok(json!(
                self.protocol.markets(&wallet.runtime(), &query).await?
            )),
            OP_MARKET => Ok(json!(
                self.protocol.market(&wallet.runtime(), market()?).await?
            )),
            OP_RESERVES if r.reserve.is_some() => Ok(json!(ReservePage {
                reserves: vec![
                    self.protocol
                        .reserve(
                            &wallet.runtime(),
                            market()?,
                            r.reserve.as_deref().expect("present")
                        )
                        .await?
                ],
                complete: true,
                next_cursor: None,
                warnings: vec![]
            })),
            OP_RESERVES => Ok(json!(
                self.protocol
                    .reserves(&wallet.runtime(), market()?, &query)
                    .await?
            )),
            OP_RESERVE => Ok(json!(
                self.protocol
                    .reserve(
                        &wallet.runtime(),
                        market()?,
                        r.reserve
                            .as_deref()
                            .ok_or_else(|| invalid("Reserve is required."))?
                    )
                    .await?
            )),
            OP_OBLIGATION | OP_OBLIGATIONS
                if operation == OP_OBLIGATION || r.obligation.is_some() =>
            {
                if r.wallet.is_none() {
                    return Err(invalid(
                        "Wallet selection is required for obligation positions.",
                    ));
                }
                let default_address;
                let address = if let Some(address) = r.obligation.as_deref() {
                    address
                } else {
                    default_address = self
                        .protocol
                        .default_obligation_address(wallet.address(), market()?)?;
                    &default_address
                };
                gofin_solana::parse_address(address)?;
                let o = self
                    .protocol
                    .obligation(&wallet.runtime(), wallet.address(), market()?, address)
                    .await?;
                validate_obligation(&o, wallet.address(), market()?, address)?;
                let value = settlement::public_obligation(&o, self.prices.as_ref()).await?;
                Ok(if operation == OP_OBLIGATIONS {
                    json!({"obligations":[value],"complete":o.complete,"next_cursor":null,"warnings":o.warnings})
                } else {
                    value
                })
            }
            OP_OBLIGATIONS => {
                if r.wallet.is_none() {
                    return Err(invalid("Wallet is required for obligations."));
                }
                let page = self
                    .protocol
                    .obligations(
                        &wallet.runtime(),
                        wallet.address(),
                        r.market.as_deref(),
                        &query,
                    )
                    .await?;
                let mut rows = Vec::new();
                for o in page.obligations {
                    validate_obligation(&o, wallet.address(), &o.market, &o.address)?;
                    rows.push(settlement::public_obligation(&o, self.prices.as_ref()).await?);
                }
                Ok(
                    json!({"obligations":rows,"complete":page.complete,"next_cursor":page.next_cursor,"warnings":page.warnings}),
                )
            }
            _ => Err(invalid("Unknown or incomplete KLend read operation.")),
        }
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.prepare_write(operation, input, &PreparationContext::default())
            .await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.prepare_write(operation, input, context).await
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        raw: &Value,
    ) -> Result<gofin_domain::boundaries::BoundarySpec> {
        boundaries::prepare(prepared, raw)
    }
    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        boundaries::validate(spec, prepared)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        raw: &Value,
    ) -> Result<gofin_domain::financial_plans::PlanPermission> {
        let (mut intent, reserve, obligation) = self.resolve(operation, input, false).await?;
        if intent.all && intent.action == LendingAction::Withdraw {
            intent.amount_atoms = raw["maximum_amount_atoms"]
                .as_str()
                .ok_or_else(|| {
                    invalid("Withdraw-all permission requires a finite collateral-unit cap.")
                })?
                .into();
        }
        boundaries::permission(
            &intent,
            self.wallet(&intent.wallet)?.address(),
            &reserve,
            &obligation,
            raw,
        )
    }
    fn plan_has_finite_non_cash_source(
        &self,
        operation: &str,
        permission: &gofin_domain::financial_plans::PlanPermission,
        target: &ResourceAmount,
    ) -> bool {
        boundaries::finite_withdrawal_source(operation, &permission.spec, target)
    }
    fn preparation_reuse_window(&self, _: &PreparedOperation) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let intent = bound_intent(prepared)?;
        let c = candidate_from(prepared)?;
        validate_candidate(&intent, self.wallet(&intent.wallet)?.address(), &c)?;
        validate_baselines(prepared, &c)?;
        if prepared.binding["last_valid_block_height"].as_u64() != Some(c.last_valid_block_height) {
            return Err(invalid("Approved lending lifetime changed."));
        }
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let (wallet, frozen) = self.freeze(prepared).await?;
        let signer = wallet
            .signer()
            .ok_or_else(|| invalid("Wallet cannot sign."))?;
        wallet
            .runtime()
            .dispatch(&frozen, &signer, prepared, journal)
            .await
    }
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        self.submit(validated.prepared(), journal).await
    }
    async fn verify_non_dispatch_expiry(
        &self,
        record: &OperationRecord,
    ) -> Result<Option<VerifiedNonDispatchExpiry>> {
        let (Some(submission), Some(verified), Some(dispatch)) = (
            &record.submission,
            &record.effects,
            &record.dispatch_evidence,
        ) else {
            return Ok(None);
        };
        if record.status != Status::Failed
            || dispatch["no_network_send"] != true
            || dispatch.get("chain_signature").is_some()
            || dispatch["message_sha256"] != submission.data["message_sha256"]
            || verified.submission != *submission
            || !verified.effects.is_empty()
            || verified_effect_digest(verified)? != verified.proof_digest
            || verified.evidence["state"] != "not_dispatched"
            || verified.evidence["reason_code"] != dispatch["reason_code"]
        {
            return Ok(None);
        }
        let kind = match dispatch["reason_code"].as_str() {
            Some("SOLANA_BLOCKHASH_EXPIRED" | "SOLANA_TX_EXPIRED") => {
                CandidateExpiryKind::TransactionLifetime
            }
            Some("PREVIEW_EXPIRED") => CandidateExpiryKind::Quote,
            _ => return Ok(None),
        };
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| invalid("Missing persisted lending preview."))?;
        let intent = bound_intent(prepared)?;
        let outcome = self
            .wallet(&intent.wallet)?
            .runtime()
            .reconcile_submission(submission, Some(dispatch))
            .await?;
        if !matches!(outcome,ReceiptOutcome::NotDispatched{ref reason_code,..} if dispatch["reason_code"].as_str()==Some(reason_code.as_str()))
        {
            return Ok(None);
        }
        Ok(Some(VerifiedNonDispatchExpiry::from_verified_effects(
            &record.id, submission, verified, kind,
        )?))
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let submission = record
            .submission
            .as_ref()
            .ok_or_else(|| invalid("Missing durable lending submission."))?;
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| invalid("Missing durable lending preview."))?;
        let intent = bound_intent(prepared)?;
        let wallet = self.wallet(&intent.wallet)?;
        let bundle = match wallet
            .runtime()
            .reconcile_submission(submission, record.dispatch_evidence.as_ref())
            .await?
        {
            ReceiptOutcome::Pending { confirmed, reason } => {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"confirmed":confirmed,"reason":reason}),
                    verified: None,
                });
            }
            ReceiptOutcome::NotDispatched {
                reason_code,
                reason,
            } => (
                Status::Failed,
                crate::settlement::verify_not_dispatched(submission, &reason_code, &reason)?,
            ),
            ReceiptOutcome::ExpiredWithoutInclusion {
                signature,
                message_sha256,
                reason,
                proof,
            } => (
                Status::Failed,
                crate::settlement::verify_expired_without_inclusion(
                    submission,
                    &signature,
                    &message_sha256,
                    &reason,
                    &proof,
                )?,
            ),
            ReceiptOutcome::Finalized { failed, proof, .. } => {
                if failed {
                    (
                        Status::Failed,
                        crate::settlement::verify_failed(
                            submission,
                            &proof,
                            wallet.address(),
                            intent.maximum_network_fee_lamports,
                        )?,
                    )
                } else {
                    let candidate = candidate_from(prepared)?;
                    let verified = self
                        .protocol
                        .verify_finalized(
                            &wallet.runtime(),
                            &intent,
                            wallet.address(),
                            &candidate,
                            &proof,
                        )
                        .await?;
                    (
                        Status::Completed,
                        settlement::verify(record, &intent, &candidate, &verified, &proof)?,
                    )
                }
            }
        };
        Ok(Reconciliation {
            status: bundle.0,
            evidence: bundle.1.evidence.clone(),
            verified: Some(bundle.1),
        })
    }
}

pub(crate) fn validate_candidate(
    intent: &LendingIntent,
    owner: &str,
    c: &LendingCandidate,
) -> Result<()> {
    let expires = chrono::DateTime::parse_from_rfc3339(&c.quote.expires_at)
        .map_err(|_| invalid("Native quote expiry is malformed."))?;
    if expires <= Utc::now() {
        return Err(GoFinError::new(
            "PREVIEW_EXPIRED",
            "validation",
            "The native lending quote expired before dispatch.",
            "Prepare a fresh quote within existing approved authority.",
        ));
    }
    validate_obligation(
        &c.obligation_before,
        owner,
        &intent.market,
        &intent.obligation,
    )?;
    validate_obligation(
        &c.quote.projected_obligation,
        owner,
        &intent.market,
        &intent.obligation,
    )?;
    if c.reserve.program != klend_interface::KLEND_PROGRAM_ID.to_string()
        || c.reserve.market != intent.market
        || c.reserve.address != intent.reserve
        || c.reserve.token != intent.token
        || !c.obligation_before.complete
        || !c.obligation_before.writes_available
        || c.obligation_before.elevation_group != 0
        || c.obligation_before.tag != 0
        || c.instructions.is_empty()
        || c.quote.rent_lamports > intent.maximum_account_funding_lamports
        || c.quote.network_fee_lamports > intent.maximum_network_fee_lamports
    {
        return Err(invalid(
            "Native lending candidate changed identity, eligibility or finite costs.",
        ));
    }
    let eligible = match intent.action {
        LendingAction::Supply => c.reserve.supply_available,
        LendingAction::Withdraw => c.reserve.withdraw_available,
        LendingAction::Borrow => c.reserve.borrow_available,
        LendingAction::Repay => c.reserve.repay_available,
    };
    if !eligible {
        return Err(invalid(
            "The selected native lending action is unavailable.",
        ));
    }
    let mut fees = Quantity::zero();
    for fee in &c.quote.protocol_fees {
        if fee.token != intent.token {
            return Err(invalid("Native fee mint differs."));
        }
        fees = fees.checked_add(&atoms(
            fee.atoms
                .as_deref()
                .ok_or_else(|| invalid("Unknown protocol fee amount."))?,
        )?)?;
    }
    if fees > atoms(&intent.maximum_protocol_fee_atoms)? {
        return Err(invalid("Native protocol fees exceed the approved cap."));
    }
    for rows in [&c.quote.debits, &c.quote.credits] {
        if rows.len() > 1 || rows.iter().any(|row| row.token != intent.token) {
            return Err(invalid(
                "Native wallet cashflow contains an unrelated asset.",
            ));
        }
    }
    let total = |rows: &[TokenAmount]| {
        rows.iter().try_fold(Quantity::zero(), |sum, r| {
            sum.checked_add(&atoms(
                r.atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Unknown native cashflow."))?,
            )?)
        })
    };
    let debit = total(&c.quote.debits)?;
    let credit = total(&c.quote.credits)?;
    let amount = atoms(&intent.amount_atoms)?;
    match intent.action {
        LendingAction::Supply if debit.is_zero() || debit > amount || !credit.is_zero() => {
            return Err(invalid(
                "Supply must debit a positive amount within the reviewed underlying input maximum.",
            ));
        }
        LendingAction::Repay if debit > amount || debit.is_zero() || !credit.is_zero() => {
            return Err(invalid(
                "Repayment exceeds its finite maximum or has no debit.",
            ));
        }
        LendingAction::Borrow
            if credit != amount
                || !debit.is_zero()
                || amount.checked_add(&fees)?
                    > atoms(
                        intent
                            .maximum_debt_increase_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Missing debt cap."))?,
                    )? =>
        {
            return Err(invalid(
                "Borrow receipt or gross new debt differs from its approved limits.",
            ));
        }
        LendingAction::Withdraw
            if !debit.is_zero()
                || credit
                    < atoms(
                        intent
                            .minimum_receive_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Missing minimum receipt."))?,
                    )? =>
        {
            return Err(invalid(
                "Withdrawal cashflow violates its reviewed receipt floor.",
            ));
        }
        _ => {}
    }
    if let Some(limits) = &intent.risk_limits {
        validate_risk(&c.quote.projected_obligation.risk, limits)?;
    }
    if intent.all
        && intent.action == LendingAction::Repay
        && c.quote.projected_obligation.positions.iter().any(|p| {
            p.reserve == intent.reserve && p.side == LendingSide::Debt && p.native_units != "0"
        })
    {
        return Err(invalid(
            "The finite repay-all cap does not clear selected debt in the native preview.",
        ));
    }
    Ok(())
}

pub(crate) fn target_side(action: LendingAction) -> LendingSide {
    if matches!(action, LendingAction::Supply | LendingAction::Withdraw) {
        LendingSide::Supply
    } else {
        LendingSide::Debt
    }
}
pub(crate) fn complete_before(
    candidate: &LendingCandidate,
    action: LendingAction,
) -> ObligationInfo {
    let mut before = candidate.obligation_before.clone();
    let side = target_side(action);
    if !before
        .positions
        .iter()
        .any(|p| p.reserve == candidate.reserve.address && p.side == side)
    {
        before.positions.push(LendingPosition {
            reserve: candidate.reserve.address.clone(),
            side,
            token: candidate.reserve.token.clone(),
            collateral_token: (side == LendingSide::Supply)
                .then(|| candidate.reserve.collateral_token.clone()),
            native_units: "0".into(),
            underlying_atoms: Some("0".into()),
            cumulative_borrow_rate_bsf: None,
            protocol_state: json!({"absence_verified":true,"debt_kind":"floating"}),
        });
    }
    before
}
fn baselines(c: &LendingCandidate, action: LendingAction) -> Result<Vec<PositionAnchor>> {
    let mut before = complete_before(c, action);
    before.positions.retain(|p| {
        p.side == LendingSide::Debt
            || (p.reserve == c.reserve.address && p.side == target_side(action))
    });
    let mut result: Vec<PositionAnchor> = settlement::snapshot_positions(&before)?
        .into_iter()
        .map(|s| {
            s.anchor
                .ok_or_else(|| invalid("Missing exact lending anchor."))
        })
        .collect::<Result<_>>()?;
    result.extend(closing_rent_baselines(c)?);
    Ok(result)
}
pub(crate) fn closing_rent_baselines(c: &LendingCandidate) -> Result<Vec<PositionAnchor>> {
    let mut result = Vec::new();
    for row in c.metadata["possible_closed_accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let account = row["account"]
            .as_str()
            .ok_or_else(|| invalid("Missing rent refund identity."))?;
        if account != c.obligation_before.address
            || !result.is_empty()
            || row["observed_slot"].as_u64() != Some(c.obligation_before.observed_slot)
            || row["observed_at"].as_str() != Some(c.obligation_before.observed_at.as_str())
        {
            return Err(invalid(
                "Rent refund must bind the exact observed obligation account.",
            ));
        }
        let lamports = row["maximum_lamports"]
            .as_u64()
            .ok_or_else(|| invalid("Missing observed rent principal."))?;
        if lamports == 0 {
            return Err(invalid("Closing rent principal must be positive."));
        }
        result.push(PositionAnchor { resource:resource(&c.obligation_before.owner),position:format!("account-funding:{account}"),position_kind:PositionKind::RefundableAccountFunding,principal:Amount { asset:native_asset(),atoms:Quantity::from_u64(lamports)},scaled_receipt:None,evidence_version:1,evidence:json!({"version":1,"source":"finalized_klend_account_observation","account":account,"observed_slot":c.obligation_before.observed_slot,"refund_authority":c.obligation_before.owner}),observed_at:c.obligation_before.observed_at.clone()});
    }
    Ok(result)
}
fn validate_baselines(prepared: &PreparedOperation, c: &LendingCandidate) -> Result<()> {
    let intent = bound_intent(prepared)?;
    let exact = baselines(c, intent.action)?;
    let bound: Vec<PositionAnchor> =
        serde_json::from_value(prepared.binding[BASELINE_ANCHORS_KEY].clone())
            .map_err(|_| invalid("Missing verified obligation baselines."))?;
    if exact != bound
        || prepared.binding["wallet_address"].as_str() != Some(c.obligation_before.owner.as_str())
    {
        return Err(invalid("Approved obligation baseline or owner changed."));
    }
    let at = chrono::DateTime::parse_from_rfc3339(&c.obligation_before.observed_at)
        .map_err(|_| invalid("Invalid native observation timestamp."))?;
    let prepared_at = chrono::DateTime::parse_from_rfc3339(&prepared.observed_at)
        .map_err(|_| invalid("Invalid preview timestamp."))?;
    if at > prepared_at || prepared_at.signed_duration_since(at).num_seconds() > 120 {
        return Err(invalid(
            "The exact obligation baseline is stale or future-dated.",
        ));
    }
    Ok(())
}
pub(crate) fn rent_limits(candidate: &LendingCandidate) -> Result<Vec<AccountFundingLimit>> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for row in candidate.metadata["rent_accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let address = row["account"]
            .as_str()
            .ok_or_else(|| invalid("Rent account identity missing."))?;
        gofin_solana::parse_address(address)?;
        if !seen.insert(address.to_owned()) {
            return Err(invalid("Duplicate rent account."));
        }
        result.push(AccountFundingLimit {
            position: format!("account-funding:{address}"),
            maximum_amount: Quantity::from_u64(
                row["maximum_lamports"]
                    .as_u64()
                    .ok_or_else(|| invalid("Rent cap missing."))?,
            ),
        });
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn build_prepared(
    operation: &str,
    input: &Value,
    intent: &LendingIntent,
    owner: &str,
    c: &LendingCandidate,
    frozen: &FrozenTransaction,
    fee: u64,
    context: &PreparationContext,
) -> Result<PreparedOperation> {
    let wallet = resource(owner);
    let underlying = asset(&intent.token);
    let amount = atoms(&intent.amount_atoms)?;
    let side = target_side(intent.action);
    let selected =
        settlement::position_id(&intent.market, &intent.obligation, &intent.reserve, side);
    let target_asset = if side == LendingSide::Supply {
        asset(&c.reserve.collateral_token)
    } else {
        underlying.clone()
    };
    let target_kind = if side == LendingSide::Supply {
        PositionKind::Supply
    } else {
        PositionKind::VariableDebt
    };
    let cash = AccountingLocation::Cash {
        resource: wallet.clone(),
    };
    let position = AccountingLocation::Position {
        resource: wallet.clone(),
        position: selected.clone(),
        position_kind: target_kind.clone(),
    };
    let mut debits = Vec::new();
    let mut expected = Vec::new();
    let mut costs = vec![CostBound {
        location: cash.clone(),
        amount: Amount {
            asset: native_asset(),
            atoms: Quantity::from_u64(intent.maximum_network_fee_lamports),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    if matches!(intent.action, LendingAction::Supply | LendingAction::Repay) {
        debits.push(ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: underlying.clone(),
                atoms: amount.clone(),
            },
        });
        expected.push(EffectConstraint::Debit {
            resource: wallet.clone(),
            asset: underlying.clone(),
            maximum_debit: amount.clone(),
        });
    } else {
        expected.push(EffectConstraint::Credit {
            to: wallet.clone(),
            asset: underlying.clone(),
            minimum_credit: if intent.action == LendingAction::Borrow {
                amount.clone()
            } else {
                atoms(
                    intent
                        .minimum_receive_atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Missing receipt floor."))?,
                )?
            },
        });
    }
    let gross = if intent.action == LendingAction::Borrow {
        atoms(
            intent
                .maximum_debt_increase_atoms
                .as_deref()
                .ok_or_else(|| invalid("Missing debt cap."))?,
        )?
    } else {
        amount.clone()
    };
    if side == LendingSide::Supply {
        expected.push(EffectConstraint::PositionOfKind {
            resource: wallet.clone(),
            position: selected.clone(),
            asset: target_asset.clone(),
            position_kind: PositionKind::Supply,
            direction: if intent.action == LendingAction::Supply {
                Direction::Increase
            } else {
                Direction::Decrease
            },
            maximum_change: if intent.action == LendingAction::Supply {
                Quantity::from_u64(u64::MAX)
            } else {
                amount.clone()
            },
        });
    }
    let baseline = baselines(c, intent.action)?;
    for anchor in &baseline {
        if anchor.position_kind != PositionKind::VariableDebt {
            continue;
        }
        let chosen = anchor.position == selected;
        expected.push(EffectConstraint::Debt {
            resource: wallet.clone(),
            debt: anchor.position.clone(),
            asset: anchor.principal.asset.clone(),
            direction: if chosen && intent.action == LendingAction::Borrow {
                Direction::Increase
            } else {
                Direction::Decrease
            },
            maximum_change: if chosen {
                gross.clone()
            } else {
                Quantity::zero()
            },
        });
        expected.push(EffectConstraint::Accrual {
            resource: wallet.clone(),
            position: anchor.position.clone(),
            asset: anchor.principal.asset.clone(),
            position_kind: PositionKind::VariableDebt,
            maximum_change: anchor.principal.atoms.checked_add(&gross)?,
        });
    }
    let (from, to, debit_asset, credit_asset, maximum_debit, minimum_credit) = match intent.action {
        LendingAction::Supply => (
            cash.clone(),
            position.clone(),
            underlying.clone(),
            target_asset.clone(),
            amount.clone(),
            Quantity::zero(),
        ),
        LendingAction::Withdraw => (
            position.clone(),
            cash.clone(),
            target_asset.clone(),
            underlying.clone(),
            amount.clone(),
            atoms(intent.minimum_receive_atoms.as_deref().expect("validated"))?,
        ),
        LendingAction::Borrow => (
            position.clone(),
            cash.clone(),
            underlying.clone(),
            underlying.clone(),
            gross.clone(),
            amount.clone(),
        ),
        LendingAction::Repay => (
            cash.clone(),
            position.clone(),
            underlying.clone(),
            underlying.clone(),
            amount.clone(),
            Quantity::zero(),
        ),
    };
    expected.push(EffectConstraint::OriginMovement {
        from,
        to,
        debit_asset,
        credit_asset,
        maximum_debit,
        minimum_credit,
    });
    if intent.action == LendingAction::Borrow {
        costs.push(CostBound {
            location: position.clone(),
            amount: Amount {
                asset: underlying.clone(),
                atoms: atoms(&intent.maximum_protocol_fee_atoms)?.min(gross.checked_sub(&amount)?),
            },
            treatment: CostTreatment::IncludedInDebit,
        });
    } else if intent.action == LendingAction::Repay {
        costs.push(CostBound {
            location: cash,
            amount: Amount {
                asset: underlying,
                atoms: amount,
            },
            treatment: CostTreatment::IncludedInDebit,
        });
    }
    let limits = rent_limits(c)?;
    let rent = limits.iter().try_fold(Quantity::zero(), |sum, row| {
        sum.checked_add(&row.maximum_amount)
    })?;
    if rent < Quantity::from_u64(c.quote.rent_lamports)
        || rent > Quantity::from_u64(intent.maximum_account_funding_lamports)
    {
        return Err(invalid(
            "Exact rent account limits do not cover quoted funding within the approved cap.",
        ));
    }
    if !rent.is_zero() {
        debits.push(ResourceAmount {
            resource: wallet.clone(),
            amount: Amount {
                asset: native_asset(),
                atoms: rent,
            },
        });
        expected.push(EffectConstraint::OptionalAccountFunding {
            resource: wallet.clone(),
            asset: native_asset(),
            accounts: limits,
        });
    }
    for anchor in closing_rent_baselines(c)? {
        let funding = AccountingLocation::Position {
            resource: wallet.clone(),
            position: anchor.position.clone(),
            position_kind: PositionKind::RefundableAccountFunding,
        };
        expected.push(EffectConstraint::PositionOfKind {
            resource: wallet.clone(),
            position: anchor.position,
            asset: native_asset(),
            position_kind: PositionKind::RefundableAccountFunding,
            direction: Direction::Decrease,
            maximum_change: anchor.principal.atoms.clone(),
        });
        expected.push(EffectConstraint::Credit {
            to: wallet.clone(),
            asset: native_asset(),
            minimum_credit: anchor.principal.atoms.clone(),
        });
        expected.push(EffectConstraint::OriginMovement {
            from: funding,
            to: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            debit_asset: native_asset(),
            credit_asset: native_asset(),
            maximum_debit: anchor.principal.atoms.clone(),
            minimum_credit: anchor.principal.atoms,
        });
    }
    let prior: Vec<_> = context
        .positions
        .iter()
        .filter(|p| {
            baseline.iter().any(|b| {
                b.resource == p.resource
                    && b.position == p.position
                    && b.position_kind == p.position_kind
                    && b.principal.asset == p.principal.asset
            })
        })
        .cloned()
        .collect();
    let mut binding = json!({"intent":intent,"wallet_address":owner,"protocol_candidate":c,"message_sha256":frozen.message_sha256(),"last_valid_block_height":c.last_valid_block_height,"expected_network_fee_lamports":fee,"prior_anchors":prior,BASELINE_ANCHORS_KEY:baseline});
    if side == LendingSide::Supply {
        let before = baseline
            .iter()
            .find(|p| p.position == selected)
            .ok_or_else(|| invalid("Missing supply baseline."))?;
        binding[LENDING_TRANSITION_BINDING_KEY] = json!(LendingTransitionBinding {
            version: 1,
            resource: wallet.clone(),
            position: selected,
            asset: target_asset,
            market: intent.market.clone(),
            obligation: intent.obligation.clone(),
            reserve: intent.reserve.clone(),
            owner: owner.into(),
            collateral_token_program: c.reserve.collateral_token.token_program.clone(),
            pre_transaction_collateral_atoms: before.principal.atoms.clone()
        });
    }
    let mut prepared=PreparedOperation{operation:operation.into(),input:input.clone(),binding,summary:format!("KLend {:?}: {} {} atoms, reserve {}, obligation {}, wallet {}. Gross new debt cap {}; protocol fee cap {} underlying atoms; network fee up to {} lamports; refundable funding up to {} lamports. Risk limits: {}. Risk and conversion checks are fresh pre-sign checks; native protocol thresholds remain independently enforced.",intent.action,intent.amount_atoms,if side==LendingSide::Supply&&intent.action==LendingAction::Withdraw{"collateral"}else{"underlying"},intent.reserve,intent.obligation,intent.wallet,intent.maximum_debt_increase_atoms.as_deref().unwrap_or("0"),intent.maximum_protocol_fee_atoms,intent.maximum_network_fee_lamports,intent.maximum_account_funding_lamports,json!(intent.risk_limits)),checks:vec![Check{name:"Native accounts and financial limits".into(),status:CheckStatus::Passed,detail:"Canonical market, reserve, obligation, assets, exact instruction graph and risk limits verified.".into()},Check{name:"Frozen message simulation".into(),status:CheckStatus::Passed,detail:format!("Simulation passed; expected network fee {fee} lamports.")}],warnings:c.quote.warnings.clone(),observed_at:Utc::now().to_rfc3339(),expires_at:c.quote.expires_at.clone(),valuation:None,intent:FinancialIntent{debits,maximum_costs:costs,expected,locks:vec![wallet]},operation_version:1,action_deadline:None};
    if intent.action == LendingAction::Supply {
        let expected = c
            .quote
            .debits
            .iter()
            .try_fold(Quantity::zero(), |sum, row| {
                sum.checked_add(&atoms(
                    row.atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Unknown expected supply debit."))?,
                )?)
            })?;
        let retained = atoms(&intent.amount_atoms)?.checked_sub(&expected)?;
        prepared.summary.push_str(&format!(" Supply input is a maximum of {} underlying atoms; expected debit {} atoms and expected unspent input {} atoms. Native collateral rounding may leave a residual; settlement records the verified actual debit.",intent.amount_atoms,expected,retained));
    }
    validate_baselines(&prepared, c)?;
    Ok(prepared)
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;
