//! Authorization and transaction lifecycle shared by the two native vault families.
use crate::{
    boundaries, common,
    protocol::*,
    settlement,
    types::{self, *},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_domain::{
    financial_plans::{FinancialPlanFundingIdentity, PlanPermission},
    *,
};
use gofin_pricing::UsdPriceSource;
use gofin_solana::{
    SolanaWalletAdapter,
    transaction::{FrozenTransaction, ReceiptOutcome},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

pub struct KaminoAdapter {
    wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
    protocols: BTreeMap<VaultFamily, Arc<dyn KaminoProtocol>>,
    prices: Arc<dyn UsdPriceSource>,
}
impl KaminoAdapter {
    pub fn new(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        protocols: Vec<Arc<dyn KaminoProtocol>>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        if wallets.is_empty() || wallets.keys().any(|name| name.trim().is_empty()) {
            return Err(invalid("Kamino requires enrolled Solana wallets."));
        }
        let count = protocols.len();
        let protocols: BTreeMap<_, _> = protocols.into_iter().map(|p| (p.family(), p)).collect();
        if count != 2 || protocols.len() != 2 {
            return Err(invalid(
                "Kamino requires exactly one lending and one liquidity protocol.",
            ));
        }
        Ok(Self {
            wallets,
            protocols,
            prices,
        })
    }
    pub fn with_price_source(
        wallets: BTreeMap<String, Arc<SolanaWalletAdapter>>,
        protocols: Vec<Arc<dyn KaminoProtocol>>,
        prices: Arc<dyn UsdPriceSource>,
    ) -> Result<Self> {
        Self::new(wallets, protocols, prices)
    }
    fn wallet(&self, name: &str) -> Result<&Arc<SolanaWalletAdapter>> {
        self.wallets
            .get(name)
            .ok_or_else(|| invalid("Select an enrolled Solana wallet."))
    }
    fn protocol(&self, family: VaultFamily) -> Result<&Arc<dyn KaminoProtocol>> {
        self.protocols.get(&family).ok_or_else(unknown)
    }
    async fn resolve(
        &self,
        operation: &str,
        input: &Value,
        resolve_all: bool,
    ) -> Result<(VaultIntent, VaultInfo)> {
        let public = parse_public(operation, input)?;
        let wallet = self.wallet(&public.wallet)?;
        let protocol = self.protocol(public.family)?;
        let vault = protocol.vault(&wallet.runtime(), &public.vault).await?;
        validate_vault(&vault, public.family, &public.vault)?;
        if vault.shares.mint != public.share_mint {
            return Err(invalid(
                "The requested share mint does not match the selected vault.",
            ));
        }
        let action = action(operation, public.action)?;
        let mut amounts = Vec::new();
        let shares_atoms = match action {
            VaultAction::Deposit => {
                if public.shares_atoms.is_some() || public.amounts.len() != vault.assets.len() {
                    return Err(invalid(
                        "A deposit requires exactly the vault's native assets and no withdrawal shares.",
                    ));
                }
                let mut seen = BTreeSet::new();
                let mut nonzero = false;
                for row in &public.amounts {
                    if !seen.insert(&row.mint) {
                        return Err(invalid("Deposit asset mints must be unique."));
                    }
                    let token = vault
                        .assets
                        .iter()
                        .find(|t| t.mint == row.mint)
                        .ok_or_else(|| invalid("A deposit asset is outside the selected vault."))?;
                    let n = atoms(&row.atoms)?;
                    nonzero |= !n.is_zero();
                    amounts.push(TokenAmount {
                        token: token.clone(),
                        atoms: Some(n.to_string()),
                    });
                }
                if !nonzero {
                    return Err(invalid("At least one deposit amount must be positive."));
                }
                amounts.sort_by(|a, b| a.token.mint.cmp(&b.token.mint));
                None
            }
            VaultAction::Withdraw => {
                if !public.amounts.is_empty()
                    || public.minimum_shares_atoms.is_some()
                    || public.stake
                {
                    return Err(invalid(
                        "Withdrawals accept shares only and cannot request deposit staking or a deposit output floor.",
                    ));
                }
                let shares = public
                    .shares_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Withdrawals require shares_atoms."))?;
                if shares == "all" {
                    if !resolve_all {
                        return Err(invalid(
                            "Named-plan permissions require a finite exact share cap; resolve 'all' through kamino.position first.",
                        ));
                    }
                    let position = protocol
                        .position(&wallet.runtime(), wallet.address(), &vault.address)
                        .await?;
                    validate_position(&position, &vault, wallet.address())?;
                    let total = owned_shares(&position)?;
                    let available=position.withdrawable_shares_atoms.as_deref().map(atoms).transpose()?.ok_or_else(||invalid("Withdrawal availability is unknown; no 'all' withdrawal can be prepared."))?;
                    if total.is_zero() || available < total {
                        return Err(invalid(
                            "All shares are not currently withdrawable; inspect locks, liquidity and pending custody.",
                        ));
                    }
                    Some(total.to_string())
                } else {
                    let n = atoms(shares)?;
                    if n.is_zero() {
                        return Err(invalid("Withdrawal shares must be positive."));
                    }
                    Some(n.to_string())
                }
            }
        };
        let intent = VaultIntent {
            wallet: public.wallet,
            family: public.family,
            action,
            vault: public.vault,
            amounts,
            shares_atoms,
            minimum_shares_atoms: public.minimum_shares_atoms,
            maximum_network_fee_lamports: public.maximum_network_fee_lamports,
            maximum_account_funding_lamports: public.maximum_account_funding_lamports,
            stake: public.stake,
        };
        validate_eligibility(&intent, &vault)?;
        Ok((intent, vault))
    }
    async fn candidate(&self, intent: &VaultIntent, vault: &VaultInfo) -> Result<VaultCandidate> {
        let wallet = self.wallet(&intent.wallet)?;
        let protocol = self.protocol(intent.family)?;
        let mut candidate = protocol
            .build_candidate(&wallet.runtime(), intent, wallet.address())
            .await?;
        if !same_vault_identity(&candidate.vault, vault) {
            return Err(invalid(
                "The candidate changed the canonical vault identity.",
            ));
        }
        validate_candidate(intent, &candidate)?;
        protocol
            .validate_candidate(&wallet.runtime(), intent, wallet.address(), &candidate)
            .await?;
        if intent.stake {
            candidate.quote.warnings.push("Farm exchange-rate rounding can leave fractional entitlements and make redeemable whole share atoms differ from native minted or staked atoms; any enforced native mint minimum applies before farm entitlement rounding.".into());
        }
        Ok(candidate)
    }
    async fn prepare_write(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if !matches!(operation, OP_DEPOSIT | OP_WITHDRAW) {
            return Err(unknown());
        }
        let (intent, vault) = self.resolve(operation, input, true).await?;
        let wallet = self.wallet(&intent.wallet)?;
        let signer = wallet.signer().ok_or_else(read_only)?;
        let mut candidate = self.candidate(&intent, &vault).await?;
        let mut rent_observations = Vec::new();
        for row in rent_limits(&candidate)? {
            let account = row
                .position
                .strip_prefix("account-funding:")
                .ok_or_else(|| invalid("Invalid rent account identity."))?;
            let response = wallet
                .runtime()
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([account,{"encoding":"base64","commitment":"finalized"}]),
                )
                .await?;
            let slot = response["context"]["slot"].as_u64().ok_or_else(|| {
                invalid("Rent account absence requires a finalized context slot.")
            })?;
            if response.get("value") != Some(&Value::Null) {
                return Err(invalid(
                    "A proposed new rent account already exists; refresh the native preview.",
                ));
            }
            rent_observations.push(json!({"account":account,"context_slot":slot,"observed_at":Utc::now().to_rfc3339(),"commitment":"finalized","account_absence_proven":true}));
        }
        candidate.metadata["account_funding_observations"] = json!(rent_observations);
        let valuation = self.valuation(&intent, &candidate.quote).await?;
        // Native validation, rent evidence and pricing can require many reads.
        // Acquire the final message lifetime only after that preparation. This
        // is strictly pre-approval: freeze/submit retain this exact hash forever.
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
        if fee > intent.maximum_network_fee_lamports
            || candidate.quote.rent_lamports > intent.maximum_account_funding_lamports
        {
            return Err(invalid("The transaction exceeds the approved cost caps."));
        }
        validate_candidate(&intent, &candidate)?;
        let mut prepared = build_prepared(
            operation,
            input,
            &intent,
            wallet.address(),
            &candidate,
            &frozen,
            fee,
        )?;
        prepared.binding[BASELINE_ANCHORS_KEY] = json!(candidate_baselines(
            wallet.address(),
            &candidate,
            &prepared.observed_at
        )?);
        prepared.valuation = Some(valuation);
        Ok(prepared)
    }
    async fn freeze(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<(
        Arc<SolanaWalletAdapter>,
        VaultIntent,
        VaultCandidate,
        FrozenTransaction,
    )> {
        let intent = bound_intent(prepared)?;
        let wallet = self.wallet(&intent.wallet)?.clone();
        let candidate = candidate_from(prepared)?;
        validate_prepared_baselines(prepared, &candidate)?;
        // Protocol validation owns the one fresh canonical account/eligibility read
        // and graph rebuild. Repeating vault discovery here doubles RPC latency.
        validate_candidate(&intent, &candidate)?;
        self.protocol(intent.family)?
            .validate_candidate(&wallet.runtime(), &intent, wallet.address(), &candidate)
            .await?;
        let signer = wallet.signer().ok_or_else(read_only)?;
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
            return Err(invalid("The approved Kamino message changed."));
        }
        wallet.runtime().validate_fee(&frozen).await?;
        let height = wallet
            .runtime()
            .rpc()
            .request("getBlockHeight", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(|| invalid("Invalid Solana block height."))?;
        if height > candidate.last_valid_block_height {
            return Err(GoFinError::new(
                "SOLANA_BLOCKHASH_EXPIRED",
                "revalidation",
                "The approved Kamino message expired before submission.",
                "Refresh within existing authority; never replay an uncertain submission.",
            ));
        }
        validate_candidate(&intent, &candidate)?;
        Ok((wallet, intent, candidate, frozen))
    }
    async fn valuation(&self, intent: &VaultIntent, quote: &VaultQuote) -> Result<Valuation> {
        let movement = if intent.action == VaultAction::Deposit {
            &intent.amounts
        } else {
            &quote.credits
        };
        let mut values = BTreeMap::new();
        for row in movement {
            values.insert(
                asset(&row.token),
                atoms(row.atoms.as_deref().ok_or_else(|| {
                    invalid("Valuation requires known native asset quantities.")
                })?)?,
            );
        }
        let native = native_asset();
        let mut requested: Vec<_> = values.keys().cloned().collect();
        if !requested.contains(&native) {
            requested.push(native.clone());
        }
        let quotes = self.prices.quotes(&requested).await?;
        if quotes.len() != requested.len() {
            return Err(invalid("Incomplete Kamino price set."));
        }
        let mut prices = Vec::new();
        let mut movement_usd = Quantity::zero();
        let mut fee_usd = Quantity::zero();
        let mut allocation = Quantity::zero();
        let mut sources = BTreeSet::new();
        for (asset, quote) in requested.into_iter().zip(quotes) {
            let price = Quantity::from_decimal(&quote.usd_per_unit, 36)?;
            if price.is_zero() {
                return Err(invalid("USD prices must be positive."));
            }
            let unit = Quantity::new(format!("1{}", "0".repeat(usize::from(asset.decimals) + 30)))?;
            if let Some(n) = values.get(&asset) {
                movement_usd = movement_usd.checked_add(&n.mul_div_ceil(&price, &unit)?)?;
            }
            if asset == native {
                fee_usd = Quantity::from_u64(intent.maximum_network_fee_lamports)
                    .mul_div_ceil(&price, &unit)?;
                allocation = Quantity::from_u64(intent.maximum_account_funding_lamports)
                    .mul_div_ceil(&price, &unit)?;
            }
            sources.insert(quote.source.clone());
            prices.push(AssetPrice {
                asset,
                usd_micros_per_unit: price,
                unit_atoms: unit,
                observed_at: quote.updated_at,
                source: quote.source,
            });
        }
        Ok(Valuation {
            movement_usd_micros: movement_usd,
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: allocation,
            observed_at: Utc::now().to_rfc3339(),
            source: sources.into_iter().collect::<Vec<_>>().join(";"),
            prices,
        })
    }
    async fn public_position(&self, position: &PositionInfo) -> Result<Value> {
        let mut state = settlement::snapshot_position(position)?;
        settlement::value_position(&mut state, self.prices.as_ref()).await?;
        let evidence = &state
            .anchor
            .as_ref()
            .ok_or_else(|| invalid("Missing vault accounting evidence."))?
            .evidence["vault_position"];
        let mut output = serde_json::to_value(position)
            .map_err(|_| invalid("Cannot encode the position observation."))?;
        output["accounting"] = evidence.clone();
        let value = &evidence["backing_valuation"];
        let complete = !value.is_null() && value["complete"].as_bool() == Some(true);
        let excluded = value["excluded_components"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let missing = if value.is_null() {
            json!(["Current underlying amounts or exact USD prices are unavailable."])
        } else if !complete && excluded.is_empty() {
            json!([
                "Current value is partial; the protocol observation does not establish every valuation component."
            ])
        } else if !complete {
            json!([format!(
                "Current value is partial; excluded components: {}.",
                excluded
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )])
        } else {
            json!([])
        };
        output["current_value"] = json!({"usd_micros": value["total_usd_micros"], "complete": complete, "basis": value["basis"], "excluded_components": excluded, "missing_data": missing});
        Ok(output)
    }
    async fn snapshots(&self, scope: Option<&SnapshotScope>) -> Result<StateSnapshot> {
        let mut positions = Vec::new();
        let mut complete = true;
        let mut coverage = Vec::new();
        let mut pending = Vec::new();
        let mut observed_resources = BTreeSet::new();
        for wallet in self.wallets.values() {
            let resource = resource(wallet.address());
            if scope.is_some_and(|s| {
                !s.matches_network(gofin_solana::NETWORK) || !s.matches_resource(&resource)
            }) {
                continue;
            }
            if !observed_resources.insert(resource.clone()) {
                continue;
            }
            let mut wallet_complete = true;
            for protocol in self.protocols.values() {
                match protocol
                    .positions(&wallet.runtime(), wallet.address())
                    .await
                {
                    Ok(inventory) => {
                        wallet_complete &= inventory.complete;
                        pending.extend(inventory.warnings);
                        for position in inventory.positions {
                            if position.vault.family != protocol.family() {
                                return Err(invalid("Position family mismatch."));
                            }
                            validate_position(&position, &position.vault, wallet.address())?;
                            wallet_complete &= position.complete;
                            let mut state = settlement::snapshot_position(&position)?;
                            settlement::value_position(&mut state, self.prices.as_ref()).await?;
                            positions.push(state);
                        }
                    }
                    Err(error) => {
                        wallet_complete = false;
                        pending.push(format!(
                            "{} {} positions unavailable: {}",
                            wallet.address(),
                            protocol.family().as_str(),
                            error.message
                        ));
                    }
                }
            }
            complete &= wallet_complete;
            if wallet_complete {
                coverage.push(resource.0);
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
#[async_trait]
impl Adapter for KaminoAdapter {
    fn definitions(&self) -> Vec<Definition> {
        types::operation_definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.wallets
            .iter()
            .map(|(name, wallet)| ConnectionMetadata {
                name: name.clone(),
                provider: "kamino".into(),
                resource: resource(wallet.address()),
                role: Some("vaults".into()),
                parent: None,
                network: Some(gofin_solana::NETWORK.into()),
                address: Some(wallet.address().into()),
                credential_ref: None,
                signing_enabled: wallet.signer().is_some(),
                details: json!({"families":["lending","liquidity"],"cluster":"mainnet-beta"}),
            })
            .collect()
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
            let (intent, vault) = self.resolve(operation, input, true).await?;
            let candidate = self.candidate(&intent, &vault).await?;
            return Ok(
                json!({"family":intent.family,"vault":vault,"wallet":intent.wallet,"resolved_intent":intent,"quote":candidate.quote,"composition":"Native assets only. Use separate swap operations in a named plan when conversion is required."}),
            );
        }
        let query: ReadInput = serde_json::from_value(input.clone())
            .map_err(|_| invalid("Invalid Kamino read input."))?;
        if query.limit.is_some_and(|v| v == 0 || v > 100) {
            return Err(invalid("Discovery limit must be between 1 and 100."));
        }
        if let Some(address) = &query.vault {
            gofin_solana::parse_address(address)?;
        }
        let families = match query.family {
            Some(f) => vec![f],
            None => vec![VaultFamily::Lending, VaultFamily::Liquidity],
        };
        let wallet = match &query.wallet {
            Some(name) => self.wallet(name)?,
            None => self.wallets.values().next().ok_or_else(unknown)?,
        };
        match operation {
            OP_VAULTS => {
                if query.cursor.is_some() && query.vault.is_some() {
                    return Err(invalid(
                        "An exact vault lookup does not accept a discovery cursor.",
                    ));
                }
                if query.cursor.is_some() && query.family.is_none() {
                    return Err(invalid(
                        "A discovery cursor requires an explicit vault family.",
                    ));
                }
                let q = VaultQuery {
                    search: query.search,
                    token_mint: query.token_mint,
                    limit: query.limit.unwrap_or(25),
                    cursor: query.cursor,
                };
                if let Some(mint) = &q.token_mint {
                    gofin_solana::parse_address(mint)?;
                }
                let mut pages = Vec::new();
                for family in families {
                    let protocol = self.protocol(family)?;
                    let page =
                        if let Some(address) = &query.vault {
                            async {
                                let vault = protocol.vault(&wallet.runtime(), address).await?;
                                validate_vault(&vault, family, address)?;
                                let token_matches = q.token_mint.as_ref().is_none_or(|mint| {
                                    vault.assets.iter().any(|t| &t.mint == mint)
                                });
                                let search_matches = q.search.as_ref().is_none_or(|search| {
                                    let search = search.to_lowercase();
                                    vault.address.to_lowercase().contains(&search)
                                        || vault.name.as_ref().is_some_and(|name| {
                                            name.to_lowercase().contains(&search)
                                        })
                                });
                                Ok(VaultPage {
                                    vaults: if token_matches && search_matches {
                                        vec![vault]
                                    } else {
                                        vec![]
                                    },
                                    complete: true,
                                    next_cursor: None,
                                    warnings: vec![],
                                })
                            }
                            .await
                        } else {
                            protocol.vaults(&wallet.runtime(), &q).await
                        };
                    match page {
                        Ok(page) => { for v in &page.vaults { validate_vault(v, family, &v.address)?; } pages.push(json!({"family":family,"page":page})); }
                        Err(error) if query.family.is_none() => pages.push(json!({"family":family,"page":{"vaults":[],"complete":false,"next_cursor":null,"warnings":[error.message]}})),
                        Err(error) => return Err(error),
                    }
                }
                Ok(json!({"families":pages}))
            }
            OP_VAULT => {
                let family = query.family.ok_or_else(|| invalid("family is required."))?;
                let address = query.vault.ok_or_else(|| invalid("vault is required."))?;
                gofin_solana::parse_address(&address)?;
                let vault = self
                    .protocol(family)?
                    .vault(&wallet.runtime(), &address)
                    .await?;
                validate_vault(&vault, family, &address)?;
                Ok(json!({"vault":vault}))
            }
            OP_POSITIONS => {
                if query.wallet.is_none() {
                    return Err(invalid("wallet is required."));
                }
                let mut inventories = Vec::new();
                for family in families {
                    let protocol = self.protocol(family)?;
                    let inventory = if let Some(address) = &query.vault {
                        async {
                            let position = protocol
                                .position(&wallet.runtime(), wallet.address(), address)
                                .await?;
                            validate_vault(&position.vault, family, address)?;
                            validate_position(&position, &position.vault, wallet.address())?;
                            Ok(PositionInventory {
                                complete: position.complete,
                                observed_at: position.vault.observed_at.clone(),
                                warnings: position.warnings.clone(),
                                positions: vec![position],
                            })
                        }
                        .await
                    } else {
                        protocol
                            .positions(&wallet.runtime(), wallet.address())
                            .await
                    };
                    match inventory {
                        Ok(inventory) => {
                            let mut positions = Vec::new();
                            for p in &inventory.positions {
                                if p.vault.family != family { return Err(invalid("Position family mismatch.")); }
                                validate_position(p, &p.vault, wallet.address())?;
                                positions.push(self.public_position(p).await?);
                            }
                            inventories.push(json!({"family":family,"inventory":{"positions":positions,"complete":inventory.complete,"observed_at":inventory.observed_at,"warnings":inventory.warnings}}));
                        }
                        Err(error) if query.family.is_none() => inventories.push(json!({"family":family,"inventory":{"positions":[],"complete":false,"observed_at":Utc::now().to_rfc3339(),"warnings":[error.message]}})),
                        Err(error) => return Err(error),
                    }
                }
                Ok(json!({"wallet":query.wallet,"owner":wallet.address(),"families":inventories}))
            }
            OP_POSITION => {
                if query.wallet.is_none() {
                    return Err(invalid("wallet is required."));
                }
                let family = query.family.ok_or_else(|| invalid("family is required."))?;
                let address = query.vault.ok_or_else(|| invalid("vault is required."))?;
                let protocol = self.protocol(family)?;
                let vault = protocol.vault(&wallet.runtime(), &address).await?;
                validate_vault(&vault, family, &address)?;
                let p = protocol
                    .position(&wallet.runtime(), wallet.address(), &address)
                    .await?;
                validate_position(&p, &vault, wallet.address())?;
                Ok(json!({"position":self.public_position(&p).await?}))
            }
            _ => Err(unknown()),
        }
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.prepare_write(operation, input).await
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<boundaries::BoundarySpec> {
        boundaries::prepare(prepared, bounds)
    }
    fn validate_boundary_candidate(
        &self,
        spec: &boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        boundaries::validate_candidate(spec, prepared)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<PlanPermission> {
        if !matches!(operation, OP_DEPOSIT | OP_WITHDRAW) {
            return Err(unknown());
        }
        let (intent, vault) = self.resolve(operation, input, false).await?;
        let wallet = self.wallet(&intent.wallet)?;
        let farm = if intent.stake {
            Some(
                self.protocol(intent.family)?
                    .staking_farm(&wallet.runtime(), &vault)
                    .await?,
            )
        } else {
            None
        };
        let spec = boundaries::prepare_intent(
            operation,
            &intent,
            &vault,
            wallet.address(),
            farm.as_ref(),
            bounds,
        )?;
        let possible_credits = if intent.action == VaultAction::Withdraw {
            vault
                .assets
                .iter()
                .map(|t| FinancialPlanFundingIdentity {
                    resource: resource(wallet.address()),
                    asset: asset(t),
                })
                .collect()
        } else {
            vec![]
        };
        Ok(PlanPermission {
            spec,
            recovery_parent: None,
            possible_credits,
            fee_assets: vec![native_asset()],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }
    fn plan_has_finite_non_cash_source(
        &self,
        operation: &str,
        permission: &PlanPermission,
        target: &ResourceAmount,
    ) -> bool {
        boundaries::finite_withdrawal_source(operation, &permission.spec, target)
    }
    fn preparation_reuse_window(&self, prepared: &PreparedOperation) -> Option<Duration> {
        matches!(prepared.operation.as_str(), OP_DEPOSIT | OP_WITHDRAW)
            .then_some(Duration::from_secs(30))
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let intent = bound_intent(prepared)?;
        let candidate = candidate_from(prepared)?;
        validate_prepared_baselines(prepared, &candidate)?;
        validate_candidate(&intent, &candidate)?;
        if prepared.binding["wallet_address"].as_str()
            != Some(self.wallet(&intent.wallet)?.address())
            || prepared.binding["message_sha256"]
                .as_str()
                .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
            || prepared.binding["last_valid_block_height"].as_u64()
                != Some(candidate.last_valid_block_height)
        {
            return Err(invalid("The prepared wallet/message binding is invalid."));
        }
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let (wallet, _, _, frozen) = self.freeze(prepared).await?;
        let signer = wallet.signer().ok_or_else(read_only)?;
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
            .ok_or_else(|| invalid("Missing persisted vault preview."))?;
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
        let Some(submission) = &record.submission else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No durable submission identity exists."}),
                verified: None,
            });
        };
        let prepared = record
            .preview
            .as_ref()
            .ok_or_else(|| invalid("The durable vault preview is missing."))?;
        let intent = bound_intent(prepared)?;
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
            } => {
                let verified =
                    settlement::verify_not_dispatched(submission, &reason_code, &reason)?;
                Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: json!({"no_network_send":true,"reason_code":reason_code,"reason":reason}),
                    verified: Some(verified),
                })
            }
            ReceiptOutcome::ExpiredWithoutInclusion {
                signature,
                message_sha256,
                reason,
                proof,
            } => {
                let verified = settlement::verify_expired_without_inclusion(
                    submission,
                    &signature,
                    &message_sha256,
                    &reason,
                    &proof,
                )?;
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
                let bundle = if failed {
                    settlement::verify_failed(
                        submission,
                        &proof,
                        wallet.address(),
                        intent.maximum_network_fee_lamports,
                    )?
                } else {
                    let candidate = candidate_from(prepared)?;
                    let effects = self
                        .protocol(intent.family)?
                        .verify_finalized(
                            &wallet.runtime(),
                            &intent,
                            wallet.address(),
                            &candidate,
                            &proof,
                        )
                        .await?;
                    settlement::verify(record, &intent, &candidate, &effects, &proof)?
                };
                Ok(Reconciliation {
                    status: if failed {
                        Status::Failed
                    } else {
                        Status::Completed
                    },
                    evidence: json!({"signature":signature,"slot":slot,"fee_lamports":fee_lamports,"message_sha256":message_sha256,"verification":bundle.evidence}),
                    verified: Some(bundle),
                })
            }
        }
    }
}

pub(crate) fn parse_public(operation: &str, input: &Value) -> Result<PublicIntent> {
    let request: PublicIntent = serde_json::from_value(input.clone())
        .map_err(|_| invalid("Invalid Kamino intent; use the installed operation schema."))?;
    action(operation, request.action)?;
    if request.network != "solana"
        || request.cluster != "mainnet-beta"
        || request.wallet.trim().is_empty()
        || request.maximum_network_fee_lamports == 0
    {
        return Err(invalid(
            "An enrolled wallet, Solana mainnet-beta and a positive finite fee cap are required.",
        ));
    }
    gofin_solana::parse_address(&request.vault)?;
    gofin_solana::parse_address(&request.share_mint)?;
    for amount in &request.amounts {
        gofin_solana::parse_address(&amount.mint)?;
        atoms(&amount.atoms)?;
    }
    if let Some(minimum) = &request.minimum_shares_atoms {
        atoms(minimum)?;
    }
    if let Some(shares) = request.shares_atoms.as_deref().filter(|s| *s != "all") {
        atoms(shares)?;
    }
    Ok(request)
}
fn action(operation: &str, requested: Option<VaultAction>) -> Result<VaultAction> {
    let expected = match operation {
        OP_DEPOSIT => VaultAction::Deposit,
        OP_WITHDRAW => VaultAction::Withdraw,
        OP_QUOTE => {
            return requested
                .ok_or_else(|| invalid("A quote requires action deposit or withdraw."));
        }
        _ => return Err(unknown()),
    };
    if requested.is_some_and(|v| v != expected) {
        return Err(invalid(
            "The requested action does not match the operation.",
        ));
    }
    Ok(expected)
}
pub(crate) fn bound_intent(prepared: &PreparedOperation) -> Result<VaultIntent> {
    let public = parse_public(&prepared.operation, &prepared.input)?;
    let intent: VaultIntent = serde_json::from_value(prepared.binding["intent"].clone())
        .map_err(|_| invalid("The reviewed concrete vault intent is missing."))?;
    let candidate = candidate_from(prepared)?;
    if public.wallet != intent.wallet
        || public.family != intent.family
        || public.vault != intent.vault
        || public.share_mint != candidate.vault.shares.mint
        || action(&prepared.operation, public.action)? != intent.action
        || public.maximum_network_fee_lamports != intent.maximum_network_fee_lamports
        || public.maximum_account_funding_lamports != intent.maximum_account_funding_lamports
        || public.stake != intent.stake
        || public.minimum_shares_atoms != intent.minimum_shares_atoms
    {
        return Err(invalid(
            "The concrete vault intent changed the public request.",
        ));
    }
    if public.shares_atoms.as_deref() != Some("all") && public.shares_atoms != intent.shares_atoms {
        return Err(invalid(
            "The concrete withdrawal changed the exact reviewed shares.",
        ));
    }
    if public.shares_atoms.as_deref() == Some("all")
        && intent
            .shares_atoms
            .as_deref()
            .is_none_or(|n| atoms(n).is_err() || n == "0")
    {
        return Err(invalid(
            "The all-shares quantity was not frozen before approval.",
        ));
    }
    if public.amounts.len() != intent.amounts.len()
        || public.amounts.iter().any(|a| {
            !intent
                .amounts
                .iter()
                .any(|b| a.mint == b.token.mint && b.atoms.as_deref() == Some(a.atoms.as_str()))
        })
    {
        return Err(invalid(
            "The concrete deposit changed its reviewed native assets or amounts.",
        ));
    }
    Ok(intent)
}
pub(crate) fn candidate_from(prepared: &PreparedOperation) -> Result<VaultCandidate> {
    serde_json::from_value(prepared.binding["protocol_candidate"].clone())
        .map_err(|_| invalid("The immutable Kamino protocol candidate is invalid."))
}
pub(crate) fn atoms(raw: &str) -> Result<Quantity> {
    if raw.is_empty()
        || raw.len() > 20
        || !raw.bytes().all(|b| b.is_ascii_digit())
        || (raw.len() > 1 && raw.starts_with('0'))
        || raw.parse::<u64>().is_err()
    {
        return Err(invalid(
            "Token quantities must be canonical unsigned u64 base-unit strings.",
        ));
    }
    Quantity::new(raw)
}
pub(crate) fn owned_shares(position: &PositionInfo) -> Result<Quantity> {
    atoms(&position.wallet_shares_atoms)?
        .checked_add(&atoms(&position.staked_shares_atoms)?)?
        .checked_add(&atoms(&position.pending_shares_atoms)?)
}
pub(crate) fn validate_vault(vault: &VaultInfo, family: VaultFamily, address: &str) -> Result<()> {
    if vault.family != family
        || vault.address != address
        || vault.assets.len() != if family == VaultFamily::Lending { 1 } else { 2 }
    {
        return Err(invalid(
            "Canonical vault family, address or underlying asset count is invalid.",
        ));
    }
    gofin_solana::parse_address(&vault.address)?;
    gofin_solana::parse_address(&vault.program)?;
    if let Some(farm) = &vault.farm {
        gofin_solana::parse_address(farm)?;
    }
    let mut mints = BTreeSet::new();
    for token in vault.assets.iter().chain(std::iter::once(&vault.shares)) {
        gofin_solana::parse_address(&token.mint)?;
        if !matches!(
            token.token_program.as_str(),
            gofin_solana::token::TOKEN_PROGRAM_ID | gofin_solana::token::TOKEN_2022_PROGRAM_ID
        ) || !mints.insert(&token.mint)
        {
            return Err(invalid(
                "Canonical vault token identities are invalid or duplicated.",
            ));
        }
    }
    Ok(())
}
pub(crate) fn same_vault_identity(a: &VaultInfo, b: &VaultInfo) -> bool {
    a.family == b.family
        && a.address == b.address
        && a.program == b.program
        && a.assets == b.assets
        && a.shares == b.shares
        && a.farm == b.farm
        && a.backend == b.backend
}
pub(crate) fn validate_position(p: &PositionInfo, vault: &VaultInfo, owner: &str) -> Result<()> {
    validate_vault(&p.vault, vault.family, &vault.address)?;
    if p.owner != owner || !same_vault_identity(&p.vault, vault) {
        return Err(invalid("Position owner or vault identity mismatch."));
    }
    let total = owned_shares(p)?;
    if p.withdrawable_shares_atoms
        .as_deref()
        .map(atoms)
        .transpose()?
        .is_some_and(|v| v > total)
    {
        return Err(invalid("Withdrawable shares exceed total owned shares."));
    }
    if p.underlying.len() != vault.assets.len() {
        return Err(invalid(
            "Position backing must include each underlying asset, using null for unknown amounts.",
        ));
    }
    let mut seen = BTreeSet::new();
    for row in &p.underlying {
        if !vault.assets.contains(&row.token) || !seen.insert(&row.token.mint) {
            return Err(invalid("Position underlying identities mismatch."));
        }
        if let Some(value) = &row.atoms {
            atoms(value)?;
        }
    }
    Ok(())
}
fn validate_eligibility(intent: &VaultIntent, vault: &VaultInfo) -> Result<()> {
    validate_vault(vault, intent.family, &intent.vault)?;
    if (intent.action == VaultAction::Deposit && !vault.deposits_available)
        || (intent.action == VaultAction::Withdraw && !vault.withdrawals_available)
    {
        return Err(GoFinError::new(
            "KAMINO_ACTION_UNAVAILABLE",
            "preparation",
            format!(
                "This vault currently cannot perform {:?}: {}",
                intent.action,
                vault.restrictions.join("; ")
            ),
            "Inspect kamino.vault and kamino.position for liquidity, lock and eligibility restrictions.",
        ));
    }
    if intent.stake && vault.farm.is_none() {
        return Err(invalid(
            "This vault has no associated farm for the requested staking.",
        ));
    }
    Ok(())
}
pub(crate) fn validate_candidate(intent: &VaultIntent, c: &VaultCandidate) -> Result<()> {
    validate_eligibility(intent, &c.vault)?;
    if c.instructions.is_empty()
        || c.quote.rent_lamports > intent.maximum_account_funding_lamports
        || c.quote.network_fee_lamports > intent.maximum_network_fee_lamports
    {
        return Err(invalid("Candidate instructions or cost caps are invalid."));
    }
    let expiry = DateTime::parse_from_rfc3339(&c.quote.expires_at)
        .map_err(|_| invalid("Invalid vault quote expiry."))?;
    if expiry <= Utc::now() {
        return Err(GoFinError::new(
            "KAMINO_QUOTE_EXPIRED",
            "revalidation",
            "The native vault quote expired.",
            "Prepare a fresh candidate within approved authority.",
        ));
    }
    let quoted_shares = atoms(&c.quote.shares_atoms)?;
    if quoted_shares.is_zero() {
        return Err(invalid(
            "The candidate must mint or redeem positive shares.",
        ));
    }
    if intent.action == VaultAction::Withdraw
        && intent.shares_atoms.as_deref() != Some(c.quote.shares_atoms.as_str())
    {
        return Err(invalid(
            "The withdrawal candidate changed its exact share quantity.",
        ));
    }
    if intent.action == VaultAction::Deposit {
        if let Some(minimum) = &intent.minimum_shares_atoms
            && (!c.quote.minimum_shares_enforced || quoted_shares < atoms(minimum)?)
        {
            return Err(invalid(
                "The requested share minimum is not enforced by the native instruction.",
            ));
        }
        let mut seen = BTreeSet::new();
        for row in &c.quote.debits {
            let cap = intent
                .amounts
                .iter()
                .find(|a| a.token == row.token)
                .ok_or_else(|| invalid("A candidate debit has an unapproved asset identity."))?;
            if !seen.insert(&row.token.mint)
                || atoms(
                    row.atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Unknown candidate debit."))?,
                )? > atoms(
                    cap.atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Unknown approved debit."))?,
                )?
            {
                return Err(invalid(
                    "Candidate debit exceeds a reviewed native input cap.",
                ));
            }
        }
    } else {
        if c.quote.debits.len() > 1 {
            return Err(invalid(
                "A withdrawal may debit only its exact canonical shares.",
            ));
        }
        for row in &c.quote.debits {
            if row.token != c.vault.shares || row.atoms.as_deref() != intent.shares_atoms.as_deref()
            {
                return Err(invalid(
                    "A withdrawal may debit only its exact canonical shares.",
                ));
            }
        }
    }
    let mut seen = BTreeSet::new();
    for row in &c.quote.credits {
        let canonical_credit = match intent.action {
            VaultAction::Deposit if row.token == c.vault.shares => {
                row.atoms.as_deref() == Some(c.quote.shares_atoms.as_str())
            }
            VaultAction::Deposit => {
                intent.family == VaultFamily::Liquidity && c.vault.assets.contains(&row.token)
            }
            VaultAction::Withdraw => c.vault.assets.contains(&row.token),
        };
        if !canonical_credit || !seen.insert(&row.token.mint) {
            return Err(invalid(
                "A candidate credit is outside the canonical native operation receipts.",
            ));
        }
        if let Some(value) = &row.atoms {
            atoms(value)?;
        }
    }
    let before = c.metadata["position_shares_before_atoms"]
        .as_str()
        .ok_or_else(|| invalid("The candidate lacks exact owned-share prestate."))?;
    atoms(before)?;
    Ok(())
}
fn validate_signer(owner: &str, frozen: &FrozenTransaction) -> Result<()> {
    if frozen.signer() != owner
        || frozen.fee_payer() != owner
        || frozen.required_signers() != [owner]
        || !frozen.missing_cosigners().is_empty()
    {
        return Err(invalid(
            "Only the selected protected wallet may sign the vault transaction.",
        ));
    }
    Ok(())
}
pub(crate) fn asset(t: &TokenInfo) -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: t.mint.clone(),
        decimals: t.decimals,
    }
}
pub(crate) fn native_asset() -> AssetId {
    AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: "native".into(),
        decimals: 9,
    }
}
pub(crate) fn resource(owner: &str) -> ResourceId {
    ResourceId(format!("solana:mainnet-beta:{owner}"))
}
pub(crate) fn staking_farm(c: &VaultCandidate) -> Result<crate::farms::FarmInfo> {
    let address = c
        .vault
        .farm
        .as_deref()
        .ok_or_else(|| invalid("Missing staking farm identity."))?;
    let rows = match c.vault.family {
        VaultFamily::Lending => c.metadata["farms"].as_array().cloned().unwrap_or_default(),
        VaultFamily::Liquidity => vec![c.metadata["farm"].clone()],
    };
    let matches = rows
        .into_iter()
        .filter(|row| row["address"].as_str() == Some(address))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(invalid("Missing or ambiguous approved staking farm terms."));
    }
    let farm: crate::farms::FarmInfo = serde_json::from_value(matches[0].clone())
        .map_err(|_| invalid("Invalid approved staking farm terms."))?;
    if farm.token != c.vault.shares || farm.time_unit > 1 {
        return Err(invalid(
            "Approved staking farm asset or timing identity mismatch.",
        ));
    }
    Ok(farm)
}
pub(crate) fn farm_custody_summary(farm: &crate::farms::FarmInfo) -> String {
    let unit = if farm.time_unit == 1 {
        "slots"
    } else {
        "seconds"
    };
    let mut terms = Vec::new();
    if farm.warmup > 0 {
        terms.push(format!("warmup {} {unit}", farm.warmup));
    }
    if farm.cooldown > 0 {
        terms.push(format!("cooldown {} {unit}", farm.cooldown));
    }
    if farm.locking_mode != 0 {
        let mode = match farm.locking_mode {
            1 => "continuous, from each stake",
            2 => "fixed start and expiry",
            _ => "restricted",
        };
        terms.push(format!(
            "locking mode {} ({mode}), start {} {unit}, duration {} {unit}",
            farm.locking_mode, farm.locking_start, farm.locking_duration
        ));
    }
    if farm.early_penalty_bps > 0 {
        terms.push(format!(
            "early withdrawal penalty {} basis points",
            farm.early_penalty_bps
        ));
    }
    if terms.is_empty() {
        String::new()
    } else {
        format!(" Farm custody terms: {}.", terms.join("; "))
    }
}
pub(crate) fn invalid(message: impl Into<String>) -> GoFinError {
    GoFinError::validation(message)
}
fn read_only() -> GoFinError {
    GoFinError::new(
        "KAMINO_WALLET_READ_ONLY",
        "preparation",
        "The selected wallet cannot sign.",
        "Choose an enrolled signing wallet.",
    )
}
fn unknown() -> GoFinError {
    GoFinError::new(
        "KAMINO_OPERATION_UNKNOWN",
        "discovery",
        "Unknown Kamino operation or family.",
        "Use gofin capabilities --json.",
    )
}

