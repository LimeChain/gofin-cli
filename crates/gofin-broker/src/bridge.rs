//! Broker-owned Relay composition over the already protected EVM runtimes.
//! Provider responses are candidates; this adapter alone resolves wallet
//! aliases, freezes wallet transactions, and verifies receipt evidence.

use super::*;
mod base_recovery;
mod cctp_lifecycle;
mod cctp_solana;
mod cctp_solana_status;
mod relay_solana;
use gofin_bridge::{
    BridgeAssetInput, BridgeBounds, BridgeEvidenceCandidate, BridgeIntent, BridgeProvider,
    BridgeQuote, BridgeQuoteInput, BridgeQuoteRequest, BridgeStatusInput, BridgeStep,
    BridgeStepKind as ProviderStepKind, BridgeTransferInput, BridgeTransferRef, EvidenceKind,
    EvmAsset, EvmCall, PreparedBridge,
    base_canonical::{BaseCanonicalConfig, BaseCanonicalProvider},
    canonical::{CanonicalConfig, CanonicalProvider},
    relay::{RelayProvider, ReviewedChain, ReviewedConfig},
};
use gofin_domain::{
    AccountingLocation, Amount as GoFinAmount, BridgeAllocationOwner, BridgeProspectiveDebit,
    BridgeQuoteRevision, BridgeStepKind, BridgeStepLink, BridgeStepState, BridgeTransferRecord,
    BridgeTransferState, Check, CheckStatus, CostBound, CostTreatment, EffectConstraint,
    FinancialEffect, FinancialIntent, OperationKind, Quantity, ResourceAmount, StateSnapshot,
    Status, binding_hash, verified_effect_digest,
};
use gofin_evm::{CanonicalReceipt, FrozenTransaction, alloy::sol_types::SolCall};
use gofin_store::BridgeStateRepository;
use schemars::schema_for;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

const QUOTE_PREPARATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

struct QuotePreparationPhase {
    stage: &'static str,
    provider: Option<String>,
    refresh_transfer_id: Option<String>,
    timeout_ms: u128,
    started: tokio::time::Instant,
}

impl QuotePreparationPhase {
    fn timeout_error(&self) -> GoFinError {
        error(
            "BRIDGE_QUOTE_TIMEOUT",
            "quote",
            "Bridge quote preparation exceeded its bounded read deadline; no quote was saved or financial transaction sent.",
            "Retry bridge.quotes with the same intent and bounds, preserving refreshTransferId when present. Select one provider if comparing routes.",
        ).with_details(json!({"phase": self.stage, "provider": self.provider, "retry_safe": true, "timeout_ms": self.timeout_ms, "elapsed_ms": self.started.elapsed().as_millis(), "remaining_timeout_ms": 0, "refresh_transfer_id": self.refresh_transfer_id}))
    }
}

const RELAY_DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const RELAY_ROUTER: &str = "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f";
const RELAY_HUB: &str = "0xDDD361727C22A01EB137880678A20b0BEaE69318";
const RELAY_HUB_RPC: &str = "https://rpc.chain.relay.link";
const ETHEREUM_USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const ARBITRUM_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const ROBINHOOD_USDG: &str = "0x5fc5360d0400a0fd4f2af552add042d716f1d168";

// Read-only sidecars use the same reviewed fallback lists as wallet runtimes.
fn reviewed_observer_rpc(chain_id: u64) -> gofin_domain::Result<Arc<dyn gofin_evm::EvmRpc>> {
    let network = gofin_networks::by_chain_id(chain_id).ok_or_else(|| {
        GoFinError::validation("Bridge observer chain must be in the reviewed network registry.")
    })?;
    Ok(Arc::new(gofin_evm::HttpRpc::new(
        network
            .rpc_urls
            .iter()
            .map(|url| (*url).to_owned())
            .collect(),
        chain_id,
    )?))
}

gofin_evm::alloy::sol! {
    interface BridgeErc20 {
        function decimals() external view returns (uint8);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

#[derive(Clone, Deserialize)]
struct SavedQuote {
    quote: BridgeQuote,
    bounds: BridgeBounds,
    plan: PreparedBridge,
    step_ids: BTreeMap<u8, String>,
}

struct VerifiedSourceProof {
    evidence_id: String,
    log_index: u64,
    correlation: String,
    destination_locator: Option<gofin_domain::BridgeDestinationLocator>,
}

#[derive(Clone)]
struct VerifiedBridgePayment {
    method: &'static str,
    router: Option<String>,
    transaction_sender: String,
    recipient: String,
    amount: Quantity,
    correlation_id: String,
    native_gas_observation: Option<Value>,
}

/// Construction only uses reviewed provider deployments and configured wallet
/// adapters. The shared state repository is attached by `compose_runtime`.
pub struct BridgeAdapter {
    providers: BTreeMap<String, Arc<dyn BridgeProvider>>,
    wallet_adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
    solana_wallets: BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
    solana_observer: Arc<gofin_solana::SolanaRuntime>,
    observers: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
    confirmation_rules: BTreeMap<u64, (u64, bool)>,
    state: Mutex<Option<BridgeStateRepository>>,
    engine: Mutex<Option<std::sync::Weak<Engine>>>,
    valuator: Arc<SharedUsdValuator>,
    clock: Arc<dyn gofin_domain::clock::Clock>,
}

impl BridgeAdapter {
    pub fn new(
        wallet_adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        valuator: Arc<SharedUsdValuator>,
    ) -> gofin_domain::Result<Self> {
        let observers = wallet_adapters
            .iter()
            .map(|(chain_id, adapter)| (*chain_id, adapter.runtime.clone()))
            .collect::<BTreeMap<_, _>>();
        Self::with_observers(wallet_adapters, observers, valuator)
    }

    pub fn with_observers(
        wallet_adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        mut observers: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
        valuator: Arc<SharedUsdValuator>,
    ) -> gofin_domain::Result<Self> {
        let relay: Arc<dyn BridgeProvider> = Arc::new(RelayProvider::new(reviewed_relay_config())?);
        let mut providers = vec![relay];
        if let Some(source) = wallet_adapters.get(&1) {
            let source_observer = Arc::new(gofin_evm::EvmRuntime::new(
                source.runtime.rpc.clone(),
                1,
                None,
            ));
            providers.push(Arc::new(CanonicalProvider::new(
                source_observer,
                CanonicalConfig::reviewed(),
            )?));
            if let std::collections::btree_map::Entry::Vacant(entry) = observers.entry(8453) {
                entry.insert(Arc::new(gofin_evm::EvmRuntime::new(
                    reviewed_observer_rpc(8453)?,
                    8453,
                    None,
                )));
            }
            let base_rpc = observers[&8453].rpc.clone();
            providers.push(Arc::new(BaseCanonicalProvider::new(
                Arc::new(gofin_evm::EvmRuntime::new(
                    source.runtime.rpc.clone(),
                    1,
                    None,
                )),
                Arc::new(gofin_evm::EvmRuntime::new(base_rpc, 8453, None)),
                BaseCanonicalConfig::reviewed(),
            )?));
        }
        providers.push(Arc::new(gofin_bridge::cctp::CctpProvider::new(
            observers
                .iter()
                .filter(|(chain, _)| matches!(**chain, 1 | 8453 | 42161))
                .map(|(chain, runtime)| {
                    (
                        *chain,
                        Arc::new(gofin_evm::EvmRuntime::new(
                            runtime.rpc.clone(),
                            *chain,
                            None,
                        )),
                    )
                })
                .collect(),
            gofin_bridge::cctp::CctpConfig::reviewed(),
        )?));
        Self::with_providers(providers, wallet_adapters, observers, valuator)
    }

    pub fn with_provider_and_observers(
        provider: Arc<dyn BridgeProvider>,
        wallet_adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        observers: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
        valuator: Arc<SharedUsdValuator>,
    ) -> gofin_domain::Result<Self> {
        Self::with_providers(vec![provider], wallet_adapters, observers, valuator)
    }

    pub fn with_providers(
        providers: Vec<Arc<dyn BridgeProvider>>,
        wallet_adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
        mut observers: BTreeMap<u64, Arc<gofin_evm::EvmRuntime>>,
        valuator: Arc<SharedUsdValuator>,
    ) -> gofin_domain::Result<Self> {
        let mut provider_map = BTreeMap::new();
        for provider in providers {
            let id = provider.capabilities().provider;
            if id.is_empty() || provider_map.insert(id.clone(), provider).is_some() {
                return Err(GoFinError::validation(
                    "Bridge provider identities must be nonempty and unique.",
                ));
            }
        }
        if provider_map.is_empty() {
            return Err(GoFinError::validation(
                "At least one bridge provider must be configured.",
            ));
        }
        // Preserve an injected/configured Base observer, including isolated
        // fixtures. Relay's solver identity uses the same chain but conveys
        // no authority over its wallet runtime.
        if let std::collections::btree_map::Entry::Vacant(entry) = observers.entry(8453) {
            entry.insert(Arc::new(gofin_evm::EvmRuntime::new(
                reviewed_observer_rpc(8453)?,
                8453,
                None,
            )));
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = observers.entry(537713) {
            entry.insert(Arc::new(gofin_evm::EvmRuntime::new(
                Arc::new(gofin_evm::HttpRpc::new(vec![RELAY_HUB_RPC.into()], 537713)?),
                537713,
                None,
            )));
        }
        for chain_id in [42161, 4663] {
            if let std::collections::btree_map::Entry::Vacant(entry) = observers.entry(chain_id) {
                let rpc = reviewed_observer_rpc(chain_id)?;
                entry.insert(Arc::new(gofin_evm::EvmRuntime::new(rpc, chain_id, None)));
            }
        }
        let wallet_confirmation_rules = wallet_adapters
            .iter()
            .map(|(chain, adapter)| {
                (
                    *chain,
                    (
                        adapter.config.confirmations,
                        adapter.config.require_finalized,
                    ),
                )
            })
            .collect();
        let confirmation_rules = bridge_confirmation_rules(wallet_confirmation_rules);
        Ok(Self {
            providers: provider_map,
            wallet_adapters,
            solana_wallets: BTreeMap::new(),
            solana_observer: Arc::new(gofin_solana::SolanaRuntime::new(
                gofin_solana::HttpRpc::mainnet_beta()?,
            )),
            observers,
            confirmation_rules,
            state: Mutex::new(None),
            engine: Mutex::new(None),
            valuator,
            clock: Arc::new(gofin_domain::clock::SystemClock),
        })
    }

    pub fn with_solana_wallets(
        mut self,
        wallets: BTreeMap<String, Arc<gofin_solana::SolanaWalletAdapter>>,
    ) -> Self {
        if let Some(wallet) = wallets.values().next() {
            self.solana_observer = wallet.runtime();
        }
        if let Some(wallet) = wallets.values().next()
            && let Some(provider) = self
                .providers
                .get("cctp")
                .and_then(|provider| provider.as_cctp())
                .cloned()
        {
            self.providers.insert(
                "cctp".into(),
                Arc::new(provider.with_solana_runtime(wallet.runtime())),
            );
        }
        self.solana_wallets = wallets;
        self
    }

    /// Replace only the Relay transport with one carrying a broker-held,
    /// read-only service credential. Wallet authority stays with GoFin's
    /// enrolled source signer and finite bridge authorization.
    pub fn with_relay_api_key(
        mut self,
        api_key: Option<zeroize::Zeroizing<String>>,
    ) -> gofin_domain::Result<Self> {
        if let Some(api_key) = api_key {
            self.providers.insert(
                "relay".into(),
                Arc::new(RelayProvider::with_api_key(
                    reviewed_relay_config(),
                    api_key,
                )?),
            );
        }
        Ok(self)
    }

    #[doc(hidden)]
    pub fn with_clock(mut self, clock: Arc<dyn gofin_domain::clock::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn attach_state(&self, state: BridgeStateRepository) -> gofin_domain::Result<()> {
        let mut current = self.state.lock().map_err(|_| internal_error())?;
        if current.is_some() {
            return Err(GoFinError::validation(
                "Bridge state repository is already attached.",
            ));
        }
        *current = Some(state);
        Ok(())
    }

    pub fn attach_engine(&self, engine: std::sync::Weak<Engine>) -> gofin_domain::Result<()> {
        let mut current = self.engine.lock().map_err(|_| internal_error())?;
        if current.is_some() {
            return Err(GoFinError::validation("Bridge engine is already attached."));
        }
        *current = Some(engine);
        Ok(())
    }

    fn state(&self) -> gofin_domain::Result<BridgeStateRepository> {
        self.state
            .lock()
            .map_err(|_| internal_error())?
            .clone()
            .ok_or_else(|| {
                error(
                    "BRIDGE_STATE_UNAVAILABLE",
                    "startup",
                    "Bridge state repository is not attached to the protected journal.",
                    "Restart the protected broker after repairing its local store.",
                )
            })
    }

    fn provider(&self, id: &str) -> gofin_domain::Result<Arc<dyn BridgeProvider>> {
        self.providers.get(id).cloned().ok_or_else(|| {
            error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "reconciliation",
                "The provider bound to this saved bridge transfer is unavailable.",
                "Keep the transfer on its original provider and inspect the protected journal.",
            )
        })
    }

    fn base_provider(&self) -> gofin_domain::Result<BaseCanonicalProvider> {
        if !self.providers.contains_key("base_canonical") {
            return Err(GoFinError::validation(
                "The saved Base canonical provider is unavailable.",
            ));
        }
        let runtime = |chain| -> gofin_domain::Result<Arc<gofin_evm::EvmRuntime>> {
            let observer = self
                .observers
                .get(&chain)
                .ok_or_else(|| GoFinError::validation("Base canonical observer is unavailable."))?;
            Ok(Arc::new(gofin_evm::EvmRuntime::new(
                observer.rpc.clone(),
                chain,
                None,
            )))
        };
        BaseCanonicalProvider::new(runtime(1)?, runtime(8453)?, BaseCanonicalConfig::reviewed())
    }

    async fn validate_usd_amount_bounds(
        &self,
        bounds: &BridgeBounds,
        source: &GoFinAmount,
        fee_usd_micros: &[u64],
    ) -> gofin_domain::Result<()> {
        if source.asset.decimals > 36 {
            return Err(error(
                "PRICE_ASSET_UNSUPPORTED",
                "valuation",
                "Asset precision exceeds the reviewed valuation boundary.",
                "Use an asset with configured exact decimals.",
            ));
        }
        if bounds.source_usd_per_unit_micros.is_none()
            && bounds.input_usd_micros.is_none()
            && bounds.maximum_total_usd_micros.is_none()
        {
            return Ok(());
        }
        let price = self.valuator.asset_price(&source.asset).await?;
        let unit_price = price
            .usd_micros_per_unit
            .as_str()
            .parse::<u64>()
            .map_err(|_| {
                error(
                    "BRIDGE_PRICE_OUT_OF_RANGE",
                    "pricing",
                    "Fresh source price exceeds the supported exact USD bound.",
                    "Choose a route and price interval within GoFin's exact integer policy range.",
                )
            })?;
        gofin_bridge::validate_usd_bounds(bounds, source, unit_price, fee_usd_micros)
    }

    async fn value_intent(
        &self,
        intent: &FinancialIntent,
    ) -> gofin_domain::Result<gofin_domain::Valuation> {
        let mut assets = intent
            .debits
            .iter()
            .map(|debit| debit.amount.asset.clone())
            .chain(
                intent
                    .maximum_costs
                    .iter()
                    .map(|cost| cost.amount.asset.clone()),
            )
            .collect::<Vec<_>>();
        assets.sort();
        assets.dedup();
        let rows = self.valuator.asset_prices(&assets).await?;
        let prices = Self::validated_prices(&assets, rows, self.clock.now())?;

        let usd_value = |amount: &GoFinAmount| -> gofin_domain::Result<gofin_domain::Quantity> {
            let price = prices.get(&amount.asset).ok_or_else(|| {
                error(
                    "BRIDGE_VALUATION_INVALID",
                    "pricing",
                    "Bridge debit or fee has no matching exact USD price row.",
                    "Refresh GoFin pricing for each source and fee asset before authorization or submission.",
                )
            })?;
            amount
                .atoms
                .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)
        };

        let mut movement_usd_micros = gofin_domain::Quantity::zero();
        for debit in &intent.debits {
            movement_usd_micros = movement_usd_micros.checked_add(&usd_value(&debit.amount)?)?;
        }
        let mut maximum_fee_usd_micros = gofin_domain::Quantity::zero();
        for cost in &intent.maximum_costs {
            maximum_fee_usd_micros =
                maximum_fee_usd_micros.checked_add(&usd_value(&cost.amount)?)?;
        }

        let prices = prices.into_values().collect::<Vec<_>>();
        if prices.is_empty() {
            return Err(error(
                "BRIDGE_VALUATION_INVALID",
                "pricing",
                "Bridge valuation has no pricing observations.",
                "Refresh GoFin pricing before authorization or submission.",
            ));
        }
        // This timestamp describes the aggregate calculation, not a new price
        // observation. Independent assets must never be concatenated into one
        // composite source: every original leg stays in its exact price row.
        let observed_at = self.clock.now().to_rfc3339();
        let source =
            "GoFin bridge valuation from independently attributed exact asset prices".into();

        Ok(gofin_domain::Valuation {
            movement_usd_micros,
            maximum_fee_usd_micros,
            additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
            observed_at,
            source,
            prices,
        })
    }

    async fn fresh_price(
        &self,
        asset: &gofin_domain::AssetId,
    ) -> gofin_domain::Result<gofin_domain::AssetPrice> {
        let price = self.valuator.asset_price(asset).await?;
        Self::validate_price(asset, price, self.clock.now())
    }

    fn validated_prices(
        assets: &[gofin_domain::AssetId],
        rows: Vec<gofin_domain::AssetPrice>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> gofin_domain::Result<BTreeMap<gofin_domain::AssetId, gofin_domain::AssetPrice>> {
        if rows.len() != assets.len() {
            return Err(error(
                "BRIDGE_VALUATION_INVALID",
                "pricing",
                "Bridge USD valuation did not return one price row for each exact asset identity.",
                "Refresh GoFin pricing for each source and fee asset before authorization or submission.",
            ));
        }
        let mut prices = BTreeMap::new();
        for (asset, price) in assets.iter().zip(rows) {
            let price = Self::validate_price(asset, price, now)?;
            if prices.insert(asset.clone(), price).is_some() {
                return Err(error(
                    "BRIDGE_VALUATION_INVALID",
                    "pricing",
                    "Bridge USD valuation returned a duplicate exact asset identity.",
                    "Refresh GoFin pricing for each source and fee asset before authorization or submission.",
                ));
            }
        }
        Ok(prices)
    }

    fn validate_price(
        asset: &gofin_domain::AssetId,
        price: gofin_domain::AssetPrice,
        now: chrono::DateTime<chrono::Utc>,
    ) -> gofin_domain::Result<gofin_domain::AssetPrice> {
        if asset.decimals > 36 {
            return Err(error(
                "BRIDGE_VALUATION_INVALID",
                "pricing",
                "Bridge USD valuation exceeds the supported exact asset precision.",
                "Use an asset with configured exact decimals.",
            ));
        }
        let expected_unit = gofin_domain::Quantity::new(pow10(asset.decimals as u32).to_string())?;
        if price.asset != *asset
            || price.unit_atoms != expected_unit
            || price.usd_micros_per_unit.is_zero()
            || price.source.trim().is_empty()
            || price.source.len() > 1024
            || !gofin_domain::price_freshness::is_fresh(&price.observed_at, &price.source, now)
        {
            return Err(error(
                "BRIDGE_VALUATION_INVALID",
                "pricing",
                "Bridge USD valuation is missing fresh positive pricing for an exact asset identity.",
                "Refresh GoFin pricing for each source and fee asset before authorization or submission.",
            ));
        }
        Ok(price)
    }

    async fn validate_selected_usd_caps(
        &self,
        bounds: &BridgeBounds,
        source: &EvmAsset,
        wallet: &gofin_domain::ResourceId,
    ) -> gofin_domain::Result<()> {
        self.validate_selected_usd_asset_caps(bounds, &source.asset_id(), wallet)
            .await
    }

    async fn validate_selected_usd_asset_caps(
        &self,
        bounds: &BridgeBounds,
        source: &gofin_domain::AssetId,
        wallet: &gofin_domain::ResourceId,
    ) -> gofin_domain::Result<()> {
        let source_cap = bounds
            .maximum_source_debit
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Bridge maximum source debit is missing."))?;
        let costs = bounds
            .fee_caps
            .iter()
            .filter(|cap| {
                matches!(
                    cap.kind,
                    gofin_bridge::FeeKind::Gas | gofin_bridge::FeeKind::AdditionalDebit
                )
            })
            .map(|cap| CostBound {
                location: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                amount: GoFinAmount {
                    asset: cap.asset.clone(),
                    atoms: cap.maximum.clone(),
                },
                treatment: CostTreatment::AdditionalDebit,
            })
            .collect::<Vec<_>>();
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: wallet.clone(),
                amount: GoFinAmount {
                    asset: source.clone(),
                    atoms: source_cap.clone(),
                },
            }],
            maximum_costs: costs,
            expected: vec![],
            locks: vec![wallet.clone()],
        };
        let valuation = self.value_intent(&intent).await?;
        for cap in &bounds.fee_caps {
            let Some(maximum_usd) = cap.maximum_usd_micros else {
                continue;
            };
            let price = self.fresh_price(&cap.asset).await?;
            let cap_usd = cap
                .maximum
                .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?;
            if cap_usd > gofin_domain::Quantity::from_u64(maximum_usd) {
                return Err(error(
                    "BRIDGE_FEE_USD_CAP_EXCEEDED",
                    "pricing",
                    "A selected finite fee cap is worth more than its explicit USD ceiling at fresh GoFin pricing.",
                    "Lower the exact fee cap or its USD ceiling, then review a new authorization scope.",
                ));
            }
        }
        if let Some(maximum) = bounds.maximum_total_usd_micros {
            let total = valuation
                .movement_usd_micros
                .checked_add(&valuation.maximum_fee_usd_micros)?;
            if total > gofin_domain::Quantity::from_u64(maximum) {
                return Err(error(
                    "BRIDGE_TOTAL_USD_CAP_EXCEEDED",
                    "pricing",
                    "Maximum source debit plus cumulative finite gas/additional-debit caps exceeds the approved total USD ceiling.",
                    "Reduce the source debit or finite fee caps before requesting Touch ID.",
                ));
            }
        }
        Ok(())
    }

    async fn validate_frozen_gas_cap(
        &self,
        bounds: &BridgeBounds,
        gas_asset: &gofin_domain::AssetId,
        frozen_maximum_cost: &gofin_domain::Quantity,
    ) -> gofin_domain::Result<()> {
        let cap = bounds
            .fee_caps
            .iter()
            .find(|cap| cap.kind == gofin_bridge::FeeKind::Gas && &cap.asset == gas_asset)
            .ok_or_else(|| GoFinError::validation("Bridge step has no exact finite gas cap."))?;
        if frozen_maximum_cost > &cap.maximum {
            return Err(error(
                "BRIDGE_GAS_CAP_EXCEEDED",
                "preview",
                "Frozen EVM maximum transaction cost exceeds the selected cumulative gas cap.",
                "Request a quote with a supported fee cap and review any changed bound before signing.",
            ));
        }
        if let Some(maximum_usd) = cap.maximum_usd_micros {
            let price = self.fresh_price(gas_asset).await?;
            let actual_usd =
                frozen_maximum_cost.mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?;
            if actual_usd > gofin_domain::Quantity::from_u64(maximum_usd) {
                return Err(error(
                    "BRIDGE_GAS_USD_CAP_EXCEEDED",
                    "pricing",
                    "Frozen EVM maximum transaction cost exceeds the selected gas USD ceiling at fresh GoFin pricing.",
                    "Reduce the transaction gas envelope or review a new fee ceiling before signing.",
                ));
            }
        }
        Ok(())
    }

    async fn allowance_aware_plan(
        &self,
        quote: &BridgeQuote,
        mut plan: PreparedBridge,
        owner: &str,
        bounds: &BridgeBounds,
        previous: Option<&SavedQuote>,
    ) -> gofin_domain::Result<PreparedBridge> {
        if quote.provider == "base_canonical"
            || (quote.provider == "cctp"
                && plan
                    .steps
                    .iter()
                    .filter(|s| s.kind == ProviderStepKind::Mint)
                    .count()
                    > 1)
        {
            return Ok(plan);
        }
        let approval = quote
            .approval
            .as_ref()
            .or_else(|| previous.and_then(|saved| saved.quote.approval.as_ref()));
        let Some(approval) = approval else {
            return Ok(plan);
        };
        let token = gofin_evm::address(&approval.token)?;
        let spender = gofin_evm::address(&approval.spender)?;
        let required = gofin_evm::u256(
            bounds
                .maximum_source_debit
                .as_ref()
                .unwrap_or(&approval.amount),
        )?;
        let destination_steps = plan
            .steps
            .iter()
            .filter(|step| matches!(step.kind, ProviderStepKind::Mint))
            .cloned()
            .collect::<Vec<_>>();
        let kinds = if let Some(previous) = previous {
            previous
                .plan
                .steps
                .iter()
                .filter(|step| step.kind != ProviderStepKind::Mint)
                .map(|step| step.kind)
                .collect::<Vec<_>>()
        } else {
            let observer = self
                .observers
                .get(&quote.intent.evm_source()?.chain_id)
                .ok_or_else(|| {
                    error(
                        "BRIDGE_NETWORK_UNAVAILABLE",
                        "quote",
                        "Source-chain allowance observer is unavailable.",
                        "Choose a configured wallet network and retry the quote.",
                    )
                })?;
            let allowance = observer
                .call(
                    token,
                    &BridgeErc20::allowanceCall {
                        owner: gofin_evm::address(owner)?,
                        spender,
                    },
                    None,
                )
                .await?;
            if allowance >= required {
                vec![ProviderStepKind::Deposit]
            } else if !allowance.is_zero() {
                vec![
                    ProviderStepKind::ResetAllowance,
                    ProviderStepKind::Approval,
                    ProviderStepKind::Deposit,
                ]
            } else {
                vec![ProviderStepKind::Approval, ProviderStepKind::Deposit]
            }
        };

        if !matches!(
            kinds.as_slice(),
            [ProviderStepKind::Deposit]
                | [ProviderStepKind::Approval, ProviderStepKind::Deposit]
                | [
                    ProviderStepKind::ResetAllowance,
                    ProviderStepKind::Approval,
                    ProviderStepKind::Deposit
                ]
        ) {
            return Err(error(
                "BRIDGE_STEP_PLAN_INVALID",
                "quote",
                "Saved ERC-20 step structure is not a supported finite allowance plan.",
                "Keep the transfer unresolved and request a new reviewed quote.",
            ));
        }
        let provider_approval = plan
            .steps
            .iter()
            .find(|step| step.kind == ProviderStepKind::Approval)
            .cloned()
            .or_else(|| {
                previous.and_then(|saved| {
                    // A satisfied allowance can make the provider omit this
                    // transaction on refresh. Retain the validated finite call
                    // and immutable topology; refresh never submits it.
                    let prior = saved.quote.approval.as_ref()?;
                    if !prior.token.eq_ignore_ascii_case(&approval.token)
                        || !prior.spender.eq_ignore_ascii_case(&approval.spender)
                    {
                        return None;
                    }
                    saved
                        .plan
                        .steps
                        .iter()
                        .find(|step| step.kind == ProviderStepKind::Approval)
                        .cloned()
                })
            });
        let deposit = plan
            .steps
            .iter()
            .find(|step| step.kind == ProviderStepKind::Deposit)
            .cloned()
            .ok_or_else(|| GoFinError::validation("Bridge quote lacks its source deposit step."))?;
        let mut steps = Vec::with_capacity(kinds.len());
        for (ordinal, kind) in kinds.iter().enumerate() {
            let mut step = match kind {
                ProviderStepKind::ResetAllowance => BridgeStep {
                    ordinal: ordinal as u8,
                    kind: ProviderStepKind::ResetAllowance,
                    solana_mint: None,
                    solana_transaction: None,
                    call: Some(EvmCall {
                        chain_id: quote.intent.evm_source()?.chain_id,
                        to: approval.token.to_ascii_lowercase(),
                        value: gofin_domain::Quantity::zero(),
                        data: format!(
                            "0x{}",
                            hex::encode(
                                BridgeErc20::approveCall {
                                    spender,
                                    amount: gofin_evm::alloy::primitives::U256::ZERO,
                                }
                                .abi_encode()
                            )
                        ),
                    }),
                    expected_sender: owner.to_ascii_lowercase(),
                    maximum_debit: vec![],
                    dependencies: vec![],
                },
                ProviderStepKind::Approval => provider_approval.clone().ok_or_else(|| {
                    GoFinError::validation(
                        "Relay quote omitted the exact finite approval transaction.",
                    )
                })?,
                ProviderStepKind::Deposit => deposit.clone(),
                ProviderStepKind::Prove | ProviderStepKind::Finalize | ProviderStepKind::Mint => {
                    return Err(GoFinError::validation(
                        "Deferred stages require the Base canonical provider.",
                    ));
                }
            };
            step.ordinal = ordinal as u8;
            step.dependencies = match kind {
                ProviderStepKind::ResetAllowance => vec![],
                ProviderStepKind::Approval if ordinal > 0 => vec![(ordinal - 1) as u8],
                ProviderStepKind::Approval => vec![],
                ProviderStepKind::Deposit if ordinal > 0 => vec![(ordinal - 1) as u8],
                ProviderStepKind::Deposit => vec![],
                ProviderStepKind::Prove | ProviderStepKind::Finalize | ProviderStepKind::Mint => {
                    vec![]
                }
            };
            steps.push(step);
        }
        for mut destination in destination_steps {
            destination.ordinal = u8::try_from(steps.len()).map_err(|_| internal_error())?;
            destination.dependencies = vec![
                steps
                    .iter()
                    .find(|step| step.kind == ProviderStepKind::Deposit)
                    .ok_or_else(internal_error)?
                    .ordinal,
            ];
            steps.push(destination);
        }
        plan.steps = steps;
        if let Some(step) = plan
            .steps
            .iter_mut()
            .find(|step| step.kind == ProviderStepKind::Approval)
        {
            step.evm_call_mut()?.data = format!(
                "0x{}",
                hex::encode(
                    BridgeErc20::approveCall {
                        spender,
                        amount: required,
                    }
                    .abi_encode()
                )
            );
        }
        Ok(plan)
    }

    async fn current_allowance(
        &self,
        quote: &BridgeQuote,
        owner: &str,
    ) -> gofin_domain::Result<gofin_evm::alloy::primitives::U256> {
        let Some(token) = quote.intent.evm_source()?.token.as_deref() else {
            return Ok(gofin_evm::alloy::primitives::U256::ZERO);
        };
        let spender = quote
            .approval
            .as_ref()
            .map(|approval| approval.spender.as_str())
            .unwrap_or(if quote.provider == "cctp" {
                gofin_bridge::cctp::TOKEN_MESSENGER
            } else {
                RELAY_DEPOSITORY
            });
        let observer = self
            .observers
            .get(&quote.intent.evm_source()?.chain_id)
            .ok_or_else(|| {
                GoFinError::validation("Source-chain allowance observer is unavailable.")
            })?;
        observer
            .call(
                gofin_evm::address(token)?,
                &BridgeErc20::allowanceCall {
                    owner: gofin_evm::address(owner)?,
                    spender: gofin_evm::address(spender)?,
                },
                None,
            )
            .await
    }

    async fn validate_quote_prices(
        &self,
        bounds: &BridgeBounds,
        quote: &BridgeQuote,
    ) -> gofin_domain::Result<Vec<gofin_bridge::PricedBridgeFee>> {
        if quote.minimum_receipt.asset != quote.expected_receipt.asset {
            return Err(error(
                "BRIDGE_QUOTE_INVALID",
                "quote",
                "Relay quote minimum and expected receipt use different asset identities.",
                "Discard the quote and request a new one.",
            ));
        }
        if let Some(minimum) = &bounds.minimum_net_receipt
            && (minimum.asset != quote.minimum_receipt.asset
                || quote.minimum_receipt.atoms < minimum.atoms)
        {
            return Err(error(
                "BRIDGE_BOUNDS_VIOLATED",
                "quote",
                "Relay's minimum net receipt is below the explicit approved floor.",
                "Choose a route whose guaranteed receipt meets the requested minimum.",
            ));
        }
        for fee in &quote.fees {
            if let Some(cap) = bounds
                .fee_caps
                .iter()
                .find(|cap| cap.asset == fee.amount.asset && cap.kind == fee.kind)
                && fee.amount.atoms > cap.maximum
            {
                return Err(error(
                    "BRIDGE_BOUNDS_VIOLATED",
                    "quote",
                    "Relay fee exceeds the explicit atomic fee cap.",
                    "Choose another quote or request new explicit fee bounds.",
                ));
            }
        }
        let price_source = bounds.source_usd_per_unit_micros.is_some()
            || bounds.input_usd_micros.is_some()
            || bounds.maximum_total_usd_micros.is_some();
        if quote.gross_debit.asset.decimals > 36 {
            return Err(error(
                "PRICE_ASSET_UNSUPPORTED",
                "valuation",
                "Asset precision exceeds the reviewed valuation boundary.",
                "Use an asset with configured exact decimals.",
            ));
        }
        let mut amounts = quote
            .fees
            .iter()
            .map(|fee| fee.amount.clone())
            .collect::<Vec<_>>();
        if price_source {
            amounts.push(GoFinAmount {
                asset: quote.gross_debit.asset.clone(),
                atoms: Quantity::from_decimal("1", quote.gross_debit.asset.decimals)?,
            });
        }
        let mut values = self.valuator.usd_micros_batch(&amounts).await?;
        if values.len() != amounts.len() {
            return Err(error(
                "PRICE_DATA_INVALID",
                "valuation",
                "Exact bridge amount valuation returned an incomplete batch.",
                "Retry pricing for the exact source and fee asset identities.",
            ));
        }
        let source_value = price_source.then(|| values.pop().expect("source unit was appended"));
        let mut priced_fees = Vec::with_capacity(quote.fees.len());
        for (fee, (usd, _, _)) in quote.fees.iter().zip(values) {
            let usd = usd.as_str().parse::<u64>().map_err(|_| {
                error(
                    "BRIDGE_PRICE_OUT_OF_RANGE",
                    "pricing",
                    "Fee USD value exceeds the supported exact bound.",
                    "Choose a fee cap within GoFin's exact integer policy range.",
                )
            })?;
            priced_fees.push(gofin_bridge::PricedBridgeFee {
                fee: fee.clone(),
                usd_micros: usd,
            });
        }
        gofin_bridge::validate_fee_usd_caps(bounds, &priced_fees)?;
        let fee_usd = priced_fees
            .iter()
            .filter(|priced| priced.fee.kind != gofin_bridge::FeeKind::WithheldFromOutput)
            .map(|priced| priced.usd_micros)
            .collect::<Vec<_>>();
        if let Some((source_usd, _, _)) = source_value {
            let unit_price = source_usd.parse::<u64>().map_err(|_| {
                error(
                    "BRIDGE_PRICE_OUT_OF_RANGE",
                    "pricing",
                    "Fresh source price exceeds the supported exact USD bound.",
                    "Choose a route and price interval within GoFin's exact integer policy range.",
                )
            })?;
            gofin_bridge::validate_usd_bounds(bounds, &quote.gross_debit, unit_price, &fee_usd)?;
        }
        Ok(priced_fees)
    }

    pub fn operation_definitions() -> Vec<Definition> {
        vec![
            base_recovery::base_recovery_definition(),
            Definition {
                name: "bridge.routes".into(),
                description: "Discover indicative bridge routes by exact source asset and destination chain, with provider-specific availability and independently verified candidate token identities.".into(),
                kind: OperationKind::Read,
                execution_actor: gofin_domain::ExecutionActor::AgentReadable,
                input_schema: serde_json::to_value(schema_for!(gofin_bridge::BridgeRoutesInput)).expect("schema serializes"),
                output_schema: json!({"type":"object","required":["indicative","source","coverage","routes","next_action"],"properties":{"indicative":{"const":true},"routes":{"type":"array"}}}),
                error_schema: schema_for_gofin_error(),
                examples: vec![json!({"source":{"chainId":1},"destinationChainId":8453}), json!({"source":{"chainId":8453,"token":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},"destinationChainId":1,"destinationAsset":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}})],
                coverage: "Configured providers only. Currency/indexer lists are partial; empty discovery is unknown rather than proof that no route exists. Exact destination assets remain available for quotes.".into(),
                completion: "Returns indicative candidates and availability reasons. Creates no transfer, reservation, approval, submission or executable calldata.".into(),
                semantics: json!({"financial_authorization":false,"may_submit":false,"indicative":true,"asset_identity":"Exact EVM chain/token or Solana mainnet-beta/mint; symbols never establish equivalence.","token_decimals":"Independently verified at pinned RPC blocks; provider metadata is a candidate only.","next":"Choose exact destination/refund representations, wallet, recipient and amount using bridge.quotes before approval."}),
            },
            Definition {
                name: "bridge.quotes".into(),
                description: "Compare the configured Relay, Arbitrum canonical, Base canonical and Circle CCTP V2 bridge routes for exact enrolled wallet, asset and recipient identities, including Relay transfers in either direction between Solana mainnet-beta and reviewed EVM networks and CCTP EVM-to-Solana destinations.".into(),
                kind: OperationKind::Read,
                execution_actor: gofin_domain::ExecutionActor::AgentReadable,
                input_schema: serde_json::to_value(schema_for!(BridgeQuoteInput)).expect("schema serializes"),
                output_schema: json!({"type":"object","required":["coverage","transfers","failures"],"properties":{"coverage":{"type":"object","required":["providers","compared_providers","market_wide_comparison","complete"]},"transfers":{"type":"array","items":{"type":"object","required":["transfer_id","quote","priced_fees","steps","settlement_policy","next_action"],"properties":{"settlement_policy":{"type":"object","required":["source","destination","arrival_estimate_scope"],"properties":{"source":{"oneOf":[{"type":"object","required":["chain_id","min_confirmations","require_finalized"]},{"type":"object","required":["network","commitment"]}]},"destination":{"oneOf":[{"type":"object","required":["chain_id","min_confirmations","require_finalized"]},{"type":"object","required":["network","commitment"]}]},"arrival_estimate_scope":{"type":"string"}}}}}},"failures":{"type":"array"}}}),
                error_schema: schema_for_gofin_error(),
                examples: vec![
                    json!({"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"amount":"10000000"},"bounds":{}}),
                    json!({"intent":{"wallet":"wallet","source":{"chainId":1},"destinationChainId":4663,"destinationAssets":[{"chainId":4663}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[],"amount":"10000000000000000"},"bounds":{}})
                    ,json!({"provider":"cctp","cctp":{"transferMode":"standard"},"intent":{"wallet":"base-wallet","source":{"chainId":8453,"token":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},"solanaDestination":{"network":"solana:mainnet-beta","wallet":"solana-wallet","mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"},"recipient":"11111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[],"amount":"10000000"},"bounds":{"fee_caps":[{"asset":{"network":"ethereum:8453","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"},{"asset":{"network":"solana:mainnet-beta","reference":"native","decimals":9},"kind":"gas","maximum":"100000"},{"asset":{"network":"solana:mainnet-beta","reference":"native","decimals":9},"kind":"additional_debit","maximum":"2000000"}]}})
                ],
                coverage: "Compares configured Relay, Arbitrum canonical, Base canonical and Circle CCTP V2 routes where their exact assets and networks apply. The response attributes successful quotes and per-provider failures; it reports coverage of these configured providers only, not the market.".into(),
                completion: "Returns exact assets, GoFin-priced fees, net receipt floors, provider arrival estimates when supplied, refund paths, quote validity and finite reviewed steps. Quotes reserve no funds and do not authorize a write.".into(),
                semantics: json!({"service_required":true,"financial_authorization":false,"preparation_timeout_seconds":60,"preparation":"One budget covers asset observations, provider quotes, pricing and allowance reads after broker admission. Exact token observations are reused within this request, prices are batched, and expiry is rechecked before saving. Timeout reports its phase and provider, saves no quote revision and sends no transaction; retry the same intent and preserve refreshTransferId.","provider_credentials":"Relay /quote is keyless; an optional protected key enables authenticated request lookup and is never supplied in bridge input or output","providers":[{"id":"relay","coverage":"quote-dependent exact EVM assets and mainnet-beta SOL/SPL/Token-2022 in both directions; only fully validated source transactions are executable"},{"id":"arbitrum_canonical","coverage":"native ETH Ethereum Mainnet to Arbitrum One or Robinhood Chain; same source-wallet recipient; no refund path"},{"id":"base_canonical","coverage":"bidirectional Ethereum/Base ETH and verified StandardBridge ERC-20 pairs; separate withdrawal prove/finalize stages"},{"id":"cctp","coverage":"Native-USDC routes among Ethereum, Base and Arbitrum and from those EVM networks to Solana mainnet-beta; Standard and Fast; explicit destination mint"}],"cctp":{"networks":[1,42161,8453],"additional_destination":{"network":"solana:mainnet-beta","direction":"EVM source to Solana only","signer":"enrolled destination owner","account":"existing native-USDC ATA","maximum_attester_threshold":2,"transaction_size":"Direct mint requires at most two Circle attesters; larger thresholds fail before source burn."},"modes":["standard","fast"],"default_mode":"standard","gas":"Source ETH and destination ETH or SOL required; Solana needs separate gas and nonrefundable nonce-account funding caps. Mint estimates remain provisional until attestation.","stages":["approval_if_needed","deposit (burn)","attestation","mint","verified_receipt"],"unavailable":{"robinhood":"No official CCTP domain/deployment","solana_reverse":"Solana-source burns are not implemented"},"refunds":"No refund path; resume the original mint","funding_hyperliquid":"CCTP to Arbitrum owner wallet, then explicit hyperliquid.account.deposit and verified account credit"},"provider_comparison":"configured providers only; not market-wide","provider_selection":"Omitted or null provider compares configured providers for EVM-source routes. Solana-source routes automatically select Relay, the only supported source provider; an explicit provider is never substituted.","provider_usd_fields":"ignored; fee USD values are independently priced by GoFin","agent_supplied_calldata":false,"quote_persistence":"broker-owned, restart-safe","exact_child_plan_renewal":"Exact bridge.transfer child plans remain supported: preserve each original input and permission when renewing; no adapter cumulative scope or conversion to bridge.execute is required.","expired_source_quote_recovery":"Preparing an unsubmitted source step refreshes an expired saved quote once through the original transfer ID, provider, mode, route and persisted bounds. Settled allowance children and step IDs remain fixed. Submitted or uncertain source actions cannot refresh. This read does not reset candidate retry limits or renew authority.","command_line_example":"gofin read bridge.quotes --input JSON --json"}),
            },
            Definition {
                name: "bridge.execute".into(),
                description: "Execute one explicitly selected saved bridge child under the provider-neutral intent in a named plan. Review uses the intent form; each execution uses transferId, stepId, stepKind and the exact approved bounds.".into(),
                kind: OperationKind::Write,
                execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
                input_schema: serde_json::to_value(schema_for!(gofin_bridge::BridgeExecuteInput)).expect("schema serializes"),
                output_schema: json!({"type":"object","required":["transfer_id","step_id","kind","status"]}),
                error_schema: schema_for_gofin_error(),
                examples: vec![
                    json!({"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":4663,"destinationAssets":[{"chainId":4663,"token":"0x5fc5360d0400a0fd4f2af552add042d716f1d168"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"}],"amount":{"minimum":"10000000","maximum":"20000000"}},"allowedProviders":["relay"],"bounds":{"input":{"minimum":"10000000","maximum":"20000000"},"maximum_source_debit":"20000000","minimum_net_receipt":{"asset":{"network":"ethereum:4663","reference":"0x5fc5360d0400a0fd4f2af552add042d716f1d168","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}),
                    json!({"transferId":"SYNTHETIC-transfer-id","stepId":"SYNTHETIC-step-id","stepKind":"deposit","bounds":{"input":{"minimum":"10000000","maximum":"20000000"},"maximum_source_debit":"20000000","minimum_net_receipt":{"asset":{"network":"ethereum:4663","reference":"0x5fc5360d0400a0fd4f2af552add042d716f1d168","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}),
                ],
                coverage: "Plan review binds exact wallet, source and destination assets, recipient, refund scope, provider set, quantity interval and finite financial bounds without requesting or selecting a quote. Execution nominates one saved bridge child at a time; every candidate is checked against the approved scope.".into(),
                completion: "The logical plan step remains in progress until the nominated transfer has verified destination settlement. A submitted or uncertain child pins the transfer identity for reconciliation.".into(),
                semantics: json!({"service_required":true,"plan_only":true,"financial_authorization":"named-plan revision with one provider-neutral scope and finite bounds","plan_permission":{"supported":true,"mode":"agent_nominated","requires_adapter_scope":true,"bounds_schema":{"type":"object","additionalProperties":false,"required":["mode"],"properties":{"mode":{"const":"agent_nominated"}}},"scope":"Exact wallet, source/destination identities, recipient, refund scope and provider set; amount and finite financial bounds are carried in the reviewed intent input."},"providers":["relay","arbitrum_canonical","base_canonical"],"selection":"agent nominates one saved quote child at execution; provider must be in the approved set","execution_input":"Provide transferId, stepId and stepKind from a saved quote, plus the exact bounds reviewed with the intent.","agent_supplied_calldata":false,"submission_identity":"each child has its own immutable operation ID and reconciliation identity","command_line_example":"gofin plans preview --input PLAN_JSON --json"}),
            },
            Definition {
                name: "bridge.transfer".into(),
                description: "Prepare or execute one persisted bridge approval, source deposit, Base withdrawal proof/finalization or CCTP destination mint under its exact or named-plan authority.".into(),
                kind: OperationKind::Write,
                execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
                input_schema: serde_json::to_value(schema_for!(BridgeTransferInput)).expect("schema serializes"),
                output_schema: json!({"type":"object","required":["transfer_id","step_id","kind","status"]}),
                error_schema: schema_for_gofin_error(),
                examples: vec![json!({"transferId":"SYNTHETIC-transfer-id","stepId":"SYNTHETIC-step-id","stepKind":"deposit","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}})],
                coverage: "CCTP V2 supports all six directed native-USDC routes between Ethereum, Base and Arbitrum, and EVM-source transfers to Solana mainnet-beta, in Standard (default) and Fast modes with explicit mint. Solana requires an enrolled destination signer, existing native-USDC ATA, SOL gas and a separate nonrefundable nonce-account funding cap. Relay handles quote-dependent exact EVM assets and mainnet-beta SOL/SPL/Token-2022 in both directions; only fully validated Solana source transactions are executable. Base canonical handles ETH and verified ERC-20 pairs in both directions, including deferred withdrawal proof/finalization stages. Canonical Inbox handles native ETH from Ethereum Mainnet to Arbitrum One or Robinhood Chain when the destination is the same EOA; that route has no refund path.".into(),
                completion: "Each wallet step is a separate immutable operation. Source proof moves principal to nonspendable transit; destination settlement remains pending until the original provider's exact destination proof and configured finality are verified.".into(),
                semantics: json!({"service_required":true,"financial_authorization":"fresh per-write Touch ID, one finite transfer-boundary grant, or one named-plan revision with each exact bridge step listed separately","named_plan_support":"adapter-reviewed one step per bridge.transfer input; every step binds the same persisted transfer and exact step ID/kind, and selected BridgeBounds are independently enforced","providers":["relay","arbitrum_canonical","base_canonical","cctp"],"source_and_destination":"reviewed EVM networks and Solana mainnet-beta; Relay supports exact external Solana recipient without enrollment","observer_only_chains":[{"chain_id":537713,"name":"RelayChain","wallet_authority":false}],"agent_supplied_calldata":false,"request_identity":"each step has its own operation ID; uncertain submissions reconcile under the original ID","command_line_example":"gofin preview bridge.transfer --input '{\"transferId\":\"TRANSFER_ID\",\"stepId\":\"STEP_ID\",\"stepKind\":\"deposit\",\"bounds\":{...}}'"}),
            },
            Definition {
                name: "bridge.status".into(),
                description: "Reconcile a saved bridge transfer without requesting new authority or resubmitting a wallet step.".into(),
                kind: OperationKind::Read,
                execution_actor: gofin_domain::ExecutionActor::AgentReadable,
                input_schema: serde_json::to_value(schema_for!(BridgeStatusInput)).expect("schema serializes"),
                output_schema: json!({
                    "type":"object",
                    "required":["transfer_id","state","next_action","settlement","settlement_policy"],
                    "properties":{
                        "settlement_policy":{"type":"object","required":["source","destination","arrival_estimate_scope"]},
                        "provider_status":{"oneOf":[{"type":"null"},{"type":"object"}]},
                        "settlement_accounting":{"description":"Optional idempotent plan-accounting reconciliation derived from the immutable verified settlement proof; separate from proof effects.","oneOf":[{"type":"null"},{"type":"object","required":["proof_digest","cost"],"properties":{"proof_digest":{"type":"string"},"cost":{"type":"object"}}}]},
                        "settlement":{"oneOf":[{"type":"null"},{"type":"object","required":["outcome","evidence_id","observed_at","cash_credits","costs","proof"],"properties":{"outcome":{"enum":["destination","refund"]},"evidence_id":{"type":"string"},"observed_at":{"type":"string"},"cash_credits":{"type":"array"},"costs":{"type":"array"},"proof":{"type":"object"}}}]}
                    }
                }),
                error_schema: schema_for_gofin_error(),
                examples: vec![json!({"transferId":"SYNTHETIC-transfer-id"})],
                coverage: "Reconciles saved CCTP, Base canonical, Relay and Canonical Inbox transfers from protected quote/source evidence; unused quotes are not financial pending state.".into(),
                completion: "CCTP requires the original message and canonical native-USDC mint receipt; burn and attestation alone remain intermediate. Relay settles only after exact reviewed-router or direct-order-tag proof and the configured destination confirmation policy. Canonical settles only after the source-derived type-0x64 child transaction and configured destination confirmation policy match the saved message locator. Repeated status reads include the persisted typed credit, costs and sanitized proof identity; verified measured plan costs may also appear separately as settlement_accounting without changing that proof.".into(),
                semantics: json!({"financial_authorization":false,"may_reconcile":true,"may_submit":false,"child_readiness":"Parent approval_pending can include settled allowance children. Read child states and dependencies: an unsubmitted child whose prerequisites are settled is ready for preview; fresh execution checks still apply.","provider_identity":"saved provider only; no fallback or route switching","provider_request_identity":"broker-persisted only","settlement_accounting":"optional idempotent measured cost accounting is returned separately from immutable proof effects"}),
            },
        ]
    }

    fn wallet(
        &self,
        alias: &str,
        chain_id: u64,
    ) -> gofin_domain::Result<gofin_domain::ConnectionMetadata> {
        let adapter = self.wallet_adapters.get(&chain_id).ok_or_else(|| {
            error(
                "BRIDGE_NETWORK_UNAVAILABLE",
                "validation",
                "Selected bridge source chain has no enrolled EVM wallet.",
                "Select Ethereum, Arbitrum One or Robinhood Chain with the configured wallet.",
            )
        })?;
        adapter
            .connections()
            .into_iter()
            .find(|connection| {
                connection.name == alias
                    && connection.network.as_deref() == Some(&format!("ethereum:{chain_id}"))
            })
            .ok_or_else(|| {
                error(
                    "BRIDGE_WALLET_UNKNOWN",
                    "validation",
                    "The selected wallet is not configured on the source chain.",
                    "Select a configured signing EVM wallet.",
                )
            })
    }

    async fn asset(
        &self,
        input: &BridgeAssetInput,
        expected_chain: u64,
    ) -> gofin_domain::Result<EvmAsset> {
        if input.chain_id != expected_chain {
            return Err(error(
                "BRIDGE_ASSET_CHAIN_MISMATCH",
                "validation",
                "Selected asset belongs to a different EVM chain.",
                "Choose an asset whose chain_id matches the selected route.",
            ));
        }
        let token = input
            .token
            .as_deref()
            .filter(|token| !token.eq_ignore_ascii_case("native"));
        if !matches!(input.chain_id, 1 | 42161 | 4663 | 8453) {
            return Err(GoFinError::validation("Select a reviewed bridge network."));
        }
        let decimals = if let Some(token) = token {
            let token_address = gofin_evm::address(token)?;
            if token_address == gofin_evm::alloy::primitives::Address::ZERO {
                return Err(GoFinError::validation(
                    "Use native for ETH; ERC-20 contracts must be nonzero.",
                ));
            }
            let runtime = self.observers.get(&input.chain_id).ok_or_else(|| {
                GoFinError::validation("The exact token network has no configured observer.")
            })?;
            runtime.assert_chain().await?;
            let block =
                gofin_evm::rpc_u64(&runtime.rpc.request("eth_blockNumber", json!([])).await?)?;
            let code = gofin_evm::rpc_bytes(
                &runtime
                    .rpc
                    .request(
                        "eth_getCode",
                        json!([
                            gofin_evm::lower_address(token_address),
                            format!("{block:#x}")
                        ]),
                    )
                    .await?,
            )?;
            if code.is_empty() {
                return Err(error(
                    "BRIDGE_TOKEN_UNAVAILABLE",
                    "validation",
                    "Selected ERC-20 contract has no code at the observed block.",
                    "Choose an exact deployed token on the selected chain.",
                ));
            }
            runtime
                .call(token_address, &BridgeErc20::decimalsCall {}, Some(block))
                .await?
        } else {
            18
        };
        Ok(EvmAsset {
            chain_id: input.chain_id,
            token: token.map(|value| value.to_ascii_lowercase()),
            decimals,
        })
    }

    async fn quote_asset(
        &self,
        input: &BridgeAssetInput,
        expected_chain: u64,
        observations: &mut BTreeMap<(u64, Option<String>), EvmAsset>,
    ) -> gofin_domain::Result<EvmAsset> {
        // Validate the requested chain even when an identity is already verified.
        if input.chain_id != expected_chain {
            return self.asset(input, expected_chain).await;
        }
        let token = input
            .token
            .as_deref()
            .filter(|token| !token.eq_ignore_ascii_case("native"))
            .map(|token| gofin_evm::address(token).map(gofin_evm::lower_address))
            .transpose()?;
        let key = (input.chain_id, token);
        if let Some(asset) = observations.get(&key) {
            return Ok(asset.clone());
        }
        let asset = self.asset(input, expected_chain).await?;
        observations.insert(key, asset.clone());
        Ok(asset)
    }

    async fn revalidate_relay_assets(&self, quote: &BridgeQuote) -> gofin_domain::Result<()> {
        let mut chains = BTreeMap::<u64, Vec<EvmAsset>>::new();
        for asset in quote
            .intent
            .source
            .iter()
            .chain(quote.intent.destination_assets.iter())
            .chain(quote.intent.allowed_refund_assets.iter())
        {
            chains
                .entry(asset.chain_id)
                .or_default()
                .push(asset.clone());
        }
        let observe_chain = |assets: Vec<EvmAsset>| async move {
            // A fresh, attempt-local cache: source/refund aliases may reuse one
            // pinned observation, but every declared precision is still checked.
            let mut observations = BTreeMap::new();
            for asset in assets {
                let fresh = self
                    .quote_asset(
                        &BridgeAssetInput {
                            chain_id: asset.chain_id,
                            token: asset.token.clone(),
                        },
                        asset.chain_id,
                        &mut observations,
                    )
                    .await?;
                if fresh != asset {
                    return Err(GoFinError::validation(
                        "Exact Relay asset precision or deployment changed before dispatch.",
                    ));
                }
            }
            Ok::<_, GoFinError>(())
        };
        // Bound fanout to two independent chain observations. Each chain keeps
        // its own pinned deployment/decimals reads and exact identity cache.
        let mut chains = chains.into_values();
        while let Some(first) = chains.next() {
            if let Some(second) = chains.next() {
                tokio::try_join!(observe_chain(first), observe_chain(second))?;
            } else {
                observe_chain(first).await?;
            }
        }
        Ok(())
    }

    fn check_quote_freshness(&self, quote: &BridgeQuote) -> gofin_domain::Result<()> {
        let expires = chrono::DateTime::parse_from_rfc3339(&quote.valid_until).map_err(|_| {
            error(
                "BRIDGE_QUOTE_INVALID",
                "quote",
                "Bridge quote has an invalid expiry.",
                "Request a fresh quote for the same exact intent.",
            )
        })?;
        if expires <= self.clock.now() {
            return Err(error(
                "BRIDGE_QUOTE_EXPIRED", "quote",
                "Bridge quote expired during preparation; no financial transaction was sent.",
                "Refresh the same transfer and unchanged bounds, or retry the same intent if no transfer ID was returned.",
            ).with_details(json!({"provider": quote.provider, "valid_until": quote.valid_until, "retry_safe": true})));
        }
        Ok(())
    }

    async fn routes(&self, input: &Value) -> gofin_domain::Result<Value> {
        let input: gofin_bridge::BridgeRoutesInput = serde_json::from_value(input.clone())
            .map_err(|_| {
                GoFinError::validation(
                    "Use bridge.routes with exact source and destination chain identities.",
                )
            })?;
        if input.source.is_some() == input.solana_source.is_some() {
            return Err(GoFinError::validation(
                "Select exactly one EVM or Solana bridge source.",
            ));
        }
        if input.solana_source.is_some() || input.solana_destination.is_some() {
            return self.relay_solana_routes(&input).await;
        }
        if input.solana_destination.is_some() {
            return self.solana_routes(&input).await;
        }
        let source_input = input.source.as_ref().ok_or_else(internal_error)?;
        let destination_chain = input
            .destination_chain_id
            .ok_or_else(|| GoFinError::validation("Select an exact bridge destination."))?;
        if source_input.chain_id == destination_chain
            || !matches!(destination_chain, 1 | 42161 | 4663 | 8453)
        {
            return Err(GoFinError::validation(
                "Bridge source and destination must be distinct reviewed networks.",
            ));
        }
        let source = self.asset(source_input, source_input.chain_id).await?;
        let destination_asset = if let Some(asset) = &input.destination_asset {
            Some(self.asset(asset, destination_chain).await?)
        } else {
            None
        };
        let request = gofin_bridge::BridgeRouteRequest {
            source: Some(source.clone()),
            solana_source: None,
            destination_chain_id: destination_chain,
            solana_destination: None,
            destination_asset: destination_asset.clone(),
        };
        let mut routes = Vec::new();
        let mut providers = Vec::new();
        for provider in self.providers.values() {
            let provider_id = provider.capabilities().provider;
            providers.push(provider_id.clone());
            let discovered = provider.discover_routes(&request).await;
            let batch = match discovered {
                Ok(batch)
                    if batch.provider == provider_id
                        && batch.candidates.len() <= 100
                        && batch.limitations.len() <= 32 =>
                {
                    batch
                }
                Ok(_) => {
                    routes.push(json!({"provider":provider_id,"availability":"unknown","indicative":true,"destination_asset":null,"error":GoFinError::validation("Provider discovery identity or bounds did not match the configured provider.")}));
                    continue;
                }
                Err(error) => {
                    routes.push(json!({"provider":provider_id,"availability":"unknown","indicative":true,"destination_asset":null,"error":error}));
                    continue;
                }
            };
            if batch.candidates.is_empty() {
                routes.push(json!({"provider":provider_id,"availability":batch.availability,"indicative":true,"destination_asset":null,"observed_at":batch.observed_at,"evidence_source":batch.evidence_source,"complete":batch.complete,"limitations":batch.limitations,"error":batch.error}));
                continue;
            }
            let mut seen = std::collections::BTreeSet::new();
            for candidate in &batch.candidates {
                match candidate.destination.as_ref() {
                    Some(destination) => match self.asset(destination, destination_chain).await {
                    Ok(asset)
                        if candidate
                            .reported_decimals
                            .is_none_or(|decimals| decimals == asset.decimals)
                            && destination_asset
                                .as_ref()
                                .is_none_or(|expected| *expected == asset) =>
                    {
                        if !seen.insert((asset.chain_id, asset.token.clone())) {
                            continue;
                        }
                        routes.push(json!({"provider":provider_id,"availability":batch.availability,"indicative":true,"destination_asset":asset,"identity_verified":true,"observed_at":batch.observed_at,"evidence_source":batch.evidence_source,"complete":batch.complete,"limitations":batch.limitations,"error":batch.error}));
                    }
                    result => {
                        let cause = result.err().unwrap_or_else(|| GoFinError::validation("Provider candidate precision or exact destination differs from independent on-chain identity."));
                        routes.push(json!({"provider":provider_id,"availability":"unavailable","indicative":true,"destination_asset":candidate.destination,"identity_verified":false,"error":cause}));
                    }
                    },
                    None => routes.push(json!({"provider":provider_id,"availability":"unavailable","indicative":true,"destination_asset":null,"identity_verified":false,"error":GoFinError::validation("Provider returned a non-EVM destination for an EVM route.")})),
                }
            }
        }
        Ok(
            json!({"indicative":true,"source":source,"destination_chain_id":input.destination_chain_id,"coverage":{"providers":providers,"market_wide_comparison":false,"complete":false},"routes":routes,"next_action":"Select an exact provider/destination representation, then request bridge.quotes with wallet, amount, recipient and refund scope. Discovery cannot execute or authorize a transfer."}),
        )
    }

    async fn quote(&self, input: &Value) -> gofin_domain::Result<Value> {
        self.quote_with_timeout(input, QUOTE_PREPARATION_TIMEOUT)
            .await
    }

    async fn quote_with_timeout(
        &self,
        input: &Value,
        timeout: std::time::Duration,
    ) -> gofin_domain::Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut phase = QuotePreparationPhase {
            stage: "asset_identity",
            provider: None,
            refresh_transfer_id: input["refreshTransferId"].as_str().map(str::to_owned),
            timeout_ms: timeout.as_millis(),
            started: tokio::time::Instant::now(),
        };
        match tokio::time::timeout_at(deadline, self.prepare_quote(input, deadline, &mut phase))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(phase.timeout_error()),
        }
    }

    async fn prepare_quote(
        &self,
        input: &Value,
        deadline: tokio::time::Instant,
        phase: &mut QuotePreparationPhase,
    ) -> gofin_domain::Result<Value> {
        let mut observations = BTreeMap::new();
        let mut input: BridgeQuoteInput = serde_json::from_value(input.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Bridge quote input is malformed or has unknown fields.",
                "Use the bridge.quotes schema.",
            )
        })?;
        // An omitted provider means automatic discovery. Relay is currently the
        // only implemented Solana-source provider; do not require callers to
        // rediscover that constraint through a failed quote.
        if input.intent.solana_source.is_some() && input.provider.is_none() && input.cctp.is_none()
        {
            input.provider = Some("relay".into());
        }
        // Resolve caller-selected providers before any network observation, so
        // invalid input cannot delay validation or populate timeout attribution.
        if let Some(id) = input
            .provider
            .as_deref()
            .or_else(|| input.cctp.as_ref().map(|_| "cctp"))
        {
            let provider = self.providers.get(id).ok_or_else(|| {
                error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "validation",
                    "The selected bridge provider is unavailable.",
                    "Inspect installed bridge.quotes discovery and capabilities for supported providers.",
                )
            })?;
            if input.cctp.is_some() && id != "cctp" {
                return Err(GoFinError::validation(
                    "CCTP options require the cctp provider.",
                ));
            }
            phase.provider = Some(provider.capabilities().provider);
        }
        if input.intent.source.is_some() == input.intent.solana_source.is_some() {
            return Err(GoFinError::validation(
                "Select exactly one EVM or Solana bridge source.",
            ));
        }
        if input.intent.solana_destination.is_none() && input.intent.destination_assets.is_empty() {
            return Err(error(
                "INVALID_INPUT",
                "validation",
                "Choose at least one exact destination asset.",
                "Name each permitted destination asset by chain and exact token contract.",
            ));
        }
        let source = if let Some(source) = &input.intent.source {
            Some(
                self.quote_asset(source, source.chain_id, &mut observations)
                    .await?,
            )
        } else {
            None
        };
        let solana_source = if let Some(source) = &input.intent.solana_source {
            if input.provider.as_deref() != Some("relay") || input.cctp.is_some() {
                return Err(GoFinError::validation(
                    "Solana source transfers require Relay.",
                ));
            }
            Some(self.relay_solana_asset(source).await?)
        } else {
            None
        };
        if source
            .as_ref()
            .is_some_and(|source| !gofin_networks::supports_chain(source.chain_id))
            || source
                .as_ref()
                .is_some_and(|source| input.intent.destination_chain_id == Some(source.chain_id))
            || input
                .intent
                .destination_chain_id
                .is_some_and(|chain| !gofin_networks::supports_chain(chain))
        {
            return Err(error(
                "BRIDGE_NETWORK_UNSUPPORTED",
                "validation",
                "Bridge source and destination must be distinct reviewed wallet networks.",
                "Choose Ethereum Mainnet, Arbitrum One or Robinhood Chain as distinct source and destination networks.",
            ));
        }
        validate_explicit_bounds(&input.bounds)?;
        let solana_destination = if let Some(destination) = &input.intent.solana_destination {
            if solana_source.is_some()
                || !matches!(input.provider.as_deref(), Some("cctp" | "relay"))
                || input.intent.destination_chain_id.is_some()
                || !input.intent.destination_assets.is_empty()
                || (input.provider.as_deref() == Some("cctp")
                    && !input.intent.allowed_refund_assets.is_empty())
            {
                return Err(GoFinError::validation(
                    "An explicit Solana destination requires an EVM source without an EVM destination; CCTP cannot include EVM refund assets.",
                ));
            }
            Some(if input.provider.as_deref() == Some("cctp") {
                self.resolve_solana_destination(destination, &input.intent.recipient)
                    .await?
            } else {
                self.resolve_relay_solana_destination(destination, &input.intent.recipient)
                    .await?
            })
        } else {
            if input.intent.destination_chain_id.is_none() {
                return Err(GoFinError::validation(
                    "Select an exact destination network.",
                ));
            }
            gofin_evm::address(&input.intent.recipient)?;
            None
        };
        let (address, resource) = if let Some(source) = &source {
            gofin_evm::address(&input.intent.refund_to).map_err(|_| {
                GoFinError::validation("Refund recipient must be one exact EVM address.")
            })?;
            let wallet = self.wallet(&input.intent.wallet, source.chain_id)?;
            let address = wallet.address.ok_or_else(|| {
                GoFinError::validation("Configured EVM wallet has no physical address.")
            })?;
            (address, wallet.resource)
        } else {
            gofin_solana::parse_address(&input.intent.refund_to)?;
            let wallet = self
                .solana_wallets
                .get(&input.intent.wallet)
                .ok_or_else(|| {
                    GoFinError::validation("Select an enrolled Solana source wallet.")
                })?;
            if wallet.signer().is_none() {
                return Err(GoFinError::validation(
                    "Solana source wallet must have a protected signer.",
                ));
            }
            let address = wallet.address().to_owned();
            (
                address.clone(),
                gofin_domain::ResourceId(format!("{}:{}", gofin_solana::NETWORK, address)),
            )
        };
        if input.intent.destination_assets.len() > 8 || input.intent.allowed_refund_assets.len() > 8
        {
            return Err(GoFinError::validation(
                "Bridge destination and refund asset scopes are limited to eight exact identities each.",
            ));
        }
        let mut destination_assets = Vec::new();
        for asset in &input.intent.destination_assets {
            destination_assets.push(
                self.quote_asset(
                    asset,
                    input
                        .intent
                        .destination_chain_id
                        .ok_or_else(internal_error)?,
                    &mut observations,
                )
                .await?,
            );
        }
        let mut allowed_refund_assets = Vec::new();
        for asset in &input.intent.allowed_refund_assets {
            if source
                .as_ref()
                .is_none_or(|source| asset.chain_id != source.chain_id)
                && Some(asset.chain_id) != input.intent.destination_chain_id
            {
                return Err(error(
                    "BRIDGE_REFUND_ASSET_UNSUPPORTED",
                    "validation",
                    "Refund assets may be on the source or destination chain only.",
                    "Select an exact asset on one of the two wallet networks.",
                ));
            }
            allowed_refund_assets.push(
                self.quote_asset(asset, asset.chain_id, &mut observations)
                    .await?,
            );
        }
        let mut allowed_solana_refund_assets = Vec::new();
        if input.intent.allowed_solana_refund_assets.len() > 8 {
            return Err(GoFinError::validation(
                "Limit Solana refund scope to eight exact mints.",
            ));
        }
        for asset in &input.intent.allowed_solana_refund_assets {
            if solana_source.is_none()
                && !(input.provider.as_deref() == Some("relay") && solana_destination.is_some())
            {
                return Err(GoFinError::validation(
                    "Solana refund assets require a Solana Relay source or destination.",
                ));
            }
            allowed_solana_refund_assets.push(self.relay_solana_asset(asset).await?);
        }
        let intent = BridgeIntent {
            wallet: resource.clone(),
            source_address: address.clone(),
            source,
            solana_source,
            destination_chain_id: input.intent.destination_chain_id,
            solana_destination,
            destination_assets,
            recipient: input.intent.recipient,
            refund_to: input.intent.refund_to,
            allowed_refund_assets,
            allowed_solana_refund_assets,
            amount: input.intent.amount,
        };
        let mut quote_bounds = input.bounds.clone();
        quote_bounds
            .input
            .get_or_insert_with(|| gofin_bridge::AmountInterval {
                minimum: intent.amount.clone(),
                maximum: intent.amount.clone(),
            });
        quote_bounds.maximum_source_debit.get_or_insert_with(|| {
            quote_bounds
                .input
                .as_ref()
                .map(|range| range.maximum.clone())
                .unwrap_or_else(|| intent.amount.clone())
        });
        let amount_bounds = quote_bounds
            .input
            .as_ref()
            .expect("normalized quote bounds");
        if intent.amount < amount_bounds.minimum
            || intent.amount > amount_bounds.maximum
            || amount_bounds.maximum
                > *quote_bounds
                    .maximum_source_debit
                    .as_ref()
                    .expect("normalized source debit")
        {
            return Err(error(
                "BRIDGE_BOUNDS_VIOLATED",
                "validation",
                "Requested amount falls outside its exact input or maximum debit bounds.",
                "Choose an amount within the explicit atomic bounds.",
            ));
        }
        let state = self.state()?;
        let existing = if let Some(transfer_id) = input.refresh_transfer_id.as_deref() {
            let transfer = state
                .transfer(transfer_id)
                .map_err(store_error)?
                .ok_or_else(|| {
                    error(
                        "BRIDGE_TRANSFER_UNKNOWN",
                        "validation",
                        "Transfer to refresh is not in the protected journal.",
                        "Use a transfer ID returned by bridge.quotes.",
                    )
                })?;
            let original_quote_bounds: BridgeBounds = serde_json::from_value(
                transfer.metadata["quote_bounds"].clone(),
            )
            .map_err(|_| {
                error(
                    "BRIDGE_TRANSFER_CORRUPT",
                    "reconciliation",
                    "Saved bridge quote bounds are malformed.",
                    "Keep the transfer unresolved and inspect its protected journal.",
                )
            })?;
            let approved_bounds = transfer
                .approved_bounds
                .as_ref()
                .map(|value| serde_json::from_value::<BridgeBounds>(value.clone()))
                .transpose()
                .map_err(|_| internal_error())?;
            if input.bounds == BridgeBounds::default() {
                quote_bounds = approved_bounds
                    .clone()
                    .unwrap_or_else(|| original_quote_bounds.clone());
            } else if let Some(approved) = &approved_bounds {
                if &input.bounds != approved {
                    return Err(error(
                        "BRIDGE_REFRESH_SCOPE_CHANGED",
                        "validation",
                        "Refreshing a granted transfer must use its exact approved bounds.",
                        "Omit bounds to reuse the grant or provide the exact approved bounds.",
                    ));
                }
                quote_bounds = approved.clone();
            }
            let amount_bounds = quote_bounds.input.as_ref().ok_or_else(internal_error)?;
            if intent.amount < amount_bounds.minimum
                || intent.amount > amount_bounds.maximum
                || intent.amount
                    > *quote_bounds
                        .maximum_source_debit
                        .as_ref()
                        .ok_or_else(internal_error)?
            {
                return Err(error(
                    "BRIDGE_REFRESH_AMOUNT_OUT_OF_BOUNDS",
                    "validation",
                    "Refreshed source quantity is outside the persisted input and debit bounds.",
                    "Choose an exact amount inside the original or approved quantity interval.",
                ));
            }
            if !matches!(
                transfer.state,
                BridgeTransferState::Prepared | BridgeTransferState::ApprovalPending
            ) || transfer.steps.iter().any(|step| {
                step.state == BridgeStepState::Submitted
                    || step.state == BridgeStepState::Superseded
            }) {
                return Err(error(
                    "BRIDGE_REFRESH_UNSAFE",
                    "quote",
                    "Only an unsubmitted or approval-only transfer can refresh its quote.",
                    "Reconcile submitted source transactions; do not replace their immutable quote identity.",
                ));
            }
            let persisted_intent: BridgeIntent =
                serde_json::from_value(transfer.metadata["intent"].clone()).map_err(|_| {
                    error(
                        "BRIDGE_TRANSFER_CORRUPT",
                        "reconciliation",
                        "Saved bridge intent is malformed.",
                        "Keep the transfer unresolved and inspect its protected journal.",
                    )
                })?;
            let same_fixed_intent = same_bridge_intent_except_amount(&persisted_intent, &intent);
            let bounds_match = if approved_bounds.is_some() {
                true
            } else {
                original_quote_bounds == quote_bounds
            };
            if !same_fixed_intent || !bounds_match {
                return Err(error(
                    "BRIDGE_REFRESH_SCOPE_CHANGED",
                    "validation",
                    "A refreshed quote must keep the original wallet, recipient, assets, amount and explicit bounds.",
                    "Create a new transfer when the reviewed intent changes.",
                ));
            }
            let previous = self.saved_quote(&transfer)?;
            let mut refresh_intent = intent.clone();
            refresh_intent.destination_assets = previous.quote.intent.destination_assets.clone();
            Some((transfer, previous, refresh_intent))
        } else {
            None
        };
        phase.stage = "source_pricing";
        self.validate_usd_amount_bounds(
            &quote_bounds,
            &intent.source_amount(intent.amount.clone())?,
            &[],
        )
        .await?;
        let request_intent = existing
            .as_ref()
            .map(|(_, _, intent)| intent.clone())
            .unwrap_or_else(|| intent.clone());
        let request =
            BridgeQuoteRequest {
                cctp: input.cctp.clone().or_else(|| {
                    existing.as_ref().and_then(|(_, previous, _)| {
                        previous.quote.cctp.as_ref().map(|metadata| {
                            gofin_bridge::cctp::CctpOptions {
                                transfer_mode: metadata.transfer_mode,
                            }
                        })
                    })
                }),
                intent: request_intent.clone(),
                bounds: quote_bounds.clone(),
            };
        let selected_providers = if let Some((_, previous, _)) = &existing {
            if input
                .provider
                .as_ref()
                .is_some_and(|id| id != &previous.quote.provider)
            {
                return Err(GoFinError::validation(
                    "A refresh cannot change its bridge provider.",
                ));
            }
            if input.cctp.as_ref().is_some_and(|options| {
                previous
                    .quote
                    .cctp
                    .as_ref()
                    .is_none_or(|metadata| metadata.transfer_mode != options.transfer_mode)
            }) {
                return Err(GoFinError::validation(
                    "A refresh cannot change its approved CCTP mode.",
                ));
            }
            vec![self.provider(&previous.quote.provider)?]
        } else if let Some(id) = input.provider.as_deref() {
            if input.cctp.is_some() && id != "cctp" {
                return Err(GoFinError::validation(
                    "CCTP options require the cctp provider.",
                ));
            }
            vec![self.provider(id)?]
        } else if input.cctp.is_some() {
            vec![self.provider("cctp")?]
        } else {
            self.providers.values().cloned().collect::<Vec<_>>()
        };
        let compared_providers = selected_providers.len();
        let selected_provider_ids = selected_providers
            .iter()
            .map(|provider| provider.capabilities().provider)
            .collect::<Vec<_>>();
        let mut batch = gofin_bridge::BridgeQuoteBatch {
            quotes: Vec::new(),
            failures: Vec::new(),
        };
        for provider in selected_providers {
            let provider_id = provider.capabilities().provider;
            phase.stage = "provider_quote";
            phase.provider = Some(provider_id.clone());
            match provider.quote(&request).await {
                Ok(mut provider_batch) => {
                    for quote in &provider_batch.quotes {
                        self.check_quote_freshness(quote)?;
                        if quote.provider != provider_id {
                            return Err(error(
                                "BRIDGE_PROVIDER_IDENTITY_MISMATCH",
                                "quote",
                                "Provider returned a quote under a different provider identity.",
                                "Discard the quote and inspect the configured provider registry.",
                            ));
                        }
                    }
                    for failure in &mut provider_batch.failures {
                        if failure.provider.is_some()
                            && failure.provider.as_deref() != Some(provider_id.as_str())
                        {
                            return Err(error(
                                "BRIDGE_PROVIDER_IDENTITY_MISMATCH",
                                "quote",
                                "Provider attributed a quote failure to a different provider.",
                                "Discard the failure and inspect the configured provider registry.",
                            ));
                        }
                        failure.provider = Some(provider_id.clone());
                    }
                    batch.quotes.extend(provider_batch.quotes);
                    batch.failures.extend(provider_batch.failures);
                }
                Err(provider_error) => {
                    if request_intent.solana_destination.is_some() {
                        return Err(provider_error);
                    }
                    for destination_asset in &request_intent.destination_assets {
                        batch.failures.push(gofin_bridge::BridgeQuoteFailure {
                            provider: Some(provider_id.clone()),
                            destination_asset: Some(destination_asset.clone()),
                            solana_destination_asset: None,
                            error: provider_error.clone(),
                        });
                    }
                }
            }
        }
        if batch.quotes.is_empty() {
            return Ok(
                json!({"coverage":{"providers":selected_provider_ids,"compared_providers":compared_providers,"market_wide_comparison":false,"complete":false},"transfers":[],"failures":batch.failures}),
            );
        }
        if existing.is_some() && batch.quotes.len() != 1 {
            return Err(error(
                "BRIDGE_REFRESH_AMBIGUOUS",
                "quote",
                "A saved transfer refresh must produce one quote for its original destination asset.",
                "Use a new transfer to compare multiple exact destination assets.",
            ));
        }
        let mut transfers = Vec::new();
        let mut pending_records = Vec::new();
        let mut accepted_quotes = Vec::new();
        for mut quote in batch.quotes {
            phase.provider = Some(quote.provider.clone());
            self.check_quote_freshness(&quote)?;
            let mut expected_intent = request_intent.clone();
            if quote.intent.solana_destination.is_none()
                && (quote.intent.destination_assets.len() != 1
                    || !request_intent
                        .destination_assets
                        .contains(&quote.intent.destination_assets[0]))
            {
                return Err(error(
                    "BRIDGE_QUOTE_IDENTITY_MISMATCH",
                    "quote",
                    "Relay quote selected an unrequested or ambiguous destination asset.",
                    "Discard this quote and retry with exact supported assets.",
                ));
            }
            expected_intent.destination_assets = quote.intent.destination_assets.clone();
            let refund_scope_is_subset = quote
                .intent
                .allowed_refund_assets
                .iter()
                .all(|asset| request_intent.allowed_refund_assets.contains(asset))
                && {
                    let mut seen = std::collections::BTreeSet::new();
                    quote.intent.allowed_refund_assets.iter().all(|asset| {
                        seen.insert((asset.chain_id, asset.token.clone(), asset.decimals))
                    })
                };
            expected_intent.allowed_refund_assets = quote.intent.allowed_refund_assets.clone();
            if quote.intent != expected_intent || !refund_scope_is_subset {
                return Err(error(
                    "BRIDGE_QUOTE_IDENTITY_MISMATCH",
                    "quote",
                    "Relay quote differs from the broker-resolved physical intent.",
                    "Discard this quote and retry after reviewing the selected wallet and route.",
                ));
            }
            if existing.is_some()
                && existing
                    .as_ref()
                    .is_some_and(|(_, prior, _)| prior.quote.provider != quote.provider)
            {
                return Err(error(
                    "BRIDGE_REFRESH_PROVIDER_CHANGED",
                    "quote",
                    "A refreshed quote cannot change its original bridge provider.",
                    "Continue with the saved provider or create a new transfer and review it independently.",
                ));
            }
            if quote.approval.is_none()
                && let Some((_, previous, _)) = &existing
            {
                // Intent/provider identity is checked above. An allowance-aware
                // refresh may contain only a deposit; retain its prior finite
                // allowance identity. Deposit preparation rechecks allowance.
                quote.approval = previous.quote.approval.clone();
            }
            let provider = self.provider(&quote.provider)?;
            phase.stage = "fee_pricing";
            let priced_fees = self.validate_quote_prices(&quote_bounds, &quote).await?;
            let plan = provider.prepare(&quote, &quote_bounds, &address)?;
            phase.stage = "allowance";
            let plan = self
                .allowance_aware_plan(
                    &quote,
                    plan,
                    &address,
                    &quote_bounds,
                    existing.as_ref().map(|(_, previous, _)| previous),
                )
                .await?;
            if quote.intent.solana_source.is_some() {
                self.prevalidate_relay_solana_quote(&quote, &quote_bounds, &input.intent.wallet)
                    .await?;
            }
            self.check_quote_freshness(&quote)?;
            let id = existing
                .as_ref()
                .map(|(record, _, _)| record.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut step_ids = BTreeMap::new();
            if let Some((record, previous, _)) = &existing {
                if quote.route_id != previous.quote.route_id
                    || plan.steps.len() != previous.plan.steps.len()
                {
                    return Err(error(
                        "BRIDGE_REFRESH_SCOPE_CHANGED",
                        "quote",
                        "Refreshed quote changed the reviewed route or finite step set.",
                        "Create a new transfer and review its authorization scope.",
                    ));
                }
                for (next_step, old_step) in plan.steps.iter().zip(&previous.plan.steps) {
                    if next_step.ordinal != old_step.ordinal
                        || domain_step_kind(&next_step.kind) != domain_step_kind(&old_step.kind)
                        || next_step.dependencies != old_step.dependencies
                    {
                        return Err(error(
                            "BRIDGE_REFRESH_SCOPE_CHANGED",
                            "quote",
                            "Refreshed quote changed the finite approval or deposit step structure.",
                            "Create a new transfer and review its authorization scope.",
                        ));
                    }
                    let old_id = self.step_id_for(record, old_step.ordinal).ok_or_else(|| {
                        error(
                            "BRIDGE_TRANSFER_CORRUPT",
                            "reconciliation",
                            "Saved bridge step identity is missing.",
                            "Keep the transfer unresolved and inspect its protected journal.",
                        )
                    })?;
                    step_ids.insert(next_step.ordinal, old_id);
                }
            } else {
                for step in &plan.steps {
                    step_ids.insert(step.ordinal, uuid::Uuid::new_v4().to_string());
                }
            }
            let links = plan
                .steps
                .iter()
                .map(|step| {
                    let kind = domain_step_kind(&step.kind);
                    let depends_on = step
                        .dependencies
                        .iter()
                        .filter_map(|ordinal| step_ids.get(ordinal).cloned())
                        .collect();
                    let previous = existing.as_ref().and_then(|(record, _, _)| {
                        record
                            .steps
                            .iter()
                            .find(|link| link.step_id == step_ids[&step.ordinal])
                    });
                    BridgeStepLink {
                        transfer_id: id.clone(),
                        grant_id: previous.and_then(|link| link.grant_id.clone()),
                        plan_authority: previous.and_then(|link| link.plan_authority.clone()),
                        step_id: step_ids[&step.ordinal].clone(),
                        operation_id: previous.and_then(|link| link.operation_id.clone()),
                        kind,
                        depends_on,
                        state: previous
                            .map(|link| link.state.clone())
                            .unwrap_or(BridgeStepState::Unsubmitted),
                    }
                })
                .collect::<Vec<_>>();
            let payload =
                json!({"quote":quote,"bounds":quote_bounds,"plan":plan,"step_ids":step_ids});
            let quote_hash = binding_hash(&payload)?;
            let revision = BridgeQuoteRevision {
                revision: existing
                    .as_ref()
                    .map(|(record, _, _)| record.quote_revisions.len() as u32 + 1)
                    .unwrap_or(1),
                provider: quote.provider.clone(),
                route_id: quote.route_id.clone(),
                order_id: quote.protocol_order_id.clone(),
                quote_hash,
                payload,
                observed_at: quote.observed_at.clone(),
                valid_until: quote.valid_until.clone(),
            };
            let settlement_expectation =
                settlement_expectation(&quote, quote.minimum_receipt.atoms.clone());
            let deposit_step_id = links
                .iter()
                .filter(|link| link.kind == BridgeStepKind::Deposit)
                .map(|link| link.step_id.clone())
                .collect::<Vec<_>>();
            if deposit_step_id.len() != 1 {
                return Err(error(
                    "BRIDGE_PLAN_INVALID",
                    "quote",
                    "The finite bridge plan must declare exactly one source deposit step.",
                    "Discard this quote and request a supported route.",
                ));
            }
            let quote_maximum = quote_bounds
                .maximum_source_debit
                .clone()
                .unwrap_or_else(|| quote.gross_debit.atoms.clone());
            let prospective_debit = BridgeProspectiveDebit {
                deposit_step_id: deposit_step_id[0].clone(),
                resource: quote.intent.wallet.clone(),
                maximum: GoFinAmount {
                    asset: quote.gross_debit.asset.clone(),
                    atoms: quote_maximum,
                },
            };
            let mut record = if let Some((record, _, _)) = &existing {
                record.clone()
            } else {
                BridgeTransferRecord {
                    version: 1,
                    id: id.clone(),
                    grant_id: None,
                    plan_scope: None,
                    metadata: json!({"wallet_alias":input.intent.wallet,"wallet_resource":resource,"intent":intent,"quote_bounds":quote_bounds}),
                    settlement_expectation: Some(settlement_expectation.clone()),
                    approved_bounds: None,
                    prospective_debit: Some(prospective_debit.clone()),
                    state: BridgeTransferState::Prepared,
                    quote_revisions: vec![],
                    steps: vec![],
                    allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
                    transit: None,
                    applied_proofs: Vec::new(),
                    settlement_proofs: Vec::new(),
                }
            };
            if existing.is_some() {
                record.version = record.version.checked_add(1).ok_or_else(internal_error)?;
            }
            if record.grant_id.is_none() && record.approved_bounds.is_none() {
                record.prospective_debit = Some(prospective_debit);
            }
            record.quote_revisions.push(revision);
            record.steps = links.clone();
            if existing.is_none() || record.approved_bounds.is_none() {
                record.settlement_expectation = Some(settlement_expectation);
            }
            if record
                .steps
                .iter()
                .any(|step| step.state == BridgeStepState::Settled)
            {
                record.state = BridgeTransferState::ApprovalPending;
            }
            pending_records.push(record);
            accepted_quotes.push(quote.clone());
            let settlement_policy = self.settlement_policy(
                bridge_source_chain_id(&quote.intent),
                quote.intent.destination_chain_id,
            );
            transfers.push(json!({"transfer_id":id,"quote":quote,"priced_fees":priced_fees,"steps":links,"settlement_policy":settlement_policy,"next_action":"Choose this quote, then preview bridge.transfer for one listed step."}));
        }
        // No journal mutation precedes this point. All cancellable network work
        // must finish before committing any of the compared quote options.
        phase.stage = "quote_persistence";
        for quote in &accepted_quotes {
            self.check_quote_freshness(quote)?;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(phase.timeout_error());
        }
        for record in pending_records {
            if existing.is_some() {
                state.save_transfer(&record).map_err(store_error)?;
            } else {
                state.create_transfer(&record).map_err(store_error)?;
            }
        }
        Ok(json!({
            "coverage": {
                "providers": selected_provider_ids,
                "compared_providers": compared_providers,
                "market_wide_comparison": false,
                "complete": batch.failures.is_empty()
            },
            "transfers": transfers,
            "failures": batch.failures
        }))
    }

    async fn prepare_bridge_execute_permission(
        &self,
        input: &Value,
    ) -> gofin_domain::Result<gofin_domain::financial_plans::PlanPermission> {
        let input: gofin_bridge::BridgeExecuteIntentInput =
            serde_json::from_value(input.clone()).map_err(|_| {
                error(
                    "INVALID_INPUT",
                    "authorization",
                    "Provider-neutral bridge intent is malformed.",
                    "Use the bridge.execute intent schema with exact identities, an amount interval, providers and bounds.",
                )
            })?;
        let requested = &input.intent;
        let destination_chain_id = requested.destination_chain_id;
        if requested.destination_assets.is_empty()
            || requested.destination_assets.len() > 8
            || requested.allowed_refund_assets.len() > 8
            || input.allowed_providers.is_empty()
            || input.allowed_providers.len() > self.providers.len()
        {
            return Err(error(
                "BRIDGE_INTENT_SCOPE_INVALID",
                "authorization",
                "Bridge intent needs exact destination assets, a finite provider set and bounded refund assets.",
                "Choose one or more exact destination assets, configured providers and at most eight refund assets.",
            ));
        }
        validate_explicit_bounds(&input.bounds)?;
        if requested.amount.minimum.is_zero()
            || requested.amount.minimum > requested.amount.maximum
            || input.bounds.input.as_ref() != Some(&requested.amount)
            || input
                .bounds
                .maximum_source_debit
                .as_ref()
                .is_none_or(|maximum| maximum < &requested.amount.maximum)
        {
            return Err(error(
                "BRIDGE_BOUNDS_VIOLATED",
                "authorization",
                "Bridge input and source-debit bounds must explicitly contain the approved amount interval.",
                "Set bounds.input to the same interval and maximum_source_debit at or above its maximum.",
            ));
        }
        let source = self
            .asset(&requested.source, requested.source.chain_id)
            .await?;
        if !gofin_networks::supports_chain(source.chain_id)
            || requested.destination_chain_id == source.chain_id
            || !gofin_networks::supports_chain(requested.destination_chain_id)
        {
            return Err(error(
                "BRIDGE_NETWORK_UNSUPPORTED",
                "authorization",
                "Bridge source and destination must be distinct reviewed wallet networks.",
                "Choose two supported EVM wallet networks.",
            ));
        }
        gofin_evm::address(&requested.recipient).map_err(|_| {
            error(
                "BRIDGE_RECIPIENT_INVALID",
                "authorization",
                "Destination recipient must be one exact EVM address.",
                "Provide the full destination address.",
            )
        })?;
        gofin_evm::address(&requested.refund_to).map_err(|_| {
            error(
                "BRIDGE_REFUND_RECIPIENT_INVALID",
                "authorization",
                "Refund recipient must be one exact EVM address.",
                "Provide the full refund address.",
            )
        })?;
        let wallet = self.wallet(&requested.wallet, source.chain_id)?;
        let source_address = wallet.address.as_deref().ok_or_else(|| {
            GoFinError::validation("Configured EVM wallet has no physical address.")
        })?;
        let mut resolved_destinations = Vec::new();
        for asset in &requested.destination_assets {
            resolved_destinations.push(self.asset(asset, requested.destination_chain_id).await?);
        }
        let mut resolved_refunds = Vec::new();
        for asset in &requested.allowed_refund_assets {
            if asset.chain_id != source.chain_id && asset.chain_id != requested.destination_chain_id
            {
                return Err(error(
                    "BRIDGE_REFUND_ASSET_UNSUPPORTED",
                    "authorization",
                    "Refund assets may be on the source or destination chain only.",
                    "Select an exact asset on one of the two bridge networks.",
                ));
            }
            resolved_refunds.push(self.asset(asset, asset.chain_id).await?);
        }
        let intent = BridgeIntent {
            wallet: wallet.resource.clone(),
            source_address: source_address.into(),
            source: Some(source.clone()),
            solana_source: None,
            destination_chain_id: Some(destination_chain_id),
            solana_destination: None,
            destination_assets: resolved_destinations,
            recipient: requested.recipient.to_ascii_lowercase(),
            refund_to: requested.refund_to.to_ascii_lowercase(),
            allowed_refund_assets: resolved_refunds,
            allowed_solana_refund_assets: vec![],
            amount: requested.amount.minimum.clone(),
        };
        for provider_id in &input.allowed_providers {
            let provider = self.provider(provider_id)?;
            let capabilities = provider.capabilities();
            if !capabilities.chains.contains(&source.chain_id)
                || !capabilities.chains.contains(&destination_chain_id)
            {
                return Err(error(
                    "BRIDGE_PROVIDER_OUT_OF_SCOPE",
                    "authorization",
                    "An approved provider does not support both exact bridge networks.",
                    "Remove that provider or select networks it advertises.",
                ));
            }
        }
        if !intent.destination_assets.iter().any(|asset| {
            input
                .bounds
                .minimum_net_receipt
                .as_ref()
                .is_some_and(|floor| floor.asset == asset.asset_id())
        }) {
            return Err(error(
                "BRIDGE_RECEIPT_ASSET_OUT_OF_SCOPE",
                "authorization",
                "The minimum receipt asset must be one of the exact approved destination assets.",
                "Set minimum_net_receipt to an approved destination asset identity.",
            ));
        }
        // Most routes submit wallet transactions only on the source chain.
        // Base withdrawals from Base to Ethereum also expand into destination
        // proof/finalize writes, so their gas cap must be approved up front.
        let mut stage_chains = std::collections::BTreeSet::from([source.chain_id]);
        if input.allowed_providers.contains("base_canonical")
            && source.chain_id == 8453
            && destination_chain_id == 1
        {
            stage_chains.insert(destination_chain_id);
        }
        for chain_id in &stage_chains {
            maximum_gas_cap(&input.bounds, *chain_id)?;
        }
        let normalized_scope = json!({
            "wallet":intent.wallet,
            "source_address":intent.source_address.to_ascii_lowercase(),
            "source":source,
            "destination_chain_id":destination_chain_id,
            "destination_assets":intent.destination_assets,
            "recipient":intent.recipient,
            "refund_to":intent.refund_to,
            "allowed_refund_assets":intent.allowed_refund_assets,
            "amount":requested.amount,
            "allowed_providers":input.allowed_providers,
        });
        let stage_resource = |chain_id| bridge_stage_resource(&intent, chain_id);
        let mut maximum_costs = Vec::new();
        for chain_id in &stage_chains {
            push_plan_cost_cap(
                &mut maximum_costs,
                &stage_resource(*chain_id),
                &gofin_domain::AssetId {
                    network: format!("ethereum:{chain_id}"),
                    reference: "native".into(),
                    decimals: 18,
                },
                &maximum_gas_cap(&input.bounds, *chain_id)?,
            )?;
        }
        for cap in &input.bounds.fee_caps {
            if cap.kind == gofin_bridge::FeeKind::AdditionalDebit {
                let chain_id = cap
                    .asset
                    .network
                    .strip_prefix("ethereum:")
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        GoFinError::validation(
                            "Bridge fee asset must identify a reviewed EVM network.",
                        )
                    })?;
                let within_bridge_networks =
                    chain_id == source.chain_id || chain_id == destination_chain_id;
                let approved_asset = cap.asset == source.asset_id()
                    || intent
                        .destination_assets
                        .iter()
                        .chain(&intent.allowed_refund_assets)
                        .any(|asset| cap.asset == asset.asset_id())
                    || (cap.asset.reference == "native"
                        && cap.asset.decimals == 18
                        && within_bridge_networks);
                if !within_bridge_networks || !approved_asset {
                    return Err(error(
                        "BRIDGE_FEE_ASSET_OUT_OF_SCOPE",
                        "authorization",
                        "An additional-debit fee cap must use an exact bridge asset or native asset on one of its networks.",
                        "Use a source, destination or permitted refund asset identity.",
                    ));
                }
                push_plan_cost_cap(
                    &mut maximum_costs,
                    &stage_resource(chain_id),
                    &cap.asset,
                    &cap.maximum,
                )?;
            } else if cap.kind == gofin_bridge::FeeKind::WithheldFromOutput {
                let source_denominated = cap.asset == source.asset_id();
                let destination_denominated = intent
                    .destination_assets
                    .iter()
                    .any(|asset| asset.asset_id() == cap.asset);
                let location = if source_denominated {
                    AccountingLocation::Cash {
                        resource: intent.wallet.clone(),
                    }
                } else if destination_denominated {
                    AccountingLocation::Cash {
                        resource: gofin_domain::ResourceId(format!(
                            "ethereum:{}:{}",
                            destination_chain_id,
                            intent.recipient.to_ascii_lowercase()
                        )),
                    }
                } else {
                    return Err(error(
                        "BRIDGE_FEE_ASSET_OUT_OF_SCOPE",
                        "authorization",
                        "A withheld-output fee cap must use the exact source asset or one of the approved destination assets.",
                        "Select the exact source asset or a destination asset from the reviewed bridge intent.",
                    ));
                };
                maximum_costs.push(CostBound {
                    location,
                    amount: GoFinAmount {
                        asset: cap.asset.clone(),
                        atoms: cap.maximum.clone(),
                    },
                    treatment: CostTreatment::IncludedInDebit,
                });
            } else if cap.kind == gofin_bridge::FeeKind::Gas {
                let valid_gas_asset = cap.asset.reference == "native"
                    && cap.asset.decimals == 18
                    && stage_chains.contains(
                        &cap.asset
                            .network
                            .strip_prefix("ethereum:")
                            .and_then(|chain| chain.parse::<u64>().ok())
                            .unwrap_or_default(),
                    );
                if !valid_gas_asset {
                    return Err(error(
                        "BRIDGE_FEE_ASSET_OUT_OF_SCOPE",
                        "authorization",
                        "A gas cap must name native ETH on a chain where the approved provider can submit a step.",
                        "Use the source network, or add the supported Base withdrawal destination network when required.",
                    ));
                }
            }
        }
        let maximum_debits = vec![ResourceAmount {
            resource: intent.wallet.clone(),
            amount: GoFinAmount {
                asset: source.asset_id(),
                atoms: input
                    .bounds
                    .maximum_source_debit
                    .clone()
                    .ok_or_else(internal_error)?,
            },
        }];
        let maximum_costs_usd = maximum_costs.clone();
        let mut possible_credits = std::collections::BTreeSet::new();
        for target in &intent.destination_assets {
            possible_credits.insert(
                gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                    resource: gofin_domain::ResourceId(format!(
                        "ethereum:{}:{}",
                        target.chain_id,
                        intent.recipient.to_ascii_lowercase()
                    )),
                    asset: target.asset_id(),
                },
            );
        }
        for target in &intent.allowed_refund_assets {
            possible_credits.insert(
                gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                    resource: if target.chain_id == source.chain_id {
                        intent.wallet.clone()
                    } else {
                        gofin_domain::ResourceId(format!(
                            "ethereum:{}:{}",
                            target.chain_id,
                            intent.refund_to.to_ascii_lowercase()
                        ))
                    },
                    asset: target.asset_id(),
                },
            );
        }
        let scope_id = format!(
            "bridge-intent:{}",
            binding_hash(&json!({"scope":normalized_scope,"bounds":input.bounds}))?
        );
        Ok(gofin_domain::financial_plans::PlanPermission {
            recovery_parent: None,
            spec: gofin_domain::boundaries::BoundarySpec {
                fixed: json!({"bridge_execute_scope":normalized_scope}),
                parameters: json!({"bounds":input.bounds}),
                maximum_debits: maximum_debits.clone(),
                maximum_costs: maximum_costs.clone(),
                summary: format!(
                    "Execute a bridge from Ethereum network {} to network {} within the approved amount and fee bounds.",
                    source.chain_id, destination_chain_id
                ),
            },
            possible_credits: possible_credits.into_iter().collect(),
            fee_assets: maximum_costs_usd
                .iter()
                .map(|cost| cost.amount.asset.clone())
                .collect(),
            deferred_receipt: None,
            authorization_seconds: input.bounds.authorization_seconds.map(u64::from),
            cumulative_scope: Some(
                gofin_domain::financial_plans::FinancialPlanCumulativeScope {
                    maximum_obligations: Vec::new(),
                    scope_id,
                    maximum_debits,
                    maximum_costs: maximum_costs_usd,
                },
            ),
        })
    }

    async fn status(&self, input: &Value) -> gofin_domain::Result<Value> {
        let request: BridgeStatusInput = serde_json::from_value(input.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Bridge status input is malformed.",
                "Use the bridge.status schema.",
            )
        })?;
        let mut transfer = self
            .state()?
            .transfer(&request.transfer_id)
            .map_err(store_error)?
            .ok_or_else(|| {
                error(
                    "BRIDGE_TRANSFER_UNKNOWN",
                    "validation",
                    "Bridge transfer identity is not present in the protected journal.",
                    "Use a transfer ID returned by bridge.quotes.",
                )
            })?;
        let engine = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(internal_error)?;
        {
            for step in &transfer.steps {
                if let Some(operation_id) = &step.operation_id {
                    let _ = engine.status(operation_id, true).await;
                }
            }
            transfer = self
                .state()?
                .transfer(&request.transfer_id)
                .map_err(store_error)?
                .ok_or_else(internal_error)?;
        }
        let provider_status = if matches!(
            transfer.state,
            BridgeTransferState::InTransit
                | BridgeTransferState::DestinationPending
                | BridgeTransferState::RefundPending
        ) || (transfer.state == BridgeTransferState::NeedsAttention
            && transfer.transit.is_some())
        {
            let diagnostic = self.reconcile_external(&engine, &transfer).await?;
            transfer = self
                .state()?
                .transfer(&request.transfer_id)
                .map_err(store_error)?
                .ok_or_else(internal_error)?;
            Some(diagnostic)
        } else {
            None
        };
        let next_action = if transfer.state == BridgeTransferState::ApprovalPending
            && let Some(step) = ready_source_child(&transfer)
        {
            format!(
                "The saved {} child {} is ready for preview: its prerequisites are settled. Continue with the existing transfer and reviewed bounds.",
                step_kind_name(&step.kind),
                step.step_id,
            )
        } else {
            match transfer.state {
            BridgeTransferState::Prepared => "Preview one declared bridge.transfer step.",
            BridgeTransferState::ApprovalPending => {
                "Reconcile the original approval step, then preview its dependent step."
            }
            BridgeTransferState::SourcePending => {
                "Reconcile the original source transaction; do not submit another deposit."
            }
            BridgeTransferState::InTransit
            | BridgeTransferState::DestinationPending
            | BridgeTransferState::RefundPending => {
                "Retry bridge.status until an exact destination receipt or refund satisfies the configured confirmation policy."
            }
            BridgeTransferState::Completed => {
                "Transfer completed with a verified destination credit."
            }
            BridgeTransferState::Refunded => "Transfer refunded with a verified refund credit.",
            BridgeTransferState::Failed => {
                "Review the terminal source-step failure before starting a deliberate new transfer."
            }
            BridgeTransferState::NeedsAttention => {
                "Keep the transfer unresolved and inspect its original quote and operation records."
            }
            }.to_owned()
        };
        let next_action = provider_status
            .as_ref()
            .and_then(|status| status["next_action"].as_str())
            .unwrap_or(&next_action);
        let saved = self.saved_quote(&transfer)?;
        let mut settlement_accounting = None;
        if transfer.state == BridgeTransferState::Completed
            && let Some(proof) =
                transfer.settlement_proofs.iter().rev().find(|proof| {
                    proof.outcome == gofin_domain::BridgeSettlementOutcome::Destination
                })
            && !proof.effects.iter().any(|effect| {
                matches!(
                    effect,
                    FinancialEffect::Cost {
                        category,
                        treatment: CostTreatment::IncludedInDebit,
                        ..
                    } if category == "withheld_from_output"
                )
            })
            && let Some((source_principal, executed_quote)) =
                bridge_execute_source_evidence(&transfer, &engine.operations()?)
            && let Some((destination_asset, destination_atoms)) =
                proof.effects.iter().find_map(|effect| match effect {
                    FinancialEffect::UsableCredit { amount, .. } => {
                        Some((amount.asset.clone(), amount.atoms.clone()))
                    }
                    _ => None,
                })
            && let Some(candidate) = relay_native_withheld_cost(
                &transfer,
                &executed_quote,
                &self.provider(&executed_quote.provider)?.capabilities(),
                &source_principal,
                &destination_asset,
                &destination_atoms,
            )?
            && let Some(applied) = engine.commit_bridge_external_settlement_cost_accounting(
                &transfer.id,
                &proof.proof_digest,
                candidate,
            )?
        {
            settlement_accounting = Some(json!({
                "proof_digest": proof.proof_digest,
                "cost": applied,
            }));
        }
        let settlement_policy = self.settlement_policy(
            bridge_source_chain_id(&saved.quote.intent),
            saved.quote.intent.destination_chain_id,
        );
        Ok(
            json!({"transfer_id":transfer.id,"state":transfer.state,"grant_id":transfer.grant_id,"steps":transfer.steps,"transit":transfer.transit,"provider_status":provider_status,"settlement":bridge_settlement_status_summary(&transfer),"settlement_accounting":settlement_accounting,"settlement_policy":settlement_policy,"next_action":next_action}),
        )
    }

    async fn reconcile_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
    ) -> gofin_domain::Result<Value> {
        let saved = self.saved_quote(transfer)?;
        if saved.quote.provider == "cctp" {
            return self.reconcile_cctp_external(engine, transfer, &saved).await;
        }
        if saved.quote.provider == "base_canonical" {
            return self.reconcile_base_external(engine, transfer, &saved).await;
        }
        if saved.quote.provider == "arbitrum_canonical" {
            return self
                .reconcile_canonical_external(engine, transfer, &saved)
                .await;
        }
        if saved.quote.intent.solana_destination.is_some() {
            let solana = self
                .reconcile_relay_solana_external(engine, transfer, &saved)
                .await?;
            if solana["settled"] == true {
                return Ok(solana);
            }
            // A Relay EVM-source order can refund on its EVM source chain even
            // while its Solana destination remains pending. Continue through
            // the reviewed EVM refund proof below.
        }
        if saved.quote.intent.solana_source.is_some()
            && let Some(status) = self
                .try_reconcile_relay_solana_refund(engine, transfer, &saved)
                .await?
        {
            return Ok(status);
        }
        let (Some(order_id), Some(provider_request_id)) = (
            &saved.quote.protocol_order_id,
            &saved.quote.provider_request_id,
        ) else {
            return Err(error(
                "BRIDGE_ORDER_ID_MISSING",
                "reconciliation",
                "The immutable provider request or protocol order identity is missing.",
                "Keep the bridge transfer unresolved; never infer a match from a different quote.",
            ));
        };
        let provider = self.provider(&saved.quote.provider)?;
        let reconciliation = provider
            .reconcile(&BridgeTransferRef {
                order_id: Some(order_id.clone()),
                provider_request_id: Some(provider_request_id.clone()),
                source_chain_id: bridge_source_chain_id(&saved.quote.intent),
                destination_chain_id: saved.quote.intent.destination_chain_id,
                destination_network: saved
                    .quote
                    .intent
                    .solana_destination
                    .as_ref()
                    .map(|destination| destination.network.clone()),
                destination_locator: transfer
                    .transit
                    .as_ref()
                    .and_then(|transit| transit.destination_locator.clone()),
            })
            .await?;
        let refund_chains = saved
            .quote
            .refund_paths
            .iter()
            .filter(|path| {
                path.recipient
                    .eq_ignore_ascii_case(&saved.quote.intent.refund_to)
            })
            .map(|path| path.chain_id)
            .collect::<std::collections::BTreeSet<_>>();
        let candidate_transactions = bridge_candidate_locators(
            &reconciliation.candidates,
            order_id,
            saved.quote.intent.destination_chain_id.unwrap_or_default(),
            &refund_chains,
        );
        let mut matches = Vec::new();
        let mut candidate_observations = Vec::new();
        for candidate in reconciliation.candidates.iter().cloned() {
            if candidate.order_id.as_deref() != Some(order_id.as_str())
                || candidate.kind == EvidenceKind::SourceDeposit
            {
                continue;
            }
            let mut targets = Vec::new();
            if candidate.kind == EvidenceKind::DestinationFill
                && Some(candidate.chain_id) == saved.quote.intent.destination_chain_id
            {
                let expected_resource = gofin_domain::ResourceId(format!(
                    "ethereum:{}:{}",
                    saved
                        .quote
                        .intent
                        .destination_chain_id
                        .ok_or_else(internal_error)?,
                    saved.quote.intent.recipient.to_ascii_lowercase()
                ));
                let Some(minimum) = transfer.settlement_expectation.as_ref().and_then(|scope| {
                    scope
                        .destination
                        .iter()
                        .find(|target| {
                            target.resource == expected_resource
                                && target.asset == saved.quote.expected_receipt.asset
                        })
                        .map(|target| target.minimum_atoms.clone())
                }) else {
                    continue;
                };
                targets.push((
                    candidate.chain_id,
                    saved.quote.expected_receipt.asset.clone(),
                    saved.quote.intent.recipient.clone(),
                    minimum,
                    gofin_domain::BridgeSettlementOutcome::Destination,
                ));
            } else if candidate.kind == EvidenceKind::Refund {
                for path in saved.quote.refund_paths.iter().filter(|path| {
                    path.network.is_none()
                        && path
                            .recipient
                            .eq_ignore_ascii_case(&saved.quote.intent.refund_to)
                }) {
                    let expected_resource = gofin_domain::ResourceId(format!(
                        "ethereum:{}:{}",
                        path.chain_id,
                        path.recipient.to_ascii_lowercase()
                    ));
                    if let Some(minimum) =
                        transfer.settlement_expectation.as_ref().and_then(|scope| {
                            scope
                                .refund
                                .iter()
                                .find(|target| {
                                    target.asset == path.asset
                                        && target.resource == expected_resource
                                })
                                .map(|target| target.minimum_atoms.clone())
                        })
                    {
                        targets.push((
                            path.chain_id,
                            path.asset.clone(),
                            path.recipient.clone(),
                            minimum,
                            gofin_domain::BridgeSettlementOutcome::Refund,
                        ));
                    }
                }
            }
            for (chain_id, asset, recipient, minimum, outcome) in targets {
                let Some(locator) =
                    bridge_candidate_observation(&candidate, "pending_inclusion_or_finality")
                else {
                    continue;
                };
                let Some(runtime) = self.observers.get(&chain_id) else {
                    candidate_observations.push(
                        bridge_candidate_observation(&candidate, "observer_unavailable")
                            .unwrap_or(locator),
                    );
                    continue;
                };
                let (confirmations, require_finalized) = self
                    .confirmation_rules
                    .get(&chain_id)
                    .copied()
                    .unwrap_or((2, true));
                let Some(receipt) = runtime
                    .observe_receipt(
                        &candidate.transaction_hash,
                        confirmations.max(1),
                        require_finalized,
                    )
                    .await?
                else {
                    let progress = bridge_candidate_finality_observation(
                        runtime,
                        &candidate,
                        confirmations.max(1),
                        require_finalized,
                    )
                    .await;
                    candidate_observations
                        .push(attach_bridge_finality_observation(locator, progress));
                    continue;
                };
                // FundsMovement is trusted as the native-value payment proof
                // only for the reviewed RouterV3 deployment whose verified
                // implementation emits it after safeTransferETH. ERC-20 paths
                // additionally require the canonical Transfer log below.
                if !receipt.success
                    || !receipt_logs_canonical(
                        &receipt.receipt,
                        &receipt,
                        &candidate.transaction_hash,
                    )?
                {
                    candidate_observations.push(
                        bridge_candidate_observation(&candidate, "protocol_proof_not_matched")
                            .unwrap_or(locator),
                    );
                    continue;
                }
                let router = gofin_evm::address(RELAY_ROUTER)?;
                let router_deployed = runtime.code(router).await.is_ok();
                let event_topic = gofin_evm::alloy::primitives::keccak256(
                    "FundsMovement(address,address,address,uint256,bytes)",
                );
                let logs = receipt.receipt["logs"].as_array().ok_or_else(|| {
                    error(
                        "RECEIPT_LOGS_INVALID",
                        "reconciliation",
                        "Final receipt has no logs.",
                        "Keep the transfer unresolved and retry the original receipt.",
                    )
                })?;
                let transfers = logs
                    .iter()
                    .filter_map(raw_token_transfer)
                    .collect::<Vec<_>>();
                let mut protocol_proof_matched = false;
                let mut reviewed_router_event_found = false;
                let mut router_event_topics_count = None;
                let mut proof_rejection = None;
                let mut matched_method: Option<String> = None;
                for log in logs {
                    if !router_deployed {
                        continue;
                    }
                    if gofin_evm::address(log["address"].as_str().unwrap_or(""))? != router {
                        continue;
                    }
                    let Some(topic) = log["topics"]
                        .as_array()
                        .and_then(|topics| topics.first())
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if gofin_evm::hash(topic)? != event_topic {
                        continue;
                    }
                    reviewed_router_event_found = true;
                    router_event_topics_count = log["topics"].as_array().map(Vec::len);
                    let raw = raw_evm_log(log)?;
                    let expected = gofin_bridge::relay::proof::ExpectedRouterPayment {
                        router: RELAY_ROUTER.into(),
                        recipient: recipient.clone(),
                        token: (asset.reference != "native").then(|| asset.reference.clone()),
                        expected_transaction_sender: saved
                            .quote
                            .order_solver
                            .clone()
                            .unwrap_or_default(),
                        minimum_amount: minimum.clone(),
                        order_id: order_id.clone(),
                        provider_request_id: provider_request_id.clone(),
                    };
                    match gofin_bridge::relay::proof::verify_router_payment_log(
                        &raw,
                        &transfers,
                        &receipt.transaction_sender,
                        &expected,
                    ) {
                        Ok(payment) => {
                            protocol_proof_matched = true;
                            matched_method = Some("router_v3_funds_movement".to_owned());
                            let log_index = gofin_evm::rpc_u64(&log["logIndex"])?;
                            let evidence_id = format!(
                                "ethereum:{chain_id}:{}:{log_index}",
                                candidate.transaction_hash.to_ascii_lowercase()
                            );
                            matches.push((
                                candidate.clone(),
                                receipt.clone(),
                                chain_id,
                                Some(log_index),
                                evidence_id,
                                VerifiedBridgePayment {
                                    method: "router_v3_funds_movement",
                                    router: Some(payment.router),
                                    transaction_sender: receipt.transaction_sender.clone(),
                                    recipient: payment.recipient,
                                    amount: payment.amount,
                                    correlation_id: payment.correlation_id,
                                    native_gas_observation: None,
                                },
                                asset.clone(),
                                outcome.clone(),
                            ));
                        }
                        Err(error) if proof_rejection.is_none() => {
                            proof_rejection = Some(sanitize_router_proof_rejection(error));
                        }
                        Err(_) => {}
                    }
                }
                let transaction = runtime
                    .rpc
                    .request(
                        "eth_getTransactionByHash",
                        json!([candidate.transaction_hash]),
                    )
                    .await
                    .ok();
                let transaction_observation = bridge_transaction_shape_observation(
                    transaction.as_ref(),
                    &candidate,
                    order_id,
                    provider_request_id,
                );
                let direct_payment = transaction.as_ref().and_then(|transaction| {
                    direct_payment_transaction(transaction, &candidate, &receipt)
                });
                let mut direct_proof_rejection = None;
                let mut direct_code_observation = None;
                if let Some(direct_payment) = direct_payment {
                    let expected = gofin_bridge::relay::proof::ExpectedDirectPayment {
                        recipient: recipient.clone(),
                        token: (asset.reference != "native").then(|| asset.reference.clone()),
                        minimum_amount: minimum.clone(),
                        protocol_order_id: order_id.clone(),
                    };
                    let direct_transfers = direct_token_transfers(logs);
                    let payout = gofin_bridge::relay::proof::verify_direct_payment_transaction(
                        &direct_payment,
                        &direct_transfers,
                        &expected,
                    );
                    match payout {
                        Ok(payment) => {
                            let chain_matches = payment.chain_id == chain_id;
                            let code_matches = if asset.reference == "native" {
                                let recipient_code = direct_recipient_code_observation(
                                    runtime,
                                    &recipient,
                                    true,
                                    receipt.block_number,
                                )
                                .await;
                                let matches =
                                    direct_code_observation_matches(&recipient_code, true);
                                direct_code_observation = Some(json!({
                                    "chain_matches":chain_matches,
                                    "recipient_code":recipient_code
                                }));
                                matches
                            } else {
                                // For ERC-20, the exact reviewed-token target,
                                // calldata and canonical Transfer event prove
                                // execution. Historical eth_getCode availability
                                // is not an additional proof requirement.
                                chain_matches
                            };
                            let intrinsic_payment =
                                if asset.reference == "native" && chain_matches && !code_matches {
                                    transaction.as_ref().and_then(|transaction| {
                                        verify_native_intrinsic_rpc_proof(
                                            transaction,
                                            &receipt.receipt,
                                            &direct_payment,
                                            &expected,
                                        )
                                    })
                                } else {
                                    None
                                };
                            let verified_payment = if code_matches {
                                Some(payment)
                            } else {
                                intrinsic_payment
                            };
                            if let Some(payment) = verified_payment {
                                if let Some(proof_method) =
                                    bridge_payment_proof_method(&payment.proof_method)
                                {
                                    protocol_proof_matched = true;
                                    matched_method = Some(proof_method.to_owned());
                                    let log_index = payment.transfer_log_index.map(u64::from);
                                    let evidence_id = match log_index {
                                        Some(index) => format!(
                                            "ethereum:{chain_id}:{}:{index}",
                                            candidate.transaction_hash.to_ascii_lowercase()
                                        ),
                                        None => format!(
                                            "ethereum:{chain_id}:{}:transaction",
                                            candidate.transaction_hash.to_ascii_lowercase()
                                        ),
                                    };
                                    matches.push((
                                        candidate.clone(),
                                        receipt.clone(),
                                        chain_id,
                                        log_index,
                                        evidence_id,
                                        VerifiedBridgePayment {
                                            method: proof_method,
                                            router: None,
                                            transaction_sender: payment.sender,
                                            recipient: payment.recipient,
                                            amount: payment.amount,
                                            correlation_id: payment.protocol_order_id,
                                            native_gas_observation: (proof_method
                                                == "intrinsic_gas_retained_credit")
                                                .then(|| {
                                                    direct_native_gas_facts(
                                                        &receipt.receipt,
                                                        transaction.as_ref(),
                                                    )
                                                }),
                                        },
                                        asset.clone(),
                                        outcome.clone(),
                                    ));
                                } else {
                                    direct_proof_rejection = Some(json!({
                                        "code":"BRIDGE_PROOF_INVALID",
                                        "message":"The verified payout helper returned an unsupported proof method."
                                    }));
                                }
                            } else {
                                direct_proof_rejection = Some(json!({
                                    "code":"BRIDGE_DIRECT_CODE_OR_CHAIN_MISMATCH",
                                    "message":"Direct payout chain identity or required native-recipient code policy did not match the reviewed proof policy."
                                }));
                            }
                        }
                        Err(error) => {
                            direct_proof_rejection = Some(sanitize_direct_proof_rejection(error));
                        }
                    }
                }
                let observation = bridge_candidate_observation(
                    &candidate,
                    if protocol_proof_matched {
                        "protocol_proof_matched"
                    } else {
                        "protocol_proof_not_matched"
                    },
                )
                .unwrap_or(locator);
                if !protocol_proof_matched {
                    let diagnostic = attach_router_proof_diagnostics(
                        observation,
                        &receipt,
                        reviewed_router_event_found,
                        router_event_topics_count,
                        proof_rejection,
                        logs,
                        transaction_observation,
                    );
                    let diagnostic =
                        attach_direct_code_observation(diagnostic, direct_code_observation);
                    let diagnostic = if asset.reference == "native"
                        && direct_native_code_guard_failed(&diagnostic)
                    {
                        attach_direct_native_execution_observation(
                            diagnostic,
                            &receipt,
                            transaction.as_ref(),
                        )
                    } else {
                        diagnostic
                    };
                    candidate_observations.push(attach_direct_payment_rejection(
                        diagnostic,
                        direct_proof_rejection,
                    ));
                } else {
                    candidate_observations.push(attach_bridge_payment_method(
                        observation,
                        matched_method
                            .as_deref()
                            .unwrap_or("verified_protocol_payment"),
                    ));
                }
            }
        }
        if matches.len() != 1 {
            return Ok(bridge_external_pending_status(
                &reconciliation,
                candidate_transactions,
                candidate_observations,
                matches.len(),
            ));
        }
        let (candidate, receipt, chain_id, log_index, evidence_id, payment, asset, outcome) =
            matches.remove(0);
        let recipient = match outcome {
            gofin_domain::BridgeSettlementOutcome::Destination => &saved.quote.intent.recipient,
            gofin_domain::BridgeSettlementOutcome::Refund => &saved.quote.intent.refund_to,
        };
        let resource = gofin_domain::ResourceId(format!(
            "ethereum:{}:{}",
            candidate.chain_id,
            recipient.to_ascii_lowercase()
        ));
        let mut evidence = json!({"evidence_id":evidence_id,"transfer_id":transfer.id,"provider_request_id":provider_request_id,"protocol_order_id":order_id,"chain_id":chain_id,"transaction_hash":candidate.transaction_hash,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"proof_method":payment.method,"router":payment.router,"transaction_sender":payment.transaction_sender,"recipient":payment.recipient,"recipient_resource":resource,"asset":asset,"amount_atoms":payment.amount,"amount":payment.amount,"correlation":payment.correlation_id,"correlation_id":payment.correlation_id,"outcome":outcome,"native_gas_observation":payment.native_gas_observation});
        if let Some(log_index) = log_index {
            evidence["log_index"] = json!(log_index);
        } else {
            evidence["receipt_scope"] = json!("transaction");
        }
        let mut effects = vec![FinancialEffect::UsableCredit {
            to: resource.clone(),
            amount: GoFinAmount {
                asset: asset.clone(),
                atoms: payment.amount.clone(),
            },
        }];
        if outcome == gofin_domain::BridgeSettlementOutcome::Destination
            && let Some((source_principal, executed_quote)) =
                bridge_execute_source_evidence(transfer, &engine.operations()?)
            && let Some(source_withheld_cost) = relay_native_withheld_cost(
                transfer,
                &executed_quote,
                &self.provider(&executed_quote.provider)?.capabilities(),
                &source_principal,
                &asset,
                &payment.amount,
            )?
        {
            effects.push(FinancialEffect::Cost {
                location: source_withheld_cost.location,
                amount: source_withheld_cost.amount,
                category: "withheld_from_output".into(),
                treatment: source_withheld_cost.treatment,
            });
        }
        let mut proof = gofin_domain::BridgeSettlementProof {
            transfer_id: transfer.id.clone(),
            outcome: outcome.clone(),
            evidence_id: evidence["evidence_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            effects,
            evidence,
            observed_at: self.clock.now().to_rfc3339(),
            proof_digest: String::new(),
        };
        proof.proof_digest = gofin_domain::bridge_settlement_digest(&proof)?;
        engine.commit_bridge_external_settlement(proof)?;
        Ok(
            json!({"status":reconciliation.status,"settled":true,"evidence_id":evidence_id,"outcome":outcome}),
        )
    }

    async fn reconcile_canonical_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> gofin_domain::Result<Value> {
        let Some(locator) = transfer
            .transit
            .as_ref()
            .and_then(|transit| transit.destination_locator.as_ref())
        else {
            return Ok(json!({
                "provider": saved.quote.provider,
                "status": "source_confirmed_destination_pending",
                "settled": false,
                "reason": "verified_destination_locator_unavailable"
            }));
        };
        let chain_id = saved.quote.intent.evm_destination_chain_id()?;
        if locator.scheme != "arbitrum_eth_deposit_v1"
            || locator.destination_chain_id != Some(chain_id)
            || !gofin_evm::hash(&locator.transaction_hash).is_ok_and(|hash| {
                format!("{hash:#x}").eq_ignore_ascii_case(&locator.transaction_hash)
            })
        {
            return Err(error(
                "BRIDGE_DESTINATION_LOCATOR_INVALID",
                "reconciliation",
                "The persisted canonical destination locator does not match the original route.",
                "Keep the source-confirmed transfer unresolved and inspect its protected journal.",
            ));
        }
        let runtime = self.observers.get(&chain_id).ok_or_else(|| {
            error(
                "BRIDGE_NETWORK_UNAVAILABLE",
                "reconciliation",
                "The original canonical destination observer is unavailable.",
                "Retry the same transfer status check when the reviewed chain observer is available.",
            )
        })?;
        let (confirmations, require_finalized) = self
            .confirmation_rules
            .get(&chain_id)
            .copied()
            .unwrap_or((2, true));
        let Some(receipt) = runtime
            .observe_receipt(
                &locator.transaction_hash,
                confirmations.max(1),
                require_finalized,
            )
            .await?
        else {
            let observation = canonical_candidate_progress(
                runtime,
                &locator.transaction_hash,
                confirmations.max(1),
                require_finalized,
            )
            .await;
            return Ok(json!({
                "provider": saved.quote.provider,
                "status": "source_confirmed_destination_pending",
                "settled": false,
                "candidate": {
                    "chain_id": chain_id,
                    "transaction_hash": locator.transaction_hash,
                    "reason": "canonical_child_receipt_or_finality_pending",
                    "observation": observation
                }
            }));
        };
        if !receipt.success {
            return Ok(json!({
                "provider": saved.quote.provider,
                "status": "source_confirmed_destination_pending",
                "settled": false,
                "candidate": {
                    "chain_id": chain_id,
                    "transaction_hash": locator.transaction_hash,
                    "reason": "canonical_child_transaction_reverted"
                }
            }));
        }
        let transaction = runtime
            .rpc
            .request(
                "eth_getTransactionByHash",
                json!([locator.transaction_hash]),
            )
            .await?;
        if transaction.is_null() {
            return Ok(json!({
                "provider": saved.quote.provider,
                "status": "source_confirmed_destination_pending",
                "settled": false,
                "candidate": {
                    "chain_id": chain_id,
                    "transaction_hash": locator.transaction_hash,
                    "reason": "canonical_child_transaction_unavailable"
                }
            }));
        }
        let transaction_hash = transaction["hash"].as_str().unwrap_or_default();
        let transaction_chain_id = transaction
            .get("chainId")
            .filter(|value| !value.is_null())
            .map(gofin_evm::rpc_u64)
            .transpose()
            .ok()
            .flatten();
        let transaction_type = gofin_evm::rpc_u64(&transaction["type"]).ok();
        let block_number = gofin_evm::rpc_u64(&transaction["blockNumber"]).ok();
        let block_hash = transaction["blockHash"].as_str().unwrap_or_default();
        if !transaction_hash.eq_ignore_ascii_case(&locator.transaction_hash)
            || transaction_chain_id.is_some_and(|observed| observed != chain_id)
            || block_number != Some(receipt.block_number)
            || !block_hash.eq_ignore_ascii_case(&format!("{:#x}", receipt.block_hash))
        {
            return Ok(json!({
                "provider": saved.quote.provider,
                "status": "source_confirmed_destination_pending",
                "settled": false,
                "candidate": {
                    "chain_id": chain_id,
                    "transaction_hash": locator.transaction_hash,
                    "reason": "canonical_child_transaction_inclusion_mismatch"
                }
            }));
        }
        let expected: gofin_bridge::canonical::VerifiedEthDeposit =
            serde_json::from_value(locator.proof.clone()).map_err(|_| {
                error(
                    "BRIDGE_DESTINATION_LOCATOR_INVALID",
                    "reconciliation",
                    "The saved canonical source proof cannot be decoded.",
                    "Keep the transfer unresolved and inspect its protected source evidence.",
                )
            })?;
        let child = gofin_bridge::canonical::CanonicalChildTransaction {
            hash: transaction_hash.into(),
            chain_id,
            transaction_type: transaction_type
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or_default(),
            from: transaction["from"].as_str().unwrap_or_default().into(),
            to: transaction["to"].as_str().unwrap_or_default().into(),
            value: gofin_evm::quantity(gofin_evm::rpc_u256(&transaction["value"])?),
        };
        let verified = match gofin_bridge::canonical::verify_eth_deposit_child(
            &child,
            &gofin_bridge::canonical::CanonicalChildReceipt {
                transaction_hash: format!("{:#x}", receipt.hash),
                success: receipt.success,
            },
            &expected,
        ) {
            Ok(verified) => verified,
            Err(_) => {
                return Ok(json!({
                    "provider": saved.quote.provider,
                    "status": "source_confirmed_destination_pending",
                    "settled": false,
                    "candidate": {
                        "chain_id": chain_id,
                        "transaction_hash": locator.transaction_hash,
                        "reason": "canonical_child_message_identity_not_matched"
                    }
                }));
            }
        };
        let resource = gofin_domain::ResourceId(format!(
            "ethereum:{chain_id}:{}",
            verified.recipient.to_ascii_lowercase()
        ));
        let amount = GoFinAmount {
            asset: saved.quote.expected_receipt.asset.clone(),
            atoms: verified.amount.clone(),
        };
        let evidence_id = format!(
            "ethereum:{chain_id}:{}:transaction",
            verified.transaction_hash.to_ascii_lowercase()
        );
        let evidence = json!({
            "transfer_id": transfer.id,
            "evidence_id": evidence_id,
            "receipt_scope": "transaction",
            "chain_id": chain_id,
            "transaction_hash": verified.transaction_hash,
            "correlation": locator.transaction_hash,
            "source_evidence_id": locator.source_evidence_id,
            "block_number": receipt.block_number,
            "block_hash": format!("{:#x}", receipt.block_hash),
            "confirmations": receipt.confirmations,
            "canonical": true,
            "success": true,
            "recipient": verified.recipient,
            "recipient_resource": resource.0,
            "asset": amount.asset,
            "amount_atoms": amount.atoms,
            "message_index": verified.message_index,
            "proof_method": "canonical_eth_deposit_child_transaction"
        });
        let mut proof = gofin_domain::BridgeSettlementProof {
            transfer_id: transfer.id.clone(),
            outcome: gofin_domain::BridgeSettlementOutcome::Destination,
            evidence_id: evidence_id.clone(),
            effects: vec![FinancialEffect::UsableCredit {
                to: resource,
                amount,
            }],
            evidence,
            observed_at: self.clock.now().to_rfc3339(),
            proof_digest: String::new(),
        };
        proof.proof_digest = gofin_domain::bridge_settlement_digest(&proof)?;
        engine.commit_bridge_external_settlement(proof)?;
        Ok(json!({
            "provider": saved.quote.provider,
            "status": "destination_settled",
            "settled": true,
            "evidence_id": evidence_id,
            "transaction_hash": locator.transaction_hash,
            "outcome": "destination"
        }))
    }

    async fn reconcile_base_external(
        &self,
        engine: &Engine,
        transfer: &BridgeTransferRecord,
        saved: &SavedQuote,
    ) -> gofin_domain::Result<Value> {
        let Some(locator) = transfer
            .transit
            .as_ref()
            .and_then(|transit| transit.destination_locator.as_ref())
        else {
            return Ok(
                json!({"provider":"base_canonical","settled":false,"status":"source_proof_pending"}),
            );
        };
        let chain = saved.quote.intent.evm_destination_chain_id()?;
        let runtime = self.observers.get(&chain).ok_or_else(internal_error)?;
        let mut candidates = Vec::new();
        let (message, withdrawal_hash) = if chain == 8453 {
            let deposit: gofin_bridge::base_canonical::BaseDeposit =
                serde_json::from_value(locator.proof["deposit"].clone()).map_err(|_| {
                    GoFinError::validation("Saved Base deposit proof is malformed.")
                })?;
            if locator.scheme != "base_deposit_v1"
                || locator.transaction_hash != deposit.transaction_hash
                || locator.message_hash.is_some()
            {
                return Err(GoFinError::validation(
                    "Base deposit locator differs from its source-derived transaction identity.",
                ));
            }
            candidates.push(locator.transaction_hash.clone());
            (deposit.message_hash, None)
        } else {
            let verified = saved_base_withdrawal(locator)?;
            let operations = engine.operations()?;
            for link in transfer
                .steps
                .iter()
                .filter(|link| link.kind == BridgeStepKind::Finalize)
            {
                if let Some(operation) = operations
                    .iter()
                    .find(|op| Some(&op.id) == link.operation_id.as_ref())
                    && let Some(submission) = &operation.submission
                {
                    candidates.push(submission.reference.clone());
                }
            }
            // Permissionless third-party finalization may satisfy the same
            // immutable withdrawal. Logs locate candidates; receipts prove it.
            {
                let block =
                    gofin_evm::rpc_u64(&runtime.rpc.request("eth_blockNumber", json!([])).await?)?;
                let topic =
                    gofin_evm::alloy::primitives::keccak256("WithdrawalFinalized(bytes32,bool)");
                let logs = runtime.rpc.request("eth_getLogs",json!([{"address":gofin_bridge::base_canonical::PORTAL,"fromBlock":format!("{:#x}",block.saturating_sub(10_000)),"toBlock":format!("{block:#x}"),"topics":[format!("{topic:#x}"),verified.withdrawal_hash]}])).await?;
                if let Some(logs) = logs.as_array().filter(|logs| logs.len() <= 16) {
                    for log in logs {
                        if let Some(hash) = log["transactionHash"].as_str() {
                            gofin_evm::hash(hash)?;
                            candidates.push(hash.into());
                        }
                    }
                }
            }
            if candidates.is_empty() {
                let provider = self.base_provider()?;
                let readiness = provider
                    .withdrawal_readiness(&verified.withdrawal, &saved.quote.intent.source_address)
                    .await?;
                return Ok(
                    json!({"provider":"base_canonical","settled":false,"status":"withdrawal_pending","withdrawal_hash":verified.withdrawal_hash,"readiness":readiness,"locator_coverage":"GoFin submission identities and latest 10,000 Ethereum blocks; partial for externally finalized withdrawals","next_action":if readiness.finalized {"The portal reports finalization, but exact usable credit was not found in bounded discovery. Preserve this withdrawal and inspect destination receipt coverage; do not submit again."} else if readiness.proof_invalidated {"The proof was invalidated. An explicitly authorized re-proof of this same withdrawal is required."} else if readiness.ready {"Prepare the saved finalize stage."} else if readiness.proven_at==0 {"Prepare the saved prove stage once a checkpoint is available."} else {"Wait for the current portal proof/game checks; do not reinitiate the withdrawal."}}),
                );
            }
            (verified.message_hash, Some(verified.withdrawal_hash))
        };
        candidates.sort();
        candidates.dedup();
        let (confirmations, finalized) = self
            .confirmation_rules
            .get(&chain)
            .copied()
            .unwrap_or((2, true));
        for candidate in candidates {
            let Some(receipt) = runtime
                .observe_receipt(&candidate, confirmations, finalized)
                .await?
            else {
                continue;
            };
            if !receipt.success || !receipt_logs_canonical(&receipt.receipt, &receipt, &candidate)?
            {
                continue;
            }
            let raw = receipt.receipt["logs"]
                .as_array()
                .ok_or_else(internal_error)?
                .iter()
                .map(canonical_raw_log)
                .collect::<gofin_domain::Result<Vec<_>>>()?;
            if let Some(hash) = &withdrawal_hash {
                if gofin_bridge::base_canonical::verify_withdrawal_finalized(&raw, hash).is_err() {
                    continue;
                }
            } else {
                let deposit: gofin_bridge::base_canonical::BaseDeposit =
                    serde_json::from_value(locator.proof["deposit"].clone())
                        .map_err(|_| internal_error())?;
                let tx = runtime
                    .rpc
                    .request("eth_getTransactionByHash", json!([candidate]))
                    .await?;
                gofin_bridge::base_canonical::verify_deposit_transaction(&tx, &deposit)?;
            }
            let log_index = gofin_bridge::base_canonical::verify_destination_logs(
                &raw,
                &saved.quote.intent,
                &message,
            )?;
            if saved.quote.expected_receipt.asset.reference == "native" {
                require_empty_code(
                    runtime,
                    &saved.quote.intent.recipient,
                    Some(receipt.block_number),
                )
                .await?;
            }
            let resource = gofin_domain::ResourceId(format!(
                "ethereum:{chain}:{}",
                saved.quote.intent.recipient.to_ascii_lowercase()
            ));
            let amount = saved.quote.expected_receipt.clone();
            let evidence_id = format!(
                "ethereum:{chain}:{}:{log_index}",
                candidate.to_ascii_lowercase()
            );
            let evidence = json!({"transfer_id":transfer.id,"log_index":log_index,"correlation":withdrawal_hash.as_ref().unwrap_or(&message),"canonical":true,"success":true,"confirmations":confirmations,"recipient":saved.quote.intent.recipient,"provider":"base_canonical","chain_id":chain,"transaction_hash":candidate,"withdrawal_hash":withdrawal_hash,"message_hash":message,"source_evidence_id":locator.source_evidence_id,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"recipient_resource":resource,"asset":amount.asset,"amount_atoms":amount.atoms,"evidence_id":evidence_id,"proof_method":"base_canonical_message_and_usable_credit"});
            let mut proof = gofin_domain::BridgeSettlementProof {
                transfer_id: transfer.id.clone(),
                outcome: gofin_domain::BridgeSettlementOutcome::Destination,
                evidence_id: evidence_id.clone(),
                effects: vec![FinancialEffect::UsableCredit {
                    to: resource,
                    amount,
                }],
                evidence,
                observed_at: self.clock.now().to_rfc3339(),
                proof_digest: String::new(),
            };
            proof.proof_digest = gofin_domain::bridge_settlement_digest(&proof)?;
            engine.commit_bridge_external_settlement(proof)?;
            return Ok(
                json!({"provider":"base_canonical","settled":true,"status":"destination_settled","evidence_id":evidence_id}),
            );
        }
        Ok(
            json!({"provider":"base_canonical","settled":false,"status":"destination_receipt_pending","message_hash":message}),
        )
    }

    async fn prepare_base_stage(
        &self,
        operation: &str,
        input: &Value,
        record: &BridgeTransferRecord,
        saved: &SavedQuote,
        mut step: BridgeStep,
        requested: Option<BridgeBounds>,
    ) -> gofin_domain::Result<PreparedOperation> {
        let step_id = saved
            .step_ids
            .get(&step.ordinal)
            .ok_or_else(internal_error)?;
        let link = record
            .steps
            .iter()
            .find(|link| &link.step_id == step_id)
            .ok_or_else(internal_error)?;
        if saved.quote.provider != "base_canonical"
            || saved.quote.intent.evm_source()?.chain_id != 8453
            || saved.quote.intent.destination_chain_id != Some(1)
            || record.grant_id.is_some()
        {
            return Err(GoFinError::validation(
                "Deferred stages require an original Base canonical withdrawal under exact or named-plan authority.",
            ));
        }
        let locator = record.transit.as_ref().and_then(|transit| transit.destination_locator.as_ref())
            .ok_or_else(|| error("BRIDGE_SOURCE_PROOF_REQUIRED", "preview", "The original Base withdrawal source proof is not settled.", "Reconcile the original initiation transaction before preparing a proof or finalization."))?;
        let verified = saved_base_withdrawal(locator)?;
        let alias = record.metadata["wallet_alias"]
            .as_str()
            .ok_or_else(internal_error)?;
        let wallet = self.wallet(alias, 1)?;
        let sender = wallet.address.as_deref().ok_or_else(internal_error)?;
        if !sender.eq_ignore_ascii_case(&saved.quote.intent.source_address)
            || !wallet.signing_enabled
        {
            return Err(GoFinError::validation(
                "The saved withdrawal requires the same reusable signing wallet on Ethereum.",
            ));
        }
        let bounds: BridgeBounds =
            serde_json::from_value(record.approved_bounds.clone().ok_or_else(|| {
                GoFinError::validation("Original withdrawal bounds are missing.")
            })?)
            .map_err(|_| GoFinError::validation("Original withdrawal bounds are invalid."))?;
        if let Some(requested) = requested
            && normalize_authorization_bounds(Some(requested), saved)? != bounds
        {
            return Err(GoFinError::validation(
                "Recovery stages preserve the original withdrawal bounds and consumed fees.",
            ));
        }
        let provider = self.base_provider()?;
        let readiness = provider
            .withdrawal_readiness(&verified.withdrawal, sender)
            .await?;
        let witness = if link.kind == BridgeStepKind::Prove {
            if readiness.finalized || (readiness.proven_at > 0 && !readiness.proof_invalidated) {
                return Err(error(
                    "BRIDGE_PROOF_ALREADY_PRESENT",
                    "preview",
                    "The saved withdrawal already has a proof or finalization.",
                    "Inspect bridge.status; do not reset a valid proof's maturity timer.",
                ));
            }
            let source_block = locator.proof["source_block_number"]
                .as_u64()
                .ok_or_else(internal_error)?;
            let witness = provider
                .withdrawal_witness(&verified.withdrawal, source_block, sender)
                .await?;
            step.call = Some(gofin_bridge::base_canonical::encode_prove_call(
                &verified.withdrawal,
                &witness,
            )?);
            Some(witness)
        } else {
            if !readiness.ready || readiness.finalized {
                return Err(error(
                    "BRIDGE_FINALIZATION_PENDING",
                    "preview",
                    "The saved withdrawal has not satisfied the portal's current finalization checks.",
                    "Use bridge.status to inspect proof maturity and game validity; no write was submitted.",
                ));
            }
            step.call = Some(gofin_bridge::base_canonical::encode_finalize_call(
                &verified.withdrawal,
                sender,
            )?);
            None
        };
        if step.evm_call()?.chain_id != 1
            || step.evm_call()?.to != gofin_bridge::base_canonical::PORTAL
            || !step.evm_call()?.value.is_zero()
            || step.evm_call_mut()?.data == "0x"
            || !step.maximum_debit.is_empty()
        {
            return Err(GoFinError::validation(
                "Deferred Base call was not materialized as an exact zero-value portal stage.",
            ));
        }
        let runtime = &self
            .wallet_adapters
            .get(&1)
            .ok_or_else(internal_error)?
            .runtime;
        let transaction = runtime
            .prepare(
                gofin_evm::address(sender)?,
                gofin_evm::address(&step.evm_call()?.to)?,
                gofin_evm::alloy::primitives::U256::ZERO,
                gofin_evm::rpc_bytes(&json!(step.evm_call()?.data))?,
                gofin_evm::u256(&maximum_gas_cap(&bounds, 1)?)?,
            )
            .await?
            .transaction;
        let gas_asset = EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        }
        .asset_id();
        self.validate_frozen_gas_cap(
            &bounds,
            &gas_asset,
            &gofin_evm::quantity(transaction.maximum_cost()?),
        )
        .await?;
        self.validate_transfer_gas_budget(
            record,
            &link.step_id,
            &bounds,
            &gas_asset,
            transaction.maximum_cost()?,
        )?;
        let resource = wallet.resource;
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: GoFinAmount {
                    asset: gas_asset,
                    atoms: gofin_evm::quantity(transaction.maximum_cost()?),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![resource.clone()],
        };
        let valuation = self.value_intent(&intent).await?;
        let expectation = record
            .settlement_expectation
            .clone()
            .ok_or_else(internal_error)?;
        let mut plan = saved.plan.clone();
        plan.bounds = bounds.clone();
        plan.fixed_scope_digest =
            gofin_bridge::base_canonical::scope_digest(&saved.quote, &bounds, sender)?;
        let mut prepared = PreparedOperation {
            action_deadline: None,
            operation:operation.into(), input:input.clone(),
            binding:json!({"resource":resource,"transaction":transaction,"transfer_id":record.id,"step_id":link.step_id,"step_kind":link.kind,"step_dependencies":link.depends_on,"grant_id":null,"quote_id":saved.quote.quote_id,"route_id":saved.quote.route_id,"quote":saved.quote,"intent":saved.quote.intent,"plan_digest":plan.fixed_scope_digest,"plan":plan,"step_ids":saved.step_ids,"step":step,"bounds":bounds,"source_locator":locator,"base_withdrawal":verified,"base_witness":witness,
                (gofin_domain::BRIDGE_STEP_METADATA_KEY):{"transfer_id":record.id,"grant_id":null,"step_id":link.step_id,"kind":step_kind_name(&link.kind),"bounds":bounds,"settlement_expectation":expectation}}),
            summary:self.summary(saved,&link.kind,alias),
            checks:vec![Check{name:"saved Base withdrawal identity and current portal stage".into(),status:CheckStatus::Passed,detail:"The broker materialized this exact stage from settled source proof; no new principal is moved.".into()}],
            warnings:vec!["Canonical withdrawal timing follows current on-chain proof maturity and game validity; source confirmation is not destination credit.".into()],
            observed_at:self.clock.now().to_rfc3339(), expires_at:(self.clock.now()+chrono::Duration::seconds(90)).to_rfc3339(),valuation:Some(valuation),intent,operation_version:1,
        };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }

    fn validate_transfer_gas_budget(
        &self,
        transfer: &BridgeTransferRecord,
        current_step: &str,
        bounds: &BridgeBounds,
        asset: &gofin_domain::AssetId,
        candidate: gofin_evm::alloy::primitives::U256,
    ) -> gofin_domain::Result<()> {
        let (spent, reserved) = self.transfer_gas_usage(transfer, Some(current_step), asset)?;
        let total = candidate
            .checked_add(spent)
            .and_then(|n| n.checked_add(reserved))
            .ok_or_else(internal_error)?;
        let chain = asset
            .network
            .strip_prefix("ethereum:")
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(internal_error)?;
        if total > gofin_evm::u256(&maximum_gas_cap(bounds, chain)?)? {
            return Err(error(
                "BRIDGE_CUMULATIVE_FEE_LIMIT",
                "preview",
                "Prior bridge costs and unresolved reservations leave insufficient approved gas budget for this stage.",
                "Preserve the transfer identity and inspect bridge.status; obtain an explicit revised fee budget where supported.",
            ));
        }
        Ok(())
    }

    /// Canonical costs and uncertain reservations remain separate: only proven
    /// costs can reduce the balance needed to fund the remaining approved cap.
    fn transfer_gas_usage(
        &self,
        transfer: &BridgeTransferRecord,
        current_step: Option<&str>,
        asset: &gofin_domain::AssetId,
    ) -> gofin_domain::Result<(
        gofin_evm::alloy::primitives::U256,
        gofin_evm::alloy::primitives::U256,
    )> {
        let engine = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        let operations = if let Some(engine) = engine {
            engine.operations()?
        } else {
            if transfer
                .steps
                .iter()
                .any(|link| link.operation_id.is_some())
            {
                return Err(GoFinError::validation(
                    "Historical bridge costs require the attached protected operation journal.",
                ));
            }
            Vec::new()
        };
        let mut spent = gofin_evm::alloy::primitives::U256::ZERO;
        let mut reserved = gofin_evm::alloy::primitives::U256::ZERO;
        for operation in operations.iter().filter(|operation| {
            operation.preview.as_ref().is_some_and(|preview| {
                preview.binding["transfer_id"] == transfer.id
                    && current_step
                        .is_none_or(|step| preview.binding["step_id"].as_str() != Some(step))
            })
        }) {
            if let Some(effects) = &operation.effects {
                for effect in &effects.effects {
                    if let FinancialEffect::Cost {
                        amount,
                        treatment: CostTreatment::AdditionalDebit,
                        ..
                    } = effect
                        && amount.asset == *asset
                    {
                        spent = spent
                            .checked_add(gofin_evm::u256(&amount.atoms)?)
                            .ok_or_else(internal_error)?;
                    }
                }
            } else if operation.submission.is_some() || !operation.status.terminal() {
                // Uncertain submissions continue to reserve their complete frozen
                // bound; terminal unsubmitted previews consume nothing.
                for cost in &operation
                    .preview
                    .as_ref()
                    .ok_or_else(internal_error)?
                    .intent
                    .maximum_costs
                {
                    if cost.amount.asset == *asset
                        && cost.treatment == CostTreatment::AdditionalDebit
                    {
                        reserved = reserved
                            .checked_add(gofin_evm::u256(&cost.amount.atoms)?)
                            .ok_or_else(internal_error)?;
                    }
                }
            }
        }
        Ok((spent, reserved))
    }

    fn saved_quote(&self, record: &BridgeTransferRecord) -> gofin_domain::Result<SavedQuote> {
        let revision = record.quote_revisions.last().ok_or_else(|| {
            error(
                "BRIDGE_QUOTE_MISSING",
                "validation",
                "Transfer has no persisted quote revision.",
                "Request a new bridge quote.",
            )
        })?;
        serde_json::from_value::<SavedQuote>(revision.payload.clone()).map_err(|_| {
            error(
                "BRIDGE_QUOTE_CORRUPT",
                "reconciliation",
                "Persisted quote revision cannot be decoded.",
                "Keep the transfer unresolved and inspect the original broker journal.",
            )
        })
    }

    fn transfer_input(&self, input: &Value) -> gofin_domain::Result<BridgeTransferInput> {
        serde_json::from_value(input.clone()).map_err(|_| {
            error(
                "INVALID_INPUT",
                "validation",
                "Bridge transfer input is malformed or has unknown fields.",
                "Use the bridge.transfer schema.",
            )
        })
    }

    /// Refresh an expired source quote through the same checked read path as
    /// explicit refreshTransferId. One preparation makes at most one refresh;
    /// the engine still owns the cumulative candidate retry budget.
    async fn refresh_expired_source_quote(
        &self,
        record: &BridgeTransferRecord,
        saved: &SavedQuote,
        link: &BridgeStepLink,
    ) -> gofin_domain::Result<bool> {
        if link.kind.is_destination_stage() {
            return Ok(false);
        }
        match self.check_quote_freshness(&saved.quote) {
            Ok(()) => return Ok(false),
            Err(error) if error.code == "BRIDGE_QUOTE_EXPIRED" => {}
            Err(error) => return Err(error),
        }
        gofin_domain::execution_progress::emit(
            gofin_domain::execution_progress::Stage::RefreshingCandidate,
        );
        let intent: BridgeIntent = serde_json::from_value(record.metadata["intent"].clone())
            .map_err(|_| internal_error())?;
        let evm_asset = |asset: &EvmAsset| BridgeAssetInput {
            chain_id: asset.chain_id,
            token: asset.token.clone(),
        };
        let solana_asset = |asset: &gofin_bridge::SolanaAsset| gofin_bridge::SolanaAssetInput {
            network: asset.network.clone(),
            mint: asset.mint.clone(),
        };
        let input = BridgeQuoteInput {
            provider: Some(saved.quote.provider.clone()),
            cctp: saved
                .quote
                .cctp
                .as_ref()
                .map(|metadata| gofin_bridge::cctp::CctpOptions {
                    transfer_mode: metadata.transfer_mode,
                }),
            refresh_transfer_id: Some(record.id.clone()),
            // Default means reuse the persisted approved bounds (or original
            // quote bounds before authority exists), never caller-widened caps.
            bounds: BridgeBounds::default(),
            intent: gofin_bridge::BridgeQuoteIntentInput {
                wallet: record.metadata["wallet_alias"]
                    .as_str()
                    .ok_or_else(internal_error)?
                    .into(),
                source: intent.source.as_ref().map(evm_asset),
                solana_source: intent.solana_source.as_ref().map(solana_asset),
                destination_chain_id: intent.destination_chain_id,
                solana_destination: intent.solana_destination.as_ref().map(|destination| {
                    gofin_bridge::SolanaDestinationInput {
                        network: destination.network.clone(),
                        wallet: destination.wallet.clone(),
                        mint: (destination.asset.reference != "native")
                            .then(|| destination.asset.reference.clone()),
                    }
                }),
                destination_assets: intent.destination_assets.iter().map(evm_asset).collect(),
                recipient: intent.recipient,
                refund_to: intent.refund_to,
                allowed_refund_assets: intent.allowed_refund_assets.iter().map(evm_asset).collect(),
                allowed_solana_refund_assets: intent
                    .allowed_solana_refund_assets
                    .iter()
                    .map(solana_asset)
                    .collect(),
                amount: saved.quote.intent.amount.clone(),
            },
        };
        let output = self
            .quote_with_timeout(
                &serde_json::to_value(input).map_err(|_| internal_error())?,
                QUOTE_PREPARATION_TIMEOUT,
            )
            .await?;
        if output["transfers"]
            .as_array()
            .is_none_or(|rows| rows.len() != 1 || rows[0]["transfer_id"] != record.id)
        {
            if let Some(failure) = output["failures"].as_array().and_then(|rows| rows.first())
                && let Ok(failure) = serde_json::from_value::<GoFinError>(failure["error"].clone())
            {
                return Err(failure);
            }
            return Err(error("BRIDGE_QUOTE_REFRESH_UNAVAILABLE","quote","The saved transfer could not obtain a fresh quote; no financial transaction was sent.","Resolve the reported provider failure, then retry the original transfer and step under still-active authority. Do not create replacement transfer IDs to reset retries.")
                .with_details(json!({"transfer_id":record.id,"step_id":link.step_id,"submission_recorded":false,"same_transfer_required":true})));
        }
        Ok(true)
    }

    fn step_context(
        &self,
        input: &Value,
    ) -> gofin_domain::Result<(
        BridgeTransferRecord,
        SavedQuote,
        BridgeStep,
        BridgeStepLink,
        Option<BridgeBounds>,
    )> {
        let input = self.transfer_input(input)?;
        let record = self
            .state()?
            .transfer(&input.transfer_id)
            .map_err(store_error)?
            .ok_or_else(|| {
                error(
                    "BRIDGE_TRANSFER_UNKNOWN",
                    "validation",
                    "Bridge transfer identity is not present in the protected journal.",
                    "Request a fresh bridge quote.",
                )
            })?;
        if matches!(
            record.state,
            BridgeTransferState::Completed
                | BridgeTransferState::Refunded
                | BridgeTransferState::Failed
        ) {
            return Err(error(
                "BRIDGE_TRANSFER_TERMINAL",
                "validation",
                "This bridge transfer is already terminal.",
                "Inspect its status; create a new transfer only for a deliberate new intent.",
            ));
        }
        let saved = self.saved_quote(&record)?;
        let link = record
            .steps
            .iter()
            .find(|step| step.step_id == input.step_id)
            .cloned()
            .ok_or_else(|| {
                error(
                    "BRIDGE_STEP_UNKNOWN",
                    "validation",
                    "Step identity is not part of this persisted transfer.",
                    "Choose one of the exact broker-returned step IDs.",
                )
            })?;
        if link.kind != input.step_kind {
            return Err(error(
                "BRIDGE_STEP_KIND_MISMATCH",
                "validation",
                "Step kind differs from the protected step plan.",
                "Use the kind returned for this exact step ID.",
            ));
        }
        let current_attempt_is_preparing = if link.state == BridgeStepState::Submitted {
            link.operation_id.as_deref().is_some_and(|operation_id| {
                self.engine
                    .lock()
                    .ok()
                    .and_then(|weak| weak.as_ref().and_then(std::sync::Weak::upgrade))
                    .and_then(|engine| engine.operations().ok())
                    .is_some_and(|operations| {
                        operations.iter().any(|operation| {
                            let same_step_input = serde_json::from_value::<BridgeTransferInput>(
                                operation.input.clone(),
                            )
                            .ok()
                            .as_ref()
                                == Some(&input);
                            operation.id == operation_id
                                && matches!(
                                    operation.operation.as_str(),
                                    "bridge.transfer" | "bridge.execute"
                                )
                                && same_step_input
                                && operation.status == Status::AwaitingApproval
                                && operation.submission.is_none()
                        })
                    })
            })
        } else {
            false
        };
        if link.state != BridgeStepState::Unsubmitted && !current_attempt_is_preparing {
            return Err(error(
                "BRIDGE_STEP_ALREADY_SUBMITTED",
                "validation",
                "This exact bridge step was already submitted or settled.",
                "Reconcile its original operation identity; never replay a submitted step.",
            ));
        }
        for dependency in &link.depends_on {
            if !record
                .steps
                .iter()
                .any(|step| step.step_id == *dependency && step.state == BridgeStepState::Settled)
            {
                return Err(error(
                    "BRIDGE_STEP_DEPENDENCY_PENDING",
                    "validation",
                    "A required earlier bridge step is not proven settled.",
                    "Reconcile the required approval step before preparing this step.",
                ));
            }
        }
        let provider_step = saved
            .plan
            .steps
            .iter()
            .find(|step| {
                self.step_id_for(&record, step.ordinal).as_deref() == Some(link.step_id.as_str())
            })
            .cloned()
            .ok_or_else(|| {
                error(
                    "BRIDGE_STEP_PLAN_MISMATCH",
                    "reconciliation",
                    "Persisted step link no longer matches its normalized provider plan.",
                    "Keep the transfer unresolved and inspect its journal.",
                )
            })?;
        Ok((record, saved, provider_step, link, input.bounds))
    }

    fn step_id_for(&self, record: &BridgeTransferRecord, ordinal: u8) -> Option<String> {
        let payload = record.quote_revisions.last()?.payload.as_object()?;
        let step_ids = payload.get("step_ids")?.as_object()?;
        step_ids
            .get(&ordinal.to_string())?
            .as_str()
            .map(str::to_owned)
    }

    fn summary(&self, saved: &SavedQuote, kind: &BridgeStepKind, alias: &str) -> String {
        let source = saved.quote.intent.source.as_ref();
        let destination = saved.quote.intent.destination_chain_id;
        let source_name = source
            .map(|source| chain_name(source.chain_id))
            .unwrap_or("Solana mainnet-beta");
        let destination_name = destination.map(chain_name).unwrap_or("Solana mainnet-beta");
        let amount = format_amount(&saved.quote.gross_debit);
        let range = saved
            .bounds
            .input
            .as_ref()
            .filter(|range| range.minimum != range.maximum)
            .map(|range| {
                format!(
                    "; authorized input {}–{}",
                    format_amount(&GoFinAmount {
                        asset: saved.quote.gross_debit.asset.clone(),
                        atoms: range.minimum.clone()
                    }),
                    format_amount(&GoFinAmount {
                        asset: saved.quote.gross_debit.asset.clone(),
                        atoms: range.maximum.clone()
                    })
                )
            })
            .unwrap_or_default();
        let minimum = format_amount(
            saved
                .bounds
                .minimum_net_receipt
                .as_ref()
                .unwrap_or(&saved.quote.minimum_receipt),
        );
        let fees = saved
            .bounds
            .fee_caps
            .iter()
            .map(format_fee_cap)
            .collect::<Vec<_>>()
            .join(", ");
        let total_fees = aggregate_fee_caps(&saved.bounds.fee_caps)
            .map(|amounts| format!("; Total fees up to {amounts}"))
            .unwrap_or_default();
        let recipient = saved
            .quote
            .intent
            .destination_chain_id
            .map(|chain| self.wallet_label(chain, &saved.quote.intent.recipient))
            .unwrap_or_else(|| abbreviate_address(&saved.quote.intent.recipient));
        let refund = if !saved.quote.refund_paths.is_empty()
            && !saved
                .quote
                .intent
                .refund_to
                .eq_ignore_ascii_case(&saved.quote.intent.recipient)
        {
            format!(
                "; Refund recipient {}",
                abbreviate_address(&saved.quote.intent.refund_to)
            )
        } else {
            String::new()
        };
        let refund_floor = saved
            .quote
            .refund_paths
            .iter()
            .map(|path| {
                format!(
                    "{} {} ≥{}",
                    if path.network.as_deref() == Some(gofin_solana::NETWORK) {
                        "Solana mainnet-beta"
                    } else {
                        chain_name(path.chain_id)
                    },
                    abbreviate_address(&path.recipient),
                    format_amount(&GoFinAmount {
                        asset: path.asset.clone(),
                        atoms: path.minimum.clone()
                    })
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let action = match kind {
            BridgeStepKind::Approval => {
                let cap = saved
                    .bounds
                    .maximum_source_debit
                    .as_ref()
                    .map(|atoms| {
                        format_amount(&GoFinAmount {
                            asset: saved.quote.gross_debit.asset.clone(),
                            atoms: atoms.clone(),
                        })
                    })
                    .unwrap_or_else(|| "the selected finite cap".into());
                let spender = saved
                    .quote
                    .approval
                    .as_ref()
                    .map(|approval| abbreviate_address(&approval.spender))
                    .unwrap_or_else(|| "reviewed spender".into());
                format!(
                    "Approve up to {cap} for {} spender {spender}",
                    bridge_provider_label(&saved.quote.provider)
                )
            }
            BridgeStepKind::ResetAllowance => {
                let zero = format_amount(&GoFinAmount {
                    asset: saved.quote.gross_debit.asset.clone(),
                    atoms: gofin_domain::Quantity::zero(),
                });
                let spender = saved
                    .quote
                    .approval
                    .as_ref()
                    .map(|approval| abbreviate_address(&approval.spender))
                    .unwrap_or_else(|| "reviewed spender".into());
                format!(
                    "Reset token allowance to {zero} for {} spender {spender}",
                    bridge_provider_label(&saved.quote.provider)
                )
            }
            BridgeStepKind::Deposit => String::new(),
            BridgeStepKind::Prove => "Prove the saved Base withdrawal on Ethereum".into(),
            BridgeStepKind::Finalize => "Finalize the saved Base withdrawal on Ethereum".into(),
            BridgeStepKind::Mint => "Mint the saved CCTP burn on its approved destination".into(),
        };
        let bridge_clause = if matches!(kind, BridgeStepKind::Deposit) {
            if saved.quote.provider == "cctp" {
                format!("Burn {amount}{range} for CCTP")
            } else {
                format!("Bridge {amount}{range}")
            }
        } else if kind.is_destination_stage() {
            format!("{action}; saved principal {amount}")
        } else {
            format!("{action}; bridge {amount}{range}")
        };
        let suffix = if fees.is_empty() {
            "fee caps unavailable".into()
        } else {
            format!("Max fees {fees}{total_fees}")
        };
        let refund = if refund_floor.is_empty() {
            refund
        } else {
            format!("{refund}; Refund paths {refund_floor}")
        };
        format!(
            "{bridge_clause}; {} {} → {} via {}; recipient {}; Receive ≥{minimum}; {suffix}{refund}.",
            safe_label(alias),
            source_name,
            destination_name,
            bridge_provider_label(&saved.quote.provider),
            recipient
        )
    }

    fn wallet_label(&self, chain_id: u64, address: &str) -> String {
        if let Some(adapter) = self.wallet_adapters.get(&chain_id)
            && let Some(connection) = adapter.connections().into_iter().find(|connection| {
                connection
                    .address
                    .as_deref()
                    .is_some_and(|configured| configured.eq_ignore_ascii_case(address))
            })
        {
            return safe_label(&connection.name);
        }
        abbreviate_address(address)
    }

    fn settlement_policy(&self, source_chain_id: u64, destination_chain_id: Option<u64>) -> Value {
        if source_chain_id == 792703809 {
            let mut policy = settlement_policy_from_rules(
                &self.confirmation_rules,
                destination_chain_id.unwrap_or(1),
                destination_chain_id.unwrap_or(1),
            );
            policy["source"] = json!({"network":gofin_solana::NETWORK,"commitment":"finalized"});
            return policy;
        }
        let Some(destination_chain_id) = destination_chain_id else {
            let mut policy = settlement_policy_from_rules(
                &self.confirmation_rules,
                source_chain_id,
                source_chain_id,
            );
            policy["destination"] =
                json!({"network":gofin_solana::NETWORK,"commitment":"finalized"});
            return policy;
        };
        settlement_policy_from_rules(
            &self.confirmation_rules,
            source_chain_id,
            destination_chain_id,
        )
    }
}

fn bridge_provider_label(provider: &str) -> &'static str {
    match provider {
        "relay" => "Relay",
        "arbitrum_canonical" => "Canonical Inbox",
        "base_canonical" => "Base canonical bridge",
        "cctp" => "Circle CCTP V2",
        _ => "reviewed bridge provider",
    }
}

fn bridge_settlement_status_summary(transfer: &BridgeTransferRecord) -> Option<Value> {
    let proof = transfer.settlement_proofs.last()?;
    let mut proof_identity = serde_json::Map::new();
    for key in [
        "chain_id",
        "network",
        "transaction_hash",
        "proof_method",
        "slot",
    ] {
        if let Some(value) = proof.evidence.get(key)
            && (value.is_string() || value.is_number())
        {
            proof_identity.insert(key.to_owned(), value.clone());
        }
    }
    for key in ["native_costs", "native_cost_accounting"] {
        if let Some(value) = proof.evidence.get(key) {
            proof_identity.insert(key.into(), value.clone());
        }
    }
    if let Some(gas) = proof.evidence.get("native_gas_observation")
        && gas.is_object()
    {
        proof_identity.insert("native_gas_observation".into(), gas.clone());
    }
    if let Some(partial) = proof.evidence.get("partial_payout_reconciled")
        && partial.is_boolean()
    {
        proof_identity.insert("partial_payout_reconciled".into(), partial.clone());
    }
    let mut cash_credits = Vec::new();
    let mut costs = Vec::new();
    for effect in &proof.effects {
        match effect {
            gofin_domain::FinancialEffect::UsableCredit { to, amount } => {
                cash_credits.push(json!({
                    "resource":to.0,
                    "asset":amount.asset,
                    "amount_atoms":amount.atoms,
                }));
            }
            gofin_domain::FinancialEffect::Cost {
                location,
                amount,
                category,
                treatment,
            } => costs.push(json!({
                "location":location,
                "asset":amount.asset,
                "amount_atoms":amount.atoms,
                "category":category,
                "treatment":treatment,
            })),
            _ => {}
        }
    }
    Some(json!({
        "outcome":match proof.outcome {
            gofin_domain::BridgeSettlementOutcome::Destination => "destination",
            gofin_domain::BridgeSettlementOutcome::Refund => "refund",
        },
        "evidence_id":proof.evidence_id,
        "observed_at":proof.observed_at,
        "cash_credits":cash_credits,
        "costs":costs,
        "proof":proof_identity,
    }))
}

fn bridge_execute_source_evidence(
    transfer: &BridgeTransferRecord,
    operations: &[gofin_domain::OperationRecord],
) -> Option<(GoFinAmount, BridgeQuote)> {
    let deposit = transfer
        .steps
        .iter()
        .find(|link| link.kind == BridgeStepKind::Deposit && link.operation_id.is_some())?;
    let operation_id = deposit.operation_id.as_deref()?;
    let operation = operations
        .iter()
        .find(|operation| operation.id == operation_id)?;
    if operation.operation != "bridge.execute" || operation.status != Status::Completed {
        return None;
    }
    let preview = operation.preview.as_ref()?;
    if preview.binding["transfer_id"] != transfer.id
        || preview.binding["step_id"] != deposit.step_id
        || preview.binding["step_kind"] != "deposit"
    {
        return None;
    }
    let quote: BridgeQuote = serde_json::from_value(preview.binding["quote"].clone()).ok()?;
    let effects = &operation.effects.as_ref()?.effects;
    let principal = effects.iter().find_map(|effect| match effect {
        FinancialEffect::TransitPrincipal {
            transfer_id,
            amount,
        } if transfer_id == &transfer.id => Some(amount),
        _ => None,
    })?;
    let source_resource = preview
        .intent
        .debits
        .iter()
        .find(|debit| debit.amount == *principal)?
        .resource
        .clone();
    effects
        .iter()
        .any(|effect| {
            matches!(
                effect,
                FinancialEffect::CashDebit { resource, amount }
                    if resource == &source_resource && amount == principal
            )
        })
        .then(|| (principal.clone(), quote))
}

fn relay_native_withheld_cost(
    transfer: &BridgeTransferRecord,
    quote: &BridgeQuote,
    capabilities: &gofin_bridge::BridgeCapabilities,
    source_principal: &GoFinAmount,
    destination_asset: &gofin_domain::AssetId,
    destination_atoms: &Quantity,
) -> gofin_domain::Result<Option<CostBound>> {
    let Some(source) = quote.intent.source.as_ref() else {
        return Ok(None);
    };
    let [destination] = quote.intent.destination_assets.as_slice() else {
        return Ok(None);
    };
    let source_asset = source.asset_id();
    let destination_identity = destination.asset_id();
    let destination_chain_id = quote.intent.evm_destination_chain_id()?;
    let supported_native_eth_route = quote.provider == "relay"
        && capabilities.provider == "relay"
        && capabilities.native_eth
        && capabilities.chains.contains(&source.chain_id)
        && capabilities.chains.contains(&destination_chain_id)
        && source.chain_id != destination_chain_id
        && source.token.is_none()
        && destination.token.is_none()
        && source.decimals == 18
        && destination.decimals == 18
        && source_asset.reference == "native"
        && destination_identity.reference == "native"
        && destination.chain_id == destination_chain_id
        && source_asset.network == format!("ethereum:{}", source.chain_id)
        && destination_identity.network == format!("ethereum:{destination_chain_id}")
        && quote.gross_debit.asset == source_asset
        && quote.expected_receipt.asset == destination_identity
        && *destination_asset == destination_identity
        && source_principal.asset == source_asset
        && source_principal.atoms == quote.gross_debit.atoms;
    if !supported_native_eth_route {
        return Ok(None);
    }

    let quoted_source_withheld = quote
        .fees
        .iter()
        .filter(|fee| {
            fee.kind == gofin_bridge::FeeKind::WithheldFromOutput
                && fee.amount.asset == source_asset
        })
        .try_fold(Quantity::zero(), |total, fee| {
            total.checked_add(&fee.amount.atoms)
        })?;
    if quoted_source_withheld.is_zero() {
        return Ok(None);
    }

    // This cross-network comparison is limited to native ETH on chains that
    // the selected Relay provider reports as reviewed native-ETH networks.
    // The fee is observed from proved principal minus the finalized payout;
    // the provider's quote only identifies the fee asset and category, never
    // its realized amount.
    let principal = source_principal.atoms.integer();
    let payout = destination_atoms.integer();
    if payout >= principal {
        return Ok(None);
    }
    let realized = Quantity::new((principal - payout).to_string())?;

    let Some(raw_bounds) = transfer.approved_bounds.as_ref() else {
        return Ok(None);
    };
    let bounds: BridgeBounds = serde_json::from_value(raw_bounds.clone()).map_err(|_| {
        error(
            "BRIDGE_BOUNDS_INVALID",
            "reconciliation",
            "The frozen bridge fee bounds are malformed.",
            "Keep the transfer unresolved and inspect the original approved bounds.",
        )
    })?;
    let Some(cap) = bounds.fee_caps.iter().find(|cap| {
        cap.asset == source_asset && cap.kind == gofin_bridge::FeeKind::WithheldFromOutput
    }) else {
        return Ok(None);
    };
    if realized > cap.maximum {
        return Err(error(
            "BRIDGE_WITHHELD_FEE_CAP_EXCEEDED",
            "reconciliation",
            "The proved native-ETH payout implies a source-denominated withheld fee above its frozen cap.",
            "Keep the verified proof and transfer unresolved for explicit recovery; do not submit another bridge route.",
        ));
    }
    Ok(Some(CostBound {
        location: AccountingLocation::Cash {
            resource: quote.intent.wallet.clone(),
        },
        amount: GoFinAmount {
            asset: source_asset,
            atoms: realized,
        },
        treatment: CostTreatment::IncludedInDebit,
    }))
}

fn bridge_confirmation_rules(mut rules: BTreeMap<u64, (u64, bool)>) -> BTreeMap<u64, (u64, bool)> {
    // The human-selected rollup completion policy also applies to historical
    // bridge records and observer-only Base transactions. Mainnet and the
    // separately reviewed settlement hub retain their configured policies.
    for chain_id in [42161, 4663, 8453] {
        rules.insert(chain_id, (1, false));
    }
    rules.entry(537713).or_insert((2, true));
    rules
}

async fn canonical_candidate_progress(
    runtime: &gofin_evm::EvmRuntime,
    transaction_hash: &str,
    minimum_confirmations: u64,
    require_finalized: bool,
) -> Value {
    let unavailable = |receipt_observed, receipt_matches_candidate, candidate_block| {
        canonical_progress_observation(
            "observation_unavailable",
            receipt_observed,
            receipt_matches_candidate,
            candidate_block,
            None,
            None,
            None,
            minimum_confirmations,
            require_finalized,
        )
    };
    let receipt = match runtime
        .rpc
        .request("eth_getTransactionReceipt", json!([transaction_hash]))
        .await
    {
        Ok(receipt) if receipt.is_null() => {
            return canonical_progress_observation(
                "receipt_unavailable",
                Some(false),
                Some(false),
                None,
                None,
                None,
                None,
                minimum_confirmations,
                require_finalized,
            );
        }
        Ok(receipt) => receipt,
        Err(_) => return unavailable(None, None, None),
    };
    let candidate_hash_matches = canonical_receipt_matches_candidate(&receipt, transaction_hash);
    let Some(candidate_block_number) = receipt
        .get("blockNumber")
        .and_then(|number| gofin_evm::rpc_u64(number).ok())
    else {
        return unavailable(Some(true), Some(candidate_hash_matches), None);
    };
    if !candidate_hash_matches {
        return unavailable(Some(true), Some(false), Some(candidate_block_number));
    }
    let Some(candidate_block_hash) = receipt
        .get("blockHash")
        .and_then(Value::as_str)
        .and_then(|hash| gofin_evm::hash(hash).ok())
    else {
        return unavailable(Some(true), Some(true), Some(candidate_block_number));
    };
    let latest_block_number = match runtime
        .rpc
        .request("eth_blockNumber", json!([]))
        .await
        .and_then(|number| gofin_evm::rpc_u64(&number))
    {
        Ok(number) => number,
        Err(_) => {
            return unavailable(Some(true), Some(true), Some(candidate_block_number));
        }
    };
    let current_confirmations = if latest_block_number >= candidate_block_number {
        latest_block_number - candidate_block_number + 1
    } else {
        0
    };
    let finalized_head_block_number = match runtime
        .rpc
        .request("eth_getBlockByNumber", json!(["finalized", false]))
        .await
    {
        Ok(block) if !block.is_null() => gofin_evm::rpc_u64(&block["number"]).ok(),
        _ => None,
    };
    let canonical_block_hash = runtime
        .rpc
        .request(
            "eth_getBlockByNumber",
            json!([format!("{candidate_block_number:#x}"), false]),
        )
        .await
        .ok()
        .filter(|block| !block.is_null())
        .and_then(|block| {
            block
                .get("hash")
                .and_then(Value::as_str)
                .and_then(|hash| gofin_evm::hash(hash).ok())
        });
    let canonical_inclusion_matches = canonical_block_hash == Some(candidate_block_hash);
    let phase = canonical_progress_phase(
        canonical_inclusion_matches,
        current_confirmations,
        minimum_confirmations,
        finalized_head_block_number,
        candidate_block_number,
        require_finalized,
    );
    canonical_progress_observation(
        phase,
        Some(true),
        Some(true),
        Some(candidate_block_number),
        Some(latest_block_number),
        Some(current_confirmations),
        finalized_head_block_number,
        minimum_confirmations,
        require_finalized,
    )
}

fn canonical_receipt_matches_candidate(receipt: &Value, transaction_hash: &str) -> bool {
    receipt
        .get("transactionHash")
        .and_then(Value::as_str)
        .and_then(|hash| gofin_evm::hash(hash).ok())
        .zip(gofin_evm::hash(transaction_hash).ok())
        .is_some_and(|(observed, expected)| observed == expected)
}

#[expect(clippy::too_many_arguments)]
fn canonical_progress_observation(
    phase: &str,
    receipt_observed: Option<bool>,
    receipt_matches_candidate: Option<bool>,
    candidate_block_number: Option<u64>,
    latest_block_number: Option<u64>,
    current_confirmations: Option<u64>,
    finalized_head_block_number: Option<u64>,
    minimum_confirmations: u64,
    require_finalized: bool,
) -> Value {
    json!({
        "phase": phase,
        "receipt_observed": receipt_observed,
        "receipt_matches_candidate": receipt_matches_candidate,
        "candidate_block_number": candidate_block_number,
        "latest_block_number": latest_block_number,
        "current_confirmations": current_confirmations,
        "finalized_head_block_number": finalized_head_block_number,
        "minimum_confirmations": minimum_confirmations,
        "require_finalized": require_finalized,
    })
}

fn canonical_progress_phase(
    canonical_inclusion_matches: bool,
    current_confirmations: u64,
    minimum_confirmations: u64,
    finalized_head_block_number: Option<u64>,
    candidate_block_number: u64,
    require_finalized: bool,
) -> &'static str {
    if !canonical_inclusion_matches {
        "canonical_inclusion_wait"
    } else if current_confirmations < minimum_confirmations {
        "confirmation_wait"
    } else if require_finalized && finalized_head_block_number.is_none() {
        "observation_unavailable"
    } else if require_finalized
        && finalized_head_block_number.is_some_and(|head| head < candidate_block_number)
    {
        "finality_wait"
    } else {
        // The primary observer returned None, so a contradictory ready state
        // is reported as unavailable rather than inferred to be settled.
        "observation_unavailable"
    }
}

fn settlement_policy_from_rules(
    rules: &BTreeMap<u64, (u64, bool)>,
    source_chain_id: u64,
    destination_chain_id: u64,
) -> Value {
    let (source_confirmations, source_finalized) =
        rules.get(&source_chain_id).copied().unwrap_or((2, true));
    let (destination_confirmations, destination_finalized) = rules
        .get(&destination_chain_id)
        .copied()
        .unwrap_or((2, true));
    json!({
        "source": {
            "chain_id": source_chain_id,
            "min_confirmations": source_confirmations,
            "require_finalized": source_finalized,
        },
        "destination": {
            "chain_id": destination_chain_id,
            "min_confirmations": destination_confirmations,
            "require_finalized": destination_finalized,
        },
        "arrival_estimate_scope": "Provider arrival estimates describe route timing only; they do not predict or replace GoFin receipt confirmation and settlement verification."
    })
}

#[async_trait]
impl Adapter for BridgeAdapter {
    fn uses_rpc_reads(&self, operation: &str, _input: &Value) -> bool {
        matches!(
            operation,
            "bridge.quotes" | "bridge.status" | "bridge.recovery" | "bridge.transfer"
        )
    }
    fn plan_step_allowance(
        &self,
        operation: &str,
        input: &Value,
    ) -> gofin_domain::Result<Option<u64>> {
        if operation == "bridge.execute" {
            let scope: gofin_bridge::BridgeExecuteIntentInput =
                serde_json::from_value(input.clone()).map_err(|_| {
                    error(
                        "INVALID_INPUT",
                        "authorization",
                        "Provider-neutral bridge intent is malformed.",
                        "Use the bridge.execute intent schema with an explicit provider set and bounds.",
                    )
                })?;
            if scope.allowed_providers.is_empty() {
                return Err(error(
                    "BRIDGE_PROVIDER_SCOPE_REQUIRED",
                    "authorization",
                    "A bridge intent must name at least one permitted provider.",
                    "List the reviewed provider IDs in allowedProviders.",
                ));
            }
            return Ok(Some(
                gofin_domain::plan_timing::WITHDRAWAL_ALLOWANCE_SECONDS,
            ));
        }
        if operation != "bridge.transfer" {
            let definition = self
                .definitions()
                .into_iter()
                .find(|d| d.name == operation)
                .ok_or_else(|| GoFinError::validation("Unknown bridge plan operation."))?;
            return gofin_domain::plan_timing::definition_allowance(&definition, input);
        }
        let requested = self.transfer_input(input)?;
        let record = self
            .state()?
            .transfer(&requested.transfer_id)
            .map_err(store_error)?
            .ok_or_else(|| {
                error(
                    "BRIDGE_TRANSFER_UNKNOWN",
                    "authorization",
                    "Saved bridge route is missing.",
                    "Obtain a bridge quote before plan review.",
                )
            })?;
        let saved = self.saved_quote(&record)?;
        if requested.step_kind == BridgeStepKind::Mint {
            return Ok(Some(gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS));
        }
        if requested.step_kind.is_destination_stage()
            || (saved.quote.provider == "base_canonical"
                && saved.quote.intent.evm_source()?.chain_id == 8453)
        {
            return Ok(None);
        }
        Ok(Some(
            if matches!(
                requested.step_kind,
                BridgeStepKind::Approval | BridgeStepKind::ResetAllowance
            ) {
                gofin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS
            } else {
                gofin_domain::plan_timing::WITHDRAWAL_ALLOWANCE_SECONDS
                    .max(saved.quote.estimated_arrival_seconds.unwrap_or(0))
            },
        ))
    }

    fn definitions(&self) -> Vec<Definition> {
        Self::operation_definitions()
    }

    fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        Vec::new()
    }

    fn diagnostics(&self) -> Value {
        // Canonical providers and settlement observers share these clients.
        // Wallet runtimes also cover source transports supplied independently
        // through the fixture/custom-provider composition entrypoint.
        let snapshots = self
            .observers
            .values()
            .map(|runtime| runtime.rpc.diagnostics())
            .chain(
                self.wallet_adapters
                    .values()
                    .map(|adapter| adapter.diagnostics()),
            )
            .filter(|snapshot| !snapshot.is_null())
            .collect::<Vec<_>>();
        json!(snapshots)
    }

    fn snapshot_is_global(&self) -> bool {
        true
    }

    async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        let transfers = self.state()?.transfers().map_err(store_error)?;
        let pending = transfers
            .iter()
            .filter(|transfer| {
                !matches!(
                    transfer.state,
                    BridgeTransferState::Completed
                        | BridgeTransferState::Refunded
                        | BridgeTransferState::Failed
                ) && (transfer.transit.is_some()
                    || transfer
                        .steps
                        .iter()
                        .any(|step| step.operation_id.is_some()))
            })
            .map(|transfer| format!("bridge transfer {} is {:?}", transfer.id, transfer.state))
            .collect::<Vec<_>>();
        Ok(StateSnapshot { state: gofin_domain::FinancialState::default(), observed_at: self.clock.now().to_rfc3339(), complete: pending.is_empty(), coverage: vec!["EVM wallet snapshots are reported by the EVM network adapter; this observer reports active submitted bridge steps and in-transit principal. Unused quote options are not pending financial operations.".into()], pending })
    }

    async fn read(&self, operation: &str, input: &Value) -> gofin_domain::Result<Value> {
        if operation == "bridge.recovery" {
            return self.bridge_recovery(input).await;
        }
        if operation == "bridge.routes" {
            return self.routes(input).await;
        }
        if operation == "bridge.quotes" {
            return self.quote(input).await;
        }
        if operation == "bridge.status" {
            return self.status(input).await;
        }
        Err(GoFinError::new(
            "OPERATION_UNAVAILABLE",
            "read",
            "This bridge read operation is unavailable.",
            "Inspect bridge.quotes and gofin status.",
        ))
    }

    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> gofin_domain::Result<PreparedOperation> {
        use gofin_domain::evidence_phase::{Phase, during};
        if operation != "bridge.transfer" && operation != "bridge.execute" {
            return Err(GoFinError::new(
                "OPERATION_UNAVAILABLE",
                "preview",
                "This bridge write operation is unavailable.",
                "Use bridge.transfer for one exact persisted step.",
            ));
        }
        if operation == "bridge.execute" && input.get("intent").is_some() {
            return Err(error(
                "BRIDGE_EXECUTION_CHILD_REQUIRED",
                "validation",
                "This is the reviewed bridge intent, not an executable saved quote child. Nothing was submitted.",
                "Read bridge.quotes for the approved intent, select a saved child, then execute bridge.execute with its transferId, stepId and stepKind plus the exact reviewed bounds under the same plan. Inspect bridge.execute discovery for its execution input; no new approval is required for a child within active authority.",
            ));
        }
        if operation == "bridge.execute" && self.transfer_input(input)?.bounds.is_none() {
            return Err(error(
                "BRIDGE_BOUNDS_REQUIRED",
                "authorization",
                "A nominated bridge child must carry the exact bounds approved with its logical intent.",
                "Include the reviewed bounds when nominating a saved bridge child.",
            ));
        }
        let (record, saved, step, link, requested_bounds) = self.step_context(input)?;
        let (mut record, mut saved, mut step, link, requested_bounds) = if self
            .refresh_expired_source_quote(&record, &saved, &link)
            .await?
        {
            self.step_context(input)?
        } else {
            (record, saved, step, link, requested_bounds)
        };
        if step.solana_transaction.is_some() {
            return self
                .prepare_relay_solana_source(input, &record, &saved, step, &link, requested_bounds)
                .await;
        }
        if link.kind == BridgeStepKind::Mint {
            return self
                .prepare_cctp_stage(input, &record, &saved, step, &link, requested_bounds)
                .await;
        }
        if link.kind.is_destination_stage() {
            return self
                .prepare_base_stage(operation, input, &record, &saved, step, requested_bounds)
                .await;
        }
        if saved.quote.class != gofin_bridge::QuoteClass::Executable {
            return Err(error(
                "BRIDGE_QUOTE_NOT_EXECUTABLE",
                "preview",
                "This Relay quote is indicative and cannot authorize a wallet write.",
                "Request a fresh executable quote.",
            ));
        }
        let selected_bounds = if let Some(approved) = &record.approved_bounds {
            let approved: BridgeBounds =
                serde_json::from_value(approved.clone()).map_err(|_| {
                    error(
                        "BRIDGE_APPROVED_BOUNDS_CORRUPT",
                        "reconciliation",
                        "The transfer's approved bounds are malformed.",
                        "Keep this transfer unresolved and inspect its protected journal.",
                    )
                })?;
            if let Some(requested) = requested_bounds
                && normalize_authorization_bounds(Some(requested), &saved)? != approved
            {
                return Err(error(
                    "BRIDGE_BOUNDS_ALREADY_APPROVED",
                    "authorization",
                    "This transfer already has immutable approved bounds.",
                    "Continue with those exact bounds or start a new transfer for a changed intent.",
                ));
            }
            approved
        } else {
            normalize_authorization_bounds(requested_bounds, &saved)?
        };
        saved.bounds = selected_bounds.clone();
        if saved.quote.intent.evm_source()?.token.is_some()
            && saved.quote.provider != "base_canonical"
            && record.approved_bounds.is_none()
            && record.grant_id.is_none()
        {
            let current = during(
                Phase::BridgeAllowance,
                self.current_allowance(&saved.quote, &saved.quote.intent.source_address),
            )
            .await?;
            let cap = gofin_evm::u256(
                selected_bounds
                    .maximum_source_debit
                    .as_ref()
                    .expect("normalized source debit cap"),
            )?;
            let desired = if current >= cap {
                vec![ProviderStepKind::Deposit]
            } else if !current.is_zero() {
                vec![
                    ProviderStepKind::ResetAllowance,
                    ProviderStepKind::Approval,
                    ProviderStepKind::Deposit,
                ]
            } else {
                vec![ProviderStepKind::Approval, ProviderStepKind::Deposit]
            };
            let actual = saved
                .plan
                .steps
                .iter()
                .filter(|step| step.kind != ProviderStepKind::Mint)
                .map(|step| step.kind)
                .collect::<Vec<_>>();
            if actual != desired {
                return Err(error(
                    "BRIDGE_PLAN_REFRESH_REQUIRED",
                    "preview",
                    "Selected input bounds require a different finite allowance step plan than this quote declared.",
                    "Request a new bridge quote with these exact bounds before authorizing any step.",
                ));
            }
        }
        let expectation = settlement_expectation(
            &saved.quote,
            selected_bounds
                .minimum_net_receipt
                .as_ref()
                .expect("normalized receipt floor")
                .atoms
                .clone(),
        );
        let deposit_step_ids = record
            .steps
            .iter()
            .filter(|candidate| candidate.kind == BridgeStepKind::Deposit)
            .map(|candidate| candidate.step_id.clone())
            .collect::<Vec<_>>();
        if deposit_step_ids.len() != 1 {
            return Err(error(
                "BRIDGE_TRANSFER_CORRUPT",
                "reconciliation",
                "The saved transfer does not contain exactly one source deposit step.",
                "Keep the transfer unresolved and inspect its protected journal.",
            ));
        }
        let selected_prospective_debit = BridgeProspectiveDebit {
            deposit_step_id: deposit_step_ids[0].clone(),
            resource: saved.quote.intent.wallet.clone(),
            maximum: GoFinAmount {
                asset: saved.quote.gross_debit.asset.clone(),
                atoms: selected_bounds
                    .maximum_source_debit
                    .as_ref()
                    .expect("normalized source debit cap")
                    .clone(),
            },
        };
        let claimed_exact_deposit = record.grant_id.is_none()
            && record.approved_bounds.is_none()
            && link.kind == BridgeStepKind::Deposit
            && link.state == BridgeStepState::Submitted
            && link.operation_id.is_some();
        let prospective_debit = if claimed_exact_deposit {
            let existing = record.prospective_debit.as_ref().ok_or_else(|| {
                error(
                    "BRIDGE_TRANSFER_CORRUPT",
                    "reconciliation",
                    "The claimed source deposit lacks its durable prospective debit scope.",
                    "Keep the transfer unresolved and inspect its protected journal.",
                )
            })?;
            if existing.deposit_step_id != selected_prospective_debit.deposit_step_id
                || existing.resource != selected_prospective_debit.resource
                || existing.maximum.asset != selected_prospective_debit.maximum.asset
                || existing.maximum.atoms < saved.quote.gross_debit.atoms
            {
                return Err(error(
                    "BRIDGE_PROSPECTIVE_DEBIT_MISMATCH",
                    "authorization",
                    "The claimed exact deposit exceeds its durable source-debit scope.",
                    "Keep this transfer unresolved and request a new reviewed quote.",
                ));
            }
            existing.clone()
        } else {
            selected_prospective_debit.clone()
        };
        if record.prospective_debit.as_ref() != Some(&prospective_debit) {
            if record.grant_id.is_some() || record.approved_bounds.is_some() {
                return Err(error(
                    "BRIDGE_PROSPECTIVE_DEBIT_MISMATCH",
                    "authorization",
                    "The authorized transfer has a different finite source-debit scope.",
                    "Keep this transfer unresolved and request a new reviewed quote for a changed source cap.",
                ));
            }
            let claimed_steps = record.steps.iter().any(|planned| {
                planned.operation_id.is_some() || planned.state != BridgeStepState::Unsubmitted
            });
            let safe_reset_update = prospective_debit_update_before_source_approval(&record, &link);
            if claimed_steps && !safe_reset_update {
                return Err(error(
                    "BRIDGE_PROSPECTIVE_DEBIT_FROZEN",
                    "authorization",
                    "The source-debit scope cannot change after a transfer step is claimed.",
                    "Continue with the already prepared scope or create a new transfer.",
                ));
            }
            record.prospective_debit = Some(selected_prospective_debit.clone());
            record.version = record.version.checked_add(1).ok_or_else(internal_error)?;
            self.state()?.save_transfer(&record).map_err(store_error)?;
        }
        if record.approved_bounds.is_some()
            && record.settlement_expectation.as_ref() != Some(&expectation)
        {
            return Err(error(
                "BRIDGE_SETTLEMENT_SCOPE_MISMATCH",
                "reconciliation",
                "Saved settlement targets differ from the approved bridge receipt scope.",
                "Keep the transfer unresolved and inspect its protected journal.",
            ));
        }
        // These independent read-only checks enforce the same frozen bounds.
        // Overlap their price observations within the engine's preflight budget.
        tokio::try_join!(
            during(
                Phase::BridgeQuotePricing,
                self.validate_quote_prices(&selected_bounds, &saved.quote)
            ),
            during(
                Phase::BridgeCapPricing,
                self.validate_selected_usd_caps(
                    &selected_bounds,
                    saved.quote.intent.evm_source()?,
                    &saved.quote.intent.wallet,
                )
            ),
        )?;
        let wallet_alias = record.metadata["wallet_alias"].as_str().ok_or_else(|| {
            error(
                "BRIDGE_WALLET_IDENTITY_MISSING",
                "reconciliation",
                "Persisted wallet alias is missing.",
                "Keep the transfer unresolved and inspect its protected journal.",
            )
        })?;
        let wallet = self.wallet(wallet_alias, saved.quote.intent.evm_source()?.chain_id)?;
        let sender = wallet
            .address
            .as_deref()
            .ok_or_else(|| GoFinError::validation("Configured wallet address is missing."))?;
        // Canonical's fixed digest includes bounds, while its saved quote plan
        // may contain only read-time hint bounds. Recompute just that provider
        // digest under the bounds selected for this attempt; the engine binds
        // those exact bounds separately. Relay's digest already excludes the
        // mutable quantity and its raw provider steps need allowance-aware
        // normalization, so retain its persisted plan digest here.
        if saved.quote.provider == "arbitrum_canonical" || saved.quote.provider == "base_canonical"
        {
            let provider = self.provider(&saved.quote.provider)?;
            let selected_plan = provider.prepare(&saved.quote, &selected_bounds, sender)?;
            if !same_bridge_step_topology(&saved.plan.steps, &selected_plan.steps) {
                return Err(error(
                    "BRIDGE_PLAN_SCOPE_CHANGED",
                    "preview",
                    "Selected bounds changed the provider's declared bridge step topology.",
                    "Request a new quote with these bounds before authorizing a write.",
                ));
            }
            saved.plan.fixed_scope_digest = selected_plan.fixed_scope_digest;
        }
        if saved.quote.provider == "cctp" {
            saved.plan.fixed_scope_digest = self
                .provider("cctp")?
                .prepare(&saved.quote, &selected_bounds, sender)?
                .fixed_scope_digest;
            self.validate_cctp_gas(&saved.quote, &selected_bounds, wallet_alias, &record)
                .await?;
        }
        if matches!(step.kind, ProviderStepKind::Approval) {
            let approval = saved.quote.approval.as_ref().ok_or_else(|| {
                GoFinError::validation("Bridge quote lacks the exact finite allowance identity.")
            })?;
            let amount = gofin_evm::u256(
                selected_bounds
                    .maximum_source_debit
                    .as_ref()
                    .expect("normalized source debit cap"),
            )?;
            let replacement = format!(
                "0x{}",
                hex::encode(
                    BridgeErc20::approveCall {
                        spender: gofin_evm::address(&approval.spender)?,
                        amount,
                    }
                    .abi_encode()
                )
            );
            if step.evm_call()?.data != replacement {
                step.evm_call_mut()?.data = replacement.clone();
                let plan_step = saved
                    .plan
                    .steps
                    .iter_mut()
                    .find(|candidate| candidate.ordinal == step.ordinal)
                    .ok_or_else(internal_error)?;
                plan_step.evm_call_mut()?.data = replacement;
                let latest = record.quote_revisions.last().ok_or_else(internal_error)?;
                let mut payload = latest.payload.clone();
                payload["plan"] =
                    serde_json::to_value(&saved.plan).map_err(|_| internal_error())?;
                let revision = BridgeQuoteRevision {
                    revision: record.quote_revisions.len() as u32 + 1,
                    provider: latest.provider.clone(),
                    route_id: latest.route_id.clone(),
                    order_id: latest.order_id.clone(),
                    quote_hash: binding_hash(&payload)?,
                    payload,
                    observed_at: latest.observed_at.clone(),
                    valid_until: latest.valid_until.clone(),
                };
                self.state()?
                    .append_quote_revision(&record.id, &revision)
                    .map_err(store_error)?;
            }
        }
        if !step.expected_sender.eq_ignore_ascii_case(sender) {
            return Err(error(
                "BRIDGE_SIGNER_MISMATCH",
                "validation",
                "Provider step sender differs from the broker-resolved wallet.",
                "Discard this quote and review the configured wallet identity.",
            ));
        }
        let runtime = self
            .wallet_adapters
            .get(&step.evm_call()?.chain_id)
            .ok_or_else(|| {
                error(
                    "BRIDGE_NETWORK_UNAVAILABLE",
                    "preview",
                    "This bridge step has no configured signing runtime.",
                    "Choose a wallet network enabled for bridging.",
                )
            })?
            .runtime
            .clone();
        if !wallet.signing_enabled || !runtime.can_sign() {
            return Err(error(
                "BRIDGE_SIGNER_UNAVAILABLE",
                "preview",
                "The selected source wallet is read-only or has no protected signer loaded.",
                "Select an enrolled signing wallet before approving a bridge step.",
            ));
        }
        if saved.quote.provider == "arbitrum_canonical" {
            require_empty_code(&runtime, &saved.quote.intent.source_address, None).await?;
        }
        let target = gofin_evm::address(&step.evm_call()?.to)?;
        let source_token = saved
            .quote
            .intent
            .evm_source()?
            .token
            .as_deref()
            .map(gofin_evm::address)
            .transpose()?;
        // An allowance child targets the source token itself. Observe that exact
        // deployment once; distinct deposit targets still require both checks.
        during(Phase::BridgeDeployments, async {
            tokio::try_join!(runtime.code(target), async {
                if let Some(token) = source_token
                    && token != target
                {
                    runtime.code(token).await?;
                }
                Ok::<_, GoFinError>(())
            })
        })
        .await?;
        if let Some(token) = &saved.quote.intent.evm_source()?.token
            && matches!(step.kind, ProviderStepKind::Deposit)
        {
            let spender = saved
                .quote
                .approval
                .as_ref()
                .map(|approval| approval.spender.as_str())
                .unwrap_or(RELAY_DEPOSITORY);
            let allowance = during(
                Phase::BridgeAllowance,
                runtime.call(
                    gofin_evm::address(token)?,
                    &BridgeErc20::allowanceCall {
                        owner: gofin_evm::address(sender)?,
                        spender: gofin_evm::address(spender)?,
                    },
                    None,
                ),
            )
            .await?;
            if allowance < gofin_evm::u256(&saved.quote.gross_debit.atoms)? {
                return Err(error(
                    "BRIDGE_ALLOWANCE_INSUFFICIENT",
                    "preview",
                    "Current token allowance no longer covers the exact source deposit.",
                    "Request a new quote and finite approval plan; no deposit was submitted.",
                ));
            }
        }
        if matches!(
            step.kind,
            ProviderStepKind::Approval | ProviderStepKind::ResetAllowance
        ) {
            let approval = saved.quote.approval.as_ref().ok_or_else(|| {
                GoFinError::validation("Bridge quote lacks the exact ERC-20 allowance identity.")
            })?;
            let amount = if matches!(step.kind, ProviderStepKind::ResetAllowance) {
                gofin_evm::alloy::primitives::U256::ZERO
            } else {
                gofin_evm::u256(
                    selected_bounds
                        .maximum_source_debit
                        .as_ref()
                        .expect("normalized source debit cap"),
                )?
            };
            let simulated = during(
                Phase::BridgeApprovalSimulation,
                runtime.call_from(
                    gofin_evm::address(sender)?,
                    gofin_evm::address(&step.evm_call()?.to)?,
                    &BridgeErc20::approveCall {
                        spender: gofin_evm::address(&approval.spender)?,
                        amount,
                    },
                    None,
                ),
            )
            .await?;
            if !simulated {
                return Err(error(
                    "BRIDGE_APPROVAL_SIMULATION_FAILED",
                    "preview",
                    "The exact ERC-20 approval call did not return true in simulation.",
                    "Do not approve this route; choose another quote or inspect token behavior.",
                ));
            }
        }
        let maximum_fee = maximum_gas_cap(&saved.bounds, step.evm_call()?.chain_id)?;
        let call = gofin_evm::PreparedCall {
            transaction: during(
                Phase::BridgeTransactionPreparation,
                runtime.prepare(
                    gofin_evm::address(sender)?,
                    gofin_evm::address(&step.evm_call()?.to)?,
                    gofin_evm::u256(&step.evm_call()?.value)?,
                    gofin_evm::rpc_bytes(&json!(step.evm_call()?.data))?,
                    gofin_evm::u256(&maximum_fee)?,
                ),
            )
            .await?
            .transaction,
            simulation: Default::default(),
        };
        let transaction = call.transaction;
        let fee = transaction.maximum_cost()?;
        self.validate_transfer_gas_budget(
            &record,
            &link.step_id,
            &selected_bounds,
            &EvmAsset {
                chain_id: step.evm_call()?.chain_id,
                token: None,
                decimals: 18,
            }
            .asset_id(),
            fee,
        )?;
        let source_asset = saved.quote.intent.evm_source()?.asset_id();
        let resource = wallet.resource.clone();
        let mut debits = step
            .maximum_debit
            .iter()
            .map(|amount| ResourceAmount {
                resource: resource.clone(),
                amount: amount.clone(),
            })
            .collect::<Vec<_>>();
        if matches!(step.kind, ProviderStepKind::Deposit) && source_asset.reference == "native" {
            let step_amount = step
                .maximum_debit
                .iter()
                .find(|amount| amount.asset == source_asset)
                .map(|amount| amount.atoms.clone())
                .unwrap_or_else(|| saved.quote.gross_debit.atoms.clone());
            if !debits
                .iter()
                .any(|debit| debit.amount.asset == source_asset)
            {
                debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: GoFinAmount {
                        asset: source_asset.clone(),
                        atoms: step_amount,
                    },
                });
            }
        }
        let gas_asset = gofin_domain::AssetId {
            network: format!("ethereum:{}", step.evm_call()?.chain_id),
            reference: "native".into(),
            decimals: 18,
        };
        let cost = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: GoFinAmount {
                asset: gas_asset.clone(),
                atoms: gofin_evm::quantity(fee),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut allowance_transition = None;
        let expected = match step.kind {
            ProviderStepKind::Approval | ProviderStepKind::ResetAllowance => {
                vec![EffectConstraint::Authority {
                    resource: resource.clone(),
                    authority: {
                        let token = saved
                            .quote
                            .intent
                            .evm_source()?
                            .token
                            .as_deref()
                            .ok_or_else(|| {
                                GoFinError::validation(
                                    "Native ETH cannot require ERC-20 allowance.",
                                )
                            })?;
                        let approval = saved.quote.approval.as_ref().ok_or_else(|| {
                            GoFinError::validation("Bridge quote lacks an exact approval identity.")
                        })?;
                        format!(
                            "erc20_allowance:{}:{}",
                            token.to_ascii_lowercase(),
                            approval.spender.to_ascii_lowercase()
                        )
                    },
                    before: {
                        let token = saved
                            .quote
                            .intent
                            .evm_source()?
                            .token
                            .as_deref()
                            .ok_or_else(|| {
                                GoFinError::validation(
                                    "Native ETH cannot require ERC-20 allowance.",
                                )
                            })?;
                        let approval = saved.quote.approval.as_ref().ok_or_else(|| {
                            GoFinError::validation("Bridge quote lacks an exact approval identity.")
                        })?;
                        let runtime = self
                            .wallet_adapters
                            .get(&step.evm_call()?.chain_id)
                            .ok_or_else(|| {
                                GoFinError::validation("Allowance observer is unavailable.")
                            })?
                            .runtime
                            .clone();
                        let observed = during(
                            Phase::BridgeAllowance,
                            runtime.call(
                                gofin_evm::address(token)?,
                                &BridgeErc20::allowanceCall {
                                    owner: gofin_evm::address(sender)?,
                                    spender: gofin_evm::address(&approval.spender)?,
                                },
                                None,
                            ),
                        )
                        .await?;
                        let after = if matches!(step.kind, ProviderStepKind::ResetAllowance) {
                            gofin_evm::alloy::primitives::U256::ZERO
                        } else {
                            gofin_evm::u256(
                                selected_bounds
                                    .maximum_source_debit
                                    .as_ref()
                                    .expect("normalized source debit cap"),
                            )?
                        };
                        allowance_transition = Some((observed.to_string(), after.to_string()));
                        observed.to_string()
                    },
                    after: if let Some((_, after)) = &allowance_transition {
                        after.clone()
                    } else {
                        String::new()
                    },
                }]
            }
            ProviderStepKind::Deposit => vec![EffectConstraint::Debit {
                resource: resource.clone(),
                asset: source_asset.clone(),
                maximum_debit: saved.quote.gross_debit.atoms.clone(),
            }],
            ProviderStepKind::Prove | ProviderStepKind::Finalize | ProviderStepKind::Mint => vec![],
        };
        let intent = FinancialIntent {
            debits,
            maximum_costs: vec![cost],
            expected,
            locks: vec![resource.clone()],
        };
        // Preserve the late allowance observation above. Frozen gas-cap and
        // policy valuation checks can then observe prices concurrently; both
        // must succeed before any prepared candidate reaches authorization.
        let frozen_fee = gofin_evm::quantity(fee);
        let (_, valuation) = tokio::try_join!(
            during(
                Phase::BridgeGasPricing,
                self.validate_frozen_gas_cap(&selected_bounds, &gas_asset, &frozen_fee)
            ),
            during(Phase::BridgePolicyValuation, self.value_intent(&intent)),
        )?;
        let alias = self
            .state()?
            .transfer(&record.id)
            .map_err(store_error)?
            .and_then(|record| record.metadata["wallet_alias"].as_str().map(str::to_owned))
            .unwrap_or_else(|| saved.quote.intent.wallet.0.clone());
        let summary = self.summary(&saved, &link.kind, &alias);
        let binding = json!({
            "resource":resource,
            "transaction":transaction,
            "transfer_id":record.id,
            "step_id":link.step_id,
            "step_kind":link.kind,
            "step_dependencies":link.depends_on,
            "grant_id":record.grant_id,
            "quote_id":saved.quote.quote_id,
            "route_id":saved.quote.route_id,
            "provider_request_id":saved.quote.provider_request_id,
            "protocol_order_id":saved.quote.protocol_order_id,
            "quote":saved.quote,
            "intent":saved.quote.intent,
            "plan_digest":saved.plan.fixed_scope_digest,
            "plan":saved.plan,
            "step_ids":saved.step_ids,
            "step":step,
            "allowance_before":allowance_transition.as_ref().map(|(before,_)| before),
            "allowance_after":allowance_transition.as_ref().map(|(_,after)| after),
            "bounds":selected_bounds,
            (gofin_domain::BRIDGE_STEP_METADATA_KEY):{"transfer_id":record.id,"grant_id":record.grant_id,"step_id":link.step_id,"kind":step_kind_name(&link.kind),"bounds":selected_bounds,"settlement_expectation":expectation,"prospective_debit":prospective_debit,"source_principal":ResourceAmount { resource: saved.quote.intent.wallet.clone(), amount: saved.quote.gross_debit.clone() }},
        });
        let mut prepared = PreparedOperation {
            action_deadline: None,
            operation: operation.into(), input: input.clone(), binding, summary,
            checks: vec![Check { name:format!("reviewed {} order and step identity", bridge_provider_label(&saved.quote.provider)), status:CheckStatus::Passed, detail:"Prepared from the broker-stored quote and exact normalized step.".into() }],
            warnings: saved.quote.refund_paths.iter().filter(|path| path.minimum.is_zero()).map(|_| "One or more permitted refund paths have a zero minimum; refund deductions may leave an unrecovered loss.".into()).collect(),
            observed_at: self.clock.now().to_rfc3339(), expires_at: saved.quote.valid_until.clone(), valuation: Some(valuation), intent, operation_version:1,
        };
        // Ordinary exact approval uses the shared fresh-price ceiling. Once
        // a finite bridge grant is bound, its selected amount/fee/USD bounds
        // are the authority; adding this quote-specific cent-rounded ceiling
        // would incorrectly narrow repricing allowed within those bounds.
        if record.grant_id.is_none() {
            gofin_domain::value_bounds::bind(&mut prepared);
        }
        Ok(prepared)
    }

    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        variations: &Value,
    ) -> gofin_domain::Result<gofin_domain::financial_plans::PlanPermission> {
        if operation == "bridge.execute" {
            if variations != &json!({"mode":"agent_nominated"}) {
                return Err(error(
                    "PLAN_PERMISSION_UNSUPPORTED",
                    "authorization",
                    "A provider-neutral bridge step supports only a concrete agent nomination at execution.",
                    "Set permitted_variations to {\"mode\":\"agent_nominated\"} and place all financial bounds in the reviewed bridge.execute input.",
                ));
            }
            return self.prepare_bridge_execute_permission(input).await;
        }
        if operation != "bridge.transfer" {
            return Err(error(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Only an exact persisted bridge step can be included in a named plan.",
                "Use bridge.transfer with its broker-returned transfer, step and bounds, or use a standalone bridge authorization.",
            ));
        }
        // BridgeBounds is the entire supported variation contract. Accepting a
        // second, free-form variation object would make the authorization
        // review ambiguous about which layer owns amount and fee limits.
        if !variations
            .as_object()
            .is_some_and(|fields| fields.is_empty())
        {
            return Err(error(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Bridge plan scope is expressed by its explicit BridgeBounds input.",
                "Set permitted_variations to an empty object and place amount, receipt and fee limits in input.bounds.",
            ));
        }
        let requested = self.transfer_input(input)?;
        let selected_bounds = requested.bounds.clone().ok_or_else(|| {
            error(
                "BRIDGE_BOUNDS_REQUIRED",
                "authorization",
                "A named plan needs explicit bridge bounds for every source write.",
                "Include input, maximum source debit, minimum receipt and finite gas caps in bounds.",
            )
        })?;
        let record = self
            .state()?
            .transfer(&requested.transfer_id)
            .map_err(store_error)?
            .ok_or_else(|| {
                error(
                    "BRIDGE_TRANSFER_UNKNOWN",
                    "authorization",
                    "Bridge transfer identity is not present in the protected journal.",
                    "Request a fresh bridge quote before creating the named plan.",
                )
            })?;
        if record.grant_id.is_some()
            || (matches!(
                record.state,
                BridgeTransferState::Completed
                    | BridgeTransferState::Refunded
                    | BridgeTransferState::Failed
            ) && !record.steps.iter().any(|link| {
                link.step_id == requested.step_id && link.plan_authority.is_some()
            }))
            // A plan revision may refresh the same plan-owned transfer after
            // settled approval/reset steps. The store checks plan/revision
            // ownership when it claims a concrete attempt. Never adopt an
            // independently submitted exact step into a new named plan.
            || record.steps.iter().any(|link| {
                link.plan_authority.is_none()
                    && (!matches!(link.state, BridgeStepState::Unsubmitted | BridgeStepState::Superseded)
                        || link.operation_id.is_some())
            })
        {
            return Err(error(
                "BRIDGE_TRANSFER_NOT_PLANABLE",
                "authorization",
                "This bridge transfer already has an authorization, submitted step or plan owner.",
                "Create a new quote and transfer before adding bridge writes to a named plan.",
            ));
        }
        let saved = self.saved_quote(&record)?;
        if saved.quote.class != gofin_bridge::QuoteClass::Executable {
            return Err(error(
                "BRIDGE_QUOTE_NOT_EXECUTABLE",
                "authorization",
                "An indicative bridge quote cannot authorize a plan write.",
                "Request an executable quote and create a new plan preview.",
            ));
        }
        let bounds = normalize_authorization_bounds(Some(selected_bounds), &saved)?;
        let link = record
            .steps
            .iter()
            .find(|link| link.step_id == requested.step_id)
            .ok_or_else(|| {
                error(
                    "BRIDGE_STEP_UNKNOWN",
                    "authorization",
                    "Step identity is not part of this persisted transfer.",
                    "Use one of the exact step IDs returned by bridge.quotes.",
                )
            })?;
        if link.kind != requested.step_kind {
            return Err(error(
                "BRIDGE_STEP_KIND_MISMATCH",
                "authorization",
                "Step kind differs from the protected bridge plan.",
                "Use the kind returned for this exact step ID.",
            ));
        }
        if let Some(authority) = &link.plan_authority {
            let original: BridgeBounds =
                serde_json::from_value(record.approved_bounds.clone().ok_or_else(internal_error)?)
                    .map_err(|_| internal_error())?;
            if bounds != original {
                return Err(GoFinError::validation(
                    "A plan renewal must preserve the original bridge bounds and consumed authority.",
                ));
            }
            // A recovery recipe adds future identities, not new permissions
            // to previously approved writes. Preserve their exact historical
            // inventory so the started plan prefix remains immutable.
            return self
                .state()?
                .plan_permission(authority)
                .map_err(store_error)?
                .ok_or_else(|| {
                    GoFinError::validation(
                        "The original immutable bridge plan permission is missing.",
                    )
                });
        }

        // Rebuild only deterministic provider calls from the protected quote.
        // This path deliberately does not inspect balances, allowance state,
        // estimate gas or simulate writes: all future bridge steps must be
        // reviewable before upstream funding arrives. Actual execution still
        // uses `prepare`, which repeats live preflight immediately before the
        // immutable operation attempt is created.
        let provider = self.provider(&saved.quote.provider)?;
        let mut normalized_plan = if requested.step_kind.is_destination_stage()
            && record.transit.is_some()
            && matches!(saved.quote.provider.as_str(), "base_canonical" | "cctp")
        {
            let mut plan = saved.plan.clone();
            plan.bounds = bounds.clone();
            plan.fixed_scope_digest = if saved.quote.provider == "cctp" {
                gofin_bridge::cctp::scope_digest(
                    &saved.quote,
                    &bounds,
                    &saved.quote.intent.source_address,
                )?
            } else {
                gofin_bridge::base_canonical::scope_digest(
                    &saved.quote,
                    &bounds,
                    &saved.quote.intent.source_address,
                )?
            };
            plan
        } else {
            provider.prepare(&saved.quote, &bounds, &saved.quote.intent.source_address)?
        };
        normalized_plan = self
            .allowance_aware_plan(
                &saved.quote,
                normalized_plan,
                &saved.quote.intent.source_address,
                &bounds,
                Some(&saved),
            )
            .await?;
        if !same_bridge_plan_for_permission(&saved.plan.steps, &normalized_plan.steps) {
            return Err(error(
                "BRIDGE_PLAN_SCOPE_CHANGED",
                "authorization",
                "The selected bounds or saved quote no longer produce the persisted bridge step topology.",
                "Refresh this transfer's quote, then rebuild the named plan before requesting approval.",
            ));
        }
        let step = normalized_plan
            .steps
            .iter()
            .find(|step| {
                record
                    .steps
                    .get(step.ordinal as usize)
                    .is_some_and(|link| link.step_id == requested.step_id)
            })
            .ok_or_else(|| {
                GoFinError::validation("Persisted bridge step ordinal is inconsistent.")
            })?;
        let step_kind = domain_step_kind(&step.kind);
        if step_kind != link.kind {
            return Err(GoFinError::validation(
                "Persisted bridge step kind differs from its normalized provider step.",
            ));
        }
        let expected_dependencies = step
            .dependencies
            .iter()
            .map(|ordinal| {
                saved.step_ids.get(ordinal).cloned().ok_or_else(|| {
                    GoFinError::validation("Provider dependency identity is missing.")
                })
            })
            .collect::<gofin_domain::Result<Vec<_>>>()?;
        if expected_dependencies != link.depends_on {
            return Err(GoFinError::validation(
                "Persisted bridge dependency identities differ from the reviewed provider topology.",
            ));
        }
        let priced = self.validate_quote_prices(&bounds, &saved.quote).await?;
        self.validate_selected_usd_caps(
            &bounds,
            saved.quote.intent.evm_source()?,
            &saved.quote.intent.wallet,
        )
        .await?;

        let mut step_debits = Vec::new();
        if matches!(step.kind, ProviderStepKind::Deposit) {
            let source_asset = saved.quote.intent.evm_source()?.asset_id();
            if step
                .maximum_debit
                .iter()
                .any(|debit| debit.asset != source_asset)
            {
                return Err(GoFinError::validation(
                    "Bridge deposit declares a debit in an unreviewed asset.",
                ));
            }
            step_debits.push(ResourceAmount {
                resource: saved.quote.intent.wallet.clone(),
                amount: GoFinAmount {
                    asset: source_asset,
                    atoms: bounds
                        .maximum_source_debit
                        .clone()
                        .ok_or_else(|| GoFinError::validation("Bridge source cap is missing."))?,
                },
            });
        } else if !step.maximum_debit.is_empty() {
            return Err(GoFinError::validation(
                "Bridge approval step unexpectedly declares principal debit.",
            ));
        }

        let gas_asset = step.gas_asset()?;
        let step_resource = bridge_step_resource(&saved.quote.intent, step)?;
        let step_gas = CostBound {
            location: AccountingLocation::Cash {
                resource: step_resource.clone(),
            },
            amount: GoFinAmount {
                asset: gas_asset.clone(),
                atoms: maximum_gas_asset_cap(&bounds, &gas_asset)?,
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut step_costs = vec![step_gas];
        if step.solana_mint.is_some() {
            push_plan_cost_cap(
                &mut step_costs,
                &step_resource,
                &gas_asset,
                &cctp_solana::sol_cap(&bounds, gofin_bridge::FeeKind::AdditionalDebit)?,
            )?;
        }
        if matches!(step.kind, ProviderStepKind::Deposit) {
            let source_network = saved.quote.intent.source_asset_id()?.network;
            for fee in bounds.fee_caps.iter().filter(|fee| {
                fee.kind == gofin_bridge::FeeKind::AdditionalDebit
                    && fee.asset.network == source_network
            }) {
                push_plan_cost_cap(
                    &mut step_costs,
                    &saved.quote.intent.wallet,
                    &fee.asset,
                    &fee.maximum,
                )?;
            }
        }

        // The shared scope spans every concrete write in this one persisted
        // bridge. Its principal cap and route fee ceilings are listed once;
        // the engine sums actual/reserved use across the plan steps against
        // this group in addition to the user's plan-wide aggregate caps.
        let maximum_debits = vec![ResourceAmount {
            resource: saved.quote.intent.wallet.clone(),
            amount: GoFinAmount {
                asset: saved.quote.intent.evm_source()?.asset_id(),
                atoms: bounds
                    .maximum_source_debit
                    .clone()
                    .ok_or_else(|| GoFinError::validation("Bridge source cap is missing."))?,
            },
        }];
        let mut maximum_costs =
            cumulative_gas_cost_caps(&bounds, &normalized_plan, &saved.quote.intent.wallet)?;
        for fee in bounds
            .fee_caps
            .iter()
            .filter(|fee| fee.kind == gofin_bridge::FeeKind::AdditionalDebit)
        {
            push_plan_cost_cap(
                &mut maximum_costs,
                &if fee.asset.network == gofin_solana::NETWORK {
                    saved.quote.intent.destination_resource()?
                } else {
                    saved.quote.intent.wallet.clone()
                },
                &fee.asset,
                &fee.maximum,
            )?;
        }

        let allowed_steps = normalized_plan
            .steps
            .iter()
            .map(|provider_step| {
                let step_id = saved
                    .step_ids
                    .get(&provider_step.ordinal)
                    .ok_or_else(|| GoFinError::validation("Provider step identity is missing."))?;
                let mut descriptor = bridge_step_execution(provider_step)?;
                descriptor["step_id"] = json!(step_id);
                descriptor["step_kind"] = json!(domain_step_kind(&provider_step.kind));
                descriptor["dependencies"] = json!(
                    provider_step
                        .dependencies
                        .iter()
                        .filter_map(|ordinal| saved.step_ids.get(ordinal))
                        .collect::<Vec<_>>()
                );
                Ok(descriptor)
            })
            .collect::<gofin_domain::Result<Vec<_>>>()?;
        let mut fixed = json!({
            "transfer_id":record.id,
            "wallet":saved.quote.intent.wallet,
            "source":saved.quote.intent.source,
            "source_address":saved.quote.intent.source_address.to_ascii_lowercase(),
            "destination_chain_id":saved.quote.intent.destination_chain_id,
            "destination_assets":saved.quote.intent.destination_assets,
            "recipient":saved.quote.intent.recipient,
            "solana_destination":saved.quote.intent.solana_destination,
            "refund_to":saved.quote.intent.refund_to.to_ascii_lowercase(),
            "allowed_refund_assets":saved.quote.intent.allowed_refund_assets,
            "provider":saved.quote.provider,
            "route_id":saved.quote.route_id,
            "plan_digest":normalized_plan.fixed_scope_digest,
            "steps":allowed_steps,
            "plan_step_identity":{
                "transfer_id":record.id,
                "step_id":link.step_id,
                "step_kind":step_kind_name(&link.kind),
            }
        });
        if saved.quote.intent.solana_destination.is_none() {
            fixed
                .as_object_mut()
                .ok_or_else(internal_error)?
                .remove("solana_destination");
            fixed["recipient"] = json!(saved.quote.intent.recipient.to_ascii_lowercase());
        }
        let summary = finite_bridge_boundary_summary(
            &format!(
                "{} step of {}",
                step_kind_name(&link.kind),
                saved.quote.provider
            ),
            &normalized_plan,
            &saved.quote.intent,
            &bounds,
        )?;
        let expectation = settlement_expectation(
            &saved.quote,
            bounds
                .minimum_net_receipt
                .as_ref()
                .expect("normalized minimum receipt")
                .atoms
                .clone(),
        );
        let deferred_receipt =
            if matches!(link.kind, BridgeStepKind::Finalize | BridgeStepKind::Mint) {
                expectation
                    .destination
                    .first()
                    .map(|target| ResourceAmount {
                        resource: target.resource.clone(),
                        amount: GoFinAmount {
                            asset: target.asset.clone(),
                            atoms: target.minimum_atoms.clone(),
                        },
                    })
            } else {
                None
            };
        let mut possible_credits = std::collections::BTreeSet::new();
        for target in expectation
            .destination
            .into_iter()
            .chain(expectation.refund)
        {
            possible_credits.insert(
                gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                    resource: target.resource,
                    asset: target.asset,
                },
            );
        }
        let fee_assets = maximum_costs
            .iter()
            .map(|cost| cost.amount.asset.clone())
            .chain(bounds.fee_caps.iter().map(|fee| fee.asset.clone()))
            .chain(priced.into_iter().map(|fee| fee.fee.amount.asset))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(gofin_domain::financial_plans::PlanPermission {
            recovery_parent: None,
            spec: gofin_domain::boundaries::BoundarySpec {
                fixed,
                parameters: serde_json::to_value(&bounds).map_err(|_| internal_error())?,
                maximum_debits: step_debits,
                maximum_costs: step_costs,
                summary,
            },
            possible_credits: possible_credits.into_iter().collect(),
            fee_assets,
            deferred_receipt,
            authorization_seconds: bounds.authorization_seconds.map(u64::from),
            cumulative_scope: Some(
                gofin_domain::financial_plans::FinancialPlanCumulativeScope {
                    maximum_obligations: vec![],
                    scope_id: record.id,
                    maximum_debits,
                    maximum_costs,
                },
            ),
        })
    }

    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> gofin_domain::Result<gofin_domain::boundaries::BoundarySpec> {
        if matches!(
            prepared.binding["quote"]["provider"].as_str(),
            Some("base_canonical" | "cctp")
        ) {
            return Err(error(
                "BRIDGE_BOUNDARY_UNSUPPORTED",
                "authorization",
                "Explicit destination stages use exact approval or a named sequential plan.",
                "Authorize the exact step, or include the finite stages and cumulative fees in a named plan.",
            ));
        }
        bridge_boundary_spec(prepared, bounds)
    }

    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<()> {
        if prepared.operation == "bridge.execute" {
            return validate_bridge_execute_candidate(spec, prepared);
        }
        validate_bridge_boundary(spec, prepared)
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<PreparedOperation> {
        if prepared
            .binding
            .get("relay_solana_source_message_sha256")
            .is_some()
        {
            return self.revalidate_relay_solana_source(prepared).await;
        }
        if prepared.binding.get("solana_message_sha256").is_some() {
            return self.revalidate_cctp_solana_mint(prepared).await;
        }
        let transaction: FrozenTransaction =
            serde_json::from_value(prepared.binding["transaction"].clone()).map_err(|_| {
                error(
                    "BRIDGE_BINDING_INVALID",
                    "submission",
                    "Frozen EVM bridge envelope is malformed.",
                    "Reconcile the original step; do not prepare an alternate transaction.",
                )
            })?;
        let runtime = self.wallet_adapters.get(&transaction.chain_id).ok_or_else(|| error(
            "BRIDGE_NETWORK_UNAVAILABLE", "submission", "Original bridge chain has no configured wallet runtime.", "Keep the original operation unresolved until its configured chain is available."))?.runtime.clone();
        let target = gofin_evm::address(&transaction.to)?;
        tokio::try_join!(runtime.revalidate(&transaction), runtime.code(target))?;
        let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
            .map_err(|_| {
                error(
                    "BRIDGE_BINDING_INVALID",
                    "submission",
                    "Saved quote binding is malformed.",
                    "Reconcile the original operation; do not prepare an alternate route.",
                )
            })?;
        let kind: BridgeStepKind = serde_json::from_value(prepared.binding["step_kind"].clone())
            .map_err(|_| GoFinError::validation("Bridge stage binding is malformed."))?;
        if quote.provider == "relay" {
            self.revalidate_relay_assets(&quote).await?;
        }
        if kind == BridgeStepKind::Mint {
            return self
                .revalidate_cctp_mint(prepared, &transaction, &quote)
                .await;
        }
        if kind.is_destination_stage() {
            let id = prepared.binding["transfer_id"]
                .as_str()
                .ok_or_else(internal_error)?;
            let transfer = self
                .state()?
                .transfer(id)
                .map_err(store_error)?
                .ok_or_else(internal_error)?;
            let locator = transfer
                .transit
                .as_ref()
                .and_then(|transit| transit.destination_locator.as_ref())
                .ok_or_else(|| {
                    GoFinError::validation("Original withdrawal transit proof is missing.")
                })?;
            if quote.provider != "base_canonical"
                || transfer.grant_id.is_some()
                || json!(locator) != prepared.binding["source_locator"]
            {
                return Err(GoFinError::validation(
                    "Deferred Base stage differs from its original settled source identity.",
                ));
            }
            let verified = saved_base_withdrawal(locator)?;
            let call = if kind == BridgeStepKind::Prove {
                let readiness = self
                    .base_provider()?
                    .withdrawal_readiness(&verified.withdrawal, &transaction.sender)
                    .await?;
                if readiness.finalized || (readiness.proven_at > 0 && !readiness.proof_invalidated)
                {
                    return Err(error(
                        "BRIDGE_PROOF_ALREADY_PRESENT",
                        "submission",
                        "A valid proof or finalization appeared before dispatch.",
                        "Inspect bridge.status; preserve the existing proof's maturity timer.",
                    ));
                }
                let witness: gofin_bridge::base_canonical::BaseWithdrawalWitness =
                    serde_json::from_value(prepared.binding["base_witness"].clone())
                        .map_err(|_| GoFinError::validation("Base proof witness is malformed."))?;
                gofin_bridge::base_canonical::encode_prove_call(&verified.withdrawal, &witness)?
            } else {
                let readiness = self
                    .base_provider()?
                    .withdrawal_readiness(&verified.withdrawal, &transaction.sender)
                    .await?;
                if !readiness.ready || readiness.finalized {
                    return Err(error(
                        "BRIDGE_FINALIZATION_PENDING",
                        "submission",
                        "Portal finalization checks no longer permit this saved withdrawal.",
                        "Inspect bridge.status and preserve the original withdrawal identity.",
                    ));
                }
                gofin_bridge::base_canonical::encode_finalize_call(
                    &verified.withdrawal,
                    &transaction.sender,
                )?
            };
            if transaction.chain_id != call.chain_id
                || !transaction.to.eq_ignore_ascii_case(&call.to)
                || transaction.value != call.value
                || !transaction.calldata.eq_ignore_ascii_case(&call.data)
            {
                return Err(GoFinError::validation(
                    "Frozen Base stage is not the exact broker-derived portal call.",
                ));
            }
            let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
                .map_err(|_| internal_error())?;
            self.validate_frozen_gas_cap(
                &bounds,
                &EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                &gofin_evm::quantity(transaction.maximum_cost()?),
            )
            .await?;
            self.validate_transfer_gas_budget(
                &transfer,
                prepared.binding["step_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
                &bounds,
                &EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                }
                .asset_id(),
                transaction.maximum_cost()?,
            )?;
            let mut fresh = prepared.clone();
            fresh.valuation = Some(self.value_intent(&fresh.intent).await?);
            gofin_domain::value_bounds::carry_at(prepared, &mut fresh, self.clock.now())?;
            return Ok(fresh);
        }
        if quote.provider == "base_canonical"
            && kind == BridgeStepKind::Deposit
            && let Some(source_token) = &quote.intent.evm_source()?.token
        {
            let destination_token = quote
                .intent
                .destination_assets
                .first()
                .and_then(|asset| asset.token.as_deref())
                .ok_or_else(internal_error)?;
            let (l1, l2) = if quote.intent.evm_source()?.chain_id == 1 {
                (source_token.as_str(), destination_token)
            } else {
                (destination_token, source_token.as_str())
            };
            let decimals = self.base_provider()?.verify_pair(l1, l2).await?;
            if decimals != quote.intent.evm_source()?.decimals
                || quote.intent.destination_assets[0].decimals != decimals
            {
                return Err(GoFinError::validation(
                    "Canonical token-pair precision or deployment changed before dispatch.",
                ));
            }
        }
        if quote.provider == "cctp" {
            let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
                .map_err(|_| internal_error())?;
            let transfer = self
                .state()?
                .transfer(
                    prepared.binding["transfer_id"]
                        .as_str()
                        .ok_or_else(internal_error)?,
                )
                .map_err(store_error)?
                .ok_or_else(internal_error)?;
            self.cctp_provider()?
                .verify_deployments(&quote.intent)
                .await?;
            self.validate_cctp_gas(
                &quote,
                &bounds,
                transfer.metadata["wallet_alias"]
                    .as_str()
                    .ok_or_else(internal_error)?,
                &transfer,
            )
            .await?;
        }
        if quote.provider == "arbitrum_canonical" {
            require_empty_code(&runtime, &quote.intent.source_address, None).await?;
        }
        if let Some(token) = &quote.intent.evm_source()?.token {
            let token = gofin_evm::address(token)?;
            // The exact latest deployment was already checked for an allowance
            // target. A different source network or contract still needs its own.
            if quote.intent.evm_source()?.chain_id != transaction.chain_id || token != target {
                self.wallet_adapters.get(&quote.intent.evm_source()?.chain_id).ok_or_else(|| error("BRIDGE_NETWORK_UNAVAILABLE", "submission", "Original source token network observer is unavailable.", "Keep the existing step unresolved until its configured network is available."))?.runtime.code(token).await?;
            }
        }
        let bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
            .map_err(|_| {
                error(
                    "BRIDGE_BINDING_INVALID",
                    "submission",
                    "Saved bounds binding is malformed.",
                    "Reconcile the original operation; do not change its authorization scope.",
                )
            })?;
        let transfer = self
            .state()?
            .transfer(
                prepared.binding["transfer_id"]
                    .as_str()
                    .ok_or_else(internal_error)?,
            )
            .map_err(store_error)?
            .ok_or_else(internal_error)?;
        self.validate_transfer_gas_budget(
            &transfer,
            prepared.binding["step_id"]
                .as_str()
                .ok_or_else(internal_error)?,
            &bounds,
            &EvmAsset {
                chain_id: transaction.chain_id,
                token: None,
                decimals: 18,
            }
            .asset_id(),
            transaction.maximum_cost()?,
        )?;
        let gas_asset = EvmAsset {
            chain_id: transaction.chain_id,
            token: None,
            decimals: 18,
        }
        .asset_id();
        let maximum_cost = gofin_evm::quantity(transaction.maximum_cost()?);
        // All checks use the same immutable bounds and transaction. Keep fresh
        // price evidence while overlapping the independent quote, cap and policy
        // observations instead of serially extending the dispatch path.
        let (_, (), (), valuation) = tokio::try_join!(
            self.validate_quote_prices(&bounds, &quote),
            self.validate_selected_usd_caps(
                &bounds,
                quote.intent.evm_source()?,
                &quote.intent.wallet
            ),
            self.validate_frozen_gas_cap(&bounds, &gas_asset, &maximum_cost),
            self.value_intent(&prepared.intent),
        )?;
        let mut fresh = prepared.clone();
        fresh.valuation = Some(valuation);
        gofin_domain::value_bounds::carry_at(prepared, &mut fresh, self.clock.now())?;
        fresh.binding = prepared.binding.clone();
        Ok(fresh)
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> gofin_domain::Result<gofin_domain::Submission> {
        let fresh = self.revalidate(prepared).await?;
        if fresh.binding != prepared.binding {
            return Err(error(
                "BRIDGE_BINDING_CHANGED",
                "submission",
                "Bridge step differs from the immutable approved envelope.",
                "Reconcile the original step before preparing a replacement.",
            ));
        }
        let validated =
            gofin_domain::RevalidatedOperation::after_revalidation(fresh, self.clock.clone())?;
        self.submit_revalidated(&validated, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> gofin_domain::Result<gofin_domain::Submission> {
        validated.assert_fresh()?;
        let prepared = validated.prepared();
        journal.assert_candidate(prepared)?;
        if prepared
            .binding
            .get("relay_solana_source_message_sha256")
            .is_some()
        {
            return self.submit_relay_solana_source(validated, journal).await;
        }
        if prepared.binding.get("solana_message_sha256").is_some() {
            return self.submit_cctp_solana_mint(validated, journal).await;
        }
        let transaction: FrozenTransaction =
            serde_json::from_value(prepared.binding["transaction"].clone())
                .map_err(|_| GoFinError::validation("Frozen bridge transaction is invalid."))?;
        let runtime = self
            .wallet_adapters
            .get(&transaction.chain_id)
            .ok_or_else(|| GoFinError::validation("Bridge chain runtime disappeared."))?
            .runtime
            .clone();
        runtime.submit_revalidated(&transaction, json!({"operation":prepared.operation,"transfer_id":prepared.binding["transfer_id"],"step_id":prepared.binding["step_id"],"kind":prepared.binding["step_kind"]}), journal, validated).await
    }

    async fn reconcile(
        &self,
        record: &gofin_domain::OperationRecord,
    ) -> gofin_domain::Result<gofin_domain::Reconciliation> {
        if record.preview.as_ref().is_some_and(|preview| {
            preview
                .binding
                .get("relay_solana_source_message_sha256")
                .is_some()
        }) {
            return self.reconcile_relay_solana_source(record).await;
        }
        if record
            .preview
            .as_ref()
            .is_some_and(|preview| preview.binding.get("solana_message_sha256").is_some())
        {
            return self.reconcile_cctp_solana_step(record).await;
        }
        reconcile_wallet_step(self, record).await
    }
}

fn bridge_candidate_locators(
    candidates: &[BridgeEvidenceCandidate],
    expected_order_id: &str,
    destination_chain_id: u64,
    approved_refund_chains: &std::collections::BTreeSet<u64>,
) -> Vec<Value> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate.order_id.as_deref() == Some(expected_order_id)
                && match &candidate.kind {
                    EvidenceKind::DestinationFill => candidate.chain_id == destination_chain_id,
                    EvidenceKind::Refund => approved_refund_chains.contains(&candidate.chain_id),
                    EvidenceKind::SourceDeposit | EvidenceKind::SettlementHub => false,
                }
        })
        .filter_map(|candidate| {
            let hash = gofin_evm::hash(&candidate.transaction_hash).ok()?;
            let kind = match &candidate.kind {
                EvidenceKind::DestinationFill => "destination_fill",
                EvidenceKind::Refund => "refund",
                EvidenceKind::SourceDeposit | EvidenceKind::SettlementHub => return None,
            };
            Some(json!({
                "kind":kind,
                "chain_id":candidate.chain_id,
                "transaction_hash":format!("{hash:#x}"),
                "classification":"candidate_locator_only"
            }))
        })
        .collect()
}

fn bridge_candidate_observation(candidate: &BridgeEvidenceCandidate, state: &str) -> Option<Value> {
    let hash = gofin_evm::hash(&candidate.transaction_hash).ok()?;
    let kind = match &candidate.kind {
        EvidenceKind::DestinationFill => "destination_fill",
        EvidenceKind::Refund => "refund",
        EvidenceKind::SourceDeposit | EvidenceKind::SettlementHub => return None,
    };
    Some(json!({
        "kind":kind,
        "chain_id":candidate.chain_id,
        "transaction_hash":format!("{hash:#x}"),
        "observation":state
    }))
}

fn sanitize_router_proof_rejection(error: gofin_domain::GoFinError) -> Value {
    let (code, message) = match error.code.as_str() {
        "BRIDGE_PROOF_MISMATCH" => (
            "BRIDGE_PROOF_MISMATCH",
            "A reviewed RouterV3 event did not match the saved sender, recipient, asset, correlation, or receipt floor.",
        ),
        "BRIDGE_PROOF_INVALID" => (
            "BRIDGE_PROOF_INVALID",
            "A reviewed RouterV3 event has an invalid ABI encoding.",
        ),
        _ => (
            "BRIDGE_PROOF_NOT_MATCHED",
            "A reviewed RouterV3 event could not be verified against the saved transfer.",
        ),
    };
    json!({"code":code,"message":message})
}

fn sanitize_direct_proof_rejection(error: gofin_domain::GoFinError) -> Value {
    let (code, message) = match error.code.as_str() {
        "BRIDGE_PROOF_MISMATCH" => (
            "BRIDGE_PROOF_MISMATCH",
            "The direct payout transaction did not match the approved recipient, asset, receipt floor, or exact protocol order tag.",
        ),
        "BRIDGE_PROOF_INVALID" => (
            "BRIDGE_PROOF_INVALID",
            "The direct payout transaction or its ERC-20 receipt event has an invalid encoding.",
        ),
        _ => (
            "BRIDGE_PROOF_NOT_MATCHED",
            "The direct payout transaction could not be verified against the saved transfer.",
        ),
    };
    json!({"code":code,"message":message})
}

fn attach_direct_payment_rejection(mut observation: Value, rejection: Option<Value>) -> Value {
    if let (Some(object), Some(rejection)) = (observation.as_object_mut(), rejection) {
        object.insert("direct_payment_rejection".into(), rejection);
    }
    observation
}

fn attach_direct_code_observation(
    mut observation: Value,
    code_observation: Option<Value>,
) -> Value {
    if let (Some(object), Some(code_observation)) = (observation.as_object_mut(), code_observation)
    {
        object.insert("direct_code_observation".into(), code_observation);
    }
    observation
}

fn direct_native_code_guard_failed(observation: &Value) -> bool {
    let code = &observation["direct_code_observation"];
    code["chain_matches"] == true && !direct_code_observation_matches(&code["recipient_code"], true)
}

fn attach_direct_native_execution_observation(
    mut observation: Value,
    receipt: &CanonicalReceipt,
    transaction: Option<&Value>,
) -> Value {
    observation["native_execution_observation"] =
        direct_native_gas_facts(&receipt.receipt, transaction);
    observation
}

fn direct_native_gas_facts(receipt: &Value, transaction: Option<&Value>) -> Value {
    let quantity = |value: Option<&Value>| {
        value
            .and_then(|value| gofin_evm::rpc_u256(value).ok())
            .map(|value| value.to_string())
            .map(Value::String)
            .unwrap_or(Value::Null)
    };
    let gas_used = receipt
        .get("gasUsed")
        .and_then(|value| gofin_evm::rpc_u256(value).ok());
    let gas_used_for_l1 = receipt
        .get("gasUsedForL1")
        .and_then(|value| gofin_evm::rpc_u256(value).ok());
    let l2_execution_gas = gas_used
        .zip(gas_used_for_l1)
        .and_then(|(gas, l1)| gas.checked_sub(l1));
    let expected_intrinsic_gas = transaction
        .and_then(|transaction| transaction.get("input").or_else(|| transaction.get("data")))
        .and_then(Value::as_str)
        .and_then(|input| hex::decode(input.strip_prefix("0x")?).ok())
        .map(|input| {
            let zero_bytes = input.iter().filter(|byte| **byte == 0).count() as u64;
            let nonzero_bytes = input.len() as u64 - zero_bytes;
            21_000u64 + 4 * zero_bytes + 16 * nonzero_bytes
        });
    let access_list_entries = transaction
        .and_then(|transaction| transaction["accessList"].as_array())
        .map(Vec::len);
    json!({
        "receipt_gas_used_atoms":quantity(receipt.get("gasUsed")),
        "receipt_gas_used_for_l1_atoms":quantity(receipt.get("gasUsedForL1")),
        "l2_execution_gas_atoms":l2_execution_gas.map(|gas| gas.to_string()),
        "required_intrinsic_gas_atoms":expected_intrinsic_gas,
        "receipt_effective_gas_price_atoms":quantity(receipt.get("effectiveGasPrice")),
        "transaction_access_list_entry_count":access_list_entries
    })
}

fn attach_bridge_payment_method(mut observation: Value, method: &str) -> Value {
    if let Some(object) = observation.as_object_mut() {
        object.insert("proof_method".into(), json!(method));
    }
    observation
}

fn bridge_payment_proof_method(method: &str) -> Option<&'static str> {
    match method {
        "direct_transaction_order_tag" => Some("direct_transaction_order_tag"),
        "intrinsic_gas_retained_credit" => Some("intrinsic_gas_retained_credit"),
        _ => None,
    }
}

fn direct_payment_transaction(
    transaction: &Value,
    candidate: &BridgeEvidenceCandidate,
    receipt: &CanonicalReceipt,
) -> Option<gofin_bridge::relay::proof::DirectPaymentTransaction> {
    let tx_hash = gofin_evm::hash(transaction.get("hash")?.as_str()?).ok()?;
    let candidate_hash = gofin_evm::hash(&candidate.transaction_hash).ok()?;
    let receipt_hash = gofin_evm::hash(&format!("{:#x}", receipt.hash)).ok()?;
    if tx_hash != candidate_hash || receipt_hash != candidate_hash {
        return None;
    }
    let chain_id = gofin_evm::rpc_u64(transaction.get("chainId")?).ok()?;
    if chain_id != candidate.chain_id
        || gofin_evm::rpc_u64(transaction.get("blockNumber")?).ok()? != receipt.block_number
        || gofin_evm::hash(transaction.get("blockHash")?.as_str()?).ok()? != receipt.block_hash
    {
        return None;
    }
    let transaction_type: u8 = gofin_evm::rpc_u64(transaction.get("type")?)
        .ok()?
        .try_into()
        .ok()?;
    let from = gofin_evm::address(transaction.get("from")?.as_str()?).ok()?;
    let to = gofin_evm::address(transaction.get("to")?.as_str()?).ok()?;
    if gofin_evm::lower_address(from) != receipt.transaction_sender {
        return None;
    }
    let value = gofin_evm::rpc_u256(transaction.get("value")?).ok()?;
    let input = transaction
        .get("input")
        .or_else(|| transaction.get("data"))?
        .as_str()?
        .to_owned();
    Some(gofin_bridge::relay::proof::DirectPaymentTransaction {
        transaction_hash: format!("{tx_hash:#x}"),
        chain_id,
        transaction_type,
        from: format!("{from:#x}"),
        to: format!("{to:#x}"),
        value: Quantity::new(value.to_string()).ok()?,
        input,
    })
}

fn direct_access_list(
    transaction: &Value,
) -> Option<Vec<gofin_bridge::relay::proof::DirectAccessListEntry>> {
    transaction
        .get("accessList")?
        .as_array()?
        .iter()
        .map(|entry| {
            let address = entry.get("address")?.as_str()?.to_owned();
            let storage_keys = entry
                .get("storageKeys")?
                .as_array()?
                .iter()
                .map(|key| key.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            Some(gofin_bridge::relay::proof::DirectAccessListEntry {
                address,
                storage_keys,
            })
        })
        .collect()
}

fn direct_receipt_quantity(receipt: &Value, field: &str) -> Option<gofin_domain::Quantity> {
    let value = receipt.get(field)?;
    let atoms = gofin_evm::rpc_u256(value).ok()?.to_string();
    gofin_domain::Quantity::new(atoms).ok()
}

fn verify_native_intrinsic_rpc_proof(
    transaction_value: &Value,
    receipt: &Value,
    transaction: &gofin_bridge::relay::proof::DirectPaymentTransaction,
    expected: &gofin_bridge::relay::proof::ExpectedDirectPayment,
) -> Option<gofin_bridge::relay::proof::VerifiedDirectPayment> {
    let access_list = direct_access_list(transaction_value)?;
    let gas_used = direct_receipt_quantity(receipt, "gasUsed")?;
    let gas_used_for_l1 = direct_receipt_quantity(receipt, "gasUsedForL1");
    gofin_bridge::relay::proof::verify_native_intrinsic_credit(
        transaction,
        &access_list,
        &gas_used,
        gas_used_for_l1.as_ref(),
        expected,
    )
    .ok()
}

fn direct_token_transfers(logs: &[Value]) -> Vec<gofin_bridge::relay::proof::DirectTokenTransfer> {
    logs.iter()
        .filter_map(|log| {
            let log_index: u32 = gofin_evm::rpc_u64(log.get("logIndex")?)
                .ok()?
                .try_into()
                .ok()?;
            let address = log.get("address")?.as_str()?.to_owned();
            let topics = log
                .get("topics")?
                .as_array()?
                .iter()
                .map(|topic| topic.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            let data = log.get("data")?.as_str()?.to_owned();
            Some(gofin_bridge::relay::proof::DirectTokenTransfer {
                log_index,
                address,
                topics,
                data,
            })
        })
        .collect()
}

async fn direct_recipient_code_observation(
    runtime: &gofin_evm::EvmRuntime,
    address: &str,
    expect_empty: bool,
    inclusion_block: u64,
) -> Value {
    let Ok(address) = gofin_evm::address(address) else {
        return json!({
            "address_valid":false,
            "expected_code_empty":expect_empty,
            "before":{"block_number":Value::Null,"code_empty":Value::Null,"error":{"code":"ADDRESS_INVALID"}},
            "inclusion":{"block_number":inclusion_block,"code_empty":Value::Null,"error":{"code":"ADDRESS_INVALID"}}
        });
    };
    let read_at = |block: u64| async move {
        match runtime
            .rpc
            .request(
                "eth_getCode",
                json!([
                    gofin_evm::lower_address(address),
                    gofin_evm::block_tag(Some(block))
                ]),
            )
            .await
        {
            Ok(response) => match gofin_evm::rpc_bytes(&response) {
                Ok(code) => json!({
                    "block_number":block,
                    "code_empty":code.is_empty(),
                    "error":Value::Null
                }),
                Err(error) => json!({
                    "block_number":block,
                    "code_empty":Value::Null,
                    "error":sanitized_rpc_error(&error)
                }),
            },
            Err(error) => json!({
                "block_number":block,
                "code_empty":Value::Null,
                "error":sanitized_rpc_error(&error)
            }),
        }
    };
    let before = if inclusion_block > 0 {
        read_at(inclusion_block - 1).await
    } else {
        json!({
            "block_number":Value::Null,
            "code_empty":Value::Null,
            "error":{"code":"PRE_STATE_BLOCK_UNAVAILABLE"}
        })
    };
    let inclusion = read_at(inclusion_block).await;
    json!({
        "address_valid":true,
        "expected_code_empty":expect_empty,
        "before":before,
        "inclusion":inclusion
    })
}

fn direct_code_observation_matches(observation: &Value, expect_empty: bool) -> bool {
    observation["address_valid"] == true
        && observation["before"]["code_empty"] == json!(expect_empty)
        && observation["inclusion"]["code_empty"] == json!(expect_empty)
}

fn sanitized_rpc_error(error: &gofin_domain::GoFinError) -> Value {
    let mut sanitized = json!({"code":error.code});
    let details = error.details.as_deref();
    if let Some(status) = details
        .and_then(|details| details.get("http_status"))
        .and_then(Value::as_u64)
    {
        sanitized["http_status"] = json!(status);
    }
    if let Some(code) = details
        .and_then(|details| details.get("json_rpc_code"))
        .and_then(Value::as_i64)
    {
        sanitized["json_rpc_code"] = json!(code);
    }
    sanitized
}

fn attach_router_proof_diagnostics(
    mut observation: Value,
    receipt: &gofin_evm::CanonicalReceipt,
    reviewed_router_event_found: bool,
    router_event_topics_count: Option<usize>,
    proof_rejection: Option<Value>,
    logs: &[Value],
    transaction_observation: Value,
) -> Value {
    if let Some(object) = observation.as_object_mut() {
        object.insert(
            "observed_transaction_sender".into(),
            json!(receipt.transaction_sender),
        );
        object.insert(
            "reviewed_router_event_found".into(),
            json!(reviewed_router_event_found),
        );
        object.insert(
            "router_event_topics_count".into(),
            json!(router_event_topics_count),
        );
        object.insert("transaction_observation".into(), transaction_observation);
        if let Some(rejection) = proof_rejection {
            object.insert("proof_rejection".into(), rejection);
        }
        if !reviewed_router_event_found {
            let log_observations = logs
                .iter()
                .take(16)
                .map(|log| {
                    let emitter = log
                        .get("address")
                        .and_then(Value::as_str)
                        .and_then(|address| gofin_evm::address(address).ok())
                        .map(|address| format!("{address:#x}"));
                    let topics = log.get("topics").and_then(Value::as_array);
                    let topic0 = topics
                        .and_then(|topics| topics.first())
                        .and_then(Value::as_str)
                        .and_then(|topic| gofin_evm::hash(topic).ok())
                        .map(|topic| format!("{topic:#x}"));
                    json!({
                        "emitter":emitter,
                        "topic0":topic0,
                        "topics_count":topics.map(Vec::len)
                    })
                })
                .collect::<Vec<_>>();
            object.insert("receipt_logs".into(), json!(log_observations));
            object.insert("receipt_log_count".into(), json!(logs.len()));
            object.insert("receipt_logs_truncated".into(), json!(logs.len() > 16));
            object.insert(
                "proof_rejection".into(),
                json!({
                    "code":"ROUTER_EVENT_NOT_FOUND",
                    "message":"The finalized receipt contains no matching reviewed RouterV3 FundsMovement event."
                }),
            );
        }
    }
    observation
}

fn bridge_transaction_shape_observation(
    transaction: Option<&Value>,
    candidate: &BridgeEvidenceCandidate,
    order_id: &str,
    provider_request_id: &str,
) -> Value {
    let expected_hash = gofin_evm::hash(&candidate.transaction_hash).ok();
    let reported_hash = transaction
        .filter(|value| !value.is_null())
        .and_then(|value| value.get("hash"))
        .and_then(Value::as_str)
        .and_then(|hash| gofin_evm::hash(hash).ok());
    let hash_matches = expected_hash.is_some() && reported_hash == expected_hash;
    let Some(transaction) = transaction
        .filter(|value| !value.is_null())
        .filter(|_| hash_matches)
    else {
        return json!({
            "transaction_found":false,
            "candidate_hash_matches":false,
            "chain_id":candidate.chain_id,
            "reported_chain_id":Value::Null,
            "from":Value::Null,
            "to":Value::Null,
            "value_atoms":Value::Null,
            "type":Value::Null,
            "input_byte_length":Value::Null,
            "input_matches_order_id":Value::Null,
            "input_matches_provider_request_id":Value::Null
        });
    };

    let input = transaction
        .get("input")
        .or_else(|| transaction.get("data"))
        .and_then(Value::as_str)
        .and_then(|input| input.strip_prefix("0x"))
        .and_then(|input| hex::decode(input).ok());
    let order = gofin_evm::hash(order_id).ok();
    let request = gofin_evm::hash(provider_request_id).ok();
    let input_matches = |identity: Option<gofin_evm::alloy::primitives::B256>| {
        input.as_ref().is_some_and(|input| {
            input.len() == 32
                && identity.is_some_and(|identity| input.as_slice() == identity.as_slice())
        })
    };
    let canonical_address = |key: &str| {
        transaction
            .get(key)
            .and_then(Value::as_str)
            .and_then(|address| gofin_evm::address(address).ok())
            .map(|address| format!("{address:#x}"))
    };
    let reported_chain_id = transaction
        .get("chainId")
        .and_then(|chain| gofin_evm::rpc_u64(chain).ok());
    let value_atoms = transaction
        .get("value")
        .and_then(|value| gofin_evm::rpc_u256(value).ok())
        .map(|value| value.to_string());
    let transaction_type = transaction
        .get("type")
        .and_then(|kind| gofin_evm::rpc_u64(kind).ok());
    json!({
        "transaction_found":true,
        "candidate_hash_matches":true,
        "chain_id":candidate.chain_id,
        "reported_chain_id":reported_chain_id,
        "from":canonical_address("from"),
        "to":canonical_address("to"),
        "value_atoms":value_atoms,
        "type":transaction_type,
        "input_byte_length":input.as_ref().map(Vec::len),
        "input_matches_order_id":input_matches(order),
        "input_matches_provider_request_id":input_matches(request)
    })
}

async fn bridge_candidate_finality_observation(
    runtime: &gofin_evm::EvmRuntime,
    candidate: &BridgeEvidenceCandidate,
    required_confirmations: u64,
    require_finalized: bool,
) -> Value {
    // These are diagnostic reads only. `observe_receipt` remains the authority
    // for canonical inclusion and finality, and this result never settles funds.
    let receipt = runtime
        .rpc
        .request(
            "eth_getTransactionReceipt",
            json!([candidate.transaction_hash]),
        )
        .await
        .ok();
    let latest = runtime.rpc.request("eth_blockNumber", json!([])).await.ok();
    let finalized = runtime
        .rpc
        .request("eth_getBlockByNumber", json!(["finalized", false]))
        .await
        .ok();
    bridge_finality_observation(
        receipt.as_ref(),
        latest.as_ref(),
        finalized.as_ref(),
        required_confirmations,
        require_finalized,
    )
}

fn bridge_finality_observation(
    receipt: Option<&Value>,
    latest: Option<&Value>,
    finalized: Option<&Value>,
    required_confirmations: u64,
    require_finalized: bool,
) -> Value {
    let receipt_block_number = receipt
        .filter(|value| !value.is_null())
        .and_then(|value| gofin_evm::rpc_u64(&value["blockNumber"]).ok());
    let latest_block_number = latest.and_then(|value| gofin_evm::rpc_u64(value).ok());
    let finalized_block_number = finalized
        .filter(|value| !value.is_null())
        .and_then(|value| gofin_evm::rpc_u64(&value["number"]).ok());

    let cause = match receipt_block_number {
        None => "receipt_missing",
        Some(_) if latest_block_number.is_none() => "latest_head_unavailable",
        Some(block) => {
            let latest = latest_block_number.expect("checked above");
            let confirmations = if latest >= block {
                latest.saturating_sub(block).saturating_add(1)
            } else {
                0
            };
            if confirmations < required_confirmations {
                "confirmations_pending"
            } else if require_finalized && finalized_block_number.is_none() {
                "finalized_head_unavailable"
            } else if require_finalized
                && finalized_block_number.is_some_and(|finalized| finalized < block)
            {
                "finality_pending"
            } else {
                // `observe_receipt` still returned None despite both reported
                // heads being sufficient, so do not infer canonical inclusion.
                "inclusion_not_canonical"
            }
        }
    };

    json!({
        "receipt_block_number":receipt_block_number,
        "latest_block_number":latest_block_number,
        "finalized_block_number":finalized_block_number,
        "require_finalized":require_finalized,
        "cause":cause
    })
}

fn attach_bridge_finality_observation(mut locator: Value, progress: Value) -> Value {
    if let (Some(locator), Some(progress)) = (locator.as_object_mut(), progress.as_object()) {
        locator.insert("observation".into(), json!("pending_inclusion_or_finality"));
        for (key, value) in progress {
            locator.insert(key.clone(), value.clone());
        }
    }
    locator
}

fn bridge_external_pending_status(
    reconciliation: &gofin_bridge::BridgeReconciliation,
    candidate_transactions: Vec<Value>,
    candidate_observations: Vec<Value>,
    verified_candidates: usize,
) -> Value {
    json!({
        "status":reconciliation.status,
        "candidate_transactions":candidate_transactions,
        "candidate_notice":"Provider transaction hashes are locators only; GoFin has not verified settlement.",
        "candidate_observations":candidate_observations,
        "verified_candidates":verified_candidates,
        "settled":false
    })
}

fn reviewed_relay_config() -> ReviewedConfig {
    let mut chains = BTreeMap::new();
    for (chain_id, protocol_chain_id) in [
        (1, "ethereum"),
        (42161, "arbitrum"),
        (4663, "robinhood"),
        (8453, "base"),
    ] {
        chains.insert(
            chain_id,
            ReviewedChain {
                chain_id,
                protocol_chain_id: protocol_chain_id.into(),
                depository: RELAY_DEPOSITORY.into(),
                router_v3: Some(RELAY_ROUTER.into()),
                protocol: "relay".into(),
            },
        );
    }
    ReviewedConfig {
        chains,
        tokens: Default::default(),
        relay_solver_protocol_chain_id: "base".into(),
        settlement_hub_chain_id: 537713,
        settlement_hub_address: RELAY_HUB.into(),
    }
}

fn schema_for_gofin_error() -> Value {
    serde_json::to_value(schema_for!(GoFinError)).expect("error schema serializes")
}

fn validate_explicit_bounds(bounds: &BridgeBounds) -> gofin_domain::Result<()> {
    if bounds
        .input
        .as_ref()
        .is_some_and(|input| input.minimum > input.maximum)
        || bounds
            .input_usd_micros
            .as_ref()
            .is_some_and(|range| range.minimum > range.maximum)
        || bounds
            .source_usd_per_unit_micros
            .as_ref()
            .is_some_and(|range| range.minimum > range.maximum)
        || bounds.authorization_seconds == Some(0)
    {
        return Err(error(
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Bridge lower bounds must not exceed their upper bounds.",
            "Correct the explicit amount and price intervals.",
        ));
    }
    if bounds
        .fee_caps
        .iter()
        .any(|cap| cap.maximum.is_zero() || cap.maximum_usd_micros == Some(0))
    {
        return Err(error(
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Fee caps must be positive finite quantities and USD values.",
            "Provide a positive exact cap for each possible bridge fee.",
        ));
    }
    Ok(())
}

fn normalize_authorization_bounds(
    requested: Option<BridgeBounds>,
    saved: &SavedQuote,
) -> gofin_domain::Result<BridgeBounds> {
    let mut bounds = requested.unwrap_or_default();
    let quote_amount = saved.quote.intent.amount.clone();
    bounds
        .input
        .get_or_insert_with(|| gofin_bridge::AmountInterval {
            minimum: quote_amount.clone(),
            maximum: quote_amount.clone(),
        });
    bounds.maximum_source_debit.get_or_insert_with(|| {
        bounds
            .input
            .as_ref()
            .map(|range| range.maximum.clone())
            .unwrap_or_else(|| saved.quote.gross_debit.atoms.clone())
    });
    bounds
        .minimum_net_receipt
        .get_or_insert_with(|| saved.quote.minimum_receipt.clone());
    validate_explicit_bounds(&bounds)?;
    let input = bounds.input.as_ref().expect("normalized exact input");
    if input.minimum > quote_amount
        || input.maximum < quote_amount
        || bounds
            .maximum_source_debit
            .as_ref()
            .is_none_or(|maximum| maximum < &input.maximum)
    {
        return Err(error(
            "BRIDGE_BOUNDS_VIOLATED",
            "preview",
            "Chosen input and debit bounds do not include the exact stored quote amount.",
            "Choose bounds containing the quote's exact source amount or request a new quote.",
        ));
    }
    let receipt = bounds
        .minimum_net_receipt
        .as_ref()
        .expect("normalized receipt floor");
    if receipt.asset != saved.quote.minimum_receipt.asset
        || receipt.atoms.is_zero()
        || receipt.atoms > saved.quote.minimum_receipt.atoms
    {
        return Err(error(
            "BRIDGE_BOUNDS_VIOLATED",
            "preview",
            "Chosen receipt floor must use the exact destination asset, be positive, and not exceed the quote's guaranteed minimum.",
            "Choose a positive floor no higher than the quote's guarantee, or request a route with a stronger guarantee.",
        ));
    }
    for step in &saved.plan.steps {
        let gas = step.gas_asset()?;
        if !bounds.fee_caps.iter().any(|cap| {
            cap.kind == gofin_bridge::FeeKind::Gas && cap.asset == gas && !cap.maximum.is_zero()
        }) {
            return Err(error(
                "BRIDGE_FEE_CAP_REQUIRED",
                "preview",
                "A finite native gas cap is required for every wallet step.",
                "Add an explicit positive gas cap for each source-chain approval and deposit step.",
            ));
        }
    }
    Ok(bounds)
}

fn same_bridge_intent_except_amount(left: &BridgeIntent, right: &BridgeIntent) -> bool {
    let mut left = left.clone();
    left.amount = right.amount.clone();
    left == *right
}

fn bridge_source_chain_id(intent: &BridgeIntent) -> u64 {
    intent
        .source
        .as_ref()
        .map(|source| source.chain_id)
        .unwrap_or(792703809)
}

fn settlement_expectation(
    quote: &BridgeQuote,
    destination_floor: gofin_domain::Quantity,
) -> gofin_domain::BridgeSettlementExpectation {
    gofin_domain::BridgeSettlementExpectation {
        destination: vec![gofin_domain::BridgeSettlementTarget {
            resource: quote
                .intent
                .destination_resource()
                .expect("validated bridge destination"),
            asset: quote.expected_receipt.asset.clone(),
            minimum_atoms: destination_floor,
        }],
        refund: quote
            .refund_paths
            .iter()
            .filter(|path| {
                if path.network.as_deref() == Some(gofin_solana::NETWORK) {
                    path.recipient == quote.intent.refund_to
                } else {
                    path.recipient.eq_ignore_ascii_case(&quote.intent.refund_to)
                }
            })
            .map(|path| gofin_domain::BridgeSettlementTarget {
                resource: if path.network.as_deref() == Some(gofin_solana::NETWORK) {
                    gofin_domain::ResourceId(format!(
                        "{}:{}",
                        gofin_solana::NETWORK,
                        path.recipient
                    ))
                } else {
                    gofin_domain::ResourceId(format!(
                        "ethereum:{}:{}",
                        path.chain_id,
                        path.recipient.to_ascii_lowercase()
                    ))
                },
                asset: path.asset.clone(),
                minimum_atoms: if path.minimum.is_zero() {
                    gofin_domain::Quantity::from_u64(1)
                } else {
                    path.minimum.clone()
                },
            })
            .collect(),
    }
}

fn safe_label(value: &str) -> String {
    let clean = value
        .chars()
        .filter(|character| !character.is_control())
        .take(48)
        .collect::<String>();
    if clean.trim().is_empty() {
        "wallet".into()
    } else {
        clean
    }
}

fn chain_name(chain_id: u64) -> &'static str {
    gofin_networks::by_chain_id(chain_id)
        .map(|network| network.display_name)
        .unwrap_or("EVM")
}

fn format_amount(amount: &GoFinAmount) -> String {
    let atoms = amount.atoms.as_str();
    let decimals = amount.asset.decimals as usize;
    let mut digits = atoms.to_owned();
    if decimals == 0 {
        return format!("{digits} {}", short_asset_name(&amount.asset));
    }
    if digits.len() <= decimals {
        digits.insert_str(0, &"0".repeat(decimals + 1 - digits.len()));
    }
    let split = digits.len() - decimals;
    let fraction = digits.split_off(split);
    let fraction = fraction.trim_end_matches('0');
    let quantity = if fraction.is_empty() {
        digits
    } else {
        format!("{digits}.{fraction}")
    };
    format!("{quantity} {}", short_asset_name(&amount.asset))
}

fn short_asset_name(asset: &gofin_domain::AssetId) -> String {
    match asset.reference.as_str() {
        "native" if asset.network == gofin_solana::NETWORK => "SOL".into(),
        gofin_solana::funding::MAINNET_USDC_MINT
            if asset.network == gofin_solana::NETWORK && asset.decimals == 6 =>
        {
            "USDC".into()
        }
        "native" if gofin_networks::by_network_id(&asset.network).is_some() => {
            gofin_networks::by_network_id(&asset.network)
                .expect("reviewed network checked above")
                .native_symbol()
                .into()
        }
        value
            if value.eq_ignore_ascii_case(ETHEREUM_USDC)
                || value.eq_ignore_ascii_case(ARBITRUM_USDC) =>
        {
            "USDC".into()
        }
        value if value.eq_ignore_ascii_case(ROBINHOOD_USDG) => "USDG".into(),
        other => safe_label(other),
    }
}

fn format_fee_cap(cap: &gofin_bridge::FeeCap) -> String {
    let label = match cap.kind {
        gofin_bridge::FeeKind::Gas => "gas",
        gofin_bridge::FeeKind::AdditionalDebit => "additional fees",
        gofin_bridge::FeeKind::WithheldFromOutput => "withheld from output",
    };
    format!(
        "{} {label}",
        format_amount(&GoFinAmount {
            asset: cap.asset.clone(),
            atoms: cap.maximum.clone()
        })
    )
}

fn aggregate_fee_caps(caps: &[gofin_bridge::FeeCap]) -> Option<String> {
    let mut totals = BTreeMap::<gofin_domain::AssetId, gofin_domain::Quantity>::new();
    for cap in caps {
        let total = match totals.get(&cap.asset) {
            Some(current) => current.checked_add(&cap.maximum).ok()?,
            None => cap.maximum.clone(),
        };
        totals.insert(cap.asset.clone(), total);
    }
    (!totals.is_empty()).then(|| {
        totals
            .into_iter()
            .map(|(asset, atoms)| format_amount(&GoFinAmount { asset, atoms }))
            .collect::<Vec<_>>()
            .join(" and ")
    })
}

fn abbreviate_address(address: &str) -> String {
    if address.len() > 12 {
        format!("{}…{}", &address[..6], &address[address.len() - 4..])
    } else {
        address.to_owned()
    }
}

fn domain_step_kind(kind: &ProviderStepKind) -> BridgeStepKind {
    match kind {
        ProviderStepKind::Approval => BridgeStepKind::Approval,
        ProviderStepKind::ResetAllowance => BridgeStepKind::ResetAllowance,
        ProviderStepKind::Deposit => BridgeStepKind::Deposit,
        ProviderStepKind::Prove => BridgeStepKind::Prove,
        ProviderStepKind::Finalize => BridgeStepKind::Finalize,
        ProviderStepKind::Mint => BridgeStepKind::Mint,
    }
}

fn bridge_calls_match(
    left: &BridgeStep,
    right: &BridgeStep,
    approval_amount_may_change: bool,
) -> bool {
    match (
        &left.call,
        &right.call,
        &left.solana_mint,
        &right.solana_mint,
        &left.solana_transaction,
        &right.solana_transaction,
    ) {
        (Some(left), Some(right), None, None, None, None) => {
            left.chain_id == right.chain_id
                && left.to.eq_ignore_ascii_case(&right.to)
                && left.value == right.value
                && (approval_amount_may_change || left.data.eq_ignore_ascii_case(&right.data))
        }
        (None, None, Some(left), Some(right), None, None) => left == right,
        (None, None, None, None, Some(left), Some(right)) => left == right,
        _ => false,
    }
}
fn same_bridge_step_topology(left: &[BridgeStep], right: &[BridgeStep]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.ordinal == right.ordinal
                && left.kind == right.kind
                && left.dependencies == right.dependencies
                && bridge_calls_match(left, right, false)
                && if left.solana_mint.is_some() || left.solana_transaction.is_some() {
                    left.expected_sender == right.expected_sender
                        && left.maximum_debit == right.maximum_debit
                } else {
                    left.expected_sender
                        .eq_ignore_ascii_case(&right.expected_sender)
                        && left.maximum_debit == right.maximum_debit
                }
        })
}

// Parent state records the transfer's stage; child links determine readiness.
fn ready_source_child(transfer: &BridgeTransferRecord) -> Option<&BridgeStepLink> {
    transfer.steps.iter().find(|step| {
        matches!(
            step.kind,
            BridgeStepKind::ResetAllowance | BridgeStepKind::Approval | BridgeStepKind::Deposit
        ) && step.state == BridgeStepState::Unsubmitted
            && step.operation_id.is_none()
            && step.depends_on.iter().all(|dependency| {
                transfer.steps.iter().any(|prior| {
                    prior.step_id == *dependency && prior.state == BridgeStepState::Settled
                })
            })
    })
}

fn step_kind_name(kind: &BridgeStepKind) -> &'static str {
    match kind {
        BridgeStepKind::Approval => "approval",
        BridgeStepKind::ResetAllowance => "reset_allowance",
        BridgeStepKind::Deposit => "deposit",
        BridgeStepKind::Prove => "prove",
        BridgeStepKind::Finalize => "finalize",
        BridgeStepKind::Mint => "mint",
    }
}

fn maximum_gas_cap(
    bounds: &BridgeBounds,
    chain_id: u64,
) -> gofin_domain::Result<gofin_domain::Quantity> {
    let asset = gofin_domain::AssetId {
        network: format!("ethereum:{chain_id}"),
        reference: "native".into(),
        decimals: 18,
    };
    bounds
        .fee_caps
        .iter()
        .find(|cap| cap.kind == gofin_bridge::FeeKind::Gas && cap.asset == asset)
        .map(|cap| cap.maximum.clone())
        .ok_or_else(|| {
            error(
                "BRIDGE_FEE_CAP_REQUIRED",
                "preview",
                "An explicit native gas fee cap is required for every wallet step.",
                "Add a gas fee cap for the source EVM chain and request a new quote.",
            )
        })
}

fn cumulative_gas_cost_caps(
    bounds: &BridgeBounds,
    plan: &PreparedBridge,
    resource: &gofin_domain::ResourceId,
) -> gofin_domain::Result<Vec<CostBound>> {
    let mut caps = Vec::new();
    for step in &plan.steps {
        let asset = step.gas_asset()?;
        if caps
            .iter()
            .any(|cost: &CostBound| cost.amount.asset == asset)
        {
            continue;
        }
        caps.push(CostBound {
            location: AccountingLocation::Cash {
                resource: bridge_step_resource(&plan.quote.intent, step)?,
            },
            amount: GoFinAmount {
                atoms: maximum_gas_asset_cap(bounds, &asset)?,
                asset,
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let _ = resource;
    Ok(caps)
}

fn bridge_step_resource(
    intent: &BridgeIntent,
    step: &BridgeStep,
) -> gofin_domain::Result<gofin_domain::ResourceId> {
    if let Some(solana) = &step.solana_mint {
        return Ok(solana.destination.wallet_resource.clone());
    }
    if step.solana_transaction.is_some() {
        return Ok(intent.wallet.clone());
    }
    Ok(bridge_stage_resource(intent, step.evm_call()?.chain_id))
}
fn maximum_gas_asset_cap(
    bounds: &BridgeBounds,
    asset: &gofin_domain::AssetId,
) -> gofin_domain::Result<Quantity> {
    bounds
        .fee_caps
        .iter()
        .find(|cap| cap.kind == gofin_bridge::FeeKind::Gas && &cap.asset == asset)
        .map(|cap| cap.maximum.clone())
        .ok_or_else(|| {
            GoFinError::validation(
                "Every bridge execution network requires a finite native gas cap.",
            )
        })
}
fn bridge_step_execution(step: &BridgeStep) -> gofin_domain::Result<Value> {
    if let Some(solana) = &step.solana_transaction {
        if step.call.is_some() || step.solana_mint.is_some() {
            return Err(GoFinError::validation("Ambiguous bridge execution family."));
        }
        return Ok(
            json!({"network":gofin_solana::NETWORK,"solana_order_id":solana.order_id,"solana_request_id":solana.request_id}),
        );
    }
    if let Some(solana) = &step.solana_mint {
        if step.call.is_some() {
            return Err(GoFinError::validation("Ambiguous bridge execution family."));
        }
        return Ok(json!({"network":solana.destination.network,"solana_mint":solana}));
    }
    let call = step.evm_call()?;
    Ok(json!({"chain_id":call.chain_id,"to":call.to.to_ascii_lowercase()}))
}

fn bridge_stage_resource(intent: &BridgeIntent, chain_id: u64) -> gofin_domain::ResourceId {
    if intent
        .source
        .as_ref()
        .is_some_and(|source| chain_id == source.chain_id)
    {
        intent.wallet.clone()
    } else {
        gofin_domain::ResourceId(format!(
            "ethereum:{chain_id}:{}",
            intent.source_address.to_ascii_lowercase()
        ))
    }
}

fn push_plan_cost_cap(
    costs: &mut Vec<CostBound>,
    resource: &gofin_domain::ResourceId,
    asset: &gofin_domain::AssetId,
    amount: &Quantity,
) -> gofin_domain::Result<()> {
    let location = AccountingLocation::Cash {
        resource: resource.clone(),
    };
    if let Some(existing) = costs.iter_mut().find(|cost| {
        cost.location == location
            && cost.amount.asset == *asset
            && cost.treatment == CostTreatment::AdditionalDebit
    }) {
        existing.amount.atoms = existing.amount.atoms.checked_add(amount)?;
    } else {
        costs.push(CostBound {
            location,
            amount: GoFinAmount {
                asset: asset.clone(),
                atoms: amount.clone(),
            },
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    Ok(())
}

/// Check that an adapter-produced normalized plan still has the saved route's
/// ordered call topology. Approval calldata is the only allowed difference:
/// its finite amount is regenerated from the selected maximum source debit.
fn same_bridge_plan_for_permission(left: &[BridgeStep], right: &[BridgeStep]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.ordinal == right.ordinal
                && left.kind == right.kind
                && left.dependencies == right.dependencies
                && bridge_calls_match(left, right, left.kind == ProviderStepKind::Approval)
                && if left.solana_mint.is_some() || left.solana_transaction.is_some() {
                    left.expected_sender == right.expected_sender
                        && left.maximum_debit == right.maximum_debit
                } else {
                    left.expected_sender
                        .eq_ignore_ascii_case(&right.expected_sender)
                        && left.maximum_debit == right.maximum_debit
                }
        })
}

fn fixed_plan_step_identity_matches(fixed: &Value, binding: &Value) -> bool {
    let Some(identity) = fixed.get("plan_step_identity") else {
        return true;
    };
    identity["transfer_id"] == binding["transfer_id"]
        && identity["step_id"] == binding["step_id"]
        && identity["step_kind"] == binding["step_kind"]
        && fixed["provider"] == binding["quote"]["provider"]
}

fn bridge_boundary_spec(
    prepared: &PreparedOperation,
    raw_bounds: &Value,
) -> gofin_domain::Result<gofin_domain::boundaries::BoundarySpec> {
    let bounds: BridgeBounds = serde_json::from_value(raw_bounds.clone()).map_err(|_| {
        GoFinError::validation(
            "Use the bridge bounds schema with explicit input, receipt and fee caps.",
        )
    })?;
    let transfer_id = prepared.binding["transfer_id"]
        .as_str()
        .ok_or_else(|| GoFinError::validation("Bridge transfer identity is missing."))?;
    let plan: PreparedBridge =
        serde_json::from_value(prepared.binding["plan"].clone()).map_err(|_| {
            GoFinError::validation("Normalized bridge plan is missing from the approved binding.")
        })?;
    let step_ids = prepared.binding["step_ids"].as_object().ok_or_else(|| {
        GoFinError::validation("Persisted step identities are missing from the approved binding.")
    })?;
    let intent: BridgeIntent = serde_json::from_value(prepared.binding["intent"].clone())
        .or_else(|_| serde_json::from_value(prepared.binding["quote"]["intent"].clone()))
        .map_err(|_| {
            GoFinError::validation("Bridge intent is missing from the approved binding.")
        })?;
    let prepared_bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
        .map_err(|_| {
            GoFinError::validation("Normalized bridge bounds are missing from the prepared step.")
        })?;
    if bounds != prepared_bounds {
        return Err(GoFinError::validation(
            "Bridge bounds differ from the exact normalized bounds selected for this step.",
        ));
    }
    let source_cap = bounds
        .maximum_source_debit
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Bridge maximum source debit is missing."))?
        .clone();
    let source_asset_id = intent.source_asset_id()?;
    let mut maximum_debits = Vec::new();
    for step in &plan.steps {
        for maximum in &step.maximum_debit {
            let mut amount = maximum.clone();
            if matches!(step.kind, ProviderStepKind::Deposit) && amount.asset == source_asset_id {
                amount.atoms = source_cap.clone();
            }
            maximum_debits.push(ResourceAmount {
                resource: intent.wallet.clone(),
                amount,
            });
        }
        if matches!(step.kind, ProviderStepKind::Deposit)
            && step
                .maximum_debit
                .iter()
                .all(|value| value.asset != source_asset_id)
            && (intent
                .source
                .as_ref()
                .is_some_and(|source| source.token.is_none())
                || intent
                    .solana_source
                    .as_ref()
                    .is_some_and(|source| source.mint.is_none()))
        {
            maximum_debits.push(ResourceAmount {
                resource: intent.wallet.clone(),
                amount: intent.source_amount(source_cap.clone())?,
            });
        }
    }
    let mut maximum_costs = cumulative_gas_cost_caps(&bounds, &plan, &intent.wallet)?;
    for fee in bounds
        .fee_caps
        .iter()
        .filter(|fee| fee.kind == gofin_bridge::FeeKind::AdditionalDebit)
    {
        let resource =
            if fee.asset.network == gofin_solana::NETWORK && intent.solana_destination.is_some() {
                intent.destination_resource()?
            } else {
                intent.wallet.clone()
            };
        push_plan_cost_cap(&mut maximum_costs, &resource, &fee.asset, &fee.maximum)?;
    }
    let allowed_steps = plan
        .steps
        .iter()
        .map(|step| {
            let step_id = step_ids
                .get(&step.ordinal.to_string())
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("A declared step has no persisted identity.")
                })?;
            let mut descriptor = bridge_step_execution(step)?;
            descriptor["step_id"] = json!(step_id);
            descriptor["step_kind"] = json!(domain_step_kind(&step.kind));
            descriptor["dependencies"] = json!(
                step.dependencies
                    .iter()
                    .filter_map(|ordinal| step_ids
                        .get(&ordinal.to_string())
                        .and_then(Value::as_str))
                    .collect::<Vec<_>>()
            );
            Ok(descriptor)
        })
        .collect::<gofin_domain::Result<Vec<_>>>()?;
    maximum_debits.sort_by(|a, b| {
        (a.resource.0.as_str(), &a.amount.asset).cmp(&(b.resource.0.as_str(), &b.amount.asset))
    });
    maximum_costs.sort_by(|a, b| {
        (&a.amount.asset, &a.amount.atoms).cmp(&(&b.amount.asset, &b.amount.atoms))
    });
    Ok(gofin_domain::boundaries::BoundarySpec {
        fixed: json!({"transfer_id":transfer_id,"wallet":intent.wallet,"source":intent.source,"solana_source":intent.solana_source,"source_address":intent.source_address,"destination_chain_id":intent.destination_chain_id,"solana_destination":intent.solana_destination,"destination_assets":intent.destination_assets,"recipient":intent.recipient,"refund_to":intent.refund_to,"allowed_refund_assets":intent.allowed_refund_assets,"allowed_solana_refund_assets":intent.allowed_solana_refund_assets,"route_id":prepared.binding["route_id"],"plan_digest":prepared.binding["plan_digest"],"steps":allowed_steps}),
        parameters: serde_json::to_value(&bounds).map_err(|_| internal_error())?,
        maximum_debits,
        maximum_costs,
        summary: finite_bridge_boundary_summary(&prepared.summary, &plan, &intent, &bounds)?,
    })
}

fn finite_bridge_boundary_summary(
    current_summary: &str,
    plan: &PreparedBridge,
    intent: &BridgeIntent,
    bounds: &BridgeBounds,
) -> gofin_domain::Result<String> {
    let source_asset = intent.source_asset_id()?;
    let source_amount = |atoms: gofin_domain::Quantity| GoFinAmount {
        asset: source_asset.clone(),
        atoms,
    };
    let max_source = bounds
        .maximum_source_debit
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Bridge maximum source debit is missing."))?;
    let spender = plan
        .quote
        .approval
        .as_ref()
        .map(|approval| abbreviate_address(&approval.spender))
        .unwrap_or_else(|| "reviewed Relay spender".into());
    let input = bounds
        .input
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Bridge input bounds are missing."))?;
    let steps = plan
        .steps
        .iter()
        .map(|step| match step.kind {
            ProviderStepKind::ResetAllowance => Ok(format!(
                "reset token allowance to {} for {} spender {spender}",
                format_amount(&source_amount(gofin_domain::Quantity::zero())),bridge_provider_label(&plan.quote.provider)
            )),
            ProviderStepKind::Approval => Ok(format!(
                "approve up to {} for {} spender {spender}",
                format_amount(&source_amount(max_source.clone())),bridge_provider_label(&plan.quote.provider)
            )),
            ProviderStepKind::Deposit => {
                let input_scope = if input.minimum == input.maximum {
                    format_amount(&source_amount(input.maximum.clone()))
                } else {
                    format!(
                        "{}–{}",
                        format_amount(&source_amount(input.minimum.clone())),
                        format_amount(&source_amount(input.maximum.clone()))
                    )
                };
                Ok(format!("{} source input {input_scope}", if plan.quote.provider == "cctp" { "burn" } else { "deposit" }))
            }
            ProviderStepKind::Prove => Ok("prove the original withdrawal on Ethereum; witness and simulation deferred until source proof".into()),
            ProviderStepKind::Finalize => Ok("finalize the original withdrawal on Ethereum after current portal readiness checks".into()),
            ProviderStepKind::Mint => Ok(if step.solana_mint.is_some() {"mint the original CCTP burn to the approved Solana native-USDC ATA; SOL gas and nonrefundable nonce-account funding required".into()} else {"mint the original CCTP burn on the approved destination after attestation validation; destination ETH gas required".into()}),
        })
        .collect::<gofin_domain::Result<Vec<_>>>()?
        .join("; ");
    let candidate_summary = current_summary
        .split_once(" USD ceilings: $")
        .map(|(summary, _)| summary)
        .unwrap_or(current_summary)
        .trim_end();
    let summary = candidate_summary.replace("; Max fees ", "; Cumulative max fees ");
    let explicit_usd = explicit_bridge_usd_bounds(bounds);
    let explicit_usd = if explicit_usd.is_empty() {
        String::new()
    } else {
        format!(" Explicit USD limits: {}.", explicit_usd.join("; "))
    };
    Ok(format!(
        "{summary} Finite grant covers these declared steps: {steps}.{explicit_usd}"
    ))
}

fn explicit_bridge_usd_bounds(bounds: &BridgeBounds) -> Vec<String> {
    let mut limits = Vec::new();
    if let Some(input) = &bounds.input_usd_micros {
        limits.push(format!(
            "input value {}–{}",
            format_usd_quantity(&input.minimum),
            format_usd_quantity(&input.maximum)
        ));
    }
    for fee in &bounds.fee_caps {
        if let Some(maximum) = fee.maximum_usd_micros {
            limits.push(format!(
                "{} fee ≤{}",
                match fee.kind {
                    gofin_bridge::FeeKind::Gas => "gas",
                    gofin_bridge::FeeKind::AdditionalDebit => "additional",
                    gofin_bridge::FeeKind::WithheldFromOutput => "withheld",
                },
                format_usd_micros(maximum)
            ));
        }
    }
    if let Some(maximum) = bounds.maximum_total_usd_micros {
        limits.push(format!("total ≤{}", format_usd_micros(maximum)));
    }
    limits
}

fn format_usd_micros(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fractional = micros % 1_000_000;
    let value = format!("${whole}.{fractional:06}");
    value.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn format_usd_quantity(micros: &gofin_domain::Quantity) -> String {
    format!("${}", micros.to_decimal(6))
}

fn prospective_debit_update_before_source_approval(
    record: &BridgeTransferRecord,
    current: &BridgeStepLink,
) -> bool {
    if record.grant_id.is_some()
        || record.approved_bounds.is_some()
        || record.transit.is_some()
        || !matches!(
            current.kind,
            BridgeStepKind::ResetAllowance | BridgeStepKind::Approval
        )
        || !matches!(
            current.state,
            BridgeStepState::Unsubmitted | BridgeStepState::Submitted
        )
    {
        return false;
    }
    let settled_resets = record
        .steps
        .iter()
        .filter(|step| {
            step.kind == BridgeStepKind::ResetAllowance
                && step.state == BridgeStepState::Settled
                && step.operation_id.is_some()
                && step.grant_id.is_none()
        })
        .collect::<Vec<_>>();
    let reset_history_is_safe = match current.kind {
        BridgeStepKind::ResetAllowance => settled_resets.is_empty(),
        BridgeStepKind::Approval => {
            settled_resets.len() <= 1
                && settled_resets
                    .first()
                    .is_none_or(|reset| current.depends_on.contains(&reset.step_id))
        }
        BridgeStepKind::Deposit
        | BridgeStepKind::Prove
        | BridgeStepKind::Finalize
        | BridgeStepKind::Mint => false,
    };
    if !reset_history_is_safe {
        return false;
    }
    record.steps.iter().all(|step| {
        if step.step_id == current.step_id {
            step.kind == current.kind
                && step.grant_id.is_none()
                && matches!(
                    step.state,
                    BridgeStepState::Unsubmitted | BridgeStepState::Submitted
                )
                && (step.state == BridgeStepState::Unsubmitted || step.operation_id.is_some())
        } else if step.kind == BridgeStepKind::ResetAllowance
            && step.state == BridgeStepState::Settled
        {
            step.operation_id.is_some() && step.grant_id.is_none()
        } else {
            step.state == BridgeStepState::Unsubmitted
                && step.operation_id.is_none()
                && step.grant_id.is_none()
        }
    })
}

fn bridge_step_execution_matches(approved: &Value, candidate: &Value) -> bool {
    if approved.get("solana_order_id").is_some() {
        return candidate.get("call").is_none_or(Value::is_null)
            && candidate.get("solana_mint").is_none_or(Value::is_null)
            && approved["network"] == gofin_solana::NETWORK
            && approved["solana_order_id"] == candidate["solana_transaction"]["order_id"]
            && approved["solana_request_id"] == candidate["solana_transaction"]["request_id"];
    }
    if let Some(solana) = approved.get("solana_mint").filter(|value| !value.is_null()) {
        return candidate.get("call").is_none_or(Value::is_null)
            && approved["network"] == solana["destination"]["network"]
            && approved["network"] == gofin_solana::NETWORK
            && candidate.get("solana_mint") == Some(solana);
    }
    candidate.get("solana_mint").is_none_or(Value::is_null)
        && approved["chain_id"].is_u64()
        && approved["chain_id"] == candidate["call"]["chain_id"]
        && approved["to"]
            == candidate["call"]["to"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
}

fn validate_bridge_execute_candidate(
    spec: &gofin_domain::boundaries::BoundarySpec,
    prepared: &PreparedOperation,
) -> gofin_domain::Result<()> {
    let approved = &spec.fixed["bridge_execute_scope"];
    let quote: BridgeQuote = serde_json::from_value(prepared.binding["quote"].clone())
        .map_err(|_| GoFinError::validation("Nominated bridge quote binding is malformed."))?;
    let bounds: BridgeBounds = serde_json::from_value(spec.parameters["bounds"].clone())
        .map_err(|_| GoFinError::validation("Approved bridge intent bounds are malformed."))?;
    let approved_destinations = approved["destination_assets"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Approved bridge destination set is malformed."))?;
    let approved_providers = approved["allowed_providers"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Approved bridge provider set is malformed."))?;
    let destination_is_allowed = quote.intent.destination_assets.len() == 1
        && approved_destinations
            .iter()
            .any(|asset| asset == &json!(quote.intent.destination_assets[0]));
    let provider_is_allowed = approved_providers
        .iter()
        .any(|provider| provider.as_str() == Some(quote.provider.as_str()));
    let amount_interval: gofin_bridge::AmountInterval =
        serde_json::from_value(approved["amount"].clone())
            .map_err(|_| GoFinError::validation("Approved bridge amount interval is malformed."))?;
    let source_debit_cap = bounds
        .maximum_source_debit
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Approved bridge source debit cap is missing."))?;
    let input_bounds = bounds
        .input
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Approved bridge input interval is missing."))?;
    let minimum_receipt = bounds
        .minimum_net_receipt
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Approved bridge receipt floor is missing."))?;
    let candidate_in_scope = provider_is_allowed
        && quote.intent.wallet.0 == approved["wallet"].as_str().unwrap_or_default()
        && quote
            .intent
            .source_address
            .eq_ignore_ascii_case(approved["source_address"].as_str().unwrap_or_default())
        && json!(quote.intent.source) == approved["source"]
        && quote.intent.evm_destination_chain_id().ok()
            == approved["destination_chain_id"].as_u64()
        && destination_is_allowed
        && quote
            .intent
            .recipient
            .eq_ignore_ascii_case(approved["recipient"].as_str().unwrap_or_default())
        && quote
            .intent
            .refund_to
            .eq_ignore_ascii_case(approved["refund_to"].as_str().unwrap_or_default())
        && quote.intent.allowed_refund_assets.iter().all(|asset| {
            approved["allowed_refund_assets"]
                .as_array()
                .is_some_and(|assets| assets.iter().any(|approved| approved == &json!(asset)))
        })
        && quote.intent.amount >= amount_interval.minimum
        && quote.intent.amount <= amount_interval.maximum
        && quote.intent.amount >= input_bounds.minimum
        && quote.intent.amount <= input_bounds.maximum
        && quote.gross_debit.asset == quote.intent.source_asset_id()?
        && quote.gross_debit.atoms <= *source_debit_cap
        && quote.minimum_receipt.asset == minimum_receipt.asset
        && quote.minimum_receipt.atoms >= minimum_receipt.atoms;
    let selected_bounds: BridgeBounds = serde_json::from_value(prepared.binding["bounds"].clone())
        .map_err(|_| GoFinError::validation("Nominated bridge bounds are malformed."))?;
    if !candidate_in_scope || selected_bounds != bounds {
        return Err(gofin_domain::boundaries::violation_error(
            "Nominated bridge quote changes the approved intent or exceeds its finite bounds.",
            gofin_domain::boundaries::BoundaryViolation {
                constraint: "fixed_scope".into(),
                approved_bound: spec.fixed.clone(),
                candidate_value: prepared.binding.clone(),
                evidence_time: Some(prepared.observed_at.clone()),
                reauthorization_required: true,
                reconciliation_required: false,
            },
        ));
    }
    Ok(())
}

fn validate_bridge_boundary(
    spec: &gofin_domain::boundaries::BoundarySpec,
    prepared: &PreparedOperation,
) -> gofin_domain::Result<()> {
    if spec.fixed["transfer_id"] != prepared.binding["transfer_id"]
        || spec.fixed["route_id"] != prepared.binding["route_id"]
        || spec.fixed["plan_digest"] != prepared.binding["plan_digest"]
        || !fixed_plan_step_identity_matches(&spec.fixed, &prepared.binding)
        || !spec.fixed["steps"].as_array().is_some_and(|steps| {
            steps.iter().any(|entry| {
                entry["step_id"] == prepared.binding["step_id"]
                    && entry["step_kind"] == prepared.binding["step_kind"]
                    && bridge_step_execution_matches(entry, &prepared.binding["step"])
                    && entry["dependencies"] == prepared.binding["step_dependencies"]
            })
        })
    {
        return Err(gofin_domain::boundaries::violation_error(
            "Bridge candidate changes fixed transfer or route identity.",
            gofin_domain::boundaries::BoundaryViolation {
                constraint: "fixed_scope".into(),
                approved_bound: spec.fixed.clone(),
                candidate_value: prepared.binding.clone(),
                evidence_time: Some(prepared.observed_at.clone()),
                reauthorization_required: true,
                reconciliation_required: true,
            },
        ));
    }
    let caps = spec
        .maximum_debits
        .iter()
        .map(|bound| {
            (
                bound.resource.clone(),
                bound.amount.asset.clone(),
                bound.amount.atoms.clone(),
            )
        })
        .collect::<Vec<_>>();
    for debit in &prepared.intent.debits {
        if !caps.iter().any(|(resource, asset, amount)| {
            resource == &debit.resource
                && asset == &debit.amount.asset
                && debit.amount.atoms <= *amount
        }) {
            return Err(GoFinError::validation(
                "Bridge step debit exceeds its finite authorization boundary.",
            ));
        }
    }
    for cost in &prepared.intent.maximum_costs {
        if !spec.maximum_costs.iter().any(|cap| {
            cap.location == cost.location
                && cap.amount.asset == cost.amount.asset
                && cost.amount.atoms <= cap.amount.atoms
        }) {
            return Err(GoFinError::validation(
                "Bridge step maximum fees exceed the finite authorization boundary.",
            ));
        }
    }
    Ok(())
}

async fn reconcile_wallet_step(
    adapter: &BridgeAdapter,
    record: &gofin_domain::OperationRecord,
) -> gofin_domain::Result<gofin_domain::Reconciliation> {
    let submission = record
        .submission
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Original bridge submission identity is missing."))?;
    let preview = record
        .preview
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Original bridge preview is missing."))?;
    if !matches!(
        record.operation.as_str(),
        "bridge.transfer" | "bridge.execute"
    ) || preview.operation != record.operation
        || preview.input != record.input
    {
        return Err(error(
            "BRIDGE_RECORD_IDENTITY_MISMATCH",
            "reconciliation",
            "Bridge operation differs from its original persisted preview.",
            "Keep the transfer unresolved and inspect the original operation.",
        ));
    }
    let transaction: FrozenTransaction =
        serde_json::from_value(preview.binding["transaction"].clone()).map_err(|_| {
            GoFinError::validation("Original bridge transaction binding is malformed.")
        })?;
    if submission.kind != "ethereum" || submission.data["transaction"] != json!(transaction) {
        return Err(error(
            "BRIDGE_SUBMISSION_IDENTITY_MISMATCH",
            "reconciliation",
            "Persisted bridge submission differs from its original frozen envelope.",
            "Keep the transfer unresolved and reconcile the original submission.",
        ));
    }
    let runtime = adapter
        .wallet_adapters
        .get(&transaction.chain_id)
        .ok_or_else(|| {
            error(
                "BRIDGE_NETWORK_UNAVAILABLE",
                "reconciliation",
                "Original bridge network observer is unavailable.",
                "Retry after its reviewed RPC is available.",
            )
        })?
        .runtime
        .clone();
    let config = &adapter.wallet_adapters[&transaction.chain_id].config;
    let sequencer_inclusion =
        gofin_ethereum::EthereumConfig::external_credit_uses_sequencer(transaction.chain_id);
    let required_confirmations = if sequencer_inclusion {
        1
    } else {
        config.confirmations.max(1)
    };
    let exact_receipt = match runtime
        .receipt(submission, &transaction, required_confirmations)
        .await?
    {
        Some(receipt) => receipt,
        None => {
            return Ok(gofin_domain::Reconciliation {
                status: Status::Pending,
                evidence: json!({"candidate":submission.reference,"transaction_envelope_verified":false,"canonical_finality":false}),
                verified: None,
            });
        }
    };
    let receipt = if config.require_finalized && !sequencer_inclusion {
        let Some(observed) = runtime
            .observe_receipt(&submission.reference, required_confirmations, true)
            .await?
        else {
            return Ok(gofin_domain::Reconciliation {
                status: Status::Pending,
                evidence: json!({"candidate":submission.reference,"transaction_envelope_verified":true,"canonical_finality":false}),
                verified: None,
            });
        };
        if !bridge_receipts_match(&exact_receipt, &observed) {
            return Ok(gofin_domain::Reconciliation {
                status: Status::Pending,
                evidence: json!({"candidate":submission.reference,"transaction_envelope_verified":true,"canonical_finality":false,"receipt_observation":"inclusion_or_fee_changed"}),
                verified: None,
            });
        }
        observed
    } else {
        exact_receipt
    };
    if !receipt.success {
        let mut failure_effects = Vec::new();
        if !receipt.fee.is_zero() {
            let resource: gofin_domain::ResourceId =
                serde_json::from_value(preview.binding["resource"].clone()).map_err(|_| {
                    GoFinError::validation("Bridge resource identity is malformed.")
                })?;
            failure_effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: GoFinAmount {
                    asset: gofin_domain::AssetId {
                        network: format!("ethereum:{}", transaction.chain_id),
                        reference: "native".into(),
                        decimals: 18,
                    },
                    atoms: gofin_evm::quantity(receipt.fee),
                },
                category: "gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        return Ok(gofin_domain::Reconciliation {
            status: Status::Failed,
            evidence: json!({"candidate":submission.reference,"chain_id":transaction.chain_id,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"canonical":true,"success":false,"receipt":receipt.receipt}),
            verified: Some(verified_bundle(
                record,
                submission.clone(),
                failure_effects,
                json!({"receipt":receipt.receipt,"status":"reverted","chain_id":transaction.chain_id,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash)}),
            )?),
        });
    }
    let kind: BridgeStepKind = serde_json::from_value(preview.binding["step_kind"].clone())
        .map_err(|_| GoFinError::validation("Bridge step kind is malformed."))?;
    let transfer_id = preview.binding["transfer_id"].as_str().unwrap_or_default();
    let step = serde_json::from_value::<BridgeStep>(preview.binding["step"].clone())
        .map_err(|_| GoFinError::validation("Normalized bridge step is malformed."))?;
    let (amount_effects, source_proof) = match kind {
        BridgeStepKind::Approval | BridgeStepKind::ResetAllowance => (
            verify_allowance_step(adapter, &step, &preview.binding, &receipt).await?,
            None,
        ),
        BridgeStepKind::Deposit => {
            verify_source_deposit(&step, &preview.binding, &receipt, transfer_id, &runtime).await?
        }
        BridgeStepKind::Mint => {
            cctp_lifecycle::verify_mint_step(&preview.binding, &receipt)?;
            (vec![], None)
        }
        BridgeStepKind::Prove | BridgeStepKind::Finalize => {
            let locator: gofin_domain::BridgeDestinationLocator =
                serde_json::from_value(preview.binding["source_locator"].clone())
                    .map_err(|_| GoFinError::validation("Stage source locator is malformed."))?;
            let verified = saved_base_withdrawal(&locator)?;
            let logs = receipt.receipt["logs"]
                .as_array()
                .ok_or_else(internal_error)?
                .iter()
                .map(canonical_raw_log)
                .collect::<gofin_domain::Result<Vec<_>>>()?;
            if kind == BridgeStepKind::Prove {
                gofin_bridge::base_canonical::verify_withdrawal_proven(
                    &logs,
                    &verified.withdrawal,
                    &transaction.sender,
                )?;
            } else {
                gofin_bridge::base_canonical::verify_withdrawal_finalized(
                    &logs,
                    &verified.withdrawal_hash,
                )?;
            }
            (vec![], None)
        }
    };
    let mut effects = amount_effects;
    if !receipt.fee.is_zero() {
        let resource: gofin_domain::ResourceId =
            serde_json::from_value(preview.binding["resource"].clone())
                .map_err(|_| GoFinError::validation("Bridge resource identity is malformed."))?;
        effects.push(FinancialEffect::Cost {
            location: AccountingLocation::Cash { resource },
            amount: GoFinAmount {
                asset: gofin_domain::AssetId {
                    network: format!("ethereum:{}", transaction.chain_id),
                    reference: "native".into(),
                    decimals: 18,
                },
                atoms: gofin_evm::quantity(receipt.fee),
            },
            category: "gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        });
    }
    let mut evidence = json!({"transaction_hash":submission.reference,"chain_id":transaction.chain_id,"block_number":receipt.block_number,"block_hash":format!("{:#x}",receipt.block_hash),"confirmations":receipt.confirmations,"receipt":receipt.receipt,"transfer_id":transfer_id,"step_id":preview.binding["step_id"]});
    if let Some(source_proof) = source_proof {
        evidence["evidence_id"] = json!(source_proof.evidence_id);
        evidence["log_index"] = json!(source_proof.log_index);
        evidence["correlation"] = json!(source_proof.correlation);
        if let Some(locator) = source_proof.destination_locator {
            evidence["destination_locator"] =
                serde_json::to_value(locator).map_err(|_| internal_error())?;
        }
        if let Some((resource, amount)) = effects.iter().find_map(|effect| match effect {
            FinancialEffect::CashDebit { resource, amount } => Some((resource, amount)),
            _ => None,
        }) {
            evidence["recipient_resource"] = json!(resource.0);
            evidence["resource"] = json!(resource.0);
            evidence["asset"] = json!(amount.asset);
            evidence["amount_atoms"] = json!(amount.atoms);
        }
    }
    Ok(gofin_domain::Reconciliation {
        status: Status::Completed,
        verified: Some(verified_bundle(
            record,
            submission.clone(),
            effects,
            evidence.clone(),
        )?),
        evidence,
    })
}

fn bridge_receipts_match(
    exact_envelope: &CanonicalReceipt,
    finalized_observation: &CanonicalReceipt,
) -> bool {
    exact_envelope.hash == finalized_observation.hash
        && exact_envelope.block_hash == finalized_observation.block_hash
        && exact_envelope.block_number == finalized_observation.block_number
        && exact_envelope.success == finalized_observation.success
        && exact_envelope.fee == finalized_observation.fee
        && exact_envelope
            .transaction_sender
            .eq_ignore_ascii_case(&finalized_observation.transaction_sender)
}

fn verified_bundle(
    record: &gofin_domain::OperationRecord,
    submission: gofin_domain::Submission,
    effects: Vec<FinancialEffect>,
    evidence: Value,
) -> gofin_domain::Result<gofin_domain::VerifiedEffects> {
    let mut verified = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission,
        effects,
        evidence,
        observed_at: now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified)?;
    let _ = record;
    Ok(verified)
}

async fn verify_source_deposit(
    step: &BridgeStep,
    binding: &Value,
    receipt: &CanonicalReceipt,
    transfer_id: &str,
    runtime: &gofin_evm::EvmRuntime,
) -> gofin_domain::Result<(Vec<FinancialEffect>, Option<VerifiedSourceProof>)> {
    let quote: BridgeQuote = serde_json::from_value(binding["quote"].clone())
        .map_err(|_| GoFinError::validation("Stored bridge quote is malformed."))?;
    if quote.provider == "cctp" {
        return cctp_lifecycle::verify_source(&quote, receipt, transfer_id);
    }
    if quote.provider == "arbitrum_canonical" {
        return verify_canonical_source_deposit(
            step,
            binding,
            receipt,
            transfer_id,
            &quote,
            runtime,
        )
        .await;
    }
    if quote.provider == "base_canonical" {
        let raw = receipt.receipt["logs"]
            .as_array()
            .ok_or_else(|| GoFinError::validation("Base source logs are missing."))?
            .iter()
            .map(canonical_raw_log)
            .collect::<gofin_domain::Result<Vec<_>>>()?;
        let (log_index, correlation, transaction_hash, message_hash, scheme, proof) =
            if quote.intent.evm_source()?.chain_id == 1 {
                let deposit = gofin_bridge::base_canonical::verify_deposit_source(
                    &raw,
                    &quote.intent,
                    &format!("{:#x}", receipt.block_hash),
                )?;
                (
                    u64::from(deposit.bridge_log_index),
                    deposit.source_hash.clone(),
                    deposit.transaction_hash.clone(),
                    None,
                    "base_deposit_v1",
                    json!({"deposit":deposit,"source_block_number":receipt.block_number}),
                )
            } else {
                let withdrawal =
                    gofin_bridge::base_canonical::verify_withdrawal_source(&raw, &quote.intent)?;
                (
                    u64::from(withdrawal.bridge_log_index),
                    withdrawal.withdrawal_hash.clone(),
                    String::new(),
                    Some(withdrawal.withdrawal_hash.clone()),
                    "base_withdrawal_message",
                    json!({"withdrawal":withdrawal,"source_block_number":receipt.block_number}),
                )
            };
        let evidence_id = format!(
            "ethereum:{}:{:#x}:{log_index}",
            quote.intent.evm_source()?.chain_id,
            receipt.hash
        );
        let locator = gofin_domain::BridgeDestinationLocator {
            scheme: scheme.into(),
            source_evidence_id: evidence_id.clone(),
            destination_chain_id: quote.intent.destination_chain_id,
            destination_network: None,
            transaction_hash,
            message_hash,
            proof,
        };
        let effect = FinancialEffect::CashDebit {
            resource: quote.intent.wallet.clone(),
            amount: quote.gross_debit.clone(),
        };
        let transit = FinancialEffect::TransitPrincipal {
            transfer_id: transfer_id.into(),
            amount: quote.gross_debit.clone(),
        };
        return Ok((
            vec![effect, transit],
            Some(VerifiedSourceProof {
                evidence_id,
                log_index,
                correlation,
                destination_locator: Some(locator),
            }),
        ));
    }
    let order_id = quote.protocol_order_id.as_deref().ok_or_else(|| {
        error(
            "BRIDGE_ORDER_ID_MISSING",
            "reconciliation",
            "The original protocol order identity is missing.",
            "Keep the source transaction unresolved.",
        )
    })?;
    let source = quote.intent.evm_source()?;
    let logs = receipt.receipt["logs"].as_array().ok_or_else(|| {
        error(
            "RECEIPT_LOGS_INVALID",
            "reconciliation",
            "Source receipt has no logs.",
            "Keep the original transaction unresolved.",
        )
    })?;
    let expected = gofin_bridge::relay::proof::ExpectedSourceDeposit {
        depository: RELAY_DEPOSITORY.into(),
        sender: quote.intent.source_address.clone(),
        token: source.token.clone(),
        amount: quote.gross_debit.atoms.clone(),
        order_id: order_id.into(),
    };
    let native_topic =
        gofin_evm::alloy::primitives::keccak256("RelayNativeDeposit(address,uint256,bytes32)");
    let token_topic = gofin_evm::alloy::primitives::keccak256(
        "RelayErc20Deposit(address,address,uint256,bytes32)",
    );
    let mut verified = Vec::new();
    for log in logs {
        let Some(topic) = log["topics"]
            .as_array()
            .and_then(|topics| topics.first())
            .and_then(Value::as_str)
        else {
            continue;
        };
        let topic = gofin_evm::hash(topic)?;
        if (source.token.is_none() && topic == native_topic)
            || (source.token.is_some() && topic == token_topic)
        {
            let raw = raw_evm_log(log)?;
            if let Ok(event) =
                gofin_bridge::relay::proof::verify_source_deposit_log(&raw, &expected)
            {
                verified.push((event, gofin_evm::rpc_u64(&log["logIndex"])?));
            }
        }
    }
    if verified.len() != 1 {
        return Err(error(
            "BRIDGE_SOURCE_PROOF_UNVERIFIED",
            "reconciliation",
            "Canonical receipt does not contain exactly one Relay depository event for the original order.",
            "Keep the source step unresolved and reconcile the same transaction.",
        ));
    }
    let (event, log_index) = verified.remove(0);
    if let Some(token) = &source.token {
        let token = gofin_evm::address(token)?;
        let transfer_topic =
            gofin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)");
        let mut actual_debits = Vec::new();
        for log in logs {
            if gofin_evm::address(log["address"].as_str().unwrap_or(""))? != token {
                continue;
            }
            let topics = log["topics"].as_array().ok_or_else(|| {
                GoFinError::validation("Source token transfer topics are malformed.")
            })?;
            if topics.len() != 3
                || gofin_evm::hash(topics[0].as_str().unwrap_or(""))? != transfer_topic
            {
                continue;
            }
            let from = topic_address(topics[1].as_str().unwrap_or(""))?;
            let to = topic_address(topics[2].as_str().unwrap_or(""))?;
            if from == gofin_evm::address(&quote.intent.source_address)?
                && to == gofin_evm::address(RELAY_DEPOSITORY)?
            {
                let data = gofin_evm::rpc_bytes(&log["data"])?;
                actual_debits.push(gofin_evm::alloy::primitives::U256::from_be_slice(&data));
            }
        }
        if actual_debits.len() != 1
            || actual_debits[0]
                != gofin_evm::alloy::primitives::U256::from_str_radix(event.amount.as_str(), 10)
                    .map_err(|_| {
                        GoFinError::validation("Verified source amount is not a uint256.")
                    })?
        {
            return Err(error(
                "BRIDGE_SOURCE_TOKEN_DEBIT_UNVERIFIED",
                "reconciliation",
                "ERC-20 source receipt does not prove one exact wallet-to-depository token transfer.",
                "Keep the source step unresolved and reconcile the same transaction.",
            ));
        }
    }
    let resource: gofin_domain::ResourceId = serde_json::from_value(binding["resource"].clone())
        .map_err(|_| GoFinError::validation("Bridge source resource is malformed."))?;
    let amount = source.amount(event.amount);
    let effects = vec![
        FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: amount.clone(),
        },
        FinancialEffect::TransitPrincipal {
            transfer_id: transfer_id.into(),
            amount,
        },
    ];
    let transaction_hash = receipt.hash;
    let evidence_id = format!(
        "ethereum:{}:{}:{}",
        step.evm_call()?.chain_id,
        format!("{transaction_hash:#x}").to_ascii_lowercase(),
        log_index
    );
    Ok((
        effects,
        Some(VerifiedSourceProof {
            evidence_id,
            log_index,
            correlation: event.order_id,
            destination_locator: None,
        }),
    ))
}

async fn verify_canonical_source_deposit(
    step: &BridgeStep,
    binding: &Value,
    receipt: &CanonicalReceipt,
    transfer_id: &str,
    quote: &BridgeQuote,
    runtime: &gofin_evm::EvmRuntime,
) -> gofin_domain::Result<(Vec<FinancialEffect>, Option<VerifiedSourceProof>)> {
    let config = CanonicalConfig::reviewed();
    let deployment = config
        .deployments
        .get(&quote.intent.evm_destination_chain_id()?)
        .ok_or_else(|| {
            error(
                "BRIDGE_ROUTE_UNAVAILABLE",
                "reconciliation",
                "Saved canonical destination is not one of the reviewed child chains.",
                "Keep the source transaction unresolved and inspect its original quote.",
            )
        })?;
    let expected = gofin_bridge::canonical::ExpectedEthDeposit {
        source_address: quote.intent.source_address.clone(),
        recipient: quote.intent.recipient.clone(),
        amount: quote.gross_debit.atoms.clone(),
        destination_chain_id: quote.intent.evm_destination_chain_id()?,
        inbox: deployment.inbox.clone(),
        bridge: deployment.bridge.clone(),
    };
    if step.evm_call()?.chain_id != 1
        || !step.evm_call()?.to.eq_ignore_ascii_case(&deployment.inbox)
        || step.evm_call()?.value != quote.gross_debit.atoms
        || step.evm_call()?.data != "0x439370b1"
        || !step
            .expected_sender
            .eq_ignore_ascii_case(&quote.intent.source_address)
        || !receipt
            .transaction_sender
            .eq_ignore_ascii_case(&quote.intent.source_address)
        || runtime.chain_id != 1
    {
        return Err(error(
            "BRIDGE_SOURCE_ENVELOPE_MISMATCH",
            "reconciliation",
            "Canonical source receipt does not match the saved Ethereum Inbox deposit envelope.",
            "Keep the source step unresolved and inspect its original frozen transaction.",
        ));
    }
    require_empty_code(runtime, &quote.intent.source_address, None).await?;
    require_empty_code(
        runtime,
        &quote.intent.source_address,
        Some(receipt.block_number),
    )
    .await?;
    let logs = receipt.receipt["logs"].as_array().ok_or_else(|| {
        error(
            "RECEIPT_LOGS_INVALID",
            "reconciliation",
            "Canonical source receipt has no logs.",
            "Keep the source transaction unresolved and reconcile its original receipt.",
        )
    })?;
    let mut verified = Vec::new();
    for inbox_log in logs {
        let Ok(inbox_log) = canonical_raw_log(inbox_log) else {
            continue;
        };
        if !inbox_log.address.eq_ignore_ascii_case(&deployment.inbox) {
            continue;
        }
        for bridge_log in logs {
            let Ok(bridge_log) = canonical_raw_log(bridge_log) else {
                continue;
            };
            if !bridge_log.address.eq_ignore_ascii_case(&deployment.bridge) {
                continue;
            }
            let pair = gofin_bridge::canonical::CanonicalDepositReceipt {
                inbox_log: inbox_log.clone(),
                bridge_log,
            };
            if let Ok(proof) = gofin_bridge::canonical::verify_eth_deposit_source(&pair, &expected)
            {
                verified.push(proof);
            }
        }
    }
    if verified.len() != 1 {
        return Err(error(
            "BRIDGE_SOURCE_PROOF_UNVERIFIED",
            "reconciliation",
            "Canonical receipt does not contain one exact paired Inbox and Bridge event for the approved ETH deposit.",
            "Keep the source step unresolved and reconcile the same transaction.",
        ));
    }
    let proof = verified.remove(0);
    let resource: gofin_domain::ResourceId = serde_json::from_value(binding["resource"].clone())
        .map_err(|_| GoFinError::validation("Bridge source resource is malformed."))?;
    let amount = quote.intent.evm_source()?.amount(proof.amount.clone());
    let effects = vec![
        FinancialEffect::CashDebit {
            resource: resource.clone(),
            amount: amount.clone(),
        },
        FinancialEffect::TransitPrincipal {
            transfer_id: transfer_id.into(),
            amount,
        },
    ];
    let transaction_hash = format!("{:#x}", receipt.hash).to_ascii_lowercase();
    let log_index = u64::from(proof.bridge_log_index);
    let evidence_id = format!(
        "ethereum:{}:{transaction_hash}:{log_index}",
        step.evm_call()?.chain_id
    );
    let locator = gofin_domain::BridgeDestinationLocator {
        message_hash: None,
        scheme: "arbitrum_eth_deposit_v1".into(),
        source_evidence_id: evidence_id.clone(),
        destination_chain_id: Some(proof.destination_chain_id),
        destination_network: None,
        transaction_hash: proof.destination_transaction_hash.clone(),
        proof: serde_json::to_value(&proof).map_err(|_| internal_error())?,
    };
    Ok((
        effects,
        Some(VerifiedSourceProof {
            evidence_id,
            log_index,
            correlation: proof.destination_transaction_hash,
            destination_locator: Some(locator),
        }),
    ))
}

fn canonical_raw_log(
    log: &Value,
) -> gofin_domain::Result<gofin_bridge::canonical::CanonicalRawLog> {
    let topics = log["topics"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Canonical log topics are malformed."))?
        .iter()
        .map(|topic| {
            topic
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| GoFinError::validation("Canonical log topic is malformed."))
        })
        .collect::<gofin_domain::Result<Vec<_>>>()?;
    Ok(gofin_bridge::canonical::CanonicalRawLog {
        log_index: u32::try_from(gofin_evm::rpc_u64(&log["logIndex"])?).map_err(|_| {
            GoFinError::validation("Canonical log index exceeds its supported range.")
        })?,
        address: log["address"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("Canonical log emitter is malformed."))?
            .to_owned(),
        topics,
        data: log["data"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("Canonical log data is malformed."))?
            .to_owned(),
    })
}

fn saved_base_withdrawal(
    locator: &gofin_domain::BridgeDestinationLocator,
) -> gofin_domain::Result<gofin_bridge::base_canonical::VerifiedBaseWithdrawal> {
    let withdrawal: gofin_bridge::base_canonical::VerifiedBaseWithdrawal =
        serde_json::from_value(locator.proof["withdrawal"].clone())
            .map_err(|_| GoFinError::validation("Persisted Base withdrawal proof is malformed."))?;
    let actual = format!("{:#x}", withdrawal.withdrawal.hash()?);
    if locator.scheme != "base_withdrawal_message"
        || locator.destination_chain_id != Some(1)
        || !locator.transaction_hash.is_empty()
        || locator.source_evidence_id.is_empty()
        || locator.message_hash.as_deref() != Some(actual.as_str())
        || withdrawal.withdrawal_hash != actual
    {
        return Err(GoFinError::validation(
            "Persisted Base withdrawal message identity does not match its exact source proof.",
        ));
    }
    Ok(withdrawal)
}

async fn require_empty_code(
    runtime: &gofin_evm::EvmRuntime,
    address: &str,
    block: Option<u64>,
) -> gofin_domain::Result<()> {
    runtime.assert_chain().await?;
    let block_tag = block
        .map(|number| format!("{number:#x}"))
        .unwrap_or_else(|| "latest".into());
    let code = runtime
        .rpc
        .request(
            "eth_getCode",
            json!([
                gofin_evm::lower_address(gofin_evm::address(address)?),
                block_tag
            ]),
        )
        .await?;
    if !gofin_evm::rpc_bytes(&code)?.is_empty() {
        return Err(error(
            "BRIDGE_SOURCE_NOT_EOA",
            "reconciliation",
            "Canonical native-ETH deposits require the exact source recipient to have no contract code on Ethereum.",
            "Keep the transfer unresolved; this canonical route supports same-address EOAs only.",
        ));
    }
    Ok(())
}

async fn verify_allowance_step(
    adapter: &BridgeAdapter,
    step: &BridgeStep,
    binding: &Value,
    receipt: &CanonicalReceipt,
) -> gofin_domain::Result<Vec<FinancialEffect>> {
    if !receipt.success {
        return Err(GoFinError::validation(
            "Allowance transaction did not succeed.",
        ));
    }
    let quote: BridgeQuote = serde_json::from_value(binding["quote"].clone())
        .map_err(|_| GoFinError::validation("Stored bridge quote is malformed."))?;
    let token = quote
        .intent
        .evm_source()?
        .token
        .as_deref()
        .ok_or_else(|| GoFinError::validation("Native ETH cannot require ERC-20 allowance."))?;
    let approval = quote
        .approval
        .as_ref()
        .ok_or_else(|| GoFinError::validation("Bridge quote lacks an exact approval identity."))?;
    let owner = gofin_evm::address(&quote.intent.source_address)?;
    let spender = gofin_evm::address(&approval.spender)?;
    let runtime = adapter
        .wallet_adapters
        .get(&step.evm_call()?.chain_id)
        .ok_or_else(|| GoFinError::validation("Allowance observer is unavailable."))?
        .runtime
        .clone();
    let before = binding["allowance_before"]
        .as_str()
        .and_then(|value| gofin_evm::alloy::primitives::U256::from_str_radix(value, 10).ok())
        .ok_or_else(|| {
            error(
                "BRIDGE_ALLOWANCE_BINDING_INVALID",
                "reconciliation",
                "Original allowance state was not frozen in the preview.",
                "Keep the operation unresolved and inspect its immutable binding.",
            )
        })?;
    let after = binding["allowance_after"]
        .as_str()
        .and_then(|value| gofin_evm::alloy::primitives::U256::from_str_radix(value, 10).ok())
        .ok_or_else(|| {
            error(
                "BRIDGE_ALLOWANCE_BINDING_INVALID",
                "reconciliation",
                "Expected allowance value was not frozen in the preview.",
                "Keep the operation unresolved and inspect its immutable binding.",
            )
        })?;
    if receipt.block_number == 0 {
        return Err(error(
            "BRIDGE_ALLOWANCE_UNVERIFIED",
            "reconciliation",
            "Allowance receipt is at the first block and has no predecessor state.",
            "Keep the operation unresolved and reconcile its original transaction.",
        ));
    }
    let observed_before = runtime
        .call(
            gofin_evm::address(token)?,
            &BridgeErc20::allowanceCall { owner, spender },
            Some(receipt.block_number - 1),
        )
        .await?;
    let allowance = runtime
        .call(
            gofin_evm::address(token)?,
            &BridgeErc20::allowanceCall { owner, spender },
            Some(receipt.block_number),
        )
        .await?;
    let kind = serde_json::from_value::<BridgeStepKind>(binding["step_kind"].clone())
        .map_err(|_| GoFinError::validation("Bridge step kind is invalid."))?;
    let expected = match kind {
        BridgeStepKind::ResetAllowance => gofin_evm::alloy::primitives::U256::ZERO,
        BridgeStepKind::Approval => after,
        BridgeStepKind::Deposit
        | BridgeStepKind::Prove
        | BridgeStepKind::Finalize
        | BridgeStepKind::Mint => {
            return Err(GoFinError::validation(
                "This bridge stage is not an allowance step.",
            ));
        }
    };
    let expected_call = BridgeErc20::approveCall {
        spender,
        amount: expected,
    }
    .abi_encode();
    if gofin_evm::address(&step.evm_call()?.to)? != gofin_evm::address(token)?
        || gofin_evm::rpc_bytes(&json!(step.evm_call()?.data))? != expected_call
        || observed_before != before
        || allowance != after
        || after != expected
    {
        return Err(error(
            "BRIDGE_ALLOWANCE_UNVERIFIED",
            "reconciliation",
            "Canonical pre-step and post-step allowance values do not match the exact approved bridge transition.",
            "Keep the transfer unresolved and reconcile the original approval.",
        ));
    }
    let resource: gofin_domain::ResourceId = serde_json::from_value(binding["resource"].clone())
        .map_err(|_| GoFinError::validation("Bridge resource identity is malformed."))?;
    Ok(vec![FinancialEffect::AuthorityChange {
        resource,
        authority: format!(
            "erc20_allowance:{}:{}",
            token.to_ascii_lowercase(),
            approval.spender.to_ascii_lowercase()
        ),
        before: before.to_string(),
        after: after.to_string(),
    }])
}

fn store_error(_: impl std::fmt::Display) -> GoFinError {
    error(
        "BRIDGE_STORE_UNAVAILABLE",
        "storage",
        "Bridge state could not be persisted or loaded.",
        "Keep the operation unchanged and retry after inspecting the protected journal.",
    )
}

fn raw_evm_log(log: &Value) -> gofin_domain::Result<gofin_bridge::relay::proof::RawEvmLog> {
    let address = log["address"]
        .as_str()
        .ok_or_else(|| GoFinError::validation("Receipt log address is malformed."))?
        .to_owned();
    let topics = log["topics"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Receipt log topics are malformed."))?
        .iter()
        .map(|topic| {
            topic
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| GoFinError::validation("Receipt log topic is malformed."))
        })
        .collect::<gofin_domain::Result<Vec<_>>>()?;
    let data = log["data"]
        .as_str()
        .ok_or_else(|| GoFinError::validation("Receipt log data is malformed."))?
        .to_owned();
    Ok(gofin_bridge::relay::proof::RawEvmLog {
        address,
        topics,
        data,
    })
}

fn raw_token_transfer(log: &Value) -> Option<gofin_bridge::relay::proof::RawTokenTransfer> {
    let raw = raw_evm_log(log).ok()?;
    let topic = raw.topics.first()?;
    let signature = format!(
        "{:#x}",
        gofin_evm::alloy::primitives::keccak256("Transfer(address,address,uint256)")
    );
    if !topic.eq_ignore_ascii_case(&signature) || raw.topics.len() != 3 {
        return None;
    }
    Some(gofin_bridge::relay::proof::RawTokenTransfer {
        address: raw.address,
        topics: raw.topics,
        data: raw.data,
    })
}

fn topic_address(topic: &str) -> gofin_domain::Result<gofin_evm::alloy::primitives::Address> {
    let hash = gofin_evm::hash(topic)?;
    if hash.as_slice()[..12].iter().any(|byte| *byte != 0) {
        return Err(GoFinError::validation(
            "Receipt indexed address topic is malformed.",
        ));
    }
    Ok(gofin_evm::alloy::primitives::Address::from_slice(
        &hash.as_slice()[12..],
    ))
}

fn receipt_logs_canonical(
    receipt_value: &Value,
    receipt: &CanonicalReceipt,
    expected_hash: &str,
) -> gofin_domain::Result<bool> {
    let Some(logs) = receipt_value["logs"].as_array() else {
        return Ok(false);
    };
    let expected_hash = gofin_evm::hash(expected_hash)?;
    for log in logs {
        if log["removed"].as_bool() == Some(true) {
            return Ok(false);
        }
        if let Some(hash) = log["transactionHash"].as_str()
            && gofin_evm::hash(hash)? != expected_hash
        {
            return Ok(false);
        }
        if let Some(hash) = log["blockHash"].as_str()
            && gofin_evm::hash(hash)? != receipt.block_hash
        {
            return Ok(false);
        }
        if let Some(number) = log.get("blockNumber")
            && gofin_evm::rpc_u64(number)? != receipt.block_number
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    mod valuation_batch {
        include!("bridge/valuation_batch_tests.rs");
    }
    mod quote_preparation {
        include!("bridge/quote_preparation_tests.rs");
    }
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct ScopeOnlyProvider {
        provider: &'static str,
        quote_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl BridgeProvider for ScopeOnlyProvider {
        fn capabilities(&self) -> gofin_bridge::BridgeCapabilities {
            gofin_bridge::BridgeCapabilities {
                provider: self.provider.into(),
                chains: std::collections::BTreeSet::from([1, 42161, 4663]),
                native_eth: true,
                erc20: true,
                protocol_versions: std::collections::BTreeSet::new(),
            }
        }

        async fn quote(
            &self,
            _request: &BridgeQuoteRequest,
        ) -> gofin_domain::Result<gofin_bridge::BridgeQuoteBatch> {
            self.quote_calls.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Quote should not be requested during plan review.",
            ))
        }

        fn prepare(
            &self,
            _quote: &BridgeQuote,
            _bounds: &BridgeBounds,
            _wallet: &str,
        ) -> gofin_domain::Result<PreparedBridge> {
            Err(GoFinError::validation(
                "Provider plan should not be requested during plan review.",
            ))
        }

        async fn reconcile(
            &self,
            _transfer: &BridgeTransferRef,
        ) -> gofin_domain::Result<gofin_bridge::BridgeReconciliation> {
            Ok(gofin_bridge::BridgeReconciliation {
                status: "unused".into(),
                failure: None,
                refund_fail_reason: None,
                candidates: vec![],
            })
        }
    }

    struct UnexpectedRpc(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for UnexpectedRpc {
        async fn request(&self, _method: &str, _params: Value) -> gofin_domain::Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Bridge intent review must not use an RPC quote call.",
            ))
        }
    }

    struct FixedBridgePrices;

    #[async_trait::async_trait]
    impl gofin_pricing::UsdPriceSource for FixedBridgePrices {
        async fn quote(
            &self,
            _asset: &gofin_domain::AssetId,
        ) -> gofin_domain::Result<gofin_pricing::UsdQuote> {
            Ok(gofin_pricing::UsdQuote {
                usd_per_unit: "2000".into(),
                updated_at: gofin_domain::now(),
                observed_at: gofin_domain::now(),
                source: "synthetic bridge test price".into(),
            })
        }
    }

    #[tokio::test]
    async fn bridge_execute_review_uses_public_intent_schema_without_quote_or_rpc() {
        struct BatchBridgePrices(std::sync::Mutex<Vec<Vec<gofin_domain::AssetId>>>);
        #[async_trait::async_trait]
        impl gofin_pricing::UsdPriceSource for BatchBridgePrices {
            async fn quote(
                &self,
                _: &gofin_domain::AssetId,
            ) -> gofin_domain::Result<gofin_pricing::UsdQuote> {
                panic!("bridge fee and source-unit valuation must use one exact-asset batch")
            }
            async fn quotes(
                &self,
                assets: &[gofin_domain::AssetId],
            ) -> gofin_domain::Result<Vec<gofin_pricing::UsdQuote>> {
                self.0.lock().unwrap().push(assets.to_vec());
                let mut quotes = Vec::new();
                for asset in assets {
                    quotes.push(
                        gofin_pricing::UsdPriceSource::quote(&FixedBridgePrices, asset).await?,
                    );
                }
                Ok(quotes)
            }
        }
        let prices = Arc::new(BatchBridgePrices(Default::default()));
        let quote_calls = Arc::new(AtomicUsize::new(0));
        let rpc_calls = Arc::new(AtomicUsize::new(0));
        let relay: Arc<dyn BridgeProvider> = Arc::new(ScopeOnlyProvider {
            provider: "relay",
            quote_calls: quote_calls.clone(),
        });
        let canonical: Arc<dyn BridgeProvider> = Arc::new(ScopeOnlyProvider {
            provider: "arbitrum_canonical",
            quote_calls: quote_calls.clone(),
        });
        let rpc: Arc<dyn gofin_evm::EvmRpc> = Arc::new(UnexpectedRpc(rpc_calls.clone()));
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(rpc, 1, None));
        let wallet = Arc::new(
            gofin_ethereum::EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(1, 1, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "synthetic-wallet".into(),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    signing_enabled: false,
                }],
                runtime.clone(),
            )
            .unwrap(),
        );
        let adapter = BridgeAdapter::with_providers(
            vec![relay, canonical],
            BTreeMap::from([(1, wallet)]),
            BTreeMap::from([(1, runtime)]),
            Arc::new(SharedUsdValuator::with_source(prices.clone())),
        )
        .unwrap();
        let definition = BridgeAdapter::operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "bridge.execute")
            .unwrap();
        assert_eq!(definition.semantics["plan_only"], true);
        let plan_permission = &definition.semantics["plan_permission"];
        assert_eq!(plan_permission["supported"], true);
        assert_eq!(plan_permission["mode"], "agent_nominated");
        assert_eq!(plan_permission["requires_adapter_scope"], true);
        let status_definition = BridgeAdapter::operation_definitions()
            .into_iter()
            .find(|definition| definition.name == "bridge.status")
            .expect("bridge.status operation definition");
        let accounting_schema =
            &status_definition.output_schema["properties"]["settlement_accounting"];
        assert_eq!(
            accounting_schema["oneOf"].as_array().unwrap().len(),
            2,
            "the separately reconciled accounting result is nullable"
        );
        let variation_schema = jsonschema::validator_for(&plan_permission["bounds_schema"])
            .expect("bridge.execute variation schema compiles");
        assert!(variation_schema.is_valid(&json!({"mode":"agent_nominated"})));
        assert!(!variation_schema.is_valid(&json!({})));
        assert!(!variation_schema.is_valid(&json!({"mode":"exact"})));
        let schema = jsonschema::validator_for(&definition.input_schema)
            .expect("bridge.execute schema compiles");
        for example in &definition.examples {
            let _: gofin_bridge::BridgeExecuteInput =
                serde_json::from_value(example.clone()).expect("public example matches schema");
            assert!(
                schema.is_valid(example),
                "invalid public example: {example}"
            );
        }
        let input = json!({
            "intent": {
                "wallet": "synthetic-wallet",
                "source": {"chainId": 1},
                "destinationChainId": 4663,
                "destinationAssets": [{"chainId": 4663}],
                "recipient": "0x2222222222222222222222222222222222222222",
                "refundTo": "0x3333333333333333333333333333333333333333",
                "allowedRefundAssets": [{"chainId": 1}, {"chainId": 4663}],
                "amount": {"minimum": "10000000000000000", "maximum": "20000000000000000"}
            },
            "allowedProviders": ["relay", "arbitrum_canonical"],
            "bounds": {
                "input": {"minimum": "10000000000000000", "maximum": "20000000000000000"},
                "maximum_source_debit": "20000000000000000",
                "minimum_net_receipt": {
                    "asset": {"network": "ethereum:4663", "reference": "native", "decimals": 18},
                    "atoms": "1"
                },
                "fee_caps": [
                    {
                        "asset": {"network": "ethereum:1", "reference": "native", "decimals": 18},
                        "kind": "gas",
                        "maximum": "1000000000000000"
                    },
                    {
                        "asset": {"network": "ethereum:1", "reference": "native", "decimals": 18},
                        "kind": "withheld_from_output",
                        "maximum": "25000000000000"
                    },
                    {
                        "asset": {"network": "ethereum:4663", "reference": "native", "decimals": 18},
                        "kind": "withheld_from_output",
                        "maximum": "200000000000000"
                    }
                ]
            }
        });
        let intent_execution_error = adapter.prepare("bridge.execute", &input).await.unwrap_err();
        assert_eq!(
            intent_execution_error.code,
            "BRIDGE_EXECUTION_CHILD_REQUIRED"
        );
        assert_eq!(rpc_calls.load(Ordering::SeqCst), 0);
        assert_eq!(quote_calls.load(Ordering::SeqCst), 0);
        let permission = adapter
            .prepare_plan_permission("bridge.execute", &input, &json!({"mode":"agent_nominated"}))
            .await
            .expect("quote-independent intent review succeeds");
        assert_eq!(
            permission.spec.fixed["bridge_execute_scope"]["allowed_providers"],
            json!(["arbitrum_canonical", "relay"])
        );
        assert_eq!(
            permission.spec.parameters["bounds"]["maximum_source_debit"],
            "20000000000000000"
        );
        let costs = &permission.spec.maximum_costs;
        let source_asset = gofin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let destination_asset = gofin_domain::AssetId {
            network: "ethereum:4663".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let source_withheld = costs
            .iter()
            .find(|cost| {
                cost.amount.asset == source_asset
                    && cost.treatment == CostTreatment::IncludedInDebit
            })
            .expect("source-denominated Relay withheld fee has an included-cost cap");
        assert_eq!(
            source_withheld.location,
            AccountingLocation::Cash {
                resource: gofin_domain::ResourceId(
                    "ethereum:1:0x1111111111111111111111111111111111111111".into()
                )
            }
        );
        assert_eq!(
            source_withheld.amount.atoms,
            Quantity::new("25000000000000").unwrap()
        );
        let destination_withheld = costs
            .iter()
            .find(|cost| {
                cost.amount.asset == destination_asset
                    && cost.treatment == CostTreatment::IncludedInDebit
            })
            .expect("destination-denominated withheld fee remains supported");
        assert_eq!(
            destination_withheld.location,
            AccountingLocation::Cash {
                resource: gofin_domain::ResourceId(
                    "ethereum:4663:0x2222222222222222222222222222222222222222".into()
                )
            }
        );
        assert_eq!(
            destination_withheld.amount.atoms,
            Quantity::new("200000000000000").unwrap()
        );
        let source = EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        };
        let destination = EvmAsset {
            chain_id: 4663,
            token: None,
            decimals: 18,
        };
        let amount = Quantity::new("15000000000000000").unwrap();
        let quote = BridgeQuote {
            quote_id: "synthetic-relay-native".into(),
            provider: "relay".into(),
            route_id: "synthetic-relay-native-route".into(),
            intent: BridgeIntent {
                wallet: gofin_domain::ResourceId(
                    "ethereum:1:0x1111111111111111111111111111111111111111".into(),
                ),
                source_address: "0x1111111111111111111111111111111111111111".into(),
                source: Some(source.clone()),
                solana_source: None,
                destination_chain_id: Some(4663),
                solana_destination: None,
                destination_assets: vec![destination.clone()],
                recipient: "0x2222222222222222222222222222222222222222".into(),
                refund_to: "0x3333333333333333333333333333333333333333".into(),
                allowed_refund_assets: vec![source.clone(), destination.clone()],
                allowed_solana_refund_assets: vec![],
                amount: amount.clone(),
            },
            gross_debit: source.amount(amount.clone()),
            expected_receipt: destination.amount(Quantity::new("14000000000000000").unwrap()),
            minimum_receipt: destination.amount(Quantity::new("12000000000000000").unwrap()),
            fees: vec![
                gofin_bridge::BridgeFee {
                    amount: source.amount(Quantity::new("10000000000000").unwrap()),
                    kind: gofin_bridge::FeeKind::WithheldFromOutput,
                    payer: "relay".into(),
                },
                gofin_bridge::BridgeFee {
                    amount: destination.amount(Quantity::new("50000000000000").unwrap()),
                    kind: gofin_bridge::FeeKind::WithheldFromOutput,
                    payer: "relay".into(),
                },
            ],
            approval: None,
            protocol_steps: vec![],
            estimated_arrival_seconds: Some(120),
            observed_at: "2026-01-01T00:00:00Z".into(),
            valid_until: "2099-01-01T00:00:00Z".into(),
            class: gofin_bridge::QuoteClass::Executable,
            refund_paths: vec![
                gofin_bridge::RefundPath {
                    chain_id: 1,
                    network: None,
                    asset: source.asset_id(),
                    recipient: "0x3333333333333333333333333333333333333333".into(),
                    minimum: Quantity::new("1").unwrap(),
                    maximum: amount.clone(),
                },
                gofin_bridge::RefundPath {
                    chain_id: 4663,
                    network: None,
                    asset: destination.asset_id(),
                    recipient: "0x3333333333333333333333333333333333333333".into(),
                    minimum: Quantity::new("1").unwrap(),
                    maximum: amount.clone(),
                },
            ],
            order_solver: None,
            protocol_order_id: Some("0x01".into()),
            provider_request_id: Some("synthetic-relay-request".into()),
            cctp: None,
        };
        let candidate = PreparedOperation {
            operation: "bridge.execute".into(),
            input: json!({"transferId":"synthetic-relay-transfer","stepId":"synthetic-relay-deposit","stepKind":"deposit"}),
            binding: json!({"quote":quote.clone(),"bounds":permission.spec.parameters["bounds"]}),
            summary: "synthetic Relay native bridge child".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        validate_bridge_execute_candidate(&permission.spec, &candidate)
            .expect("native Relay candidate with source-denominated withheld fee fits intent");
        let approved_bounds: BridgeBounds =
            serde_json::from_value(permission.spec.parameters["bounds"].clone()).unwrap();
        adapter
            .validate_quote_prices(&approved_bounds, &quote)
            .await
            .expect("exact source and destination withheld fee caps cover the Relay candidate");
        assert_eq!(
            *prices.0.lock().unwrap(),
            [vec![source.asset_id(), destination.asset_id()]],
            "source unit shares its exact asset quote with the source-denominated fee"
        );
        let mut over_source_fee = quote.clone();
        over_source_fee.fees[0].amount.atoms = Quantity::new("25000000000001").unwrap();
        assert_eq!(
            adapter
                .validate_quote_prices(&approved_bounds, &over_source_fee)
                .await
                .expect_err("source-denominated withheld fee over its cap is rejected")
                .code,
            "BRIDGE_BOUNDS_VIOLATED"
        );
        let mut over_destination_fee = quote.clone();
        over_destination_fee.fees[1].amount.atoms = Quantity::new("200000000000001").unwrap();
        assert_eq!(
            adapter
                .validate_quote_prices(&approved_bounds, &over_destination_fee)
                .await
                .unwrap_err()
                .code,
            "BRIDGE_BOUNDS_VIOLATED"
        );
        assert_eq!(
            prices.0.lock().unwrap().len(),
            1,
            "all atomic bounds must be checked before any pricing request"
        );
        adapter
            .validate_usd_amount_bounds(&approved_bounds, &quote.gross_debit, &[])
            .await
            .unwrap();
        let mut destination_fee_only = quote.clone();
        destination_fee_only.fees.remove(0);
        adapter
            .validate_quote_prices(&approved_bounds, &destination_fee_only)
            .await
            .unwrap();
        assert_eq!(
            prices.0.lock().unwrap().last().unwrap(),
            &vec![destination.asset_id()],
            "no source lookup when no source USD bounds require one"
        );
        let calls = prices.0.lock().unwrap().len();
        destination_fee_only.fees.clear();
        adapter
            .validate_quote_prices(&approved_bounds, &destination_fee_only)
            .await
            .unwrap();
        assert_eq!(
            prices.0.lock().unwrap().len(),
            calls,
            "an unpriced, fee-free read quote needs no USD lookup"
        );
        let mut unsupported_precision = destination_fee_only.clone();
        unsupported_precision.gross_debit.asset.decimals = 37;
        assert_eq!(
            adapter
                .validate_usd_amount_bounds(
                    &approved_bounds,
                    &unsupported_precision.gross_debit,
                    &[],
                )
                .await
                .unwrap_err()
                .code,
            "PRICE_ASSET_UNSUPPORTED"
        );
        assert_eq!(
            adapter
                .validate_quote_prices(&approved_bounds, &unsupported_precision)
                .await
                .unwrap_err()
                .code,
            "PRICE_ASSET_UNSUPPORTED"
        );
        assert_eq!(prices.0.lock().unwrap().len(), calls);
        let mut usd_bounds = approved_bounds.clone();
        usd_bounds.maximum_total_usd_micros = Some(30_000_000);
        let source_fee_cap = usd_bounds
            .fee_caps
            .iter_mut()
            .find(|cap| {
                cap.asset == source.asset_id()
                    && cap.kind == gofin_bridge::FeeKind::WithheldFromOutput
            })
            .unwrap();
        source_fee_cap.maximum_usd_micros = Some(20_000);
        let fees = adapter
            .validate_quote_prices(&usd_bounds, &quote)
            .await
            .unwrap();
        assert_eq!(
            fees.iter().map(|fee| fee.usd_micros).collect::<Vec<_>>(),
            [20_000, 100_000]
        );
        usd_bounds.maximum_total_usd_micros = Some(29_999_999);
        assert_eq!(
            adapter
                .validate_quote_prices(&usd_bounds, &quote)
                .await
                .unwrap_err()
                .code,
            "BRIDGE_BOUNDS_VIOLATED"
        );
        usd_bounds.maximum_total_usd_micros = Some(30_000_000);
        usd_bounds
            .fee_caps
            .iter_mut()
            .find(|cap| {
                cap.asset == source.asset_id()
                    && cap.kind == gofin_bridge::FeeKind::WithheldFromOutput
            })
            .unwrap()
            .maximum_usd_micros = Some(19_999);
        assert_eq!(
            adapter
                .validate_quote_prices(&usd_bounds, &quote)
                .await
                .unwrap_err()
                .code,
            "BRIDGE_BOUNDS_VIOLATED"
        );
        let mut over_gross_debit = candidate.clone();
        over_gross_debit.binding["quote"]["gross_debit"]["atoms"] = json!("20000000000000001");
        assert!(validate_bridge_execute_candidate(&permission.spec, &over_gross_debit).is_err());
        let mut under_minimum_receipt = candidate.clone();
        under_minimum_receipt.binding["quote"]["minimum_receipt"]["atoms"] = json!("0");
        assert!(
            validate_bridge_execute_candidate(&permission.spec, &under_minimum_receipt).is_err()
        );
        assert_eq!(
            permission.spec.maximum_debits[0].amount.atoms,
            Quantity::new("20000000000000000").unwrap(),
            "withheld fee authority does not reduce principal or add a second debit"
        );
        assert!(permission.cumulative_scope.is_some());
        assert_eq!(quote_calls.load(Ordering::SeqCst), 0);
        assert_eq!(rpc_calls.load(Ordering::SeqCst), 0);

        for asset in [
            json!({"network":"ethereum:42161","reference":"native","decimals":18}),
            json!({"network":"ethereum:1","reference":ETHEREUM_USDC,"decimals":6}),
            json!({"network":"ethereum:4663","reference":"native","decimals":6}),
        ] {
            let mut unrelated_fee = input.clone();
            unrelated_fee["bounds"]["fee_caps"][1]["asset"] = asset;
            let error = adapter
                .prepare_plan_permission(
                    "bridge.execute",
                    &unrelated_fee,
                    &json!({"mode":"agent_nominated"}),
                )
                .await
                .expect_err(
                    "withheld fee cap outside exact source and destination assets must be rejected",
                );
            assert_eq!(error.code, "BRIDGE_FEE_ASSET_OUT_OF_SCOPE");
        }
    }

    struct DiagnosticObserverRpc {
        health: Arc<Mutex<Value>>,
        requested: AtomicBool,
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for DiagnosticObserverRpc {
        fn diagnostics(&self) -> Value {
            self.health.lock().unwrap().clone()
        }
        async fn request(&self, _: &str, _: Value) -> gofin_domain::Result<Value> {
            self.requested.store(true, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Doctor must not probe bridge observers.",
            ))
        }
    }

    #[test]
    fn bridge_diagnostics_read_existing_sidecar_transport_without_network_probes() {
        let health = Arc::new(Mutex::new(json!({
            "chain_id":42161,"read_timeout_ms":null,
            "endpoints":[{"endpoint_id":"rpc-1","identity_verified":true,"status":"healthy","retry_after_ms":0,"last_error_code":null,"unsupported_methods":[]}],
            "last_read":{"method":"eth_call","selected_endpoint":"rpc-1","attempts":[{"endpoint_id":"rpc-1","reason":"ok"}],"outcome":"ok"}
        })));
        let rpc = Arc::new(DiagnosticObserverRpc {
            health: health.clone(),
            requested: AtomicBool::new(false),
        });
        let mut adapter =
            BridgeAdapter::new(BTreeMap::new(), Arc::new(SharedUsdValuator::new().unwrap()))
                .unwrap();
        adapter.observers.insert(
            42161,
            Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 42161, None)),
        );
        assert!(
            adapter
                .diagnostics()
                .as_array()
                .unwrap()
                .contains(&health.lock().unwrap().clone())
        );
        health.lock().unwrap()["last_read"]["outcome"] = json!("cancelled");
        assert!(
            adapter
                .diagnostics()
                .as_array()
                .unwrap()
                .contains(&health.lock().unwrap().clone())
        );
        assert!(!rpc.requested.load(Ordering::SeqCst));
    }

    struct SequencedBlockRpc {
        finalized_requested: AtomicBool,
        tx_hash: String,
        block_hash: String,
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for SequencedBlockRpc {
        async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
            Ok(match method {
                "eth_chainId" => json!("0x1237"),
                "eth_getTransactionReceipt" => json!({
                    "transactionHash": self.tx_hash,
                    "blockHash": self.block_hash,
                    "blockNumber": "0x10",
                    "status": "0x1",
                    "gasUsed": "0x5208",
                    "effectiveGasPrice": "0x2"
                }),
                "eth_getTransactionByHash" => json!({
                    "hash": self.tx_hash,
                    "blockHash": self.block_hash,
                    "blockNumber": "0x10",
                    "from": "0x4444444444444444444444444444444444444444"
                }),
                "eth_getBlockByNumber" if params[0] == "finalized" => {
                    self.finalized_requested.store(true, Ordering::SeqCst);
                    Value::Null
                }
                "eth_getBlockByNumber" => json!({
                    "hash": self.block_hash,
                    "number": "0x10"
                }),
                "eth_blockNumber" => json!("0x10"),
                _ => return Err(GoFinError::validation("Unexpected synthetic RPC method.")),
            })
        }
    }

    #[tokio::test]
    async fn robinhood_sequenced_success_needs_no_finalized_head_for_bridge_settlement() {
        let rules = bridge_confirmation_rules(BTreeMap::new());
        let (confirmations, require_finalized) = rules[&4663];
        assert_eq!((confirmations, require_finalized), (1, false));

        let rpc = Arc::new(SequencedBlockRpc {
            finalized_requested: AtomicBool::new(false),
            tx_hash: format!("0x{}", "11".repeat(32)),
            block_hash: format!("0x{}", "22".repeat(32)),
        });
        let runtime = gofin_evm::EvmRuntime::new(rpc.clone(), 4663, None);
        let receipt = runtime
            .observe_receipt(&rpc.tx_hash, confirmations, require_finalized)
            .await
            .unwrap()
            .unwrap();

        assert!(receipt.success);
        assert_eq!(receipt.confirmations, 1);
        assert!(!rpc.finalized_requested.load(Ordering::SeqCst));
    }

    #[test]
    fn bridge_observers_use_sequencer_policy_and_preserve_mainnet_and_hub_finality() {
        let configured = bridge_confirmation_rules(BTreeMap::from([
            (1, (6, true)),
            (42161, (11, true)),
            (4663, (7, true)),
            (8453, (20, true)),
        ]));
        assert_eq!(configured[&1], (6, true));
        assert_eq!(configured[&42161], (1, false));
        assert_eq!(configured[&4663], (1, false));
        assert_eq!(configured[&8453], (1, false));
        assert_eq!(configured[&537713], (2, true));
    }

    #[test]
    fn settlement_policy_exposes_effective_rules_without_guessing_finality_time() {
        let configured =
            bridge_confirmation_rules(BTreeMap::from([(1, (6, false)), (42161, (4, false))]));
        let configured_policy = settlement_policy_from_rules(&configured, 1, 42161);
        assert_eq!(configured_policy["source"]["chain_id"], 1);
        assert_eq!(configured_policy["source"]["min_confirmations"], 6);
        assert_eq!(configured_policy["source"]["require_finalized"], false);
        assert_eq!(configured_policy["destination"]["chain_id"], 42161);
        assert_eq!(configured_policy["destination"]["min_confirmations"], 1);
        assert_eq!(configured_policy["destination"]["require_finalized"], false);
        assert!(
            configured_policy["arrival_estimate_scope"]
                .as_str()
                .unwrap()
                .contains("do not predict or replace")
        );

        let observer_policy = settlement_policy_from_rules(&configured, 1, 4663);
        assert_eq!(observer_policy["destination"]["min_confirmations"], 1);
        assert_eq!(observer_policy["destination"]["require_finalized"], false);

        let stricter = bridge_confirmation_rules(BTreeMap::from([(42161, (12, true))]));
        let stricter_policy = settlement_policy_from_rules(&stricter, 1, 42161);
        assert_eq!(stricter_policy["destination"]["min_confirmations"], 1);
        assert_eq!(stricter_policy["destination"]["require_finalized"], false);
    }

    #[test]
    fn canonical_child_progress_diagnostics_are_numeric_and_never_prove_settlement() {
        assert_eq!(
            canonical_progress_phase(false, 20, 2, Some(20), 10, true),
            "canonical_inclusion_wait"
        );
        assert_eq!(
            canonical_progress_phase(true, 1, 2, Some(20), 10, true),
            "confirmation_wait"
        );
        assert_eq!(
            canonical_progress_phase(true, 10, 2, Some(9), 10, true),
            "finality_wait"
        );
        assert_eq!(
            canonical_progress_phase(true, 10, 2, None, 10, true),
            "observation_unavailable"
        );

        let expected_hash = format!("0x{}", "12".repeat(32));
        assert!(canonical_receipt_matches_candidate(
            &json!({"transactionHash":expected_hash}),
            &expected_hash
        ));
        assert!(!canonical_receipt_matches_candidate(
            &json!({"transactionHash":format!("0x{}", "34".repeat(32))}),
            &expected_hash
        ));
        assert!(!canonical_receipt_matches_candidate(
            &json!({"transactionHash":"provider error: https://private.invalid"}),
            &expected_hash
        ));

        let observation = canonical_progress_observation(
            "finality_wait",
            Some(true),
            Some(true),
            Some(10),
            Some(20),
            Some(11),
            Some(9),
            2,
            true,
        );
        assert_eq!(observation["candidate_block_number"], 10);
        assert_eq!(observation["current_confirmations"], 11);
        assert!(observation["candidate_block_number"].is_number());
        assert!(observation.get("provider_error").is_none());
        assert!(observation.get("raw_receipt").is_none());

        let unavailable = canonical_progress_observation(
            "observation_unavailable",
            Some(true),
            Some(false),
            Some(10),
            None,
            None,
            None,
            2,
            true,
        );
        assert_eq!(unavailable["latest_block_number"], Value::Null);
        assert_eq!(unavailable["phase"], "observation_unavailable");
    }

    #[test]
    fn native_code_guard_diagnostics_are_bounded_numeric_observations() {
        let observation = json!({
            "direct_code_observation": {
                "chain_matches": true,
                "recipient_code": {
                    "address_valid": true,
                    "before": {"code_empty": false},
                    "inclusion": {"code_empty": false}
                }
            }
        });
        assert!(direct_native_code_guard_failed(&observation));
        let receipt = CanonicalReceipt {
            hash: gofin_evm::hash(&format!("0x{}", "12".repeat(32))).unwrap(),
            transaction_sender: format!("0x{}", "34".repeat(20)),
            block_hash: gofin_evm::hash(&format!("0x{}", "56".repeat(32))).unwrap(),
            block_number: 99,
            confirmations: 3,
            success: true,
            fee: gofin_evm::alloy::primitives::U256::from(0x5208u64),
            receipt: json!({
                "gasUsed":"0x5208",
                "gasUsedForL1":"0x120",
                "effectiveGasPrice":"0x3b9aca00",
                "logs":[]
            }),
        };
        let transaction = json!({"accessList":[{},{}],"input":"0xdeadbeef"});
        let diagnostic =
            attach_direct_native_execution_observation(observation, &receipt, Some(&transaction));
        assert_eq!(
            diagnostic["native_execution_observation"]["receipt_gas_used_atoms"],
            "21000"
        );
        assert_eq!(
            diagnostic["native_execution_observation"]["receipt_gas_used_for_l1_atoms"],
            "288"
        );
        assert_eq!(
            diagnostic["native_execution_observation"]["receipt_effective_gas_price_atoms"],
            "1000000000"
        );
        assert_eq!(
            diagnostic["native_execution_observation"]["transaction_access_list_entry_count"],
            2
        );
        let missing_optional = attach_direct_native_execution_observation(
            json!({}),
            &CanonicalReceipt {
                receipt: json!({}),
                ..receipt
            },
            None,
        );
        assert!(
            missing_optional["native_execution_observation"]["receipt_gas_used_for_l1_atoms"]
                .is_null()
        );
        assert!(
            missing_optional["native_execution_observation"]["transaction_access_list_entry_count"]
                .is_null()
        );
        assert!(diagnostic.get("input").is_none());
        assert!(diagnostic.get("transaction_hash").is_none());
        assert!(!direct_native_code_guard_failed(&json!({
            "direct_code_observation": {
                "chain_matches": true,
                "recipient_code": {
                    "address_valid": true,
                    "before": {"code_empty": true},
                    "inclusion": {"code_empty": true}
                }
            }
        })));
    }

    #[test]
    fn native_intrinsic_fallback_requires_exact_tag_and_receipt_gas() {
        let order_id = format!("0x{}", "11".repeat(32));
        let recipient = format!("0x{}", "22".repeat(20));
        let transaction_value = json!({"accessList": []});
        let transaction = gofin_bridge::relay::proof::DirectPaymentTransaction {
            transaction_hash: format!("0x{}", "33".repeat(32)),
            chain_id: 42161,
            transaction_type: 2,
            from: format!("0x{}", "44".repeat(20)),
            to: recipient.clone(),
            value: Quantity::new("1").unwrap(),
            input: order_id.clone(),
        };
        let expected = gofin_bridge::relay::proof::ExpectedDirectPayment {
            recipient,
            token: None,
            minimum_amount: Quantity::new("1").unwrap(),
            protocol_order_id: order_id,
        };
        // The 32 nonzero bytes in the order tag add 512 intrinsic gas.
        let receipt = json!({"gasUsed":"0x5408","gasUsedForL1":"0x0"});
        let verified = verify_native_intrinsic_rpc_proof(
            &transaction_value,
            &receipt,
            &transaction,
            &expected,
        )
        .expect("exact intrinsic-gas payout should verify");
        assert_eq!(verified.proof_method, "intrinsic_gas_retained_credit");
        assert_eq!(verified.amount, Quantity::new("1").unwrap());
        let gas_facts = direct_native_gas_facts(
            &receipt,
            Some(&json!({"input":transaction.input,"accessList":[]})),
        );
        assert_eq!(gas_facts["l2_execution_gas_atoms"], "21512");
        assert_eq!(gas_facts["required_intrinsic_gas_atoms"], 21512);
        assert_eq!(
            bridge_payment_proof_method(&verified.proof_method),
            Some("intrinsic_gas_retained_credit")
        );

        let too_little_gas = json!({"gasUsed":"0x5208","gasUsedForL1":"0x0"});
        assert!(
            verify_native_intrinsic_rpc_proof(
                &transaction_value,
                &too_little_gas,
                &transaction,
                &expected,
            )
            .is_none()
        );
        let no_poster_component = json!({"gasUsed":"0x5408"});
        assert!(
            verify_native_intrinsic_rpc_proof(
                &transaction_value,
                &no_poster_component,
                &transaction,
                &expected,
            )
            .is_none()
        );
        let undocumented_poster_alias = json!({"gasUsed":"0x5408","l1GasUsed":"0x0"});
        assert!(
            verify_native_intrinsic_rpc_proof(
                &transaction_value,
                &undocumented_poster_alias,
                &transaction,
                &expected,
            )
            .is_none()
        );
        let access_list = json!({"accessList":[{"address":expected.recipient,"storageKeys":[]}]});
        assert!(
            verify_native_intrinsic_rpc_proof(&access_list, &receipt, &transaction, &expected,)
                .is_none()
        );
    }

    #[test]
    fn pending_status_preserves_only_exact_order_chain_candidate_locators() {
        let hash = format!("0x{}", "12".repeat(32));
        let candidates = vec![
            BridgeEvidenceCandidate {
                order_id: Some("order-1".into()),
                kind: EvidenceKind::DestinationFill,
                chain_id: 42161,
                transaction_hash: hash.clone(),
                correlation: None,
            },
            BridgeEvidenceCandidate {
                order_id: Some("other-order".into()),
                kind: EvidenceKind::DestinationFill,
                chain_id: 42161,
                transaction_hash: format!("0x{}", "34".repeat(32)),
                correlation: None,
            },
            BridgeEvidenceCandidate {
                order_id: Some("order-1".into()),
                kind: EvidenceKind::DestinationFill,
                chain_id: 1,
                transaction_hash: format!("0x{}", "56".repeat(32)),
                correlation: None,
            },
            BridgeEvidenceCandidate {
                order_id: Some("order-1".into()),
                kind: EvidenceKind::Refund,
                chain_id: 1,
                transaction_hash: format!("0x{}", "78".repeat(32)),
                correlation: None,
            },
            BridgeEvidenceCandidate {
                order_id: Some("order-1".into()),
                kind: EvidenceKind::SourceDeposit,
                chain_id: 1,
                transaction_hash: format!("0x{}", "9a".repeat(32)),
                correlation: None,
            },
        ];
        let locators = bridge_candidate_locators(
            &candidates,
            "order-1",
            42161,
            &std::collections::BTreeSet::from([1]),
        );
        assert_eq!(locators.len(), 2);
        assert_eq!(locators[0]["kind"], "destination_fill");
        assert_eq!(locators[0]["chain_id"], 42161);
        assert_eq!(locators[0]["transaction_hash"], hash);
        assert_eq!(locators[0]["classification"], "candidate_locator_only");
        assert_eq!(locators[1]["kind"], "refund");
        assert_eq!(locators[1]["chain_id"], 1);
        assert!(
            locators
                .iter()
                .all(|value| value.get("correlation").is_none())
        );
    }

    #[test]
    fn external_candidate_observations_distinguish_finality_from_proof_mismatch_without_settlement()
    {
        let candidate = BridgeEvidenceCandidate {
            order_id: Some("synthetic-order".into()),
            kind: EvidenceKind::DestinationFill,
            chain_id: 42161,
            transaction_hash: format!("0x{}", "ab".repeat(32)),
            correlation: None,
        };
        let reconciliation = gofin_bridge::BridgeReconciliation {
            status: "success".into(),
            failure: Some(json!({"message":"PRIVATE_PROVIDER_ERROR"})),
            refund_fail_reason: None,
            candidates: vec![],
        };
        let locator = bridge_candidate_locators(
            std::slice::from_ref(&candidate),
            "synthetic-order",
            42161,
            &std::collections::BTreeSet::new(),
        );
        let pending = bridge_external_pending_status(
            &reconciliation,
            locator.clone(),
            vec![
                bridge_candidate_observation(&candidate, "pending_inclusion_or_finality").unwrap(),
            ],
            0,
        );
        let unmatched = bridge_external_pending_status(
            &reconciliation,
            locator,
            vec![bridge_candidate_observation(&candidate, "protocol_proof_not_matched").unwrap()],
            0,
        );

        assert_eq!(
            pending["candidate_observations"][0]["observation"],
            "pending_inclusion_or_finality"
        );
        assert_eq!(
            unmatched["candidate_observations"][0]["observation"],
            "protocol_proof_not_matched"
        );
        assert_eq!(pending["settled"], false);
        assert_eq!(unmatched["settled"], false);
        assert_eq!(unmatched["verified_candidates"], 0);
        assert!(
            !serde_json::to_string(&unmatched)
                .unwrap()
                .contains("PRIVATE_PROVIDER_ERROR")
        );
    }

    #[test]
    fn candidate_finality_diagnostics_are_numeric_and_never_settle() {
        let cases = [
            (
                None,
                Some(json!("0x100")),
                Some(json!({"number":"0xf0"})),
                "receipt_missing",
                Value::Null,
                json!(256),
                json!(240),
            ),
            (
                Some(json!({"blockNumber":"0x100"})),
                Some(json!("0x100")),
                Some(json!({"number":"0x100"})),
                "confirmations_pending",
                json!(256),
                json!(256),
                json!(256),
            ),
            (
                Some(json!({"blockNumber":"0x100"})),
                Some(json!("0x110")),
                None,
                "finalized_head_unavailable",
                json!(256),
                json!(272),
                Value::Null,
            ),
            (
                Some(json!({"blockNumber":"0x100"})),
                Some(json!("0x110")),
                Some(json!({"number":"0xff"})),
                "finality_pending",
                json!(256),
                json!(272),
                json!(255),
            ),
            (
                Some(json!({"blockNumber":"malformed"})),
                Some(json!("malformed")),
                Some(json!({"number":"malformed"})),
                "receipt_missing",
                Value::Null,
                Value::Null,
                Value::Null,
            ),
        ];

        for (
            receipt,
            latest,
            finalized,
            cause,
            expected_receipt,
            expected_latest,
            expected_finalized,
        ) in cases
        {
            let diagnostic = bridge_finality_observation(
                receipt.as_ref(),
                latest.as_ref(),
                finalized.as_ref(),
                2,
                true,
            );
            assert_eq!(diagnostic["cause"], cause);
            assert_eq!(diagnostic["receipt_block_number"], expected_receipt);
            assert_eq!(diagnostic["latest_block_number"], expected_latest);
            assert_eq!(diagnostic["finalized_block_number"], expected_finalized);
            assert!(diagnostic.get("settled").is_none());
        }

        let sufficient_heads = bridge_finality_observation(
            Some(&json!({"blockNumber":"0x100"})),
            Some(&json!("0x110")),
            Some(&json!({"number":"0x100"})),
            2,
            true,
        );
        assert_eq!(sufficient_heads["cause"], "inclusion_not_canonical");
        assert!(sufficient_heads.get("settled").is_none());

        let confirmations_only = bridge_finality_observation(
            Some(&json!({"blockNumber":"0x100"})),
            Some(&json!("0x100")),
            None,
            1,
            false,
        );
        assert_eq!(confirmations_only["cause"], "inclusion_not_canonical");
        assert_eq!(confirmations_only["require_finalized"], false);

        let candidate = BridgeEvidenceCandidate {
            order_id: Some("synthetic-order".into()),
            kind: EvidenceKind::DestinationFill,
            chain_id: 42161,
            transaction_hash: format!("0x{}", "ab".repeat(32)),
            correlation: None,
        };
        let attached = attach_bridge_finality_observation(
            bridge_candidate_observation(&candidate, "pending_inclusion_or_finality").unwrap(),
            bridge_finality_observation(
                Some(&json!({"blockNumber":"0x100"})),
                Some(&json!("0x100")),
                None,
                2,
                true,
            ),
        );
        let status = bridge_external_pending_status(
            &gofin_bridge::BridgeReconciliation {
                status: "success".into(),
                failure: None,
                refund_fail_reason: None,
                candidates: vec![],
            },
            vec![],
            vec![attached],
            0,
        );
        assert_eq!(status["settled"], false);
        assert_eq!(
            status["candidate_observations"][0]["cause"],
            "confirmations_pending"
        );
    }

    #[test]
    fn router_proof_diagnostics_are_bounded_and_never_include_raw_errors_or_calldata() {
        let receipt = CanonicalReceipt {
            hash: gofin_evm::hash(&format!("0x{}", "11".repeat(32))).unwrap(),
            transaction_sender: "0x0000000000000000000000000000000000000001".into(),
            block_hash: gofin_evm::hash(&format!("0x{}", "22".repeat(32))).unwrap(),
            block_number: 100,
            confirmations: 2,
            success: true,
            fee: gofin_evm::alloy::primitives::U256::ZERO,
            receipt: json!({}),
        };
        let logs = (0..18)
            .map(|_| {
                json!({
                    "address":"0x0000000000000000000000000000000000000002",
                    "topics":[format!("0x{}", "33".repeat(32))],
                    "data":"0xprivate-calldata-must-not-be-included"
                })
            })
            .collect::<Vec<_>>();
        let no_router_event = attach_router_proof_diagnostics(
            json!({"observation":"protocol_proof_not_matched"}),
            &receipt,
            false,
            None,
            None,
            &logs,
            json!({"transaction_found":false}),
        );
        assert_eq!(
            no_router_event["observed_transaction_sender"],
            receipt.transaction_sender
        );
        assert_eq!(no_router_event["reviewed_router_event_found"], false);
        assert_eq!(
            no_router_event["receipt_logs"].as_array().unwrap().len(),
            16
        );
        assert_eq!(no_router_event["receipt_log_count"], 18);
        assert_eq!(no_router_event["receipt_logs_truncated"], true);
        assert_eq!(
            no_router_event["receipt_logs"][0]["topic0"],
            format!("0x{}", "33".repeat(32))
        );

        let error = gofin_domain::GoFinError::new(
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "untrusted provider text with calldata 0xdeadbeef",
            "keep pending",
        );
        let event_rejected = attach_router_proof_diagnostics(
            json!({"observation":"protocol_proof_not_matched"}),
            &receipt,
            true,
            Some(1),
            Some(sanitize_router_proof_rejection(error)),
            &logs,
            json!({"transaction_found":true}),
        );
        assert_eq!(event_rejected["reviewed_router_event_found"], true);
        assert_eq!(event_rejected["router_event_topics_count"], 1);
        assert_eq!(
            event_rejected["proof_rejection"]["code"],
            "BRIDGE_PROOF_MISMATCH"
        );
        assert!(event_rejected.get("receipt_logs").is_none());
        let serialized = serde_json::to_string(&event_rejected).unwrap();
        assert!(!serialized.contains("untrusted provider text"));
        assert!(!serialized.contains("deadbeef"));
        assert!(!serialized.contains("private-calldata"));
        assert_eq!(event_rejected.get("settled"), None);
    }

    #[test]
    fn transaction_shape_diagnostics_use_exact_hash_and_never_emit_input() {
        let candidate = BridgeEvidenceCandidate {
            order_id: Some("order".into()),
            kind: EvidenceKind::DestinationFill,
            chain_id: 42161,
            transaction_hash: format!("0x{}", "ab".repeat(32)),
            correlation: None,
        };
        let order_id = format!("0x{}", "11".repeat(32));
        let request_id = format!("0x{}", "22".repeat(32));
        let transaction = json!({
            "hash":candidate.transaction_hash.clone(),
            "chainId":"0xa4b1",
            "from":"0x0000000000000000000000000000000000000001",
            "to":"0x0000000000000000000000000000000000000002",
            "value":"0x20",
            "type":"0x2",
            "input":order_id.clone()
        });
        let observation = bridge_transaction_shape_observation(
            Some(&transaction),
            &candidate,
            &order_id,
            &request_id,
        );
        assert_eq!(observation["transaction_found"], true);
        assert_eq!(observation["candidate_hash_matches"], true);
        assert_eq!(observation["chain_id"], 42161);
        assert_eq!(observation["reported_chain_id"], 42161);
        assert_eq!(
            observation["from"],
            "0x0000000000000000000000000000000000000001"
        );
        assert_eq!(
            observation["to"],
            "0x0000000000000000000000000000000000000002"
        );
        assert_eq!(observation["value_atoms"], "32");
        assert_eq!(observation["type"], 2);
        assert_eq!(observation["input_byte_length"], 32);
        assert_eq!(observation["input_matches_order_id"], true);
        assert_eq!(observation["input_matches_provider_request_id"], false);
        let serialized = serde_json::to_string(&observation).unwrap();
        assert!(!serialized.contains(&"11".repeat(32)));

        let wrong_hash = json!({
            "hash":format!("0x{}", "cd".repeat(32)),
            "input":"0xdeadbeef"
        });
        let absent = bridge_transaction_shape_observation(
            Some(&wrong_hash),
            &candidate,
            &order_id,
            &request_id,
        );
        assert_eq!(absent["transaction_found"], false);
        assert_eq!(absent["input_byte_length"], Value::Null);
        assert!(!serde_json::to_string(&absent).unwrap().contains("deadbeef"));
    }

    #[test]
    fn direct_payment_transaction_must_match_canonical_receipt_identity() {
        let candidate = BridgeEvidenceCandidate {
            order_id: Some("order".into()),
            kind: EvidenceKind::DestinationFill,
            chain_id: 42161,
            transaction_hash: format!("0x{}", "ab".repeat(32)),
            correlation: None,
        };
        let receipt = CanonicalReceipt {
            hash: gofin_evm::hash(&candidate.transaction_hash).unwrap(),
            transaction_sender: "0xca7ded7e4f4ba8ab3b10009236ae6d1b95094589".into(),
            block_hash: gofin_evm::hash(&format!("0x{}", "44".repeat(32))).unwrap(),
            block_number: 100,
            confirmations: 2,
            success: true,
            fee: gofin_evm::alloy::primitives::U256::ZERO,
            receipt: json!({}),
        };
        let transaction = json!({
            "hash":candidate.transaction_hash.clone(),
            "chainId":"0xa4b1",
            "type":"0x2",
            "from":receipt.transaction_sender.clone(),
            "to":"0x0000000000000000000000000000000000000002",
            "value":"0x1",
            "input":"0x",
            "blockNumber":"0x64",
            "blockHash":format!("{:#x}",receipt.block_hash)
        });
        assert!(direct_payment_transaction(&transaction, &candidate, &receipt).is_some());

        for (key, value) in [
            ("hash", json!(format!("0x{}", "cd".repeat(32)))),
            ("from", json!("0x0000000000000000000000000000000000000003")),
            ("chainId", json!("0x1")),
            ("blockNumber", json!("0x65")),
            ("blockHash", json!(format!("0x{}", "55".repeat(32)))),
        ] {
            let mut changed = transaction.clone();
            changed[key] = value;
            assert!(
                direct_payment_transaction(&changed, &candidate, &receipt).is_none(),
                "changed {key} must not match the canonical receipt"
            );
        }
    }

    #[test]
    fn direct_code_status_is_strict_and_rpc_diagnostics_are_sanitized() {
        let empty = json!({
            "address_valid":true,
            "expected_code_empty":true,
            "before":{"block_number":99,"code_empty":true,"error":null},
            "inclusion":{"block_number":100,"code_empty":true,"error":null}
        });
        assert!(direct_code_observation_matches(&empty, true));
        assert!(!direct_code_observation_matches(&empty, false));

        let unavailable = json!({
            "address_valid":true,
            "expected_code_empty":true,
            "before":{"block_number":99,"code_empty":null,"error":{"code":"RPC_HISTORICAL_STATE_UNAVAILABLE","json_rpc_code":-32001}},
            "inclusion":{"block_number":100,"code_empty":true,"error":null}
        });
        assert!(!direct_code_observation_matches(&unavailable, true));

        let error = gofin_domain::GoFinError::new(
            "RPC_UNAVAILABLE",
            "rpc",
            "private endpoint response should not be exposed",
            "retry",
        )
        .with_details(json!({
            "http_status":503,
            "json_rpc_code":-32001,
            "url":"https://private.example/secret",
            "bytecode":"0xdeadbeef",
            "message":"missing trie node 0xsecret"
        }));
        let sanitized = sanitized_rpc_error(&error);
        assert_eq!(sanitized["code"], "RPC_UNAVAILABLE");
        assert_eq!(sanitized["http_status"], 503);
        assert_eq!(sanitized["json_rpc_code"], -32001);
        let serialized = serde_json::to_string(&sanitized).unwrap();
        assert!(!serialized.contains("private.example"));
        assert!(!serialized.contains("deadbeef"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn source_finality_observation_must_match_exact_receipt_inclusion_and_fee() {
        let exact = CanonicalReceipt {
            hash: gofin_evm::hash(&format!("0x{}", "11".repeat(32))).unwrap(),
            transaction_sender: "0x0000000000000000000000000000000000000001".into(),
            block_hash: gofin_evm::hash(&format!("0x{}", "22".repeat(32))).unwrap(),
            block_number: 100,
            confirmations: 2,
            success: true,
            fee: gofin_evm::alloy::primitives::U256::from(10),
            receipt: json!({}),
        };
        assert!(bridge_receipts_match(&exact, &exact));

        let mut changed = exact.clone();
        changed.block_hash = gofin_evm::hash(&format!("0x{}", "33".repeat(32))).unwrap();
        assert!(!bridge_receipts_match(&exact, &changed));

        let mut changed = exact.clone();
        changed.block_number += 1;
        assert!(!bridge_receipts_match(&exact, &changed));

        let mut changed = exact.clone();
        changed.fee += gofin_evm::alloy::primitives::U256::from(1);
        assert!(!bridge_receipts_match(&exact, &changed));
    }

    #[tokio::test]
    async fn bridge_usd_limits_cover_cumulative_gas_envelope_and_frozen_transaction() {
        let adapter = BridgeAdapter::new(
            BTreeMap::new(),
            Arc::new(crate::pricing::tests::fixed_valuator("2000")),
        )
        .unwrap();
        let source = EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        };
        let source_asset = source.asset_id();
        let gas_asset = gofin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let wallet = gofin_domain::ResourceId(
            "ethereum:1:0x1111111111111111111111111111111111111111".into(),
        );
        let mut bounds = BridgeBounds {
            maximum_source_debit: Some(gofin_domain::Quantity::new("100000000000000000").unwrap()),
            fee_caps: vec![gofin_bridge::FeeCap {
                asset: gas_asset.clone(),
                kind: gofin_bridge::FeeKind::Gas,
                maximum: gofin_domain::Quantity::new("10000000000000000").unwrap(),
                maximum_usd_micros: Some(20_000_000),
            }],
            maximum_total_usd_micros: Some(219_999_999),
            ..BridgeBounds::default()
        };

        // 0.1 ETH principal at $2,000 plus one cumulative 0.01 ETH gas
        // envelope is $220. The plan may have several sequential steps, but
        // the user's gas allowance is counted once across all of them.
        assert_eq!(
            adapter
                .validate_selected_usd_caps(&bounds, &source, &wallet)
                .await
                .unwrap_err()
                .code,
            "BRIDGE_TOTAL_USD_CAP_EXCEEDED"
        );
        bounds.maximum_total_usd_micros = Some(220_000_000);
        adapter
            .validate_selected_usd_caps(&bounds, &source, &wallet)
            .await
            .unwrap();

        assert_eq!(
            adapter
                .validate_frozen_gas_cap(
                    &bounds,
                    &gas_asset,
                    &gofin_domain::Quantity::new("10000000000000001").unwrap(),
                )
                .await
                .unwrap_err()
                .code,
            "BRIDGE_GAS_CAP_EXCEEDED"
        );
        assert_eq!(source_asset, gas_asset);
    }

    #[test]
    fn bridge_summary_keeps_alias_network_minimum_and_fee_cap() {
        let asset = GoFinAmount {
            asset: gofin_domain::AssetId {
                network: "ethereum:42161".into(),
                reference: ARBITRUM_USDC.into(),
                decimals: 6,
            },
            atoms: gofin_domain::Quantity::new("9950000").unwrap(),
        };
        let saved = SavedQuote {
            quote: BridgeQuote {
                cctp: None,
                quote_id: "synthetic-quote".into(),
                provider: "relay".into(),
                route_id: "synthetic-route".into(),
                intent: BridgeIntent {
                    wallet: gofin_domain::ResourceId("evm:synthetic".into()),
                    source_address: "0x1111111111111111111111111111111111111111".into(),
                    source: Some(EvmAsset {
                        chain_id: 1,
                        token: Some(ETHEREUM_USDC.into()),
                        decimals: 6,
                    }),
                    solana_source: None,
                    solana_destination: None,
                    destination_chain_id: Some(42161),
                    destination_assets: vec![],
                    recipient: "0x1111111111111111111111111111111111111111".into(),
                    refund_to: "0x1111111111111111111111111111111111111111".into(),
                    allowed_refund_assets: vec![],
                    allowed_solana_refund_assets: vec![],
                    amount: gofin_domain::Quantity::new("10000000").unwrap(),
                },
                gross_debit: GoFinAmount {
                    asset: gofin_domain::AssetId {
                        network: "ethereum:1".into(),
                        reference: ETHEREUM_USDC.into(),
                        decimals: 6,
                    },
                    atoms: gofin_domain::Quantity::new("10000000").unwrap(),
                },
                expected_receipt: asset.clone(),
                minimum_receipt: asset,
                fees: vec![],
                approval: None,
                protocol_steps: vec![],
                estimated_arrival_seconds: None,
                observed_at: now(),
                valid_until: now(),
                order_solver: Some(gofin_bridge::relay::RELAY_FILL_AUTHORITY.into()),
                class: gofin_bridge::QuoteClass::Executable,
                refund_paths: vec![],
                protocol_order_id: Some("0x01".into()),
                provider_request_id: Some("synthetic-request".into()),
            },
            bounds: BridgeBounds {
                fee_caps: {
                    let gas_asset = gofin_domain::AssetId {
                        network: "ethereum:1".into(),
                        reference: "native".into(),
                        decimals: 18,
                    };
                    vec![
                        gofin_bridge::FeeCap {
                            asset: gas_asset.clone(),
                            kind: gofin_bridge::FeeKind::Gas,
                            maximum: gofin_domain::Quantity::new("100000000000000").unwrap(),
                            maximum_usd_micros: Some(1000),
                        },
                        gofin_bridge::FeeCap {
                            asset: gas_asset.clone(),
                            kind: gofin_bridge::FeeKind::AdditionalDebit,
                            maximum: gofin_domain::Quantity::new("10000000000000").unwrap(),
                            maximum_usd_micros: None,
                        },
                        gofin_bridge::FeeCap {
                            asset: gas_asset,
                            kind: gofin_bridge::FeeKind::WithheldFromOutput,
                            maximum: gofin_domain::Quantity::new("30000000000000").unwrap(),
                            maximum_usd_micros: None,
                        },
                    ]
                },
                ..BridgeBounds::default()
            },
            plan: PreparedBridge {
                quote: BridgeQuote {
                    cctp: None,
                    quote_id: String::new(),
                    provider: String::new(),
                    route_id: String::new(),
                    intent: BridgeIntent {
                        wallet: gofin_domain::ResourceId(String::new()),
                        source_address: String::new(),
                        source: Some(EvmAsset {
                            chain_id: 1,
                            token: None,
                            decimals: 18,
                        }),
                        solana_source: None,
                        solana_destination: None,
                        destination_chain_id: Some(42161),
                        destination_assets: vec![],
                        recipient: String::new(),
                        refund_to: String::new(),
                        allowed_refund_assets: vec![],
                        allowed_solana_refund_assets: vec![],
                        amount: gofin_domain::Quantity::new("1").unwrap(),
                    },
                    gross_debit: GoFinAmount {
                        asset: gofin_domain::AssetId {
                            network: String::new(),
                            reference: String::new(),
                            decimals: 0,
                        },
                        atoms: gofin_domain::Quantity::new("1").unwrap(),
                    },
                    expected_receipt: GoFinAmount {
                        asset: gofin_domain::AssetId {
                            network: String::new(),
                            reference: String::new(),
                            decimals: 0,
                        },
                        atoms: gofin_domain::Quantity::new("1").unwrap(),
                    },
                    minimum_receipt: GoFinAmount {
                        asset: gofin_domain::AssetId {
                            network: String::new(),
                            reference: String::new(),
                            decimals: 0,
                        },
                        atoms: gofin_domain::Quantity::new("1").unwrap(),
                    },
                    fees: vec![],
                    approval: None,
                    protocol_steps: vec![],
                    estimated_arrival_seconds: None,
                    observed_at: now(),
                    valid_until: now(),
                    order_solver: Some(gofin_bridge::relay::RELAY_FILL_AUTHORITY.into()),
                    class: gofin_bridge::QuoteClass::Executable,
                    refund_paths: vec![],
                    protocol_order_id: None,
                    provider_request_id: None,
                },
                bounds: BridgeBounds::default(),
                steps: vec![],
                source_order_id: None,
                fixed_scope_digest: String::new(),
            },
            step_ids: BTreeMap::new(),
        };
        let adapter =
            BridgeAdapter::new(BTreeMap::new(), Arc::new(SharedUsdValuator::new().unwrap()))
                .unwrap();
        let summary = adapter.summary(&saved, &BridgeStepKind::Deposit, "dev-wallet");
        assert!(summary.contains("dev-wallet"));
        assert!(summary.contains("Ethereum Mainnet → Arbitrum One via Relay"));
        assert!(summary.contains("Receive ≥9.95 USDC"));
        assert!(summary.contains("Max fees"));
        assert!(summary.contains("0.0001 ETH gas"));
        assert!(summary.contains("0.00001 ETH additional fees"));
        assert!(summary.contains("0.00003 ETH withheld from output"));
        assert!(summary.contains("Total fees up to 0.00014 ETH"));
        assert!(summary.contains("recipient 0x1111…1111"));

        let mut solana_source_review = saved.clone();
        solana_source_review.quote.intent.source = None;
        solana_source_review.quote.intent.destination_chain_id = Some(1);
        solana_source_review.quote.gross_debit.asset = gofin_domain::AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: gofin_solana::funding::MAINNET_USDC_MINT.into(),
            decimals: 6,
        };
        solana_source_review.quote.gross_debit.atoms =
            gofin_domain::Quantity::new("5000000").unwrap();
        let solana_summary = adapter.summary(
            &solana_source_review,
            &BridgeStepKind::Deposit,
            "solana-dev",
        );
        assert!(solana_summary.starts_with(
            "Bridge 5 USDC; solana-dev Solana mainnet-beta → Ethereum Mainnet via Relay;"
        ));
        let mut other_mint = solana_source_review.quote.gross_debit.asset.clone();
        other_mint.reference = "11111111111111111111111111111111".into();
        assert_ne!(short_asset_name(&other_mint), "USDC");
        let mut wrong_decimals = solana_source_review.quote.gross_debit.asset.clone();
        wrong_decimals.decimals = 9;
        assert_ne!(short_asset_name(&wrong_decimals), "USDC");
        let mut wrong_network = solana_source_review.quote.gross_debit.asset.clone();
        wrong_network.network = "ethereum:1".into();
        assert_ne!(short_asset_name(&wrong_network), "USDC");

        let mut solana_review = saved.clone();
        solana_review.quote.provider = "cctp".into();
        solana_review.quote.intent.destination_chain_id = None;
        solana_review.bounds.fee_caps = vec![
            gofin_bridge::FeeCap {
                asset: gofin_domain::AssetId {
                    network: "ethereum:42161".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
                kind: gofin_bridge::FeeKind::Gas,
                maximum: gofin_domain::Quantity::new("50000000000000").unwrap(),
                maximum_usd_micros: None,
            },
            gofin_bridge::FeeCap {
                asset: gofin_domain::AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                kind: gofin_bridge::FeeKind::Gas,
                maximum: gofin_domain::Quantity::new("100000").unwrap(),
                maximum_usd_micros: None,
            },
            gofin_bridge::FeeCap {
                asset: gofin_domain::AssetId {
                    network: gofin_solana::NETWORK.into(),
                    reference: "native".into(),
                    decimals: 9,
                },
                kind: gofin_bridge::FeeKind::AdditionalDebit,
                maximum: gofin_domain::Quantity::new("1200000").unwrap(),
                maximum_usd_micros: None,
            },
        ];
        let solana_summary = adapter.summary(&solana_review, &BridgeStepKind::Mint, "dev-wallet");
        assert!(solana_summary.contains("Solana mainnet-beta via Circle CCTP V2"));
        assert!(
            solana_summary.contains("0.00005 ETH gas, 0.0001 SOL gas, 0.0012 SOL additional fees")
        );
        assert!(solana_summary.contains("Total fees up to 0.00005 ETH and 0.0013 SOL"));

        let mut canonical_review = saved.clone();
        canonical_review.quote.provider = "arbitrum_canonical".into();
        let canonical_summary =
            adapter.summary(&canonical_review, &BridgeStepKind::Deposit, "dev-wallet");
        assert!(canonical_summary.contains("Ethereum Mainnet → Arbitrum One via Canonical Inbox"));
        assert!(!canonical_summary.contains("via Relay"));

        let mut allowance_review = saved.clone();
        allowance_review.bounds.maximum_source_debit =
            Some(gofin_domain::Quantity::new("12000000").unwrap());
        allowance_review.quote.approval = Some(gofin_bridge::ApprovalRequirement {
            token: ETHEREUM_USDC.into(),
            spender: "0xccc88a9d1b4ed6b0eaba998850414b24f1c315be".into(),
            amount: gofin_domain::Quantity::new("10000000").unwrap(),
        });
        let approval = adapter.summary(&allowance_review, &BridgeStepKind::Approval, "dev-wallet");
        assert!(approval.contains("Approve up to 12 USDC"));
        assert!(approval.contains("Relay spender 0xccc8…15be"));
        assert!(approval.contains("bridge 10 USDC"));
        assert!(!approval.contains("synthetic-route"));

        let reset = adapter.summary(
            &allowance_review,
            &BridgeStepKind::ResetAllowance,
            "dev-wallet",
        );
        assert!(reset.contains("Reset token allowance to 0 USDC"));
        assert!(reset.contains("Relay spender 0xccc8…15be"));
        assert!(reset.contains("bridge 10 USDC"));
        allowance_review.bounds.input = Some(gofin_bridge::AmountInterval {
            minimum: gofin_domain::Quantity::new("10000000").unwrap(),
            maximum: gofin_domain::Quantity::new("10000000").unwrap(),
        });
        let step = |ordinal, kind| BridgeStep {
            ordinal,
            kind,
            solana_mint: None,
            solana_transaction: None,
            call: Some(EvmCall {
                chain_id: 1,
                to: ETHEREUM_USDC.into(),
                value: gofin_domain::Quantity::zero(),
                data: "0x".into(),
            }),
            expected_sender: allowance_review.quote.intent.source_address.clone(),
            maximum_debit: vec![],
            dependencies: vec![],
        };
        let mut reset_start_plan = allowance_review.plan.clone();
        reset_start_plan.quote = allowance_review.quote.clone();
        reset_start_plan.steps = vec![
            step(0, ProviderStepKind::ResetAllowance),
            step(1, ProviderStepKind::Approval),
            step(2, ProviderStepKind::Deposit),
        ];
        let grant_summary = finite_bridge_boundary_summary(
            &reset,
            &reset_start_plan,
            &allowance_review.quote.intent,
            &allowance_review.bounds,
        )
        .unwrap();
        assert!(grant_summary.contains("Finite grant covers these declared steps"));
        assert!(grant_summary.contains("reset token allowance to 0 USDC"));
        assert!(grant_summary.contains("approve up to 12 USDC for Relay spender 0xccc8…15be"));
        assert!(grant_summary.contains("deposit source input 10 USDC"));
        assert!(grant_summary.contains("Cumulative max fees"));

        let mut explicit_usd_bounds = allowance_review.bounds.clone();
        explicit_usd_bounds.input_usd_micros = Some(gofin_bridge::AmountInterval {
            minimum: gofin_domain::Quantity::new("9990000").unwrap(),
            maximum: gofin_domain::Quantity::new("10000000").unwrap(),
        });
        explicit_usd_bounds.fee_caps[0].maximum =
            gofin_domain::Quantity::new("500000000000000").unwrap();
        explicit_usd_bounds.fee_caps[0].maximum_usd_micros = Some(2_000_000);
        explicit_usd_bounds.maximum_total_usd_micros = Some(4_000_000);
        let grant_summary = finite_bridge_boundary_summary(
            "Reset allowance; Max fees 0.0005 ETH gas. USD ceilings: $1.01 movement, $0.11 fees.",
            &reset_start_plan,
            &allowance_review.quote.intent,
            &explicit_usd_bounds,
        )
        .unwrap();
        assert!(grant_summary.contains("gas fee ≤$2"));
        assert!(grant_summary.contains("input value $9.99–$10"));
        assert!(grant_summary.contains("total ≤$4"));
        assert!(!grant_summary.contains("USD ceilings"));
        assert!(!grant_summary.contains("$0.11 fees"));
        let mut wide_usd_bounds = explicit_usd_bounds.clone();
        wide_usd_bounds.input_usd_micros = Some(gofin_bridge::AmountInterval {
            minimum: gofin_domain::Quantity::new("18446744073709551616000000").unwrap(),
            maximum: gofin_domain::Quantity::new("18446744073709551616000001").unwrap(),
        });
        let wide_summary = finite_bridge_boundary_summary(
            "Reset allowance; Max fees 0.0005 ETH gas. USD ceilings: $1.01 movement, $0.11 fees.",
            &reset_start_plan,
            &allowance_review.quote.intent,
            &wide_usd_bounds,
        )
        .unwrap();
        assert!(
            wide_summary.contains("input value $18446744073709551616–$18446744073709551616.000001")
        );
        assert!(wide_summary.contains("gas fee ≤$2"));
        let defaults = normalize_authorization_bounds(None, &saved).unwrap();
        assert_eq!(
            defaults.input.as_ref().unwrap().minimum.as_str(),
            "10000000"
        );
        assert_eq!(
            defaults.input.as_ref().unwrap().maximum.as_str(),
            "10000000"
        );
        assert_eq!(
            defaults
                .minimum_net_receipt
                .as_ref()
                .unwrap()
                .atoms
                .as_str(),
            "9950000"
        );
        assert!(defaults.input_usd_micros.is_none());
        assert!(defaults.source_usd_per_unit_micros.is_none());
        let mut selected_floor = saved.bounds.clone();
        selected_floor.minimum_net_receipt = Some(GoFinAmount {
            asset: saved.quote.minimum_receipt.asset.clone(),
            atoms: gofin_domain::Quantity::new("9500000").unwrap(),
        });
        assert_eq!(
            normalize_authorization_bounds(Some(selected_floor.clone()), &saved)
                .unwrap()
                .minimum_net_receipt
                .unwrap()
                .atoms
                .as_str(),
            "9500000"
        );
        selected_floor.minimum_net_receipt.as_mut().unwrap().atoms =
            gofin_domain::Quantity::new("10000000").unwrap();
        assert!(normalize_authorization_bounds(Some(selected_floor), &saved).is_err());

        let mut lower_summary_floor = saved.clone();
        lower_summary_floor.bounds.minimum_net_receipt = Some(GoFinAmount {
            asset: lower_summary_floor.quote.minimum_receipt.asset.clone(),
            atoms: gofin_domain::Quantity::new("9500000").unwrap(),
        });
        assert!(
            adapter
                .summary(&lower_summary_floor, &BridgeStepKind::Deposit, "dev-wallet")
                .contains("Receive ≥9.5 USDC")
        );
    }

    #[test]
    fn multi_step_bridge_uses_one_cumulative_gas_cap_per_asset() {
        let quote = BridgeQuote {
            cctp: None,
            quote_id: "synthetic-quote".into(),
            provider: "relay".into(),
            route_id: "synthetic-route".into(),
            intent: BridgeIntent {
                wallet: gofin_domain::ResourceId("ethereum:synthetic".into()),
                source_address: "0x1111111111111111111111111111111111111111".into(),
                source: Some(EvmAsset {
                    chain_id: 1,
                    token: Some(ETHEREUM_USDC.into()),
                    decimals: 6,
                }),
                solana_source: None,
                solana_destination: None,
                destination_chain_id: Some(42161),
                destination_assets: vec![],
                recipient: "0x2222222222222222222222222222222222222222".into(),
                refund_to: "0x2222222222222222222222222222222222222222".into(),
                allowed_refund_assets: vec![],
                allowed_solana_refund_assets: vec![],
                amount: gofin_domain::Quantity::new("100").unwrap(),
            },
            gross_debit: GoFinAmount {
                asset: gofin_domain::AssetId {
                    network: "ethereum:1".into(),
                    reference: ETHEREUM_USDC.into(),
                    decimals: 6,
                },
                atoms: gofin_domain::Quantity::new("100").unwrap(),
            },
            expected_receipt: GoFinAmount {
                asset: gofin_domain::AssetId {
                    network: "ethereum:42161".into(),
                    reference: ARBITRUM_USDC.into(),
                    decimals: 6,
                },
                atoms: gofin_domain::Quantity::new("99").unwrap(),
            },
            minimum_receipt: GoFinAmount {
                asset: gofin_domain::AssetId {
                    network: "ethereum:42161".into(),
                    reference: ARBITRUM_USDC.into(),
                    decimals: 6,
                },
                atoms: gofin_domain::Quantity::new("99").unwrap(),
            },
            fees: vec![],
            approval: None,
            protocol_steps: vec![],
            estimated_arrival_seconds: None,
            observed_at: now(),
            valid_until: now(),
            order_solver: Some(gofin_bridge::relay::RELAY_FILL_AUTHORITY.into()),
            class: gofin_bridge::QuoteClass::Executable,
            refund_paths: vec![],
            protocol_order_id: Some("0x01".into()),
            provider_request_id: Some("synthetic-request".into()),
        };
        let step = |ordinal, kind, dependencies| BridgeStep {
            ordinal,
            kind,
            solana_mint: None,
            solana_transaction: None,
            call: Some(EvmCall {
                chain_id: 1,
                to: ETHEREUM_USDC.into(),
                value: gofin_domain::Quantity::zero(),
                data: "0x00".into(),
            }),
            expected_sender: "0x1111111111111111111111111111111111111111".into(),
            maximum_debit: vec![],
            dependencies,
        };
        let plan = PreparedBridge {
            quote,
            bounds: BridgeBounds::default(),
            steps: vec![
                step(0, ProviderStepKind::ResetAllowance, vec![]),
                step(1, ProviderStepKind::Approval, vec![0]),
                step(2, ProviderStepKind::Deposit, vec![1]),
            ],
            source_order_id: Some("0x01".into()),
            fixed_scope_digest: "0x02".into(),
        };
        let gas = gofin_domain::AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let bounds = BridgeBounds {
            fee_caps: vec![gofin_bridge::FeeCap {
                asset: gas.clone(),
                kind: gofin_bridge::FeeKind::Gas,
                maximum: gofin_domain::Quantity::new("50000000000000").unwrap(),
                maximum_usd_micros: None,
            }],
            ..BridgeBounds::default()
        };
        let caps = cumulative_gas_cost_caps(
            &bounds,
            &plan,
            &gofin_domain::ResourceId("ethereum:synthetic".into()),
        )
        .unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].amount.asset, gas);
        assert_eq!(caps[0].amount.atoms.as_str(), "50000000000000");
        let mut with_additional = bounds;
        with_additional.fee_caps.push(gofin_bridge::FeeCap {
            asset: caps[0].amount.asset.clone(),
            kind: gofin_bridge::FeeKind::AdditionalDebit,
            maximum: gofin_domain::Quantity::new("20000000000000").unwrap(),
            maximum_usd_micros: None,
        });
        let mut combined = cumulative_gas_cost_caps(
            &with_additional,
            &plan,
            &gofin_domain::ResourceId("ethereum:synthetic".into()),
        )
        .unwrap();
        push_plan_cost_cap(
            &mut combined,
            &gofin_domain::ResourceId("ethereum:synthetic".into()),
            &gas,
            &gofin_domain::Quantity::new("20000000000000").unwrap(),
        )
        .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].amount.atoms.as_str(), "70000000000000");
    }

    #[test]
    fn bridge_amount_refresh_can_change_only_within_fixed_intent() {
        let first = BridgeIntent {
            wallet: gofin_domain::ResourceId("ethereum:synthetic".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(42161),
            destination_assets: vec![EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            }],
            recipient: "0x2222222222222222222222222222222222222222".into(),
            refund_to: "0x2222222222222222222222222222222222222222".into(),
            allowed_refund_assets: vec![EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }],
            allowed_solana_refund_assets: vec![],
            amount: gofin_domain::Quantity::new("8").unwrap(),
        };
        let mut refreshed = first.clone();
        refreshed.amount = gofin_domain::Quantity::new("9").unwrap();
        assert!(same_bridge_intent_except_amount(&first, &refreshed));
        refreshed.recipient = "0x3333333333333333333333333333333333333333".into();
        assert!(!same_bridge_intent_except_amount(&first, &refreshed));
    }

    #[test]
    fn public_bridge_step_uses_the_shared_camel_case_contract() {
        let public = BridgeTransferInput {
            transfer_id: "synthetic-transfer".into(),
            step_id: "synthetic-step".into(),
            step_kind: BridgeStepKind::Deposit,
            bounds: None,
        };
        let wire = serde_json::to_value(&public).unwrap();
        assert_eq!(wire["transferId"], "synthetic-transfer");
        assert_eq!(wire["stepId"], "synthetic-step");
        assert_eq!(wire["stepKind"], "deposit");
        assert!(wire.get("transfer_id").is_none());
        assert!(
            BridgeAdapter::new(BTreeMap::new(), Arc::new(SharedUsdValuator::new().unwrap()))
                .unwrap()
                .transfer_input(&wire)
                .is_ok()
        );
    }

    #[test]
    fn plan_step_identity_is_exact_within_a_finite_bridge_topology() {
        let fixed = json!({"provider":"relay","plan_step_identity":{
            "transfer_id":"transfer-a","step_id":"step-deposit","step_kind":"deposit"
        }});
        let candidate = json!({
            "transfer_id":"transfer-a",
            "step_id":"step-deposit",
            "step_kind":"deposit",
            "quote":{"provider":"relay"}
        });
        assert!(fixed_plan_step_identity_matches(&fixed, &candidate));

        let mut other_step = candidate.clone();
        other_step["step_id"] = json!("step-approval");
        other_step["step_kind"] = json!("approval");
        assert!(!fixed_plan_step_identity_matches(&fixed, &other_step));

        let mut other_transfer = candidate.clone();
        other_transfer["transfer_id"] = json!("transfer-b");
        assert!(!fixed_plan_step_identity_matches(&fixed, &other_transfer));

        let mut other_provider = candidate;
        other_provider["quote"]["provider"] = json!("arbitrum_canonical");
        assert!(!fixed_plan_step_identity_matches(&fixed, &other_provider));
        assert!(fixed_plan_step_identity_matches(&json!({}), &json!({})));
    }

    fn bridge_execute_candidate_fixture()
    -> (gofin_domain::boundaries::BoundarySpec, PreparedOperation) {
        let source = EvmAsset {
            chain_id: 1,
            token: Some(ETHEREUM_USDC.into()),
            decimals: 6,
        };
        let destination = EvmAsset {
            chain_id: 4663,
            token: Some(ROBINHOOD_USDG.into()),
            decimals: 18,
        };
        let refunds = vec![source.clone()];
        let amount = |value: &str| Quantity::new(value).unwrap();
        let quote = BridgeQuote {
            quote_id: "synthetic-quote".into(),
            provider: "relay".into(),
            route_id: "synthetic-route".into(),
            intent: BridgeIntent {
                wallet: gofin_domain::ResourceId("ethereum:1:wallet".into()),
                source_address: "0x1111111111111111111111111111111111111111".into(),
                source: Some(source.clone()),
                solana_source: None,
                destination_chain_id: Some(4663),
                solana_destination: None,
                destination_assets: vec![destination.clone()],
                recipient: "0x2222222222222222222222222222222222222222".into(),
                refund_to: "0x3333333333333333333333333333333333333333".into(),
                allowed_refund_assets: refunds.clone(),
                allowed_solana_refund_assets: vec![],
                amount: amount("15000000"),
            },
            gross_debit: source.amount(amount("15000000")),
            expected_receipt: destination.amount(amount("14000000000000000000")),
            minimum_receipt: destination.amount(amount("12000000000000000000")),
            fees: vec![],
            approval: None,
            protocol_steps: vec![],
            estimated_arrival_seconds: Some(120),
            observed_at: "2026-01-01T00:00:00Z".into(),
            valid_until: "2099-01-01T00:00:00Z".into(),
            class: gofin_bridge::QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: None,
            provider_request_id: None,
            cctp: None,
        };
        let bounds = BridgeBounds {
            input: Some(gofin_bridge::AmountInterval {
                minimum: amount("10000000"),
                maximum: amount("20000000"),
            }),
            maximum_source_debit: Some(amount("20000000")),
            minimum_net_receipt: Some(destination.amount(amount("10000000000000000000"))),
            fee_caps: vec![gofin_bridge::FeeCap {
                asset: gofin_domain::AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
                kind: gofin_bridge::FeeKind::Gas,
                maximum: amount("1000000000000000"),
                maximum_usd_micros: None,
            }],
            ..BridgeBounds::default()
        };
        let fixed = json!({"bridge_execute_scope":{
            "wallet":"ethereum:1:wallet",
            "source_address":"0x1111111111111111111111111111111111111111",
            "source":source,
            "destination_chain_id":4663,
            "destination_assets":[destination],
            "recipient":"0x2222222222222222222222222222222222222222",
            "refund_to":"0x3333333333333333333333333333333333333333",
            "allowed_refund_assets":refunds,
            "amount":{"minimum":"10000000","maximum":"20000000"},
            "allowed_providers":["relay"]
        }});
        let spec = gofin_domain::boundaries::BoundarySpec {
            fixed,
            parameters: json!({"bounds":bounds}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "synthetic bridge intent".into(),
        };
        let prepared = PreparedOperation {
            operation: "bridge.execute".into(),
            input: json!({"transferId":"synthetic-transfer","stepId":"synthetic-deposit","stepKind":"deposit"}),
            binding: json!({"quote":quote,"bounds":bounds}),
            summary: "synthetic bridge step".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        (spec, prepared)
    }

    fn relay_native_eth_settlement_fixture(
        source_cap: &str,
    ) -> (
        BridgeTransferRecord,
        BridgeQuote,
        gofin_bridge::BridgeCapabilities,
        GoFinAmount,
        gofin_domain::AssetId,
        Quantity,
    ) {
        let source = EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        };
        let destination = EvmAsset {
            chain_id: 4663,
            token: None,
            decimals: 18,
        };
        let source_asset = source.asset_id();
        let destination_asset = destination.asset_id();
        let gross = Quantity::new("7740000000000000").unwrap();
        let payout = Quantity::new("7726633447612574").unwrap();
        let fee = Quantity::new("13366552387426").unwrap();
        let wallet = gofin_domain::ResourceId(
            "ethereum:1:0x1111111111111111111111111111111111111111".into(),
        );
        let quote = BridgeQuote {
            quote_id: "synthetic-relay-native-quote".into(),
            provider: "relay".into(),
            route_id: "synthetic-relay-native-route".into(),
            intent: BridgeIntent {
                wallet: wallet.clone(),
                source_address: "0x1111111111111111111111111111111111111111".into(),
                source: Some(source.clone()),
                solana_source: None,
                destination_chain_id: Some(destination.chain_id),
                solana_destination: None,
                destination_assets: vec![destination.clone()],
                recipient: "0x2222222222222222222222222222222222222222".into(),
                refund_to: "0x3333333333333333333333333333333333333333".into(),
                allowed_refund_assets: vec![source.clone()],
                allowed_solana_refund_assets: vec![],
                amount: gross.clone(),
            },
            gross_debit: GoFinAmount {
                asset: source_asset.clone(),
                atoms: gross.clone(),
            },
            expected_receipt: GoFinAmount {
                asset: destination_asset.clone(),
                atoms: payout.clone(),
            },
            minimum_receipt: GoFinAmount {
                asset: destination_asset.clone(),
                atoms: Quantity::new("7500000000000000").unwrap(),
            },
            fees: vec![gofin_bridge::BridgeFee {
                amount: GoFinAmount {
                    asset: source_asset.clone(),
                    atoms: fee,
                },
                kind: gofin_bridge::FeeKind::WithheldFromOutput,
                payer: "relay".into(),
            }],
            approval: None,
            protocol_steps: vec![],
            estimated_arrival_seconds: None,
            observed_at: "2026-01-01T00:00:00Z".into(),
            valid_until: "2099-01-01T00:00:00Z".into(),
            class: gofin_bridge::QuoteClass::Executable,
            refund_paths: vec![],
            order_solver: None,
            protocol_order_id: Some("synthetic-order".into()),
            provider_request_id: Some("synthetic-request".into()),
            cctp: None,
        };
        let bounds = BridgeBounds {
            fee_caps: vec![gofin_bridge::FeeCap {
                asset: source_asset.clone(),
                kind: gofin_bridge::FeeKind::WithheldFromOutput,
                maximum: Quantity::new(source_cap).unwrap(),
                maximum_usd_micros: None,
            }],
            ..BridgeBounds::default()
        };
        let transfer = BridgeTransferRecord {
            version: 1,
            id: "synthetic-relay-native-transfer".into(),
            grant_id: None,
            plan_scope: None,
            metadata: Value::Null,
            settlement_expectation: None,
            approved_bounds: Some(serde_json::to_value(bounds).unwrap()),
            prospective_debit: None,
            state: BridgeTransferState::InTransit,
            quote_revisions: vec![],
            steps: vec![],
            allocation_owner: BridgeAllocationOwner::Transfer {
                transfer_id: "synthetic-relay-native-transfer".into(),
            },
            transit: None,
            applied_proofs: vec![],
            settlement_proofs: vec![],
        };
        let capabilities = gofin_bridge::BridgeCapabilities {
            provider: "relay".into(),
            chains: std::collections::BTreeSet::from([1, 42161, 4663, 8453]),
            native_eth: true,
            erc20: true,
            protocol_versions: std::collections::BTreeSet::new(),
        };
        (
            transfer,
            quote,
            capabilities,
            GoFinAmount {
                asset: source_asset,
                atoms: gross,
            },
            destination_asset,
            payout,
        )
    }

    #[test]
    fn relay_native_eth_withheld_cost_uses_verified_principal_and_final_payout() {
        let (transfer, quote, capabilities, source_principal, destination_asset, payout) =
            relay_native_eth_settlement_fixture("25000000000000");
        let cost = relay_native_withheld_cost(
            &transfer,
            &quote,
            &capabilities,
            &source_principal,
            &destination_asset,
            &payout,
        )
        .unwrap()
        .expect("measured source-denominated Relay fee is within the approved cap");
        assert_eq!(
            cost.location,
            AccountingLocation::Cash {
                resource: quote.intent.wallet.clone()
            }
        );
        assert_eq!(cost.amount.asset, quote.intent.source_asset_id().unwrap());
        assert_eq!(cost.amount.atoms.as_str(), "13366552387426");
        assert_eq!(cost.treatment, CostTreatment::IncludedInDebit);

        let (_, quote, capabilities, source_principal, destination_asset, _) =
            relay_native_eth_settlement_fixture("25000000000000");
        // The quote is a fee-category marker, not the realized amount. Use
        // the proved payout to derive a different actual value under the cap.
        let payout_with_different_realized_fee = Quantity::new("7726633447612573").unwrap();
        let measured = relay_native_withheld_cost(
            &transfer,
            &quote,
            &capabilities,
            &source_principal,
            &destination_asset,
            &payout_with_different_realized_fee,
        )
        .unwrap()
        .expect("measured fee can differ from quote while remaining under cap");
        assert_eq!(measured.amount.atoms.as_str(), "13366552387427");
    }

    #[test]
    fn relay_native_eth_withheld_cost_rejects_unreviewed_assets_and_over_cap_amounts() {
        let (transfer, quote, capabilities, source_principal, destination_asset, payout) =
            relay_native_eth_settlement_fixture("10000000000000");
        assert_eq!(
            relay_native_withheld_cost(
                &transfer,
                &quote,
                &capabilities,
                &source_principal,
                &destination_asset,
                &payout,
            )
            .unwrap_err()
            .code,
            "BRIDGE_WITHHELD_FEE_CAP_EXCEEDED"
        );

        let (transfer, quote, capabilities, source_principal, destination_asset, payout) =
            relay_native_eth_settlement_fixture("25000000000000");
        let mut unsupported = quote.clone();
        unsupported.intent.destination_chain_id = Some(56);
        unsupported.intent.destination_assets[0].chain_id = 56;
        assert!(
            relay_native_withheld_cost(
                &transfer,
                &unsupported,
                &capabilities,
                &source_principal,
                &destination_asset,
                &payout,
            )
            .unwrap()
            .is_none()
        );

        let mut unsupported_source = quote.clone();
        unsupported_source.intent.source.as_mut().unwrap().chain_id = 56;
        assert!(
            relay_native_withheld_cost(
                &transfer,
                &unsupported_source,
                &capabilities,
                &source_principal,
                &destination_asset,
                &payout,
            )
            .unwrap()
            .is_none()
        );

        let mut wrong_decimals = quote.clone();
        wrong_decimals.intent.destination_assets[0].decimals = 6;
        assert!(
            relay_native_withheld_cost(
                &transfer,
                &wrong_decimals,
                &capabilities,
                &source_principal,
                &destination_asset,
                &payout,
            )
            .unwrap()
            .is_none()
        );

        let mut token_route = quote.clone();
        token_route.intent.destination_assets[0].token =
            Some("0x4444444444444444444444444444444444444444".into());
        assert!(
            relay_native_withheld_cost(
                &transfer,
                &token_route,
                &capabilities,
                &source_principal,
                &destination_asset,
                &payout,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn bridge_execute_candidate_accepts_only_the_approved_provider_scope_and_bounds() {
        let (spec, prepared) = bridge_execute_candidate_fixture();
        assert!(validate_bridge_execute_candidate(&spec, &prepared).is_ok());

        let mut changed_provider = prepared.clone();
        changed_provider.binding["quote"]["provider"] = json!("arbitrum_canonical");
        assert!(validate_bridge_execute_candidate(&spec, &changed_provider).is_err());

        let mut changed_source_asset = prepared.clone();
        changed_source_asset.binding["quote"]["intent"]["source"]["token"] =
            json!("0x4444444444444444444444444444444444444444");
        assert!(validate_bridge_execute_candidate(&spec, &changed_source_asset).is_err());

        let mut changed_destination_asset = prepared.clone();
        changed_destination_asset.binding["quote"]["intent"]["destination_assets"][0]["token"] =
            json!("0x4444444444444444444444444444444444444444");
        assert!(validate_bridge_execute_candidate(&spec, &changed_destination_asset).is_err());

        let mut changed_recipient = prepared.clone();
        changed_recipient.binding["quote"]["intent"]["recipient"] =
            json!("0x5555555555555555555555555555555555555555");
        assert!(validate_bridge_execute_candidate(&spec, &changed_recipient).is_err());

        let mut over_debit = prepared.clone();
        over_debit.binding["quote"]["gross_debit"]["atoms"] = json!("20000001");
        assert!(validate_bridge_execute_candidate(&spec, &over_debit).is_err());

        let mut out_of_range = prepared.clone();
        out_of_range.binding["quote"]["intent"]["amount"] = json!("20000001");
        assert!(validate_bridge_execute_candidate(&spec, &out_of_range).is_err());

        let mut subset_refund_scope = prepared.clone();
        subset_refund_scope.binding["quote"]["intent"]["allowed_refund_assets"] = json!([]);
        assert!(validate_bridge_execute_candidate(&spec, &subset_refund_scope).is_ok());

        let mut changed_bounds = prepared;
        changed_bounds.binding["bounds"]["maximum_source_debit"] = json!("30000000");
        assert!(validate_bridge_execute_candidate(&spec, &changed_bounds).is_err());
    }

    #[test]
    fn plan_refresh_keeps_route_topology_but_floats_only_finite_approval_amount() {
        let step = |kind, amount: &str| BridgeStep {
            ordinal: 0,
            kind,
            solana_mint: None,
            solana_transaction: None,
            call: Some(EvmCall {
                chain_id: 1,
                to: ETHEREUM_USDC.into(),
                value: Quantity::zero(),
                data: format!("approve:{amount}"),
            }),
            expected_sender: "0x1111111111111111111111111111111111111111".into(),
            maximum_debit: vec![],
            dependencies: vec![],
        };
        assert!(same_bridge_plan_for_permission(
            &[step(ProviderStepKind::Approval, "100")],
            &[step(ProviderStepKind::Approval, "120")],
        ));
        assert!(!same_bridge_plan_for_permission(
            &[step(ProviderStepKind::Deposit, "100")],
            &[step(ProviderStepKind::Deposit, "120")],
        ));
    }

    #[test]
    fn pending_evm_relay_intent_without_solana_fields_remains_readable() {
        let previous = json!({
            "wallet":"ethereum:1:0x1111111111111111111111111111111111111111",
            "source_address":"0x1111111111111111111111111111111111111111",
            "source":{"chain_id":1,"token":null,"decimals":18},
            "destination_chain_id":8453,
            "destination_assets":[{"chain_id":8453,"token":null,"decimals":18}],
            "recipient":"0x2222222222222222222222222222222222222222",
            "refund_to":"0x1111111111111111111111111111111111111111",
            "allowed_refund_assets":[{"chain_id":1,"token":null,"decimals":18}],
            "amount":"1000"
        });
        let intent: BridgeIntent = serde_json::from_value(previous).unwrap();
        assert_eq!(intent.evm_source().unwrap().chain_id, 1);
        assert!(intent.solana_source.is_none());
        assert!(intent.allowed_solana_refund_assets.is_empty());
        assert_eq!(intent.evm_destination_chain_id().unwrap(), 8453);
    }
}