fn build_prepared(
    operation: &str,
    input: &Value,
    intent: &VaultIntent,
    owner: &str,
    c: &VaultCandidate,
    frozen: &FrozenTransaction,
    fee: u64,
) -> Result<PreparedOperation> {
    let resource = resource(owner);
    let native = native_asset();
    let share = asset(&c.vault.shares);
    let position = settlement::position_id(&c.vault);
    let kind = settlement::position_kind(c.vault.family);
    let mut debits = Vec::new();
    let mut expected = Vec::new();
    if intent.action == VaultAction::Deposit {
        for row in &intent.amounts {
            let n = atoms(
                row.atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Deposit amount missing."))?,
            )?;
            if !n.is_zero() {
                debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset(&row.token),
                        atoms: n.clone(),
                    },
                });
                expected.push(EffectConstraint::Debit {
                    resource: resource.clone(),
                    asset: asset(&row.token),
                    maximum_debit: n,
                });
            }
        }
    } else {
        for token in &c.vault.assets {
            expected.push(EffectConstraint::Credit {
                to: resource.clone(),
                asset: asset(token),
                minimum_credit: Quantity::zero(),
            });
        }
    }
    expected.push(EffectConstraint::PositionOfKind {
        resource: resource.clone(),
        position: position.clone(),
        asset: share.clone(),
        position_kind: kind,
        direction: if intent.action == VaultAction::Deposit {
            Direction::Increase
        } else {
            Direction::Decrease
        },
        maximum_change: if intent.action == VaultAction::Deposit {
            Quantity::from_u64(u64::MAX)
        } else {
            atoms(
                intent
                    .shares_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Withdrawal shares missing."))?,
            )?
        },
    });
    let rent_limits = rent_limits(c)?;
    let rent_cap = rent_limits.iter().try_fold(Quantity::zero(), |sum, row| {
        sum.checked_add(&row.maximum_amount)
    })?;
    if rent_cap > Quantity::from_u64(intent.maximum_account_funding_lamports)
        || rent_cap < Quantity::from_u64(c.quote.rent_lamports)
    {
        return Err(invalid(
            "Rent account identities do not cover the quoted funding within the reviewed cap.",
        ));
    }
    if !rent_cap.is_zero() {
        debits.push(ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: native.clone(),
                atoms: rent_cap,
            },
        });
        expected.push(EffectConstraint::OptionalAccountFunding {
            resource: resource.clone(),
            asset: native.clone(),
            accounts: rent_limits,
        });
    }
    let mut binding = json!({"intent":intent,"wallet_address":owner,"vault":c.vault,"protocol_candidate":c,"message_sha256":frozen.message_sha256(),"last_valid_block_height":c.last_valid_block_height,"expected_network_fee_lamports":fee,"position_shares_before_atoms":c.metadata["position_shares_before_atoms"]});
    binding[VAULT_TRANSITION_BINDING_KEY] = json!({"version":1,"resource":resource,"position":position,"asset":share,"position_kind":settlement::position_kind(c.vault.family),"pre_transaction_shares_atoms":c.metadata["position_shares_before_atoms"]});
    let amounts = |rows: &[TokenAmount]| {
        rows.iter()
            .map(|row| {
                format!(
                    "{} atoms of {}",
                    row.atoms.as_deref().unwrap_or("unknown"),
                    row.token.mint
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let action_summary = match intent.action {
        VaultAction::Deposit => format!(
            "Deposit up to {}; expected {} share atoms of {}",
            amounts(&intent.amounts),
            c.quote.shares_atoms,
            c.vault.shares.mint
        ),
        VaultAction::Withdraw => format!(
            "Redeem {} share atoms of {}; expected {}",
            c.quote.shares_atoms,
            c.vault.shares.mint,
            amounts(&c.quote.credits)
        ),
    };
    let staking = if intent.stake {
        let farm = staking_farm(c)?;
        format!(
            " Stake {} share atoms in farm {}.{}",
            c.metadata["stake_atoms"]
                .as_str()
                .unwrap_or("the disclosed candidate quantity of"),
            farm.address,
            farm_custody_summary(&farm)
        )
    } else {
        String::new()
    };
    let minimum = intent
        .minimum_shares_atoms
        .as_deref()
        .map(|n| {
            format!(
                " Enforced native mint minimum: {n} share atoms before farm entitlement rounding."
            )
        })
        .unwrap_or_default();
    let fees = if c.quote.protocol_fees.is_empty() {
        "Protocol fee amounts are not separately quantified; inspect native quote details for the applicable rates.".to_owned()
    } else {
        format!("Quoted protocol fees: {}.", amounts(&c.quote.protocol_fees))
    };
    Ok(PreparedOperation {
        operation: operation.into(),
        input: input.clone(),
        binding,
        summary: format!("{action_summary}; Kamino {} vault {}, wallet {}.{minimum}{staking} Network fee up to {} lamports; refundable account funding up to {} lamports. {fees}", intent.family.as_str(), intent.vault, intent.wallet, intent.maximum_network_fee_lamports, intent.maximum_account_funding_lamports),
        checks: vec![
            Check { name: "Canonical vault, assets and owner".into(), status: CheckStatus::Passed, detail: "The native instruction graph was independently validated and the protected wallet is the sole signer.".into() },
            Check { name: "Simulation and finite costs".into(), status: CheckStatus::Passed, detail: format!("Frozen message simulation passed; expected network fee {fee} lamports and rent {} lamports.", c.quote.rent_lamports) },
        ],
        warnings: c.quote.warnings.clone(),
        observed_at: Utc::now().to_rfc3339(),
        expires_at: c.quote.expires_at.clone(),
        valuation: None,
        intent: FinancialIntent {
            debits,
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash { resource: resource.clone() },
                amount: Amount { asset: native, atoms: Quantity::from_u64(intent.maximum_network_fee_lamports) },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected,
            locks: vec![resource],
        },
        operation_version: 1,
        action_deadline: None,
    })
}
pub(crate) fn rent_limits(c: &VaultCandidate) -> Result<Vec<AccountFundingLimit>> {
    let rows = c.metadata["refundable_rent_accounts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for row in rows {
        let account = row["account"]
            .as_str()
            .ok_or_else(|| invalid("Rent account identity missing."))?;
        gofin_solana::parse_address(account)?;
        if !seen.insert(account.to_owned()) {
            return Err(invalid("Duplicate rent account."));
        }
        let max = row["maximum_lamports"]
            .as_u64()
            .ok_or_else(|| invalid("Exact rent cap missing."))?;
        result.push(AccountFundingLimit {
            position: format!("account-funding:{account}"),
            maximum_amount: Quantity::from_u64(max),
        });
    }
    Ok(result)
}

fn candidate_baselines(
    owner: &str,
    candidate: &VaultCandidate,
    observed_at: &str,
) -> Result<Vec<PositionAnchor>> {
    let observed = DateTime::parse_from_rfc3339(observed_at)
        .map_err(|_| invalid("Invalid preview observation time."))?;
    let check_time = |value: &str| -> Result<()> {
        let at = DateTime::parse_from_rfc3339(value)
            .map_err(|_| invalid("Invalid exact baseline observation time."))?;
        if at > observed || observed.signed_duration_since(at).num_seconds() > 120 {
            return Err(invalid(
                "The exact position baseline is stale or future-dated; refresh the native preview.",
            ));
        }
        Ok(())
    };
    let position: PositionInfo =
        serde_json::from_value(candidate.metadata["position_before"].clone())
            .map_err(|_| invalid("The candidate lacks a verified exact position baseline."))?;
    validate_position(&position, &candidate.vault, owner)?;
    if !position.complete
        || position.vault.observed_slot.is_none()
        || owned_shares(&position)?.to_string()
            != candidate.metadata["position_shares_before_atoms"]
                .as_str()
                .unwrap_or("")
    {
        return Err(invalid(
            "The exact position baseline is incomplete or disagrees with the candidate prestate.",
        ));
    }
    check_time(&position.vault.observed_at)?;
    let anchors = vec![
        settlement::snapshot_position(&position)?
            .anchor
            .ok_or_else(|| invalid("Missing exact vault baseline anchor."))?,
    ];
    let observations = candidate.metadata["account_funding_observations"]
        .as_array()
        .ok_or_else(|| invalid("Missing exact account-funding observations."))?;
    let limits = rent_limits(candidate)?;
    if observations.len() != limits.len() {
        return Err(invalid("Account-funding baseline coverage is incomplete."));
    }
    for (limit, observation) in limits.iter().zip(observations) {
        let account = limit
            .position
            .strip_prefix("account-funding:")
            .ok_or_else(|| invalid("Invalid rent account identity."))?;
        let at = observation["observed_at"]
            .as_str()
            .ok_or_else(|| invalid("Missing account-funding observation time."))?;
        check_time(at)?;
        if observation["account"].as_str() != Some(account)
            || observation["commitment"] != "finalized"
            || observation["account_absence_proven"] != true
            || observation["context_slot"].as_u64().is_none()
        {
            return Err(invalid("Unverified account-funding absence baseline."));
        }
        // OptionalAccountFunding does not require an account to be created.
        // Its absence proof remains in the native candidate; a finalized
        // positive funding delta establishes its own first post-state anchor.
    }
    Ok(anchors)
}

fn validate_prepared_baselines(
    prepared: &PreparedOperation,
    candidate: &VaultCandidate,
) -> Result<()> {
    let owner = prepared.binding["wallet_address"]
        .as_str()
        .ok_or_else(|| invalid("Missing baseline wallet identity."))?;
    let expected = candidate_baselines(owner, candidate, &prepared.observed_at)?;
    let bound: Vec<PositionAnchor> =
        serde_json::from_value(prepared.binding[BASELINE_ANCHORS_KEY].clone())
            .map_err(|_| invalid("Missing exact bound position baselines."))?;
    if bound != expected {
        return Err(invalid(
            "The bound position baseline changed its verified native observation.",
        ));
    }
    Ok(())
}
